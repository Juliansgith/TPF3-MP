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
use tpf3mp_net::close;
use tpf3mp_proto::{
    ContentFingerprint, Event, EventBody, FRAME_HEADER_LEN, FixedBytes, IntentRejection,
    LaneDigest, MemberView, Payload, Platform, PlayerId, RequestError, Resume, RoomId, RoomPhase,
    RoomSettings, RoomView, ServerMessage, Speed, TURN_MAX_FRAME, Text, Turn, TurnMessage,
    TurnStart, decode_frame, encode_frame,
};
use tracing::{debug, error, info, warn};

use crate::{
    admission::RoomShare,
    directory::Directory,
    limit::TokenBucket,
    metrics::{self, Metrics},
    pacing::Pacer,
    persist::{self, LogError, RoomLog, StartMember, StartRecord},
    ruleset::Ruleset,
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
}

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
    frame: Arc<[u8]>,
}

pub(crate) struct Room {
    id: RoomId,
    name: Text<48>,
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
    /// Since when no member has been connected, while the game runs.
    unattended_since: Option<Instant>,
    password_guard: PasswordGuard,
    /// Players the owner removed, who cannot join again.
    banned: BTreeSet<PlayerId>,
    /// Counts this room against the address that created it until it
    /// closes. Restored rooms have none.
    _share: Option<RoomShare>,
    closed: bool,
}

/// What every room of a server shares.
#[derive(Clone)]
pub(crate) struct RoomEnv {
    pub(crate) tick: Duration,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) data_dir: Option<PathBuf>,
    pub(crate) timeouts: Timeouts,
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
    #[error("the log's file name does not match its room")]
    Misnamed,
    #[error("turn record {0} is unreadable")]
    Turn(usize),
    #[error("turn record {0} breaks the log's continuity")]
    Continuity(usize),
    #[error("turn record {0} seals implausibly far")]
    Frontier(usize),
}

pub(crate) struct RoomSpec {
    pub(crate) id: RoomId,
    pub(crate) name: Text<48>,
    pub(crate) max_players: u8,
    pub(crate) settings: RoomSettings,
    pub(crate) secrets: RoomSecrets,
    pub(crate) ruleset: Box<dyn Ruleset>,
    pub(crate) env: RoomEnv,
    pub(crate) share: RoomShare,
}

impl Room {
    pub(crate) fn new(spec: RoomSpec, owner: NewMember) -> Self {
        let mut room = Self {
            id: spec.id,
            name: spec.name,
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
            unattended_since: None,
            password_guard: PasswordGuard::new(),
            banned: BTreeSet::new(),
            _share: Some(spec.share),
            closed: false,
        };
        room.members.push(Member::new(owner));
        room
    }

    /// Rebuilds a running room from its log: replays every turn through the
    /// ruleset and seats everyone who had not left, disconnected and ready to
    /// resume. A log whose players had all left is deleted and gives `None`.
    pub(crate) fn recover(
        path: &Path,
        key: hmac::Key,
        mut ruleset: Box<dyn Ruleset>,
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
        let expected = start.id.to_string();
        if path.file_stem() != Some(std::ffi::OsStr::new(&expected)) {
            return Err(RecoverError::Misnamed);
        }
        let mut game = Game::new(start.settings, start.history);
        let mut departed = BTreeSet::new();
        let mut index = 0;
        while let Some(frame) = reader.next_record()? {
            let turn = match frame
                .get(FRAME_HEADER_LEN..)
                .map(decode_frame::<TurnMessage>)
            {
                Some(Ok(TurnMessage::Turn(turn))) => turn,
                Some(Ok(TurnMessage::Start(marker))) => {
                    // An earlier recovery began a new history here.
                    if marker.next_turn != game.next_turn || marker.next_event != game.next_event {
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
                ruleset.apply(event);
                match &event.body {
                    EventBody::PlayerLeft { player } => {
                        departed.insert(*player);
                    }
                    EventBody::PlayerJoined { player, .. } => {
                        departed.remove(player);
                    }
                    EventBody::Command { .. } => {}
                }
            }
            game.remember(LoggedTurn {
                first_event,
                frame: Arc::from(frame),
            });
            game.next_turn += 1;
            game.sealed_through = turn.sealed_through;
            game.speed = turn.speed;
            game.announced_speed = turn.speed;
            index += 1;
        }
        game.pacer.resume_at(game.sealed_through);
        let members: Vec<Member> = start
            .members
            .into_iter()
            .filter(|member| !departed.contains(&member.player))
            .map(|member| Member {
                player: member.player,
                name: member.name,
                platform: member.platform,
                ready: true,
                content: member.content,
                link: None,
                streaming: false,
                pace: Pace::CatchingUp(None),
                advanced: Instant::now(),
                intents: TokenBucket::new(INTENTS_PER_SECOND, INTENT_BURST),
                payload_bytes: TokenBucket::new(PAYLOAD_BYTES_PER_SECOND, PAYLOAD_BURST),
            })
            .collect();
        if members.is_empty() {
            reader.delete()?;
            return Ok(None);
        }
        // Only now, with the room rebuilt, may a torn final record be cut.
        let mut log = reader.into_log()?;
        // Turns after the last one logged may have reached clients before
        // the crash, and the room will now number different turns the same
        // way. It begins a new history, so nobody is resumed onto turns that
        // differ from the ones they saw.
        game.begin_history(new_history());
        let marker = TurnMessage::Start(game.turn_start(
            start.id,
            start.settings,
            game.next_turn,
            game.next_event,
        ));
        log.append(&encode_frame(&marker, TURN_MAX_FRAME).map_err(io::Error::other)?)?;
        // The live room hands ownership to the earliest remaining member at
        // each departure, which leaves the same owner as this.
        let owner = if members.iter().any(|member| member.player == start.owner) {
            start.owner
        } else {
            members[0].player
        };
        Ok(Some(Self {
            id: start.id,
            name: start.name,
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
            // Nobody is connected after a restart; the abandon timeout runs
            // from here.
            unattended_since: None,
            password_guard: PasswordGuard::new(),
            banned: BTreeSet::new(),
            _share: None,
            closed: false,
        }))
    }

    pub(crate) fn id(&self) -> RoomId {
        self.id
    }

    pub(crate) fn view(&self) -> RoomView {
        RoomView {
            id: self.id,
            name: self.name.clone(),
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
                reply,
            } => {
                let result = self.join(member, &token, password.as_ref(), resume);
                let joined = result.is_ok();
                let _ = reply.send(result);
                if joined {
                    self.broadcast_view();
                }
            }
            RoomCommand::Leave { player, reply } => {
                let result = self.leave(player);
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
            let feed = match &self.phase {
                Phase::Running(game) => Some(game.resume_feed(self.id, self.settings, resume)?),
                Phase::Lobby => None,
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
            if let Some(feed) = feed {
                member.pace = Pace::CatchingUp(None);
                member.streaming = new.link.turns.try_send(feed).is_ok();
            }
            member.link = Some(new.link);
            return Ok(self.view());
        }
        if matches!(self.phase, Phase::Running(_)) {
            // Joining a running game needs a world snapshot (later milestone).
            return Err(RequestError::GameRunning);
        }
        if self.members.len() >= usize::from(self.max_players) {
            return Err(RequestError::RoomFull);
        }
        self.members.push(Member::new(new));
        Ok(self.view())
    }

    fn leave(&mut self, player: PlayerId) -> Result<(), RequestError> {
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
            game.append(EventBody::PlayerLeft { player }, self.ruleset.as_mut());
        }
        self.after_departure(player);
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
        self.banned.insert(target);
        self.leave(target)
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
            if let Some(log) = self.log.take()
                && let Err(error) = log.delete()
            {
                warn!(room = %self.id, %error, "cannot delete the log of a closed room");
            }
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
        let first = self.members[0].content;
        if first.is_none() || self.members.iter().any(|member| member.content != first) {
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
                },
                self.ruleset.as_mut(),
            );
        }
        let open = game.turn_start(self.id, self.settings, game.next_turn, 1);
        for member in &mut self.members {
            member.pace = Pace::Loading;
            if let Some(link) = &member.link {
                member.streaming = link
                    .turns
                    .try_send(TurnFeed::Open {
                        start: open,
                        backlog: Vec::new(),
                    })
                    .is_ok();
            }
        }
        let history = game.history();
        self.phase = Phase::Running(Box::new(game));
        self.open_log(history);
        info!(room = %self.id, players = self.members.len(), "game started");
        metrics::increment(&self.metrics.games_started);
        Ok(())
    }

    /// Starts the log of a game that has just started. A game whose log
    /// cannot be written keeps running, in memory only.
    fn open_log(&mut self, history: u64) {
        let Some(dir) = &self.data_dir else {
            return;
        };
        let start = StartRecord {
            version: persist::FORMAT_VERSION,
            history,
            id: self.id,
            name: self.name.clone(),
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
        };
        match RoomLog::create(dir, &start) {
            Ok(log) => self.log = Some(log),
            Err(error) => {
                error!(room = %self.id, %error, "cannot create the room log; the game will not survive a restart");
            }
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
        for member in self.members.iter_mut().filter(|m| m.streaming) {
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

    fn announce_divergence(&mut self, step: u64, notices: Vec<(PlayerId, Vec<u16>)>) {
        for (player, lanes) in notices {
            warn!(room = %self.id, %player, step, ?lanes, "replica diverged from the verdict");
            metrics::increment(&self.metrics.divergences);
            if let Some(index) = self.members.iter().position(|m| m.player == player) {
                self.push(index, ServerMessage::Diverged { step, lanes });
            }
        }
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
        if let Some(log) = self.log.take()
            && let Err(error) = log.delete()
        {
            warn!(room = %self.id, %error, "cannot delete the log of an abandoned room");
        }
        self.closed = true;
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
        if let Some(log) = &mut self.log
            && let Err(error) = frames.iter().try_for_each(|frame| log.append(frame))
        {
            error!(room = %self.id, %error, "cannot append to the room log; it stops here");
            self.log = None;
        }
        for frame in frames {
            for index in 0..self.members.len() {
                if self.members[index].streaming {
                    self.send_turn(index, TurnFeed::Frame(Arc::clone(&frame)));
                }
            }
        }
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
        }
    }
}

/// Whether every member pacing the room has reported this round. Members
/// catching up report later and are judged against the verdict then.
fn round_complete(members: &[Member], round: &Round) -> bool {
    members
        .iter()
        .filter(|m| m.streaming && matches!(m.pace, Pace::Loading | Pace::Following(_)))
        .all(|m| round.reports.iter().any(|report| report.player == m.player))
}

fn decide_round(round: &mut Round) -> Vec<(PlayerId, Vec<u16>)> {
    let (verdict, diverged) = verdict::decide(&round.reports);
    round.verdict = Some(verdict);
    diverged
}

/// The lowest progress among members who pace the room, or `None` to hold
/// the clock: while anyone is still loading, or when nobody is following.
/// Only members with an open turn stream count; `streaming` implies a link.
fn slowest_pacer(members: &[Member]) -> Option<u64> {
    let mut slowest: Option<u64> = None;
    for member in members.iter().filter(|m| m.streaming) {
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

    /// The start of a turn stream that continues at `next_turn`.
    fn turn_start(
        &self,
        room: RoomId,
        settings: RoomSettings,
        next_turn: u64,
        next_event: u64,
    ) -> TurnStart {
        TurnStart {
            room,
            next_turn,
            next_event,
            steps_per_second: settings.steps_per_second,
            checkpoint_interval: settings.checkpoint_interval,
            history: self.history(),
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
            let turn = Turn {
                number: self.next_turn,
                sealed_through: if index == last {
                    frontier
                } else {
                    self.sealed_through
                },
                speed: self.speed,
                events,
            };
            let frame: Arc<[u8]> = encode_frame(&TurnMessage::Turn(turn), TURN_MAX_FRAME)?.into();
            self.remember(LoggedTurn {
                first_event,
                frame: Arc::clone(&frame),
            });
            self.next_turn += 1;
            frames.push(frame);
        }
        self.sealed_through = frontier;
        self.announced_speed = self.speed;
        Ok(frames)
    }

    /// Keeps a sealed turn for resuming, dropping the oldest beyond the
    /// window's turns or bytes. The newest turn always stays.
    fn remember(&mut self, turn: LoggedTurn) {
        self.log_bytes += turn.frame.len();
        self.log.push_back(turn);
        while self.log.len() > self.resume_window
            || (self.log_bytes > RESUME_WINDOW_BYTES && self.log.len() > 1)
        {
            let Some(oldest) = self.log.pop_front() else {
                break;
            };
            self.log_bytes -= oldest.frame.len();
            self.log_first_turn += 1;
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
        resume: Option<Resume>,
    ) -> Result<TurnFeed, RequestError> {
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
        Ok(TurnFeed::Open {
            start: self.turn_start(room, settings, from, next_event),
            backlog: backlog.iter().map(|turn| Arc::clone(&turn.frame)).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            game.resume_feed(room, settings, resume)
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
        }
    }
}
