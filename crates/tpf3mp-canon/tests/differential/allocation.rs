//! `economy_allocation.lua`: largest-remainder choice and capacity admission.

use std::collections::BTreeMap;

use mlua::Table;
use proptest::prelude::*;
use tpf3mp_canon::economy::SHARE_SCALE;
use tpf3mp_canon::economy::allocation::{self, CapacityOption, OUTSIDE_CID};

use crate::tpf2mp::{
    Call, Check, Outcome, Tpf2mp, field, get, int, int_map, num, opt_table, runner, same, string,
    table,
};

pub const CHECKS: &[(&str, Check)] = &[
    ("economy_allocation.proportional", check_proportional),
    (
        "economy_allocation.capacityConstrained",
        check_capacity_constrained,
    ),
];

pub fn check_proportional(call: &Call) -> Outcome {
    let total = int(&call.arg(1), "total");
    let items: Vec<(String, i64)> = table(&call.arg(2), "items")
        .sequence_values::<Table>()
        .map(|item| {
            let item = item.unwrap();
            (string(&get(&item, "cid"), "cid"), field(&item, "weight"))
        })
        .collect();
    let context = format!("proportional({total}, {items:?})");
    let Some(port) = allocation::proportional(total, &items) else {
        return Outcome::Refused;
    };
    let lua = int_map(&table(&call.result(1), "allocations"), &context);
    assert_eq!(lua, port, "{context}");
    Outcome::Matched
}

pub fn check_capacity_constrained(call: &Call) -> Outcome {
    let demand = int(&call.arg(1), "demand");
    let options: Vec<(String, i64, i64)> = table(&call.arg(2), "services")
        .sequence_values::<Table>()
        .map(|option| {
            let option = option.unwrap();
            let service = table(&get(&option, "service"), "service");
            (
                string(&get(&option, "cid"), "cid"),
                field(&service, "sharePpm"),
                field(&option, "availableCapacity"),
            )
        })
        .collect();
    let outside_ppm = int(&call.arg(3), "outsidePpm");
    let version = int(&call.arg(4), "version");
    let context = format!("capacityConstrained({demand}, {options:?}, {outside_ppm}, {version})");
    let services: Vec<CapacityOption> = options
        .iter()
        .map(|(cid, share_ppm, available_capacity)| CapacityOption {
            cid,
            share_ppm: *share_ppm,
            available_capacity: *available_capacity,
        })
        .collect();
    let Some(port) = allocation::capacity_constrained(demand, &services, outside_ppm, version)
    else {
        return Outcome::Refused;
    };
    let owned = |map: &BTreeMap<&str, i64>| -> BTreeMap<String, i64> {
        map.iter()
            .map(|(cid, amount)| ((*cid).to_owned(), *amount))
            .collect()
    };
    let allocations = int_map(&table(&call.result(1), "allocations"), &context);
    assert_eq!(
        allocations,
        owned(&port.allocations),
        "{context}: allocations"
    );
    let requested =
        opt_table(&call.result(2), "requested").map(|requested| int_map(&requested, &context));
    assert_eq!(
        requested,
        port.requested.as_ref().map(owned),
        "{context}: requested"
    );
    same(&context, "queued", &call.result(3), port.queued);
    Outcome::Matched
}

fn run_proportional(tpf2mp: &Tpf2mp, total: i64, items: &[(&str, i64)]) -> Call {
    let list = tpf2mp.table();
    for (cid, weight) in items {
        let item = tpf2mp.record(&[("weight", Some(*weight))]);
        item.set("cid", *cid).unwrap();
        list.push(item).unwrap();
    }
    tpf2mp.run("economy_allocation.proportional", (num(total), list))
}

fn run_capacity_constrained(
    tpf2mp: &Tpf2mp,
    demand: i64,
    services: &[(&str, i64, i64)],
    outside_ppm: i64,
    version: i64,
) -> Call {
    let list = tpf2mp.table();
    for (cid, share_ppm, available_capacity) in services {
        let option = tpf2mp.record(&[("availableCapacity", Some(*available_capacity))]);
        option.set("cid", *cid).unwrap();
        option
            .set("service", tpf2mp.record(&[("sharePpm", Some(*share_ppm))]))
            .unwrap();
        list.push(option).unwrap();
    }
    tpf2mp.run(
        "economy_allocation.capacityConstrained",
        (num(demand), list, num(outside_ppm), num(version)),
    )
}

/// Ids that exercise tie-breaking: the outside option, plain line ids, an
/// upper-case and a non-ASCII id, and repeats.
fn cid() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
        OUTSIDE_CID,
        "line:a",
        "line:b",
        "line:c",
        "line:d",
        "line:A",
        "line:\u{e4}",
    ])
}

#[test]
fn proportional_matches_lua() {
    let tpf2mp = Tpf2mp::new();
    let weight = prop_oneof![0i64..=SHARE_SCALE, 0i64..=65_536, 0i64..=3];
    let total = prop_oneof![0i64..=20, 0i64..=1_000_000_000];
    let designed = (total, proptest::collection::vec((cid(), weight), 0..7));
    runner(8192)
        .run(&designed, |(total, items)| {
            let call = run_proportional(&tpf2mp, total, &items);
            prop_assert_eq!(check_proportional(&call), Outcome::Matched);
            Ok(())
        })
        .unwrap();
    // Negative totals and weights still keep every product exact.
    let wide = (
        -1000i64..=1_000_000_000,
        proptest::collection::vec((cid(), -SHARE_SCALE..=SHARE_SCALE), 0..7),
    );
    runner(4096)
        .run(&wide, |(total, items)| {
            let call = run_proportional(&tpf2mp, total, &items);
            prop_assert_eq!(check_proportional(&call), Outcome::Matched);
            Ok(())
        })
        .unwrap();
}

#[test]
fn leftover_units_follow_remainders_then_ids() {
    let tpf2mp = Tpf2mp::new();
    let cases: [(i64, &[(&str, i64)]); 5] = [
        (4, &[("line:b", 1), ("line:a", 1), ("line:c", 1)]),
        (1, &[(OUTSIDE_CID, 1), ("line:z", 1)]),
        (7, &[("line:a", 0), ("line:b", 0), ("line:c", 5)]),
        (10, &[("line:a", 3), ("line:a", 1)]),
        (5, &[("line:a", -1), ("line:b", 3)]),
    ];
    for (total, items) in cases {
        let call = run_proportional(&tpf2mp, total, items);
        assert_eq!(
            check_proportional(&call),
            Outcome::Matched,
            "{total} {items:?}"
        );
    }
}

#[test]
fn capacity_constrained_matches_lua() {
    let tpf2mp = Tpf2mp::new();
    let service = (
        cid(),
        0i64..=SHARE_SCALE,
        prop_oneof![0i64..=50, 0i64..=10_000_000],
    );
    let strategy = (
        prop_oneof![0i64..=100, 0i64..=10_000_000],
        proptest::collection::vec(service, 0..6),
        0i64..=SHARE_SCALE,
        1i64..=10,
    );
    runner(8192)
        .run(&strategy, |(demand, services, outside_ppm, version)| {
            let call = run_capacity_constrained(&tpf2mp, demand, &services, outside_ppm, version);
            prop_assert_eq!(check_capacity_constrained(&call), Outcome::Matched);
            Ok(())
        })
        .unwrap();
}

#[test]
fn legacy_admission_resplits_over_several_rounds() {
    let tpf2mp = Tpf2mp::new();
    let services = [
        ("line:a", 400_000, 10),
        ("line:b", 300_000, 40),
        ("line:c", 200_000, 1_000),
    ];
    for version in [8, 9] {
        let call = run_capacity_constrained(&tpf2mp, 200, &services, 100_000, version);
        assert_eq!(
            check_capacity_constrained(&call),
            Outcome::Matched,
            "version {version}"
        );
    }
    // With no weight at all, legacy admission drops the demand entirely.
    let call = run_capacity_constrained(&tpf2mp, 50, &[("line:a", 0, 10)], 0, 8);
    assert_eq!(check_capacity_constrained(&call), Outcome::Matched);
    assert!(table(&call.result(1), "allocations").is_empty());
}

/// Exact largest-remainder allocation in 128-bit integers, for comparison
/// with Lua beyond its exact range.
fn exact_proportional(total: i64, items: &[(&str, i64)]) -> BTreeMap<String, i64> {
    let weight_sum: i128 = items.iter().map(|(_, weight)| i128::from(*weight)).sum();
    let mut allocations = BTreeMap::new();
    let mut ranked = Vec::new();
    let mut used = 0i128;
    for (cid, weight) in items {
        let numerator = i128::from(total) * i128::from(*weight);
        let base = numerator.div_euclid(weight_sum);
        allocations.insert((*cid).to_owned(), i64::try_from(base).unwrap());
        used += base;
        ranked.push((*cid, numerator.rem_euclid(weight_sum)));
    }
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    for index in 0..usize::try_from(i128::from(total) - used).unwrap() {
        *allocations.get_mut(ranked[index % ranked.len()].0).unwrap() += 1;
    }
    allocations
}

#[test]
fn lua_misallocates_once_products_pass_two_to_the_53() {
    // A day-long interval admits up to 1e9 * 24 riders per market, and
    // 2.4e10 * 999998 ppm is past 2^53. Lua then ranks rounded remainders and
    // can hand the leftover rider to the wrong option. The port refuses.
    let tpf2mp = Tpf2mp::new();
    let items = [("line:a", 999_998), (OUTSIDE_CID, 1)];
    // Totals of 500,000 mod 999,999 leave one rider over and give the outside
    // option a remainder one above line:a's. Past 2^54 a double holds only
    // multiples of four, so Lua's `total * 999998` can round up by two, and
    // then line:a takes the rider.
    let divergent = (18_101i64..18_165)
        .map(|k| 500_000 + k * 999_999)
        .find(|total| {
            let call = run_proportional(&tpf2mp, *total, &items);
            let lua = int_map(&table(&call.result(1), "allocations"), "allocations");
            assert_eq!(check_proportional(&call), Outcome::Refused);
            lua != exact_proportional(*total, &items)
        });
    assert!(
        divergent.is_some(),
        "expected Lua to misallocate somewhere past 2^53"
    );
}
