//! Difficulty presets and revenue scaling (`economy_difficulty.lua`).
//!
//! Difficulty scales gross revenue only. The sub-cent remainder, in ppm of a
//! cent, is service state, so many small payments add up to what one large
//! payment would earn.

use super::ACCUMULATOR_LIMIT;

/// Parts per million of a revenue multiplier.
pub const SCALE: i64 = 1_000_000;
/// Largest revenue multiplier `apply` accepts: 4x.
pub const MAX_MULTIPLIER_PPM: i64 = 4_000_000;

/// A save's economy difficulty.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Difficulty {
    Normal,
    Hard,
    Easy,
    Relaxed,
}

impl Difficulty {
    pub const DEFAULT: Self = Self::Normal;

    /// Presentation order (`M.ORDER`).
    pub const ORDER: [Self; 4] = [Self::Normal, Self::Hard, Self::Easy, Self::Relaxed];

    /// The preset key stored in match rules.
    pub const fn key(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Hard => "hard",
            Self::Easy => "easy",
            Self::Relaxed => "relaxed",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Normal => "Normal",
            Self::Hard => "Hard",
            Self::Easy => "Easy",
            Self::Relaxed => "Relaxed",
        }
    }

    pub const fn revenue_multiplier_ppm(self) -> i64 {
        match self {
            Self::Normal => 1_000_000,
            Self::Hard => 600_000,
            Self::Easy => 1_500_000,
            Self::Relaxed => 2_000_000,
        }
    }

    /// `normaliseKey`: the preset named by `key`, ignoring ASCII case; any
    /// other key, including a missing one (pass `""`), selects the default.
    ///
    /// Lua lowercases with C `tolower`, which in the C locale folds ASCII
    /// letters only, as [`str::eq_ignore_ascii_case`] does.
    pub fn from_key(key: &str) -> Self {
        Self::ORDER
            .into_iter()
            .find(|preset| key.eq_ignore_ascii_case(preset.key()))
            .unwrap_or(Self::DEFAULT)
    }
}

/// `apply`: scale `raw_cents` by `multiplier_ppm`, carrying `residual` (ppm
/// of a cent) in and out. Returns the scaled cents and the new residual.
///
/// The raw amount is clamped to `[0, ACCUMULATOR_LIMIT]`, the multiplier to
/// `[0, 4x]` and the residual to `[0, SCALE)`. Whole millions and the
/// remainder are scaled separately, so no product exceeds 4 * 10^15 and the
/// function is total. A result that reaches the limit returns the limit and
/// drops the residual.
pub fn apply(raw_cents: i64, multiplier_ppm: i64, residual: i64) -> (i64, i64) {
    let raw = raw_cents.clamp(0, ACCUMULATOR_LIMIT);
    let multiplier = multiplier_ppm.clamp(0, MAX_MULTIPLIER_PPM);
    let carried = residual.clamp(0, SCALE - 1);
    let (whole, remainder) = (raw / SCALE, raw % SCALE);
    let base = whole * multiplier;
    if base >= ACCUMULATOR_LIMIT {
        return (ACCUMULATOR_LIMIT, 0);
    }
    let tail = remainder * multiplier + carried;
    let scaled = base + tail / SCALE;
    if scaled >= ACCUMULATOR_LIMIT {
        return (ACCUMULATOR_LIMIT, 0);
    }
    (scaled, tail % SCALE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_normalise_case_insensitively() {
        assert_eq!(Difficulty::from_key("HARD"), Difficulty::Hard);
        assert_eq!(Difficulty::from_key("Relaxed"), Difficulty::Relaxed);
        assert_eq!(Difficulty::from_key(""), Difficulty::Normal);
        assert_eq!(Difficulty::from_key("nightmare"), Difficulty::Normal);
        assert_eq!(Difficulty::from_key("h\u{e4}rd"), Difficulty::Normal);
    }

    #[test]
    fn many_small_payments_equal_one_large_payment() {
        let multiplier = Difficulty::Hard.revenue_multiplier_ppm();
        let (mut total, mut residual) = (0, 0);
        for _ in 0..7 {
            let (scaled, carried) = apply(3, multiplier, residual);
            total += scaled;
            residual = carried;
        }
        assert_eq!(total, apply(21, multiplier, 0).0);
    }

    #[test]
    fn apply_saturates_at_the_limit() {
        assert_eq!(apply(i64::MAX, i64::MAX, i64::MAX), (ACCUMULATOR_LIMIT, 0));
        assert_eq!(
            apply(ACCUMULATOR_LIMIT, 1_000_000, 0),
            (ACCUMULATOR_LIMIT, 0)
        );
        // 1 cent at 1.5x plus a carried 0.6 cent: 2 cents, 0.1 cent carried.
        assert_eq!(apply(1, 1_500_000, 600_000), (2, 100_000));
    }
}
