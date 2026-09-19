//! Shared helpers for the server's end-to-end tests: a real server and real
//! clients over loopback QUIC.

// Each test binary uses a different subset of these helpers.
#![allow(dead_code, clippy::unwrap_used)]

use std::{net::SocketAddr, sync::Arc, time::Duration};

use tokio::{sync::oneshot, task::JoinHandle};
use tpf3mp_agent::{Action, Client, ClientEvent, ConnectOptions, Events, TurnFollower, connect};
use tpf3mp_net::{Identity, ServerIdentity, ServerTrust, client_config};
use tpf3mp_proto::{
    ContentFingerprint, CreateRoom, Event, FixedBytes, IntentRejection, Invite, JoinRoom,
    LaneDigest, PlayerId, RoomSettings, RoomView, Text, TurnStart,
};
use tpf3mp_server::{Server, ServerConfig, ServerStats};

/// Longest wait for anything a test expects to happen.
pub const WAIT: Duration = Duration::from_secs(10);

/// Settings that move fast enough for tests.
pub const FAST: RoomSettings = RoomSettings {
    steps_per_second: 50,
    input_delay_ms: 40,
    checkpoint_interval: 10,
};

pub struct RunningServer {
    pub address: SocketAddr,
    /// Where the server accepts tunnels, if it was configured to.
    pub tunnel: Option<SocketAddr>,
    pub trust: ServerTrust,
    pub stats: ServerStats,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl RunningServer {
    pub async fn start(configure: impl FnOnce(&mut ServerConfig)) -> Self {
        let identity = ServerIdentity::self_signed(&["localhost"]).unwrap();
        let trust = ServerTrust::Pinned(identity.leaf().clone());
        let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), identity);
        config.tick = Duration::from_millis(20);
        // Every test client connects from loopback, one address.
        config.max_sessions_per_address = 1000;
        config.max_handshakes_per_address = 1000;
        config.max_rooms_per_address = 1000;
        configure(&mut config);
        let server = Server::bind(config).unwrap();
        let address = server.local_addr().unwrap();
        let tunnel = server.tunnel_addr();
        let stats = server.stats();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(server.run(async {
            let _ = stopped.await;
        }));
        Self {
            address,
            tunnel,
            trust,
            stats,
            stop: Some(stop),
            task,
        }
    }

    pub fn options(&self, identity: Arc<Identity>, name: &str) -> ConnectOptions {
        ConnectOptions::new(
            self.address,
            "localhost",
            self.trust.clone(),
            identity,
            Text::new(name).unwrap(),
        )
    }

    pub async fn client(&self, name: &str) -> TestClient {
        self.client_as(new_identity(), name).await
    }

    pub async fn client_as(&self, identity: Arc<Identity>, name: &str) -> TestClient {
        let (client, events) = connect(self.options(Arc::clone(&identity), name))
            .await
            .unwrap();
        TestClient {
            client,
            events,
            identity,
        }
    }

    /// A bare QUIC connection that speaks whatever the test sends.
    pub async fn raw_connection(&self) -> (quinn::Endpoint, quinn::Connection) {
        let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        endpoint.set_default_client_config(client_config(self.trust.clone()).unwrap());
        let connection = endpoint
            .connect(self.address, "localhost")
            .unwrap()
            .await
            .unwrap();
        (endpoint, connection)
    }

    pub async fn shut_down(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        tokio::time::timeout(WAIT, &mut self.task)
            .await
            .expect("the server drains in time")
            .unwrap();
    }

    /// Waits until the server hosts `rooms` rooms.
    pub async fn wait_for_rooms(&self, rooms: usize) {
        tokio::time::timeout(WAIT, async {
            while self.stats.rooms() != rooms {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the room count settles");
    }
}

pub fn new_identity() -> Arc<Identity> {
    Arc::new(Identity::generate().unwrap().0)
}

pub fn content(value: u8) -> ContentFingerprint {
    ContentFingerprint(FixedBytes([value; 32]))
}

pub fn room(name: &str, settings: RoomSettings) -> CreateRoom {
    CreateRoom {
        name: Text::new(name).unwrap(),
        max_players: 8,
        password: None,
        settings,
    }
}

pub fn join(invite: &Invite) -> JoinRoom {
    JoinRoom {
        invite: invite.clone(),
        password: None,
        resume: None,
        content: None,
    }
}

pub fn application_close_code(error: &quinn::ConnectionError) -> Option<quinn::VarInt> {
    match error {
        quinn::ConnectionError::ApplicationClosed(close) => Some(close.error_code),
        _ => None,
    }
}

pub struct TestClient {
    pub client: Client,
    pub events: Events,
    pub identity: Arc<Identity>,
}

impl TestClient {
    /// Waits for the first event `pick` accepts, discarding the others. Only
    /// for tests that do not follow the turn stream.
    pub async fn wait_for<T>(&mut self, mut pick: impl FnMut(ClientEvent) -> Option<T>) -> T {
        tokio::time::timeout(WAIT, async {
            loop {
                let event = self.events.recv().await.expect("event channel closed");
                if let Some(found) = pick(event) {
                    return found;
                }
            }
        })
        .await
        .expect("timed out waiting for an event")
    }

    pub async fn room_where(&mut self, mut accept: impl FnMut(&RoomView) -> bool) -> RoomView {
        self.wait_for(|event| match event {
            ClientEvent::RoomUpdate(room) if accept(&room) => Some(room),
            _ => None,
        })
        .await
    }

    pub async fn closed(&mut self) -> quinn::ConnectionError {
        self.wait_for(|event| match event {
            ClientEvent::Closed(reason) => Some(reason),
            _ => None,
        })
        .await
    }
}

/// Computes checkpoint lanes for a step.
pub type Lanes = Box<dyn FnMut(u64) -> Vec<LaneDigest> + Send>;

/// Plays a room the way a game would: follows the turn stream through
/// [`TurnFollower`], applies events, executes steps, and reports progress
/// and checkpoints. Every turn is checked against the protocol invariants.
pub struct Player {
    pub test: TestClient,
    pub follower: Option<TurnFollower>,
    pub start: Option<TurnStart>,
    pub applied: Vec<Event>,
    pub executed: u64,
    pub rejections: Vec<(u64, IntentRejection)>,
    pub diverged: Vec<(u64, Vec<u16>)>,
    pub room: Option<RoomView>,
    pub closed: bool,
    pub kicked: bool,
    /// Chat heard, in order: who, and what.
    pub chat: Vec<(PlayerId, String)>,
    /// Whether this player reports progress; a player that never does holds
    /// the room at the load gate.
    pub reports_progress: bool,
    pub lanes: Option<Lanes>,
}

impl Player {
    pub fn new(test: TestClient) -> Self {
        Self {
            test,
            follower: None,
            start: None,
            applied: Vec::new(),
            executed: 0,
            rejections: Vec::new(),
            diverged: Vec::new(),
            room: None,
            closed: false,
            kicked: false,
            chat: Vec::new(),
            reports_progress: true,
            lanes: None,
        }
    }

    pub fn client(&self) -> &Client {
        &self.test.client
    }

    /// Handles events until `done` holds.
    pub async fn play_until(&mut self, mut done: impl FnMut(&Player) -> bool) {
        tokio::time::timeout(WAIT, async {
            while !done(self) {
                let event = self.test.events.recv().await.expect("event channel closed");
                self.handle(event).await;
            }
        })
        .await
        .expect("timed out playing");
    }

    /// Handles whatever arrives during `duration`.
    pub async fn play_for(&mut self, duration: Duration) {
        let deadline = tokio::time::Instant::now() + duration;
        while let Ok(Some(event)) = tokio::time::timeout_at(deadline, self.test.events.recv()).await
        {
            self.handle(event).await;
        }
    }

    async fn handle(&mut self, event: ClientEvent) {
        match event {
            ClientEvent::TurnStream(start) => {
                match &mut self.follower {
                    Some(follower) => follower
                        .restart(&start)
                        .expect("a resumed stream continues exactly"),
                    None => self.follower = Some(TurnFollower::new(&start)),
                }
                self.start = Some(start);
                if self.reports_progress {
                    // Loaded (or resumed) at the executed step.
                    let _ = self.test.client.report_progress(self.executed).await;
                }
            }
            ClientEvent::Turn(turn) => {
                let follower = self.follower.as_mut().expect("a turn before its stream");
                follower
                    .accept(turn)
                    .expect("the server broke a turn invariant");
                self.drain().await;
            }
            ClientEvent::RoomUpdate(room) => self.room = Some(room),
            ClientEvent::IntentRejected { client_seq, reason } => {
                self.rejections.push((client_seq, reason));
            }
            ClientEvent::Diverged { step, lanes } => self.diverged.push((step, lanes)),
            ClientEvent::Upload { .. } => {}
            ClientEvent::Chat { from, text } => self.chat.push((from, text.as_str().to_owned())),
            ClientEvent::Kicked => self.kicked = true,
            ClientEvent::Closed(_) => self.closed = true,
        }
    }

    async fn drain(&mut self) {
        let interval = self
            .start
            .map_or(u64::MAX, |start| u64::from(start.checkpoint_interval));
        let mut checkpoints = Vec::new();
        let follower = self.follower.as_mut().expect("following");
        while let Some(action) = follower.next_action() {
            match action {
                Action::Apply(event) => self.applied.push(event),
                Action::Execute(step) => {
                    self.executed = step;
                    if step.is_multiple_of(interval)
                        && let Some(lanes) = self.lanes.as_mut()
                    {
                        checkpoints.push((step, lanes(step)));
                    }
                }
            }
        }
        for (step, lanes) in checkpoints {
            let _ = self.test.client.report_checkpoint(step, lanes).await;
        }
        if self.reports_progress {
            let _ = self.test.client.report_progress(self.executed).await;
        }
    }
}

/// Creates a room owned by the first player and seats the rest, all ready
/// with the same content. Returns the invite.
pub async fn seat(players: &mut [&mut TestClient], settings: RoomSettings) -> Invite {
    let (owner, rest) = players.split_first_mut().expect("at least one player");
    let (invite, _) = owner
        .client
        .create_room(room("table", settings))
        .await
        .unwrap();
    for player in rest.iter_mut() {
        player.client.join_room(join(&invite)).await.unwrap();
    }
    for player in players.iter() {
        player.client.declare_content(content(1)).await.unwrap();
        player.client.set_ready(true).await.unwrap();
    }
    invite
}
