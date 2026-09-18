//! The room clock: how far the sequencer may seal. See "Pacing" in
//! `docs/PROTOCOL.md`.

use std::time::Duration;

use tpf3mp_proto::Speed;

/// One step, in units of microseconds times speed percent.
const UNITS_PER_STEP: u128 = 1_000_000 * 100;

/// Advances a room's ideal step count in real time and turns it into the
/// frontier the sequencer may seal.
#[derive(Debug, Clone)]
pub(crate) struct Pacer {
    steps_per_second: u16,
    /// How far ahead of the ideal clock the frontier runs, hiding latency.
    input_delay: Duration,
    /// How far past the slowest pacing member the frontier may run.
    max_ahead: Duration,
    ideal: u64,
    remainder: u128,
}

impl Pacer {
    pub(crate) fn new(steps_per_second: u16, input_delay: Duration, max_ahead: Duration) -> Self {
        Self {
            steps_per_second,
            input_delay,
            max_ahead,
            ideal: 0,
            remainder: 0,
        }
    }

    /// Continues a clock that stood at `step`, for a room recovered from its
    /// log.
    pub(crate) fn resume_at(&mut self, step: u64) {
        self.ideal = step;
        self.remainder = 0;
    }

    /// Steps that `duration` covers at `speed`, rounded up.
    fn steps_in(&self, duration: Duration, speed: Speed) -> u64 {
        let units = duration.as_micros() * u128::from(self.steps_per_second) * u128::from(speed.0);
        u64::try_from(units.div_ceil(UNITS_PER_STEP)).unwrap_or(u64::MAX)
    }

    /// The lead the frontier keeps over the ideal clock at `speed`.
    pub(crate) fn lead(&self, speed: Speed) -> u64 {
        if speed.is_paused() {
            0
        } else {
            self.steps_in(self.input_delay, speed).max(1)
        }
    }

    /// How far behind the frontier a member may be and still pace the room.
    /// Members further behind are catching up and do not hold the others.
    pub(crate) fn window(&self, speed: Speed) -> u64 {
        let pace = speed.max(Speed::NORMAL);
        self.lead(pace) + self.steps_in(self.max_ahead, pace)
    }

    /// Advances the clock by `elapsed` and returns the new frontier, which is
    /// never below `sealed`.
    ///
    /// `slowest` is the lowest progress among the members pacing the room;
    /// `None` holds the clock, for example while members are still loading
    /// the world or nobody is connected.
    pub(crate) fn advance(
        &mut self,
        elapsed: Duration,
        speed: Speed,
        slowest: Option<u64>,
        sealed: u64,
    ) -> u64 {
        let Some(slowest) = slowest else {
            return sealed;
        };
        if !speed.is_paused() {
            let units = self.remainder
                + elapsed.as_micros() * u128::from(self.steps_per_second) * u128::from(speed.0);
            let whole = u64::try_from(units / UNITS_PER_STEP).unwrap_or(u64::MAX);
            self.ideal = self.ideal.saturating_add(whole);
            self.remainder = units % UNITS_PER_STEP;
        }
        let lead = self.lead(speed);
        let cap = slowest.saturating_add(self.window(speed));
        let mut frontier = self.ideal.saturating_add(lead);
        if frontier > cap {
            frontier = cap;
            // The room waits for its slowest member. Hold the clock rather
            // than build a debt the room would rush through afterwards.
            self.ideal = self.ideal.min(cap.saturating_sub(lead));
            self.remainder = 0;
        }
        frontier.max(sealed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: Duration = Duration::from_millis(100);

    fn pacer() -> Pacer {
        // TPF2's pace: 5 steps per second at 1x.
        Pacer::new(5, Duration::from_millis(250), Duration::from_secs(2))
    }

    #[test]
    fn normal_speed_runs_at_steps_per_second_plus_lead() {
        let mut pacer = pacer();
        let mut sealed = 0;
        for _ in 0..10 {
            sealed = pacer.advance(TICK, Speed::NORMAL, Some(sealed), sealed);
        }
        // One second at 5 steps per second, plus 250 ms of lead (2 steps).
        assert_eq!(sealed, 5 + 2);
    }

    #[test]
    fn fractional_steps_accumulate_exactly() {
        let mut pacer = Pacer::new(3, Duration::from_millis(250), Duration::from_secs(2));
        let mut sealed = 0;
        for _ in 0..100 {
            sealed = pacer.advance(TICK, Speed::NORMAL, Some(sealed), sealed);
        }
        // Ten seconds at 3 steps per second, plus a one-step lead.
        assert_eq!(sealed, 30 + 1);
    }

    #[test]
    fn speed_scales_the_clock() {
        let mut pacer = pacer();
        let mut sealed = 0;
        for _ in 0..10 {
            sealed = pacer.advance(TICK, Speed(400), Some(sealed), sealed);
        }
        // 4x: 20 steps in one second, and the lead covers 250 ms at 4x.
        assert_eq!(sealed, 20 + 5);
    }

    #[test]
    fn pause_holds_the_frontier() {
        let mut pacer = pacer();
        let sealed = pacer.advance(Duration::from_secs(1), Speed::NORMAL, Some(0), 0);
        let paused = pacer.advance(Duration::from_secs(60), Speed::PAUSED, Some(sealed), sealed);
        assert_eq!(paused, sealed);
        // Resuming continues where the clock stood, without a burst.
        let resumed = pacer.advance(TICK, Speed::NORMAL, Some(sealed), sealed);
        assert!(resumed <= sealed + 1, "{resumed} after {sealed}");
    }

    #[test]
    fn a_slow_member_holds_the_room_without_building_debt() {
        let mut pacer = pacer();
        let window = pacer.window(Speed::NORMAL);
        let mut sealed = 0;
        // The slowest member stays at step 0 for a minute.
        for _ in 0..600 {
            sealed = pacer.advance(TICK, Speed::NORMAL, Some(0), sealed);
        }
        assert_eq!(sealed, window);
        // Once it catches up, the room resumes at normal pace instead of
        // rushing through the minute it waited.
        let next = pacer.advance(TICK, Speed::NORMAL, Some(sealed), sealed);
        assert!(next <= sealed + 1, "{next} after {sealed}");
    }

    #[test]
    fn nobody_pacing_holds_the_clock() {
        let mut pacer = pacer();
        assert_eq!(
            pacer.advance(Duration::from_secs(10), Speed::NORMAL, None, 0),
            0
        );
        // No debt accumulated while held.
        assert_eq!(pacer.advance(TICK, Speed::NORMAL, Some(0), 0), 2);
    }

    #[test]
    fn frontier_never_moves_backwards() {
        let mut pacer = pacer();
        let sealed = pacer.advance(Duration::from_secs(4), Speed(1600), Some(0), 0);
        // Dropping to 1x shrinks the lead; the frontier must hold, not retreat.
        assert_eq!(
            pacer.advance(TICK, Speed::NORMAL, Some(sealed), sealed),
            sealed
        );
    }
}
