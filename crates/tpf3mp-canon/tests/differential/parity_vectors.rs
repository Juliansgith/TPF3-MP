//! TPF2MP's parity vectors (`tests/run_economy_parity_vectors.lua`) as
//! conformance tests: the generator runs unmodified, every call it makes to a
//! ported function is recorded, and every recorded call is replayed through
//! the port. The generator's 109 scenarios are TPF2MP's own cross-language
//! vectors: a 40-settlement demo, hand-written edge scenarios (legacy model
//! versions, fare shocks, maximum revenue, negative glides, capacity
//! cascades, clamps, cargo, town growth, feeder access) and seeded fuzz.

use std::collections::BTreeMap;

use mlua::{Table, Value};

use crate::tpf2mp::{Call, Check, Outcome, Tpf2mp, field, get, string, table};
use crate::{
    allocation, costs, difficulty, feeder_access, flow, market, revenue, settlement, town_demand,
};

/// Every ported function the scenarios reach. The others
/// (`defaultFareCents`, `vehicleAnnualUpkeepCents`, `allocateCapital`,
/// `marketSizeFromBuildings`) are called only from TPF2MP's engine glue and
/// are covered by the property tests.
const REACHED: [&str; 30] = [
    "economy.saturatingAdd",
    "economy.saturatingMultiply",
    "economy.signedAdd",
    "economy.walletDeltaDollars",
    "economy_allocation.capacityConstrained",
    "economy_allocation.proportional",
    "economy_costs.charge",
    "economy_costs.hourlyCharge",
    "economy_costs.infrastructureAnnualUpkeepCents",
    "economy_costs.periodCharge",
    "economy_difficulty.apply",
    "economy_difficulty.multiplier",
    "economy_difficulty.normaliseKey",
    "economy_feeder_access.buildIndex",
    "economy_feeder_access.cents",
    "economy_flow.evaluateMarket",
    "economy_flow.generalizedCost",
    "economy_flow.glide",
    "economy_flow.logitWeight",
    "economy_flow.scaledRate",
    "economy_flow.signedAdd",
    "economy_revenue.modelDeliveryCents",
    "economy_revenue.passengerDeliveryCents",
    "economy_revenue.saturatingMultiply",
    "economy_town_demand.advance",
    "economy_town_demand.carriedByTown",
    "economy_town_demand.gravityDemand",
    "economy_town_demand.observeMarket",
    "economy_town_demand.refreshMarkets",
    "economy_town_demand.upsertTown",
];

/// The checker of every hooked function, by label.
fn checks() -> BTreeMap<&'static str, Check> {
    [
        flow::CHECKS,
        market::CHECKS,
        allocation::CHECKS,
        revenue::CHECKS,
        costs::CHECKS,
        difficulty::CHECKS,
        town_demand::CHECKS,
        feeder_access::CHECKS,
        settlement::CHECKS,
    ]
    .into_iter()
    .flatten()
    .map(|(label, check)| (*label, *check))
    .collect()
}

#[test]
fn every_hooked_function_has_a_check() {
    let tpf2mp = Tpf2mp::new();
    let checks = checks();
    let hooked: Vec<String> = table(&get(tpf2mp.harness(), "originals"), "originals")
        .pairs::<Value, Value>()
        .map(|pair| string(&pair.unwrap().0, "label"))
        .collect();
    for label in &hooked {
        assert!(checks.contains_key(label.as_str()), "no check for {label}");
    }
    assert_eq!(hooked.len(), checks.len(), "a check has no hooked function");
}

#[test]
fn tpf2mp_parity_vectors_replay_identically() {
    let tpf2mp = Tpf2mp::new();
    let (vectors, trace) = tpf2mp.run_parity_vectors();
    assert_eq!(field(&vectors, "schema"), 1);
    let scenarios = table(&get(&vectors, "scenarios"), "scenarios");
    assert_eq!(
        scenarios.raw_len(),
        109,
        "TPF2MP's generator defines 109 scenarios"
    );
    assert_eq!(
        string(&get(tpf2mp.harness(), "printed"), "printed"),
        "PASS generated 109 Lua economy parity scenarios"
    );

    let checks = checks();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for record in trace.sequence_values::<Table>() {
        let call = Call::from_record(&record.unwrap());
        let (label, check) = checks
            .get_key_value(call.label.as_str())
            .unwrap_or_else(|| panic!("no check for {}", call.label));
        assert_eq!(
            check(&call),
            Outcome::Matched,
            "the port refused a call the parity vectors make: {label}"
        );
        *counts.entry(label).or_default() += 1;
    }
    let calls: usize = counts.values().sum();

    // The scoreboard's model value is computed inline in `scoreboard`; its
    // rows are part of the vectors.
    let mut scoreboard_rows = 0;
    for scenario in scenarios.sequence_values::<Table>() {
        let scoreboard = table(&get(&scenario.unwrap(), "scoreboard"), "scoreboard");
        for row in scoreboard.pairs::<Value, Table>() {
            settlement::check_scoreboard_row(&row.unwrap().1);
            scoreboard_rows += 1;
        }
    }

    let reached: Vec<&str> = counts.keys().copied().collect();
    assert_eq!(
        reached, REACHED,
        "the functions the scenarios reach changed"
    );
    println!(
        "parity vectors: {} scenarios, {calls} recorded calls replayed, {scoreboard_rows} scoreboard rows",
        scenarios.raw_len(),
    );
    for (label, count) in &counts {
        println!("  {count:>6}  {label}");
    }
}
