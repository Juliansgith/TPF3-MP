//! The hook's side of the step gate.

use thiserror::Error;
use tpf3mp_proto::{Event, IntentRejection, Speed, Text};

use crate::ToHook;

/// Stands before every step the game runs. The game may run its next step
/// once the agent has released it; until then the hook reads messages and
/// the gate hands over the events to apply first.
///
/// Anything out of order is an error. The hook must then stop following the
/// room and say so, rather than guess.
///
/// ```text
/// before each step:
///     while !gate.may_run() {
///         match gate.on_message(next message)? {
///             Gated::Apply(event) => apply it to the world,
///             ...
///         }
///     }
///     run the step, then gate.ran() and report ToAgent::Ran
/// ```
#[derive(Debug, Clone)]
pub struct Gate {
    next_step: u64,
    released: u64,
    ended: bool,
}

/// What the game should do with a message read at the gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gated {
    /// Apply this event now, before the next step.
    Apply(Event),
    /// Show the room's new speed.
    Speed(Speed),
    /// Tell the player this replica diverged.
    Diverged { step: u64, lanes: Vec<u16> },
    /// Tell the player one of their commands was refused.
    Refused {
        command: u64,
        reason: IntentRejection,
    },
    /// The session is over; stop waiting at the gate.
    Ended(Text<128>),
    /// Nothing to do but check [`Gate::may_run`] again.
    Nothing,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GateError {
    #[error("an event for step {got} arrived while the game waits before step {expected}")]
    EventStep { expected: u64, got: u64 },
    #[error("an event arrived after step {step} was already released")]
    EventAfterRelease { step: u64 },
    #[error("the release went back from step {from} to {to}")]
    ReleaseBackwards { from: u64, to: u64 },
    #[error("{0} has no place at the gate")]
    Unexpected(&'static str),
    #[error("the step counter is at the end of its range")]
    Overflow,
}

impl Gate {
    /// A gate before `next_step`, the first step the game will run: 1 for
    /// a new game.
    pub fn new(next_step: u64) -> Self {
        Self {
            next_step,
            released: next_step.saturating_sub(1),
            ended: false,
        }
    }

    /// The step the game runs next.
    pub fn next_step(&self) -> u64 {
        self.next_step
    }

    /// Whether the game may run its next step now.
    pub fn may_run(&self) -> bool {
        !self.ended && self.released >= self.next_step
    }

    /// Whether the session has ended.
    pub fn ended(&self) -> bool {
        self.ended
    }

    /// Handles one message read while the game waits before its next step.
    pub fn on_message(&mut self, message: ToHook) -> Result<Gated, GateError> {
        match message {
            ToHook::Apply(event) => {
                if event.step != self.next_step {
                    return Err(GateError::EventStep {
                        expected: self.next_step,
                        got: event.step,
                    });
                }
                if self.released >= self.next_step {
                    return Err(GateError::EventAfterRelease { step: event.step });
                }
                Ok(Gated::Apply(event))
            }
            ToHook::Release { through } => {
                if through < self.released {
                    return Err(GateError::ReleaseBackwards {
                        from: self.released,
                        to: through,
                    });
                }
                self.released = through;
                Ok(Gated::Nothing)
            }
            ToHook::Speed(speed) => Ok(Gated::Speed(speed)),
            ToHook::Diverged { step, lanes } => Ok(Gated::Diverged { step, lanes }),
            ToHook::Refused { command, reason } => Ok(Gated::Refused { command, reason }),
            ToHook::End { reason } => {
                self.ended = true;
                Ok(Gated::Ended(reason))
            }
            ToHook::Hello { .. } => Err(GateError::Unexpected("a hello")),
            ToHook::Begin { .. } => Err(GateError::Unexpected("the start of a game")),
        }
    }

    /// Records that the game ran its next step, and returns that step.
    pub fn ran(&mut self) -> Result<u64, GateError> {
        let step = self.next_step;
        self.next_step = step.checked_add(1).ok_or(GateError::Overflow)?;
        Ok(step)
    }
}

#[cfg(test)]
mod tests {
    use tpf3mp_proto::{EventBody, FixedBytes, PlayerId};

    use super::*;

    fn event(seq: u64, step: u64) -> Event {
        Event {
            seq,
            step,
            body: EventBody::PlayerLeft {
                player: PlayerId(FixedBytes([1; 32])),
            },
        }
    }

    #[test]
    fn events_apply_between_the_right_steps() {
        let mut gate = Gate::new(1);
        assert!(!gate.may_run(), "nothing released yet");
        assert_eq!(
            gate.on_message(ToHook::Apply(event(1, 1))),
            Ok(Gated::Apply(event(1, 1)))
        );
        gate.on_message(ToHook::Release { through: 3 }).unwrap();
        assert!(gate.may_run());
        assert_eq!(gate.ran(), Ok(1));
        assert_eq!(gate.ran(), Ok(2));
        assert_eq!(gate.ran(), Ok(3));
        assert!(!gate.may_run(), "step 4 is not released");
        gate.on_message(ToHook::Apply(event(2, 4))).unwrap();
        gate.on_message(ToHook::Release { through: 4 }).unwrap();
        assert_eq!(gate.ran(), Ok(4));
    }

    #[test]
    fn an_event_for_another_step_is_refused() {
        let mut gate = Gate::new(5);
        assert_eq!(
            gate.on_message(ToHook::Apply(event(1, 6))),
            Err(GateError::EventStep {
                expected: 5,
                got: 6
            })
        );
        assert_eq!(
            gate.on_message(ToHook::Apply(event(1, 4))),
            Err(GateError::EventStep {
                expected: 5,
                got: 4
            })
        );
    }

    #[test]
    fn an_event_for_a_released_step_is_refused() {
        let mut gate = Gate::new(1);
        gate.on_message(ToHook::Release { through: 1 }).unwrap();
        assert_eq!(
            gate.on_message(ToHook::Apply(event(1, 1))),
            Err(GateError::EventAfterRelease { step: 1 })
        );
    }

    #[test]
    fn releases_never_go_back() {
        let mut gate = Gate::new(1);
        gate.on_message(ToHook::Release { through: 5 }).unwrap();
        assert_eq!(
            gate.on_message(ToHook::Release { through: 4 }),
            Err(GateError::ReleaseBackwards { from: 5, to: 4 })
        );
        assert_eq!(
            gate.on_message(ToHook::Release { through: 5 }),
            Ok(Gated::Nothing),
            "repeating a release is harmless"
        );
    }

    #[test]
    fn the_end_stops_the_game_waiting() {
        let mut gate = Gate::new(1);
        gate.on_message(ToHook::Release { through: 9 }).unwrap();
        let reason = Text::new("the room closed").unwrap();
        assert_eq!(
            gate.on_message(ToHook::End {
                reason: reason.clone()
            }),
            Ok(Gated::Ended(reason))
        );
        assert!(gate.ended());
        assert!(!gate.may_run());
    }

    #[test]
    fn a_resumed_world_starts_where_it_stands() {
        let mut gate = Gate::new(101);
        assert!(!gate.may_run());
        gate.on_message(ToHook::Release { through: 101 }).unwrap();
        assert_eq!(gate.ran(), Ok(101));
    }
}
