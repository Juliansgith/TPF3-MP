//! Canonical game state and rules.
//!
//! The server's canonical state is the truth that every native replica is
//! checked against (see `docs/ARCHITECTURE.md`), so everything here must give
//! bit-identical results on every platform, compiler and build. The crate
//! therefore:
//!
//! - uses integer and fixed-point arithmetic only (`clippy::float_arithmetic`
//!   is denied);
//! - never reads a clock, a random source or the environment;
//! - never iterates a hash-ordered collection (`clippy.toml` disallows
//!   `HashMap` and `HashSet`; use `BTreeMap` and `BTreeSet`).
//!
//! Rules ported from TPF2MP's Lua economy must reproduce its parity vectors
//! exactly, including Lua's rounding (see [`floor_div`] and [`lua`]). The
//! port lives in [`economy`]; `docs/ECONOMY.md` describes it and the
//! differential tests that hold it to the original Lua.

#![deny(clippy::float_arithmetic, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod economy;
pub mod lua;

/// An amount of money in cents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cents(pub i64);

impl Cents {
    pub const ZERO: Self = Self(0);

    pub fn checked_add(self, rhs: Self) -> Option<Self> {
        self.0.checked_add(rhs.0).map(Self)
    }

    pub fn checked_sub(self, rhs: Self) -> Option<Self> {
        self.0.checked_sub(rhs.0).map(Self)
    }

    /// `self * numerator / denominator`, rounded toward negative infinity. The
    /// product is exact (128-bit), so only the final result can overflow.
    pub fn mul_div_floor(self, numerator: i64, denominator: i64) -> Option<Self> {
        if denominator == 0 {
            return None;
        }
        let product = i128::from(self.0) * i128::from(numerator);
        let denominator = i128::from(denominator);
        let quotient = product / denominator;
        let floored = if product % denominator != 0 && (product < 0) != (denominator < 0) {
            quotient - 1
        } else {
            quotient
        };
        i64::try_from(floored).ok().map(Self)
    }
}

/// Integer division rounding toward negative infinity, like Lua's
/// `math.floor(a / b)`.
///
/// Rust's `/` rounds toward zero, which differs whenever the exact quotient is
/// negative and not whole: `-7 / 2 == -3`, but `floor_div(-7, 2) == Some(-4)`.
/// A port of Lua rules that used `/` would diverge from the Lua original only
/// on negative values, which parity vectors easily miss. Returns `None` when
/// `b` is zero or the result overflows (`i64::MIN / -1`).
pub fn floor_div(a: i64, b: i64) -> Option<i64> {
    let quotient = a.checked_div(b)?;
    let remainder = a.checked_rem(b)?;
    if remainder != 0 && (remainder < 0) != (b < 0) {
        quotient.checked_sub(1)
    } else {
        Some(quotient)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_div_rounds_toward_negative_infinity() {
        let cases = [
            (7, 2, 3),
            (-7, 2, -4),
            (7, -2, -4),
            (-7, -2, 3),
            (6, 3, 2),
            (-6, 3, -2),
            (0, 5, 0),
            (-1, 3, -1),
            (i64::MIN, 1, i64::MIN),
            (i64::MAX, -1, -i64::MAX),
        ];
        for (a, b, expected) in cases {
            assert_eq!(floor_div(a, b), Some(expected), "floor_div({a}, {b})");
        }
    }

    #[test]
    fn floor_div_reports_invalid_operations() {
        assert_eq!(floor_div(1, 0), None);
        assert_eq!(floor_div(i64::MIN, -1), None);
    }

    #[test]
    fn mul_div_floor_is_exact_beyond_64_bits() {
        // 9e18 * 3 overflows i64, but the result of the division fits.
        assert_eq!(
            Cents(9_000_000_000_000_000_000).mul_div_floor(3, 4),
            Some(Cents(6_750_000_000_000_000_000))
        );
    }

    #[test]
    fn mul_div_floor_rounds_like_lua() {
        // Lua: math.floor(-100 * 1 / 3) == -34
        assert_eq!(Cents(-100).mul_div_floor(1, 3), Some(Cents(-34)));
        assert_eq!(Cents(100).mul_div_floor(1, 3), Some(Cents(33)));
        assert_eq!(Cents(100).mul_div_floor(-1, 3), Some(Cents(-34)));
    }

    #[test]
    fn mul_div_floor_reports_invalid_operations() {
        assert_eq!(Cents(1).mul_div_floor(1, 0), None);
        assert_eq!(Cents(i64::MAX).mul_div_floor(2, 1), None);
    }

    #[test]
    fn checked_arithmetic_reports_overflow() {
        assert_eq!(Cents(i64::MAX).checked_add(Cents(1)), None);
        assert_eq!(Cents(i64::MIN).checked_sub(Cents(1)), None);
        assert_eq!(Cents(5).checked_sub(Cents(7)), Some(Cents(-2)));
    }
}
