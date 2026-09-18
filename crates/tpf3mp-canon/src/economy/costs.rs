//! Operating costs and their proration (`economy_costs.lua`).
//!
//! The native game quotes running costs per year. TPF2MP compresses the
//! financial year to three hours of play and charges every settlement
//! interval its exact share, carrying the sub-cent remainder so that a
//! year's charges add up to the annual amount.

use std::collections::{BTreeMap, BTreeSet};

use super::ACCUMULATOR_LIMIT;
use crate::lua;

/// Hours in a calendar year: the proration basis before model version 6.
pub const HOURS_PER_YEAR: i64 = 365 * 24;
/// One financial year of play: three hours.
pub const FINANCIAL_YEAR_SECONDS: i64 = 3 * 3600;
/// Annual upkeep of a vehicle whose native running cost cannot be read, as a
/// divisor of its purchase price.
pub const VEHICLE_PURCHASE_TO_ANNUAL_DIVISOR: i64 = 6;
/// Annual infrastructure upkeep as a divisor of invested capital: 10%.
pub const INFRASTRUCTURE_CAPITAL_TO_ANNUAL_DIVISOR: i64 = 10;

fn non_negative(value: i64) -> i64 {
    value.clamp(0, ACCUMULATOR_LIMIT)
}

/// `vehicleAnnualUpkeepCents`: fallback annual upkeep from a purchase price
/// in dollars.
///
/// TPF2MP clamps the price to 10^15 dollars but then multiplies by 100, so
/// above 2^53 / 100 dollars (about 90 trillion) Lua rounds. Those prices
/// return `None`.
pub fn vehicle_annual_upkeep_cents(purchase_price_dollars: i64) -> Option<i64> {
    lua::floor_div(
        lua::mul(non_negative(purchase_price_dollars), 100)?,
        VEHICLE_PURCHASE_TO_ANNUAL_DIVISOR,
    )
}

/// `infrastructureAnnualUpkeepCents`: 10% of invested capital per year.
pub fn infrastructure_annual_upkeep_cents(capital_cents: i64) -> i64 {
    non_negative(capital_cents) / INFRASTRUCTURE_CAPITAL_TO_ANNUAL_DIVISOR
}

/// `hourlyCharge`: one hour's share of an annual amount before model
/// version 6, and the residual (in cents times hours) to carry.
pub fn hourly_charge(annual_cents: i64, residual: i64) -> Option<(i64, i64)> {
    let numerator = lua::add(non_negative(annual_cents), residual.max(0))?;
    Some((numerator / HOURS_PER_YEAR, numerator % HOURS_PER_YEAR))
}

/// `periodCharge`: a `period_seconds` interval's share of an annual amount,
/// and the residual (in cents times seconds) to carry.
///
/// TPF2MP splits the annual amount into whole cents per financial-year
/// second plus a remainder, so it never forms `annual * period` in one
/// double. The charge is capped at the accumulator limit. Returns `None`
/// when an intermediate leaves Lua's exact range, which needs a period far
/// longer than the one-day maximum interval.
pub fn period_charge(annual_cents: i64, residual: i64, period_seconds: i64) -> Option<(i64, i64)> {
    let annual = non_negative(annual_cents);
    let period = period_seconds.max(0);
    let quotient = annual / FINANCIAL_YEAR_SECONDS;
    let remainder = annual % FINANCIAL_YEAR_SECONDS;
    let tail = lua::add(lua::mul(remainder, period)?, residual.max(0))?;
    let charge = lua::add(lua::mul(quotient, period)?, tail / FINANCIAL_YEAR_SECONDS)?;
    Some((charge.min(ACCUMULATOR_LIMIT), tail % FINANCIAL_YEAR_SECONDS))
}

/// `charge`: [`hourly_charge`] before model version 6, [`period_charge`]
/// from then on.
pub fn charge(
    annual_cents: i64,
    residual: i64,
    period_seconds: i64,
    version: i64,
) -> Option<(i64, i64)> {
    if version < 6 {
        hourly_charge(annual_cents, residual)
    } else {
        period_charge(annual_cents, residual, period_seconds)
    }
}

/// `allocateCapital`: split capital across canonical output ids.
///
/// Empty and repeated ids are dropped, the rest sorted; each gets an equal
/// share and the first `total % count` get one cent more, so the sum is
/// exact and independent of machine-local ids. Returns `None` only if the id
/// count does not fit an `i64`.
pub fn allocate_capital(cids: &[&str], total_cents: i64) -> Option<BTreeMap<String, i64>> {
    let ordered: BTreeSet<&str> = cids.iter().copied().filter(|cid| !cid.is_empty()).collect();
    let mut result = BTreeMap::new();
    if ordered.is_empty() {
        return Some(result);
    }
    let total = non_negative(total_cents);
    let count = i64::try_from(ordered.len()).ok()?;
    let (base, remainder) = (total / count, total % count);
    for (index, cid) in (0..).zip(ordered) {
        result.insert(cid.to_owned(), base + i64::from(index < remainder));
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_year_of_period_charges_adds_up_to_the_annual_amount() {
        let annual = 1_234_567;
        let (mut total, mut residual) = (0, 0);
        for _ in 0..(FINANCIAL_YEAR_SECONDS / 300) {
            let (charge, carried) = period_charge(annual, residual, 300).unwrap();
            total += charge;
            residual = carried;
        }
        assert_eq!((total, residual), (annual, 0));
    }

    #[test]
    fn hourly_charges_add_up_to_the_annual_amount() {
        let annual = 8_760_001;
        let (mut total, mut residual) = (0, 0);
        for _ in 0..HOURS_PER_YEAR {
            let (charge, carried) = hourly_charge(annual, residual).unwrap();
            total += charge;
            residual = carried;
        }
        // 8759 hours of 1000 cents, then 1001 once the residual reaches 8760.
        assert_eq!((total, residual), (annual, 0));
    }

    #[test]
    fn vehicle_upkeep_refuses_prices_that_lua_rounds() {
        assert_eq!(vehicle_annual_upkeep_cents(600), Some(10_000));
        assert_eq!(vehicle_annual_upkeep_cents(-1), Some(0));
        assert!(vehicle_annual_upkeep_cents(lua::MAX_EXACT_INTEGER / 100).is_some());
        assert_eq!(
            vehicle_annual_upkeep_cents(lua::MAX_EXACT_INTEGER / 100 + 1),
            None
        );
    }

    #[test]
    fn capital_splits_exactly_in_id_order() {
        let result = allocate_capital(&["b", "a", "", "b", "c"], 10).unwrap();
        assert_eq!(
            result,
            BTreeMap::from([("a".into(), 4), ("b".into(), 3), ("c".into(), 3)])
        );
        assert!(allocate_capital(&[], 10).unwrap().is_empty());
        assert!(allocate_capital(&[""], 10).unwrap().is_empty());
    }
}
