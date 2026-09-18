//! `economy_revenue.lua`: fares and delivery revenue.

use mlua::Value;
use proptest::option;
use proptest::prelude::*;
use tpf3mp_canon::economy::revenue;
use tpf3mp_canon::economy::{ACCUMULATOR_LIMIT, MarketKind};
use tpf3mp_canon::lua::MAX_EXACT_INTEGER;

use crate::tpf2mp::{
    Call, Check, Outcome, Tpf2mp, get, int, num, opt, opt_int, opt_table, runner, same,
    wide_amount, wide_num,
};

pub const CHECKS: &[(&str, Check)] = &[
    (
        "economy_revenue.saturatingMultiply",
        check_saturating_multiply,
    ),
    ("economy_revenue.defaultFareCents", check_default_fare_cents),
    (
        "economy_revenue.passengerDeliveryCents",
        check_passenger_delivery_cents,
    ),
    (
        "economy_revenue.modelDeliveryCents",
        check_model_delivery_cents,
    ),
];

/// `util.integer(x, 0)` reads nil as zero.
fn integer_or_zero(value: &Value, what: &str) -> i64 {
    opt_int(value, what).unwrap_or(0)
}

/// Lua's kind test is `kind == "cargo"`; any other value means passengers.
fn kind_of(value: &Value) -> MarketKind {
    match value {
        Value::String(kind) if kind.as_bytes() == b"cargo".as_slice() => MarketKind::Cargo,
        _ => MarketKind::Passenger,
    }
}

pub fn check_saturating_multiply(call: &Call) -> Outcome {
    let left = integer_or_zero(&call.arg(1), "left");
    let right = integer_or_zero(&call.arg(2), "right");
    let context = format!("revenue.saturatingMultiply({left}, {right})");
    same(
        &context,
        "product",
        &call.result(1),
        revenue::saturating_multiply(left, right),
    );
    Outcome::Matched
}

pub fn check_default_fare_cents(call: &Call) -> Outcome {
    let distance = opt_int(&call.arg(1), "distanceMeters");
    let kind = kind_of(&call.arg(2));
    let context = format!("defaultFareCents({distance:?}, {kind:?})");
    match revenue::default_fare_cents(distance, kind) {
        Some(fare) => {
            same(&context, "fare", &call.result(1), fare);
            Outcome::Matched
        }
        None => Outcome::Refused,
    }
}

pub fn check_passenger_delivery_cents(call: &Call) -> Outcome {
    let passengers = integer_or_zero(&call.arg(1), "passengers");
    let fare = integer_or_zero(&call.arg(2), "fareCents");
    let context = format!("passengerDeliveryCents({passengers}, {fare})");
    same(
        &context,
        "revenue",
        &call.result(1),
        revenue::passenger_delivery_cents(passengers, fare),
    );
    Outcome::Matched
}

pub fn check_model_delivery_cents(call: &Call) -> Outcome {
    let market = opt_table(&call.arg(1), "market");
    let kind = market.as_ref().map_or(MarketKind::Passenger, |market| {
        kind_of(&get(market, "kind"))
    });
    let service = opt_table(&call.arg(2), "service").expect("TPF2MP always passes a service");
    let fare = int(&get(&service, "fareCents"), "fareCents");
    let distance = opt_table(&get(&service, "metadata"), "metadata")
        .and_then(|metadata| opt_int(&get(&metadata, "distanceMeters"), "distanceMeters"));
    let delivered = integer_or_zero(&call.arg(3), "delivered");
    let context = format!("modelDeliveryCents({kind:?}, {distance:?}, {fare}, {delivered})");
    match revenue::model_delivery_cents(kind, distance, fare, delivered) {
        Some(cents) => {
            same(&context, "revenue", &call.result(1), cents);
            Outcome::Matched
        }
        None => Outcome::Refused,
    }
}

fn kind_name(kind: MarketKind) -> &'static str {
    match kind {
        MarketKind::Passenger => "passenger",
        MarketKind::Cargo => "cargo",
    }
}

fn run_model_delivery(
    tpf2mp: &Tpf2mp,
    kind: MarketKind,
    distance: Option<i64>,
    fare: i64,
    delivered: i64,
) -> Call {
    let market = tpf2mp.table();
    market.set("kind", kind_name(kind)).unwrap();
    let service = tpf2mp.record(&[("fareCents", Some(fare))]);
    service
        .set("metadata", tpf2mp.record(&[("distanceMeters", distance)]))
        .unwrap();
    tpf2mp.run(
        "economy_revenue.modelDeliveryCents",
        (market, service, num(delivered)),
    )
}

fn kind() -> impl Strategy<Value = MarketKind> {
    prop_oneof![Just(MarketKind::Passenger), Just(MarketKind::Cargo)]
}

/// Any Lua-exact integer, weighted toward the interesting magnitudes.
fn amount() -> impl Strategy<Value = i64> {
    prop_oneof![
        -10i64..=10,
        0i64..=1_000_000_000,
        0i64..=ACCUMULATOR_LIMIT + 10,
        -MAX_EXACT_INTEGER..=MAX_EXACT_INTEGER,
    ]
}

#[test]
fn saturating_multiply_matches_lua() {
    let tpf2mp = Tpf2mp::new();
    runner(8192)
        .run(&(amount(), amount()), |(left, right)| {
            let call = tpf2mp.run(
                "economy_revenue.saturatingMultiply",
                (num(left), num(right)),
            );
            prop_assert_eq!(check_saturating_multiply(&call), Outcome::Matched);
            Ok(())
        })
        .unwrap();
    for (left, right) in [
        (1_000_000_000, 1_000_000),
        (1_000_000_000, 1_000_001),
        (0, -1),
    ] {
        let call = tpf2mp.run(
            "economy_revenue.saturatingMultiply",
            (num(left), num(right)),
        );
        assert_eq!(check_saturating_multiply(&call), Outcome::Matched);
    }
}

#[test]
fn default_fare_matches_lua() {
    let tpf2mp = Tpf2mp::new();
    // Up to the largest distance whose `distance * 150 + 500` stays within
    // 2^53 - 1.
    let longest = 60_047_995_031_603;
    let distance = option::of(prop_oneof![-1000i64..=5_000_000, 0i64..=longest]);
    runner(8192)
        .run(&(distance, kind()), |(distance, kind)| {
            let call = tpf2mp.run(
                "economy_revenue.defaultFareCents",
                (opt(distance), kind_name(kind)),
            );
            prop_assert_eq!(check_default_fare_cents(&call), Outcome::Matched);
            Ok(())
        })
        .unwrap();
    let call = tpf2mp.run(
        "economy_revenue.defaultFareCents",
        (num(longest), "passenger"),
    );
    assert_eq!(check_default_fare_cents(&call), Outcome::Matched);
    let call = tpf2mp.run(
        "economy_revenue.defaultFareCents",
        (num(longest + 1), "passenger"),
    );
    assert_eq!(check_default_fare_cents(&call), Outcome::Refused);
}

#[test]
fn passenger_delivery_matches_lua() {
    let tpf2mp = Tpf2mp::new();
    runner(4096)
        .run(&(amount(), amount()), |(passengers, fare)| {
            let call = tpf2mp.run(
                "economy_revenue.passengerDeliveryCents",
                (num(passengers), num(fare)),
            );
            prop_assert_eq!(check_passenger_delivery_cents(&call), Outcome::Matched);
            Ok(())
        })
        .unwrap();
}

#[test]
fn model_delivery_matches_lua() {
    let tpf2mp = Tpf2mp::new();
    let distance = option::of(prop_oneof![
        -5000i64..=5_000_000,
        -MAX_EXACT_INTEGER..=MAX_EXACT_INTEGER
    ]);
    runner(8192)
        .run(
            &(kind(), distance, amount(), amount()),
            |(kind, distance, fare, delivered)| {
                let call = run_model_delivery(&tpf2mp, kind, distance, fare, delivered);
                prop_assert_eq!(check_model_delivery_cents(&call), Outcome::Matched);
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn revenue_clamps_like_lua_for_any_whole_double() {
    // Every operand is clamped to [0, 10^15] before any product forms.
    let tpf2mp = Tpf2mp::new();
    let distance = option::of(-5000i64..=5_000_000);
    runner(4096)
        .run(
            &(wide_amount(), wide_amount(), kind(), distance),
            |(left, right, kind, distance)| {
                let call = tpf2mp.run(
                    "economy_revenue.saturatingMultiply",
                    (wide_num(left), wide_num(right)),
                );
                prop_assert_eq!(check_saturating_multiply(&call), Outcome::Matched);
                let call = tpf2mp.run(
                    "economy_revenue.passengerDeliveryCents",
                    (wide_num(left), wide_num(right)),
                );
                prop_assert_eq!(check_passenger_delivery_cents(&call), Outcome::Matched);
                let market = tpf2mp.table();
                market.set("kind", kind_name(kind)).unwrap();
                let service = tpf2mp.table();
                service.set("fareCents", wide_num(left)).unwrap();
                service
                    .set("metadata", tpf2mp.record(&[("distanceMeters", distance)]))
                    .unwrap();
                let call = tpf2mp.run(
                    "economy_revenue.modelDeliveryCents",
                    (market, service, wide_num(right)),
                );
                prop_assert_eq!(check_model_delivery_cents(&call), Outcome::Matched);
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn model_delivery_defaults_follow_lua() {
    let tpf2mp = Tpf2mp::new();
    // No service metadata at all: cargo assumes one kilometre.
    let market = tpf2mp.table();
    market.set("kind", "cargo").unwrap();
    let service = tpf2mp.record(&[("fareCents", Some(1500))]);
    let call = tpf2mp.run("economy_revenue.modelDeliveryCents", (market, service, 7.0));
    assert_eq!(check_model_delivery_cents(&call), Outcome::Matched);
    // No market: passengers.
    let service = tpf2mp.record(&[("fareCents", Some(1500))]);
    let call = tpf2mp.run(
        "economy_revenue.modelDeliveryCents",
        (Value::Nil, service, 7.0),
    );
    assert_eq!(check_model_delivery_cents(&call), Outcome::Matched);
    // An unknown kind is passengers too.
    let market = tpf2mp.table();
    market.set("kind", "mail").unwrap();
    let service = tpf2mp.record(&[("fareCents", Some(1500))]);
    let call = tpf2mp.run("economy_revenue.modelDeliveryCents", (market, service, 7.0));
    assert_eq!(check_model_delivery_cents(&call), Outcome::Matched);
}
