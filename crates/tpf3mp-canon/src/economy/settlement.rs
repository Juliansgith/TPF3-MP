//! Aggregate arithmetic of settlement and scoring (`economy.lua`).
//!
//! Every function here is total: TPF2MP clamps each result to
//! [`ACCUMULATOR_LIMIT`], and a double that has to round is already beyond
//! the clamp, so rounding never reaches a result.

use super::ACCUMULATOR_LIMIT;

/// Cents per native wallet dollar.
pub const CENTS_PER_DOLLAR: i64 = 100;

/// `saturatingAdd`: sum of the non-negative parts, capped at the limit.
pub fn saturating_add(left: i64, right: i64) -> i64 {
    left.max(0)
        .saturating_add(right.max(0))
        .min(ACCUMULATOR_LIMIT)
}

/// `saturatingMultiply` of `economy.lua`: product of the non-negative parts,
/// capped at the limit. The revenue module has a different function of the
/// same name, [`super::revenue::saturating_multiply`], which clamps its
/// operands first.
pub fn saturating_multiply(left: i64, right: i64) -> i64 {
    left.max(0)
        .saturating_mul(right.max(0))
        .min(ACCUMULATOR_LIMIT)
}

/// `signedAdd`: a sum clamped to `±ACCUMULATOR_LIMIT`.
pub fn signed_add(left: i64, right: i64) -> i64 {
    left.saturating_add(right)
        .clamp(-ACCUMULATOR_LIMIT, ACCUMULATOR_LIMIT)
}

/// `walletDeltaDollars`: whole dollars to credit to a native wallet, and the
/// signed sub-dollar residual to carry into the next settlement.
///
/// The quotient truncates toward zero, so a one-cent loss stays a carried
/// cent instead of a one-dollar debit, and the residual keeps the sign of the
/// combined amount. This is Rust's `/` and `%`, not Lua's floor.
pub fn wallet_delta_dollars(net_revenue_cents: i64, carried_residual_cents: i64) -> (i64, i64) {
    let combined = signed_add(net_revenue_cents, carried_residual_cents);
    (combined / CENTS_PER_DOLLAR, combined % CENTS_PER_DOLLAR)
}

/// The scoreboard's `modelValueCents` from a company's settled totals:
/// ten times net revenue, plus 100 per settled passenger, 500,000 per market
/// reached and 250,000 per active line, never below zero.
pub fn model_value_cents(
    net_revenue_cents: i64,
    demand: i64,
    markets_reached: i64,
    active_lines: i64,
) -> i64 {
    // Lua forms `net * 10` without saturation. Once that product is too large
    // for a double to hold exactly (above 2^53), the clamped sum is already
    // at the limit, so saturating here gives the same result.
    let earnings = signed_add(
        net_revenue_cents.saturating_mul(10),
        saturating_multiply(demand, 100),
    );
    let reach = saturating_add(
        saturating_multiply(markets_reached, 500_000),
        saturating_multiply(active_lines, 250_000),
    );
    signed_add(earnings, reach).max(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallet_delta_truncates_toward_zero_and_carries_the_sign() {
        assert_eq!(wallet_delta_dollars(199, 0), (1, 99));
        assert_eq!(wallet_delta_dollars(-199, 0), (-1, -99));
        assert_eq!(wallet_delta_dollars(-1, 0), (0, -1));
        assert_eq!(wallet_delta_dollars(50, 60), (1, 10));
        assert_eq!(wallet_delta_dollars(-50, -60), (-1, -10));
        assert_eq!(
            wallet_delta_dollars(i64::MAX, i64::MAX),
            (ACCUMULATOR_LIMIT / 100, 0)
        );
    }

    #[test]
    fn aggregates_saturate() {
        assert_eq!(saturating_add(-5, 7), 7);
        assert_eq!(saturating_add(ACCUMULATOR_LIMIT, 1), ACCUMULATOR_LIMIT);
        assert_eq!(saturating_multiply(i64::MAX, 2), ACCUMULATOR_LIMIT);
        assert_eq!(saturating_multiply(-3, 4), 0);
        assert_eq!(signed_add(i64::MIN, -1), -ACCUMULATOR_LIMIT);
    }

    #[test]
    fn model_value_never_goes_negative() {
        assert_eq!(model_value_cents(-1_000_000, 0, 0, 0), 0);
        assert_eq!(model_value_cents(10, 2, 1, 1), 100 + 200 + 750_000);
        assert_eq!(
            model_value_cents(ACCUMULATOR_LIMIT, 0, 0, 0),
            ACCUMULATOR_LIMIT
        );
    }
}
