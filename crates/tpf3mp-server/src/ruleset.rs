//! The hook through which a room's canonical rules take part in sequencing.

use std::sync::Arc;

use tpf3mp_proto::{Event, Payload, PlayerId};

/// A room's canonical rules.
///
/// The sequencer calls [`validate`](Ruleset::validate) for every intent before
/// ordering it, and [`apply`](Ruleset::apply) for every ordered event, in log
/// order, immediately after ordering it. The ruleset's state therefore always
/// matches the event log, and a validation sees every earlier event.
pub trait Ruleset: Send + 'static {
    /// Accepts an intent, or refuses it with a ruleset-defined code that is
    /// sent back to the player.
    fn validate(&self, player: &PlayerId, payload: &Payload) -> Result<(), u16>;

    fn apply(&mut self, event: &Event);
}

/// Creates the ruleset of each new room.
pub type RulesetFactory = Arc<dyn Fn() -> Box<dyn Ruleset> + Send + Sync>;

/// Orders every intent without interpreting it.
#[derive(Debug, Default)]
pub struct AcceptAll;

impl Ruleset for AcceptAll {
    fn validate(&self, _player: &PlayerId, _payload: &Payload) -> Result<(), u16> {
        Ok(())
    }

    fn apply(&mut self, _event: &Event) {}
}
