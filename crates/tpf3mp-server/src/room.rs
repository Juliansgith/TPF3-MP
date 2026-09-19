//! A room: its lobby, its members and, once running, its sequencer. Each room
//! is one task that owns all of its state; connections talk to it through
//! [`RoomHandle`]. The turn invariants it upholds are in `docs/PROTOCOL.md`.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use ring::hmac;
use thiserror::Error;
use tokio::{
    sync::{mpsc, oneshot},
    time::MissedTickBehavior,
};
use tpf3mp_net::{bulk, close};
use tpf3mp_proto::{
    ChatText, ContentFingerprint, Event, EventBody, FRAME_HEADER_LEN, FixedBytes, IntentRejection,
    LaneDigest, MemberView, Payload, Platform, PlayerId, RequestError, Resume, RoomId, RoomPhase,
    RoomSettings, RoomView, RulesName, SavedWorld, ServerMessage, SnapshotId, Speed,
    TURN_MAX_FRAME, Text, Turn, TurnMessage, TurnStart, WorldOffer, decode_frame, encode_frame,
};
use tpf3mp_snapshot::{Manifest, ManifestId};
use tracing::{debug, error, info, warn};

use crate::{
    admission::RoomShare,
    directory::Directory,
    limit::TokenBucket,
    metrics::{self, Metrics},
    pacing::Pacer,
    persist::{self, Base, LogError, RoomLog, StartMember, StartRecord},
    ruleset::{RulesMenu, Ruleset},
    snapshots::{
        self, Agreed, Candidates, Pointer, SAVE_DEADLINE, SavePoint, SaveReport, SaveRound, Saves,
        Snapshots, UPLOAD_START, Upload,
    },
    verdict::{self, Report, Verdict},
};

/// Commands a room queues before senders wait.
pub(crate) const ROOM_QUEUE: usize = 1024;
/// Payload bytes one turn may carry. Far below the frame cap, so a turn with
/// the per-event overhead always encodes.
const TURN_PAYLOAD_BUDGET: usize = TURN_MAX_FRAME / 2;
/// How far past the slowest member the frontier may run, beyond the lead.
const MAX_AHEAD: Duration = Duration::from_secs(2);
/// Intents per second a player may send, and the burst allowed on top.
const INTENTS_PER_SECOND: u32 = 20;
const INTENT_BURST: u32 = 40;
/// Chat messages a player may send per second, and the burst on top. Every
/// message goes to every member.
const CHATS_PER_SECOND: u32 = 1;
const CHAT_BURST: u32 = 5;
/// Intent payload bytes per second a player may send, and the burst allowed
/// on top. A build command is a few hundred bytes; this leaves room for
/// large ones without letting one player grow a room's log quickly.
const PAYLOAD_BYTES_PER_SECOND: u32 = 32 * 1024;
const PAYLOAD_BURST: u32 = 256 * 1024;
/// A checkpoint round waits this long for every pacing member before
/// deciding with the reports it has.
const CHECKPOINT_DEADLINE: Duration = Duration::from_secs(30);
/// Decided rounds kept to judge members who report late.
const DECIDED_ROUNDS_KEPT: usize = 64;
/// Reports a round needs for a verdict. One report compares against
/// nothing, and would only judge later reporters by an unchecked claim.
const MIN_VERDICT_REPORTS: usize = 2;
/// Undecided rounds at once; no client can make the server hold more.
const MAX_OPEN_ROUNDS: usize = 64;
/// Turns kept in memory for resuming: an hour at the default tick, and at
/// most this many bytes. A player away longer needs a world snapshot
/// instead.
const RESUME_WINDOW: usize = 36_000;
const RESUME_WINDOW_BYTES: usize = 64 << 20;
/// No honest log seals further than this: centuries of play at the fastest
/// settings. Recovery refuses logs that do, so arithmetic on steps never
/// overflows.
const MAX_FRONTIER: u64 = 1 << 48;
/// No honest game gets this many turns or events; a compacted log's base
/// claiming more is refused, so counting on never overflows.
const MAX_COUNT: u64 = 1 << 56;
/// The least time between two rebases of one member. A replica that keeps
/// diverging is told so every time, but reloading it more often would only
/// keep its player out of the game.
const REBASE_GAP: Duration = Duration::from_secs(300);
/// Players a room keeps barred. The owner kicking throwaway identities in a
/// loop cannot grow it further.
const MAX_BANNED: usize = 1024;

/// The channels to one connection of a member.
#[derive(Clone)]
pub(crate) struct MemberLink {
    /// Distinguishes this connection from an earlier or later one of the
    /// same player.
    pub(crate) id: u64,
    pub(crate) control: mpsc::Sender<ServerMessage>,
    pub(crate) turns: mpsc::Sender<TurnFeed>,
    pub(crate) connection: quinn::Connection,
}

/// What a connection's turn-stream writer receives.
pub(crate) enum TurnFeed {
    /// Open a new turn stream: the start message, then turns already sealed.
    Open {
        start: TurnStart,
        backlog: Vec<Arc<[u8]>>,
    },
    /// One encoded turn frame.
    Frame(Arc<[u8]>),
    /// Finish the turn stream.
    Close,
}

pub(crate) struct NewMember {
    pub(crate) player: PlayerId,
    pub(crate) name: Text<32>,
    pub(crate) platform: Platform,
    pub(crate) link: MemberLink,
}

pub(crate) type Reply<T = ()> = oneshot::Sender<Result<T, RequestError>>;

pub(crate) enum RoomCommand {
    Join {
        member: NewMember,
        token: FixedBytes<32>,
        password: Option<Text<64>>,
        resume: Option<Resume>,
        content: Option<ContentFingerprint>,
        reply: Reply<RoomView>,
    },
    Leave {
        player: PlayerId,
        reply: Reply,
    },
    Disconnected {
        player: PlayerId,
        link: u64,
    },
    SetReady {
        player: PlayerId,
        ready: bool,
        reply: Reply,
    },
    DeclareContent {
        player: PlayerId,
        content: ContentFingerprint,
        reply: Reply,
    },
    Start {
        player: PlayerId,
        reply: Reply,
    },
    SetSpeed {
        player: PlayerId,
        speed: Speed,
        reply: Reply,
    },
    Kick {
        player: PlayerId,
        target: PlayerId,
        reply: Reply,
    },
    /// Whether the player is still a member; `NotInRoom` if not, for
    /// example after a kick.
    IsMember {
        player: PlayerId,
        reply: Reply,
    },
    Chat {
        player: PlayerId,
        text: ChatText,
        reply: Reply,
    },
    Intent {
        player: PlayerId,
        client_seq: u64,
        payload: Payload,
    },
    Progress {
        player: PlayerId,
        link: u64,
        step: u64,
    },
    Checkpoint {
        player: PlayerId,
        link: u64,
        step: u64,
        lanes: Vec<LaneDigest>,
    },
    Saved {
        player: PlayerId,
        link: u64,
        event: u64,
        lanes: Vec<LaneDigest>,
        world: Option<SavedWorld>,
    },
    /// A member's connection wants to fetch a snapshot. The room answers
    /// with its manifest only if it offered that snapshot to that
    /// connection.
    Fetch {
        player: PlayerId,
        link: u64,
        snapshot: SnapshotId,
        reply: oneshot::Sender<Option<Arc<Manifest>>>,
    },
    /// A member's connection starts to upload a save. The room answers
    /// whether it asked that member for that save.
    Upload {
        player: PlayerId,
        link: u64,
        snapshot: SnapshotId,
        reply: oneshot::Sender<bool>,
    },
    /// An upload ended: the save is in the store, verified and retained, or
    /// it failed.
    Uploaded {
        player: PlayerId,
        snapshot: SnapshotId,
        result: Result<Arc<Manifest>, String>,
    },
}

/// A connection's way to reach a room.
#[derive(Clone)]
pub(crate) struct RoomHandle {
    commands: mpsc::Sender<RoomCommand>,
}

impl RoomHandle {
    pub(crate) fn new(commands: mpsc::Sender<RoomCommand>) -> Self {
        Self { commands }
    }

    /// Sends a request and waits for the room's answer. A room that has
    /// closed answers `NotInRoom`.
    pub(crate) async fn request<T>(
        &self,
        make: impl FnOnce(Reply<T>) -> RoomCommand,
    ) -> Result<T, RequestError> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(make(reply))
            .await
            .map_err(|_| RequestError::NotInRoom)?;
        answer.await.map_err(|_| RequestError::NotInRoom)?
    }

    /// Queues a command without waiting. Returns false when the room is gone
    /// or its queue is full.
    pub(crate) fn notify(&self, command: RoomCommand) -> bool {
        self.commands.try_send(command).is_ok()
    }

    /// Sends a command that carries its own reply channel and waits for the
    /// answer. `None` when the room is gone.
    pub(crate) async fn ask<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> RoomCommand,
    ) -> Option<T> {
        let (reply, answer) = oneshot::channel();
        self.commands.send(make(reply)).await.ok()?;
        answer.await.ok()
    }

    /// Queues a command, waiting while the queue is full: for news the room
    /// must not miss. Returns false when the room is gone.
    pub(crate) async fn tell(&self, command: RoomCommand) -> bool {
        self.commands.send(command).await.is_ok()
    }
}

/// Secret material that authorizes joining a room.
pub(crate) struct RoomSecrets {
    pub(crate) key: hmac::Key,
    /// Kept as bytes: they are persisted, and ring verifies against slices.
    pub(crate) invite_tag: Vec<u8>,
    pub(crate) password_tag: Option<Vec<u8>>,
}

impl RoomSecrets {
    pub(crate) fn invite_input(room: &RoomId, token: &FixedBytes<32>) -> Vec<u8> {
        [b"invite".as_slice(), &room.0.0, &token.0].concat()
    }

    pub(crate) fn password_input(room: &RoomId, password: &Text<64>) -> Vec<u8> {
        [
            b"password".as_slice(),
            &room.0.0,
            password.as_str().as_bytes(),
        ]
        .concat()
    }

    /// Checks the invite token and password in constant time. Both are always
    /// checked; which one failed stays inside the server, and the client
    /// learns only `BadInvite`.
    fn check(
        &self,
        room: &RoomId,
        token: &FixedBytes<32>,
        password: Option<&Text<64>>,
    ) -> Admittance {
        let invite_ok = hmac::verify(
            &self.key,
            &Self::invite_input(room, token),
            self.invite_tag.as_ref(),
        )
        .is_ok();
        let password_ok = match (&self.password_tag, password) {
            (None, _) => true,
            (Some(tag), Some(password)) => hmac::verify(
                &self.key,
                &Self::password_input(room, password),
                tag.as_ref(),
            )
            .is_ok(),
            (Some(_), None) => false,
        };
        match (invite_ok, password_ok) {
            (true, true) => Admittance::Admitted,
            (true, false) => Admittance::WrongPassword,
            (false, _) => Admittance::WrongInvite,
        }
    }
}

enum Admittance {
    Admitted,
    /// A valid invite with a wrong or missing password: someone holding the
    /// invite, possibly guessing.
    WrongPassword,
    WrongInvite,
}

/// Wrong passwords a room takes from newcomers per minute. Past this, it
/// refuses every newcomer's password, right or wrong, for the rest of the
/// minute, so a leaked invite does not let anyone guess the password at
/// line rate. Members already seated are never held up.
const PASSWORD_FAILURES_PER_MINUTE: u32 = 10;

struct PasswordGuard {
    window_start: Instant,
    failures: u32,
}

impl PasswordGuard {
    fn new() -> Self {
        Self {
            window_start: Instant::now(),
            failures: 0,
        }
    }

    fn roll(&mut self, now: Instant) {
        if now.saturating_duration_since(self.window_start) >= Duration::from_secs(60) {
            self.window_start = now;
            self.failures = 0;
        }
    }

    fn open(&mut self, now: Instant) -> bool {
        self.roll(now);
        self.failures < PASSWORD_FAILURES_PER_MINUTE
    }

    fn failed(&mut self, now: Instant) {
        self.roll(now);
        self.failures = self.failures.saturating_add(1);
    }
}

struct Member {
    player: PlayerId,
    name: Text<32>,
    platform: Platform,
    ready: bool,
    content: Option<ContentFingerprint>,
    link: Option<MemberLink>,
    /// Whether this member's current link has an open turn stream.
    streaming: bool,
    pace: Pace,
    /// When this member's progress last moved forward.
    advanced: Instant,
    intents: TokenBucket,
    payload_bytes: TokenBucket,
    chats: TokenBucket,
    /// What this member must receive before it can follow the game.
    needs: Needs,
    /// The snapshot this member's connection may fetch.
    offered: Option<SnapshotId>,
    /// When the room last rebased this member.
    rebased: Option<Instant>,
    /// The first event of this member's turn stream: it can report only
    /// saves from there on.
    stream_from: u64,
}

/// What a member of a running game must receive before it can follow it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Needs {
    Nothing,
    /// A world: it joined the running game, or could no longer resume.
    World,
    /// A world agreed on at or after this step, where its own diverged.
    Rebase {
        after: u64,
    },
}

/// How long the room waits for a member before it stops holding everyone
/// else for them. A demoted member rejoins the pacing set by catching up.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timeouts {
    /// A member with sealed steps to run that has not advanced for this long.
    pub(crate) stall: Duration,
    /// A member still loading the world this long after the start.
    pub(crate) load: Duration,
    /// A running game with nobody connected for this long closes.
    pub(crate) abandoned: Duration,
}

/// How a member relates to the room clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pace {
    /// Loading the world after the start; holds the clock.
    Loading,
    /// Executing close to the frontier; the slowest of these paces the room.
    Following(u64),
    /// Too far behind to pace the room (after reconnecting, for example).
    CatchingUp(Option<u64>),
}

enum Phase {
    Lobby,
    Running(Box<Game>),
}

struct Game {
    pacer: Pacer,
    speed: Speed,
    /// The speed in the last turn sent. A change must reach clients even
    /// when nothing else happens, or a pause would go unannounced.
    announced_speed: Speed,
    sealed_through: u64,
    next_turn: u64,
    next_event: u64,
    pending: Vec<Event>,
    /// The most recent turns, encoded, for members who resume: at most
    /// [`RESUME_WINDOW`] of them, starting with turn `log_first_turn`.
    log: VecDeque<LoggedTurn>,
    log_first_turn: u64,
    /// The frontier of the turn before `log_first_turn`.
    sealed_before_log: u64,
    /// Bytes of the turns in `log`.
    log_bytes: usize,
    resume_window: usize,
    last_tick: Instant,
    /// When the game started (or was restored), for the load timeout.
    started: Instant,
    /// Checkpoint rounds by step.
    rounds: BTreeMap<u64, Round>,
    /// Rounds at or below this step are closed: pruned or expired. A report
    /// for such a step without a kept round is ignored, so nobody can reopen
    /// old rounds, fill the open-round limit and switch verdicts off.
    rounds_closed_through: u64,
    /// Every history of the game, oldest first; the last is current. A new
    /// one begins at each recovery, after the last turn logged.
    histories: Vec<History>,
    saves: Saves,
    /// Who sits at the table as the events say, in join order.
    seated: Vec<Seat>,
}

/// A player at a game's table: who, under which name, on which platform.
type Seat = (PlayerId, Text<32>, Platform);

/// A stretch of a game's turns that clients can resume on.
struct History {
    id: u64,
    /// The last turn this history shares with the one before it.
    after_turn: u64,
}

struct Round {
    opened: Instant,
    reports: Vec<Report>,
    verdict: Option<Verdict>,
}

struct LoggedTurn {
    first_event: u64,
    sealed_through: u64,
    frame: Arc<[u8]>,
}

/// Where a turn stream starts, and the sealed turns it starts with.
struct Stream {
    next_turn: u64,
    next_event: u64,
    sealed_through: u64,
    backlog: Vec<Arc<[u8]>>,
}

/// How a seated member comes back.
enum Rejoin {
    Lobby,
    Stream(TurnFeed),
    World,
}

pub(crate) struct Room {
    id: RoomId,
    name: Text<48>,
    /// The name of the rules `ruleset` plays by.
    rules: RulesName,
    owner: PlayerId,
    max_players: u8,
    settings: RoomSettings,
    secrets: RoomSecrets,
    members: Vec<Member>,
    phase: Phase,
    ruleset: Box<dyn Ruleset>,
    tick: Duration,
    metrics: Arc<Metrics>,
    /// Where running games are logged; `None` keeps rooms in memory only.
    data_dir: Option<PathBuf>,
    timeouts: Timeouts,
    log: Option<RoomLog>,
    /// A compaction of the log in progress.
    compaction: Option<Compaction>,
    /// The log's size at which it is compacted next.
    compact_at: u64,
    /// See [`RoomEnv::compact_log_at`].
    compact_log_at: u64,
    /// Since when no member has been connected, while the game runs.
    unattended_since: Option<Instant>,
    password_guard: PasswordGuard,
    /// Players the owner removed, who cannot join again.
    banned: BTreeSet<PlayerId>,
    /// Counts this room against the address that created it until it
    /// closes. Restored rooms have none.
    _share: Option<RoomShare>,
    /// The game build and mods of a running game, which players who join
    /// it must match.
    content: Option<ContentFingerprint>,
    /// The server's snapshots; `None` if it keeps none, and then nobody can
    /// join a running game.
    snapshots: Option<Arc<Snapshots>>,
    closed: bool,
}

/// What every room of a server shares.
#[derive(Clone)]
pub(crate) struct RoomEnv {
    pub(crate) tick: Duration,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) data_dir: Option<PathBuf>,
    pub(crate) timeouts: Timeouts,
    pub(crate) snapshots: Option<Arc<Snapshots>>,
    /// A game's log is compacted once it grows past this many bytes.
    pub(crate) compact_log_at: u64,
}

/// Why a room log could not be turned back into a room.
#[derive(Debug, Error)]
pub(crate) enum RecoverError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Log(#[from] LogError),
    #[error("the log has no start record")]
    Empty,
    #[error("the start record is unreadable: {0}")]
    Start(postcard::Error),
    #[error("the log's format version {0} is not supported")]
    Version(u16),
    #[error("the start record's room settings are out of range")]
    Settings,
    #[error("the room is played by rules \"{0}\", which this server no longer offers")]
    UnknownRules(String),
    #[error("the log's file name does not match its room")]
    Misnamed,
    #[error("turn record {0} is unreadable")]
    Turn(usize),
    #[error("turn record {0} breaks the log's continuity")]
    Continuity(usize),
    #[error("turn record {0} seals implausibly far")]
    Frontier(usize),
    #[error("the log's base contradicts itself")]
    Base,
    #[error("the rules cannot take on the log's base: {0}")]
    Rules(String),
}

pub(crate) struct RoomSpec {
    pub(crate) id: RoomId,
    pub(crate) name: Text<48>,
    pub(crate) max_players: u8,
    pub(crate) settings: RoomSettings,
    pub(crate) secrets: RoomSecrets,
    pub(crate) rules: RulesName,
    pub(crate) ruleset: Box<dyn Ruleset>,
    pub(crate) env: RoomEnv,
    pub(crate) share: RoomShare,
}

impl Room {
    pub(crate) fn new(spec: RoomSpec, owner: NewMember) -> Self {
        let mut room = Self {
            id: spec.id,
            name: spec.name,
            rules: spec.rules,
            owner: owner.player,
            max_players: spec.max_players,
            settings: spec.settings,
            secrets: spec.secrets,
            members: Vec::new(),
            phase: Phase::Lobby,
            ruleset: spec.ruleset,
            tick: spec.env.tick,
            metrics: spec.env.metrics,
            data_dir: spec.env.data_dir,
            timeouts: spec.env.timeouts,
            log: None,
            compaction: None,
            compact_at: spec.env.compact_log_at,
            compact_log_at: spec.env.compact_log_at,
            unattended_since: None,
            password_guard: PasswordGuard::new(),
            banned: BTreeSet::new(),
            _share: Some(spec.share),
            content: None,
            snapshots: spec.env.snapshots,
            closed: false,
        };
        room.members.push(Member::new(owner));
        room
    }

    /// Rebuilds a running room from its log: replays every turn through the
    /// ruleset and seats everyone who had not left, disconnected and ready to
    /// resume. A compacted log starts from its base instead, and replays
    /// only the turns after it. A log whose players had all left is deleted
    /// and gives `None`.
    pub(crate) fn recover(
        path: &Path,
        key: hmac::Key,
        menu: &RulesMenu,
        env: RoomEnv,
    ) -> Result<Option<Self>, RecoverError> {
        let mut reader = RoomLog::read(path)?;
        let first = reader.next_record()?.ok_or(RecoverError::Empty)?;
        let start: StartRecord = postcard::from_bytes(&first).map_err(RecoverError::Start)?;
        if start.version != persist::FORMAT_VERSION {
            return Err(RecoverError::Version(start.version));
        }
        if !start.settings.is_valid() {
            return Err(RecoverError::Settings);
        }
        // A room keeps its rules for good; the server must still have them.
        let mut ruleset = match menu.find(Some(start.rules.as_str())) {
            Some(choice) => (choice.factory)(),
            None => return Err(RecoverError::UnknownRules(start.rules.as_str().to_owned())),
        };
        let expected = start.id.to_string();
        if path.file_stem() != Some(std::ffi::OsStr::new(&expected)) {
            return Err(RecoverError::Misnamed);
        }
        let mut game = Game::new(start.settings, start.history);
        // Players the owner removed.
        let mut banned = BTreeSet::new();
        let base = start.base.as_ref();
        if let Some(base) = base {
            let mut seats: Vec<PlayerId> = base.seated.iter().map(|(player, ..)| *player).collect();
            seats.sort_unstable();
            seats.dedup();
            if !game.rebase(base)
                || base.banned.len() > MAX_BANNED
                || seats.len() != base.seated.len()
                || seats.len() > usize::from(start.max_players)
            {
                return Err(RecoverError::Base);
            }
            ruleset.restore(&base.rules).map_err(RecoverError::Rules)?;
            banned.extend(base.banned.iter().copied());
        }
        // A compacted log's turns up to its base are only kept for players
        // who resume: the base holds what they did.
        let base_turn = base.map_or(0, |base| base.after_turn);
        let mut based = base.is_none();
        // Ownership passes as the live room passed it: to the earliest
        // remaining player whenever the owner leaves.
        let mut owner = start.owner;
        let mut index = 0;
        loop {
            if !based && game.next_turn > base_turn {
                // The kept turns must end exactly where the base stands.
                if !base.is_some_and(|base| game.meets(base)) {
                    return Err(RecoverError::Continuity(index));
                }
                based = true;
            }
            let Some(frame) = reader.next_record()? else {
                break;
            };
            let turn = match frame
                .get(FRAME_HEADER_LEN..)
                .map(decode_frame::<TurnMessage>)
            {
                Some(Ok(TurnMessage::Turn(turn))) => turn,
                Some(Ok(TurnMessage::Start(marker))) => {
                    // An earlier recovery began a new history here. The
                    // histories of the kept turns are in the base.
                    if !based
                        || marker.next_turn != game.next_turn
                        || marker.next_event != game.next_event
                    {
                        return Err(RecoverError::Continuity(index));
                    }
                    game.begin_history(marker.history);
                    index += 1;
                    continue;
                }
                _ => return Err(RecoverError::Turn(index)),
            };
            if turn.number != game.next_turn || turn.sealed_through < game.sealed_through {
                return Err(RecoverError::Continuity(index));
            }
            if turn.sealed_through > MAX_FRONTIER {
                return Err(RecoverError::Frontier(index));
            }
            let first_event = turn
                .events
                .first()
                .map_or(game.next_event, |event| event.seq);
            for event in &turn.events {
                if event.seq != game.next_event {
                    return Err(RecoverError::Continuity(index));
                }
                game.next_event += 1;
                if !based {
                    continue;
                }
                ruleset.apply(event);
                seat(&mut game.seated, event);
                if let EventBody::PlayerLeft { player, kicked } = &event.body {
                    if *player == owner
                        && let Some((first, ..)) = game.seated.first()
                    {
                        owner = *first;
                    }
                    if *kicked && banned.len() < MAX_BANNED {
                        banned.insert(*player);
                    }
                }
            }
            game.remember(LoggedTurn {
                first_event,
                sealed_through: turn.sealed_through,
                frame: Arc::from(frame),
            });
            game.next_turn += 1;
            game.sealed_through = turn.sealed_through;
            game.speed = turn.speed;
            game.announced_speed = turn.speed;
            index += 1;
        }
        if !based {
            // The log ends among its kept turns.
            return Err(RecoverError::Continuity(index));
        }
        game.pacer.resume_at(game.sealed_through);
        // Every player started with the same content, and players who joined
        // later had to match it.
        let content = match base {
            Some(base) => base.content,
            None => start.members.first().and_then(|member| member.content),
        };
        let members: Vec<Member> = game
            .seated
            .iter()
            .cloned()
            .map(|(player, name, platform)| Member {
                player,
                name,
                platform,
                ready: true,
                content,
                link: None,
                streaming: false,
                pace: Pace::CatchingUp(None),
                advanced: Instant::now(),
                intents: TokenBucket::new(INTENTS_PER_SECOND, INTENT_BURST),
                payload_bytes: TokenBucket::new(PAYLOAD_BYTES_PER_SECOND, PAYLOAD_BURST),
                chats: TokenBucket::new(CHATS_PER_SECOND, CHAT_BURST),
                needs: Needs::Nothing,
                offered: None,
                rebased: None,
                stream_from: 0,
            })
            .collect();
        if members.is_empty() {
            reader.delete()?;
            if let Some(dir) = &env.data_dir {
                Pointer::remove(dir, &start.id);
            }
            return Ok(None);
        }
        if let (Some(dir), Some(snapshots)) = (&env.data_dir, &env.snapshots) {
            game.saves.current = recover_snapshot(dir, &start.id, snapshots, &game);
            game.saves.last_point = game.saves.current.as_ref().map(|agreed| agreed.point);
        }
        // Only now, with the room rebuilt, may a torn final record be cut.
        let mut log = reader.into_log()?;
        // Turns after the last one logged may have reached clients before
        // the crash, and the room will now number different turns the same
        // way. It begins a new history, so nobody is resumed onto turns that
        // differ from the ones they saw.
        game.begin_history(new_history());
        let head = Stream {
            next_turn: game.next_turn,
            next_event: game.next_event,
            sealed_through: game.sealed_through,
            backlog: Vec::new(),
        };
        let marker = TurnMessage::Start(game.turn_start(
            start.id,
            start.settings,
            &start.rules,
            &head,
            None,
        ));
        log.append(&encode_frame(&marker, TURN_MAX_FRAME).map_err(io::Error::other)?)?;
        let owner = if members.iter().any(|member| member.player == owner) {
            owner
        } else {
            members[0].player
        };
        // A log compacted just before the restart is not compacted again at
        // once.
        let compact_at = next_compaction(log.written(), env.compact_log_at);
        Ok(Some(Self {
            id: start.id,
            name: start.name,
            rules: start.rules,
            owner,
            max_players: start.max_players,
            settings: start.settings,
            secrets: RoomSecrets {
                key,
                invite_tag: start.invite_tag,
                password_tag: start.password_tag,
            },
            members,
            phase: Phase::Running(Box::new(game)),
            ruleset,
            tick: env.tick,
            metrics: env.metrics,
            data_dir: env.data_dir,
            timeouts: env.timeouts,
            log: Some(log),
            compaction: None,
            compact_at,
            compact_log_at: env.compact_log_at,
            // Nobody is connected after a restart; the abandon timeout runs
            // from here.
            unattended_since: None,
            password_guard: PasswordGuard::new(),
            banned,
            _share: None,
            content,
            snapshots: env.snapshots,
            closed: false,
        }))
    }

    pub(crate) fn id(&self) -> RoomId {
        self.id
    }

    /// The snapshots this room holds in the server's store.
    pub(crate) fn held_snapshots(&self) -> Vec<ManifestId> {
        match &self.phase {
            Phase::Running(game) => game.saves.held(),
            Phase::Lobby => Vec::new(),
        }
    }

    pub(crate) fn view(&self) -> RoomView {
        RoomView {
            id: self.id,
            name: self.name.clone(),
            rules: self.rules.clone(),
            owner: self.owner,
            max_players: self.max_players,
            has_password: self.secrets.password_tag.is_some(),
            phase: match self.phase {
                Phase::Lobby => RoomPhase::Lobby,
                Phase::Running(_) => RoomPhase::Running,
            },
            settings: self.settings,
            members: self
                .members
                .iter()
                .map(|member| MemberView {
                    player: member.player,
                    name: member.name.clone(),
                    platform: member.platform,
                    ready: member.ready,
                    content: member.content,
                    connected: member.link.is_some(),
                })
                .collect(),
        }
    }

    pub(crate) async fn run(
        mut self,
        mut commands: mpsc::Receiver<RoomCommand>,
        directory: Arc<Directory>,
    ) {
        info!(room = %self.id, "room opened");
        let mut ticker = tokio::time::interval(self.tick);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        while !self.closed {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => self.handle(command),
                    None => break,
                },
                _ = ticker.tick() => self.on_tick(Instant::now()),
                done = compaction_done(&mut self.compaction) => self.finish_compaction(done),
            }
        }
        directory.remove(&self.id);
        info!(room = %self.id, "room closed");
    }

    fn handle(&mut self, command: RoomCommand) {
        match command {
            RoomCommand::Join {
                member,
                token,
                password,
                resume,
                content,
                reply,
            } => {
                let result = self.join(member, &token, password.as_ref(), resume, content);
                let joined = result.is_ok();
                let _ = reply.send(result);
                if joined {
                    self.broadcast_view();
                }
            }
            RoomCommand::Leave { player, reply } => {
                let result = self.leave(player, false);
                let _ = reply.send(result);
            }
            RoomCommand::Disconnected { player, link } => self.disconnected(player, link),
            RoomCommand::SetReady {
                player,
                ready,
                reply,
            } => {
                let result = self
                    .in_lobby(player)
                    .map(|member| std::mem::replace(&mut member.ready, ready) != ready);
                self.answer_and_broadcast_if_changed(reply, result);
            }
            RoomCommand::DeclareContent {
                player,
                content,
                reply,
            } => {
                let result = self
                    .in_lobby(player)
                    .map(|member| member.content.replace(content) != Some(content));
                self.answer_and_broadcast_if_changed(reply, result);
            }
            RoomCommand::Start { player, reply } => {
                let result = self.start(player);
                self.answer_and_broadcast(reply, result);
            }
            RoomCommand::SetSpeed {
                player,
                speed,
                reply,
            } => {
                let _ = reply.send(self.set_speed(player, speed));
            }
            RoomCommand::Kick {
                player,
                target,
                reply,
            } => {
                let _ = reply.send(self.kick(player, target));
            }
            RoomCommand::Chat {
                player,
                text,
                reply,
            } => {
                let _ = reply.send(self.chat(player, text));
            }
            RoomCommand::IsMember { player, reply } => {
                let member = self.members.iter().any(|m| m.player == player);
                let _ = reply.send(if member {
                    Ok(())
                } else {
                    Err(RequestError::NotInRoom)
                });
            }
            RoomCommand::Intent {
                player,
                client_seq,
                payload,
            } => self.intent(player, client_seq, payload),
            RoomCommand::Progress { player, link, step } => self.progress(player, link, step),
            RoomCommand::Checkpoint {
                player,
                link,
                step,
                lanes,
            } => self.checkpoint(player, link, step, lanes, Instant::now()),
            RoomCommand::Saved {
                player,
                link,
                event,
                lanes,
                world,
            } => self.saved(player, link, event, lanes, world, Instant::now()),
            RoomCommand::Fetch {
                player,
                link,
                snapshot,
                reply,
            } => {
                let _ = reply.send(self.fetchable(player, link, &snapshot));
            }
            RoomCommand::Upload {
                player,
                link,
                snapshot,
                reply,
            } => {
                let _ = reply.send(self.upload_starts(player, link, &snapshot));
            }
            RoomCommand::Uploaded {
                player,
                snapshot,
                result,
            } => self.uploaded(player, snapshot, result, Instant::now()),
        }
    }

    fn answer_and_broadcast(&mut self, reply: Reply, result: Result<(), RequestError>) {
        let changed = result.is_ok();
        let _ = reply.send(result);
        if changed {
            self.broadcast_view();
        }
    }

    /// Answers a request that may have changed nothing. Repeating a request
    /// must not make the room broadcast to everyone again.
    fn answer_and_broadcast_if_changed(
        &mut self,
        reply: Reply,
        result: Result<bool, RequestError>,
    ) {
        let changed = result == Ok(true);
        let _ = reply.send(result.map(|_| ()));
        if changed {
            self.broadcast_view();
        }
    }

    fn member_mut(&mut self, player: PlayerId) -> Option<&mut Member> {
        self.members
            .iter_mut()
            .find(|member| member.player == player)
    }

    fn in_lobby(&mut self, player: PlayerId) -> Result<&mut Member, RequestError> {
        if !matches!(self.phase, Phase::Lobby) {
            return Err(RequestError::GameRunning);
        }
        self.member_mut(player).ok_or(RequestError::NotInRoom)
    }

    fn join(
        &mut self,
        new: NewMember,
        token: &FixedBytes<32>,
        password: Option<&Text<64>>,
        resume: Option<Resume>,
        content: Option<ContentFingerprint>,
    ) -> Result<RoomView, RequestError> {
        let now = Instant::now();
        if self.banned.contains(&new.player) {
            return Err(RequestError::BadInvite);
        }
        let seated = self.members.iter().any(|m| m.player == new.player);
        match self.secrets.check(&self.id, token, password) {
            Admittance::Admitted if seated || self.password_guard.open(now) => {}
            Admittance::WrongPassword if !seated => {
                self.password_guard.failed(now);
                return Err(RequestError::BadInvite);
            }
            _ => return Err(RequestError::BadInvite),
        }
        if let Some(index) = self.members.iter().position(|m| m.player == new.player) {
            // The same player again, e.g. after reconnecting: the new
            // connection takes over the seat. Validate the resume point
            // before touching the seat, so a failed resume changes nothing.
            let rejoin = match &self.phase {
                Phase::Lobby => Rejoin::Lobby,
                Phase::Running(_) if content.is_some() && content != self.content => {
                    return Err(RequestError::ContentMismatch);
                }
                // Without a world of this game, a player receives one.
                Phase::Running(_) if resume.is_none() && self.snapshots.is_some() => Rejoin::World,
                Phase::Running(game) => {
                    Rejoin::Stream(game.resume_feed(self.id, self.settings, &self.rules, resume)?)
                }
            };
            let member = &mut self.members[index];
            if let Some(old) = member.link.take()
                && old.id != new.link.id
            {
                old.connection
                    .close(close::REPLACED, b"signed in on another connection");
            }
            member.name = new.name;
            member.platform = new.platform;
            member.streaming = false;
            member.offered = None;
            match rejoin {
                Rejoin::Lobby => {}
                Rejoin::Stream(feed) => {
                    member.pace = Pace::CatchingUp(None);
                    member.needs = Needs::Nothing;
                    if let TurnFeed::Open { start, .. } = &feed {
                        member.stream_from = start.next_event;
                    }
                    member.streaming = new.link.turns.try_send(feed).is_ok();
                }
                Rejoin::World => {
                    member.pace = Pace::CatchingUp(None);
                    member.needs = Needs::World;
                }
            }
            member.link = Some(new.link);
            self.offer_worlds(now);
            return Ok(self.view());
        }
        let full = self.members.len() >= usize::from(self.max_players);
        if let Phase::Running(game) = &mut self.phase {
            // A newcomer to a running game starts from a snapshot, which
            // only a server that keeps them can give, of the same content.
            if self.snapshots.is_none() {
                return Err(RequestError::GameRunning);
            }
            if full {
                return Err(RequestError::RoomFull);
            }
            if content.is_none() || content != self.content {
                return Err(RequestError::ContentMismatch);
            }
            game.append(
                EventBody::PlayerJoined {
                    player: new.player,
                    name: new.name.clone(),
                    platform: new.platform,
                },
                self.ruleset.as_mut(),
            );
            info!(room = %self.id, player = %new.player, "a player joins the running game");
            metrics::increment(&self.metrics.late_joins);
            let mut member = Member::new(new);
            member.ready = true;
            member.content = content;
            member.needs = Needs::World;
            self.members.push(member);
            self.offer_worlds(now);
            return Ok(self.view());
        }
        if full {
            return Err(RequestError::RoomFull);
        }
        self.members.push(Member::new(new));
        Ok(self.view())
    }

    fn leave(&mut self, player: PlayerId, kicked: bool) -> Result<(), RequestError> {
        let index = self
            .members
            .iter()
            .position(|member| member.player == player)
            .ok_or(RequestError::NotInRoom)?;
        let member = self.members.remove(index);
        if let Some(link) = &member.link
            && member.streaming
        {
            let _ = link.turns.try_send(TurnFeed::Close);
        }
        if let Phase::Running(game) = &mut self.phase {
            game.append(
                EventBody::PlayerLeft { player, kicked },
                self.ruleset.as_mut(),
            );
        }
        self.after_departure(player);
        Ok(())
    }

    /// Passes a member's message to everyone in the room, the sender too, so
    /// every member sees the same conversation.
    fn chat(&mut self, player: PlayerId, text: ChatText) -> Result<(), RequestError> {
        let now = Instant::now();
        let member = self
            .members
            .iter_mut()
            .find(|member| member.player == player)
            .ok_or(RequestError::NotInRoom)?;
        if !member.chats.take(now, 1) {
            return Err(RequestError::RateLimited);
        }
        for index in 0..self.members.len() {
            self.push(
                index,
                ServerMessage::Chat {
                    from: player,
                    text: text.clone(),
                },
            );
        }
        Ok(())
    }

    /// The owner removes a player for good: the player is told, leaves as if
    /// by choice, and cannot join this room again.
    fn kick(&mut self, by: PlayerId, target: PlayerId) -> Result<(), RequestError> {
        if by != self.owner {
            return Err(RequestError::NotOwner);
        }
        if target == by {
            return Err(RequestError::CannotKickSelf);
        }
        let index = self
            .members
            .iter()
            .position(|member| member.player == target)
            .ok_or(RequestError::NoSuchPlayer)?;
        info!(room = %self.id, player = %target, "the owner removed a player");
        self.push(index, ServerMessage::Kicked);
        if self.banned.len() < MAX_BANNED {
            self.banned.insert(target);
        }
        self.leave(target, true)
    }

    fn disconnected(&mut self, player: PlayerId, link: u64) {
        // The member's link may already be gone, dropped by the room for a
        // full or closed queue.
        let Some(index) = self.members.iter().position(|member| {
            member.player == player && member.link.as_ref().is_none_or(|l| l.id == link)
        }) else {
            // An older connection of a player who has reconnected since.
            return;
        };
        match self.phase {
            Phase::Lobby => {
                // A lobby seat is not held for anyone.
                self.members.remove(index);
                self.after_departure(player);
            }
            Phase::Running(_) => {
                // Running games hold the seat: the player can resume.
                let member = &mut self.members[index];
                member.link = None;
                member.streaming = false;
                member.pace = Pace::CatchingUp(None);
                self.broadcast_view();
            }
        }
    }

    /// Hands ownership on and closes the room when nobody is left.
    fn after_departure(&mut self, player: PlayerId) {
        if self.members.is_empty() {
            // Everyone left: the game is over, and so is its log.
            self.discard_game();
            self.closed = true;
            return;
        }
        if self.owner == player {
            self.owner = self.members[0].player;
        }
        self.broadcast_view();
    }

    fn start(&mut self, player: PlayerId) -> Result<(), RequestError> {
        if player != self.owner {
            return Err(RequestError::NotOwner);
        }
        if matches!(self.phase, Phase::Running(_)) {
            return Err(RequestError::GameRunning);
        }
        if !self.members.iter().all(|member| member.ready) {
            return Err(RequestError::NotAllReady);
        }
        let first_content = self.members[0].content;
        if first_content.is_none()
            || self
                .members
                .iter()
                .any(|member| member.content != first_content)
        {
            return Err(RequestError::ContentMismatch);
        }
        let mut game = Game::new(self.settings, new_history());
        // The log starts by naming everyone at the table, in join order, so a
        // replay of the log alone reproduces membership.
        for member in &self.members {
            game.append(
                EventBody::PlayerJoined {
                    player: member.player,
                    name: member.name.clone(),
                    platform: member.platform,
                },
                self.ruleset.as_mut(),
            );
        }
        let first = Stream {
            next_turn: game.next_turn,
            next_event: 1,
            sealed_through: 0,
            backlog: Vec::new(),
        };
        let open = game.turn_start(self.id, self.settings, &self.rules, &first, None);
        self.content = first_content;
        // With snapshots, the owner's world is everyone's: the owner loads
        // it, the room saves it before the first step, and every other
        // player loads that save. Worlds generated on each machine could
        // differ between platforms. Everyone still holds the clock until
        // loaded.
        let shared_world = self.snapshots.is_some();
        let owner = self.owner;
        for member in &mut self.members {
            member.pace = Pace::Loading;
            member.stream_from = 1;
            member.streaming = false;
            if shared_world && member.player != owner {
                member.needs = Needs::World;
                continue;
            }
            member.needs = Needs::Nothing;
            if let Some(link) = &member.link {
                member.streaming = link
                    .turns
                    .try_send(TurnFeed::Open {
                        start: open.clone(),
                        backlog: Vec::new(),
                    })
                    .is_ok();
            }
        }
        let history = game.history();
        self.phase = Phase::Running(Box::new(game));
        if shared_world && self.members.len() > 1 {
            // Save as soon as the owner has loaded.
            self.offer_worlds(Instant::now());
        }
        self.open_log(history);
        info!(room = %self.id, players = self.members.len(), shared_world, "game started");
        metrics::increment(&self.metrics.games_started);
        Ok(())
    }

    /// Starts the log of a game that has just started. A game whose log
    /// cannot be written keeps running, in memory only.
    fn open_log(&mut self, history: u64) {
        let Some(dir) = &self.data_dir else {
            return;
        };
        let start = self.start_record(history, None);
        match RoomLog::create(dir, &start) {
            Ok(log) => self.log = Some(log),
            Err(error) => {
                error!(room = %self.id, %error, "cannot create the room log; the game will not survive a restart");
            }
        }
    }

    /// What the room's log starts with: the room, and for a compacted log
    /// the game's state the log continues from.
    fn start_record(&self, history: u64, base: Option<Base>) -> StartRecord {
        StartRecord {
            version: persist::FORMAT_VERSION,
            history,
            id: self.id,
            name: self.name.clone(),
            rules: self.rules.clone(),
            owner: self.owner,
            max_players: self.max_players,
            settings: self.settings,
            invite_tag: self.secrets.invite_tag.clone(),
            password_tag: self.secrets.password_tag.clone(),
            members: self
                .members
                .iter()
                .map(|member| StartMember {
                    player: member.player,
                    name: member.name.clone(),
                    platform: member.platform,
                    content: member.content,
                })
                .collect(),
            base,
        }
    }

    /// Starts rewriting a long game's log to begin from where the game
    /// stands now, keeping only the turns players may still resume on.
    /// Recovery then replays only what comes after, and the log never
    /// reaches its size limit. Runs right after sealing, when the rules have
    /// applied exactly the logged events. The rewrite runs on a blocking
    /// thread while the game goes on; see [`Room::finish_compaction`]. Rules
    /// that cannot save their state keep the whole log.
    fn start_compaction(&mut self) {
        if self.compaction.is_some() {
            return;
        }
        let (Some(dir), Some(written)) = (&self.data_dir, self.log.as_ref().map(RoomLog::written))
        else {
            return;
        };
        let Phase::Running(game) = &self.phase else {
            return;
        };
        if written < self.compact_at || !game.pending.is_empty() {
            return;
        }
        let Some(rules) = self.ruleset.save() else {
            self.compact_at = u64::MAX;
            return;
        };
        let base = game.base(rules, self.content, &self.banned);
        let first_history = game.histories.first().map_or(0, |history| history.id);
        let start = self.start_record(first_history, Some(base));
        let frames: Vec<Arc<[u8]>> = game
            .log
            .iter()
            .map(|turn| Arc::clone(&turn.frame))
            .collect();
        let dir = dir.clone();
        let task = tokio::task::spawn_blocking(move || RoomLog::compacted(&dir, &start, &frames));
        self.compaction = Some(Compaction {
            task,
            tail: Vec::new(),
        });
    }

    /// Puts a finished compaction's log in place of the room's, with the
    /// turns sealed since it began. Until then the old log stays the room's,
    /// and stays so if anything fails.
    fn finish_compaction(&mut self, done: Result<io::Result<RoomLog>, tokio::task::JoinError>) {
        let Some(compaction) = self.compaction.take() else {
            return;
        };
        let before = self.log.as_ref().map_or(0, RoomLog::written);
        let installed = match done {
            Ok(Ok(mut log)) => {
                let result = compaction
                    .tail
                    .iter()
                    .try_for_each(|frame| log.append(frame))
                    .and_then(|()| log.install());
                match result {
                    Ok(()) => Ok(log),
                    Err(error) => {
                        log.discard();
                        Err(error)
                    }
                }
            }
            Ok(Err(error)) => Err(error),
            Err(error) => Err(io::Error::other(error)),
        };
        match installed {
            Ok(log) => {
                info!(room = %self.id, before, after = log.written(), "compacted the room log");
                metrics::increment(&self.metrics.logs_compacted);
                self.compact_at = next_compaction(log.written(), self.compact_log_at);
                // The old log closes here, already replaced.
                self.log = Some(log);
            }
            Err(error) => {
                warn!(room = %self.id, %error, "cannot compact the room log; it keeps growing");
                self.compact_at = next_compaction(before, self.compact_log_at);
            }
        }
    }

    /// Drops a running compaction, for a game that is over: its log, once
    /// written, is deleted rather than put in place.
    fn cancel_compaction(&mut self) {
        let Some(compaction) = self.compaction.take() else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Ok(Ok(log)) = compaction.task.await {
                    tokio::task::spawn_blocking(move || log.discard());
                }
            });
        }
    }

    fn set_speed(&mut self, player: PlayerId, speed: Speed) -> Result<(), RequestError> {
        if player != self.owner {
            return Err(RequestError::NotOwner);
        }
        let Phase::Running(game) = &mut self.phase else {
            return Err(RequestError::GameNotRunning);
        };
        if speed > Speed::MAX {
            return Err(RequestError::InvalidSettings);
        }
        game.speed = speed;
        Ok(())
    }

    fn intent(&mut self, player: PlayerId, client_seq: u64, payload: Payload) {
        let now = Instant::now();
        let Some(index) = self.members.iter().position(|m| m.player == player) else {
            return;
        };
        let rejection = match &mut self.phase {
            Phase::Lobby => Some(IntentRejection::GameNotRunning),
            Phase::Running(game) => {
                let member = &mut self.members[index];
                if !member.intents.take(now, 1)
                    || !member.payload_bytes.take(now, payload.len() as u64)
                {
                    Some(IntentRejection::RateLimited)
                } else if let Err(code) = self.ruleset.validate(&player, &payload) {
                    Some(IntentRejection::Refused { code })
                } else {
                    game.append(
                        EventBody::Command {
                            player,
                            client_seq,
                            payload,
                        },
                        self.ruleset.as_mut(),
                    );
                    None
                }
            }
        };
        if let Some(reason) = rejection {
            metrics::increment(&self.metrics.intents_refused);
            self.push(index, ServerMessage::IntentRejected { client_seq, reason });
        }
    }

    fn progress(&mut self, player: PlayerId, link: u64, step: u64) {
        let Phase::Running(game) = &self.phase else {
            return;
        };
        let sealed = game.sealed_through;
        let window = game.pacer.window(game.speed);
        let Some(member) = self
            .members
            .iter_mut()
            .find(|m| m.player == player && m.link.as_ref().is_some_and(|l| l.id == link))
        else {
            return;
        };
        if step > sealed {
            // Executing an unsealed step breaks the protocol's central rule.
            warn!(room = %self.id, %player, step, sealed, "progress beyond the frontier");
            if let Some(link) = member.link.take() {
                link.connection
                    .close(close::PROTOCOL_VIOLATION, b"progress beyond the frontier");
            }
            member.streaming = false;
            member.pace = Pace::CatchingUp(None);
            return;
        }
        let pace = match member.pace {
            Pace::Loading => Pace::Following(step),
            Pace::Following(previous) => Pace::Following(previous.max(step)),
            Pace::CatchingUp(_) if step.saturating_add(window) >= sealed => Pace::Following(step),
            Pace::CatchingUp(_) => Pace::CatchingUp(Some(step)),
        };
        let moved = match (member.pace, pace) {
            (Pace::Following(before), Pace::Following(after)) => after > before,
            (_, Pace::Following(_)) => true,
            _ => false,
        };
        if moved {
            member.advanced = Instant::now();
        }
        member.pace = pace;
    }

    /// Stops members who stopped advancing from holding the room: one that
    /// has sealed steps to run but has not moved for the stall timeout, or
    /// one still loading after the load timeout. While the room is paused
    /// nobody is expected to move, so every clock restarts.
    fn demote_stalled(&mut self, now: Instant) {
        let Phase::Running(game) = &self.phase else {
            return;
        };
        let (sealed, paused, started) = (game.sealed_through, game.speed.is_paused(), game.started);
        for member in self.members.iter_mut().filter(|m| holds_clock(m)) {
            if paused {
                member.advanced = now;
                continue;
            }
            let stalled = match member.pace {
                Pace::Loading => now.saturating_duration_since(started) >= self.timeouts.load,
                Pace::Following(step) => {
                    step < sealed
                        && now.saturating_duration_since(member.advanced) >= self.timeouts.stall
                }
                Pace::CatchingUp(_) => false,
            };
            if stalled {
                info!(room = %self.id, player = %member.player, pace = ?member.pace, "a member stopped advancing; the room no longer waits for it");
                metrics::increment(&self.metrics.stalls);
                member.pace = match member.pace {
                    Pace::Following(step) => Pace::CatchingUp(Some(step)),
                    _ => Pace::CatchingUp(None),
                };
            }
        }
    }

    fn checkpoint(
        &mut self,
        player: PlayerId,
        link: u64,
        step: u64,
        mut lanes: Vec<LaneDigest>,
        now: Instant,
    ) {
        let interval = u64::from(self.settings.checkpoint_interval);
        let Phase::Running(game) = &mut self.phase else {
            return;
        };
        let Some(order) = self
            .members
            .iter()
            .position(|m| m.player == player && m.link.as_ref().is_some_and(|l| l.id == link))
        else {
            return;
        };
        if step == 0 || !step.is_multiple_of(interval) || step > game.sealed_through {
            debug!(room = %self.id, %player, step, "ignoring a checkpoint that is not due");
            return;
        }
        if step <= game.rounds_closed_through && !game.rounds.contains_key(&step) {
            debug!(room = %self.id, %player, step, "ignoring a checkpoint for a closed round");
            return;
        }
        let open = game
            .rounds
            .values()
            .filter(|round| round.verdict.is_none())
            .count();
        if !game.rounds.contains_key(&step) && open >= MAX_OPEN_ROUNDS {
            return;
        }
        lanes.sort_by_key(|lane| lane.lane);
        lanes.dedup_by_key(|lane| lane.lane);
        let round = game.rounds.entry(step).or_insert_with(|| Round {
            opened: now,
            reports: Vec::new(),
            verdict: None,
        });
        if round.reports.iter().any(|report| report.player == player) {
            return;
        }
        let report = Report {
            player,
            platform: self.members[order].platform,
            order,
            lanes,
        };
        let notices = match &round.verdict {
            Some(verdict) => {
                // A late report is judged against the decided verdict.
                let diverged = verdict::diverging_lanes(&report.lanes, verdict);
                round.reports.push(report);
                if diverged.is_empty() {
                    Vec::new()
                } else {
                    vec![(player, diverged)]
                }
            }
            None => {
                round.reports.push(report);
                if round.reports.len() >= MIN_VERDICT_REPORTS
                    && round_complete(&self.members, round)
                {
                    decide_round(round)
                } else {
                    Vec::new()
                }
            }
        };
        game.prune_rounds();
        self.announce_divergence(step, notices);
    }

    /// Decides rounds that everyone pacing the room has reported (members may
    /// have left since the last report) or whose deadline has passed. A
    /// round too few members reported by its deadline closes without a
    /// verdict.
    fn decide_waiting_rounds(&mut self, now: Instant) {
        let Phase::Running(game) = &mut self.phase else {
            return;
        };
        let mut decided = Vec::new();
        let mut expired = Vec::new();
        for (step, round) in &mut game.rounds {
            if round.verdict.is_some() {
                continue;
            }
            let overdue = now.saturating_duration_since(round.opened) >= CHECKPOINT_DEADLINE;
            let enough = round.reports.len() >= MIN_VERDICT_REPORTS;
            if enough && (overdue || round_complete(&self.members, round)) {
                decided.push((*step, decide_round(round)));
            } else if overdue {
                expired.push(*step);
            }
        }
        for step in expired {
            game.close_round(step);
        }
        if decided.is_empty() {
            return;
        }
        game.prune_rounds();
        for (step, notices) in decided {
            self.announce_divergence(step, notices);
        }
    }

    /// Tells members their world diverged at `step`, and schedules a rebase
    /// for each: the first world the room agrees on from there on replaces
    /// theirs.
    fn announce_divergence(&mut self, step: u64, notices: Vec<(PlayerId, Vec<u16>)>) {
        if notices.is_empty() {
            return;
        }
        let now = Instant::now();
        let rebasing = self.snapshots.is_some();
        for (player, lanes) in notices {
            warn!(room = %self.id, %player, step, ?lanes, "replica diverged from the verdict");
            metrics::increment(&self.metrics.divergences);
            if let Some(index) = self.members.iter().position(|m| m.player == player) {
                self.push(index, ServerMessage::Diverged { step, lanes });
                let member = &mut self.members[index];
                let rested = member
                    .rebased
                    .is_none_or(|at| now.saturating_duration_since(at) >= REBASE_GAP);
                if rebasing && member.needs == Needs::Nothing && rested {
                    member.needs = Needs::Rebase { after: step };
                }
            }
        }
        self.offer_worlds(now);
    }

    /// A member's report of a save.
    fn saved(
        &mut self,
        player: PlayerId,
        link: u64,
        event: u64,
        mut lanes: Vec<LaneDigest>,
        world: Option<SavedWorld>,
        now: Instant,
    ) {
        let Some(order) = self
            .members
            .iter()
            .position(|m| m.player == player && m.link.as_ref().is_some_and(|l| l.id == link))
        else {
            return;
        };
        // Only a member whose stream carried the save can have made it.
        let member = &self.members[order];
        if !member.streaming || member.stream_from > event {
            debug!(room = %self.id, %player, event, "ignoring a report of a save this member never saw");
            return;
        }
        let platform = member.platform;
        let Phase::Running(game) = &mut self.phase else {
            return;
        };
        // A save decided already, or never made: nothing to add to.
        let Some(round) = game.saves.rounds.get_mut(&event) else {
            debug!(room = %self.id, %player, event, "ignoring a report of no open save");
            return;
        };
        if round
            .reports
            .iter()
            .any(|save| save.report.player == player)
        {
            return;
        }
        lanes.sort_by_key(|lane| lane.lane);
        lanes.dedup_by_key(|lane| lane.lane);
        round.reports.push(SaveReport {
            report: Report {
                player,
                platform,
                order,
                lanes,
            },
            world,
        });
        if save_complete(&self.members, round) {
            self.decide_save(event, now);
        }
    }

    /// The manifest a member's connection may fetch: the snapshot the room
    /// offered that connection, and nothing else, so nobody can probe the
    /// server's store for other rooms' worlds.
    fn fetchable(
        &self,
        player: PlayerId,
        link: u64,
        snapshot: &SnapshotId,
    ) -> Option<Arc<Manifest>> {
        let member = self
            .members
            .iter()
            .find(|m| m.player == player && m.link.as_ref().is_some_and(|l| l.id == link))?;
        if member.offered != Some(*snapshot) {
            return None;
        }
        let Phase::Running(game) = &self.phase else {
            return None;
        };
        game.saves
            .find(snapshot)
            .map(|agreed| Arc::clone(&agreed.manifest))
    }

    /// Whether the room asked this member's connection for this save, and
    /// has not seen it start uploading yet.
    fn upload_starts(&mut self, player: PlayerId, link: u64, snapshot: &SnapshotId) -> bool {
        let linked = self
            .members
            .iter()
            .any(|m| m.player == player && m.link.as_ref().is_some_and(|l| l.id == link));
        let Phase::Running(game) = &mut self.phase else {
            return false;
        };
        match &mut game.saves.upload {
            Some(upload)
                if linked
                    && upload.from == player
                    && upload.world.snapshot == *snapshot
                    && !upload.receiving =>
            {
                upload.receiving = true;
                true
            }
            _ => false,
        }
    }

    fn uploaded(
        &mut self,
        player: PlayerId,
        snapshot: SnapshotId,
        result: Result<Arc<Manifest>, String>,
        now: Instant,
    ) {
        let Phase::Running(game) = &mut self.phase else {
            return;
        };
        let Some(upload) = game
            .saves
            .upload
            .take_if(|upload| upload.from == player && upload.world.snapshot == snapshot)
        else {
            // An upload the room gave up on: its hold goes back.
            if let (Ok(manifest), Some(snapshots)) = (&result, &self.snapshots) {
                release_in_background(Arc::clone(snapshots), vec![manifest.id()]);
            }
            return;
        };
        match result {
            Ok(manifest) => {
                info!(room = %self.id, %player, %snapshot, "received a save");
                self.promote(Agreed {
                    manifest,
                    point: upload.point,
                });
            }
            Err(error) => {
                warn!(room = %self.id, %player, %snapshot, %error, "a save's upload failed");
                metrics::increment(&self.metrics.uploads_failed);
                game.saves.failed_uploader(player);
                self.ask_for_upload(upload.point, upload.rest, now);
            }
        }
    }

    /// Asks the first reachable player of `candidates` to upload its save.
    /// A save the room already holds needs no upload.
    fn ask_for_upload(&mut self, point: SavePoint, mut candidates: Candidates, now: Instant) {
        while let Some((player, world)) = candidates.pop_front() {
            let held = match &self.phase {
                Phase::Running(game) => game
                    .saves
                    .find(&world.snapshot)
                    .map(|agreed| Arc::clone(&agreed.manifest)),
                Phase::Lobby => return,
            };
            if let Some(manifest) = held {
                if let Some(snapshots) = &self.snapshots {
                    snapshots.hold(manifest.id());
                }
                self.promote(Agreed { manifest, point });
                return;
            }
            let Some(index) = self
                .members
                .iter()
                .position(|m| m.player == player && m.link.is_some())
            else {
                continue;
            };
            if let Phase::Running(game) = &mut self.phase {
                game.saves.upload = Some(Upload {
                    point,
                    from: player,
                    world,
                    rest: candidates,
                    asked: now,
                    receiving: false,
                });
            }
            self.push(
                index,
                ServerMessage::Upload {
                    event: point.event,
                    snapshot: world.snapshot,
                },
            );
            return;
        }
        debug!(room = %self.id, event = point.event, "nobody could hand this save on");
    }

    /// Makes `agreed` the snapshot players who need a world receive. The one
    /// before stays for downloads that may still run; the one before that
    /// goes. The caller took a hold on `agreed` for its slot; the slot that
    /// goes gives its hold back.
    fn promote(&mut self, agreed: Agreed) {
        let Phase::Running(game) = &mut self.phase else {
            return;
        };
        let id = agreed.id();
        let pointer = Pointer::new(id, agreed.point);
        info!(
            room = %self.id,
            snapshot = %id,
            step = agreed.point.sealed_through,
            bytes = agreed.manifest.total_size(),
            "the room agreed on a snapshot"
        );
        metrics::increment(&self.metrics.snapshots_agreed);
        let dropped = game.saves.previous.take();
        game.saves.previous = game.saves.current.replace(agreed);
        let released: Vec<ManifestId> = dropped
            .map(|agreed| agreed.manifest.id())
            .into_iter()
            .collect();
        if let Some(snapshots) = &self.snapshots {
            let snapshots = Arc::clone(snapshots);
            let dir = self.data_dir.clone();
            let room = self.id;
            tokio::task::spawn_blocking(move || {
                if let Some(dir) = dir
                    && let Err(error) = pointer.write(&dir, &room)
                {
                    warn!(%room, %error, "cannot record the room's snapshot; a restart will forget it");
                }
                snapshots.release(&released);
            });
        }
        self.offer_worlds(Instant::now());
    }

    /// Sends the current snapshot, with the turns since it, to every member
    /// waiting for a world it serves. Wants a save if someone still waits.
    fn offer_worlds(&mut self, now: Instant) {
        let Phase::Running(game) = &mut self.phase else {
            return;
        };
        let mut waiting = false;
        let mut feeds = Vec::new();
        for (index, member) in self.members.iter().enumerate() {
            let serves = |agreed: &Agreed| match member.needs {
                Needs::Nothing => false,
                Needs::World => true,
                Needs::Rebase { after } => agreed.point.sealed_through >= after,
            };
            if member.needs == Needs::Nothing || member.link.is_none() {
                continue;
            }
            let feed = game
                .saves
                .current
                .as_ref()
                .filter(|agreed| serves(agreed))
                .and_then(|agreed| {
                    game.feed_from(self.id, self.settings, &self.rules, agreed)
                        .ok()
                });
            match feed {
                Some(feed) => feeds.push((index, feed)),
                None => waiting = true,
            }
        }
        game.saves.wanted = waiting;
        let offered = game.saves.current.as_ref().map(Agreed::id);
        for (index, (feed, stream_from)) in feeds {
            let member = &mut self.members[index];
            let Some(link) = &member.link else {
                continue;
            };
            member.streaming = link.turns.try_send(feed).is_ok();
            if !member.streaming {
                continue;
            }
            if matches!(member.needs, Needs::Rebase { .. }) {
                info!(room = %self.id, player = %member.player, "rebasing a replica that diverged");
                metrics::increment(&self.metrics.rebases);
                member.rebased = Some(now);
            }
            member.needs = Needs::Nothing;
            member.offered = offered;
            // A player still loading the first world keeps holding the
            // clock; anyone else catches up.
            if member.pace != Pace::Loading {
                member.pace = Pace::CatchingUp(None);
            }
            member.stream_from = stream_from;
        }
    }

    /// Advances the game's saves: decides rounds that are complete or out of
    /// time, moves past an upload that never started, and seals a new save
    /// when one is due.
    fn run_saves(&mut self, now: Instant) {
        let Some(snapshots) = self.snapshots.clone() else {
            return;
        };
        let Phase::Running(game) = &mut self.phase else {
            return;
        };
        let ready: Vec<u64> = game
            .saves
            .rounds
            .iter()
            .filter(|(_, round)| {
                now.saturating_duration_since(round.opened) >= SAVE_DEADLINE
                    || save_complete(&self.members, round)
            })
            .map(|(event, _)| *event)
            .collect();
        let stalled = game.saves.upload.take_if(|upload| {
            let waited = now.saturating_duration_since(upload.asked);
            if upload.receiving {
                waited >= snapshots::upload_deadline(upload.world.size)
            } else {
                waited >= UPLOAD_START
            }
        });
        if let Some(upload) = &stalled {
            game.saves.failed_uploader(upload.from);
        }
        for event in ready {
            self.decide_save(event, now);
        }
        if let Some(upload) = stalled {
            warn!(room = %self.id, player = %upload.from, receiving = upload.receiving, "a player asked for its save did not deliver it in time");
            metrics::increment(&self.metrics.uploads_failed);
            if upload.receiving
                && let Some(member) = self.members.iter_mut().find(|m| m.player == upload.from)
                && let Some(link) = member.link.take()
            {
                // Its transfer is still running: end it, which frees the
                // slot it holds. The player reconnects and resumes.
                link.connection
                    .close(close::SLOW_CONSUMER, b"the upload was too slow");
                member.streaming = false;
                member.pace = Pace::CatchingUp(None);
            }
            self.ask_for_upload(upload.point, upload.rest, now);
        }
        self.save_if_due(&snapshots, now);
    }

    /// Decides the save round of the save event `event`, tells members who
    /// diverged, and asks a member whose save agreed to upload it.
    fn decide_save(&mut self, event: u64, now: Instant) {
        let Phase::Running(game) = &mut self.phase else {
            return;
        };
        let Some(round) = game.saves.rounds.remove(&event) else {
            return;
        };
        let (mut candidates, diverged) = snapshots::decide(&round.reports);
        // Players whose uploads failed go last.
        candidates
            .make_contiguous()
            .sort_by_key(|(player, _)| game.saves.failed.contains(player));
        debug!(
            room = %self.id,
            event,
            reports = round.reports.len(),
            candidates = candidates.len(),
            "decided a save"
        );
        self.announce_divergence(round.point.sealed_through, diverged);
        self.ask_for_upload(round.point, candidates, now);
    }

    /// Seals a save when one is due: the save event alone at the end of a
    /// turn that runs no new steps, so a stream from it starts at a turn
    /// boundary.
    fn save_if_due(&mut self, snapshots: &Snapshots, now: Instant) {
        let playing = self
            .members
            .iter()
            .any(|m| m.streaming && matches!(m.pace, Pace::Following(_)));
        let Phase::Running(game) = &mut self.phase else {
            return;
        };
        let log = (game.sealed_through, game.next_event);
        if !playing
            || !game
                .saves
                .due(now, snapshots.every, snapshots.min_gap, game.started, log)
        {
            return;
        }
        game.append(EventBody::Save, self.ruleset.as_mut());
        let event = game.next_event - 1;
        let frontier = game.sealed_through;
        let frames = match game.seal(frontier) {
            Ok(frames) => frames,
            Err(error) => {
                error!(room = %self.id, %error, "cannot encode a turn; closing the room");
                self.close_all(close::SHUTTING_DOWN, b"internal error");
                return;
            }
        };
        let point = SavePoint {
            event,
            after_turn: game.next_turn - 1,
            history: game.history(),
            sealed_through: frontier,
        };
        game.saves.rounds.insert(
            event,
            SaveRound {
                point,
                opened: now,
                reports: Vec::new(),
            },
        );
        game.saves.last_save = Some(now);
        game.saves.last_point = Some(point);
        info!(room = %self.id, event, step = frontier, "the room saves its world");
        metrics::increment(&self.metrics.saves);
        self.publish(frames);
    }

    /// Closes a running game nobody has been connected to for the abandon
    /// timeout, and deletes its log. Without this, games whose players all
    /// disconnected would hold server resources forever, even across
    /// restarts.
    fn expire_if_abandoned(&mut self, now: Instant) {
        if !matches!(self.phase, Phase::Running(_)) {
            return;
        }
        if self.members.iter().any(|member| member.link.is_some()) {
            self.unattended_since = None;
            return;
        }
        let since = *self.unattended_since.get_or_insert(now);
        if now.saturating_duration_since(since) < self.timeouts.abandoned {
            return;
        }
        info!(room = %self.id, "closing a game nobody returned to");
        metrics::increment(&self.metrics.rooms_abandoned);
        self.discard_game();
        self.closed = true;
    }

    /// Deletes what a finished game kept: its log, its snapshot pointer and
    /// its snapshots.
    fn discard_game(&mut self) {
        self.cancel_compaction();
        if let Some(log) = self.log.take()
            && let Err(error) = log.delete()
        {
            warn!(room = %self.id, %error, "cannot delete the log of a closed room");
        }
        if let Some(dir) = &self.data_dir {
            Pointer::remove(dir, &self.id);
        }
        if let (Some(snapshots), Phase::Running(game)) = (&self.snapshots, &self.phase) {
            release_in_background(Arc::clone(snapshots), game.saves.held());
        }
    }

    /// Frees lobby seats whose connection is gone. A lobby seat is not held
    /// for anyone, and a notice of the disconnect can be lost when the
    /// room's queue is full, so this does not wait for one.
    fn sweep_lobby(&mut self) {
        if !matches!(self.phase, Phase::Lobby) {
            return;
        }
        while let Some(index) = self.members.iter().position(|m| m.link.is_none()) {
            let player = self.members.remove(index).player;
            self.after_departure(player);
            if self.closed {
                return;
            }
        }
    }

    fn on_tick(&mut self, now: Instant) {
        self.sweep_lobby();
        self.expire_if_abandoned(now);
        if self.closed {
            return;
        }
        self.decide_waiting_rounds(now);
        self.demote_stalled(now);
        self.run_saves(now);
        if self.closed {
            return;
        }
        let Phase::Running(game) = &mut self.phase else {
            return;
        };
        let elapsed = now.saturating_duration_since(game.last_tick);
        game.last_tick = now;
        let slowest = slowest_pacer(&self.members);
        let frontier = game
            .pacer
            .advance(elapsed, game.speed, slowest, game.sealed_through);
        if frontier == game.sealed_through
            && game.pending.is_empty()
            && game.speed == game.announced_speed
        {
            return;
        }
        let ordered = game.pending.len() as u64;
        let frames = match game.seal(frontier) {
            Ok(frames) => {
                metrics::add(&self.metrics.events_ordered, ordered);
                metrics::add(&self.metrics.turns_sealed, frames.len() as u64);
                frames
            }
            Err(error) => {
                // Unreachable with the payload budget; fail closed if it
                // happens rather than send a partial log.
                error!(room = %self.id, %error, "cannot encode a turn; closing the room");
                self.close_all(close::SHUTTING_DOWN, b"internal error");
                return;
            }
        };
        self.publish(frames);
    }

    /// Logs sealed turns and sends them to every member with a stream.
    fn publish(&mut self, frames: Vec<Arc<[u8]>>) {
        if let Some(log) = &mut self.log
            && let Err(error) = frames.iter().try_for_each(|frame| log.append(frame))
        {
            error!(room = %self.id, %error, "cannot append to the room log; it stops here");
            self.log = None;
        }
        // A compaction under way gets them too, logged or not: its log may
        // yet be put in place.
        if let Some(compaction) = &mut self.compaction {
            compaction.tail.extend(frames.iter().cloned());
        }
        for frame in frames {
            for index in 0..self.members.len() {
                if self.members[index].streaming {
                    self.send_turn(index, TurnFeed::Frame(Arc::clone(&frame)));
                }
            }
        }
        self.start_compaction();
    }

    /// Sends a control message to a member, disconnecting members whose
    /// queue is full instead of buffering without bound.
    fn push(&mut self, index: usize, message: ServerMessage) {
        let member = &mut self.members[index];
        let Some(link) = &member.link else {
            return;
        };
        if let Err(error) = link.control.try_send(message) {
            self.drop_link(index, matches!(error, mpsc::error::TrySendError::Full(_)));
        }
    }

    fn send_turn(&mut self, index: usize, feed: TurnFeed) {
        let member = &mut self.members[index];
        let Some(link) = &member.link else {
            return;
        };
        if let Err(error) = link.turns.try_send(feed) {
            self.drop_link(index, matches!(error, mpsc::error::TrySendError::Full(_)));
        }
    }

    fn drop_link(&mut self, index: usize, slow: bool) {
        let member = &mut self.members[index];
        if let Some(link) = member.link.take() {
            if slow {
                debug!(room = %self.id, player = %member.player, "disconnecting a slow consumer");
                metrics::increment(&self.metrics.slow_consumers);
                link.connection
                    .close(close::SLOW_CONSUMER, b"not reading fast enough");
            }
            member.streaming = false;
            member.pace = Pace::CatchingUp(None);
        }
    }

    fn broadcast_view(&mut self) {
        let view = self.view();
        for index in 0..self.members.len() {
            self.push(index, ServerMessage::RoomUpdate(view.clone()));
        }
    }

    fn close_all(&mut self, code: quinn::VarInt, reason: &[u8]) {
        for member in &mut self.members {
            if let Some(link) = member.link.take() {
                link.connection.close(code, reason);
            }
            member.streaming = false;
        }
        self.closed = true;
    }
}

impl Member {
    fn new(new: NewMember) -> Self {
        Self {
            player: new.player,
            name: new.name,
            platform: new.platform,
            ready: false,
            content: None,
            link: Some(new.link),
            streaming: false,
            pace: Pace::CatchingUp(None),
            advanced: Instant::now(),
            intents: TokenBucket::new(INTENTS_PER_SECOND, INTENT_BURST),
            payload_bytes: TokenBucket::new(PAYLOAD_BYTES_PER_SECOND, PAYLOAD_BURST),
            chats: TokenBucket::new(CHATS_PER_SECOND, CHAT_BURST),
            needs: Needs::Nothing,
            offered: None,
            rebased: None,
            stream_from: 0,
        }
    }
}

/// Whether a member can hold the room's clock: one with a turn stream, or
/// one still waiting for the world the game starts from.
fn holds_clock(member: &Member) -> bool {
    member.streaming || (member.pace == Pace::Loading && member.link.is_some())
}

/// Whether every member pacing the room has reported this round. Members
/// catching up report later and are judged against the verdict then.
fn round_complete(members: &[Member], round: &Round) -> bool {
    members
        .iter()
        .filter(|m| m.streaming && matches!(m.pace, Pace::Loading | Pace::Following(_)))
        .all(|m| round.reports.iter().any(|report| report.player == m.player))
}

/// Whether every member pacing the room whose stream carries this save has
/// reported it. A member whose stream starts after the save never sees it.
fn save_complete(members: &[Member], round: &SaveRound) -> bool {
    members
        .iter()
        .filter(|m| {
            m.streaming
                && m.stream_from <= round.point.event
                && matches!(m.pace, Pace::Loading | Pace::Following(_))
        })
        .all(|m| {
            round
                .reports
                .iter()
                .any(|save| save.report.player == m.player)
        })
}

/// Stops keeping `ids` in the store, off the room's task. The server's
/// next collection deletes their chunks.
fn release_in_background(snapshots: Arc<Snapshots>, ids: Vec<ManifestId>) {
    if ids.is_empty() {
        return;
    }
    tokio::task::spawn_blocking(move || snapshots.release(&ids));
}

/// The snapshot a restored room last agreed on, if its pointer is readable,
/// the store still holds it, and the recovered log can be followed from it.
fn recover_snapshot(
    dir: &Path,
    room: &RoomId,
    snapshots: &Snapshots,
    game: &Game,
) -> Option<Agreed> {
    let pointer = Pointer::read(dir, room)?;
    let manifest = match snapshots
        .store
        .manifest(&bulk::manifest_id(&pointer.snapshot))
    {
        Ok(manifest) => manifest,
        Err(error) => {
            warn!(%room, %error, "a restored room's snapshot is gone");
            return None;
        }
    };
    let agreed = Agreed {
        manifest: Arc::new(manifest),
        point: pointer.point,
    };
    if game.stream_from_save(&agreed.point).is_err() {
        warn!(%room, "a restored room's snapshot does not fit its log");
        return None;
    }
    Some(agreed)
}

fn decide_round(round: &mut Round) -> Vec<(PlayerId, Vec<u16>)> {
    let (verdict, diverged) = verdict::decide(&round.reports);
    round.verdict = Some(verdict);
    diverged
}

/// The lowest progress among members who pace the room, or `None` to hold
/// the clock: while anyone is still loading, or when nobody is following.
/// Only members that can hold the clock count (see [`holds_clock`]).
fn slowest_pacer(members: &[Member]) -> Option<u64> {
    let mut slowest: Option<u64> = None;
    for member in members.iter().filter(|m| holds_clock(m)) {
        match member.pace {
            Pace::Loading => return None,
            Pace::Following(step) => {
                slowest = Some(slowest.map_or(step, |current| current.min(step)));
            }
            Pace::CatchingUp(_) => {}
        }
    }
    slowest
}

/// Keeps `seated` as a game's events say: who sits at the table, in join
/// order.
fn seat(seated: &mut Vec<Seat>, event: &Event) {
    match &event.body {
        EventBody::PlayerLeft { player, .. } => seated.retain(|(seat, ..)| seat != player),
        EventBody::PlayerJoined {
            player,
            name,
            platform,
        } => {
            seated.retain(|(seat, ..)| seat != player);
            seated.push((*player, name.clone(), *platform));
        }
        EventBody::Command { .. } | EventBody::Save => {}
    }
}

/// A room's log being compacted on a blocking thread.
struct Compaction {
    task: tokio::task::JoinHandle<io::Result<RoomLog>>,
    /// Turns sealed since it began, which the new log needs too.
    tail: Vec<Arc<[u8]>>,
}

/// The result of the running compaction, or never without one.
async fn compaction_done(
    compaction: &mut Option<Compaction>,
) -> Result<io::Result<RoomLog>, tokio::task::JoinError> {
    match compaction {
        Some(compaction) => (&mut compaction.task).await,
        None => std::future::pending().await,
    }
}

/// Where a log compacted to `size` bytes is compacted next: once it has
/// grown by `every`, or by its own size if that is more, so a rewrite never
/// writes more than was appended since the last.
fn next_compaction(size: u64, every: u64) -> u64 {
    size.saturating_add(every.max(size))
}

/// A fresh history ID. It is random, so a client can never mistake a later
/// history of the room for one it saw.
fn new_history() -> u64 {
    let mut bytes = [0; 8];
    getrandom::fill(&mut bytes).expect("the operating system's random source is available");
    u64::from_le_bytes(bytes)
}

impl Game {
    fn new(settings: RoomSettings, history: u64) -> Self {
        Self {
            pacer: Pacer::new(
                settings.steps_per_second,
                Duration::from_millis(u64::from(settings.input_delay_ms)),
                MAX_AHEAD,
            ),
            speed: Speed::NORMAL,
            announced_speed: Speed::NORMAL,
            sealed_through: 0,
            next_turn: 1,
            next_event: 1,
            pending: Vec::new(),
            log: VecDeque::new(),
            log_first_turn: 1,
            sealed_before_log: 0,
            log_bytes: 0,
            resume_window: RESUME_WINDOW,
            last_tick: Instant::now(),
            started: Instant::now(),
            rounds: BTreeMap::new(),
            rounds_closed_through: 0,
            histories: vec![History {
                id: history,
                after_turn: 0,
            }],
            saves: Saves::default(),
            seated: Vec::new(),
        }
    }

    /// Stands the game where a compacted log's kept turns begin, with the
    /// base's histories and table. `false` if the base contradicts itself.
    fn rebase(&mut self, base: &Base) -> bool {
        // Counters that could not have grown this far in a game's life: a
        // crafted base must not bring them near overflowing.
        let plausible = base.after_turn < MAX_COUNT && base.next_event < MAX_COUNT;
        let consistent = plausible
            && base.first_turn >= 1
            && base.first_turn <= base.after_turn.saturating_add(1)
            && base.first_event >= 1
            && base.first_event <= base.next_event
            && base.sealed_before <= base.sealed_through
            && base.sealed_through <= MAX_FRONTIER
            && base.histories.first().is_some_and(|&(_, after)| after == 0)
            && base.histories.windows(2).all(|pair| pair[0].1 <= pair[1].1)
            && base
                .histories
                .last()
                .is_some_and(|&(_, after)| after <= base.after_turn);
        if !consistent {
            return false;
        }
        self.next_turn = base.first_turn;
        self.log_first_turn = base.first_turn;
        self.next_event = base.first_event;
        self.sealed_through = base.sealed_before;
        self.sealed_before_log = base.sealed_before;
        self.speed = base.speed;
        self.announced_speed = base.speed;
        self.histories = base
            .histories
            .iter()
            .map(|&(id, after_turn)| History { id, after_turn })
            .collect();
        self.seated.clone_from(&base.seated);
        true
    }

    /// Whether the game stands where `base` says its kept turns end.
    fn meets(&self, base: &Base) -> bool {
        self.next_turn == base.after_turn.saturating_add(1)
            && self.next_event == base.next_event
            && self.sealed_through == base.sealed_through
    }

    /// Where the game stands now, for a compacted log that keeps the turns
    /// of the resume window. Only between turns: nothing may be pending.
    fn base(
        &self,
        rules: Vec<u8>,
        content: Option<ContentFingerprint>,
        banned: &BTreeSet<PlayerId>,
    ) -> Base {
        Base {
            first_turn: self.log_first_turn,
            first_event: self
                .log
                .front()
                .map_or(self.next_event, |turn| turn.first_event),
            sealed_before: self.sealed_before_log,
            after_turn: self.next_turn - 1,
            next_event: self.next_event,
            sealed_through: self.sealed_through,
            speed: self.speed,
            histories: self
                .histories
                .iter()
                .map(|history| (history.id, history.after_turn))
                .collect(),
            content,
            seated: self.seated.clone(),
            banned: banned.iter().copied().collect(),
            rules,
        }
    }

    /// The current history.
    fn history(&self) -> u64 {
        self.histories.last().map_or(0, |history| history.id)
    }

    /// Begins a new history after the last turn so far.
    fn begin_history(&mut self, id: u64) {
        let after_turn = self.next_turn.saturating_sub(1);
        self.histories.push(History { id, after_turn });
    }

    /// The start message of `stream`, which starts from `world` if given.
    fn turn_start(
        &self,
        room: RoomId,
        settings: RoomSettings,
        rules: &RulesName,
        stream: &Stream,
        world: Option<WorldOffer>,
    ) -> TurnStart {
        TurnStart {
            room,
            rules: rules.clone(),
            next_turn: stream.next_turn,
            next_event: stream.next_event,
            sealed_through: stream.sealed_through,
            steps_per_second: settings.steps_per_second,
            checkpoint_interval: settings.checkpoint_interval,
            history: self.history(),
            world,
        }
    }

    /// Keeps the newest decided rounds for late reports and drops the rest.
    fn prune_rounds(&mut self) {
        let decided: Vec<u64> = self
            .rounds
            .iter()
            .filter(|(_, round)| round.verdict.is_some())
            .map(|(step, _)| *step)
            .collect();
        let excess = decided.len().saturating_sub(DECIDED_ROUNDS_KEPT);
        for step in &decided[..excess] {
            self.close_round(*step);
        }
    }

    fn close_round(&mut self, step: u64) {
        self.rounds.remove(&step);
        self.rounds_closed_through = self.rounds_closed_through.max(step);
    }

    /// Orders an event: the next sequence number, and the first step no
    /// member can have executed yet.
    fn append(&mut self, body: EventBody, ruleset: &mut dyn Ruleset) {
        let event = Event {
            seq: self.next_event,
            step: self.sealed_through + 1,
            body,
        };
        self.next_event += 1;
        ruleset.apply(&event);
        seat(&mut self.seated, &event);
        self.pending.push(event);
    }

    /// Seals up to `frontier`, returning the encoded turns. Pending events
    /// are split over several turns if needed; only the last one moves the
    /// frontier, so every turn is a valid prefix of the log.
    fn seal(&mut self, frontier: u64) -> Result<Vec<Arc<[u8]>>, tpf3mp_proto::FrameError> {
        let mut batches: Vec<Vec<Event>> = vec![Vec::new()];
        let mut budget = 0;
        for event in std::mem::take(&mut self.pending) {
            let size = match &event.body {
                EventBody::Command { payload, .. } => payload.len(),
                _ => 0,
            };
            if budget + size > TURN_PAYLOAD_BUDGET && !batches.last().is_some_and(Vec::is_empty) {
                batches.push(Vec::new());
                budget = 0;
            }
            budget += size;
            if let Some(batch) = batches.last_mut() {
                batch.push(event);
            }
        }
        let last = batches.len() - 1;
        let mut frames = Vec::with_capacity(batches.len());
        for (index, events) in batches.into_iter().enumerate() {
            let first_event = events.first().map_or(self.next_event, |event| event.seq);
            let sealed_through = if index == last {
                frontier
            } else {
                self.sealed_through
            };
            let turn = Turn {
                number: self.next_turn,
                sealed_through,
                speed: self.speed,
                events,
            };
            let frame: Arc<[u8]> = encode_frame(&TurnMessage::Turn(turn), TURN_MAX_FRAME)?.into();
            self.remember(LoggedTurn {
                first_event,
                sealed_through,
                frame: Arc::clone(&frame),
            });
            self.next_turn += 1;
            frames.push(frame);
        }
        self.sealed_through = frontier;
        self.announced_speed = self.speed;
        Ok(frames)
    }

    /// Keeps a sealed turn for resuming.
    fn remember(&mut self, turn: LoggedTurn) {
        self.log_bytes += turn.frame.len();
        self.log.push_back(turn);
        self.trim();
    }

    /// Drops the oldest turns beyond the window's turns or bytes. The newest
    /// turn always stays.
    fn trim(&mut self) {
        while self.log.len() > self.resume_window
            || (self.log_bytes > RESUME_WINDOW_BYTES && self.log.len() > 1)
        {
            let Some(oldest) = self.log.pop_front() else {
                break;
            };
            self.log_bytes -= oldest.frame.len();
            self.log_first_turn += 1;
            self.sealed_before_log = oldest.sealed_through;
        }
    }

    /// The turn feed for a member resuming at `resume` (or from the first
    /// turn). Refused: resuming before the window, after a turn that does
    /// not exist yet, on a history this game never had, or past the point
    /// where the client's history and the current one part.
    fn resume_feed(
        &self,
        room: RoomId,
        settings: RoomSettings,
        rules: &RulesName,
        resume: Option<Resume>,
    ) -> Result<TurnFeed, RequestError> {
        let stream = self.stream_after(resume)?;
        Ok(TurnFeed::Open {
            start: self.turn_start(room, settings, rules, &stream, None),
            backlog: stream.backlog,
        })
    }

    /// The turn feed that starts from an agreed snapshot, and the first
    /// event it carries.
    fn feed_from(
        &self,
        room: RoomId,
        settings: RoomSettings,
        rules: &RulesName,
        agreed: &Agreed,
    ) -> Result<(TurnFeed, u64), RequestError> {
        let stream = self.stream_from_save(&agreed.point)?;
        let next_event = stream.next_event;
        let feed = TurnFeed::Open {
            start: self.turn_start(room, settings, rules, &stream, Some(agreed.offer())),
            backlog: stream.backlog,
        };
        Ok((feed, next_event))
    }

    /// The stream after a save: it must be in the resume window, and the
    /// log must still say what the save says about where it stands.
    fn stream_from_save(&self, point: &SavePoint) -> Result<Stream, RequestError> {
        let stream = self.stream_after(Some(Resume {
            after_turn: point.after_turn,
            history: point.history,
        }))?;
        if stream.next_event != point.event.saturating_add(1)
            || stream.sealed_through != point.sealed_through
        {
            return Err(RequestError::ResumeUnavailable);
        }
        Ok(stream)
    }

    /// Where a stream after `resume` starts, with the turns it starts with.
    fn stream_after(&self, resume: Option<Resume>) -> Result<Stream, RequestError> {
        let from = match resume {
            None => 1,
            Some(resume) => {
                let index = self
                    .histories
                    .iter()
                    .position(|history| history.id == resume.history)
                    .ok_or(RequestError::ResumeUnavailable)?;
                if self
                    .histories
                    .get(index + 1)
                    .is_some_and(|next| resume.after_turn > next.after_turn)
                {
                    return Err(RequestError::ResumeUnavailable);
                }
                resume.after_turn.saturating_add(1)
            }
        };
        if from > self.next_turn || from < self.log_first_turn {
            return Err(RequestError::ResumeUnavailable);
        }
        let skip = usize::try_from(from - self.log_first_turn)
            .map_err(|_| RequestError::ResumeUnavailable)?;
        let backlog: Vec<&LoggedTurn> = self.log.range(skip..).collect();
        // The first event the resumed stream will carry: from the backlog,
        // else from the events waiting for the next turn.
        let next_event = backlog.first().map_or_else(
            || {
                self.pending
                    .first()
                    .map_or(self.next_event, |event| event.seq)
            },
            |turn| turn.first_event,
        );
        // The frontier of the turn before the stream's first.
        let sealed_through = match skip.checked_sub(1) {
            None => self.sealed_before_log,
            Some(before) => self
                .log
                .get(before)
                .map_or(self.sealed_through, |turn| turn.sealed_through),
        };
        Ok(Stream {
            next_turn: from,
            next_event,
            sealed_through,
            backlog: backlog.iter().map(|turn| Arc::clone(&turn.frame)).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ruleset::{NATIVE, RulesChoice};

    fn native() -> RulesName {
        RulesName::new(NATIVE).unwrap()
    }

    /// The native rules, recording what they apply.
    fn recorder_menu() -> RulesMenu {
        RulesMenu::single(RulesChoice {
            name: native(),
            description: Text::new("records").unwrap(),
            factory: Arc::new(|| Box::new(Recorder::default())),
        })
    }

    #[test]
    fn resuming_is_limited_to_the_window() {
        let room = RoomId(FixedBytes([0; 16]));
        let settings = RoomSettings::DEFAULT;
        let mut game = Game::new(settings, 1);
        game.resume_window = 3;
        for frontier in 1..=5 {
            // Five empty turns, numbered 1 to 5; the window keeps 3 to 5.
            game.seal(frontier).unwrap();
        }
        let feed = |after_turn: Option<u64>| {
            let resume = after_turn.map(|after_turn| Resume {
                after_turn,
                history: 1,
            });
            game.resume_feed(room, settings, &native(), resume)
        };
        assert!(matches!(feed(None), Err(RequestError::ResumeUnavailable)));
        assert!(matches!(
            feed(Some(1)),
            Err(RequestError::ResumeUnavailable)
        ));
        let Ok(TurnFeed::Open { start, backlog }) = feed(Some(2)) else {
            panic!("the window starts at turn 3");
        };
        assert_eq!((start.next_turn, backlog.len()), (3, 3));
        let Ok(TurnFeed::Open { start, backlog }) = feed(Some(5)) else {
            panic!("resuming at the head needs no backlog");
        };
        assert_eq!((start.next_turn, backlog.len()), (6, 0));
        assert!(matches!(
            feed(Some(6)),
            Err(RequestError::ResumeUnavailable)
        ));
    }

    #[test]
    fn resuming_never_crosses_into_turns_the_client_did_not_see() {
        let room = RoomId(FixedBytes([0; 16]));
        let settings = RoomSettings::DEFAULT;
        let mut game = Game::new(settings, 1);
        for frontier in 1..=4 {
            game.seal(frontier).unwrap();
        }
        // A crash lost the turns after 4. The restored room numbers its new
        // turns 5 and on too, in history 2.
        game.begin_history(2);
        for frontier in 5..=8 {
            game.seal(frontier).unwrap();
        }
        let feed = |after_turn, history| {
            game.resume_feed(
                room,
                settings,
                &native(),
                Some(Resume {
                    after_turn,
                    history,
                }),
            )
        };
        let Ok(TurnFeed::Open { start, .. }) = feed(4, 1) else {
            panic!("turns 1 to 4 are the same in both histories");
        };
        assert_eq!(start.history, 2, "the stream names the current history");
        assert!(
            matches!(feed(6, 1), Err(RequestError::ResumeUnavailable)),
            "turn 6 of history 1 was lost"
        );
        assert!(feed(6, 2).is_ok());
        assert!(
            matches!(feed(2, 9), Err(RequestError::ResumeUnavailable)),
            "a history the game never had"
        );
    }

    #[test]
    fn the_resume_window_is_bounded_in_bytes_too() {
        let mut game = Game::new(RoomSettings::DEFAULT, 1);
        let big = RESUME_WINDOW_BYTES / 4 + 1;
        for _ in 0..8 {
            game.remember(LoggedTurn {
                first_event: 1,
                sealed_through: 0,
                frame: vec![0; big].into(),
            });
        }
        assert_eq!(game.log.len(), 3, "four would exceed the bytes");
        assert_eq!(game.log_first_turn, 6);
        assert_eq!(game.log_bytes, 3 * big);
    }

    #[test]
    fn slowest_pacer_holds_for_loading_members_and_ignores_catching_up() {
        let pace = |pace| Member {
            pace,
            streaming: true,
            ..test_member()
        };
        assert_eq!(slowest_pacer(&[]), None);
        assert_eq!(
            slowest_pacer(&[pace(Pace::Following(9)), pace(Pace::Following(4))]),
            Some(4)
        );
        assert_eq!(
            slowest_pacer(&[pace(Pace::Following(9)), pace(Pace::Loading)]),
            None
        );
        assert_eq!(
            slowest_pacer(&[pace(Pace::Following(9)), pace(Pace::CatchingUp(Some(1)))]),
            Some(9)
        );
    }

    /// Rules whose state is every command's payload, in order, so a replay
    /// that skips or repeats one shows.
    #[derive(Default)]
    struct Recorder(Vec<u8>);

    impl Ruleset for Recorder {
        fn validate(&self, _player: &PlayerId, _payload: &Payload) -> Result<(), u16> {
            Ok(())
        }

        fn apply(&mut self, event: &Event) {
            if let EventBody::Command { payload, .. } = &event.body {
                self.0.extend_from_slice(payload.as_bytes());
            }
        }

        fn save(&self) -> Option<Vec<u8>> {
            Some(self.0.clone())
        }

        fn restore(&mut self, state: &[u8]) -> Result<(), String> {
            self.0 = state.to_vec();
            Ok(())
        }
    }

    fn player(n: u8) -> PlayerId {
        PlayerId(FixedBytes([n; 32]))
    }

    fn joined(n: u8) -> EventBody {
        EventBody::PlayerJoined {
            player: player(n),
            name: Text::new(format!("p{n}")).unwrap(),
            platform: Platform::current(),
        }
    }

    fn command(n: u8, byte: u8) -> EventBody {
        EventBody::Command {
            player: player(n),
            client_seq: u64::from(byte),
            payload: Payload::new(vec![byte]).unwrap(),
        }
    }

    fn recover_room(path: &Path, dir: &Path) -> Room {
        recover_by(path, dir, &recorder_menu()).unwrap().unwrap()
    }

    fn recover_by(path: &Path, dir: &Path, menu: &RulesMenu) -> Result<Option<Room>, RecoverError> {
        let env = RoomEnv {
            tick: Duration::from_millis(100),
            metrics: Arc::new(Metrics::default()),
            data_dir: Some(dir.to_owned()),
            timeouts: Timeouts {
                stall: Duration::from_secs(20),
                load: Duration::from_secs(300),
                abandoned: Duration::from_secs(600),
            },
            snapshots: None,
            compact_log_at: u64::MAX,
        };
        let key = hmac::Key::new(hmac::HMAC_SHA256, &[0; 32]);
        Room::recover(path, key, menu, env)
    }

    #[test]
    fn a_room_keeps_its_rules_or_is_not_restored() {
        let dir = std::env::temp_dir().join(format!("tpf3mp-rules-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let id = RoomId(FixedBytes([9; 16]));
        let settings = RoomSettings::DEFAULT;
        let mut game = Game::new(settings, 1);
        let mut rules = Recorder::default();
        game.append(joined(1), &mut rules);
        let frames = game.seal(10).unwrap();
        let start = StartRecord {
            version: persist::FORMAT_VERSION,
            history: 1,
            id,
            name: Text::new("strict room").unwrap(),
            rules: RulesName::new("strict").unwrap(),
            owner: player(1),
            max_players: 8,
            settings,
            invite_tag: vec![0; 32],
            password_tag: None,
            members: Vec::new(),
            base: None,
        };
        let mut log = RoomLog::create(&dir, &start).unwrap();
        for frame in &frames {
            log.append(frame).unwrap();
        }
        drop(log);
        let path = RoomLog::path_for(&dir, &id);

        // A server that dropped the rules does not restore the room with
        // other rules.
        assert!(matches!(
            recover_by(&path, &dir, &recorder_menu()),
            Err(RecoverError::UnknownRules(name)) if name == "strict"
        ));
        // One that has them restores it with them, even if they are not its
        // default.
        let menu = recorder_menu().with(RulesChoice {
            name: RulesName::new("strict").unwrap(),
            description: Text::new("strict").unwrap(),
            factory: Arc::new(|| Box::new(Recorder::default())),
        });
        let room = recover_by(&path, &dir, &menu).unwrap().unwrap();
        assert_eq!(room.view().rules.as_str(), "strict");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn running(room: &mut Room) -> &mut Game {
        let Phase::Running(game) = &mut room.phase else {
            panic!("the room runs a game");
        };
        game
    }

    #[tokio::test]
    async fn a_compacted_log_restores_the_same_game() {
        let dir = std::env::temp_dir().join(format!("tpf3mp-compact-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let id = RoomId(FixedBytes([5; 16]));
        let settings = RoomSettings::DEFAULT;

        // Twenty turns with a command each, then one where a third player
        // joins, then two empty ones.
        let mut game = Game::new(settings, 1);
        let mut rules = Recorder::default();
        let mut frames = Vec::new();
        game.append(joined(1), &mut rules);
        game.append(joined(2), &mut rules);
        for turn in 1..=20 {
            game.append(command(1, turn), &mut rules);
            frames.extend(game.seal(u64::from(turn) * 10).unwrap());
        }
        game.append(joined(3), &mut rules);
        game.append(command(3, 21), &mut rules);
        frames.extend(game.seal(210).unwrap());
        frames.extend(game.seal(220).unwrap());
        frames.extend(game.seal(230).unwrap());
        let start = StartRecord {
            version: persist::FORMAT_VERSION,
            history: 1,
            id,
            name: Text::new("compacted").unwrap(),
            rules: native(),
            owner: player(1),
            max_players: 8,
            settings,
            invite_tag: vec![0; 32],
            password_tag: None,
            members: Vec::new(),
            base: None,
        };
        let mut log = RoomLog::create(&dir, &start).unwrap();
        for frame in &frames {
            log.append(frame).unwrap();
        }
        drop(log);
        let path = RoomLog::path_for(&dir, &id);

        // A restored room compacts, keeping its last three turns.
        let mut first = recover_room(&path, &dir);
        let before = std::fs::metadata(&path).unwrap().len();
        let game = running(&mut first);
        game.resume_window = 3;
        game.trim();
        first.compact_at = 0;
        first.start_compaction();
        // A turn sealed while the rewrite runs joins the new log.
        let during = {
            let Phase::Running(game) = &mut first.phase else {
                panic!("the room runs a game");
            };
            game.append(command(1, 77), first.ruleset.as_mut());
            game.seal(235).unwrap()
        };
        first.publish(during.clone());
        let done = compaction_done(&mut first.compaction).await;
        first.finish_compaction(done);
        assert_eq!(
            first
                .metrics
                .logs_compacted
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        let after = std::fs::metadata(&path).unwrap().len();
        assert!(after < before, "the log shrank: {after} < {before}");

        // Play goes on after the compaction: the second player is kicked
        // and a fourth joins.
        let Phase::Running(game) = &mut first.phase else {
            panic!("the room runs a game");
        };
        let kicked = EventBody::PlayerLeft {
            player: player(2),
            kicked: true,
        };
        game.append(kicked, first.ruleset.as_mut());
        game.append(joined(4), first.ruleset.as_mut());
        game.append(command(4, 99), first.ruleset.as_mut());
        let frames_after = game.seal(240).unwrap();
        first.publish(frames_after.clone());
        let game = running(&mut first);
        let expected = (game.next_turn, game.next_event, game.sealed_through);
        let seated = game.seated.clone();
        let histories: Vec<(u64, u64)> = game
            .histories
            .iter()
            .map(|history| (history.id, history.after_turn))
            .collect();
        let rules = first.ruleset.save().unwrap();
        drop(first);

        // A second restart reads the compacted log: the base, the kept
        // turns for resuming only, and the turn after, replayed.
        let mut second = recover_room(&path, &dir);
        assert_eq!(
            second.ruleset.save().unwrap(),
            rules,
            "no command lost or repeated"
        );
        let seats: Vec<PlayerId> = second.members.iter().map(|m| m.player).collect();
        assert_eq!(seats, [player(1), player(3), player(4)]);
        assert!(second.banned.contains(&player(2)));
        let game = running(&mut second);
        assert_eq!(
            (game.next_turn, game.next_event, game.sealed_through),
            expected
        );
        assert_eq!(game.seated, seated);
        assert_eq!((game.log_first_turn, game.sealed_before_log), (21, 200));
        let restored: Vec<(u64, u64)> = game
            .histories
            .iter()
            .map(|history| (history.id, history.after_turn))
            .collect();
        assert_eq!(restored[..histories.len()], histories[..], "and one more");
        assert_eq!(restored.len(), histories.len() + 1);

        // A player who saw turn 22 resumes on the kept turns; one who saw
        // only turn 19 cannot.
        let history = histories.last().unwrap().0;
        let stream = game
            .stream_after(Some(Resume {
                after_turn: 22,
                history,
            }))
            .unwrap();
        // Turn 23, kept; the turn sealed during the rewrite; the one after.
        assert_eq!(stream.backlog.len(), 3);
        assert_eq!(stream.backlog[0], frames[22]);
        assert_eq!(stream.backlog[1], during[0]);
        assert_eq!(stream.backlog[2], frames_after[0]);
        assert!(
            game.stream_after(Some(Resume {
                after_turn: 19,
                history,
            }))
            .is_err()
        );
        drop(second);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_base_that_contradicts_itself_is_refused() {
        let good = Base {
            first_turn: 5,
            first_event: 3,
            sealed_before: 40,
            after_turn: 7,
            next_event: 4,
            sealed_through: 70,
            speed: Speed::NORMAL,
            histories: vec![(1, 0), (2, 6)],
            content: None,
            seated: Vec::new(),
            banned: Vec::new(),
            rules: Vec::new(),
        };
        let fresh = || Game::new(RoomSettings::DEFAULT, 1);
        assert!(fresh().rebase(&good));
        let broken = [
            Base {
                first_turn: 9,
                ..good.clone()
            },
            Base {
                first_event: 5,
                ..good.clone()
            },
            Base {
                sealed_before: 71,
                ..good.clone()
            },
            Base {
                histories: Vec::new(),
                ..good.clone()
            },
            Base {
                histories: vec![(1, 0), (2, 8)],
                ..good.clone()
            },
            Base {
                histories: vec![(1, 3)],
                ..good.clone()
            },
        ];
        for base in &broken {
            assert!(!fresh().rebase(base), "{base:?}");
        }
    }

    /// A base's counters stand where the game stood. One standing near the
    /// end of their range is refused, rather than overflowing on the next
    /// turn read.
    #[test]
    fn an_implausible_base_is_refused() {
        let dir = std::env::temp_dir().join(format!("tpf3mp-poc-base-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let id = RoomId(FixedBytes([6; 16]));
        let base = Base {
            first_turn: u64::MAX,
            first_event: 1,
            sealed_before: 0,
            after_turn: u64::MAX - 1,
            next_event: 1,
            sealed_through: 0,
            speed: Speed::NORMAL,
            histories: vec![(1, 0)],
            content: None,
            seated: vec![(player(1), Text::new("p1").unwrap(), Platform::current())],
            banned: Vec::new(),
            rules: Vec::new(),
        };
        let start = StartRecord {
            version: persist::FORMAT_VERSION,
            history: 1,
            id,
            name: Text::new("crafted").unwrap(),
            rules: native(),
            owner: player(1),
            max_players: 8,
            settings: RoomSettings::DEFAULT,
            invite_tag: vec![0; 32],
            password_tag: None,
            members: Vec::new(),
            base: Some(base),
        };
        let mut log = RoomLog::create(&dir, &start).unwrap();
        let turn = TurnMessage::Turn(Turn {
            number: u64::MAX,
            sealed_through: 0,
            speed: Speed::NORMAL,
            events: Vec::new(),
        });
        log.append(&encode_frame(&turn, TURN_MAX_FRAME).unwrap())
            .unwrap();
        drop(log);
        let path = RoomLog::path_for(&dir, &id);
        let env = RoomEnv {
            tick: Duration::from_millis(100),
            metrics: Arc::new(Metrics::default()),
            data_dir: Some(dir.clone()),
            timeouts: Timeouts {
                stall: Duration::from_secs(20),
                load: Duration::from_secs(300),
                abandoned: Duration::from_secs(600),
            },
            snapshots: None,
            compact_log_at: u64::MAX,
        };
        let key = hmac::Key::new(hmac::HMAC_SHA256, &[0; 32]);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            matches!(
                Room::recover(&path, key, &recorder_menu(), env),
                Err(RecoverError::Base | RecoverError::Continuity(_))
            )
        }));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            matches!(outcome, Ok(true)),
            "an implausible base must be refused; recovery panicked: {}",
            outcome.is_err()
        );
    }

    /// The live room hands ownership to the earliest remaining player when
    /// the owner leaves; an owner who comes back is a player like any other.
    #[test]
    fn a_restored_room_has_the_owner_the_live_room_had() {
        let dir = std::env::temp_dir().join(format!("tpf3mp-owner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let id = RoomId(FixedBytes([7; 16]));
        let mut game = Game::new(RoomSettings::DEFAULT, 1);
        let mut rules = Recorder::default();
        let mut frames = Vec::new();
        for n in 1..=3 {
            game.append(joined(n), &mut rules);
        }
        frames.extend(game.seal(10).unwrap());
        // The owner leaves, and comes back later.
        let left = EventBody::PlayerLeft {
            player: player(1),
            kicked: false,
        };
        game.append(left, &mut rules);
        frames.extend(game.seal(20).unwrap());
        game.append(joined(1), &mut rules);
        frames.extend(game.seal(30).unwrap());
        let start = StartRecord {
            version: persist::FORMAT_VERSION,
            history: 1,
            id,
            name: Text::new("owners").unwrap(),
            rules: native(),
            owner: player(1),
            max_players: 8,
            settings: RoomSettings::DEFAULT,
            invite_tag: vec![0; 32],
            password_tag: None,
            members: Vec::new(),
            base: None,
        };
        let mut log = RoomLog::create(&dir, &start).unwrap();
        for frame in &frames {
            log.append(frame).unwrap();
        }
        drop(log);
        let room = recover_room(&RoomLog::path_for(&dir, &id), &dir);
        assert_eq!(room.owner, player(2), "the second player took over");
        let seats: Vec<PlayerId> = room.members.iter().map(|m| m.player).collect();
        assert_eq!(seats, [player(2), player(3), player(1)]);
        drop(room);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn test_member() -> Member {
        // A link needs a live QUIC connection. Pacing only reads `streaming`,
        // which the room keeps false whenever the link is gone.
        Member {
            player: PlayerId(FixedBytes([0; 32])),
            name: Text::new("t").unwrap(),
            platform: Platform::current(),
            ready: false,
            content: None,
            link: None,
            streaming: false,
            pace: Pace::Loading,
            advanced: Instant::now(),
            intents: TokenBucket::new(1, 1),
            payload_bytes: TokenBucket::new(1, 1),
            chats: TokenBucket::new(1, 1),
            needs: Needs::Nothing,
            offered: None,
            rebased: None,
            stream_from: 0,
        }
    }
}
