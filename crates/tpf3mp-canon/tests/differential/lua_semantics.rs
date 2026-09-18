//! The Lua 5.1 semantics the port relies on, checked against the interpreter:
//! `tpf3mp_canon::lua` against Lua's operators, TPF2MP's `util.clamp` and
//! `util.integer`, and Lua's string order and case folding.

use std::cell::Cell;

use mlua::{Function, Value};
use proptest::prelude::*;
use tpf3mp_canon::lua::{self, MAX_EXACT_INTEGER};

use crate::tpf2mp::{Tpf2mp, exact_int, num, runner};

const TWO_53: i64 = 1 << 53;

/// Integers that stress the exact range: small ones, ones near 2^53, and
/// everything in between.
fn operand() -> impl Strategy<Value = i64> {
    prop_oneof![
        -1000i64..=1000,
        (0i64..=64).prop_map(|k| MAX_EXACT_INTEGER - k),
        (0i64..=64).prop_map(|k| -(MAX_EXACT_INTEGER - k)),
        (0u32..=53).prop_map(|shift| (1i64 << shift) - 1),
        -MAX_EXACT_INTEGER..=MAX_EXACT_INTEGER,
        -(1i64 << 27)..=(1i64 << 27),
    ]
}

#[test]
fn arithmetic_matches_lua_wherever_the_port_answers() {
    let tpf2mp = Tpf2mp::new();
    let operators: Function = tpf2mp
        .lua()
        .load("return function(a, b) return a + b, a - b, a * b, math.floor(a / b), a % b end")
        .eval()
        .unwrap();
    let answered: [Cell<u32>; 5] = Default::default();
    let cases = 20_000;
    runner(cases)
        .run(&(operand(), operand()), |(a, b)| {
            let (sum, difference, product, quotient, remainder): (
                Value,
                Value,
                Value,
                Value,
                Value,
            ) = operators.call((num(a), num(b))).unwrap();
            let port = [
                lua::add(a, b),
                lua::sub(a, b),
                lua::mul(a, b),
                lua::floor_div(a, b),
                lua::modulo(a, b),
            ];
            let lua_results = [sum, difference, product, quotient, remainder];
            for (index, (lua_value, port_value)) in lua_results.iter().zip(port).enumerate() {
                if let Some(port_value) = port_value {
                    answered[index].set(answered[index].get() + 1);
                    prop_assert_eq!(
                        exact_int(lua_value),
                        Some(port_value),
                        "operator {} of ({}, {})",
                        index,
                        a,
                        b
                    );
                }
            }
            Ok(())
        })
        .unwrap();
    for (index, count) in answered.iter().enumerate() {
        assert!(
            count.get() >= cases / 10,
            "operator {index} answered only {} of {cases} cases",
            count.get()
        );
    }
}

#[test]
fn lua_rounds_where_the_port_refuses() {
    let tpf2mp = Tpf2mp::new();
    let sum: f64 = tpf2mp
        .lua()
        .load(format!("return {} + 2", MAX_EXACT_INTEGER))
        .eval()
        .unwrap();
    assert_eq!(sum, TWO_53 as f64, "Lua cannot hold 2^53 + 1");
    assert_eq!(lua::add(MAX_EXACT_INTEGER, 2), None);
    // (2^53 - 1) * 3 rounds; floor((2^53 - 1) * 3 / 2) is then off by one.
    let product: f64 = tpf2mp
        .lua()
        .load(format!("return {} * 3", MAX_EXACT_INTEGER))
        .eval()
        .unwrap();
    assert_ne!(product as i128, i128::from(MAX_EXACT_INTEGER) * 3);
    assert_eq!(lua::mul(MAX_EXACT_INTEGER, 3), None);
}

#[test]
fn clamp_matches_util_clamp_even_when_bounds_cross() {
    let tpf2mp = Tpf2mp::new();
    let clamp: Function = tpf2mp
        .module_field("util", "clamp")
        .as_function()
        .unwrap()
        .clone();
    runner(4096)
        .run(
            &(-2000i64..=2000, -2000i64..=2000, -2000i64..=2000),
            |(value, low, high)| {
                let expected: Value = clamp.call((num(value), num(low), num(high))).unwrap();
                prop_assert_eq!(exact_int(&expected), Some(lua::clamp(value, low, high)));
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn util_integer_is_the_identity_on_integers_and_defaults_nil() {
    let tpf2mp = Tpf2mp::new();
    let integer: Function = tpf2mp
        .module_field("util", "integer")
        .as_function()
        .unwrap()
        .clone();
    runner(4096)
        .run(&operand(), |value| {
            let coerced: Value = integer.call((num(value), 17.0)).unwrap();
            prop_assert_eq!(exact_int(&coerced), Some(value));
            Ok(())
        })
        .unwrap();
    let fallback: Value = integer.call((Value::Nil, 17.0)).unwrap();
    assert_eq!(exact_int(&fallback), Some(17));
    let zero: Value = integer.call(Value::Nil).unwrap();
    assert_eq!(exact_int(&zero), Some(0));
}

#[test]
fn string_order_is_bytewise() {
    // Lua 5.1 compares strings with strcoll. The port orders ids as Rust
    // does, bytewise, which equals strcoll only in the C locale; this pins
    // that the interpreter the tests use runs in it.
    let tpf2mp = Tpf2mp::new();
    let less: Function = tpf2mp
        .lua()
        .load("return function(a, b) return a < b end")
        .eval()
        .unwrap();
    let sort: Function = tpf2mp
        .lua()
        .load("return function(list) table.sort(list) return list end")
        .eval()
        .unwrap();
    let bytes = proptest::collection::vec(any::<u8>(), 0..6);
    runner(4096)
        .run(&proptest::collection::vec(bytes, 2..8), |strings| {
            let lua = tpf2mp.lua();
            let first = lua.create_string(&strings[0]).unwrap();
            let second = lua.create_string(&strings[1]).unwrap();
            let lua_less: bool = less.call((first, second)).unwrap();
            prop_assert_eq!(lua_less, strings[0] < strings[1]);
            let list = lua.create_table().unwrap();
            for string in &strings {
                list.push(lua.create_string(string).unwrap()).unwrap();
            }
            let sorted: mlua::Table = sort.call(list).unwrap();
            let sorted: Vec<Vec<u8>> = sorted
                .sequence_values::<mlua::LuaString>()
                .map(|value| value.unwrap().as_bytes().to_vec())
                .collect();
            let mut expected = strings.clone();
            expected.sort();
            prop_assert_eq!(sorted, expected);
            Ok(())
        })
        .unwrap();
}

#[test]
fn string_lower_folds_ascii_only() {
    let tpf2mp = Tpf2mp::new();
    let lower: Function = tpf2mp
        .lua()
        .load("return function(value) return value:lower() end")
        .eval()
        .unwrap();
    runner(4096)
        .run(&proptest::collection::vec(any::<u8>(), 0..12), |bytes| {
            let lua = tpf2mp.lua();
            let lowered: mlua::LuaString = lower.call(lua.create_string(&bytes).unwrap()).unwrap();
            prop_assert_eq!(lowered.as_bytes().to_vec(), bytes.to_ascii_lowercase());
            Ok(())
        })
        .unwrap();
}
