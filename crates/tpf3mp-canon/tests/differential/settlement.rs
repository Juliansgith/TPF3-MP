//! `economy.lua`: aggregate arithmetic, wallet deltas and the scoreboard's
//! model value.

use mlua::{Table, Value};
use proptest::option;
use proptest::prelude::*;
use tpf3mp_canon::economy::{ACCUMULATOR_LIMIT, settlement};
use tpf3mp_canon::lua::MAX_EXACT_INTEGER;

use crate::tpf2mp::{
    Call, Check, Outcome, Tpf2mp, field, get, num, opt, opt_int, opt_table, runner, same, string,
    table, wide_amount, wide_num,
};

pub const CHECKS: &[(&str, Check)] = &[
    ("economy.saturatingAdd", check_saturating_add),
    ("economy.saturatingMultiply", check_saturating_multiply),
    ("economy.signedAdd", check_signed_add),
    ("economy.walletDeltaDollars", check_wallet_delta_dollars),
];

/// `left or 0`, `tonumber(left) or 0` and `util.integer(left, 0)` all read
/// nil as zero.
fn operands(call: &Call) -> (i64, i64) {
    (
        opt_int(&call.arg(1), "left").unwrap_or(0),
        opt_int(&call.arg(2), "right").unwrap_or(0),
    )
}

pub fn check_saturating_add(call: &Call) -> Outcome {
    let (left, right) = operands(call);
    let context = format!("saturatingAdd({left}, {right})");
    same(
        &context,
        "sum",
        &call.result(1),
        settlement::saturating_add(left, right),
    );
    Outcome::Matched
}

pub fn check_saturating_multiply(call: &Call) -> Outcome {
    let (left, right) = operands(call);
    let context = format!("saturatingMultiply({left}, {right})");
    same(
        &context,
        "product",
        &call.result(1),
        settlement::saturating_multiply(left, right),
    );
    Outcome::Matched
}

pub fn check_signed_add(call: &Call) -> Outcome {
    let (left, right) = operands(call);
    let context = format!("signedAdd({left}, {right})");
    same(
        &context,
        "sum",
        &call.result(1),
        settlement::signed_add(left, right),
    );
    Outcome::Matched
}

pub fn check_wallet_delta_dollars(call: &Call) -> Outcome {
    let company = string(&call.arg(2), "companyCid");
    let residuals = |index| {
        opt_table(
            &get(&table(&index, "state"), "payoutResidCents"),
            "payoutResidCents",
        )
    };
    let carried = residuals(call.arg(1))
        .and_then(|residuals| {
            opt_int(
                &residuals.get::<Value>(company.as_str()).unwrap(),
                "residual",
            )
        })
        .unwrap_or(0);
    let net = opt_int(&call.arg(3), "netRevenueCents").unwrap_or(0);
    let context = format!("walletDeltaDollars({company}, {net}) carrying {carried}");
    let (dollars, residual) = settlement::wallet_delta_dollars(net, carried);
    same(&context, "dollars", &call.result(1), dollars);
    same(&context, "residual", &call.result(2), residual);
    let stored = residuals(call.after(1))
        .unwrap()
        .get::<Value>(company.as_str())
        .unwrap();
    same(&context, "stored residual", &stored, residual);
    Outcome::Matched
}

/// Checks one row of TPF2MP's `scoreboard` against [`settlement::model_value_cents`].
pub fn check_scoreboard_row(row: &Table) {
    let net = field(row, "settledNetRevenueCents");
    let demand = field(row, "settledDemand");
    let reach = field(row, "marketsReached");
    let lines = field(row, "activeLines");
    let context = format!("scoreboard row ({net}, {demand}, {reach}, {lines})");
    same(
        &context,
        "modelValueCents",
        &get(row, "modelValueCents"),
        settlement::model_value_cents(net, demand, reach, lines),
    );
}

/// Any Lua-exact integer, weighted toward the aggregate range.
fn amount() -> impl Strategy<Value = i64> {
    prop_oneof![
        -1000i64..=1000,
        -ACCUMULATOR_LIMIT - 10..=ACCUMULATOR_LIMIT + 10,
        -MAX_EXACT_INTEGER..=MAX_EXACT_INTEGER,
    ]
}

#[test]
fn aggregates_match_lua() {
    let tpf2mp = Tpf2mp::new();
    runner(8192)
        .run(
            &(option::of(amount()), option::of(amount())),
            |(left, right)| {
                for (label, check) in CHECKS.iter().take(3) {
                    let call = tpf2mp.run(label, (opt(left), opt(right)));
                    prop_assert_eq!(check(&call), Outcome::Matched, "{}", label);
                }
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn aggregates_clamp_like_lua_for_any_whole_double() {
    // Lua rounds sums and products beyond 2^53, but only where the result is
    // already beyond the clamp, so these functions stay exact for any
    // operand. TPF2MP relies on it: the scoreboard adds `net * 10`.
    let tpf2mp = Tpf2mp::new();
    runner(4096)
        .run(&(wide_amount(), wide_amount()), |(left, right)| {
            for (label, check) in CHECKS.iter().take(3) {
                let call = tpf2mp.run(label, (wide_num(left), wide_num(right)));
                prop_assert_eq!(check(&call), Outcome::Matched, "{}", label);
            }
            let state = tpf2mp.table();
            let residuals = tpf2mp.table();
            residuals.set("company:1", wide_num(right)).unwrap();
            state.set("payoutResidCents", residuals).unwrap();
            let call = tpf2mp.run(
                "economy.walletDeltaDollars",
                (state, "company:1", wide_num(left)),
            );
            prop_assert_eq!(check_wallet_delta_dollars(&call), Outcome::Matched);
            Ok(())
        })
        .unwrap();
}

#[test]
fn wallet_deltas_match_lua() {
    let tpf2mp = Tpf2mp::new();
    let residual = option::of(prop_oneof![
        -99i64..=99,
        -MAX_EXACT_INTEGER..=MAX_EXACT_INTEGER
    ]);
    runner(8192)
        .run(&(amount(), residual), |(net, residual)| {
            let state = tpf2mp.table();
            let residuals = tpf2mp.table();
            if let Some(residual) = residual {
                residuals.set("company:1", num(residual)).unwrap();
            }
            state.set("payoutResidCents", residuals).unwrap();
            let call = tpf2mp.run("economy.walletDeltaDollars", (state, "company:1", num(net)));
            prop_assert_eq!(check_wallet_delta_dollars(&call), Outcome::Matched);
            Ok(())
        })
        .unwrap();
}

#[test]
fn a_sequence_of_small_losses_carries_cents_not_dollars() {
    let tpf2mp = Tpf2mp::new();
    let state = tpf2mp.table();
    state.set("payoutResidCents", tpf2mp.table()).unwrap();
    let nets = [-1, -1, 150, -250, 99, 1];
    let (mut carried, mut dollars) = (0, 0);
    for net in nets {
        let call = tpf2mp.run(
            "economy.walletDeltaDollars",
            (state.clone(), "company:1", num(net)),
        );
        assert_eq!(check_wallet_delta_dollars(&call), Outcome::Matched);
        let (delta, residual) = settlement::wallet_delta_dollars(net, carried);
        if dollars == 0 && carried == 0 {
            assert_eq!(delta, 0, "a one-cent loss moves no dollar");
        }
        dollars += delta;
        carried = residual;
        // Carry the Lua state forward, as TPF2MP does between settlements.
        let after = table(&call.after(1), "state");
        state
            .set("payoutResidCents", get(&after, "payoutResidCents"))
            .unwrap();
    }
    // No cent is created or lost.
    assert_eq!(dollars * 100 + carried, nets.iter().sum::<i64>());
    assert_eq!((dollars, carried), (-1, 98));
}

/// A company's settled ledger totals and services, for `scoreboard`.
#[derive(Clone, Debug)]
struct Company {
    net: Option<i64>,
    gross: Option<i64>,
    revenue: Option<i64>,
    operating: Option<i64>,
    demand: Option<i64>,
    /// Market index of each service, and whether it is enabled.
    services: Vec<(usize, Option<bool>)>,
}

fn company() -> impl Strategy<Value = Company> {
    // Ledger totals are clamped aggregates. TPF2MP derives a missing net as
    // `gross - operatingCost` without a clamp, which must stay exact.
    let total = option::of(prop_oneof![
        -1000i64..=1000,
        -ACCUMULATOR_LIMIT - 10..=ACCUMULATOR_LIMIT + 10
    ]);
    (
        (total.clone(), total.clone(), total.clone(), total.clone()),
        option::of(prop_oneof![0i64..=1_000_000, 0i64..=ACCUMULATOR_LIMIT]),
        proptest::collection::vec((0usize..4, option::of(any::<bool>())), 0..6),
    )
        .prop_map(
            |((net, gross, revenue, operating), demand, services)| Company {
                net,
                gross,
                revenue,
                operating,
                demand,
                services,
            },
        )
}

#[test]
fn scoreboard_model_value_matches_lua() {
    let tpf2mp = Tpf2mp::new();
    let scoreboard = tpf2mp.module_field("economy", "scoreboard");
    let scoreboard = scoreboard.as_function().unwrap();
    runner(4096)
        .run(&proptest::collection::vec(company(), 1..4), |companies| {
            let state = tpf2mp.table();
            let ledger = tpf2mp.table();
            let totals = tpf2mp.table();
            let services = tpf2mp.table();
            let names = tpf2mp.table();
            for (index, company) in companies.iter().enumerate() {
                let cid = format!("company:{index}");
                let row = tpf2mp.record(&[
                    ("netRevenueCents", company.net),
                    ("grossRevenueCents", company.gross),
                    ("revenueCents", company.revenue),
                    ("operatingCostCents", company.operating),
                    ("demand", company.demand),
                ]);
                totals.set(cid.as_str(), row).unwrap();
                for (line, (market, enabled)) in company.services.iter().enumerate() {
                    let service = tpf2mp.table();
                    service.set("companyCid", cid.as_str()).unwrap();
                    service
                        .set("marketCid", format!("market:{market}"))
                        .unwrap();
                    if let Some(enabled) = enabled {
                        service.set("enabled", *enabled).unwrap();
                    }
                    services
                        .set(format!("line:{index}-{line}"), service)
                        .unwrap();
                }
                let name = tpf2mp.table();
                name.set("name", cid.as_str()).unwrap();
                names.set(cid.as_str(), name).unwrap();
            }
            ledger.set("companies", totals).unwrap();
            state.set("ledger", ledger).unwrap();
            state.set("services", services).unwrap();
            let rows: Table = scoreboard.call((state, names)).unwrap();
            for (index, company) in companies.iter().enumerate() {
                let row: Table = rows.get(format!("company:{index}")).unwrap();
                // `scoreboard` counts only services whose `enabled` is truthy:
                // a missing flag does not count here, unlike feeder access.
                let enabled: Vec<usize> = company
                    .services
                    .iter()
                    .filter(|(_, enabled)| *enabled == Some(true))
                    .map(|(market, _)| *market)
                    .collect();
                let reach = enabled
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len();
                prop_assert_eq!(field(&row, "marketsReached"), i64::try_from(reach).unwrap());
                prop_assert_eq!(
                    field(&row, "activeLines"),
                    i64::try_from(enabled.len()).unwrap()
                );
                check_scoreboard_row(&row);
            }
            Ok(())
        })
        .unwrap();
}
