//! Fares and delivery revenue (`economy_revenue.lua`).

use super::{ACCUMULATOR_LIMIT, MarketKind};
use crate::lua;

/// Revenue of one passenger trip is the fare times this cohort size.
pub const PASSENGER_COHORT_SCALE: i64 = 1000;
/// Cargo revenue per unit and kilometre at the reference fare.
pub const CARGO_CENTS_PER_UNIT_KM: i64 = 100_000;
/// Cargo fare at which [`CARGO_CENTS_PER_UNIT_KM`] applies unscaled.
pub const CARGO_REFERENCE_FARE_CENTS: i64 = 1000;
pub const DEFAULT_PASSENGER_BASE_FARE_CENTS: i64 = 500;
pub const DEFAULT_PASSENGER_FARE_CENTS_PER_KM: i64 = 150;
/// Cargo distance assumed when a service records none.
pub const DEFAULT_CARGO_DISTANCE_METERS: i64 = 1000;

fn bounded(value: i64) -> i64 {
    value.clamp(0, ACCUMULATOR_LIMIT)
}

/// `saturatingMultiply` of `economy_revenue.lua`: clamps both operands to
/// `[0, ACCUMULATOR_LIMIT]` and caps the product at the limit. Total: no
/// intermediate exceeds the limit, which is below 2^53.
pub fn saturating_multiply(left: i64, right: i64) -> i64 {
    let (left, right) = (bounded(left), bounded(right));
    if left == 0 || right == 0 {
        return 0;
    }
    if left > ACCUMULATOR_LIMIT / right {
        return ACCUMULATOR_LIMIT;
    }
    left * right
}

/// `defaultFareCents`: the fare a new line starts with. Cargo pays the
/// reference fare; passengers pay a base fare plus 150 cents per kilometre,
/// rounded to the nearest cent. A missing distance counts as zero.
///
/// Returns `None` for distances above about 6 * 10^13 m, where Lua's
/// `distance * 150 + 500` leaves the exact range.
pub fn default_fare_cents(distance_meters: Option<i64>, kind: MarketKind) -> Option<i64> {
    if kind == MarketKind::Cargo {
        return Some(CARGO_REFERENCE_FARE_CENTS);
    }
    let distance = distance_meters.unwrap_or(0).max(0);
    let per_km = lua::floor_div(
        lua::add(
            lua::mul(distance, DEFAULT_PASSENGER_FARE_CENTS_PER_KM)?,
            500,
        )?,
        1000,
    )?;
    lua::add(DEFAULT_PASSENGER_BASE_FARE_CENTS, per_km)
}

/// `passengerDeliveryCents`: fare times passengers times the cohort scale,
/// saturating.
pub fn passenger_delivery_cents(passengers: i64, fare_cents: i64) -> i64 {
    saturating_multiply(
        saturating_multiply(passengers, fare_cents),
        PASSENGER_COHORT_SCALE,
    )
}

/// `modelDeliveryCents`: revenue of `delivered` units that the model (not a
/// delivery ledger) carried.
///
/// Passengers pay [`passenger_delivery_cents`]. Cargo pays per unit and whole
/// kilometre (at least one), scaled by the fare against the reference fare;
/// `distance_meters` defaults to one kilometre. Returns `None` for distances
/// beyond Lua's exact range.
pub fn model_delivery_cents(
    kind: MarketKind,
    distance_meters: Option<i64>,
    fare_cents: i64,
    delivered: i64,
) -> Option<i64> {
    match kind {
        MarketKind::Passenger => Some(passenger_delivery_cents(delivered, fare_cents)),
        MarketKind::Cargo => {
            let distance = distance_meters
                .unwrap_or(DEFAULT_CARGO_DISTANCE_METERS)
                .max(0);
            let km = lua::floor_div(distance, 1000)?.max(1);
            let base =
                saturating_multiply(saturating_multiply(delivered, km), CARGO_CENTS_PER_UNIT_KM);
            // Both operands are non-negative, so Rust's division floors.
            Some(saturating_multiply(base, fare_cents) / CARGO_REFERENCE_FARE_CENTS)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturating_multiply_clamps_operands_and_product() {
        assert_eq!(saturating_multiply(3, 4), 12);
        assert_eq!(saturating_multiply(-3, 4), 0);
        assert_eq!(saturating_multiply(i64::MAX, 1), ACCUMULATOR_LIMIT);
        assert_eq!(
            saturating_multiply(1_000_000_000, 1_000_001),
            ACCUMULATOR_LIMIT
        );
        assert_eq!(
            saturating_multiply(1_000_000_000, 1_000_000),
            ACCUMULATOR_LIMIT
        );
    }

    #[test]
    fn default_fares() {
        assert_eq!(
            default_fare_cents(Some(12_345), MarketKind::Cargo),
            Some(1000)
        );
        assert_eq!(default_fare_cents(None, MarketKind::Passenger), Some(500));
        // 3 km: 3000 * 150 = 450000, + 500, / 1000 = 450.
        assert_eq!(
            default_fare_cents(Some(3000), MarketKind::Passenger),
            Some(950)
        );
        // 3 m is 0.45 cents and rounds down; 4 m is 0.6 cents and rounds up.
        assert_eq!(
            default_fare_cents(Some(3), MarketKind::Passenger),
            Some(500)
        );
        assert_eq!(
            default_fare_cents(Some(4), MarketKind::Passenger),
            Some(501)
        );
        assert_eq!(
            default_fare_cents(Some(-50), MarketKind::Passenger),
            Some(500)
        );
    }

    #[test]
    fn cargo_revenue_scales_with_distance_and_fare() {
        // 10 units, 2 km, fare 1500: 10 * 2 * 100000 * 1500 / 1000.
        assert_eq!(
            model_delivery_cents(MarketKind::Cargo, Some(2999), 1500, 10),
            Some(3_000_000)
        );
        assert_eq!(
            model_delivery_cents(MarketKind::Cargo, Some(10), 1000, 1),
            Some(100_000)
        );
        assert_eq!(
            model_delivery_cents(MarketKind::Cargo, None, 1000, 1),
            Some(100_000)
        );
        assert_eq!(
            model_delivery_cents(MarketKind::Passenger, None, 950, 7),
            Some(6_650_000)
        );
    }
}
