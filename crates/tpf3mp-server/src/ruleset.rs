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

    /// The ruleset's whole state, for compacting a room's log: a fresh
    /// ruleset that [restores](Ruleset::restore) it and then applies the
    /// events after must end exactly where this one does. `None`, the
    /// default, means the ruleset cannot say, and its rooms keep their whole
    /// log.
    ///
    /// The state outlives the process: a server upgraded meanwhile restores
    /// it with newer rules. So the bytes should name their own format, and
    /// `restore` refuse a format it does not know; the room is then set
    /// aside, not restored wrong.
    fn save(&self) -> Option<Vec<u8>> {
        None
    }

    /// Takes on a state that [`save`](Ruleset::save) produced.
    fn restore(&mut self, state: &[u8]) -> Result<(), String> {
        let _ = state;
        Err("this ruleset cannot restore a saved state".into())
    }
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

    fn save(&self) -> Option<Vec<u8>> {
        Some(Vec::new())
    }

    fn restore(&mut self, state: &[u8]) -> Result<(), String> {
        if state.is_empty() {
            Ok(())
        } else {
            Err("accepting every intent keeps no state".into())
        }
    }
}
