//! The TPF3-MP client: connects to a server, proves the player's identity,
//! and exposes rooms and the turn stream to the game side. The shared-memory
//! link to the in-game hook builds on this (see `docs/ARCHITECTURE.md`).

pub mod bridge;
mod follower;
mod playout;

use std::{
    collections::HashMap,
    fmt, io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

use quinn::{RecvStream, SendStream};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tpf3mp_net::{
    Identity, IdentityError, NetError, ServerTrust, TlsError, client_config, close, read_message,
    read_preamble, write_message, write_preamble,
};
use tpf3mp_proto::{
    CONTROL_MAX_FRAME, ClientMessage, ContentFingerprint, CreateRoom, GameMessage, Hello,
    IntentRejection, Invite, JoinRoom, LaneDigest, PROTOCOL_VERSION, Payload, Platform, PlayerId,
    RejectReason, Request, RequestError, Response, RoomView, ServerMessage, Speed, TURN_MAX_FRAME,
    Text, Turn, TurnMessage, TurnStart, Welcome,
};

pub use follower::{Action, FollowError, TurnFollower};
pub use playout::Playout;

/// How long connecting and the handshake may take, so a server that never
/// answers cannot hang the client.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a request may wait for its response.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Messages queued for the server before senders wait.
const OUTGOING_QUEUE: usize = 256;
/// Events queued for the application before the client stops reading. The
/// server then sees a slow consumer and disconnects rather than buffer.
const EVENT_QUEUE: usize = 1024;
/// Bytes of turns queued for the application before the client stops
/// reading, weighed like [`TurnFollower`]'s backlog. Counting events alone,
/// a hostile server could make the client hold gigabytes.
const EVENT_BYTES: usize = 64 << 20;

#[derive(Debug, Clone)]
pub struct ConnectOptions {
    pub server: SocketAddr,
    /// The name the server's certificate must be valid for.
    pub server_name: String,
    pub trust: ServerTrust,
    pub identity: Arc<Identity>,
    pub name: Text<32>,
    pub client_version: Text<64>,
    /// The protocol version announced in the preamble. Only tests change it.
    pub protocol_version: u32,
}

impl ConnectOptions {
    pub fn new(
        server: SocketAddr,
        server_name: impl Into<String>,
        trust: ServerTrust,
        identity: Arc<Identity>,
        name: Text<32>,
    ) -> Self {
        Self {
            server,
            server_name: server_name.into(),
            trust,
            identity,
            name,
            client_version: Text::new(env!("CARGO_PKG_VERSION"))
                .expect("the crate version is short printable text"),
            protocol_version: PROTOCOL_VERSION,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConnectError {
    #[error(transparent)]
    Tls(#[from] TlsError),
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error("cannot open a UDP socket: {0}")]
    Socket(#[from] io::Error),
    #[error("cannot start the connection: {0}")]
    Start(#[from] quinn::ConnectError),
    #[error("connection failed: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("{}", version_mismatch(.client, .server))]
    VersionMismatch { client: u32, server: u32 },
    #[error("the server declined: {0}")]
    Rejected(RejectReason),
    #[error("the server broke the protocol: {0}")]
    Protocol(#[from] NetError),
    #[error("the server answered the handshake with an unexpected message")]
    UnexpectedMessage,
    #[error("the server did not complete the handshake in time")]
    Timeout,
}

fn version_mismatch(client: &u32, server: &u32) -> String {
    let advice = if server > client {
        "update TPF3-MP"
    } else {
        "the server has not been updated yet"
    };
    format!("this client speaks protocol {client} but the server speaks {server}: {advice}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ClientError {
    #[error("the server refused: {0}")]
    Refused(RequestError),
    #[error("the connection to the server is gone")]
    Disconnected,
    #[error("the server did not answer in time")]
    Timeout,
    #[error("the server answered with an unexpected response")]
    UnexpectedResponse,
}

/// Something the server told this client.
#[derive(Debug)]
pub enum ClientEvent {
    RoomUpdate(RoomView),
    IntentRejected {
        client_seq: u64,
        reason: IntentRejection,
    },
    Diverged {
        step: u64,
        lanes: Vec<u16>,
    },
    /// A turn stream began; turns that follow continue from it.
    TurnStream(TurnStart),
    Turn(Turn),
    /// The connection ended.
    Closed(quinn::ConnectionError),
}

/// An event with the share of the byte budget it holds while queued.
type Queued = (ClientEvent, Option<OwnedSemaphorePermit>);

/// What the server tells this client, in order. Turns count against a byte
/// budget until received, so a client that reads slowly stops reading the
/// network instead of buffering without bound.
#[derive(Debug)]
pub struct Events {
    receiver: mpsc::Receiver<Queued>,
}

impl Events {
    /// The next event, or `None` once the connection is gone and every
    /// event has been received.
    pub async fn recv(&mut self) -> Option<ClientEvent> {
        self.receiver.recv().await.map(|(event, _budget)| event)
    }

    /// The next event if one is queued.
    pub fn try_recv(&mut self) -> Result<ClientEvent, mpsc::error::TryRecvError> {
        self.receiver.try_recv().map(|(event, _budget)| event)
    }

    /// Events queued now.
    pub fn len(&self) -> usize {
        self.receiver.len()
    }

    pub fn is_empty(&self) -> bool {
        self.receiver.is_empty()
    }
}

type Pending = Arc<Mutex<HashMap<u32, oneshot::Sender<Result<Response, RequestError>>>>>;

/// An open, authenticated session with a server.
pub struct Client {
    endpoint: quinn::Endpoint,
    connection: quinn::Connection,
    welcome: Welcome,
    player: PlayerId,
    outgoing: mpsc::Sender<ClientMessage>,
    pending: Pending,
    reader_done: Arc<AtomicBool>,
    next_request: AtomicU32,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("player", &self.player)
            .field("session_id", &self.welcome.session_id)
            .finish_non_exhaustive()
    }
}

/// Connects to a server and completes the handshake. Server messages arrive
/// on the returned receiver.
pub async fn connect(options: ConnectOptions) -> Result<(Client, Events), ConnectError> {
    tokio::time::timeout(CONNECT_TIMEOUT, connect_within(options))
        .await
        .map_err(|_| ConnectError::Timeout)?
}

async fn connect_within(options: ConnectOptions) -> Result<(Client, Events), ConnectError> {
    let local: SocketAddr = if options.server.is_ipv6() {
        (Ipv6Addr::UNSPECIFIED, 0).into()
    } else {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    };
    let mut endpoint = quinn::Endpoint::client(local)?;
    endpoint.set_default_client_config(client_config(options.trust.clone())?);
    let connection = endpoint
        .connect(options.server, &options.server_name)?
        .await?;
    let (welcome, send, recv) = match handshake(&connection, &options).await {
        Ok(opened) => opened,
        Err(error) => {
            let code = match &error {
                ConnectError::VersionMismatch { .. } => close::VERSION_MISMATCH,
                ConnectError::Rejected(_) => close::NORMAL,
                _ => close::PROTOCOL_VIOLATION,
            };
            connection.close(code, b"");
            endpoint.wait_idle().await;
            return Err(error);
        }
    };

    let (outgoing, outgoing_rx) = mpsc::channel(OUTGOING_QUEUE);
    let (events, events_rx) = mpsc::channel(EVENT_QUEUE);
    let pending = Pending::default();
    let reader_done = Arc::new(AtomicBool::new(false));
    tokio::spawn(write_control(send, outgoing_rx));
    tokio::spawn(read_control(
        recv,
        connection.clone(),
        Arc::clone(&pending),
        Arc::clone(&reader_done),
        events.clone(),
    ));
    tokio::spawn(read_turns(
        connection.clone(),
        events.clone(),
        Arc::new(Semaphore::new(EVENT_BYTES)),
    ));
    tokio::spawn({
        let connection = connection.clone();
        async move {
            let reason = connection.closed().await;
            let _ = events.send((ClientEvent::Closed(reason), None)).await;
        }
    });

    Ok((
        Client {
            endpoint,
            connection,
            welcome,
            player: options.identity.player(),
            outgoing,
            pending,
            reader_done,
            next_request: AtomicU32::new(1),
        },
        Events {
            receiver: events_rx,
        },
    ))
}

async fn handshake(
    connection: &quinn::Connection,
    options: &ConnectOptions,
) -> Result<(Welcome, SendStream, RecvStream), ConnectError> {
    let failed = |error| stream_failure(connection, error);
    let (mut send, mut recv) = connection.open_bi().await?;
    write_preamble(&mut send, options.protocol_version)
        .await
        .map_err(failed)?;
    let server_protocol = read_preamble(&mut recv).await.map_err(failed)?;
    if server_protocol != options.protocol_version {
        return Err(ConnectError::VersionMismatch {
            client: options.protocol_version,
            server: server_protocol,
        });
    }
    let hello = ClientMessage::Hello(Hello {
        client_version: options.client_version.clone(),
        platform: Platform::current(),
        name: options.name.clone(),
        identity: options.identity.player(),
        proof: options.identity.prove(connection)?,
    });
    write_message(&mut send, &hello, CONTROL_MAX_FRAME)
        .await
        .map_err(failed)?;
    match read_message::<ServerMessage>(&mut recv, CONTROL_MAX_FRAME)
        .await
        .map_err(failed)?
    {
        ServerMessage::Welcome(welcome) => Ok((welcome, send, recv)),
        ServerMessage::Reject(reject) => Err(ConnectError::Rejected(reject.reason)),
        _ => Err(ConnectError::UnexpectedMessage),
    }
}

/// A stream error caused by the server closing the connection is reported as
/// the server's close reason, which says why.
fn stream_failure(connection: &quinn::Connection, error: NetError) -> ConnectError {
    match connection.close_reason() {
        Some(reason) => ConnectError::Connection(reason),
        None => ConnectError::Protocol(error),
    }
}

impl Client {
    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    pub fn player(&self) -> PlayerId {
        self.player
    }

    pub fn rtt(&self) -> Duration {
        self.connection.rtt()
    }

    /// Sends a request and waits for its response.
    pub async fn request(&self, request: Request) -> Result<Response, ClientError> {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = oneshot::channel();
        self.pending_map().insert(id, reply);
        // The reader marks itself done before failing every pending request,
        // so a request registered after that is caught here.
        if self.reader_done.load(Ordering::SeqCst) {
            self.pending_map().remove(&id);
            return Err(ClientError::Disconnected);
        }
        if self
            .outgoing
            .send(ClientMessage::Request { id, request })
            .await
            .is_err()
        {
            self.pending_map().remove(&id);
            return Err(ClientError::Disconnected);
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, answer).await {
            Ok(Ok(Ok(response))) => Ok(response),
            Ok(Ok(Err(error))) => Err(ClientError::Refused(error)),
            Ok(Err(_)) => Err(ClientError::Disconnected),
            Err(_) => {
                self.pending_map().remove(&id);
                Err(ClientError::Timeout)
            }
        }
    }

    pub async fn create_room(&self, create: CreateRoom) -> Result<(Invite, RoomView), ClientError> {
        match self.request(Request::CreateRoom(create)).await? {
            Response::RoomCreated { invite, room } => Ok((invite, room)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn join_room(&self, join: JoinRoom) -> Result<RoomView, ClientError> {
        match self.request(Request::JoinRoom(join)).await? {
            Response::RoomJoined(room) => Ok(room),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn leave_room(&self) -> Result<(), ClientError> {
        self.done(Request::LeaveRoom).await
    }

    pub async fn set_ready(&self, ready: bool) -> Result<(), ClientError> {
        self.done(Request::SetReady(ready)).await
    }

    pub async fn declare_content(&self, content: ContentFingerprint) -> Result<(), ClientError> {
        self.done(Request::DeclareContent(content)).await
    }

    pub async fn start_game(&self) -> Result<(), ClientError> {
        self.done(Request::StartGame).await
    }

    pub async fn set_speed(&self, speed: Speed) -> Result<(), ClientError> {
        self.done(Request::SetSpeed(speed)).await
    }

    async fn done(&self, request: Request) -> Result<(), ClientError> {
        match self.request(request).await? {
            Response::Done => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn send_intent(&self, client_seq: u64, payload: Payload) -> Result<(), ClientError> {
        self.send(GameMessage::Intent {
            client_seq,
            payload,
        })
        .await
    }

    pub async fn report_progress(&self, step: u64) -> Result<(), ClientError> {
        self.send(GameMessage::Progress { step }).await
    }

    pub async fn report_checkpoint(
        &self,
        step: u64,
        lanes: Vec<LaneDigest>,
    ) -> Result<(), ClientError> {
        self.send(GameMessage::Checkpoint { step, lanes }).await
    }

    async fn send(&self, message: GameMessage) -> Result<(), ClientError> {
        self.outgoing
            .send(ClientMessage::Game(message))
            .await
            .map_err(|_| ClientError::Disconnected)
    }

    /// Ends the session and waits until the server has been told.
    pub async fn close(self) {
        self.connection.close(close::NORMAL, b"client leaving");
        self.endpoint.wait_idle().await;
    }

    fn pending_map(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<u32, oneshot::Sender<Result<Response, RequestError>>>>
    {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Background tasks hold the connection; without this, a dropped
        // client would linger until the idle timeout.
        self.connection.close(close::NORMAL, b"client dropped");
    }
}

async fn write_control(mut send: SendStream, mut outgoing: mpsc::Receiver<ClientMessage>) {
    while let Some(message) = outgoing.recv().await {
        if write_message(&mut send, &message, CONTROL_MAX_FRAME)
            .await
            .is_err()
        {
            return;
        }
    }
}

async fn read_control(
    mut recv: RecvStream,
    connection: quinn::Connection,
    pending: Pending,
    done: Arc<AtomicBool>,
    events: mpsc::Sender<Queued>,
) {
    loop {
        let message = match read_message::<ServerMessage>(&mut recv, CONTROL_MAX_FRAME).await {
            Ok(message) => message,
            Err(error) => {
                if !error.is_disconnect() {
                    connection.close(close::PROTOCOL_VIOLATION, b"malformed control message");
                }
                break;
            }
        };
        let event = match message {
            ServerMessage::Response { id, result } => {
                let reply = pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id);
                if let Some(reply) = reply {
                    let _ = reply.send(result);
                }
                continue;
            }
            ServerMessage::RoomUpdate(room) => ClientEvent::RoomUpdate(room),
            ServerMessage::IntentRejected { client_seq, reason } => {
                ClientEvent::IntentRejected { client_seq, reason }
            }
            ServerMessage::Diverged { step, lanes } => ClientEvent::Diverged { step, lanes },
            ServerMessage::Welcome(_) | ServerMessage::Reject(_) => {
                connection.close(close::PROTOCOL_VIOLATION, b"unexpected handshake message");
                break;
            }
        };
        if events.send((event, None)).await.is_err() {
            break;
        }
    }
    done.store(true, Ordering::SeqCst);
    // Dropping the reply senders fails every waiting request.
    pending
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
}

/// Reads turn streams one after another, so a stream that replaces an older
/// one is only read once the older one has ended.
async fn read_turns(
    connection: quinn::Connection,
    events: mpsc::Sender<Queued>,
    budget: Arc<Semaphore>,
) {
    while let Ok(mut recv) = connection.accept_uni().await {
        match read_turn_stream(&mut recv, &events, &budget).await {
            Ok(()) => {}
            Err(TurnStreamError::Violation) => {
                connection.close(close::PROTOCOL_VIOLATION, b"malformed turn stream");
                return;
            }
            Err(TurnStreamError::Receiver) => return,
        }
    }
}

enum TurnStreamError {
    Violation,
    Receiver,
}

async fn read_turn_stream(
    recv: &mut RecvStream,
    events: &mpsc::Sender<Queued>,
    budget: &Arc<Semaphore>,
) -> Result<(), TurnStreamError> {
    match read_preamble(recv).await {
        Ok(PROTOCOL_VERSION) => {}
        Ok(_) => return Err(TurnStreamError::Violation),
        Err(error) if error.is_disconnect() => return Ok(()),
        Err(_) => return Err(TurnStreamError::Violation),
    }
    let start = match read_message::<TurnMessage>(recv, TURN_MAX_FRAME).await {
        Ok(TurnMessage::Start(start)) => start,
        Ok(TurnMessage::Turn(_)) => return Err(TurnStreamError::Violation),
        Err(error) if error.is_disconnect() => return Ok(()),
        Err(_) => return Err(TurnStreamError::Violation),
    };
    events
        .send((ClientEvent::TurnStream(start), None))
        .await
        .map_err(|_| TurnStreamError::Receiver)?;
    loop {
        match read_message::<TurnMessage>(recv, TURN_MAX_FRAME).await {
            Ok(TurnMessage::Turn(turn)) => {
                // Waits while the application holds the whole budget, which
                // stops reading and lets QUIC flow control reach the server.
                let weight = u32::try_from(follower::turn_weight(&turn).min(EVENT_BYTES))
                    .unwrap_or(u32::MAX);
                let share = Arc::clone(budget)
                    .acquire_many_owned(weight)
                    .await
                    .map_err(|_| TurnStreamError::Receiver)?;
                events
                    .send((ClientEvent::Turn(turn), Some(share)))
                    .await
                    .map_err(|_| TurnStreamError::Receiver)?;
            }
            Ok(TurnMessage::Start(_)) => return Err(TurnStreamError::Violation),
            // The server finished the stream, or the connection ended.
            Err(error) if error.is_disconnect() => return Ok(()),
            Err(_) => return Err(TurnStreamError::Violation),
        }
    }
}
