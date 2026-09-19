//! The hook through which a room's canonical rules take part in sequencing.

use std::sync::Arc;

use tpf3mp_proto::{Event, Payload, PlayerId, RulesName, RulesOffer, Text};

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

/// Rules a host can pick for a room.
#[derive(Clone)]
pub struct RulesChoice {
    /// What the room log records, so a recovered room keeps its rules.
    pub name: RulesName,
    pub description: Text<200>,
    pub factory: RulesetFactory,
}

/// The rules a server offers its rooms, the default first.
#[derive(Clone)]
pub struct RulesMenu {
    choices: Vec<RulesChoice>,
}

/// The name of the game's own rules: everything any player does is ordered
/// as it is, and the game's economy runs as it does alone.
pub const NATIVE: &str = "native";

impl RulesMenu {
    /// Only the game's own rules, including its economy.
    pub fn native() -> Self {
        Self::single(RulesChoice {
            name: RulesName::new(NATIVE).expect("short name"),
            description: Text::new("The game's own rules and economy, as in single player.")
                .expect("short description"),
            factory: Arc::new(|| Box::new(AcceptAll)),
        })
    }

    /// Just `choice`.
    pub fn single(choice: RulesChoice) -> Self {
        Self {
            choices: vec![choice],
        }
    }

    /// Also offers `choice`, replacing any of the same name.
    #[must_use]
    pub fn with(mut self, choice: RulesChoice) -> Self {
        match self.choices.iter_mut().find(|c| c.name == choice.name) {
            Some(existing) => *existing = choice,
            None => self.choices.push(choice),
        }
        self
    }

    /// The choice named `name`, or the default without a name.
    pub fn find(&self, name: Option<&str>) -> Option<&RulesChoice> {
        match name {
            None => self.choices.first(),
            Some(name) => self.choices.iter().find(|c| c.name.as_str() == name),
        }
    }

    /// What a host picks from.
    pub fn offers(&self) -> Vec<RulesOffer> {
        self.choices
            .iter()
            .map(|c| RulesOffer {
                name: c.name.clone(),
                description: c.description.clone(),
            })
            .collect()
    }
}

impl Default for RulesMenu {
    fn default() -> Self {
        Self::native()
    }
}

impl std::fmt::Debug for RulesMenu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.choices.iter().map(|c| c.name.as_str()))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_the_first_and_names_pick_the_rest() {
        let menu = RulesMenu::native().with(RulesChoice {
            name: RulesName::new("strict").unwrap(),
            description: Text::new("Validated").unwrap(),
            factory: Arc::new(|| Box::new(AcceptAll)),
        });
        assert_eq!(menu.find(None).unwrap().name.as_str(), NATIVE);
        assert_eq!(menu.find(Some("strict")).unwrap().name.as_str(), "strict");
        assert!(menu.find(Some("other")).is_none());
        let offers = menu.offers();
        assert_eq!(offers.len(), 2);
        assert_eq!(offers[0].name.as_str(), NATIVE);
    }
}
