//! `economy_difficulty.lua`: presets and revenue scaling.

use mlua::{Table, Value};
use proptest::option;
use proptest::prelude::*;
use tpf3mp_canon::economy::ACCUMULATOR_LIMIT;
use tpf3mp_canon::economy::difficulty::{self, Difficulty};
use tpf3mp_canon::lua::MAX_EXACT_INTEGER;

use crate::tpf2mp::{
    Call, Check, Outcome, Tpf2mp, field, get, num, opt, opt_int, runner, same, string, table,
    wide_amount, wide_num,
};

pub const CHECKS: &[(&str, Check)] = &[
    ("economy_difficulty.normaliseKey", check_normalise_key),
    ("economy_difficulty.multiplier", check_multiplier),
    ("economy_difficulty.apply", check_apply),
];

/// Lua's `tostring(value or "")` for the values tests pass: strings, or nil.
fn key_of(value: &Value) -> String {
    match value {
        Value::Nil => String::new(),
        _ => string(value, "key"),
    }
}

pub fn check_normalise_key(call: &Call) -> Outcome {
    let key = key_of(&call.arg(1));
    let port = Difficulty::from_key(&key).key();
    assert_eq!(
        string(&call.result(1), "key"),
        port,
        "normaliseKey({key:?})"
    );
    Outcome::Matched
}

pub fn check_multiplier(call: &Call) -> Outcome {
    let key = key_of(&call.arg(1));
    let context = format!("multiplier({key:?})");
    same(
        &context,
        "ppm",
        &call.result(1),
        Difficulty::from_key(&key).revenue_multiplier_ppm(),
    );
    Outcome::Matched
}

pub fn check_apply(call: &Call) -> Outcome {
    // `util.integer(x, fallback)`: raw 0, multiplier 1x, residual 0.
    let raw = opt_int(&call.arg(1), "rawCents").unwrap_or(0);
    let multiplier = opt_int(&call.arg(2), "multiplierPpm").unwrap_or(difficulty::SCALE);
    let residual = opt_int(&call.arg(3), "residual").unwrap_or(0);
    let context = format!("apply({raw}, {multiplier}, {residual})");
    let (scaled, carried) = difficulty::apply(raw, multiplier, residual);
    same(&context, "scaled", &call.result(1), scaled);
    same(&context, "residual", &call.result(2), carried);
    Outcome::Matched
}

#[test]
fn presets_match_lua() {
    let tpf2mp = Tpf2mp::new();
    let presets = table(
        &tpf2mp.module_field("economy_difficulty", "PRESETS"),
        "PRESETS",
    );
    let order = table(&tpf2mp.module_field("economy_difficulty", "ORDER"), "ORDER");
    let lua_order: Vec<String> = order
        .sequence_values::<Value>()
        .map(|key| string(&key.unwrap(), "ORDER"))
        .collect();
    let port_order: Vec<&str> = Difficulty::ORDER
        .iter()
        .map(|preset| preset.key())
        .collect();
    assert_eq!(lua_order, port_order);
    assert_eq!(
        presets.pairs::<Value, Value>().count(),
        Difficulty::ORDER.len()
    );
    for preset in Difficulty::ORDER {
        let lua: Table = presets.get(preset.key()).unwrap();
        assert_eq!(string(&get(&lua, "key"), "key"), preset.key());
        assert_eq!(string(&get(&lua, "label"), "label"), preset.label());
        assert_eq!(
            field(&lua, "revenueMultiplierPpm"),
            preset.revenue_multiplier_ppm()
        );
    }
    let default = string(
        &tpf2mp.module_field("economy_difficulty", "DEFAULT_KEY"),
        "DEFAULT_KEY",
    );
    assert_eq!(default, Difficulty::DEFAULT.key());
    assert_eq!(field_of(&tpf2mp, "SCALE"), difficulty::SCALE);
    assert_eq!(field_of(&tpf2mp, "ACCUMULATOR_LIMIT"), ACCUMULATOR_LIMIT);
}

fn field_of(tpf2mp: &Tpf2mp, name: &str) -> i64 {
    crate::tpf2mp::int(&tpf2mp.module_field("economy_difficulty", name), name)
}

#[test]
fn keys_normalise_like_lua() {
    let tpf2mp = Tpf2mp::new();
    let known =
        prop::sample::select(vec!["normal", "hard", "easy", "relaxed"]).prop_flat_map(|key| {
            proptest::collection::vec(any::<bool>(), key.len()).prop_map(move |upper| {
                key.chars()
                    .zip(upper)
                    .map(|(letter, upper)| {
                        if upper {
                            letter.to_ascii_uppercase()
                        } else {
                            letter
                        }
                    })
                    .collect::<String>()
            })
        });
    let other = "[ -~]{0,9}|h\u{e4}rd|H\u{c4}RD|\u{130}asy";
    runner(4096)
        .run(&prop_oneof![known.boxed(), other.boxed()], |key| {
            let call = tpf2mp.run("economy_difficulty.normaliseKey", key.as_str());
            prop_assert_eq!(check_normalise_key(&call), Outcome::Matched);
            let call = tpf2mp.run("economy_difficulty.multiplier", key.as_str());
            prop_assert_eq!(check_multiplier(&call), Outcome::Matched);
            Ok(())
        })
        .unwrap();
    let call = tpf2mp.run("economy_difficulty.normaliseKey", Value::Nil);
    assert_eq!(check_normalise_key(&call), Outcome::Matched);
}

#[test]
fn apply_matches_lua() {
    let tpf2mp = Tpf2mp::new();
    let raw = prop_oneof![
        -10i64..=10_000_000,
        0i64..=ACCUMULATOR_LIMIT + 10,
        -MAX_EXACT_INTEGER..=MAX_EXACT_INTEGER,
    ];
    let multiplier = option::of(prop_oneof![
        prop::sample::select(vec![0, 600_000, 1_000_000, 1_500_000, 2_000_000, 4_000_000]),
        -1_000_000i64..=5_000_000,
    ]);
    let residual = option::of(prop_oneof![
        0i64..difficulty::SCALE,
        -2_000_000i64..=2_000_000
    ]);
    runner(8192)
        .run(
            &(raw, multiplier, residual),
            |(raw, multiplier, residual)| {
                let call = tpf2mp.run(
                    "economy_difficulty.apply",
                    (num(raw), opt(multiplier), opt(residual)),
                );
                prop_assert_eq!(check_apply(&call), Outcome::Matched);
                Ok(())
            },
        )
        .unwrap();
    // All three operands are clamped before any product forms.
    runner(4096)
        .run(
            &(wide_amount(), wide_amount(), wide_amount()),
            |(raw, multiplier, residual)| {
                let call = tpf2mp.run(
                    "economy_difficulty.apply",
                    (wide_num(raw), wide_num(multiplier), wide_num(residual)),
                );
                prop_assert_eq!(check_apply(&call), Outcome::Matched);
                Ok(())
            },
        )
        .unwrap();
}
