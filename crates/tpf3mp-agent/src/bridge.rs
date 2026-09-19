//! Connects a game, through its hook, to a room.
//!
//! Turns from the server become the messages the hook's
//! [`Gate`](tpf3mp_bridge::Gate) expects, released at the pace
//! [`Playout`] sets. What the hook reports becomes intents, progress and
//! checkpoints for the room.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use thiserror::Error;
use tpf3mp_bridge::{BridgeError, MAX_MESSAGE, ToAgent, ToHook, check_version, decode, encode};
use tpf3mp_net::close;
use tpf3mp_proto::{Invite, JoinRoom, RequestError, Resume, Speed, Text};
use tracing::{debug, info, warn};

use crate::{
    Action, Client, ClientError, ClientEvent, ConnectOptions, Events, FollowError, Playout,
    TurnFollower, connect,
};

/// The agent's end of the link to the hook.
pub trait HookLink: Send {
    /// Sends one encoded message. `Ok(false)` means the hook is not reading
    /// fast enough; the bridge tries again later.
    fn send(&mut self, message: &[u8]) -> Result<bool, BridgeFault>;
    /// Receives one encoded message into `buf`, if one is waiting.
    fn recv(&mut self, buf: &mut Vec<u8>) -> Result<bool, BridgeFault>;
    /// Signals that the agent is alive.
    fn heartbeat(&mut self);
    /// The hook's heartbeat counter, which advances while it is alive.
    fn peer_heartbeat(&self) -> u64;
}

impl HookLink for tpf3mp_ipc::Link {
    fn send(&mut self, message: &[u8]) -> Result<bool, BridgeFault> {
        match tpf3mp_ipc::Link::send(self, message) {
            Ok(()) => Ok(true),
            Err(tpf3mp_ipc::SendError::Full) => Ok(false),
            Err(error) => Err(BridgeFault::Link(error.to_string())),
        }
    }

    fn recv(&mut self, buf: &mut Vec<u8>) -> Result<bool, BridgeFault> {
        buf.resize(MAX_MESSAGE, 0);
        match self.recv_into(buf) {
            Ok(Some(len)) => {
                buf.truncate(len);
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(error) => Err(BridgeFault::Link(error.to_string())),
        }
    }

    fn heartbeat(&mut self) {
        tpf3mp_ipc::Link::heartbeat(self);
    }

    fn peer_heartbeat(&self) -> u64 {
        tpf3mp_ipc::Link::peer_heartbeat(self)
    }
}

#[derive(Debug, Clone)]
pub struct BridgeOptions {
    /// Playout margin: each step plays this long after its seal arrives.
    pub playout_margin: Duration,
    /// How long a late arrival keeps the playout buffer grown.
    pub playout_memory: Duration,
    /// How often the hook's messages are read.
    pub poll: Duration,
    /// A hook whose heartbeat stands still this long while the game runs is
    /// gone.
    pub hook_timeout: Duration,
    /// The same, while the game loads its world, which can take minutes.
    pub load_timeout: Duration,
    /// The least time between two progress reports to the server.
    pub progress_every: Duration,
}

impl Default for BridgeOptions {
    fn default() -> Self {
        Self {
            playout_margin: Duration::from_millis(20),
            playout_memory: Duration::from_secs(10),
            poll: Duration::from_millis(2),
            hook_timeout: Duration::from_secs(60),
            load_timeout: Duration::from_secs(600),
            progress_every: Duration::from_millis(20),
        }
    }
}

#[derive(Debug, Error)]
pub enum BridgeFault {
    #[error("the link to the hook failed: {0}")]
    Link(String),
    #[error(transparent)]
    Message(#[from] BridgeError),
    #[error("the hook sent {0} out of place")]
    Unexpected(&'static str),
    #[error("the hook stopped responding")]
    HookGone,
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("the server broke a turn invariant: {0}")]
    Follow(#[from] FollowError),
    #[error("lost the server and could not rejoin: {0}")]
    Rejoin(String),
}

/// How a bridged session ended.
#[derive(Debug)]
pub enum BridgeEnd {
    /// The connection to the server closed.
    Closed(quinn::ConnectionError),
    /// The room's owner removed this player.
    Kicked,
    /// The client's events ended.
    EventsEnded,
}

/// Couples one game's hook to one client.
pub struct Bridge<L> {
    link: L,
    options: BridgeOptions,
    follower: Option<TurnFollower>,
    playout: Option<Playout>,
    /// Messages for the hook, in order, sent as the hook takes them.
    outbox: VecDeque<ToHook>,
    hook_ready: bool,
    begun: bool,
    /// Whether the game has loaded its world.
    loaded: bool,
    speed: Speed,
    /// Commands the hook has sent; numbers each one's intent.
    commands: u64,
    /// The last step the game ran, or before any, the step before its
    /// first: the progress to report.
    progress: Option<u64>,
    /// The progress last reported on the current connection.
    reported: Option<u64>,
    last_report: Instant,
    wait_until: Option<Instant>,
    hook_beat: (u64, Instant),
    buf: Vec<u8>,
}

impl<L: HookLink> Bridge<L> {
    pub fn new(link: L, options: BridgeOptions) -> Self {
        let now = Instant::now();
        Self {
            hook_beat: (link.peer_heartbeat(), now),
            link,
            options,
            follower: None,
            playout: None,
            outbox: VecDeque::new(),
            hook_ready: false,
            begun: false,
            loaded: false,
            speed: Speed::NORMAL,
            commands: 0,
            progress: None,
            reported: None,
            last_report: now,
            wait_until: None,
            buf: Vec::new(),
        }
    }

    /// Runs the session on one connection until it closes or something
    /// breaks. Waits for the hook to attach first; the room may start before
    /// it. The bridge keeps its state, so after a lost connection it can run
    /// again on a new one that resumes the room (see [`play`]). The hook
    /// is not told the session ended; [`Bridge::end`] does that.
    pub async fn run(
        &mut self,
        client: &Client,
        events: &mut Events,
    ) -> Result<BridgeEnd, BridgeFault> {
        loop {
            let now = Instant::now();
            self.link.heartbeat();
            self.check_hook(now)?;
            self.read_hook(client).await?;
            self.report_progress(client, now).await?;
            if let (Some(follower), Some(playout)) = (&mut self.follower, &mut self.playout) {
                self.wait_until = pump(follower, playout, now, &mut self.outbox);
            }
            self.flush()?;
            let poll_at = now + self.options.poll;
            let wake = self.wait_until.map_or(poll_at, |at| at.min(poll_at));
            tokio::select! {
                event = events.recv() => {
                    let Some(event) = event else {
                        return Ok(BridgeEnd::EventsEnded);
                    };
                    if let Some(end) = self.on_event(event)? {
                        return Ok(end);
                    }
                }
                () = tokio::time::sleep_until(wake.into()) => {}
            }
        }
    }

    /// Tells the hook the session is over, as far as the link still takes
    /// messages.
    pub fn end(&mut self, reason: &str) {
        self.outbox.push_back(ToHook::End {
            reason: Text::lossy(reason),
        });
        let _ = self.flush();
    }

    /// Keeps the hook waiting while the agent has no connection: without a
    /// beat it would give up on the agent.
    pub fn keep_alive(&mut self) {
        self.link.heartbeat();
    }

    /// Where to resume the room after reconnecting, or `None` before the
    /// first turn.
    pub fn resume_point(&self) -> Option<Resume> {
        self.follower.as_ref().and_then(TurnFollower::resume_point)
    }

    fn check_hook(&mut self, now: Instant) -> Result<(), BridgeFault> {
        let beat = self.link.peer_heartbeat();
        let limit = if self.loaded {
            self.options.hook_timeout
        } else {
            self.options.load_timeout
        };
        if beat != self.hook_beat.0 {
            self.hook_beat = (beat, now);
        } else if self.hook_ready && now.saturating_duration_since(self.hook_beat.1) > limit {
            return Err(BridgeFault::HookGone);
        }
        Ok(())
    }

    async fn read_hook(&mut self, client: &Client) -> Result<(), BridgeFault> {
        while self.link.recv(&mut self.buf)? {
            let message: ToAgent = decode(&self.buf)?;
            if !self.hook_ready && !matches!(message, ToAgent::Hello { .. }) {
                return Err(BridgeFault::Unexpected("a message before its hello"));
            }
            match message {
                ToAgent::Hello { version, build } => {
                    if self.hook_ready {
                        return Err(BridgeFault::Unexpected("a second hello"));
                    }
                    check_version(version)?;
                    info!(%build, "the game's hook attached");
                    self.hook_ready = true;
                    // The hello goes first, ahead of a game that began
                    // before the hook attached.
                    self.outbox.push_front(ToHook::Hello {
                        version: tpf3mp_bridge::BRIDGE_VERSION,
                    });
                }
                ToAgent::Loaded { next_step } => {
                    // Loaded counts as progress: the server holds the room
                    // until every member has loaded.
                    let progress = next_step.saturating_sub(1);
                    client.report_progress(progress).await?;
                    self.progress = Some(progress);
                    self.reported = Some(progress);
                    self.loaded = true;
                }
                ToAgent::Command { payload } => {
                    client.send_intent(self.commands, payload).await?;
                    self.commands += 1;
                }
                ToAgent::Ran { step } => self.progress = Some(step),
                ToAgent::Checkpoint { step, lanes } => {
                    client.report_checkpoint(step, lanes).await?;
                }
                ToAgent::Log { message } => info!(hook = %message),
            }
        }
        Ok(())
    }

    /// Reports how far the game has run, at most every `progress_every`,
    /// and at once on a new connection, which knows nothing yet.
    async fn report_progress(&mut self, client: &Client, now: Instant) -> Result<(), BridgeFault> {
        let Some(progress) = self.progress else {
            return Ok(());
        };
        let Some(reported) = self.reported else {
            client.report_progress(progress).await?;
            self.reported = Some(progress);
            self.last_report = now;
            return Ok(());
        };
        let due = now.saturating_duration_since(self.last_report) >= self.options.progress_every;
        if due && progress > reported {
            client.report_progress(progress).await?;
            self.reported = Some(progress);
            self.last_report = now;
        }
        Ok(())
    }

    /// Handles one event from the server. Returns how the session ended, if
    /// it did.
    fn on_event(&mut self, event: ClientEvent) -> Result<Option<BridgeEnd>, BridgeFault> {
        match event {
            ClientEvent::TurnStream(start) => {
                match &mut self.follower {
                    Some(follower) => follower.restart(&start)?,
                    None => self.follower = Some(TurnFollower::new(&start)),
                }
                // A new stream, perhaps on a new connection: tell it where
                // the game stands.
                self.reported = None;
                self.playout = Some(Playout::new(
                    start.steps_per_second,
                    self.options.playout_margin,
                    self.options.playout_memory,
                ));
                if !self.begun {
                    self.begun = true;
                    self.outbox.push_back(ToHook::Begin {
                        steps_per_second: start.steps_per_second,
                        checkpoint_interval: start.checkpoint_interval,
                    });
                }
            }
            ClientEvent::Turn(turn) => {
                let follower = self
                    .follower
                    .as_mut()
                    .ok_or(BridgeFault::Unexpected("a turn before its stream"))?;
                follower.accept(turn)?;
                if let Some(playout) = &mut self.playout {
                    playout.on_turn(follower.sealed_through(), follower.speed(), Instant::now());
                }
                if follower.speed() != self.speed {
                    self.speed = follower.speed();
                    self.outbox.push_back(ToHook::Speed(self.speed));
                }
            }
            ClientEvent::IntentRejected { client_seq, reason } => {
                self.outbox.push_back(ToHook::Refused {
                    command: client_seq,
                    reason,
                });
            }
            ClientEvent::Diverged { step, lanes } => {
                self.outbox.push_back(ToHook::Diverged { step, lanes });
            }
            ClientEvent::RoomUpdate(_) => {}
            ClientEvent::Kicked => return Ok(Some(BridgeEnd::Kicked)),
            ClientEvent::Closed(reason) => return Ok(Some(BridgeEnd::Closed(reason))),
        }
        Ok(None)
    }

    /// Sends what the hook will take now, in order. Nothing goes out before
    /// the hook's hello.
    fn flush(&mut self) -> Result<(), BridgeFault> {
        if !self.hook_ready {
            return Ok(());
        }
        while let Some(message) = self.outbox.front() {
            let bytes = encode(message)?;
            if !self.link.send(&bytes)? {
                break;
            }
            self.outbox.pop_front();
        }
        Ok(())
    }
}

/// Where to find the room again after the connection drops.
#[derive(Debug, Clone)]
pub struct Rejoin {
    pub options: ConnectOptions,
    pub invite: Invite,
    pub password: Option<Text<64>>,
    /// Stop trying after this long without a connection.
    pub give_up_after: Duration,
}

/// Plays the room through the hook until it ends. After a lost connection
/// (a network drop, or the server restarting) it reconnects and resumes
/// the room where the game stands, so the game sees only a pause. Tells
/// the hook when the session is over.
pub async fn play<L: HookLink>(
    bridge: &mut Bridge<L>,
    mut client: Client,
    mut events: Events,
    rejoin: &Rejoin,
) -> Result<BridgeEnd, BridgeFault> {
    loop {
        let reason = match bridge.run(&client, &mut events).await {
            Ok(BridgeEnd::Closed(reason)) if worth_rejoining(&reason) => reason,
            Ok(end) => {
                bridge.end(&format!("{end:?}"));
                return Ok(end);
            }
            Err(fault) => {
                bridge.end(&fault.to_string());
                return Err(fault);
            }
        };
        warn!(%reason, "lost the server; rejoining the room");
        drop(client);
        match rejoin_room(bridge, rejoin).await {
            Ok((new_client, new_events)) => {
                info!("rejoined the room");
                client = new_client;
                events = new_events;
            }
            Err(error) => {
                bridge.end(&error);
                return Err(BridgeFault::Rejoin(error));
            }
        }
    }
}

/// Whether a lost connection is worth rejoining after: not when this side
/// closed it, another connection replaced it, or the protocol broke.
fn worth_rejoining(reason: &quinn::ConnectionError) -> bool {
    match reason {
        quinn::ConnectionError::LocallyClosed | quinn::ConnectionError::VersionMismatch => false,
        quinn::ConnectionError::ApplicationClosed(closed) => ![
            close::REPLACED,
            close::PROTOCOL_VIOLATION,
            close::VERSION_MISMATCH,
            close::IDLE,
        ]
        .contains(&closed.error_code),
        _ => true,
    }
}

/// Reconnects and rejoins, backing off between attempts and beating for
/// the hook all the while.
async fn rejoin_room<L: HookLink>(
    bridge: &mut Bridge<L>,
    rejoin: &Rejoin,
) -> Result<(Client, Events), String> {
    let deadline = Instant::now() + rejoin.give_up_after;
    let mut backoff = Duration::from_millis(250);
    let resume = bridge.resume_point();
    loop {
        let attempt = async {
            let (client, events) = connect(rejoin.options.clone())
                .await
                .map_err(|error| (error.to_string(), false))?;
            client
                .join_room(JoinRoom {
                    invite: rejoin.invite.clone(),
                    password: rejoin.password.clone(),
                    resume,
                })
                .await
                .map_err(|error| {
                    // The room can no longer give these turns back.
                    let hopeless = error == ClientError::Refused(RequestError::ResumeUnavailable);
                    (error.to_string(), hopeless)
                })?;
            Ok::<_, (String, bool)>((client, events))
        };
        let outcome = keeping_alive(bridge, attempt).await;
        match outcome {
            Ok(rejoined) => return Ok(rejoined),
            Err((error, true)) => return Err(error),
            Err((error, false)) if Instant::now() + backoff >= deadline => return Err(error),
            Err((error, false)) => {
                debug!(%error, "rejoining failed; trying again");
                keeping_alive(bridge, tokio::time::sleep(backoff)).await;
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
    }
}

/// Runs `work` while beating for the hook.
async fn keeping_alive<L: HookLink, T>(
    bridge: &mut Bridge<L>,
    work: impl std::future::Future<Output = T>,
) -> T {
    let mut work = std::pin::pin!(work);
    let mut beat = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            done = &mut work => return done,
            _ = beat.tick() => bridge.keep_alive(),
        }
    }
}

/// Moves what the follower allows, at the pace the playout sets, into
/// messages for the hook, in the order its gate requires: every event for
/// step `s` after the release of step `s - 1` and before the release of step
/// `s`. Steps with no events between them go out as one release. Returns
/// when the next step falls due, if one is sealed but not yet due.
pub(crate) fn pump(
    follower: &mut TurnFollower,
    playout: &mut Playout,
    now: Instant,
    out: &mut VecDeque<ToHook>,
) -> Option<Instant> {
    let mut released = None;
    let wait = loop {
        if let Some(step) = follower.next_step() {
            let due = playout.due(step, now).unwrap_or(now);
            if due > now {
                break Some(due);
            }
            playout.played(step, due);
            follower.next_action();
            released = Some(step);
            continue;
        }
        match follower.next_action() {
            Some(Action::Apply(event)) => {
                if let Some(through) = released.take() {
                    out.push_back(ToHook::Release { through });
                }
                out.push_back(ToHook::Apply(event));
            }
            Some(Action::Execute(step)) => released = Some(step),
            None => break None,
        }
    };
    if let Some(through) = released {
        out.push_back(ToHook::Release { through });
    }
    wait
}

#[cfg(test)]
mod tests {
    use tpf3mp_proto::{Event, EventBody, FixedBytes, PlayerId, RoomId, Turn, TurnStart};

    use super::*;

    fn start() -> TurnStart {
        TurnStart {
            room: RoomId(FixedBytes([0; 16])),
            next_turn: 1,
            next_event: 1,
            steps_per_second: 10,
            checkpoint_interval: 10,
            history: 1,
        }
    }

    fn event(seq: u64, step: u64) -> Event {
        Event {
            seq,
            step,
            body: EventBody::PlayerLeft {
                player: PlayerId(FixedBytes([1; 32])),
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

    /// A follower and a playout that has seen `turns` arrive at `now`.
    fn fed(turns: Vec<Turn>, now: Instant) -> (TurnFollower, Playout) {
        let mut follower = TurnFollower::new(&start());
        let mut playout = Playout::new(10, Duration::ZERO, Duration::from_secs(10));
        for turn in turns {
            follower.accept(turn).unwrap();
            playout.on_turn(follower.sealed_through(), follower.speed(), now);
        }
        (follower, playout)
    }

    #[test]
    fn steps_without_events_between_them_go_out_as_one_release() {
        let now = Instant::now();
        let (mut follower, mut playout) = fed(vec![turn(1, 5, vec![])], now);
        let mut out = VecDeque::new();
        // Steps 1 to 5 were sealed at once; reckoned at pace, all are due.
        let wait = pump(&mut follower, &mut playout, now, &mut out);
        assert_eq!(out, [ToHook::Release { through: 5 }]);
        assert_eq!(wait, None, "nothing further is sealed");
    }

    #[test]
    fn an_event_splits_the_releases_around_it() {
        let now = Instant::now();
        let (mut follower, mut playout) = fed(
            vec![turn(1, 2, vec![event(1, 1)]), turn(2, 4, vec![event(2, 3)])],
            now,
        );
        let mut out = VecDeque::new();
        // Both turns arrived together, so steps 3 and 4 play at pace after
        // step 2: a second later, all are due.
        pump(
            &mut follower,
            &mut playout,
            now + Duration::from_secs(1),
            &mut out,
        );
        assert_eq!(
            out,
            [
                ToHook::Apply(event(1, 1)),
                ToHook::Release { through: 2 },
                ToHook::Apply(event(2, 3)),
                ToHook::Release { through: 4 },
            ]
        );
    }

    #[test]
    fn a_step_not_yet_due_waits_and_says_when() {
        let now = Instant::now();
        let (mut follower, mut playout) = fed(vec![turn(1, 1, vec![])], now);
        let mut out = VecDeque::new();
        pump(&mut follower, &mut playout, now, &mut out);
        assert_eq!(out, [ToHook::Release { through: 1 }]);
        // Step 2 arrives now; with steps 100 ms apart, it plays 100 ms after
        // step 1.
        follower.accept(turn(2, 2, vec![])).unwrap();
        playout.on_turn(2, Speed::NORMAL, now);
        out.clear();
        let wait = pump(&mut follower, &mut playout, now, &mut out);
        assert!(out.is_empty());
        assert!(wait.is_some_and(|at| at > now), "{wait:?}");
        let wait = pump(
            &mut follower,
            &mut playout,
            now + Duration::from_millis(100),
            &mut out,
        );
        assert_eq!(out, [ToHook::Release { through: 2 }]);
        assert_eq!(wait, None);
    }
}
