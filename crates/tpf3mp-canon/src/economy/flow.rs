//! Generalized cost, pinned logit weights and share movement
//! (`economy_flow.lua`).
//!
//! A market's riders choose between its services and staying home (the
//! outside option). Every service is priced as one generalized cost in cents;
//! the logit turns cost differences into weights through the pinned
//! exponential table, and each service's share of demand glides toward its
//! logit equilibrium instead of jumping there.

use super::SHARE_SCALE;
use crate::lua;

/// `round(65536 * exp(-k / 10))` for `k = 0..=80`: the logit's exponential,
/// pinned so no platform computes `exp`.
pub const EXP_TABLE: [i64; 81] = [
    65536, 59299, 53656, 48550, 43930, 39750, 35967, 32544, 29447, 26645, 24109, 21815, 19739,
    17861, 16161, 14623, 13231, 11972, 10833, 9802, 8869, 8025, 7262, 6571, 5945, 5380, 4868, 4404,
    3985, 3606, 3263, 2952, 2671, 2417, 2187, 1979, 1791, 1620, 1466, 1327, 1200, 1086, 983, 889,
    805, 728, 659, 596, 539, 488, 442, 400, 362, 327, 296, 268, 242, 219, 198, 180, 162, 147, 133,
    120, 109, 99, 89, 81, 73, 66, 60, 54, 49, 44, 40, 36, 33, 30, 27, 24, 22,
];

/// Cost distance, in hundredths of theta, from which an option gets only the
/// cutoff weight: eight theta.
pub const CUTOFF_CENTINATS: i64 = 800;

/// Wait weight of a market recorded before model version 4, which carried no
/// kind-specific weights.
pub const DEFAULT_WAIT_WEIGHT_PM: i64 = 2000;

/// Alpha of a share whose equilibrium fell because its own fare rose (model
/// version 3 and later): it adopts the lower equilibrium at once, so a fare
/// hike cannot monetise yesterday's riders.
pub const FARE_SHOCK_ALPHA_PM: i64 = 1000;

/// Settlement interval before model version 6: one hour.
pub const LEGACY_PERIOD_SECONDS: i64 = 3600;

/// Settlement interval when none is given (model version 6 and later).
pub const DEFAULT_PERIOD_SECONDS: i64 = 300;

/// Shortest and longest settlement interval (model version 6 and later).
pub const MIN_PERIOD_SECONDS: i64 = 60;
pub const MAX_PERIOD_SECONDS: i64 = 86_400;

/// Ruleset parameters the generalized cost reads (`state.params`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CostParams {
    pub max_wait_seconds: i64,
    /// Transfer time of markets that carry none of their own.
    pub transfer_seconds: i64,
    /// Load above which in-vehicle time costs extra.
    pub crowd_threshold_ppm: i64,
}

/// Market facts the generalized cost reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarketCost {
    /// Value of time.
    pub vot_cents_per_hour: i64,
    /// `None` for markets recorded before model version 4.
    pub wait_weight_pm: Option<i64>,
    /// `None` for markets recorded before model version 4.
    pub transfer_seconds: Option<i64>,
}

/// Service facts the generalized cost reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServiceCost {
    pub headway_seconds: i64,
    pub journey_seconds: i64,
    pub transfers: i64,
    /// The previous settlement's load, in ppm of available capacity.
    pub lag_load_ppm: i64,
    /// Comfort bonus in cents.
    pub quality: i64,
    pub fare_cents: i64,
}

/// A generalized cost and its factors, as TPF2MP reports them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GeneralizedCost {
    pub fare_cents: i64,
    pub time_cost_cents: i64,
    pub wait_cost_cents: i64,
    pub transfer_cost_cents: i64,
    pub crowd_cost_cents: i64,
    /// Quality plus feeder access.
    pub comfort_cents: i64,
    /// The total, never below one cent.
    pub gc_cents: i64,
    /// Present when feeder access was modelled (model version 8 and later).
    pub feeder_access: Option<FeederAccessFactors>,
}

/// Comfort split reported when feeder access is modelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeederAccessFactors {
    pub base_comfort_cents: i64,
    pub feeder_access_cents: i64,
}

/// `generalizedCost`: everything a rider experiences, in cents.
///
/// Time is valued at the market's value of time; waiting at half the headway
/// (capped) and weighted; transfers at the market's transfer time; crowding
/// scales in-vehicle time by the previous settlement's load above the
/// threshold. Comfort (quality plus feeder access) is subtracted.
/// `feeder_access_cents` is `None` before model version 8.
///
/// Returns `None` when Lua would divide by zero, which happens when the crowd
/// threshold is the whole scale (Lua computes NaN and reports one cent), or
/// when an intermediate leaves Lua's exact range.
pub fn generalized_cost(
    params: &CostParams,
    market: &MarketCost,
    service: &ServiceCost,
    feeder_access_cents: Option<i64>,
) -> Option<GeneralizedCost> {
    let vot = market.vot_cents_per_hour;
    let wait_weight_pm = market.wait_weight_pm.unwrap_or(DEFAULT_WAIT_WEIGHT_PM);
    let transfer_seconds = market.transfer_seconds.unwrap_or(params.transfer_seconds);
    let wait_seconds = lua::floor_div(service.headway_seconds, 2)?.min(params.max_wait_seconds);
    let time_cost_cents = lua::floor_div(lua::mul(vot, service.journey_seconds)?, 3600)?;
    let wait_cost_cents = lua::floor_div(
        lua::mul(lua::mul(vot, wait_seconds)?, wait_weight_pm)?,
        3_600_000,
    )?;
    let transfer_cost_cents = lua::floor_div(
        lua::mul(lua::mul(vot, service.transfers)?, transfer_seconds)?,
        3600,
    )?;
    let crowd_span = lua::sub(SHARE_SCALE, params.crowd_threshold_ppm)?;
    let crowd_excess = lua::clamp(
        lua::sub(service.lag_load_ppm, params.crowd_threshold_ppm)?,
        0,
        crowd_span,
    );
    let crowd_cost_cents = lua::floor_div(lua::mul(time_cost_cents, crowd_excess)?, crowd_span)?;
    let base_comfort_cents = service.quality;
    let access_cents = feeder_access_cents.unwrap_or(0).max(0);
    let comfort_cents = lua::add(base_comfort_cents, access_cents)?;
    let mut total = lua::add(service.fare_cents, time_cost_cents)?;
    for cost in [wait_cost_cents, transfer_cost_cents, crowd_cost_cents] {
        total = lua::add(total, cost)?;
    }
    let gc_cents = lua::sub(total, comfort_cents)?.max(1);
    Some(GeneralizedCost {
        fare_cents: service.fare_cents,
        time_cost_cents,
        wait_cost_cents,
        transfer_cost_cents,
        crowd_cost_cents,
        comfort_cents,
        gc_cents,
        feeder_access: feeder_access_cents.map(|_| FeederAccessFactors {
            base_comfort_cents,
            feeder_access_cents: access_cents,
        }),
    })
}

/// Weight of an option at or beyond the cutoff: zero from model version 3,
/// one before (which let a dominated service keep a single rider).
pub fn logit_cutoff_weight(version: i64) -> i64 {
    if version >= 3 { 0 } else { 1 }
}

/// Local `logitWeight`: the weight of an option whose generalized cost is
/// `gc_cents` when the best option costs `gc_min_cents`.
///
/// The cost gap is measured in hundredths of `theta_cents` and read from
/// [`EXP_TABLE`] with linear interpolation between tenths. TPF2MP's exported
/// `M.logitWeight` is this function with a cutoff weight of zero.
///
/// Returns `None` for `theta_cents == 0` (Lua divides by zero) and outside
/// Lua's exact range.
pub fn logit_weight(
    gc_cents: i64,
    gc_min_cents: i64,
    theta_cents: i64,
    cutoff_weight: i64,
) -> Option<i64> {
    let centinats = lua::floor_div(
        lua::mul(lua::sub(gc_cents, gc_min_cents)?, 100)?,
        theta_cents,
    )?;
    if centinats >= CUTOFF_CENTINATS {
        return Some(cutoff_weight);
    }
    let centinats = centinats.max(0);
    let index = usize::try_from(centinats / 10).ok()?;
    let fraction = centinats % 10;
    let left = *EXP_TABLE.get(index)?;
    let right = *EXP_TABLE.get(index + 1)?;
    // The table falls, so this product is negative and Lua floors it toward
    // negative infinity; Rust's `/` would round it up.
    let step = lua::floor_div((right - left) * fraction, 10)?;
    Some(cutoff_weight.max(left + step))
}

/// Settlement interval `evaluateMarket` uses: the given one clamped to
/// [60, 86400] seconds (300 if absent) from model version 6, an hour before.
pub fn period_seconds(period_seconds: Option<i64>, version: i64) -> i64 {
    if version >= 6 {
        period_seconds
            .unwrap_or(DEFAULT_PERIOD_SECONDS)
            .clamp(MIN_PERIOD_SECONDS, MAX_PERIOD_SECONDS)
    } else {
        LEGACY_PERIOD_SECONDS
    }
}

/// Local `scaledRate`: the part of an hourly amount (demand or capacity) that
/// falls in a `period_seconds` interval, and the residual numerator (in
/// seconds) to carry so that successive intervals add up exactly.
pub fn scaled_rate(hourly: i64, residual: i64, period_seconds: i64) -> Option<(i64, i64)> {
    let numerator = lua::add(lua::mul(hourly.max(0), period_seconds)?, residual.max(0))?;
    Some((
        lua::floor_div(numerator, 3600)?,
        lua::modulo(numerator, 3600)?,
    ))
}

/// Local `glide`: move `actual` toward `equilibrium` by `alpha_pm` per mille,
/// carrying the sub-unit remainder. Returns the new value and residual.
///
/// A falling share has a negative delta, which Lua floors toward negative
/// infinity; the residual therefore always lies in `[0, 1000)`.
pub fn glide(actual: i64, equilibrium: i64, alpha_pm: i64, residual: i64) -> Option<(i64, i64)> {
    let delta = lua::add(
        lua::mul(lua::sub(equilibrium, actual)?, alpha_pm)?,
        residual,
    )?;
    let step = lua::floor_div(delta, 1000)?;
    Some((
        lua::add(actual, step)?,
        lua::sub(delta, lua::mul(step, 1000)?)?,
    ))
}

/// Share-movement parameters (`state.params`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShareParams {
    pub alpha_up_pm: i64,
    pub alpha_down_pm: i64,
}

/// A service's share stock and fare latch, as `evaluateMarket` reads and
/// writes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShareStock {
    /// `None` until the first settlement, which adopts the equilibrium.
    pub share_ppm: Option<i64>,
    pub share_resid: i64,
    /// Fare of the previous settlement (model version 3 and later).
    pub last_fare_cents: Option<i64>,
}

/// One settlement's share movement toward `equilibrium_ppm`, as the share
/// loop of `evaluateMarket` performs it.
///
/// A rising equilibrium glides up at `alpha_up_pm`. A falling one glides
/// down at `alpha_down_pm`, except that from model version 3 a fare above
/// the latched one (or no latched fare) drops to the equilibrium at once. The
/// share is clamped to the scale; its residual is kept as the glide left it.
pub fn move_share(
    stock: &ShareStock,
    equilibrium_ppm: i64,
    fare_cents: i64,
    params: &ShareParams,
    version: i64,
) -> Option<ShareStock> {
    let (share_ppm, share_resid) = match stock.share_ppm {
        None => (equilibrium_ppm, 0),
        Some(share) => {
            let shock = version >= 3
                && stock
                    .last_fare_cents
                    .is_none_or(|last_fare| fare_cents > last_fare);
            let alpha_pm = if equilibrium_ppm >= share {
                params.alpha_up_pm
            } else if shock {
                FARE_SHOCK_ALPHA_PM
            } else {
                params.alpha_down_pm
            };
            let (moved, residual) = glide(share, equilibrium_ppm, alpha_pm, stock.share_resid)?;
            (lua::clamp(moved, 0, SHARE_SCALE), residual)
        }
    };
    Some(ShareStock {
        share_ppm: Some(share_ppm),
        share_resid,
        last_fare_cents: if version >= 3 {
            Some(fare_cents)
        } else {
            stock.last_fare_cents
        },
    })
}

/// Share of the outside option after the services have moved: whatever the
/// services do not hold, never negative.
pub fn outside_share_ppm(service_shares_ppm: &[i64]) -> Option<i64> {
    let mut total = 0;
    for share in service_shares_ppm {
        total = lua::add(total, *share)?;
    }
    Some(lua::sub(SHARE_SCALE, total)?.max(0))
}

/// A service's load for the next settlement's crowding cost: riders who chose
/// it per unit of the capacity it offered, in ppm; the full scale when it
/// offered none.
pub fn lag_load_ppm(requested: i64, available_capacity: i64) -> Option<i64> {
    if available_capacity > 0 {
        lua::floor_div(lua::mul(requested, SHARE_SCALE)?, available_capacity)
    } else {
        Some(SHARE_SCALE)
    }
}

/// A service's allocation in basis points of the market's demand, as
/// reported in settlement results.
pub fn share_basis_points(allocated: i64, demand: i64) -> Option<i64> {
    if demand > 0 {
        lua::floor_div(lua::mul(allocated, 10_000)?, demand)
    } else {
        Some(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARAMS: CostParams = CostParams {
        max_wait_seconds: 1800,
        transfer_seconds: 480,
        crowd_threshold_ppm: 700_000,
    };

    #[test]
    fn logit_weight_interpolates_and_floors_downward() {
        // 31 centinats: index 3, fraction 1; (43930 - 48550) * 1 / 10 = -462.
        assert_eq!(logit_weight(1031, 1000, 100, 0), Some(48550 - 462));
        // 3 centinats: (59299 - 65536) * 3 / 10 = -1871.1, floored to -1872.
        assert_eq!(logit_weight(1003, 1000, 100, 0), Some(65536 - 1872));
        assert_eq!(logit_weight(1000, 1000, 100, 0), Some(65536));
        assert_eq!(logit_weight(900, 1000, 100, 0), Some(65536));
        // 799 centinats: (22 - 24) * 9 / 10 = -1.8, floored to -2.
        assert_eq!(logit_weight(1799, 1000, 100, 0), Some(22));
        assert_eq!(logit_weight(1800, 1000, 100, 0), Some(0));
        assert_eq!(logit_weight(1800, 1000, 100, 1), Some(1));
        assert_eq!(logit_weight(1000, 1000, 0, 0), None);
    }

    #[test]
    fn glide_floors_negative_steps() {
        // (0 - 10) * 250 + 0 = -2500: step -3, residual 500.
        assert_eq!(glide(10, 0, 250, 0), Some((7, 500)));
        assert_eq!(glide(0, 10, 250, 999), Some((3, 499)));
    }

    #[test]
    fn scaled_rate_carries_the_remainder() {
        assert_eq!(scaled_rate(100, 0, 300), Some((8, 1200)));
        assert_eq!(scaled_rate(100, 2400, 300), Some((9, 0)));
        assert_eq!(scaled_rate(-5, -5, 300), Some((0, 0)));
    }

    #[test]
    fn crowding_at_the_whole_scale_is_refused() {
        let market = MarketCost {
            vot_cents_per_hour: 450,
            wait_weight_pm: None,
            transfer_seconds: None,
        };
        let service = ServiceCost {
            headway_seconds: 900,
            journey_seconds: 2400,
            transfers: 1,
            lag_load_ppm: 900_000,
            quality: 100,
            fare_cents: 1000,
        };
        let cost = generalized_cost(&PARAMS, &market, &service, None).unwrap();
        assert_eq!(cost.time_cost_cents, 300);
        assert_eq!(cost.wait_cost_cents, 112);
        assert_eq!(cost.transfer_cost_cents, 60);
        assert_eq!(cost.crowd_cost_cents, 200);
        assert_eq!(cost.gc_cents, 1000 + 300 + 112 + 60 + 200 - 100);
        assert_eq!(cost.feeder_access, None);
        let saturated = CostParams {
            crowd_threshold_ppm: SHARE_SCALE,
            ..PARAMS
        };
        assert_eq!(generalized_cost(&saturated, &market, &service, None), None);
    }

    #[test]
    fn share_moves_at_once_after_a_fare_hike() {
        let params = ShareParams {
            alpha_up_pm: 350,
            alpha_down_pm: 500,
        };
        let stock = ShareStock {
            share_ppm: Some(400_000),
            share_resid: 0,
            last_fare_cents: Some(1000),
        };
        let hiked = move_share(&stock, 100_000, 2000, &params, 10).unwrap();
        assert_eq!(hiked.share_ppm, Some(100_000));
        assert_eq!(hiked.last_fare_cents, Some(2000));
        let steady = move_share(&stock, 100_000, 1000, &params, 10).unwrap();
        assert_eq!(steady.share_ppm, Some(250_000));
        let legacy = move_share(&stock, 100_000, 2000, &params, 2).unwrap();
        assert_eq!(legacy.share_ppm, Some(250_000));
        assert_eq!(legacy.last_fare_cents, Some(1000));
    }
}
