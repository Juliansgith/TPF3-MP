//! `economy_costs.lua`: annual upkeep and its proration.

use mlua::{Table, Value};
use proptest::prelude::*;
use tpf3mp_canon::economy::ACCUMULATOR_LIMIT;
use tpf3mp_canon::economy::costs::{self, FINANCIAL_YEAR_SECONDS, HOURS_PER_YEAR};
use tpf3mp_canon::lua::MAX_EXACT_INTEGER;

use crate::tpf2mp::{
    Call, Check, Outcome, Tpf2mp, int, int_map, num, opt_int, runner, same, string, table,
    wide_amount, wide_num,
};

pub const CHECKS: &[(&str, Check)] = &[
    (
        "economy_costs.vehicleAnnualUpkeepCents",
        check_vehicle_annual_upkeep_cents,
    ),
    (
        "economy_costs.infrastructureAnnualUpkeepCents",
        check_infrastructure_annual_upkeep_cents,
    ),
    ("economy_costs.hourlyCharge", check_hourly_charge),
    ("economy_costs.periodCharge", check_period_charge),
    ("economy_costs.charge", check_charge),
    ("economy_costs.allocateCapital", check_allocate_capital),
];

/// `util.integer(x, 0)` reads nil as zero.
fn integer_or_zero(call: &Call, index: usize, what: &str) -> i64 {
    opt_int(&call.arg(index), what).unwrap_or(0)
}

fn same_pair(context: &str, call: &Call, port: Option<(i64, i64)>) -> Outcome {
    match port {
        Some((charge, residual)) => {
            same(context, "charge", &call.result(1), charge);
            same(context, "residual", &call.result(2), residual);
            Outcome::Matched
        }
        None => Outcome::Refused,
    }
}

pub fn check_vehicle_annual_upkeep_cents(call: &Call) -> Outcome {
    let price = integer_or_zero(call, 1, "purchasePriceDollars");
    let context = format!("vehicleAnnualUpkeepCents({price})");
    match costs::vehicle_annual_upkeep_cents(price) {
        Some(upkeep) => {
            same(&context, "upkeep", &call.result(1), upkeep);
            Outcome::Matched
        }
        None => Outcome::Refused,
    }
}

pub fn check_infrastructure_annual_upkeep_cents(call: &Call) -> Outcome {
    let capital = integer_or_zero(call, 1, "capitalCents");
    let context = format!("infrastructureAnnualUpkeepCents({capital})");
    same(
        &context,
        "upkeep",
        &call.result(1),
        costs::infrastructure_annual_upkeep_cents(capital),
    );
    Outcome::Matched
}

pub fn check_hourly_charge(call: &Call) -> Outcome {
    let annual = integer_or_zero(call, 1, "annualCents");
    let residual = integer_or_zero(call, 2, "residual");
    same_pair(
        &format!("hourlyCharge({annual}, {residual})"),
        call,
        costs::hourly_charge(annual, residual),
    )
}

pub fn check_period_charge(call: &Call) -> Outcome {
    let annual = integer_or_zero(call, 1, "annualCents");
    let residual = integer_or_zero(call, 2, "residual");
    let period = integer_or_zero(call, 3, "periodSeconds");
    same_pair(
        &format!("periodCharge({annual}, {residual}, {period})"),
        call,
        costs::period_charge(annual, residual, period),
    )
}

pub fn check_charge(call: &Call) -> Outcome {
    let annual = integer_or_zero(call, 1, "annualCents");
    let residual = integer_or_zero(call, 2, "residual");
    let period = integer_or_zero(call, 3, "periodSeconds");
    // `util.integer(economyVersion, 1)`
    let version = opt_int(&call.arg(4), "economyVersion").unwrap_or(1);
    same_pair(
        &format!("charge({annual}, {residual}, {period}, {version})"),
        call,
        costs::charge(annual, residual, period, version),
    )
}

pub fn check_allocate_capital(call: &Call) -> Outcome {
    let cids: Vec<String> = table(&call.arg(1), "cids")
        .sequence_values::<Value>()
        .map(|cid| string(&cid.unwrap(), "cid"))
        .collect();
    let total = integer_or_zero(call, 2, "totalCents");
    let context = format!("allocateCapital({cids:?}, {total})");
    let cids: Vec<&str> = cids.iter().map(String::as_str).collect();
    let port = costs::allocate_capital(&cids, total).unwrap();
    assert_eq!(
        int_map(&table(&call.result(1), "allocation"), &context),
        port,
        "{context}"
    );
    Outcome::Matched
}

/// Any Lua-exact integer, weighted toward the magnitudes costs see.
fn amount() -> impl Strategy<Value = i64> {
    prop_oneof![
        -10i64..=100_000,
        0i64..=ACCUMULATOR_LIMIT + 10,
        -MAX_EXACT_INTEGER..=MAX_EXACT_INTEGER,
    ]
}

#[test]
fn annual_upkeep_matches_lua() {
    let tpf2mp = Tpf2mp::new();
    let exact_price = prop_oneof![-1000i64..=100_000_000, 0i64..=MAX_EXACT_INTEGER / 100];
    runner(4096)
        .run(&exact_price, |price| {
            let call = tpf2mp.run("economy_costs.vehicleAnnualUpkeepCents", num(price));
            prop_assert_eq!(check_vehicle_annual_upkeep_cents(&call), Outcome::Matched);
            Ok(())
        })
        .unwrap();
    runner(4096)
        .run(&amount(), |capital| {
            let call = tpf2mp.run(
                "economy_costs.infrastructureAnnualUpkeepCents",
                num(capital),
            );
            prop_assert_eq!(
                check_infrastructure_annual_upkeep_cents(&call),
                Outcome::Matched
            );
            Ok(())
        })
        .unwrap();
    // Both clamp their operand to [0, 10^15] first. Infrastructure upkeep is
    // then exact for any operand; vehicle upkeep multiplies the clamped
    // price by 100 and is refused once that leaves the exact range.
    runner(4096)
        .run(&wide_amount(), |amount| {
            let call = tpf2mp.run(
                "economy_costs.infrastructureAnnualUpkeepCents",
                wide_num(amount),
            );
            prop_assert_eq!(
                check_infrastructure_annual_upkeep_cents(&call),
                Outcome::Matched
            );
            let call = tpf2mp.run("economy_costs.vehicleAnnualUpkeepCents", wide_num(amount));
            let expected = if amount.clamp(0, ACCUMULATOR_LIMIT) <= MAX_EXACT_INTEGER / 100 {
                Outcome::Matched
            } else {
                Outcome::Refused
            };
            prop_assert_eq!(check_vehicle_annual_upkeep_cents(&call), expected);
            Ok(())
        })
        .unwrap();
}

#[test]
fn vehicle_upkeep_past_the_exact_range_rounds_in_lua() {
    // TPF2MP clamps the price to 10^15 dollars but multiplies by 100 first,
    // so past 2^53 / 100 dollars the port refuses. Lua's answer actually
    // goes wrong once the quotient passes 2^52, where a double has no
    // fraction bits left and `/ 6` rounds to the nearest whole cent.
    let tpf2mp = Tpf2mp::new();
    assert_eq!(
        costs::vehicle_annual_upkeep_cents(MAX_EXACT_INTEGER / 100 + 1),
        None
    );
    let divergent = (300_000_000_000_000i64..).take(1000).find(|price| {
        let call = tpf2mp.run("economy_costs.vehicleAnnualUpkeepCents", num(*price));
        assert_eq!(check_vehicle_annual_upkeep_cents(&call), Outcome::Refused);
        let lua = int(&call.result(1), "upkeep");
        i128::from(lua) != i128::from(*price) * 100 / 6
    });
    assert!(
        divergent.is_some(),
        "expected Lua to round somewhere past 2^53"
    );
}

#[test]
fn charges_match_lua() {
    let tpf2mp = Tpf2mp::new();
    let designed = (
        0i64..=ACCUMULATOR_LIMIT,
        0i64..FINANCIAL_YEAR_SECONDS,
        60i64..=86_400,
        1i64..=10,
    );
    let wide = (
        amount(),
        -1_000_000i64..=1_000_000,
        -100_000i64..=100_000,
        -3i64..=12,
    );
    runner(8192)
        .run(
            &prop_oneof![designed, wide],
            |(annual, residual, period, version)| {
                let hourly = tpf2mp.run("economy_costs.hourlyCharge", (num(annual), num(residual)));
                prop_assert_eq!(check_hourly_charge(&hourly), Outcome::Matched);
                let periodic = tpf2mp.run(
                    "economy_costs.periodCharge",
                    (num(annual), num(residual), num(period)),
                );
                let charged = tpf2mp.run(
                    "economy_costs.charge",
                    (num(annual), num(residual), num(period), num(version)),
                );
                // The port refuses exactly when the charge Lua forms passes
                // 2^53 - 1, which needs a period of over 27 hours.
                let clamped = i128::from(annual.clamp(0, ACCUMULATOR_LIMIT));
                let seconds = i128::from(period.max(0));
                let year = i128::from(FINANCIAL_YEAR_SECONDS);
                let tail = clamped % year * seconds + i128::from(residual.max(0));
                let formed = clamped / year * seconds + tail / year;
                let expected = if formed <= i128::from(MAX_EXACT_INTEGER) {
                    Outcome::Matched
                } else {
                    Outcome::Refused
                };
                prop_assert_eq!(check_period_charge(&periodic), expected);
                let expected = if version < 6 {
                    Outcome::Matched
                } else {
                    expected
                };
                prop_assert_eq!(check_charge(&charged), expected);
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn a_financial_year_of_charges_adds_up_in_both() {
    let tpf2mp = Tpf2mp::new();
    for annual in [
        0,
        1,
        10_799,
        10_800,
        43_800_001,
        8_760_001,
        ACCUMULATOR_LIMIT,
    ] {
        let mut residual = 0;
        let mut total = 0;
        for _ in 0..FINANCIAL_YEAR_SECONDS / 300 {
            let call = tpf2mp.run(
                "economy_costs.periodCharge",
                (num(annual), num(residual), 300.0),
            );
            assert_eq!(check_period_charge(&call), Outcome::Matched);
            total += int(&call.result(1), "charge");
            residual = int(&call.result(2), "residual");
        }
        assert_eq!((total, residual), (annual, 0), "annual {annual}");
    }
    let (charge, residual) = costs::hourly_charge(8_760_001, HOURS_PER_YEAR - 1).unwrap();
    assert_eq!((charge, residual), (1001, 0));
}

#[test]
fn allocate_capital_matches_lua() {
    let tpf2mp = Tpf2mp::new();
    let cid = prop::sample::select(vec![
        "",
        "output:a",
        "output:b",
        "output:c",
        "output:A",
        "output:\u{e4}",
    ]);
    runner(4096)
        .run(
            &(proptest::collection::vec(cid, 0..8), amount()),
            |(cids, total)| {
                let list: Table = tpf2mp.table();
                for cid in &cids {
                    list.push(*cid).unwrap();
                }
                let call = tpf2mp.run("economy_costs.allocateCapital", (list, num(total)));
                prop_assert_eq!(check_allocate_capital(&call), Outcome::Matched);
                Ok(())
            },
        )
        .unwrap();
}
