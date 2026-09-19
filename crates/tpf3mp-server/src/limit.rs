//! Rate limits.

use std::time::Instant;

/// A token bucket: `burst` tokens at most, refilled continuously at
/// `per_second`. Counts in thousandths of a token, so small rates stay
/// exact.
#[derive(Debug)]
pub(crate) struct TokenBucket {
    per_second: u32,
    capacity_milli: u64,
    milli: u64,
    last: Instant,
}

impl TokenBucket {
    /// A full bucket.
    pub(crate) fn new(per_second: u32, burst: u32) -> Self {
        let capacity_milli = u64::from(burst) * 1000;
        Self {
            per_second,
            capacity_milli,
            milli: capacity_milli,
            last: Instant::now(),
        }
    }

    /// Takes `cost` tokens if the bucket holds them.
    pub(crate) fn take(&mut self, now: Instant, cost: u64) -> bool {
        let elapsed_ms =
            u64::try_from(now.saturating_duration_since(self.last).as_millis()).unwrap_or(u64::MAX);
        self.last = now;
        self.milli = self
            .milli
            .saturating_add(elapsed_ms.saturating_mul(u64::from(self.per_second)))
            .min(self.capacity_milli);
        let cost_milli = cost.saturating_mul(1000);
        if self.milli >= cost_milli {
            self.milli -= cost_milli;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_bucket_allows_a_burst_then_the_rate() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new(10, 3);
        bucket.last = start;
        assert!((0..3).all(|_| bucket.take(start, 1)));
        assert!(!bucket.take(start, 1));
        // 100 ms at 10 per second refills exactly one token.
        assert!(bucket.take(start + Duration::from_millis(100), 1));
        assert!(!bucket.take(start + Duration::from_millis(100), 1));
    }

    #[test]
    fn a_bucket_of_bytes_takes_whole_payloads() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new(1000, 5000);
        bucket.last = start;
        assert!(bucket.take(start, 4000));
        assert!(!bucket.take(start, 2000), "only 1000 bytes left");
        assert!(bucket.take(start + Duration::from_secs(1), 2000));
    }
}
