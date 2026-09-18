//! Model towns, gravity demand and town growth
//! (`economy_town_demand.lua`).
//!
//! A town's model size counts four capacity units per native building. The
//! demand of a corridor between two towns follows a gravity model, and towns
//! grow from the passengers the network carries. Native observations may
//! raise a town's size but never shrink it, and refreshed demand never
//! destroys demand that players already invested against.

use std::collections::BTreeMap;

use crate::lua;

/// Schema of the town records and growth reports.
pub const SCHEMA_VERSION: i64 = 1;
pub const NOMINAL_CAPACITY_PER_BUILDING: i64 = 4;
/// Buildings assumed when a town's count cannot be observed.
pub const FALLBACK_TOWN_BUILDINGS: i64 = 50;
/// Size of a town without an observation: 50 buildings of 4 units.
pub const FALLBACK_TOWN_SIZE: i64 = FALLBACK_TOWN_BUILDINGS * NOMINAL_CAPACITY_PER_BUILDING;
pub const GRAVITY_DIVISOR: i64 = 25;
/// Bounds of a corridor's gravity demand per hour.
pub const MIN_DEMAND: i64 = 50;
pub const MAX_DEMAND: i64 = 100_000;
pub const MAX_TOWN_SIZE: i64 = 100_000;
/// Carried passengers per new building: the physical growth policy spends
/// 400 points per building, and a building is four capacity units.
pub const GROWTH_PASSENGERS_PER_BUILDING: i64 = 400;
/// Largest demand a market record holds.
pub const MAX_MARKET_DEMAND: i64 = 1_000_000_000;
/// Corridor length assumed when none is recorded.
pub const DEFAULT_CORRIDOR_METERS: i64 = 1000;

/// Local `bounded`: the value (or the fallback when absent), clamped.
fn bounded(value: Option<i64>, fallback: i64, low: i64, high: i64) -> i64 {
    lua::clamp(value.unwrap_or(fallback), low, high)
}

/// `marketSizeFromBuildings`: a town's model size from its native building
/// count. A missing or non-positive count means 50 buildings.
pub fn market_size_from_buildings(buildings: Option<i64>) -> i64 {
    let count = buildings
        .filter(|count| *count > 0)
        .unwrap_or(FALLBACK_TOWN_BUILDINGS);
    // Any count that makes Lua's product round is far above the clamp.
    bounded(
        Some(count.saturating_mul(NOMINAL_CAPACITY_PER_BUILDING)),
        FALLBACK_TOWN_SIZE,
        1,
        MAX_TOWN_SIZE,
    )
}

/// `gravityDemand`: hourly demand between towns of the given sizes
/// `distance_meters` apart: `size_a * size_b / (25 * km)`, clamped to
/// [50, 100000]. Sizes default to 200 and the distance to one kilometre;
/// whole kilometres count, at least one.
///
/// Total: a distance long enough for Lua's arithmetic to round makes the
/// quotient zero in both languages, which clamps to the minimum.
pub fn gravity_demand(
    size_a: Option<i64>,
    size_b: Option<i64>,
    distance_meters: Option<i64>,
) -> i64 {
    let first = bounded(size_a, FALLBACK_TOWN_SIZE, 1, MAX_TOWN_SIZE);
    let second = bounded(size_b, FALLBACK_TOWN_SIZE, 1, MAX_TOWN_SIZE);
    let km = (distance_meters.unwrap_or(DEFAULT_CORRIDOR_METERS).max(0) / 1000).max(1);
    lua::clamp(
        first * second / GRAVITY_DIVISOR.saturating_mul(km),
        MIN_DEMAND,
        MAX_DEMAND,
    )
}

/// A model town (`state.towns[cid]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Town {
    pub size: i64,
    /// Carried-passenger points toward the next building, in `[0, 400)`.
    pub growth_resid: i64,
    /// Buildings' worth of size gained through growth, capped.
    pub total_growth: i64,
}

/// Local `upsertTown`: record an observation of a town's size.
///
/// A new town takes the observed size (200 if unobserved). An existing town
/// keeps the larger of its size and the observation, so a stale peer-local
/// read can never shrink it, and its growth counters are clamped to their
/// ranges.
pub fn observe_town(existing: Option<Town>, observed_size: Option<i64>) -> Town {
    let size = bounded(observed_size, FALLBACK_TOWN_SIZE, 1, MAX_TOWN_SIZE);
    match existing {
        None => Town {
            size,
            growth_resid: 0,
            total_growth: 0,
        },
        Some(town) => Town {
            size: bounded(Some(town.size), size, 1, MAX_TOWN_SIZE).max(size),
            growth_resid: lua::clamp(town.growth_resid, 0, GROWTH_PASSENGERS_PER_BUILDING - 1),
            total_growth: lua::clamp(town.total_growth, 0, MAX_TOWN_SIZE),
        },
    }
}

/// A passenger market's contribution to town growth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CarriedMarket<'a> {
    pub town_a: &'a str,
    pub town_b: &'a str,
    /// Passengers each of the market's services delivered this settlement.
    pub delivered: &'a [i64],
}

/// Local `carriedByTown`: passengers carried per town. Each market's total
/// splits in half between its towns; the odd passenger goes to `town_b`.
/// Only passenger markets with two known towns take part.
pub fn carried_by_town<'a>(markets: &[CarriedMarket<'a>]) -> Option<BTreeMap<&'a str, i64>> {
    let mut carried = BTreeMap::new();
    for market in markets {
        let mut total = 0;
        for delivered in market.delivered {
            total = lua::add(total, (*delivered).max(0))?;
        }
        let half = lua::floor_div(total, 2)?;
        let sum = carried.entry(market.town_a).or_insert(0);
        *sum = lua::add(*sum, half)?;
        // Lua evaluates `(carried or 0) + total - half` left to right.
        let sum = carried.entry(market.town_b).or_insert(0);
        *sum = lua::sub(lua::add(*sum, total)?, half)?;
    }
    Some(carried)
}

/// One town's growth in a settlement, as `advance` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TownGrowth {
    pub carried: i64,
    pub previous_size: i64,
    pub size: i64,
    pub gained: i64,
    pub growth_resid: i64,
}

/// The growth step of `advance` for one town that carried `carried`
/// passengers: four points per passenger, a size unit per 400 points, the
/// remainder carried. Size and total growth are capped at the maximum town
/// size. A town without a record starts from `observe_town(None, None)`.
pub fn grow_town(town: Town, carried: i64) -> Option<(Town, TownGrowth)> {
    let numerator = lua::add(
        town.growth_resid.max(0),
        lua::mul(carried.max(0), NOMINAL_CAPACITY_PER_BUILDING)?,
    )?;
    let gain = numerator / GROWTH_PASSENGERS_PER_BUILDING;
    let growth_resid = numerator % GROWTH_PASSENGERS_PER_BUILDING;
    let size = lua::add(town.size, gain)?.min(MAX_TOWN_SIZE);
    let total_growth =
        lua::sub(lua::add(town.total_growth.max(0), size)?, town.size)?.min(MAX_TOWN_SIZE);
    Some((
        Town {
            size,
            growth_resid,
            total_growth,
        },
        TownGrowth {
            carried,
            previous_size: town.size,
            size,
            gained: lua::sub(size, town.size)?,
            growth_resid,
        },
    ))
}

/// Demand of a market after `refreshMarkets`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DemandRefresh {
    /// The demand before the refresh, clamped as TPF2MP reads it.
    pub previous_demand: i64,
    /// New `metadata.directDemand`.
    pub direct_demand: i64,
    /// New market demand: direct plus network demand, capped.
    pub demand: i64,
}

/// One market of `refreshMarkets`: recompute the corridor's gravity demand
/// from its towns' current sizes.
///
/// Direct demand never falls below its previous value (a missing previous
/// value is inferred as the demand minus the network demand), and network
/// demand from multi-hop routes is added on top.
pub fn refresh_market_demand(
    demand: i64,
    network_demand: Option<i64>,
    direct_demand: Option<i64>,
    size_a: i64,
    size_b: i64,
    corridor_meters: Option<i64>,
) -> DemandRefresh {
    let previous_demand = bounded(Some(demand), MIN_DEMAND, 0, MAX_MARKET_DEMAND);
    let computed = gravity_demand(Some(size_a), Some(size_b), corridor_meters);
    let prior_network = bounded(network_demand, 0, 0, MAX_MARKET_DEMAND);
    let prior_direct = bounded(
        direct_demand,
        (previous_demand - prior_network).max(0),
        0,
        MAX_MARKET_DEMAND,
    );
    let direct_demand = prior_direct.max(computed);
    DemandRefresh {
        previous_demand,
        direct_demand,
        demand: (direct_demand + prior_network).min(MAX_MARKET_DEMAND),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn market_size_falls_back_and_clamps() {
        assert_eq!(market_size_from_buildings(None), 200);
        assert_eq!(market_size_from_buildings(Some(0)), 200);
        assert_eq!(market_size_from_buildings(Some(-3)), 200);
        assert_eq!(market_size_from_buildings(Some(12)), 48);
        assert_eq!(market_size_from_buildings(Some(i64::MAX)), MAX_TOWN_SIZE);
    }

    #[test]
    fn gravity_demand_uses_whole_kilometres() {
        // 200 * 200 / (25 * 3) = 533.
        assert_eq!(gravity_demand(None, None, Some(3999)), 533);
        assert_eq!(gravity_demand(Some(80), Some(120), Some(3000)), 128);
        assert_eq!(gravity_demand(Some(1), Some(1), None), MIN_DEMAND);
        assert_eq!(
            gravity_demand(Some(MAX_TOWN_SIZE), Some(MAX_TOWN_SIZE), Some(0)),
            MAX_DEMAND
        );
        assert_eq!(gravity_demand(None, None, Some(i64::MAX)), MIN_DEMAND);
    }

    #[test]
    fn observation_never_shrinks_a_town() {
        let town = observe_town(None, Some(80));
        assert_eq!(town.size, 80);
        assert_eq!(observe_town(Some(town), Some(50)).size, 80);
        assert_eq!(observe_town(Some(town), Some(120)).size, 120);
        assert_eq!(observe_town(None, None).size, FALLBACK_TOWN_SIZE);
    }

    #[test]
    fn growth_carries_points_between_settlements() {
        let town = Town {
            size: 100,
            growth_resid: 399,
            total_growth: 5,
        };
        let (grown, report) = grow_town(town, 101).unwrap();
        // 399 + 404 = 803 points: two units, three carried.
        assert_eq!(
            grown,
            Town {
                size: 102,
                growth_resid: 3,
                total_growth: 7
            }
        );
        assert_eq!(report.gained, 2);
        let capped = Town {
            size: MAX_TOWN_SIZE,
            ..town
        };
        assert_eq!(grow_town(capped, 1000).unwrap().0.size, MAX_TOWN_SIZE);
    }

    #[test]
    fn carried_passengers_split_with_the_odd_one_to_town_b() {
        let delivered = [3, 4, -9];
        let markets = [
            CarriedMarket {
                town_a: "town:a",
                town_b: "town:b",
                delivered: &delivered,
            },
            CarriedMarket {
                town_a: "town:a",
                town_b: "town:a",
                delivered: &[5],
            },
        ];
        let carried = carried_by_town(&markets).unwrap();
        assert_eq!(carried, BTreeMap::from([("town:a", 3 + 5), ("town:b", 4)]));
    }

    #[test]
    fn refreshed_demand_never_falls() {
        let refresh = refresh_market_demand(900, Some(100), None, 200, 200, Some(3000));
        // Previous direct demand is inferred as 800 and outweighs gravity (533).
        assert_eq!(
            refresh,
            DemandRefresh {
                previous_demand: 900,
                direct_demand: 800,
                demand: 900
            }
        );
        let grown = refresh_market_demand(600, None, Some(533), 400, 200, Some(3000));
        assert_eq!(grown.direct_demand, 1066);
        assert_eq!(grown.demand, 1066);
    }
}
