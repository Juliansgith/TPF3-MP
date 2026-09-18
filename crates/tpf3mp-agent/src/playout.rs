//! When a game running at wall-clock pace plays each sealed step.
//!
//! The server seals steps at the room's pace, but its turns reach a client
//! with network jitter, and a turn in a lost packet arrives a retransmission
//! late. A game that played each step the moment it was sealed would stutter
//! with every late turn. Instead a client plays at a steady pace, as far
//! behind the frontier as the latest arrival it has seen recently, plus a
//! small margin. This is a jitter buffer: it grows on a poor link and shrinks
//! again once the link calms down.
//!
//! The buffer is the client's own. A client on a poor link plays further
//! behind the frontier and feels more delay on its own commands, but nobody
//! else waits for it.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use tpf3mp_proto::Speed;

/// How much faster than the room's pace a client plays to shrink its
/// buffer, in percent. The difference is invisible to a player.
const CONVERGE_PERCENT: u32 = 105;

/// A client further behind its schedule than this plays at once instead of
/// converging, for example after reconnecting.
const FAST_FORWARD: Duration = Duration::from_secs(1);

/// Schedules the steps of one turn stream.
#[derive(Debug, Clone)]
pub struct Playout {
    steps_per_second: u16,
    margin: Duration,
    memory: Duration,
    /// The pace steps play at: the room's speed, or the last one before a
    /// pause.
    pace: Speed,
    paused: bool,
    frontier: u64,
    /// The step the schedule counts from. It moves whenever the pace
    /// changes, so the schedule never mixes paces.
    base: Option<u64>,
    /// Recent arrivals: when each arrived, and when step `base` would have
    /// arrived had it come like this one. The implied times decrease from
    /// front to back, so the front is the latest arrival still remembered.
    recent: VecDeque<(Instant, Instant)>,
    /// The last step played, and when it was due.
    played: Option<(u64, Instant)>,
}

impl Playout {
    /// A schedule for a room running at `steps_per_second`. Steps play
    /// `margin` after the latest arrival seen within `memory`.
    pub fn new(steps_per_second: u16, margin: Duration, memory: Duration) -> Self {
        Self {
            steps_per_second: steps_per_second.max(1),
            margin,
            memory,
            pace: Speed::NORMAL,
            paused: false,
            frontier: 0,
            base: None,
            recent: VecDeque::new(),
            played: None,
        }
    }

    /// Records a turn accepted at `now`: its frontier and the room's speed.
    pub fn on_turn(&mut self, sealed_through: u64, speed: Speed, now: Instant) {
        if speed.is_paused() {
            // Steps sealed before the pause still play at the old pace; the
            // frontier stands still until the room resumes.
            self.paused = true;
        } else if self.paused || speed != self.pace {
            self.paused = false;
            self.pace = speed;
            self.rebase(now);
        }
        if sealed_through <= self.frontier {
            return;
        }
        self.frontier = sealed_through;
        let base = *self.base.get_or_insert(sealed_through);
        let implied = self.earlier(now, sealed_through.saturating_sub(base));
        self.remember(now, implied);
    }

    /// When to play `step`, the next step, or `None` while it is not sealed.
    pub fn due(&self, step: u64, now: Instant) -> Option<Instant> {
        if step > self.frontier {
            return None;
        }
        let Some(ideal) = self.ideal(step) else {
            // Nothing tells when this step arrived: play it at once.
            return Some(now);
        };
        let converging = ideal.checked_add(FAST_FORWARD).is_some_and(|by| by > now);
        Some(match self.played {
            Some((_, last)) if converging => ideal.max(last + self.span(1, CONVERGE_PERCENT)),
            _ => ideal,
        })
    }

    /// Records that `step` was played, having been due at `due`.
    pub fn played(&mut self, step: u64, due: Instant) {
        self.played = Some((step, due));
    }

    /// When the latest recent arrival, carried forward at pace, has `step`
    /// arrive, plus the margin.
    fn ideal(&self, step: u64) -> Option<Instant> {
        let base = self.base?;
        let (_, implied) = self.recent.front()?;
        let arrived = if step >= base {
            implied.checked_add(self.span(step - base, 100))?
        } else {
            self.earlier(*implied, base - step)
        };
        arrived.checked_add(self.margin)
    }

    /// Starts a new schedule at the current pace. Steps already sealed keep
    /// playing evenly from the last one played, so a speed change neither
    /// bursts through the buffer nor stops.
    fn rebase(&mut self, now: Instant) {
        self.recent.clear();
        let Some((played, last)) = self.played else {
            self.base = None;
            return;
        };
        let next = (last + self.span(1, 100)).max(now);
        self.base = Some(played + 1);
        self.remember(now, next.checked_sub(self.margin).unwrap_or(next));
    }

    fn remember(&mut self, now: Instant, implied: Instant) {
        while self.recent.back().is_some_and(|(_, at)| *at <= implied) {
            self.recent.pop_back();
        }
        self.recent.push_back((now, implied));
        // Forget old arrivals, but always keep the latest.
        while self.recent.len() > 1
            && self
                .recent
                .front()
                .is_some_and(|(arrived, _)| now.saturating_duration_since(*arrived) > self.memory)
        {
            self.recent.pop_front();
        }
    }

    /// The time `steps` take at the pace, sped up to `percent`.
    fn span(&self, steps: u64, percent: u32) -> Duration {
        let units = u128::from(self.steps_per_second)
            * u128::from(self.pace.0.max(1))
            * u128::from(percent);
        let micros = u128::from(steps) * 10_000_000_000 / units;
        Duration::from_micros(u64::try_from(micros).unwrap_or(u64::MAX))
    }

    /// `at`, moved back by `steps` at the pace.
    fn earlier(&self, at: Instant, steps: u64) -> Instant {
        at.checked_sub(self.span(steps, 100)).unwrap_or(at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARGIN: Duration = Duration::from_millis(20);
    const MEMORY: Duration = Duration::from_secs(10);

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    /// 10 steps per second: one step every 100 ms at 1x. The start is a
    /// little in the future, so schedules reaching back stay representable.
    fn playout() -> (Playout, Instant) {
        (
            Playout::new(10, MARGIN, MEMORY),
            Instant::now() + ms(60_000),
        )
    }

    #[test]
    fn steps_play_a_margin_after_they_arrive_at_an_even_pace() {
        let (mut playout, t0) = playout();
        assert_eq!(playout.due(1, t0), None, "nothing sealed yet");
        // The first turn seals three steps at once: the server's lead.
        playout.on_turn(3, Speed::NORMAL, t0);
        assert_eq!(playout.due(3, t0), Some(t0 + MARGIN));
        // Steps before it are reckoned to have arrived at pace, so the lead
        // does not add to the buffer.
        assert_eq!(playout.due(1, t0), Some(t0 + MARGIN - ms(200)));
        // Later turns seal one step per 100 ms.
        playout.on_turn(4, Speed::NORMAL, t0 + ms(100));
        assert_eq!(playout.due(4, t0), Some(t0 + ms(100) + MARGIN));
        assert_eq!(playout.due(5, t0), None);
    }

    #[test]
    fn a_late_turn_grows_the_buffer_until_the_link_calms() {
        let (mut playout, t0) = playout();
        playout.on_turn(1, Speed::NORMAL, t0);
        // Step 2 arrives 80 ms late; everything after plays 80 ms later.
        playout.on_turn(2, Speed::NORMAL, t0 + ms(180));
        playout.on_turn(3, Speed::NORMAL, t0 + ms(200));
        assert_eq!(playout.due(3, t0), Some(t0 + ms(280) + MARGIN));
        // Once the late arrival is forgotten, the schedule moves back, but
        // only at the converging pace.
        let later = t0 + ms(10_400);
        playout.on_turn(105, Speed::NORMAL, later);
        assert_eq!(playout.ideal(105), Some(later + MARGIN));
        playout.played(104, later);
        assert_eq!(
            playout.due(105, later),
            Some(later + playout.span(1, CONVERGE_PERCENT))
        );
    }

    #[test]
    fn a_client_far_behind_plays_at_once() {
        let (mut playout, t0) = playout();
        playout.on_turn(10, Speed::NORMAL, t0);
        playout.played(10, t0);
        // A burst after reconnecting: 50 steps sealed in one turn.
        let now = t0 + ms(5000);
        playout.on_turn(60, Speed::NORMAL, now);
        // Step 11 would have arrived 4.9 s ago, so it is due immediately.
        let due = playout.due(11, now).unwrap();
        assert!(due < now, "{:?} after now", due - now);
    }

    #[test]
    fn a_pause_plays_on_to_the_frontier_and_resuming_starts_afresh() {
        let (mut playout, t0) = playout();
        playout.on_turn(5, Speed::NORMAL, t0);
        playout.on_turn(5, Speed::PAUSED, t0 + ms(50));
        // Steps sealed before the pause keep their times.
        assert_eq!(playout.due(5, t0), Some(t0 + MARGIN));
        assert_eq!(playout.due(6, t0), None);
        playout.played(5, t0 + MARGIN);
        // A minute later the room resumes and seals step 6.
        let resumed = t0 + ms(60_000);
        playout.on_turn(5, Speed::NORMAL, resumed);
        playout.on_turn(6, Speed::NORMAL, resumed + ms(100));
        assert_eq!(playout.due(6, resumed), Some(resumed + ms(100) + MARGIN));
    }

    #[test]
    fn a_speed_change_keeps_buffered_steps_even() {
        let (mut playout, t0) = playout();
        playout.on_turn(10, Speed(400), t0);
        // At 4x, 40 steps per second: 25 ms per step.
        assert_eq!(playout.due(9, t0), Some(t0 + MARGIN - ms(25)));
        playout.played(6, t0);
        // Back to 1x with steps 7 to 10 buffered: they play 100 ms apart
        // from the last one played, not all at once.
        playout.on_turn(10, Speed::NORMAL, t0 + ms(10));
        assert_eq!(playout.due(7, t0 + ms(10)), Some(t0 + ms(100)));
        assert_eq!(playout.ideal(9), Some(t0 + ms(300)));
    }
}
