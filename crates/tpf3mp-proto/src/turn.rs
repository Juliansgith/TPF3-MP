//! Messages on the turn stream: a running room's ordered event log. See
//! `docs/PROTOCOL.md` for the invariants clients rely on. Variants are
//! identified by position: append, never reorder.

use serde::{Deserialize, Serialize};

use crate::{
    Text,
    bytes::Payload,
    control::Speed,
    ids::{PlayerId, RoomId},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnMessage {
    Start(TurnStart),
    Turn(Turn),
}

/// The first message on a turn stream: where the log continues and how the
/// room is paced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnStart {
    pub room: RoomId,
    /// Number of the next turn on this stream.
    pub next_turn: u64,
    /// Sequence number of the next event on this stream.
    pub next_event: u64,
    pub steps_per_second: u16,
    pub checkpoint_interval: u32,
    /// The history this stream's turns belong to, to name when resuming.
    pub history: u64,
}

/// One sealed turn. See the invariants in `docs/PROTOCOL.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    pub number: u64,
    /// Clients may execute every step up to and including this one.
    pub sealed_through: u64,
    pub speed: Speed,
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Room-global, gap-free sequence number.
    pub seq: u64,
    /// The event applies after step `step - 1` and before step `step`.
    pub step: u64,
    pub body: EventBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventBody {
    /// A player's accepted intent.
    Command {
        player: PlayerId,
        client_seq: u64,
        payload: Payload,
    },
    PlayerJoined {
        player: PlayerId,
        name: Text<32>,
    },
    PlayerLeft {
        player: PlayerId,
    },
}
