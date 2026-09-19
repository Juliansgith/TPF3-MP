//! The client half of the turn invariants in `docs/PROTOCOL.md`: checks every
//! turn the server sends and tells the game what it may do next.

use std::collections::VecDeque;

use thiserror::Error;
use tpf3mp_proto::{Event, EventBody, Speed, Turn, TurnStart};

/// Bytes of events a follower holds before it refuses more. An honest
/// server's backlog is far smaller (it keeps at most 64 MiB of turns for
/// resuming), so only a broken or hostile one gets here.
pub const MAX_QUEUED_BYTES: usize = 256 << 20;
/// What an event costs besides its payload, for the byte bound.
const EVENT_OVERHEAD: usize = 64;

/// A turn that breaks the protocol. The client must disconnect and resume
/// from its last good state rather than apply anything further.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FollowError {
    #[error("turn {got} arrived where turn {expected} was due")]
    TurnGap { expected: u64, got: u64 },
    #[error("event {got} arrived where event {expected} was due")]
    EventGap { expected: u64, got: u64 },
    #[error("event {seq} is for step {step}, but new events are for step {expected}")]
    EventStep { seq: u64, step: u64, expected: u64 },
    #[error("the frontier moved back from {from} to {to}")]
    FrontierRegressed { from: u64, to: u64 },
    #[error("the turn stream restarts at turn {got}, but turn {expected} is next")]
    RestartMismatch { expected: u64, got: u64 },
    #[error("a turn or event number is at the end of its range")]
    Overflow,
    #[error("more than {MAX_QUEUED_BYTES} bytes of events are waiting")]
    Backlog,
}

/// What the game should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Apply this event now, before the next step.
    Apply(Event),
    /// Execute this step.
    Execute(u64),
}

/// Follows a room's turn stream.
#[derive(Debug, Clone)]
pub struct TurnFollower {
    next_turn: u64,
    next_event: u64,
    sealed_through: u64,
    speed: Speed,
    executed: u64,
    queue: VecDeque<Event>,
    /// Bytes of the events in `queue`, counted as [`event_size`].
    queued_bytes: usize,
}

impl TurnFollower {
    /// Starts following a new game, with the world loaded at step 0.
    pub fn new(start: &TurnStart) -> Self {
        Self {
            next_turn: start.next_turn,
            next_event: start.next_event,
            sealed_through: 0,
            speed: Speed::NORMAL,
            executed: 0,
            queue: VecDeque::new(),
            queued_bytes: 0,
        }
    }

    /// Continues on a new turn stream after reconnecting. The stream must
    /// pick up exactly where this follower left off.
    pub fn restart(&mut self, start: &TurnStart) -> Result<(), FollowError> {
        if start.next_turn != self.next_turn {
            return Err(FollowError::RestartMismatch {
                expected: self.next_turn,
                got: start.next_turn,
            });
        }
        if start.next_event != self.next_event {
            return Err(FollowError::EventGap {
                expected: self.next_event,
                got: start.next_event,
            });
        }
        Ok(())
    }

    /// Checks a turn against the invariants and queues its events. On error
    /// nothing of the turn is kept.
    ///
    /// Every event must be for the step after the previous turn's frontier:
    /// that is the only step the server gives new events, and it keeps the
    /// queue in step order, so no event can be stranded behind another.
    pub fn accept(&mut self, turn: Turn) -> Result<(), FollowError> {
        if turn.number != self.next_turn {
            return Err(FollowError::TurnGap {
                expected: self.next_turn,
                got: turn.number,
            });
        }
        if turn.sealed_through < self.sealed_through {
            return Err(FollowError::FrontierRegressed {
                from: self.sealed_through,
                to: turn.sealed_through,
            });
        }
        let next_turn = self.next_turn.checked_add(1).ok_or(FollowError::Overflow)?;
        let step = self
            .sealed_through
            .checked_add(1)
            .ok_or(FollowError::Overflow)?;
        let mut expected = self.next_event;
        let mut bytes = 0;
        for event in &turn.events {
            if event.seq != expected {
                return Err(FollowError::EventGap {
                    expected,
                    got: event.seq,
                });
            }
            if event.step != step {
                return Err(FollowError::EventStep {
                    seq: event.seq,
                    step: event.step,
                    expected: step,
                });
            }
            expected = expected.checked_add(1).ok_or(FollowError::Overflow)?;
            bytes += event_size(event);
        }
        if self.queued_bytes + bytes > MAX_QUEUED_BYTES {
            return Err(FollowError::Backlog);
        }
        self.next_turn = next_turn;
        self.next_event = expected;
        self.sealed_through = turn.sealed_through;
        self.speed = turn.speed;
        self.queued_bytes += bytes;
        self.queue.extend(turn.events);
        Ok(())
    }

    /// The next thing the game may do, or `None` until more turns arrive.
    /// Events apply before their step; a step executes only once sealed.
    pub fn next_action(&mut self) -> Option<Action> {
        let step = self.executed + 1;
        if self.queue.front().is_some_and(|event| event.step == step) {
            let event = self.queue.pop_front()?;
            self.queued_bytes = self.queued_bytes.saturating_sub(event_size(&event));
            return Some(Action::Apply(event));
        }
        if step <= self.sealed_through {
            self.executed = step;
            return Some(Action::Execute(step));
        }
        None
    }

    /// The step [`next_action`](Self::next_action) would execute next, if
    /// the next action is executing a step. A game running at wall-clock
    /// pace checks this to wait until the step is due.
    pub fn next_step(&self) -> Option<u64> {
        let step = self.executed + 1;
        let event_first = self.queue.front().is_some_and(|event| event.step == step);
        (!event_first && step <= self.sealed_through).then_some(step)
    }

    /// The last step handed out for execution.
    pub fn executed(&self) -> u64 {
        self.executed
    }

    pub fn sealed_through(&self) -> u64 {
        self.sealed_through
    }

    pub fn speed(&self) -> Speed {
        self.speed
    }

    /// The last turn accepted, for resuming after a reconnect.
    pub fn last_turn(&self) -> Option<u64> {
        self.next_turn.checked_sub(1).filter(|turn| *turn > 0)
    }
}

/// What a turn weighs in bytes, for bounding what a client queues.
pub(crate) fn turn_weight(turn: &Turn) -> usize {
    EVENT_OVERHEAD + turn.events.iter().map(event_size).sum::<usize>()
}

fn event_size(event: &Event) -> usize {
    EVENT_OVERHEAD
        + match &event.body {
            EventBody::Command { payload, .. } => payload.len(),
            _ => 0,
        }
}

#[cfg(test)]
mod tests {
    use tpf3mp_proto::{EventBody, FixedBytes, PlayerId, RoomId};

    use super::*;

    fn start() -> TurnStart {
        TurnStart {
            room: RoomId(FixedBytes([0; 16])),
            next_turn: 1,
            next_event: 1,
            steps_per_second: 5,
            checkpoint_interval: 10,
        }
    }

    fn event(seq: u64, step: u64) -> Event {
        Event {
            seq,
            step,
            body: EventBody::PlayerLeft {
                player: PlayerId(FixedBytes([seq as u8; 32])),
            },
        }
    }

    fn turn(number: u64, sealed_through: u64, events: Vec<Event>) -> Turn {
        Turn {
            number,
            sealed_through,
            speed: Speed::NORMAL,
            events,
        }
    }

    fn drain(follower: &mut TurnFollower) -> Vec<Action> {
        std::iter::from_fn(|| follower.next_action()).collect()
    }

    #[test]
    fn events_apply_before_their_step_and_steps_wait_for_the_seal() {
        let mut follower = TurnFollower::new(&start());
        follower
            .accept(turn(1, 2, vec![event(1, 1), event(2, 1)]))
            .unwrap();
        assert_eq!(
            drain(&mut follower),
            vec![
                Action::Apply(event(1, 1)),
                Action::Apply(event(2, 1)),
                Action::Execute(1),
                Action::Execute(2),
            ]
        );
        // Step 3 is not sealed yet, but its event may apply already: this is
        // how building works while the game is paused.
        follower.accept(turn(2, 2, vec![event(3, 3)])).unwrap();
        assert_eq!(drain(&mut follower), vec![Action::Apply(event(3, 3))]);
        follower.accept(turn(3, 4, vec![])).unwrap();
        assert_eq!(
            drain(&mut follower),
            vec![Action::Execute(3), Action::Execute(4)]
        );
        assert_eq!(follower.last_turn(), Some(3));
    }

    #[test]
    fn next_step_names_the_step_only_when_executing_comes_next() {
        let mut follower = TurnFollower::new(&start());
        assert_eq!(follower.next_step(), None, "nothing sealed");
        follower.accept(turn(1, 2, vec![event(1, 1)])).unwrap();
        assert_eq!(follower.next_step(), None, "an event applies first");
        assert_eq!(follower.next_action(), Some(Action::Apply(event(1, 1))));
        assert_eq!(follower.next_step(), Some(1));
        assert_eq!(follower.next_action(), Some(Action::Execute(1)));
        assert_eq!(follower.next_step(), Some(2));
        follower.next_action();
        assert_eq!(follower.next_step(), None, "step 3 is not sealed");
    }

    #[test]
    fn gaps_and_rewrites_are_refused_without_side_effects() {
        let mut follower = TurnFollower::new(&start());
        follower.accept(turn(1, 5, vec![event(1, 1)])).unwrap();
        assert_eq!(
            follower.accept(turn(3, 6, vec![])),
            Err(FollowError::TurnGap {
                expected: 2,
                got: 3
            })
        );
        assert_eq!(
            follower.accept(turn(2, 6, vec![event(3, 6)])),
            Err(FollowError::EventGap {
                expected: 2,
                got: 3
            })
        );
        assert_eq!(
            follower.accept(turn(2, 6, vec![event(2, 5)])),
            Err(FollowError::EventStep {
                seq: 2,
                step: 5,
                expected: 6
            })
        );
        assert_eq!(
            follower.accept(turn(2, 6, vec![event(2, 7)])),
            Err(FollowError::EventStep {
                seq: 2,
                step: 7,
                expected: 6
            }),
            "events for a later step are refused too"
        );
        assert_eq!(
            follower.accept(turn(2, 4, vec![])),
            Err(FollowError::FrontierRegressed { from: 5, to: 4 })
        );
        // None of the refused turns changed anything.
        follower.accept(turn(2, 6, vec![event(2, 6)])).unwrap();
        assert_eq!(follower.sealed_through(), 6);
    }

    #[test]
    fn restart_must_continue_exactly() {
        let mut follower = TurnFollower::new(&start());
        follower.accept(turn(1, 3, vec![event(1, 1)])).unwrap();
        let mut resumed = start();
        resumed.next_turn = 2;
        resumed.next_event = 2;
        assert_eq!(follower.restart(&resumed), Ok(()));
        resumed.next_turn = 1;
        assert!(follower.restart(&resumed).is_err());
    }
}
