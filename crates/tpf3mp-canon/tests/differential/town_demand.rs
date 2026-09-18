//! `economy_town_demand.lua`: model towns, gravity demand and growth.
//!
//! The pure functions are compared directly. TPF2MP's state-level
//! functions (`observeMarket`, `refreshMarkets`, `advance`) are replayed
//! through the port's building blocks in TPF2MP's order, which checks that
//! order as well.

use std::collections::BTreeMap;

use mlua::{Table, Value};
use proptest::option;
use proptest::prelude::*;
use tpf3mp_canon::economy::town_demand::{
    self, CarriedMarket, MAX_TOWN_SIZE, SCHEMA_VERSION, Town,
};
use tpf3mp_canon::lua::MAX_EXACT_INTEGER;

use crate::tpf2mp::{
    Call, Check, Outcome, Tpf2mp, field, get, int, int_map, opt, opt_field, opt_int, opt_table,
    runner, same, same_opt, string, table, wide_amount, wide_num,
};

pub const CHECKS: &[(&str, Check)] = &[
    (
        "economy_town_demand.marketSizeFromBuildings",
        check_market_size_from_buildings,
    ),
    ("economy_town_demand.gravityDemand", check_gravity_demand),
    ("economy_town_demand.upsertTown", check_upsert_town),
    ("economy_town_demand.carriedByTown", check_carried_by_town),
    ("economy_town_demand.observeMarket", check_observe_market),
    ("economy_town_demand.refreshMarkets", check_refresh_markets),
    ("economy_town_demand.advance", check_advance),
];

pub fn check_market_size_from_buildings(call: &Call) -> Outcome {
    let buildings = opt_int(&call.arg(1), "buildings");
    let context = format!("marketSizeFromBuildings({buildings:?})");
    same(
        &context,
        "size",
        &call.result(1),
        town_demand::market_size_from_buildings(buildings),
    );
    Outcome::Matched
}

pub fn check_gravity_demand(call: &Call) -> Outcome {
    let [size_a, size_b, distance] =
        [1, 2, 3].map(|index| opt_int(&call.arg(index), "gravityDemand"));
    let context = format!("gravityDemand({size_a:?}, {size_b:?}, {distance:?})");
    same(
        &context,
        "demand",
        &call.result(1),
        town_demand::gravity_demand(size_a, size_b, distance),
    );
    Outcome::Matched
}

/// A string value, as a table key TPF2MP would look up.
fn string_key(value: &Value) -> Option<String> {
    match value {
        Value::String(key) => Some(key.to_str().unwrap().to_owned()),
        _ => None,
    }
}

/// Entries of a string-keyed table, in Lua's sorted-key order.
fn entries(table: &Table, what: &str) -> BTreeMap<String, Table> {
    table
        .pairs::<Value, Table>()
        .map(|pair| {
            let (key, value) = pair.unwrap();
            (string(&key, what), value)
        })
        .collect()
}

/// The town records of a state, by id.
fn towns(state: &Table) -> BTreeMap<String, Town> {
    let Some(towns) = opt_table(&get(state, "towns"), "towns") else {
        return BTreeMap::new();
    };
    entries(&towns, "town cid")
        .into_iter()
        .map(|(cid, record)| {
            let town = Town {
                size: field(&record, "size"),
                growth_resid: field(&record, "growthResid"),
                total_growth: field(&record, "totalGrowth"),
            };
            (cid, town)
        })
        .collect()
}

/// Checks a town record after an update: its fields, schema and id.
fn same_town(context: &str, record: &Table, cid: &str, expected: Town) {
    same(context, "size", &get(record, "size"), expected.size);
    same(
        context,
        "growthResid",
        &get(record, "growthResid"),
        expected.growth_resid,
    );
    same(
        context,
        "totalGrowth",
        &get(record, "totalGrowth"),
        expected.total_growth,
    );
    same(
        context,
        "schemaVersion",
        &get(record, "schemaVersion"),
        SCHEMA_VERSION,
    );
    assert_eq!(string(&get(record, "cid"), "cid"), cid, "{context}: cid");
}

pub fn check_upsert_town(call: &Call) -> Outcome {
    let state = table(&call.arg(1), "state");
    let before = towns(&state);
    let observed = opt_int(&call.arg(3), "observedSize");
    let Some(cid) = string_key(&call.arg(2)).filter(|cid| !cid.is_empty()) else {
        assert!(
            call.result(1).is_nil(),
            "upsertTown of an invalid id returns nil"
        );
        assert_eq!(
            towns(&table(&call.after(1), "state")),
            before,
            "and changes nothing"
        );
        return Outcome::Matched;
    };
    let context = format!(
        "upsertTown({cid}, {observed:?}) over {:?}",
        before.get(&cid)
    );
    let expected = town_demand::observe_town(before.get(&cid).copied(), observed);
    let after = table(&get(&table(&call.after(1), "state"), "towns"), "towns");
    same_town(
        &context,
        &after.get::<Table>(cid.as_str()).unwrap(),
        &cid,
        expected,
    );
    same_town(&context, &table(&call.result(1), "record"), &cid, expected);
    Outcome::Matched
}

/// Passengers each town carried, as `carriedByTown` computes them: passenger
/// markets with two string towns, each service row's `delivered or
/// allocated` (nil counting as 0), markets and rows in id order.
fn carried_by_town(markets: Option<&Table>, results: &Table) -> BTreeMap<String, i64> {
    let result_markets = opt_table(&get(results, "markets"), "results.markets");
    let mut selected = Vec::new();
    for (cid, result) in result_markets
        .iter()
        .flat_map(|markets| entries(markets, "market cid"))
    {
        let Some(market) = markets
            .and_then(|markets| opt_table(&markets.get::<Value>(cid.as_str()).unwrap(), "market"))
        else {
            continue;
        };
        let cargo = string_key(&get(&market, "kind")).is_some_and(|kind| kind == "cargo");
        let metadata = opt_table(&get(&market, "metadata"), "metadata");
        let town = |name: &str| {
            metadata
                .as_ref()
                .and_then(|metadata| string_key(&get(metadata, name)))
        };
        let (Some(town_a), Some(town_b), false) = (town("townA"), town("townB"), cargo) else {
            continue;
        };
        let rows = opt_table(&get(&result, "services"), "services");
        let delivered: Vec<i64> = rows
            .iter()
            .flat_map(|rows| entries(rows, "line cid").into_values())
            .map(|row| {
                opt_field(&row, "delivered")
                    .or_else(|| opt_field(&row, "allocated"))
                    .unwrap_or(0)
            })
            .collect();
        selected.push((town_a, town_b, delivered));
    }
    let markets: Vec<CarriedMarket> = selected
        .iter()
        .map(|(town_a, town_b, delivered)| CarriedMarket {
            town_a,
            town_b,
            delivered,
        })
        .collect();
    town_demand::carried_by_town(&markets)
        .unwrap()
        .into_iter()
        .map(|(town, carried)| (town.to_owned(), carried))
        .collect()
}

pub fn check_carried_by_town(call: &Call) -> Outcome {
    let state = table(&call.arg(1), "state");
    let markets = opt_table(&get(&state, "markets"), "markets");
    let expected = carried_by_town(markets.as_ref(), &table(&call.arg(2), "results"));
    assert_eq!(
        int_map(&table(&call.result(1), "carried"), "carried"),
        expected,
        "carriedByTown"
    );
    Outcome::Matched
}

pub fn check_observe_market(call: &Call) -> Outcome {
    let before = towns(&table(&call.arg(1), "state"));
    let towns_after = towns(&table(&call.after(1), "state"));
    let metadata = opt_table(&call.arg(2), "market")
        .and_then(|market| opt_table(&get(&market, "metadata"), "metadata"));
    let pair = metadata.as_ref().and_then(|metadata| {
        string_key(&get(metadata, "townA")).zip(string_key(&get(metadata, "townB")))
    });
    let (Some(metadata), Some((town_a, town_b))) = (metadata, pair) else {
        assert_eq!(
            call.result(1),
            Value::Boolean(false),
            "observeMarket without two towns"
        );
        assert_eq!(
            towns_after, before,
            "observeMarket without two towns changes nothing"
        );
        return Outcome::Matched;
    };
    let context = format!("observeMarket({town_a}, {town_b})");
    // Two upserts in order; a market whose towns are equal upserts one
    // record twice.
    let mut expected = before;
    let first = town_demand::observe_town(
        expected.get(&town_a).copied(),
        opt_field(&metadata, "townSizeA"),
    );
    expected.insert(town_a.clone(), first);
    let second = town_demand::observe_town(
        expected.get(&town_b).copied(),
        opt_field(&metadata, "townSizeB"),
    );
    expected.insert(town_b.clone(), second);
    assert_eq!(towns_after, expected, "{context}: towns");
    // Lua reads both sizes after both upserts from the record tables, which
    // alias when the towns are equal: then townSizeA is the second size too.
    let metadata_after = table(
        &get(&table(&call.after(2), "market"), "metadata"),
        "metadata",
    );
    same(
        &context,
        "townSizeA",
        &get(&metadata_after, "townSizeA"),
        expected[&town_a].size,
    );
    same(
        &context,
        "townSizeB",
        &get(&metadata_after, "townSizeB"),
        expected[&town_b].size,
    );
    assert_eq!(call.result(1), Value::Boolean(true), "{context}");
    Outcome::Matched
}

/// Checks `refreshMarkets` over `towns`: every market whose two towns have
/// records is refreshed, in id order, and the others are untouched. Returns
/// the expected changes (previous and new demand) by market id.
fn check_refresh(
    context: &str,
    towns: &BTreeMap<String, Town>,
    before: Option<&Table>,
    after: Option<&Table>,
) -> BTreeMap<String, (i64, i64)> {
    let mut changes = BTreeMap::new();
    for (cid, market) in before
        .iter()
        .flat_map(|markets| entries(markets, "market cid"))
    {
        let market_after: Table = after.unwrap().get(cid.as_str()).unwrap();
        let metadata = opt_table(&get(&market, "metadata"), "metadata");
        let town = |name: &str| {
            metadata
                .as_ref()
                .and_then(|metadata| string_key(&get(metadata, name)))
                .and_then(|cid| towns.get(&cid).copied())
        };
        let (Some(metadata), Some(first), Some(second)) =
            (metadata.as_ref(), town("townA"), town("townB"))
        else {
            same_opt(
                context,
                "untouched demand",
                &get(&market_after, "demand"),
                opt_field(&market, "demand"),
            );
            continue;
        };
        let refresh = town_demand::refresh_market_demand(
            field(&market, "demand"),
            opt_field(metadata, "networkDemand"),
            opt_field(metadata, "directDemand"),
            first.size,
            second.size,
            opt_field(metadata, "corridorMeters"),
        );
        let context = format!("{context}: market {cid}");
        same(
            &context,
            "demand",
            &get(&market_after, "demand"),
            refresh.demand,
        );
        let metadata_after = table(&get(&market_after, "metadata"), "metadata");
        same(
            &context,
            "directDemand",
            &get(&metadata_after, "directDemand"),
            refresh.direct_demand,
        );
        same(
            &context,
            "townSizeA",
            &get(&metadata_after, "townSizeA"),
            first.size,
        );
        same(
            &context,
            "townSizeB",
            &get(&metadata_after, "townSizeB"),
            second.size,
        );
        if refresh.demand != refresh.previous_demand {
            changes.insert(cid, (refresh.previous_demand, refresh.demand));
        }
    }
    changes
}

fn same_changes(context: &str, lua: &Table, expected: &BTreeMap<String, (i64, i64)>) {
    let lua: BTreeMap<String, (i64, i64)> = entries(lua, "market cid")
        .into_iter()
        .map(|(cid, change)| {
            (
                cid,
                (field(&change, "previousDemand"), field(&change, "demand")),
            )
        })
        .collect();
    assert_eq!(&lua, expected, "{context}: demand changes");
}

pub fn check_refresh_markets(call: &Call) -> Outcome {
    let before = table(&call.arg(1), "state");
    let after = table(&call.after(1), "state");
    let changes = check_refresh(
        "refreshMarkets",
        &towns(&before),
        opt_table(&get(&before, "markets"), "markets").as_ref(),
        opt_table(&get(&after, "markets"), "markets").as_ref(),
    );
    same_changes(
        "refreshMarkets",
        &table(&call.result(1), "changes"),
        &changes,
    );
    Outcome::Matched
}

pub fn check_advance(call: &Call) -> Outcome {
    let before = table(&call.arg(1), "state");
    let after = table(&call.after(1), "state");
    let markets_before = opt_table(&get(&before, "markets"), "markets");
    let carried = carried_by_town(markets_before.as_ref(), &table(&call.arg(2), "results"));
    let returned = table(&call.result(1), "growth");
    same(
        "advance",
        "schemaVersion",
        &get(&returned, "schemaVersion"),
        SCHEMA_VERSION,
    );
    let growth_rows = table(&get(&returned, "towns"), "towns");
    let lua_towns = table(&get(&after, "towns"), "towns");
    let mut expected_towns = towns(&before);
    for (cid, amount) in &carried {
        let record = expected_towns
            .get(cid)
            .copied()
            .unwrap_or_else(|| town_demand::observe_town(None, None));
        let (grown, growth) = town_demand::grow_town(record, *amount).unwrap();
        let context = format!("advance: town {cid} carried {amount}");
        same_town(
            &context,
            &lua_towns.get::<Table>(cid.as_str()).unwrap(),
            cid,
            grown,
        );
        let row: Table = growth_rows.get(cid.as_str()).unwrap();
        same(&context, "carried", &get(&row, "carried"), growth.carried);
        same(
            &context,
            "previousSize",
            &get(&row, "previousSize"),
            growth.previous_size,
        );
        same(&context, "size", &get(&row, "size"), growth.size);
        same(&context, "gained", &get(&row, "gained"), growth.gained);
        same(
            &context,
            "growthResid",
            &get(&row, "growthResid"),
            growth.growth_resid,
        );
        expected_towns.insert(cid.clone(), grown);
    }
    assert_eq!(
        growth_rows.pairs::<Value, Value>().count(),
        carried.len(),
        "advance: growth rows"
    );
    // Towns that carried nobody are untouched.
    assert_eq!(towns(&after), expected_towns, "advance: towns");
    let changes = check_refresh(
        "advance",
        &expected_towns,
        markets_before.as_ref(),
        opt_table(&get(&after, "markets"), "markets").as_ref(),
    );
    same_changes(
        "advance",
        &table(&get(&returned, "markets"), "markets"),
        &changes,
    );
    Outcome::Matched
}

const TOWN_IDS: [&str; 4] = ["town:a", "town:b", "town:c", "town:\u{e4}"];

/// A corridor market of a [`World`].
#[derive(Clone, Debug)]
struct MarketSpec {
    cargo: bool,
    /// Indices into [`TOWN_IDS`]; `None` leaves the town out.
    town_a: Option<usize>,
    town_b: Option<usize>,
    demand: i64,
    network_demand: Option<i64>,
    direct_demand: Option<i64>,
    corridor_meters: Option<i64>,
    size_a: Option<i64>,
    size_b: Option<i64>,
    /// Service rows of its settlement result: `delivered` and `allocated`.
    rows: Vec<(Option<i64>, Option<i64>)>,
}

/// Towns and corridor markets for the state-level functions. Values stray
/// outside TPF2MP's clamps, which the functions must clamp identically.
#[derive(Clone, Debug)]
struct World {
    /// Size, growth residual and total growth of each town in [`TOWN_IDS`];
    /// `None` for a town without a record.
    towns: Vec<Option<(i64, i64, i64)>>,
    markets: Vec<MarketSpec>,
}

fn market_spec() -> impl Strategy<Value = MarketSpec> {
    let town = option::of(0usize..TOWN_IDS.len());
    let row = (option::of(-5i64..=200_000), option::of(0i64..=200_000));
    (
        prop::bool::weighted(0.25),
        (town.clone(), town),
        prop_oneof![0i64..=2000, 0i64..=1_200_000_000],
        (option::of(0i64..=5000), option::of(-100i64..=5000)),
        option::of(prop_oneof![0i64..=30_000, -1000i64..=1_000_000_000]),
        (option::of(-10i64..=200_000), option::of(-10i64..=200_000)),
        proptest::collection::vec(row, 0..4),
    )
        .prop_map(
            |(
                cargo,
                (town_a, town_b),
                demand,
                (network_demand, direct_demand),
                corridor_meters,
                (size_a, size_b),
                rows,
            )| {
                MarketSpec {
                    cargo,
                    town_a,
                    town_b,
                    demand,
                    network_demand,
                    direct_demand,
                    corridor_meters,
                    size_a,
                    size_b,
                    rows,
                }
            },
        )
}

fn world() -> impl Strategy<Value = World> {
    let record = option::of((
        prop_oneof![1i64..=MAX_TOWN_SIZE, -10i64..=200_000],
        prop_oneof![0i64..400, -10i64..=1000],
        prop_oneof![0i64..=1000, -10i64..=200_000],
    ));
    (
        proptest::collection::vec(record, TOWN_IDS.len()),
        proptest::collection::vec(market_spec(), 0..5),
    )
        .prop_map(|(towns, markets)| World { towns, markets })
}

impl World {
    fn market_cid(index: usize) -> String {
        format!("market:{index}")
    }

    /// The parts of an economy state these functions read: towns, markets.
    fn state(&self, tpf2mp: &Tpf2mp) -> Table {
        let state = tpf2mp.table();
        let towns = tpf2mp.table();
        for (cid, record) in TOWN_IDS.iter().zip(&self.towns) {
            if let Some((size, growth_resid, total_growth)) = record {
                let record = tpf2mp.record(&[
                    ("size", Some(*size)),
                    ("growthResid", Some(*growth_resid)),
                    ("totalGrowth", Some(*total_growth)),
                    ("schemaVersion", Some(SCHEMA_VERSION)),
                ]);
                record.set("cid", *cid).unwrap();
                towns.set(*cid, record).unwrap();
            }
        }
        state.set("towns", towns).unwrap();
        let markets = tpf2mp.table();
        for (index, spec) in self.markets.iter().enumerate() {
            let market = tpf2mp.record(&[("demand", Some(spec.demand))]);
            market
                .set("kind", if spec.cargo { "cargo" } else { "passenger" })
                .unwrap();
            let metadata = tpf2mp.record(&[
                ("networkDemand", spec.network_demand),
                ("directDemand", spec.direct_demand),
                ("corridorMeters", spec.corridor_meters),
                ("townSizeA", spec.size_a),
                ("townSizeB", spec.size_b),
            ]);
            if let Some(town) = spec.town_a {
                metadata.set("townA", TOWN_IDS[town]).unwrap();
            }
            if let Some(town) = spec.town_b {
                metadata.set("townB", TOWN_IDS[town]).unwrap();
            }
            market.set("metadata", metadata).unwrap();
            markets.set(Self::market_cid(index), market).unwrap();
        }
        state.set("markets", markets).unwrap();
        state
    }

    /// Settlement results: each market's service rows.
    fn results(&self, tpf2mp: &Tpf2mp) -> Table {
        let markets = tpf2mp.table();
        for (index, spec) in self.markets.iter().enumerate() {
            let services = tpf2mp.table();
            for (line, (delivered, allocated)) in spec.rows.iter().enumerate() {
                let row = tpf2mp.record(&[("delivered", *delivered), ("allocated", *allocated)]);
                services.set(format!("line:{index}-{line}"), row).unwrap();
            }
            let result = tpf2mp.table();
            result.set("services", services).unwrap();
            markets.set(Self::market_cid(index), result).unwrap();
        }
        let results = tpf2mp.table();
        results.set("markets", markets).unwrap();
        results
    }
}

/// Any Lua-exact integer, weighted toward small ones.
fn any_count() -> impl Strategy<Value = i64> {
    prop_oneof![-10i64..=200_000, -MAX_EXACT_INTEGER..=MAX_EXACT_INTEGER]
}

#[test]
fn market_size_and_gravity_demand_match_lua() {
    let tpf2mp = Tpf2mp::new();
    runner(4096)
        .run(&option::of(any_count()), |buildings| {
            let call = tpf2mp.run(
                "economy_town_demand.marketSizeFromBuildings",
                opt(buildings),
            );
            prop_assert_eq!(check_market_size_from_buildings(&call), Outcome::Matched);
            Ok(())
        })
        .unwrap();
    let inputs = (
        option::of(any_count()),
        option::of(any_count()),
        option::of(any_count()),
    );
    runner(8192)
        .run(&inputs, |(size_a, size_b, distance)| {
            let call = tpf2mp.run(
                "economy_town_demand.gravityDemand",
                (opt(size_a), opt(size_b), opt(distance)),
            );
            prop_assert_eq!(check_gravity_demand(&call), Outcome::Matched);
            Ok(())
        })
        .unwrap();
    // Sizes are clamped first, a building count only doubles twice before
    // its clamp, and a distance long enough to round leaves a zero quotient:
    // both functions are exact for any operand.
    runner(4096)
        .run(
            &(wide_amount(), wide_amount(), wide_amount()),
            |(first, second, distance)| {
                let call = tpf2mp.run(
                    "economy_town_demand.marketSizeFromBuildings",
                    wide_num(first),
                );
                prop_assert_eq!(check_market_size_from_buildings(&call), Outcome::Matched);
                let call = tpf2mp.run(
                    "economy_town_demand.gravityDemand",
                    (wide_num(first), wide_num(second), wide_num(distance)),
                );
                prop_assert_eq!(check_gravity_demand(&call), Outcome::Matched);
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn town_observation_matches_lua() {
    let tpf2mp = Tpf2mp::new();
    let cid = prop::sample::select(vec!["town:a", "town:b", ""]);
    runner(4096)
        .run(
            &(world(), cid, option::of(any_count())),
            |(world, cid, observed)| {
                let call = tpf2mp.run(
                    "economy_town_demand.upsertTown",
                    (world.state(&tpf2mp), cid, opt(observed)),
                );
                prop_assert_eq!(check_upsert_town(&call), Outcome::Matched);
                Ok(())
            },
        )
        .unwrap();
    runner(2048)
        .run(&world(), |world| {
            let state = world.state(&tpf2mp);
            let markets: Table = state.get("markets").unwrap();
            for index in 0..world.markets.len() {
                let market: Table = markets.get(World::market_cid(index)).unwrap();
                let call = tpf2mp.run("economy_town_demand.observeMarket", (state.clone(), market));
                prop_assert_eq!(check_observe_market(&call), Outcome::Matched);
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn carried_passengers_match_lua() {
    let tpf2mp = Tpf2mp::new();
    runner(4096)
        .run(&world(), |world| {
            let call = tpf2mp.run(
                "economy_town_demand.carriedByTown",
                (world.state(&tpf2mp), world.results(&tpf2mp)),
            );
            prop_assert_eq!(check_carried_by_town(&call), Outcome::Matched);
            Ok(())
        })
        .unwrap();
}

#[test]
fn growth_and_demand_refresh_match_lua() {
    let tpf2mp = Tpf2mp::new();
    runner(4096)
        .run(&world(), |world| {
            let call = tpf2mp.run("economy_town_demand.refreshMarkets", world.state(&tpf2mp));
            prop_assert_eq!(check_refresh_markets(&call), Outcome::Matched);
            let call = tpf2mp.run(
                "economy_town_demand.advance",
                (world.state(&tpf2mp), world.results(&tpf2mp)),
            );
            prop_assert_eq!(check_advance(&call), Outcome::Matched);
            Ok(())
        })
        .unwrap();
}

#[test]
fn constants_match_lua() {
    let tpf2mp = Tpf2mp::new();
    let constants = [
        ("SCHEMA_VERSION", town_demand::SCHEMA_VERSION),
        (
            "NOMINAL_CAPACITY_PER_BUILDING",
            town_demand::NOMINAL_CAPACITY_PER_BUILDING,
        ),
        (
            "FALLBACK_TOWN_BUILDINGS",
            town_demand::FALLBACK_TOWN_BUILDINGS,
        ),
        ("GRAVITY_DIVISOR", town_demand::GRAVITY_DIVISOR),
        ("MIN_DEMAND", town_demand::MIN_DEMAND),
        ("MAX_DEMAND", town_demand::MAX_DEMAND),
        ("MAX_TOWN_SIZE", town_demand::MAX_TOWN_SIZE),
        (
            "GROWTH_PASSENGERS_PER_BUILDING",
            town_demand::GROWTH_PASSENGERS_PER_BUILDING,
        ),
    ];
    for (name, value) in constants {
        assert_eq!(
            int(&tpf2mp.module_field("economy_town_demand", name), name),
            value,
            "{name}"
        );
    }
}
