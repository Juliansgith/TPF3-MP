//! `economy_flow.lua` `evaluateMarket`, replayed through the port.
//!
//! `evaluateMarket` is TPF2MP's settlement step for one market. It reads and
//! writes TPF2MP's state layout, so it is not ported as one function; every
//! number it produces comes from a ported building block. The replay here
//! composes those blocks in `evaluateMarket`'s order (the recipe in
//! `docs/ECONOMY.md`) and requires every result field and every state change
//! to match, which checks the order as well as the blocks.

use std::collections::BTreeMap;

use mlua::{Table, Value};
use proptest::option;
use proptest::prelude::*;
use tpf3mp_canon::economy::allocation::{self, CapacityOption, OUTSIDE_CID};
use tpf3mp_canon::economy::flow::{
    self, CostParams, GeneralizedCost, MarketCost, ServiceCost, ShareParams, ShareStock,
};
use tpf3mp_canon::economy::{
    MarketKind, SHARE_SCALE, costs, difficulty, feeder_access, revenue, settlement,
};

use crate::feeder_access::{feeder_market, feeder_service, feeder_state};
use crate::flow::same_factors;
use crate::tpf2mp::{
    Call, Check, Outcome, Tpf2mp, field, get, num, opt, opt_field, opt_int, opt_table, runner,
    same, same_opt, string, table,
};

pub const CHECKS: &[(&str, Check)] = &[("economy_flow.evaluateMarket", check_evaluate_market)];

/// Lua truthiness: everything but nil and false.
fn truthy(value: &Value) -> bool {
    !matches!(value, Value::Nil | Value::Boolean(false))
}

/// What the port computes for one evaluated service.
struct ServiceReplay {
    cid: String,
    service: Table,
    cost: GeneralizedCost,
    /// Feeder access endpoints (model version 8 and later).
    access_endpoints: Option<i64>,
    available_capacity: i64,
    /// New capacity residual (model version 6 and later).
    capacity_resid: Option<i64>,
    equilibrium_ppm: i64,
    share: ShareStock,
    allocated: i64,
    requested: i64,
    lag_load_ppm: i64,
    raw_gross: i64,
    gross: i64,
    /// New revenue multiplier residual (model version 7 and later).
    multiplier_resid: Option<i64>,
    /// Annual upkeep as the result reports it: 0 when costed per vehicle.
    annual: Option<i64>,
    charge: i64,
    /// New upkeep residual; `None` when costed per vehicle (unchanged).
    upkeep_resid: Option<i64>,
    net: i64,
    share_basis_points: i64,
}

/// What the port computes for the market.
struct MarketReplay {
    version: i64,
    period: i64,
    demand: i64,
    demand_resid: Option<i64>,
    outside: i64,
    outside_ppm: i64,
    requested_outside: Option<i64>,
    queued: i64,
    services: Vec<ServiceReplay>,
}

/// Replays `evaluateMarket(state, market_cid, nil, period)` through the
/// port. `None` when a building block refuses.
fn replay(
    state: &Table,
    market: &Table,
    market_cid: &str,
    period: Option<i64>,
) -> Option<MarketReplay> {
    // `util.integer(state.version, 1)`
    let version = opt_field(state, "version").unwrap_or(1);
    let params = table(&get(state, "params"), "params");
    let period = flow::period_seconds(period, version);
    let (demand, demand_resid) = if version >= 6 {
        let (demand, resid) = flow::scaled_rate(
            field(market, "demand"),
            opt_field(market, "demandResid").unwrap_or(0),
            period,
        )?;
        (demand, Some(resid))
    } else {
        (field(market, "demand"), None)
    };
    let feeder_index = (version >= 8).then(|| {
        let (markets, services) = feeder_state(state);
        feeder_access::build_index(&markets, &services)
    });
    let cost_params = CostParams {
        max_wait_seconds: field(&params, "maxWaitSeconds"),
        transfer_seconds: field(&params, "transferSeconds"),
        crowd_threshold_ppm: field(&params, "crowdThresholdPpm"),
    };
    let market_cost = MarketCost {
        vot_cents_per_hour: field(market, "votCentsPerHour"),
        wait_weight_pm: opt_field(market, "waitWeightPm"),
        transfer_seconds: opt_field(market, "transferSeconds"),
    };

    // Enabled services of this market, in id order.
    let all_services = table(&get(state, "services"), "services");
    let mut chosen: BTreeMap<String, Table> = BTreeMap::new();
    for pair in all_services.pairs::<Value, Table>() {
        let (cid, service) = pair.unwrap();
        let in_market = matches!(get(&service, "marketCid"), Value::String(cid) if cid.to_str().unwrap() == market_cid);
        if truthy(&get(&service, "enabled")) && in_market {
            chosen.insert(string(&cid, "line cid"), service);
        }
    }
    let mut services = Vec::new();
    for (cid, service) in chosen {
        let access = feeder_index.as_ref().map(|index| {
            feeder_access::cents(&feeder_market(market), &feeder_service(&service), index)
        });
        let service_cost = ServiceCost {
            headway_seconds: field(&service, "headwaySeconds"),
            journey_seconds: field(&service, "journeySeconds"),
            transfers: field(&service, "transfers"),
            lag_load_ppm: opt_field(&service, "lagLoadPpm").unwrap_or(0),
            quality: field(&service, "quality"),
            fare_cents: field(&service, "fareCents"),
        };
        let cost = flow::generalized_cost(
            &cost_params,
            &market_cost,
            &service_cost,
            access.map(|(cents, _)| cents),
        )?;
        let (available_capacity, capacity_resid) = if version >= 6 {
            let (capacity, resid) = flow::scaled_rate(
                field(&service, "capacity"),
                opt_field(&service, "capacityResid").unwrap_or(0),
                period,
            )?;
            (capacity, Some(resid))
        } else {
            (field(&service, "capacity"), None)
        };
        services.push((
            cid,
            service,
            cost,
            access,
            available_capacity,
            capacity_resid,
        ));
    }

    // Logit equilibrium over the outside option and the services.
    let gc_outside = field(market, "gcOutsideCents");
    let theta = field(market, "thetaCents");
    let gc_min = services
        .iter()
        .map(|(_, _, cost, ..)| cost.gc_cents)
        .fold(gc_outside, i64::min);
    let cutoff = flow::logit_cutoff_weight(version);
    let mut weights = vec![(
        OUTSIDE_CID.to_owned(),
        flow::logit_weight(gc_outside, gc_min, theta, cutoff)?,
    )];
    for (cid, _, cost, ..) in &services {
        weights.push((
            cid.clone(),
            flow::logit_weight(cost.gc_cents, gc_min, theta, cutoff)?,
        ));
    }
    let equilibria = allocation::proportional(SHARE_SCALE, &weights)?;

    // Share movement.
    let share_params = ShareParams {
        alpha_up_pm: field(&params, "alphaUpPm"),
        alpha_down_pm: field(&params, "alphaDownPm"),
    };
    let mut shares = Vec::new();
    for (cid, service, cost, ..) in &services {
        let stock = ShareStock {
            share_ppm: opt_field(service, "sharePpm"),
            // `service.shareResid or 0`
            share_resid: opt_field(service, "shareResid").unwrap_or(0),
            last_fare_cents: opt_field(service, "lastFareCents"),
        };
        let equilibrium = equilibria.get(cid).copied().unwrap_or(0);
        shares.push((
            equilibrium,
            flow::move_share(&stock, equilibrium, cost.fare_cents, &share_params, version)?,
        ));
    }
    let share_values: Vec<i64> = shares
        .iter()
        .map(|(_, share)| share.share_ppm.unwrap())
        .collect();
    let outside_ppm = flow::outside_share_ppm(&share_values)?;

    // Choice and capacity admission.
    let options: Vec<CapacityOption> = services
        .iter()
        .zip(&share_values)
        .map(|((cid, _, _, _, available, _), share)| CapacityOption {
            cid,
            share_ppm: *share,
            available_capacity: *available,
        })
        .collect();
    let admitted = allocation::capacity_constrained(demand, &options, outside_ppm, version)?;

    // Revenue and costs per service.
    let kind = match get(market, "kind") {
        Value::String(kind) if kind.as_bytes() == b"cargo".as_slice() => MarketKind::Cargo,
        _ => MarketKind::Passenger,
    };
    let vehicle_costs = opt_table(&get(state, "vehicleCosts"), "vehicleCosts");
    let mut replayed = Vec::new();
    for (
        (cid, service, cost, access, available_capacity, capacity_resid),
        (equilibrium_ppm, share),
    ) in services.iter().zip(&shares)
    {
        let (available_capacity, equilibrium_ppm) = (*available_capacity, *equilibrium_ppm);
        let allocated = admitted.allocations.get(cid.as_str()).copied().unwrap_or(0);
        let requested = match &admitted.requested {
            Some(requested) => requested.get(cid.as_str()).copied().unwrap_or(0),
            None => allocated,
        };
        let lag_load_ppm = flow::lag_load_ppm(requested, available_capacity)?;
        let metadata = opt_table(&get(service, "metadata"), "metadata");
        let distance = metadata
            .as_ref()
            .and_then(|metadata| opt_int(&get(metadata, "distanceMeters"), "distanceMeters"));
        let raw_gross =
            revenue::model_delivery_cents(kind, distance, field(service, "fareCents"), allocated)?;
        let (gross, multiplier_resid) = if version >= 7 {
            let (gross, resid) = difficulty::apply(
                raw_gross,
                opt_field(&params, "revenueMultiplierPpm").unwrap_or(difficulty::SCALE),
                opt_field(service, "revenueMultiplierResid").unwrap_or(0),
            );
            (gross, Some(resid))
        } else {
            (raw_gross, None)
        };
        // A service is costed per vehicle when any of its vehicles has a
        // cost record; the vehicles then carry the upkeep instead.
        let managed = metadata
            .as_ref()
            .and_then(|metadata| opt_table(&get(metadata, "vehicleCids"), "vehicleCids"))
            .is_some_and(|vehicles| {
                vehicles.sequence_values::<Value>().any(|vehicle| {
                    let vehicle = vehicle.unwrap();
                    vehicle_costs
                        .as_ref()
                        .is_some_and(|costs| truthy(&costs.get::<Value>(vehicle).unwrap()))
                })
            });
        let (annual, residual) = if managed {
            (Some(0), 0)
        } else {
            (
                opt_field(service, "annualVehicleUpkeepCents"),
                opt_field(service, "upkeepResid").unwrap_or(0),
            )
        };
        let (charge, upkeep_resid) = costs::charge(annual.unwrap_or(0), residual, period, version)?;
        replayed.push(ServiceReplay {
            cid: cid.clone(),
            service: service.clone(),
            cost: *cost,
            access_endpoints: access.map(|(_, endpoints)| endpoints),
            available_capacity,
            capacity_resid: *capacity_resid,
            equilibrium_ppm,
            share: *share,
            allocated,
            requested,
            lag_load_ppm,
            raw_gross,
            gross,
            multiplier_resid,
            annual,
            charge,
            upkeep_resid: (!managed).then_some(upkeep_resid),
            net: settlement::signed_add(gross, -charge),
            share_basis_points: flow::share_basis_points(allocated, demand)?,
        });
    }
    Some(MarketReplay {
        version,
        period,
        demand,
        demand_resid,
        outside: admitted.allocations.get(OUTSIDE_CID).copied().unwrap_or(0),
        outside_ppm,
        requested_outside: admitted
            .requested
            .as_ref()
            .map(|requested| requested.get(OUTSIDE_CID).copied().unwrap_or(0)),
        queued: admitted.queued,
        services: replayed,
    })
}

/// Lua values that `evaluateMarket` copies through unchanged.
#[track_caller]
fn same_value(context: &str, what: &str, lua: &Value, expected: &Value) {
    assert_eq!(lua, expected, "{context}: {what} differs");
}

pub fn check_evaluate_market(call: &Call) -> Outcome {
    let state = table(&call.arg(1), "state");
    let market_cid = string(&call.arg(2), "marketCid");
    assert!(
        call.arg(3).is_nil(),
        "delivery snapshots are outside the port"
    );
    let period = opt_int(&call.arg(4), "periodSeconds");
    let markets = table(&get(&state, "markets"), "markets");
    let Some(market) = opt_table(
        &markets.get::<Value>(market_cid.as_str()).unwrap(),
        "market",
    ) else {
        assert!(call.result(1).is_nil(), "an unknown market has no result");
        assert_eq!(string(&call.result(2), "error"), "unknown market");
        return Outcome::Matched;
    };
    let Some(replay) = replay(&state, &market, &market_cid, period) else {
        return Outcome::Refused;
    };
    let context = format!("evaluateMarket({market_cid}) at version {}", replay.version);
    let context = context.as_str();
    let v6 = replay.version >= 6;
    let v7 = replay.version >= 7;
    let v9 = replay.version >= 9;

    let result = table(&call.result(1), "result");
    assert_eq!(
        string(&get(&result, "marketCid"), "marketCid"),
        market_cid,
        "{context}"
    );
    same_value(
        context,
        "name",
        &get(&result, "name"),
        &get(&market, "name"),
    );
    same_value(
        context,
        "kind",
        &get(&result, "kind"),
        &get(&market, "kind"),
    );
    same(context, "demand", &get(&result, "demand"), replay.demand);
    same_opt(
        context,
        "hourlyDemand",
        &get(&result, "hourlyDemand"),
        v6.then(|| field(&market, "demand")),
    );
    same_opt(
        context,
        "intervalSeconds",
        &get(&result, "intervalSeconds"),
        v6.then_some(replay.period),
    );
    same(
        context,
        "gcOutsideCents",
        &get(&result, "gcOutsideCents"),
        field(&market, "gcOutsideCents"),
    );
    same(
        context,
        "thetaCents",
        &get(&result, "thetaCents"),
        field(&market, "thetaCents"),
    );
    same(context, "outside", &get(&result, "outside"), replay.outside);
    same(
        context,
        "outsidePpm",
        &get(&result, "outsidePpm"),
        replay.outside_ppm,
    );
    same_opt(
        context,
        "requestedOutside",
        &get(&result, "requestedOutside"),
        replay.requested_outside,
    );
    same_opt(
        context,
        "queued",
        &get(&result, "queued"),
        v9.then_some(replay.queued),
    );

    let rows = table(&get(&result, "services"), "services");
    assert_eq!(
        rows.pairs::<Value, Value>().count(),
        replay.services.len(),
        "{context}: service rows"
    );
    let state_after = table(&call.after(1), "state");
    let markets_after = table(&get(&state_after, "markets"), "markets");
    let market_after: Table = markets_after.get(market_cid.as_str()).unwrap();
    let expected_resid = replay
        .demand_resid
        .or_else(|| opt_field(&market, "demandResid"));
    same_opt(
        context,
        "market demandResid",
        &get(&market_after, "demandResid"),
        expected_resid,
    );
    let services_after = table(&get(&state_after, "services"), "services");

    for service in &replay.services {
        let context = format!("{context}, service {}", service.cid);
        let context = context.as_str();
        let row: Table = rows.get(service.cid.as_str()).unwrap();
        let before = &service.service;
        let after: Table = services_after.get(service.cid.as_str()).unwrap();
        assert_eq!(
            string(&get(&row, "lineCid"), "lineCid"),
            service.cid,
            "{context}"
        );
        for passed in ["companyCid", "name", "capacity", "fareCents"] {
            same_value(context, passed, &get(&row, passed), &get(before, passed));
        }
        same(
            context,
            "allocated",
            &get(&row, "allocated"),
            service.allocated,
        );
        same(
            context,
            "delivered",
            &get(&row, "delivered"),
            service.allocated,
        );
        same(
            context,
            "availableCapacity",
            &get(&row, "availableCapacity"),
            service.available_capacity,
        );
        same(
            context,
            "revenueCents",
            &get(&row, "revenueCents"),
            service.gross,
        );
        same(
            context,
            "grossRevenueCents",
            &get(&row, "grossRevenueCents"),
            service.gross,
        );
        same_opt(
            context,
            "rawGrossRevenueCents",
            &get(&row, "rawGrossRevenueCents"),
            v7.then_some(service.raw_gross),
        );
        let multiplier = if v7 {
            get(
                &table(&get(&state, "params"), "params"),
                "revenueMultiplierPpm",
            )
        } else {
            Value::Nil
        };
        same_value(
            context,
            "revenueMultiplierPpm",
            &get(&row, "revenueMultiplierPpm"),
            &multiplier,
        );
        same_opt(
            context,
            "revenueMultiplierResid",
            &get(&row, "revenueMultiplierResid"),
            service.multiplier_resid,
        );
        same_opt(
            context,
            "annualVehicleUpkeepCents",
            &get(&row, "annualVehicleUpkeepCents"),
            service.annual,
        );
        same(
            context,
            "vehicleUpkeepCents",
            &get(&row, "vehicleUpkeepCents"),
            service.charge,
        );
        same(
            context,
            "operatingCostCents",
            &get(&row, "operatingCostCents"),
            service.charge,
        );
        same(
            context,
            "netRevenueCents",
            &get(&row, "netRevenueCents"),
            service.net,
        );
        let upkeep_resid = service
            .upkeep_resid
            .or_else(|| opt_field(before, "upkeepResid"));
        same_opt(
            context,
            "upkeepResid",
            &get(&row, "upkeepResid"),
            upkeep_resid,
        );
        same(
            context,
            "shareBasisPoints",
            &get(&row, "shareBasisPoints"),
            service.share_basis_points,
        );
        same_opt(
            context,
            "sharePpm",
            &get(&row, "sharePpm"),
            service.share.share_ppm,
        );
        same(
            context,
            "shareResid",
            &get(&row, "shareResid"),
            service.share.share_resid,
        );
        same(
            context,
            "equilibriumPpm",
            &get(&row, "equilibriumPpm"),
            service.equilibrium_ppm,
        );
        same(
            context,
            "lagLoadPpm",
            &get(&row, "lagLoadPpm"),
            service.lag_load_ppm,
        );
        same_opt(
            context,
            "requested",
            &get(&row, "requested"),
            v9.then_some(service.requested),
        );
        let overflow = (service.requested - service.allocated).max(0);
        same_opt(
            context,
            "capacityOverflow",
            &get(&row, "capacityOverflow"),
            v9.then_some(overflow),
        );
        let factors = table(&get(&row, "factors"), "factors");
        let extra = match service.access_endpoints {
            Some(endpoints) => {
                same(
                    context,
                    "feederAccessEndpoints",
                    &get(&factors, "feederAccessEndpoints"),
                    endpoints,
                );
                1
            }
            None => 0,
        };
        same_factors(context, &factors, &service.cost, extra);

        // The stocks and residuals the service carries to the next settlement.
        let capacity_resid = service
            .capacity_resid
            .or_else(|| opt_field(before, "capacityResid"));
        same_opt(
            context,
            "capacityResid after",
            &get(&after, "capacityResid"),
            capacity_resid,
        );
        same_opt(
            context,
            "sharePpm after",
            &get(&after, "sharePpm"),
            service.share.share_ppm,
        );
        same(
            context,
            "shareResid after",
            &get(&after, "shareResid"),
            service.share.share_resid,
        );
        same_opt(
            context,
            "lastFareCents after",
            &get(&after, "lastFareCents"),
            service.share.last_fare_cents,
        );
        same(
            context,
            "lagLoadPpm after",
            &get(&after, "lagLoadPpm"),
            service.lag_load_ppm,
        );
        let multiplier_resid = service
            .multiplier_resid
            .or_else(|| opt_field(before, "revenueMultiplierResid"));
        same_opt(
            context,
            "revenueMultiplierResid after",
            &get(&after, "revenueMultiplierResid"),
            multiplier_resid,
        );
        same_opt(
            context,
            "upkeepResid after",
            &get(&after, "upkeepResid"),
            upkeep_resid,
        );
    }

    // Services of other markets, and disabled ones, are untouched.
    let services_before = table(&get(&state, "services"), "services");
    for pair in services_before.pairs::<Value, Table>() {
        let (cid, before) = pair.unwrap();
        let cid = string(&cid, "line cid");
        if replay.services.iter().any(|service| service.cid == cid) {
            continue;
        }
        let after: Table = services_after.get(cid.as_str()).unwrap();
        for stock in [
            "sharePpm",
            "shareResid",
            "lagLoadPpm",
            "capacityResid",
            "upkeepResid",
            "revenueMultiplierResid",
            "lastFareCents",
        ] {
            same_value(context, stock, &get(&after, stock), &get(&before, stock));
        }
    }
    Outcome::Matched
}

/// A service of a random market.
#[derive(Clone, Debug)]
struct ServiceCase {
    /// Belongs to another market, which the evaluation must ignore.
    other_market: bool,
    /// `None` leaves `enabled` nil, which `evaluateMarket` treats as off.
    enabled: Option<bool>,
    timing: (i64, i64, i64),
    fare_cents: i64,
    capacity: (i64, i64),
    quality: i64,
    lag_load_ppm: i64,
    share: (Option<i64>, i64, Option<i64>),
    upkeep: (i64, i64, i64),
    distance: Option<i64>,
    /// Lists a vehicle that has a cost record.
    managed: bool,
}

/// A random market state within TPF2MP's upsert clamps. Hourly demand stays
/// at or below 10^8, which keeps every product exact at any interval.
#[derive(Clone, Debug)]
struct MarketCase {
    version: i64,
    params: (i64, i64, i64, i64, i64, i64),
    cargo: bool,
    demand: (i64, i64),
    vot: i64,
    gc_outside: i64,
    theta: i64,
    kind_weights: (Option<i64>, Option<i64>),
    period: Option<i64>,
    services: Vec<ServiceCase>,
}

fn service_case() -> impl Strategy<Value = ServiceCase> {
    (
        (
            prop::bool::weighted(0.15),
            prop_oneof![8 => Just(Some(true)), 1 => Just(Some(false)), 1 => Just(None)],
        ),
        (30i64..=86_400, 30i64..=604_800, 0i64..=8),
        prop_oneof![0i64..=5000, 0i64..=100_000_000],
        (
            prop_oneof![Just(0i64), 0i64..=2000, 0i64..=100_000_000],
            0i64..=3599,
        ),
        (
            0i64..=1000,
            prop_oneof![0i64..=2 * SHARE_SCALE, 0i64..=1_000_000_000_000],
        ),
        (
            option::of(0i64..=SHARE_SCALE),
            0i64..=999,
            option::of(prop_oneof![0i64..=5000, 0i64..=100_000_000]),
        ),
        (
            prop_oneof![0i64..=10_000_000, 0i64..=1_000_000_000_000_000],
            0i64..10_800,
            0i64..1_000_000,
        ),
        (
            option::of(prop_oneof![-100i64..=1_000_000, 0i64..=100_000_000]),
            prop::bool::weighted(0.2),
        ),
    )
        .prop_map(
            |(
                (other_market, enabled),
                timing,
                fare_cents,
                capacity,
                (quality, lag_load_ppm),
                share,
                upkeep,
                (distance, managed),
            )| {
                ServiceCase {
                    other_market,
                    enabled,
                    timing,
                    fare_cents,
                    capacity,
                    quality,
                    lag_load_ppm,
                    share,
                    upkeep,
                    distance,
                    managed,
                }
            },
        )
}

fn market_case() -> impl Strategy<Value = MarketCase> {
    let multiplier = prop_oneof![
        prop::sample::select(vec![600_000i64, 1_000_000, 1_500_000, 2_000_000]),
        0i64..=4_000_000
    ];
    (
        1i64..=10,
        (
            0i64..=1000,
            0i64..=1000,
            0i64..=3600,
            0i64..=3600,
            0i64..SHARE_SCALE,
            multiplier,
        ),
        prop::bool::weighted(0.3),
        (prop_oneof![0i64..=2000, 0i64..=100_000_000], 0i64..=3599),
        30i64..=100_000,
        (
            prop_oneof![1i64..=5000, 1i64..=100_000_000],
            prop_oneof![50i64..=1000, 50i64..=1_000_000],
        ),
        (option::of(0i64..=10_000), option::of(0i64..=14_400)),
        option::of(prop_oneof![60i64..=86_400, 0i64..=100_000]),
        proptest::collection::vec(service_case(), 0..6),
    )
        .prop_map(
            |(
                version,
                params,
                cargo,
                demand,
                vot,
                (gc_outside, theta),
                kind_weights,
                period,
                services,
            )| MarketCase {
                version,
                params,
                cargo,
                demand,
                vot,
                gc_outside,
                theta,
                kind_weights,
                period,
                services,
            },
        )
}

const MARKET: &str = "market:evaluated";
const OTHER_MARKET: &str = "market:other";

impl MarketCase {
    fn state(&self, tpf2mp: &Tpf2mp) -> Table {
        let (alpha_up, alpha_down, max_wait, transfer, crowd, multiplier) = self.params;
        let params = tpf2mp.record(&[
            ("alphaUpPm", Some(alpha_up)),
            ("alphaDownPm", Some(alpha_down)),
            ("maxWaitSeconds", Some(max_wait)),
            ("transferSeconds", Some(transfer)),
            ("crowdThresholdPpm", Some(crowd)),
            ("revenueMultiplierPpm", Some(multiplier)),
        ]);
        let markets = tpf2mp.table();
        for cid in [MARKET, OTHER_MARKET] {
            let market = tpf2mp.record(&[
                ("demand", Some(self.demand.0)),
                ("demandResid", Some(self.demand.1)),
                ("votCentsPerHour", Some(self.vot)),
                ("gcOutsideCents", Some(self.gc_outside)),
                ("thetaCents", Some(self.theta)),
                ("waitWeightPm", self.kind_weights.0),
                ("transferSeconds", self.kind_weights.1),
            ]);
            market.set("cid", cid).unwrap();
            market.set("name", format!("Market {cid}")).unwrap();
            if self.version >= 4 {
                market
                    .set("kind", if self.cargo { "cargo" } else { "passenger" })
                    .unwrap();
            }
            market.set("metadata", tpf2mp.table()).unwrap();
            markets.set(cid, market).unwrap();
        }
        let services = tpf2mp.table();
        for (index, case) in self.services.iter().enumerate() {
            let (headway, journey, transfers) = case.timing;
            let (share, share_resid, last_fare) = case.share;
            let (annual, upkeep_resid, multiplier_resid) = case.upkeep;
            let service = tpf2mp.record(&[
                ("headwaySeconds", Some(headway)),
                ("journeySeconds", Some(journey)),
                ("transfers", Some(transfers)),
                ("fareCents", Some(case.fare_cents)),
                ("capacity", Some(case.capacity.0)),
                ("capacityResid", Some(case.capacity.1)),
                ("quality", Some(case.quality)),
                ("lagLoadPpm", Some(case.lag_load_ppm)),
                ("sharePpm", share),
                ("shareResid", Some(share_resid)),
                ("lastFareCents", last_fare),
                ("annualVehicleUpkeepCents", Some(annual)),
                ("upkeepResid", Some(upkeep_resid)),
                ("revenueMultiplierResid", Some(multiplier_resid)),
            ]);
            let cid = format!("line:{index}");
            service.set("lineCid", cid.as_str()).unwrap();
            service
                .set(
                    "marketCid",
                    if case.other_market {
                        OTHER_MARKET
                    } else {
                        MARKET
                    },
                )
                .unwrap();
            service
                .set("companyCid", format!("company:{}", index % 2))
                .unwrap();
            service.set("name", format!("Service {index}")).unwrap();
            if let Some(enabled) = case.enabled {
                service.set("enabled", enabled).unwrap();
            }
            let metadata = tpf2mp.record(&[("distanceMeters", case.distance)]);
            let vehicle = if case.managed {
                "vehicle:costed"
            } else {
                "vehicle:uncosted"
            };
            metadata
                .set(
                    "vehicleCids",
                    tpf2mp.lua().create_sequence_from([vehicle]).unwrap(),
                )
                .unwrap();
            service.set("metadata", metadata).unwrap();
            services.set(cid, service).unwrap();
        }
        let vehicle_costs = tpf2mp.table();
        let costed = tpf2mp.record(&[
            ("annualVehicleUpkeepCents", Some(1_000_000)),
            ("upkeepResid", Some(0)),
        ]);
        costed.set("companyCid", "company:0").unwrap();
        vehicle_costs.set("vehicle:costed", costed).unwrap();
        let state = tpf2mp.record(&[("version", Some(self.version))]);
        state.set("params", params).unwrap();
        state.set("markets", markets).unwrap();
        state.set("services", services).unwrap();
        state.set("vehicleCosts", vehicle_costs).unwrap();
        state.set("deliveryCursors", tpf2mp.table()).unwrap();
        state
    }
}

#[test]
fn evaluate_market_matches_lua_on_random_markets() {
    let tpf2mp = Tpf2mp::new();
    runner(4096)
        .run(&market_case(), |case| {
            let call = tpf2mp.run(
                "economy_flow.evaluateMarket",
                (case.state(&tpf2mp), MARKET, Value::Nil, opt(case.period)),
            );
            prop_assert_eq!(check_evaluate_market(&call), Outcome::Matched);
            Ok(())
        })
        .unwrap();
}

#[test]
fn successive_settlements_carry_stocks_like_lua() {
    // Twenty settlements of one market, each starting from the state Lua
    // left behind: residuals and share stocks must stay identical.
    let tpf2mp = Tpf2mp::new();
    runner(256)
        .run(&market_case(), |case| {
            let state = case.state(&tpf2mp);
            for _ in 0..20 {
                let call = tpf2mp.run(
                    "economy_flow.evaluateMarket",
                    (state.clone(), MARKET, Value::Nil, opt(case.period)),
                );
                prop_assert_eq!(check_evaluate_market(&call), Outcome::Matched);
                let after = table(&call.after(1), "state");
                for part in ["markets", "services"] {
                    state.set(part, get(&after, part)).unwrap();
                }
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn an_unknown_market_has_no_result_in_either() {
    let tpf2mp = Tpf2mp::new();
    let case = MarketCase {
        version: 10,
        params: (350, 500, 1800, 480, 700_000, 1_000_000),
        cargo: false,
        demand: (100, 0),
        vot: 450,
        gc_outside: 2500,
        theta: 250,
        kind_weights: (Some(2000), Some(480)),
        period: None,
        services: Vec::new(),
    };
    let call = tpf2mp.run(
        "economy_flow.evaluateMarket",
        (case.state(&tpf2mp), "market:missing", Value::Nil, num(300)),
    );
    assert_eq!(check_evaluate_market(&call), Outcome::Matched);
}
