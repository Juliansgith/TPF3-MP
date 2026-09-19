//! What the agent and the in-game hook say to each other over the
//! shared-memory link (`tpf3mp-ipc`), and the step gate that keeps the
//! game's simulation in step with the room.
//!
//! The agent runs the network side: it follows the room's turn stream and
//! decides when each step may run (`tpf3mp_agent::Playout`). The hook runs
//! inside the game and does as little as it can. It applies events and runs
//! steps when the [`Gate`] allows, and reports what the player does and how
//! far the game has got.
//!
//! # Ordering
//!
//! Each direction is an ordered stream of messages. The agent sends every
//! event for step `s` after releasing step `s - 1` and before releasing step
//! `s`, and never releases several steps in one message across an event.
//! The hook reads messages only while its game waits before a step, and
//! stops reading once that step is released. Each event is therefore
//! applied exactly between the two steps the room ordered it for, and the
//! [`Gate`] refuses anything that breaks this.
//!
//! This crate has no async runtime and no network code, because the hook
//! links it into the game. [`Session`] is the hook's whole side of the
//! link; the game-specific part of the hook only implements [`Game`].

mod gate;
mod session;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tpf3mp_proto::{ChatText, Event, IntentRejection, LaneDigest, Payload, Speed, Text};

pub use gate::{Gate, GateError, Gated};
pub use session::{Begin, Game, Load, Notice, Session, SessionError, StepGate};

/// Version of these messages. Both sides send it first and refuse a peer
/// that speaks another.
pub const BRIDGE_VERSION: u32 = 2;
/// The link name the agent creates and the hook opens, unless told
/// otherwise.
pub const DEFAULT_LINK: &str = "tpf3mp.default";
/// Largest encoded message, within the link's default frame limit
/// (`tpf3mp_ipc::DEFAULT_MAX_MESSAGE`). An event with the largest intent
/// payload fits.
pub const MAX_MESSAGE: usize = 60 * 1024;
/// Longest file path the link carries, in UTF-8 bytes.
pub const MAX_PATH: usize = 1024;

/// From the agent to the hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToHook {
    /// The first message after the link opens.
    Hello { version: u32 },
    /// A game begins. A [`ToHook::Load`] follows. The hook writes the saves
    /// the room asks for into `saves`, a directory the agent made for this
    /// game.
    Begin {
        steps_per_second: u16,
        checkpoint_interval: u32,
        saves: Text<MAX_PATH>,
    },
    /// Apply this event before running step `event.step`.
    Apply(Event),
    /// Steps up to and including `through` may run.
    Release { through: u64 },
    /// The room's speed, for the game's display. Pacing comes from
    /// [`ToHook::Release`] alone.
    Speed(Speed),
    /// At the checkpoint at `step`, this replica's `lanes` differed from the
    /// room's verdict.
    Diverged { step: u64, lanes: Vec<u16> },
    /// The room refused one of the player's commands, which then never
    /// happens. `command` counts the player's [`ToAgent::Command`]s from 0.
    Refused {
        command: u64,
        reason: IntentRejection,
    },
    /// The game session is over; the game stops waiting at the gate.
    End { reason: Text<128> },
    /// Load a world, then answer [`ToAgent::Loaded`] with `next_step`, the
    /// first step that world runs. Everything sent before this is void.
    ///
    /// The first load of a game may name no file: the game then loads the
    /// world every player starts from. Otherwise `file` is a save the room
    /// agreed on, for a player joining a running game, one who could no
    /// longer resume, or one whose world diverged.
    Load {
        file: Option<Text<MAX_PATH>>,
        next_step: u64,
    },
    /// A member of the room said something; `from` is their name.
    Chat { from: Text<32>, text: ChatText },
}

/// From the hook to the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToAgent {
    /// The first message after the link opens.
    Hello { version: u32, build: Text<64> },
    /// The world is loaded, and `next_step` is the first step it will run.
    Loaded { next_step: u64 },
    /// The local player did something: have the room order it.
    Command { payload: Payload },
    /// The game ran this step.
    Ran { step: u64 },
    /// The world's digests at a checkpoint step, taken after running it.
    Checkpoint { step: u64, lanes: Vec<LaneDigest> },
    /// A line for the agent's log.
    Log { message: Text<256> },
    /// The world was saved at the save event `event`, into `file`, or
    /// `None` if saving failed. `lanes` are its digests there.
    Saved {
        event: u64,
        lanes: Vec<LaneDigest>,
        file: Option<Text<MAX_PATH>>,
    },
    /// The player says something to the room.
    Chat { text: ChatText },
}

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("the message is {0} bytes, over the {MAX_MESSAGE}-byte limit")]
    TooLarge(usize),
    #[error("the message does not decode: {0}")]
    Malformed(#[from] postcard::Error),
    #[error("the peer speaks bridge version {0}, not {BRIDGE_VERSION}")]
    Version(u32),
}

/// Encodes a message for the link.
pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>, BridgeError> {
    let bytes = postcard::to_stdvec(message)?;
    if bytes.len() > MAX_MESSAGE {
        return Err(BridgeError::TooLarge(bytes.len()));
    }
    Ok(bytes)
}

/// Decodes a message from the link. Trailing bytes are refused.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, BridgeError> {
    if bytes.len() > MAX_MESSAGE {
        return Err(BridgeError::TooLarge(bytes.len()));
    }
    let (message, rest) = postcard::take_from_bytes(bytes)?;
    if !rest.is_empty() {
        return Err(BridgeError::Malformed(
            postcard::Error::DeserializeBadEncoding,
        ));
    }
    Ok(message)
}

/// Checks a peer's hello.
pub fn check_version(version: u32) -> Result<(), BridgeError> {
    if version == BRIDGE_VERSION {
        Ok(())
    } else {
        Err(BridgeError::Version(version))
    }
}

#[cfg(test)]
mod tests {
    use tpf3mp_proto::{EventBody, FixedBytes, MAX_PAYLOAD, PlayerId};

    use super::*;

    #[test]
    fn messages_round_trip() {
        let to_hook = ToHook::Apply(Event {
            seq: 7,
            step: 3,
            body: EventBody::Command {
                player: PlayerId(FixedBytes([1; 32])),
                client_seq: 9,
                payload: Payload::new(vec![4, 5, 6]).unwrap(),
            },
        });
        assert_eq!(
            decode::<ToHook>(&encode(&to_hook).unwrap()).unwrap(),
            to_hook
        );
        let to_agent = ToAgent::Checkpoint {
            step: 50,
            lanes: vec![LaneDigest {
                lane: 2,
                digest: FixedBytes([8; 32]),
            }],
        };
        assert_eq!(
            decode::<ToAgent>(&encode(&to_agent).unwrap()).unwrap(),
            to_agent
        );
    }

    #[test]
    fn the_largest_intent_fits_in_one_message() {
        let apply = ToHook::Apply(Event {
            seq: u64::MAX,
            step: u64::MAX,
            body: EventBody::Command {
                player: PlayerId(FixedBytes([0xff; 32])),
                client_seq: u64::MAX,
                payload: Payload::new(vec![0xab; MAX_PAYLOAD]).unwrap(),
            },
        });
        assert!(encode(&apply).is_ok());
    }

    #[test]
    fn malformed_and_oversized_messages_are_refused() {
        let mut bytes = encode(&ToAgent::Ran { step: 5 }).unwrap();
        bytes.push(0);
        assert!(matches!(
            decode::<ToAgent>(&bytes),
            Err(BridgeError::Malformed(_))
        ));
        assert!(matches!(
            decode::<ToAgent>(&[0xff; 3]),
            Err(BridgeError::Malformed(_))
        ));
        assert!(matches!(
            decode::<ToAgent>(&vec![0; MAX_MESSAGE + 1]),
            Err(BridgeError::TooLarge(_))
        ));
    }

    #[test]
    fn only_the_same_version_is_accepted() {
        assert!(check_version(BRIDGE_VERSION).is_ok());
        assert!(check_version(BRIDGE_VERSION + 1).is_err());
    }
}
