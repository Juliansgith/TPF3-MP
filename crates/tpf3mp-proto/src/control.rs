//! Messages on the control stream. See `docs/PROTOCOL.md` for their
//! semantics. Variants are identified by position: append, never reorder.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    Platform, Text,
    bytes::{FixedBytes, Payload},
    ids::{Invite, PlayerId, RoomId, SessionId, Signature},
    snapshot::{SavedWorld, SnapshotId},
};

/// Domain separator for the identity proof in [`Hello`].
pub const AUTH_DOMAIN: &[u8] = b"tpf3mp-auth-v1";
/// TLS exporter label for the identity proof's keying material.
pub const AUTH_EXPORTER_LABEL: &[u8] = b"EXPORTER-tpf3mp-auth";

/// Largest number of members a room can have.
pub const MAX_ROOM_MEMBERS: u8 = 64;
/// Largest number of lanes in one [`GameMessage::Checkpoint`].
pub const MAX_CHECKPOINT_LANES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMessage {
    Hello(Hello),
    Request { id: u32, request: Request },
    Game(GameMessage),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMessage {
    Welcome(Welcome),
    Reject(Reject),
    Response {
        id: u32,
        result: Result<Response, RequestError>,
    },
    RoomUpdate(RoomView),
    IntentRejected {
        client_seq: u64,
        reason: IntentRejection,
    },
    /// This client's world differs from the room's verdict at a checkpoint,
    /// in these lanes.
    Diverged {
        step: u64,
        lanes: Vec<u16>,
    },
    /// The room's owner removed this player, who cannot come back to it.
    Kicked,
    /// Upload the world this client saved at the save event `event`: open a
    /// bulk stream and serve `snapshot` on it.
    Upload {
        event: u64,
        snapshot: SnapshotId,
    },
}

/// The client's first message after the preamble.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub client_version: Text<64>,
    pub platform: Platform,
    pub name: Text<32>,
    pub identity: PlayerId,
    /// Ed25519 signature over [`AUTH_DOMAIN`] followed by 32 bytes of TLS
    /// keying material exported under [`AUTH_EXPORTER_LABEL`].
    pub proof: Signature,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub server_version: Text<64>,
    pub session_id: SessionId,
}

/// The server's answer to a [`Hello`] it will not serve. The server closes
/// the connection after sending it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reject {
    pub reason: RejectReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    ServerFull,
    /// The identity proof did not verify.
    BadProof,
    /// The client's network address already holds its share of sessions.
    TooManyConnections,
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ServerFull => "the server is full; try again later",
            Self::BadProof => "the server could not verify this client's identity",
            Self::TooManyConnections => {
                "too many players are connected from this network; close another game first"
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    CreateRoom(CreateRoom),
    JoinRoom(JoinRoom),
    LeaveRoom,
    SetReady(bool),
    DeclareContent(ContentFingerprint),
    StartGame,
    SetSpeed(Speed),
    /// The owner removes a player from the room for good, for example one
    /// whose game froze.
    Kick(PlayerId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateRoom {
    pub name: Text<48>,
    pub max_players: u8,
    pub password: Option<Text<64>>,
    pub settings: RoomSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRoom {
    pub invite: Invite,
    pub password: Option<Text<64>>,
    /// For a running game: where this client continues, to receive only
    /// later turns. `None` means the client has no world of this game: it
    /// receives one to load (see `TurnStart::world`), or, from a server that
    /// keeps no snapshots, the game from its first turn.
    pub resume: Option<Resume>,
    /// The client's game build and mods, which must match the room's to
    /// join a running game as a new player. In the lobby, players declare
    /// theirs with [`Request::DeclareContent`] instead.
    pub content: Option<ContentFingerprint>,
}

/// Where a returning client continues a running game.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resume {
    /// The last turn this client applied.
    pub after_turn: u64,
    /// The history those turns belong to, from the `TurnStart` of the stream
    /// they came on. A room restored after a crash that lost turns starts a
    /// new history, and refuses to resume anyone past the point where the
    /// two differ.
    pub history: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    RoomCreated { invite: Invite, room: RoomView },
    RoomJoined(RoomView),
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestError {
    AlreadyInRoom,
    NotInRoom,
    /// The invite, its room or the password is wrong. The three are
    /// deliberately indistinguishable.
    BadInvite,
    RoomFull,
    NotOwner,
    NotAllReady,
    ContentMismatch,
    GameRunning,
    GameNotRunning,
    InvalidSettings,
    TooManyRooms,
    /// The requested resume point is no longer held by the server.
    ResumeUnavailable,
    /// The connection sent requests faster than the server allows.
    RateLimited,
    /// No such player is in the room.
    NoSuchPlayer,
    /// The owner cannot kick themselves; they can leave.
    CannotKickSelf,
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AlreadyInRoom => "already in a room",
            Self::NotInRoom => "not in a room",
            Self::BadInvite => "the invite or password is not valid",
            Self::RoomFull => "the room is full",
            Self::NotOwner => "only the room owner can do that",
            Self::NotAllReady => "not every player is ready",
            Self::ContentMismatch => "players have different game versions or mods",
            Self::GameRunning => "the game is already running",
            Self::GameNotRunning => "the game is not running",
            Self::InvalidSettings => "the room settings are out of range",
            Self::TooManyRooms => "the server cannot host more rooms",
            Self::ResumeUnavailable => "the game can no longer be resumed from that point",
            Self::RateLimited => "too many requests; try again in a moment",
            Self::NoSuchPlayer => "no such player is in the room",
            Self::CannotKickSelf => "the owner cannot kick themselves; leave the room instead",
        })
    }
}

/// Settings fixed when a room is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomSettings {
    /// Simulation steps per second at 1x speed. TPF2 runs 5; TPF3 is
    /// measured on release day.
    pub steps_per_second: u16,
    /// How far ahead of the room clock the server seals steps. Clients play
    /// behind a jitter buffer of their own, so this does not set the delay
    /// players feel (see "Playout" in `docs/PROTOCOL.md`).
    pub input_delay_ms: u16,
    /// Members report checkpoint digests at every step divisible by this.
    pub checkpoint_interval: u32,
}

impl RoomSettings {
    pub const DEFAULT: Self = Self {
        steps_per_second: 5,
        input_delay_ms: 250,
        checkpoint_interval: 50,
    };

    /// Whether every setting is within the range the server accepts.
    pub fn is_valid(&self) -> bool {
        (1..=240).contains(&self.steps_per_second)
            && (20..=2000).contains(&self.input_delay_ms)
            && (1..=1_000_000).contains(&self.checkpoint_interval)
    }
}

/// Digest of the game build and the ordered, content-hashed mod set. Players
/// must match exactly to start a game together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentFingerprint(pub FixedBytes<32>);

/// Session speed in percent of normal: `100` is 1x, `0` pauses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Speed(pub u16);

impl Speed {
    pub const PAUSED: Self = Self(0);
    pub const NORMAL: Self = Self(100);
    pub const MAX: Self = Self(1600);

    pub fn is_paused(self) -> bool {
        self.0 == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomView {
    pub id: RoomId,
    pub name: Text<48>,
    pub owner: PlayerId,
    pub max_players: u8,
    pub has_password: bool,
    pub phase: RoomPhase,
    pub settings: RoomSettings,
    pub members: Vec<MemberView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoomPhase {
    Lobby,
    Running,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberView {
    pub player: PlayerId,
    pub name: Text<32>,
    pub platform: Platform,
    pub ready: bool,
    pub content: Option<ContentFingerprint>,
    pub connected: bool,
}

/// A client's game traffic, carried on the control stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GameMessage {
    Intent {
        client_seq: u64,
        payload: Payload,
    },
    /// The last step this client has executed.
    Progress {
        step: u64,
    },
    Checkpoint {
        step: u64,
        lanes: Vec<LaneDigest>,
    },
    /// This client saved its world at the save event `event`, where its
    /// lanes were these. `world` is `None` if the save failed.
    Saved {
        event: u64,
        lanes: Vec<LaneDigest>,
        world: Option<SavedWorld>,
    },
}

/// The digest of one lane of world state at a checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneDigest {
    pub lane: u16,
    pub digest: FixedBytes<32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IntentRejection {
    GameNotRunning,
    RateLimited,
    /// The room's rules refused the intent; the code is ruleset-defined.
    Refused {
        code: u16,
    },
}
