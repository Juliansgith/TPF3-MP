//! `economy_feeder_access.lua`: company-owned local feeder access.

use std::collections::BTreeMap;

use mlua::{Table, Value};
use proptest::option;
use proptest::prelude::*;
use tpf3mp_canon::economy::MarketKind;
use tpf3mp_canon::economy::feeder_access::{self, FeederIndex, FeederMarket, FeederService};

use crate::tpf2mp::{
    Call, Check, Outcome, Tpf2mp, get, opt_int, opt_table, runner, same, string, table,
};

pub const CHECKS: &[(&str, Check)] = &[
    ("economy_feeder_access.buildIndex", check_build_index),
    ("economy_feeder_access.cents", check_cents),
];

fn opt_str(value: &Value, what: &str) -> Option<String> {
    (!value.is_nil()).then(|| string(value, what))
}

/// A Lua list of strings. TPF2MP's lists are proper sequences of strings; a
/// hole or another type would make Lua's `#` and indexing diverge from a
/// `Vec`, so it fails the test instead.
fn strings(value: &Value, what: &str) -> Option<Vec<String>> {
    let list = opt_table(value, what)?;
    let items: Vec<String> = list
        .clone()
        .sequence_values::<Value>()
        .map(|item| string(&item.unwrap(), what))
        .collect();
    assert_eq!(
        list.pairs::<Value, Value>().count(),
        items.len(),
        "{what} is not a sequence"
    );
    Some(items)
}

fn metadata(record: &Table) -> Option<Table> {
    opt_table(&get(record, "metadata"), "metadata")
}

fn metadata_str(metadata: Option<&Table>, name: &str) -> Option<String> {
    metadata.and_then(|metadata| opt_str(&get(metadata, name), name))
}

pub fn feeder_market(market: &Table) -> FeederMarket {
    let metadata = metadata(market);
    let cargo = matches!(get(market, "kind"), Value::String(kind) if kind.as_bytes() == b"cargo".as_slice());
    FeederMarket {
        kind: if cargo {
            MarketKind::Cargo
        } else {
            MarketKind::Passenger
        },
        market_scope: metadata_str(metadata.as_ref(), "marketScope"),
        town_a: metadata_str(metadata.as_ref(), "townA"),
        town_b: metadata_str(metadata.as_ref(), "townB"),
    }
}

pub fn feeder_service(service: &Table) -> FeederService {
    let metadata = metadata(service);
    let list = |name: &str| {
        metadata
            .as_ref()
            .and_then(|metadata| strings(&get(metadata, name), name))
    };
    FeederService {
        market_cid: string(&get(service, "marketCid"), "marketCid"),
        company_cid: string(&get(service, "companyCid"), "companyCid"),
        // `service.enabled ~= false`: a missing flag counts as enabled here.
        enabled: get(service, "enabled") != Value::Boolean(false),
        // `tonumber(service.capacity) or 0` and `util.integer(headway, 86400)`
        capacity: opt_int(&get(service, "capacity"), "capacity").unwrap_or(0),
        headway_seconds: opt_int(&get(service, "headwaySeconds"), "headwaySeconds")
            .unwrap_or(86_400),
        carrier: metadata_str(metadata.as_ref(), "carrier"),
        market_scope: metadata_str(metadata.as_ref(), "marketScope"),
        endpoint_town_cids: list("endpointTownCids"),
        station_group_cids: list("stationGroupCids"),
    }
}

fn entries(table: &Table, what: &str) -> BTreeMap<String, Table> {
    table
        .pairs::<Value, Table>()
        .map(|pair| {
            let (key, value) = pair.unwrap();
            (string(&key, what), value)
        })
        .collect()
}

/// Markets and services of a state, as feeder access reads them.
pub fn feeder_state(
    state: &Table,
) -> (
    BTreeMap<String, FeederMarket>,
    BTreeMap<String, FeederService>,
) {
    let section = |name: &str| {
        opt_table(&get(state, name), name)
            .map(|table| entries(&table, name))
            .unwrap_or_default()
    };
    let markets = section("markets")
        .into_iter()
        .map(|(cid, market)| (cid, feeder_market(&market)))
        .collect();
    let services = section("services")
        .into_iter()
        .map(|(cid, service)| (cid, feeder_service(&service)))
        .collect();
    (markets, services)
}

fn lua_index(index: &Table) -> FeederIndex {
    entries(index, "company")
        .into_iter()
        .map(|(company, towns)| {
            let towns = entries(&towns, "town")
                .into_iter()
                .map(|(town, stations)| {
                    let stations = stations
                        .pairs::<Value, Value>()
                        .map(|pair| {
                            let (station, cents) = pair.unwrap();
                            (
                                string(&station, "station"),
                                crate::tpf2mp::int(&cents, "cents"),
                            )
                        })
                        .collect();
                    (town, stations)
                })
                .collect();
            (company, towns)
        })
        .collect()
}

pub fn check_build_index(call: &Call) -> Outcome {
    let (markets, services) = feeder_state(&table(&call.arg(1), "state"));
    let port = feeder_access::build_index(&markets, &services);
    assert_eq!(
        lua_index(&table(&call.result(1), "index")),
        port,
        "buildIndex over {services:?}"
    );
    Outcome::Matched
}

pub fn check_cents(call: &Call) -> Outcome {
    let market = feeder_market(&table(&call.arg(1), "market"));
    let service = feeder_service(&table(&call.arg(2), "service"));
    let index = lua_index(&table(&call.arg(3), "index"));
    let context = format!("cents({market:?}, {service:?})");
    let (total, count) = feeder_access::cents(&market, &service, &index);
    same(&context, "cents", &call.result(1), total);
    same(&context, "endpoints", &call.result(2), count);
    Outcome::Matched
}

/// A service of a random feeder world.
#[derive(Clone, Debug)]
struct ServiceSpec {
    market: usize,
    company: usize,
    enabled: Option<bool>,
    capacity: i64,
    headway: i64,
    carrier: Option<&'static str>,
    scope: Option<&'static str>,
    towns: Option<Vec<&'static str>>,
    groups: Option<Vec<&'static str>>,
}

const TOWNS: [&str; 3] = ["town:a", "town:b", "town:\u{e4}"];
const GROUPS: [&str; 5] = [
    "group:a",
    "group:b",
    "group:suburb",
    "group:\u{e4}",
    "group:A",
];

/// A market of the random feeder world.
struct MarketFixture {
    cid: &'static str,
    kind: &'static str,
    scope: Option<&'static str>,
    towns: Option<(&'static str, &'static str)>,
}

/// A corridor between two towns, a local market in each, a market without
/// metadata, and a cargo corridor.
const MARKETS: [MarketFixture; 5] = [
    MarketFixture {
        cid: "market:corridor",
        kind: "passenger",
        scope: Some("corridor"),
        towns: Some((TOWNS[0], TOWNS[1])),
    },
    MarketFixture {
        cid: "market:local-a",
        kind: "passenger",
        scope: Some("local"),
        towns: Some((TOWNS[0], TOWNS[0])),
    },
    MarketFixture {
        cid: "market:local-b",
        kind: "passenger",
        scope: Some("local"),
        towns: Some((TOWNS[1], TOWNS[1])),
    },
    MarketFixture {
        cid: "market:bare",
        kind: "passenger",
        scope: None,
        towns: None,
    },
    MarketFixture {
        cid: "market:freight",
        kind: "cargo",
        scope: Some("corridor"),
        towns: Some((TOWNS[0], TOWNS[2])),
    },
];

fn markets(tpf2mp: &Tpf2mp) -> Table {
    let markets = tpf2mp.table();
    for fixture in &MARKETS {
        let market = tpf2mp.table();
        market.set("kind", fixture.kind).unwrap();
        let metadata = tpf2mp.table();
        if let Some(scope) = fixture.scope {
            metadata.set("marketScope", scope).unwrap();
        }
        if let Some((town_a, town_b)) = fixture.towns {
            metadata.set("townA", town_a).unwrap();
            metadata.set("townB", town_b).unwrap();
        }
        market.set("metadata", metadata).unwrap();
        markets.set(fixture.cid, market).unwrap();
    }
    markets
}

/// The markets services may name: the fixtures, and one that does not exist.
const MARKET_IDS: [&str; 6] = [
    MARKETS[0].cid,
    MARKETS[1].cid,
    MARKETS[2].cid,
    MARKETS[3].cid,
    MARKETS[4].cid,
    "market:unknown",
];

fn service_spec() -> impl Strategy<Value = ServiceSpec> {
    let carrier = option::of(prop::sample::select(vec![
        "ROAD", "TRAM", "RAIL", "WATER", "road",
    ]));
    // Usually the market's scope applies; sometimes the service overrides it.
    let scope = prop::option::weighted(
        0.2,
        prop::sample::select(vec!["local", "corridor", "regional"]),
    );
    let towns = option::of(proptest::collection::vec(
        prop::sample::select(TOWNS.to_vec()),
        0..3,
    ));
    let groups = option::of(proptest::collection::vec(
        prop::sample::select(GROUPS.to_vec()),
        0..5,
    ));
    (
        (0..MARKET_IDS.len(), 0usize..3, option::of(any::<bool>())),
        (
            prop_oneof![Just(0i64), 1i64..=200, -5i64..=1_000_000_000],
            prop_oneof![1i64..=1000, -10i64..=100_000],
        ),
        (carrier, scope),
        (towns, groups),
    )
        .prop_map(
            |(
                (market, company, enabled),
                (capacity, headway),
                (carrier, scope),
                (towns, groups),
            )| ServiceSpec {
                market,
                company,
                enabled,
                capacity,
                headway,
                carrier,
                scope,
                towns,
                groups,
            },
        )
}

fn services(tpf2mp: &Tpf2mp, specs: &[ServiceSpec]) -> Table {
    let services = tpf2mp.table();
    for (index, spec) in specs.iter().enumerate() {
        let service = tpf2mp.record(&[
            ("capacity", Some(spec.capacity)),
            ("headwaySeconds", Some(spec.headway)),
        ]);
        service.set("marketCid", MARKET_IDS[spec.market]).unwrap();
        service
            .set("companyCid", format!("company:{}", spec.company))
            .unwrap();
        if let Some(enabled) = spec.enabled {
            service.set("enabled", enabled).unwrap();
        }
        let metadata = tpf2mp.table();
        if let Some(carrier) = spec.carrier {
            metadata.set("carrier", carrier).unwrap();
        }
        if let Some(scope) = spec.scope {
            metadata.set("marketScope", scope).unwrap();
        }
        if let Some(towns) = &spec.towns {
            metadata
                .set(
                    "endpointTownCids",
                    tpf2mp
                        .lua()
                        .create_sequence_from(towns.iter().copied())
                        .unwrap(),
                )
                .unwrap();
        }
        if let Some(groups) = &spec.groups {
            metadata
                .set(
                    "stationGroupCids",
                    tpf2mp
                        .lua()
                        .create_sequence_from(groups.iter().copied())
                        .unwrap(),
                )
                .unwrap();
        }
        service.set("metadata", metadata).unwrap();
        services.set(format!("line:{index}"), service).unwrap();
    }
    services
}

#[test]
fn feeder_access_matches_lua() {
    let tpf2mp = Tpf2mp::new();
    runner(4096)
        .run(&proptest::collection::vec(service_spec(), 0..9), |specs| {
            let state = tpf2mp.table();
            let markets = markets(&tpf2mp);
            let services = services(&tpf2mp, &specs);
            state.set("markets", markets.clone()).unwrap();
            state.set("services", services.clone()).unwrap();
            let call = tpf2mp.run("economy_feeder_access.buildIndex", state);
            prop_assert_eq!(check_build_index(&call), Outcome::Matched);
            let index = call.result(1);
            for (index_in_list, spec) in specs.iter().enumerate() {
                let Ok(market) = markets.get::<Table>(MARKET_IDS[spec.market]) else {
                    continue;
                };
                let service: Table = services.get(format!("line:{index_in_list}")).unwrap();
                let call = tpf2mp.run(
                    "economy_feeder_access.cents",
                    (market, service, index.clone()),
                );
                prop_assert_eq!(check_cents(&call), Outcome::Matched);
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn feeders_never_stack_and_duplicate_endpoints_count_once() {
    let tpf2mp = Tpf2mp::new();
    let specs = [
        ServiceSpec {
            market: 1,
            company: 0,
            enabled: Some(true),
            capacity: 300,
            headway: 600,
            carrier: Some("ROAD"),
            scope: None,
            towns: None,
            groups: Some(vec!["group:suburb", "group:a"]),
        },
        ServiceSpec {
            market: 1,
            company: 0,
            enabled: None,
            capacity: 90,
            headway: 300,
            carrier: Some("TRAM"),
            scope: None,
            towns: None,
            groups: Some(vec!["group:a", "group:A", "group:a"]),
        },
        ServiceSpec {
            market: 0,
            company: 0,
            enabled: Some(true),
            capacity: 600,
            headway: 600,
            carrier: Some("RAIL"),
            scope: None,
            towns: Some(vec![TOWNS[0], TOWNS[0]]),
            groups: Some(vec!["group:a", "group:b", "group:a"]),
        },
    ];
    let state = tpf2mp.table();
    let markets = markets(&tpf2mp);
    let services = services(&tpf2mp, &specs);
    state.set("markets", markets.clone()).unwrap();
    state.set("services", services.clone()).unwrap();
    let call = tpf2mp.run("economy_feeder_access.buildIndex", state);
    assert_eq!(check_build_index(&call), Outcome::Matched);
    let port_index = lua_index(&table(&call.result(1), "index"));
    // The better of the two feeders at group:a counts: 150 (headway 600
    // scores 150) beats the tram's capacity of 90.
    assert_eq!(port_index["company:0"]["town:a"]["group:a"], 150);
    let rail: Table = services.get("line:2").unwrap();
    let corridor: Table = markets.get("market:corridor").unwrap();
    let call = tpf2mp.run(
        "economy_feeder_access.cents",
        (corridor, rail, call.result(1)),
    );
    assert_eq!(check_cents(&call), Outcome::Matched);
    // Both endpoints are town:a/group:a, which counts once.
    assert_eq!(crate::tpf2mp::exact_int(&call.result(1)), Some(150));
    assert_eq!(crate::tpf2mp::exact_int(&call.result(2)), Some(1));
}

#[test]
fn cents_per_endpoint_matches_lua() {
    let tpf2mp = Tpf2mp::new();
    let lua = crate::tpf2mp::int(
        &tpf2mp.module_field("economy_feeder_access", "CENTS_PER_ENDPOINT"),
        "CENTS_PER_ENDPOINT",
    );
    assert_eq!(lua, feeder_access::CENTS_PER_ENDPOINT);
}
