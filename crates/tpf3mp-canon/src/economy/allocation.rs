//! Largest-remainder choice and capacity admission
//! (`economy_allocation.lua`).
//!
//! Passengers and cargo units are indivisible, so proportional shares are
//! rounded by the largest-remainder method with ties broken by canonical id.
//! Every replica makes the same indivisible decisions.

use std::collections::BTreeMap;

use crate::lua;

/// Canonical id of the outside option (not travelling, or trucking for
/// cargo). It sorts after every `line:` id, which decides its ties.
pub const OUTSIDE_CID: &str = "~outside";

/// `proportional`: split `total` units in proportion to the item weights.
///
/// Each item gets the floor of its exact share; the units left over go one
/// each to the items with the largest remainders, ties to the smaller id.
/// Every item appears in the result, even with zero units, unless `total` or
/// the weight sum is not positive, in which case the result is empty. An id
/// listed twice keeps its last floor plus the extra units of both entries,
/// as the Lua table does.
///
/// The ids' `Ord` decides ties, so it must be Lua's string order: bytewise,
/// which `str` and `String` implement.
pub fn proportional<K: Ord + Clone>(total: i64, items: &[(K, i64)]) -> Option<BTreeMap<K, i64>> {
    let mut weight_sum = 0;
    for (_, weight) in items {
        weight_sum = lua::add(weight_sum, *weight)?;
    }
    let mut allocations = BTreeMap::new();
    if total <= 0 || weight_sum <= 0 {
        return Some(allocations);
    }
    let mut ranked = Vec::with_capacity(items.len());
    let mut used = 0;
    for (cid, weight) in items {
        let numerator = lua::mul(total, *weight)?;
        let base = lua::floor_div(numerator, weight_sum)?;
        allocations.insert(cid.clone(), base);
        used = lua::add(used, base)?;
        ranked.push((cid, lua::modulo(numerator, weight_sum)?));
    }
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    // The remainders are each below the weight sum and add up to the units
    // left over, so there are fewer of those than items; Lua still wraps
    // around, and so does this loop.
    let count = i64::try_from(ranked.len()).ok()?;
    for index in 0..lua::sub(total, used)? {
        let (cid, _) = ranked.get(usize::try_from(index % count).ok()?)?;
        let amount = allocations.get_mut(*cid)?;
        *amount = lua::add(*amount, 1)?;
    }
    Some(allocations)
}

/// A service competing for a market's demand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapacityOption<'a> {
    pub cid: &'a str,
    pub share_ppm: i64,
    /// Units the service can carry in this settlement.
    pub available_capacity: i64,
}

/// Result of [`capacity_constrained`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapacityAllocation<'a> {
    /// Units each service carries, and the outside option's units.
    pub allocations: BTreeMap<&'a str, i64>,
    /// First proportional choice before capacity (model version 9 and later).
    pub requested: Option<BTreeMap<&'a str, i64>>,
    /// Chosen units no service could carry (model version 9 and later).
    pub queued: i64,
}

fn choice_items<'a>(services: &[&CapacityOption<'a>], outside_ppm: i64) -> Vec<(&'a str, i64)> {
    let mut items = Vec::with_capacity(services.len() + 1);
    items.push((OUTSIDE_CID, outside_ppm));
    items.extend(services.iter().map(|option| (option.cid, option.share_ppm)));
    items
}

/// `capacityConstrained`: allocate `demand` between the services' shares
/// and the outside option, then admit no more than each service can carry.
///
/// From model version 9 the first proportional choice is kept as
/// `requested`, capacity caps each service's allocation, and the overflow
/// is reported as `queued`. Earlier versions instead re-split each capped
/// service's riders among the remaining services and the outside option,
/// round by round; that path is kept so old checkpoints replay.
pub fn capacity_constrained<'a>(
    demand: i64,
    services: &[CapacityOption<'a>],
    outside_ppm: i64,
    version: i64,
) -> Option<CapacityAllocation<'a>> {
    let all: Vec<&CapacityOption<'a>> = services.iter().collect();
    if version >= 9 {
        let requested = proportional(demand, &choice_items(&all, outside_ppm))?;
        let mut allocations = BTreeMap::new();
        allocations.insert(
            OUTSIDE_CID,
            requested.get(OUTSIDE_CID).copied().unwrap_or(0),
        );
        let mut queued = 0;
        for option in services {
            let amount = requested.get(option.cid).copied().unwrap_or(0);
            let admitted = amount.min(option.available_capacity);
            allocations.insert(option.cid, admitted);
            queued = lua::add(queued, lua::sub(amount, admitted)?.max(0))?;
        }
        return Some(CapacityAllocation {
            allocations,
            requested: Some(requested),
            queued,
        });
    }

    let mut active = all;
    let mut allocations: BTreeMap<&'a str, i64> = BTreeMap::new();
    let mut remaining = demand;
    while remaining > 0 && !active.is_empty() {
        let preview = proportional(remaining, &choice_items(&active, outside_ppm))?;
        let (capped, survivors): (Vec<&CapacityOption<'a>>, Vec<&CapacityOption<'a>>) =
            active.into_iter().partition(|option| {
                preview.get(option.cid).copied().unwrap_or(0) > option.available_capacity
            });
        if capped.is_empty() {
            for (cid, amount) in preview {
                let total = allocations.entry(cid).or_insert(0);
                *total = lua::add(*total, amount)?;
            }
            remaining = 0;
        } else {
            for option in capped {
                allocations.insert(option.cid, option.available_capacity);
                remaining = lua::sub(remaining, option.available_capacity)?;
            }
        }
        active = survivors;
    }
    if remaining > 0 {
        let outside = allocations.entry(OUTSIDE_CID).or_insert(0);
        *outside = lua::add(*outside, remaining)?;
    }
    Some(CapacityAllocation {
        allocations,
        requested: None,
        queued: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leftover_units_go_to_the_largest_remainders_then_the_smaller_id() {
        let items = [("b", 1), ("a", 1), ("c", 1)];
        let result = proportional(4, &items).unwrap();
        // 4/3 each: floors of 1, remainders all 1; one unit left, to "a".
        assert_eq!(result, BTreeMap::from([("a", 2), ("b", 1), ("c", 1)]));
        let result = proportional(5, &[("x", 3), ("y", 1)]).unwrap();
        // 15/4 = 3 r3, 5/4 = 1 r1: the unit goes to "x".
        assert_eq!(result, BTreeMap::from([("x", 4), ("y", 1)]));
    }

    #[test]
    fn nothing_to_allocate_gives_an_empty_result() {
        assert!(proportional(0, &[("a", 1)]).unwrap().is_empty());
        assert!(proportional(10, &[("a", 0)]).unwrap().is_empty());
        assert!(proportional::<&str>(10, &[]).unwrap().is_empty());
    }

    #[test]
    fn version_nine_queues_riders_over_capacity() {
        let services = [CapacityOption {
            cid: "line:a",
            share_ppm: 500_000,
            available_capacity: 30,
        }];
        let result = capacity_constrained(100, &services, 500_000, 9).unwrap();
        assert_eq!(
            result.allocations,
            BTreeMap::from([("line:a", 30), (OUTSIDE_CID, 50)])
        );
        assert_eq!(result.queued, 20);
        assert_eq!(
            result.requested,
            Some(BTreeMap::from([("line:a", 50), (OUTSIDE_CID, 50)]))
        );
    }

    #[test]
    fn legacy_versions_resplit_capped_riders() {
        let services = [
            CapacityOption {
                cid: "line:a",
                share_ppm: 500_000,
                available_capacity: 30,
            },
            CapacityOption {
                cid: "line:b",
                share_ppm: 250_000,
                available_capacity: 1000,
            },
        ];
        let result = capacity_constrained(100, &services, 250_000, 8).unwrap();
        // Round one caps line:a at 30; the other 70 split 1:1.
        assert_eq!(
            result.allocations,
            BTreeMap::from([("line:a", 30), ("line:b", 35), (OUTSIDE_CID, 35)])
        );
        assert_eq!(result.requested, None);
        assert_eq!(result.queued, 0);
    }
}
