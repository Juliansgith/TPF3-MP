//! Lua 5.1 number semantics on exact integers.
//!
//! TPF2MP's rules run on Lua 5.1, where every number is an IEEE-754 double.
//! A double holds every integer of magnitude up to 2^53 exactly, and on such
//! integers `+`, `-`, `*`, `math.floor(a / b)` and `a % b` give the exact
//! integer result. Beyond that range Lua rounds, and not necessarily the
//! same way everywhere: a C compiler may contract Lua 5.1's
//! `a - floor(a / b) * b` into a fused multiply-subtract (see "The deciding
//! constraint" in `docs/ARCHITECTURE.md`), which changes an inexact
//! remainder but never an exact one.
//!
//! These helpers compute in `i64` and return `None` as soon as an operand or
//! a result leaves the exact range. A port built from them either returns
//! exactly what Lua returns or refuses the input; it never returns a
//! different number. `docs/ECONOMY.md` lists where TPF2MP can leave the range.

/// Largest integer below which every integer is exactly representable as a
/// double: 2^53 - 1. TPF2MP's protocol validators use the same bound.
pub const MAX_EXACT_INTEGER: i64 = (1 << 53) - 1;

/// `value` if Lua represents it and everything smaller in magnitude exactly.
pub fn exact(value: i64) -> Option<i64> {
    (value.unsigned_abs() <= MAX_EXACT_INTEGER.unsigned_abs()).then_some(value)
}

/// Lua `a + b`.
pub fn add(a: i64, b: i64) -> Option<i64> {
    exact(exact(a)?.checked_add(exact(b)?)?)
}

/// Lua `a - b`.
pub fn sub(a: i64, b: i64) -> Option<i64> {
    exact(exact(a)?.checked_sub(exact(b)?)?)
}

/// Lua `a * b`.
pub fn mul(a: i64, b: i64) -> Option<i64> {
    exact(exact(a)?.checked_mul(exact(b)?)?)
}

/// Lua `math.floor(a / b)`.
///
/// The double quotient can be inexact, but its floor is exact whenever
/// `|a| <= 2^53`: rounding could only reach the next integer if the quotient
/// were closer to it than `1 / |b|`, which needs `|a| > 2^53`. Unlike
/// [`crate::floor_div`], operands outside Lua's exact range are refused. A
/// zero divisor is refused too; Lua would produce an infinity or NaN.
pub fn floor_div(a: i64, b: i64) -> Option<i64> {
    crate::floor_div(exact(a)?, exact(b)?)
}

/// Lua 5.1 `a % b`, defined as `a - math.floor(a / b) * b`: the result has
/// the sign of `b`, unlike Rust's `%`, which has the sign of `a`.
///
/// `math.floor(a / b) * b` lies within `|b|` of `a`, so every intermediate is
/// exact when `|a| + |b|` is.
pub fn modulo(a: i64, b: i64) -> Option<i64> {
    exact(
        exact(a)?
            .checked_abs()?
            .checked_add(exact(b)?.checked_abs()?)?,
    )?;
    let remainder = a.checked_rem(b)?;
    if remainder != 0 && (remainder < 0) != (b < 0) {
        Some(remainder + b)
    } else {
        Some(remainder)
    }
}

/// TPF2MP's `util.clamp`. Unlike [`i64::clamp`] it never panics: when
/// `low > high` a value below `low` gives `low` and any other gives `high`.
pub fn clamp(value: i64, low: i64, high: i64) -> i64 {
    if value < low {
        low
    } else if value > high {
        high
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_53: i64 = 1 << 53;

    #[test]
    fn exact_range_is_symmetric_and_excludes_two_to_the_53() {
        assert_eq!(exact(MAX_EXACT_INTEGER), Some(MAX_EXACT_INTEGER));
        assert_eq!(exact(-MAX_EXACT_INTEGER), Some(-MAX_EXACT_INTEGER));
        assert_eq!(exact(TWO_53), None);
        assert_eq!(exact(-TWO_53), None);
        assert_eq!(exact(i64::MIN), None);
    }

    #[test]
    fn arithmetic_refuses_results_outside_the_exact_range() {
        assert_eq!(add(MAX_EXACT_INTEGER, 0), Some(MAX_EXACT_INTEGER));
        assert_eq!(add(MAX_EXACT_INTEGER, 1), None);
        assert_eq!(sub(-MAX_EXACT_INTEGER, 1), None);
        assert_eq!(mul(1 << 26, 1 << 26), Some(1 << 52));
        assert_eq!(mul(1 << 27, 1 << 26), None);
        assert_eq!(mul(i64::MAX, 0), None, "an inexact operand is refused");
    }

    #[test]
    fn floor_div_floors_and_refuses_zero_divisors() {
        assert_eq!(floor_div(-7, 2), Some(-4));
        assert_eq!(floor_div(7, -2), Some(-4));
        assert_eq!(floor_div(7, 0), None);
        assert_eq!(floor_div(TWO_53, 3), None);
    }

    #[test]
    fn modulo_takes_the_sign_of_the_divisor() {
        assert_eq!(modulo(-7, 3), Some(2));
        assert_eq!(modulo(7, -3), Some(-2));
        assert_eq!(modulo(-7, -3), Some(-1));
        assert_eq!(modulo(6, 3), Some(0));
        assert_eq!(modulo(-6, 3), Some(0));
        assert_eq!(modulo(1, 0), None);
        assert_eq!(
            modulo(MAX_EXACT_INTEGER, 1),
            None,
            "|a| + |b| must be exact"
        );
    }

    #[test]
    fn clamp_matches_util_clamp_when_bounds_cross() {
        assert_eq!(clamp(5, 0, 10), 5);
        assert_eq!(clamp(-5, 0, 10), 0);
        assert_eq!(clamp(15, 0, 10), 10);
        assert_eq!(clamp(-1, 0, -3), 0);
        assert_eq!(clamp(7, 0, -3), -3);
    }
}
