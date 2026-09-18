//! `economy_flow.lua`: generalized cost, logit weights, rates and glides.
//! The share loop and the rest of `evaluateMarket` are checked in `market`.

use mlua::{Table, Value};
use proptest::option;
use proptest::prelude::*;
use tpf3mp_canon::economy::SHARE_SCALE;
use tpf3mp_canon::economy::flow::{self, CostParams, GeneralizedCost, MarketCost, ServiceCost};
use tpf3mp_canon::economy::settlement;

use crate::tpf2mp::{
    Call, Check, Outcome, Tpf2mp, exact_int, field, get, int, num, opt, opt_field, opt_int, runner,
    same, table,
};

pub const CHECKS: &[(&str, Check)] = &[
    ("economy_flow.generalizedCost", check_generalized_cost),
    ("economy_flow.logitWeight", check_logit_weight),
    ("economy_flow.scaledRate", check_scaled_rate),
    ("economy_flow.glide", check_glide),
    ("economy_flow.signedAdd", check_signed_add),
];

pub fn check_generalized_cost(call: &Call) -> Outcome {
    let params = table(&call.arg(1), "params");
    let market = table(&call.arg(2), "market");
    let service = table(&call.arg(3), "service");
    let params = CostParams {
        max_wait_seconds: field(&params, "maxWaitSeconds"),
        transfer_seconds: field(&params, "transferSeconds"),
        crowd_threshold_ppm: field(&params, "crowdThresholdPpm"),
    };
    let market = MarketCost {
        vot_cents_per_hour: field(&market, "votCentsPerHour"),
        wait_weight_pm: opt_field(&market, "waitWeightPm"),
        transfer_seconds: opt_field(&market, "transferSeconds"),
    };
    let service = ServiceCost {
        headway_seconds: field(&service, "headwaySeconds"),
        journey_seconds: field(&service, "journeySeconds"),
        transfers: field(&service, "transfers"),
        // `service.lagLoadPpm or 0`
        lag_load_ppm: opt_field(&service, "lagLoadPpm").unwrap_or(0),
        quality: field(&service, "quality"),
        fare_cents: field(&service, "fareCents"),
    };
    let access = opt_int(&call.arg(4), "feederAccessCents");
    let context = format!("generalizedCost({params:?}, {market:?}, {service:?}, {access:?})");
    let Some(cost) = flow::generalized_cost(&params, &market, &service, access) else {
        return Outcome::Refused;
    };
    same(&context, "gcCents", &call.result(1), cost.gc_cents);
    same_factors(&context, &table(&call.result(2), "factors"), &cost, 0);
    Outcome::Matched
}

/// Checks a factors table against a generalized cost. `extra_keys` counts
/// fields the caller adds (such as `feederAccessEndpoints`) and checks.
pub fn same_factors(context: &str, factors: &Table, cost: &GeneralizedCost, extra_keys: usize) {
    let expected = [
        ("fareCents", cost.fare_cents),
        ("timeCostCents", cost.time_cost_cents),
        ("waitCostCents", cost.wait_cost_cents),
        ("transferCostCents", cost.transfer_cost_cents),
        ("crowdCostCents", cost.crowd_cost_cents),
        ("comfortCents", cost.comfort_cents),
        ("gcCents", cost.gc_cents),
    ];
    for (name, value) in expected {
        same(context, name, &get(factors, name), value);
    }
    let mut keys = expected.len() + extra_keys;
    if let Some(access) = cost.feeder_access {
        same(
            context,
            "baseComfortCents",
            &get(factors, "baseComfortCents"),
            access.base_comfort_cents,
        );
        same(
            context,
            "feederAccessCents",
            &get(factors, "feederAccessCents"),
            access.feeder_access_cents,
        );
        keys += 2;
    }
    assert_eq!(
        factors.pairs::<Value, Value>().count(),
        keys,
        "{context}: factor set differs"
    );
}

pub fn check_logit_weight(call: &Call) -> Outcome {
    let [gc, gc_min, theta, cutoff] =
        [1, 2, 3, 4].map(|index| int(&call.arg(index), "logitWeight"));
    let context = format!("logitWeight({gc}, {gc_min}, {theta}, {cutoff})");
    match flow::logit_weight(gc, gc_min, theta, cutoff) {
        Some(weight) => {
            same(&context, "weight", &call.result(1), weight);
            Outcome::Matched
        }
        None => Outcome::Refused,
    }
}

pub fn check_scaled_rate(call: &Call) -> Outcome {
    // `util.integer(x, 0)` reads a missing hourly amount or residual as 0.
    let hourly = opt_int(&call.arg(1), "hourly").unwrap_or(0);
    let residual = opt_int(&call.arg(2), "residual").unwrap_or(0);
    let period = int(&call.arg(3), "periodSeconds");
    let context = format!("scaledRate({hourly}, {residual}, {period})");
    match flow::scaled_rate(hourly, residual, period) {
        Some((amount, carried)) => {
            same(&context, "amount", &call.result(1), amount);
            same(&context, "residual", &call.result(2), carried);
            Outcome::Matched
        }
        None => Outcome::Refused,
    }
}

pub fn check_glide(call: &Call) -> Outcome {
    let [actual, equilibrium, alpha, residual] =
        [1, 2, 3, 4].map(|index| int(&call.arg(index), "glide"));
    let context = format!("glide({actual}, {equilibrium}, {alpha}, {residual})");
    match flow::glide(actual, equilibrium, alpha, residual) {
        Some((value, carried)) => {
            same(&context, "value", &call.result(1), value);
            same(&context, "residual", &call.result(2), carried);
            Outcome::Matched
        }
        None => Outcome::Refused,
    }
}

pub fn check_signed_add(call: &Call) -> Outcome {
    // `tonumber(x) or 0`
    let left = opt_int(&call.arg(1), "left").unwrap_or(0);
    let right = opt_int(&call.arg(2), "right").unwrap_or(0);
    let context = format!("signedAdd({left}, {right})");
    same(
        &context,
        "sum",
        &call.result(1),
        settlement::signed_add(left, right),
    );
    Outcome::Matched
}

fn run_generalized_cost(
    tpf2mp: &Tpf2mp,
    params: &CostParams,
    market: &MarketCost,
    service: &ServiceCost,
    access: Option<i64>,
) -> Call {
    let params = tpf2mp.record(&[
        ("maxWaitSeconds", Some(params.max_wait_seconds)),
        ("transferSeconds", Some(params.transfer_seconds)),
        ("crowdThresholdPpm", Some(params.crowd_threshold_ppm)),
    ]);
    let market = tpf2mp.record(&[
        ("votCentsPerHour", Some(market.vot_cents_per_hour)),
        ("waitWeightPm", market.wait_weight_pm),
        ("transferSeconds", market.transfer_seconds),
    ]);
    let service = tpf2mp.record(&[
        ("headwaySeconds", Some(service.headway_seconds)),
        ("journeySeconds", Some(service.journey_seconds)),
        ("transfers", Some(service.transfers)),
        ("lagLoadPpm", Some(service.lag_load_ppm)),
        ("quality", Some(service.quality)),
        ("fareCents", Some(service.fare_cents)),
    ]);
    tpf2mp.run(
        "economy_flow.generalizedCost",
        (params, market, service, opt(access)),
    )
}

prop_compose! {
    /// Parameters, market and service within TPF2MP's upsert clamps. Loads
    /// above the scale occur: a load is chosen riders per unit of capacity.
    fn designed_cost()(
        params in (0i64..=86_400, 0i64..=14_400, 0i64..SHARE_SCALE),
        market in (30i64..=100_000, option::of(0i64..=10_000), option::of(0i64..=14_400)),
        timing in (30i64..=86_400, 30i64..=604_800, 0i64..=8),
        lag_load_ppm in prop_oneof![0i64..=2 * SHARE_SCALE, 0i64..=1_000_000_000_000_000],
        quality in 0i64..=1000,
        fare_cents in 0i64..=100_000_000,
        access in option::of(0i64..=300),
    ) -> (CostParams, MarketCost, ServiceCost, Option<i64>) {
        (
            CostParams { max_wait_seconds: params.0, transfer_seconds: params.1, crowd_threshold_ppm: params.2 },
            MarketCost { vot_cents_per_hour: market.0, wait_weight_pm: market.1, transfer_seconds: market.2 },
            ServiceCost {
                headway_seconds: timing.0, journey_seconds: timing.1, transfers: timing.2,
                lag_load_ppm, quality, fare_cents,
            },
            access,
        )
    }
}

prop_compose! {
    /// Negative and out-of-range values too, still inside Lua's exact range.
    fn wide_cost()(
        params in (-100_000i64..=100_000, -20_000i64..=20_000, -2 * SHARE_SCALE..=2 * SHARE_SCALE),
        market in (-100_000i64..=100_000, option::of(-20_000i64..=20_000), option::of(-20_000i64..=20_000)),
        timing in (-100_000i64..=100_000, -1_000_000i64..=1_000_000, -20i64..=20),
        lag_load_ppm in -2 * SHARE_SCALE..=4 * SHARE_SCALE,
        quality in -2000i64..=2000,
        fare_cents in -200_000_000i64..=200_000_000,
        access in option::of(-500i64..=500),
    ) -> (CostParams, MarketCost, ServiceCost, Option<i64>) {
        (
            CostParams { max_wait_seconds: params.0, transfer_seconds: params.1, crowd_threshold_ppm: params.2 },
            MarketCost { vot_cents_per_hour: market.0, wait_weight_pm: market.1, transfer_seconds: market.2 },
            ServiceCost {
                headway_seconds: timing.0, journey_seconds: timing.1, transfers: timing.2,
                lag_load_ppm, quality, fare_cents,
            },
            access,
        )
    }
}

#[test]
fn generalized_cost_matches_lua() {
    let tpf2mp = Tpf2mp::new();
    runner(4096)
        .run(&designed_cost(), |(params, market, service, access)| {
            let call = run_generalized_cost(&tpf2mp, &params, &market, &service, access);
            prop_assert_eq!(check_generalized_cost(&call), Outcome::Matched);
            Ok(())
        })
        .unwrap();
    // Outside the designed ranges every intermediate stays exact, so the port
    // must answer unless the crowd span is zero.
    runner(4096)
        .run(&wide_cost(), |(params, market, service, access)| {
            let call = run_generalized_cost(&tpf2mp, &params, &market, &service, access);
            let expected = if params.crowd_threshold_ppm == SHARE_SCALE {
                Outcome::Refused
            } else {
                Outcome::Matched
            };
            prop_assert_eq!(check_generalized_cost(&call), expected);
            Ok(())
        })
        .unwrap();
}

#[test]
fn a_crowd_threshold_of_the_whole_scale_is_refused() {
    // Lua divides 0 by 0 for the crowd cost, reports it as NaN and, because
    // math.max(1, NaN) is 1, prices the service at one cent.
    let tpf2mp = Tpf2mp::new();
    let params = CostParams {
        max_wait_seconds: 1800,
        transfer_seconds: 480,
        crowd_threshold_ppm: SHARE_SCALE,
    };
    let market = MarketCost {
        vot_cents_per_hour: 450,
        wait_weight_pm: Some(2000),
        transfer_seconds: Some(480),
    };
    let service = ServiceCost {
        headway_seconds: 900,
        journey_seconds: 2400,
        transfers: 0,
        lag_load_ppm: 900_000,
        quality: 100,
        fare_cents: 1000,
    };
    let call = run_generalized_cost(&tpf2mp, &params, &market, &service, None);
    assert_eq!(exact_int(&call.result(1)), Some(1));
    let crowd = get(&table(&call.result(2), "factors"), "crowdCostCents");
    assert!(matches!(crowd, Value::Number(value) if value.is_nan()));
    assert_eq!(check_generalized_cost(&call), Outcome::Refused);
}

#[test]
fn logit_weight_matches_lua() {
    let tpf2mp = Tpf2mp::new();
    // Uniform costs, and costs placed near every interpolation step and the
    // cutoff, where flooring decides the weight.
    let uniform = (
        1i64..=300_000_000,
        1i64..=300_000_000,
        50i64..=1_000_000,
        0i64..=1,
    );
    let stepped = (
        1i64..=100_000_000,
        0i64..=900,
        -3i64..=3,
        50i64..=1_000_000,
        0i64..=1,
    )
        .prop_map(|(gc_min, centinats, nudge, theta, cutoff)| {
            (
                gc_min + centinats * theta / 100 + nudge,
                gc_min,
                theta,
                cutoff,
            )
        });
    runner(8192)
        .run(
            &prop_oneof![uniform, stepped],
            |(gc, gc_min, theta, cutoff)| {
                let call = tpf2mp.run(
                    "economy_flow.logitWeight",
                    (num(gc), num(gc_min), num(theta), num(cutoff)),
                );
                prop_assert_eq!(check_logit_weight(&call), Outcome::Matched);
                Ok(())
            },
        )
        .unwrap();
    let wide = (
        -1_000_000_000_000i64..=1_000_000_000_000,
        -1_000_000_000_000i64..=1_000_000_000_000,
        prop_oneof![-10_000_000i64..=-1, 1i64..=10_000_000],
        -5i64..=5,
    );
    runner(4096)
        .run(&wide, |(gc, gc_min, theta, cutoff)| {
            let call = tpf2mp.run(
                "economy_flow.logitWeight",
                (num(gc), num(gc_min), num(theta), num(cutoff)),
            );
            prop_assert_eq!(check_logit_weight(&call), Outcome::Matched);
            Ok(())
        })
        .unwrap();
}

#[test]
fn the_exported_logit_weight_has_a_zero_cutoff() {
    let tpf2mp = Tpf2mp::new();
    let exported = tpf2mp.module_field("economy_flow", "logitWeight");
    let exported = exported.as_function().unwrap();
    runner(2048)
        .run(
            &(1i64..=3_000_000, 1i64..=3_000_000, 50i64..=100_000),
            |(gc, gc_min, theta)| {
                let weight: Value = exported.call((num(gc), num(gc_min), num(theta))).unwrap();
                prop_assert_eq!(exact_int(&weight), flow::logit_weight(gc, gc_min, theta, 0));
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn a_zero_theta_is_refused() {
    let tpf2mp = Tpf2mp::new();
    let logit = tpf2mp.function("economy_flow.logitWeight");
    // Lua divides by zero: a positive gap is +inf and gets the cutoff weight,
    // a zero gap is NaN and indexes the table with NaN, which raises.
    let above: Value = logit.call((1100.0, 1000.0, 0.0, 0.0)).unwrap();
    assert_eq!(exact_int(&above), Some(0));
    assert!(logit.call::<Value>((1000.0, 1000.0, 0.0, 0.0)).is_err());
    assert_eq!(flow::logit_weight(1100, 1000, 0, 0), None);
    assert_eq!(flow::logit_weight(1000, 1000, 0, 0), None);
}

#[test]
fn exp_table_is_tpf2mps_and_its_definition() {
    let tpf2mp = Tpf2mp::new();
    let lua_table = table(&tpf2mp.export("economy_flow.EXP_TABLE"), "EXP_TABLE");
    let lua_values: Vec<i64> = lua_table
        .sequence_values::<Value>()
        .map(|value| int(&value.unwrap(), "EXP_TABLE"))
        .collect();
    assert_eq!(lua_values, flow::EXP_TABLE);
    // round(65536 * exp(-k / 10)), as TPF2MP's Python check derives it with
    // 80-digit decimals. f64 suffices here because no entry lies within
    // 1e-6 of a rounding boundary, far beyond exp's error.
    for (k, value) in flow::EXP_TABLE.iter().enumerate() {
        let exact = 65536.0 * (-(k as f64) / 10.0).exp();
        let distance = (exact - exact.floor() - 0.5).abs();
        assert!(
            distance > 1e-6,
            "entry {k} ({exact}) is too close to a boundary"
        );
        assert_eq!(*value, exact.round() as i64, "entry {k}");
    }
}

#[test]
fn scaled_rate_matches_lua() {
    let tpf2mp = Tpf2mp::new();
    let designed = (0i64..=1_000_000_000, 0i64..=3599, 60i64..=86_400);
    let wide = (
        -10_000_000_000i64..=10_000_000_000,
        -1_000_000i64..=1_000_000,
        -100_000i64..=100_000,
    );
    runner(8192)
        .run(
            &prop_oneof![designed, wide],
            |(hourly, residual, period)| {
                let call = tpf2mp.run(
                    "economy_flow.scaledRate",
                    (num(hourly), num(residual), num(period)),
                );
                prop_assert_eq!(check_scaled_rate(&call), Outcome::Matched);
                Ok(())
            },
        )
        .unwrap();
    // Missing amounts count as zero in both.
    let call = tpf2mp.run("economy_flow.scaledRate", (Value::Nil, Value::Nil, 300.0));
    assert_eq!(check_scaled_rate(&call), Outcome::Matched);
}

#[test]
fn glide_matches_lua() {
    let tpf2mp = Tpf2mp::new();
    let designed = (
        0i64..=SHARE_SCALE,
        0i64..=SHARE_SCALE,
        0i64..=1000,
        0i64..=999,
    );
    let wide = (
        -1_000_000_000i64..=1_000_000_000,
        -1_000_000_000i64..=1_000_000_000,
        -1_000_000i64..=1_000_000,
        -1_000_000_000i64..=1_000_000_000,
    );
    runner(8192)
        .run(
            &prop_oneof![designed, wide],
            |(actual, equilibrium, alpha, residual)| {
                let call = tpf2mp.run(
                    "economy_flow.glide",
                    (num(actual), num(equilibrium), num(alpha), num(residual)),
                );
                prop_assert_eq!(check_glide(&call), Outcome::Matched);
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn signed_add_matches_lua() {
    let tpf2mp = Tpf2mp::new();
    let limit = 4_000_000_000_000_000i64;
    runner(4096)
        .run(&(-limit..=limit, -limit..=limit), |(left, right)| {
            let call = tpf2mp.run("economy_flow.signedAdd", (num(left), num(right)));
            prop_assert_eq!(check_signed_add(&call), Outcome::Matched);
            Ok(())
        })
        .unwrap();
}
