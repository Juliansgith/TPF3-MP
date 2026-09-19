//! One client connection: the handshake, then requests and game messages
//! until the connection ends.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use quinn::{RecvStream, SendStream};
use thiserror::Error;
use tokio::{
    sync::{OwnedSemaphorePermit, mpsc},
    time::Instant,
};
use tpf3mp_net::{
    NetError, close, read_message, read_preamble, verify_proof, write_frame, write_message,
    write_preamble,
};
use tpf3mp_proto::{
    CONTROL_MAX_FRAME, ClientMessage, GameMessage, Hello, IntentRejection, MAX_CHECKPOINT_LANES,
    PROTOCOL_VERSION, PlayerId, Reject, RejectReason, Request, RequestError, Response,
    ServerMessage, SessionId, TURN_MAX_FRAME, TurnMessage, TurnStart, Welcome,
};
use tracing::{debug, info};

use crate::{
    Shared,
    admission::{self, Handshake, Origin},
    limit::TokenBucket,
    metrics,
    room::{MemberLink, NewMember, Reply, RoomCommand, RoomHandle, TurnFeed},
};

/// How long a peer gets to acknowledge a final message (a `Reject`, or the
/// preamble after a version mismatch) before the server closes the connection.
const LINGER: Duration = Duration::from_secs(2);
/// Control messages queued for one client before it counts as too slow.
const CONTROL_QUEUE: usize = 256;
/// Turns queued for one client before it counts as too slow: about 100 s of
/// turns at the default tick.
const TURN_QUEUE: usize = 1024;
/// Requests one connection may make per second, and the burst on top. A
/// client makes a handful per game.
const REQUESTS_PER_SECOND: u32 = 10;
const REQUEST_BURST: u32 = 20;
/// Of those, attempts to join a room.
const JOINS_PER_SECOND: u32 = 1;
const JOIN_BURST: u32 = 5;
/// Game messages one connection may send per second: intents, progress
/// reports and checkpoints together. A game sends a few dozen.
const GAME_MESSAGES_PER_SECOND: u32 = 200;
const GAME_MESSAGE_BURST: u32 = 400;

static NEXT_LINK: AtomicU64 = AtomicU64::new(1);

pub(crate) async fn serve(incoming: quinn::Incoming, ticket: Handshake, shared: Arc<Shared>) {
    let connection = match incoming.await {
        Ok(connection) => connection,
        Err(error) => {
            debug!(%error, "connection attempt failed");
            return;
        }
    };
    // Addresses stay out of the logs; the stable ID correlates log lines.
    let connection_id = connection.stable_id();
    let admitted = match tokio::time::timeout(
        shared.handshake_timeout,
        handshake(&connection, ticket, &shared),
    )
    .await
    {
        Ok(Ok(admitted)) => admitted,
        Ok(Err(refusal)) => {
            debug!(connection = connection_id, %refusal, "handshake refused");
            metrics::increment(&shared.metrics.handshakes_refused);
            refusal.close(&connection);
            return;
        }
        Err(_) => {
            debug!(connection = connection_id, "handshake timed out");
            metrics::increment(&shared.metrics.handshakes_refused);
            connection.close(close::HANDSHAKE_TIMEOUT, b"handshake timed out");
            return;
        }
    };
    let Admitted {
        hello,
        session_id,
        slot,
        address_share,
        send,
        recv,
    } = admitted;
    info!(
        connection = connection_id,
        session = %session_id,
        player = %hello.identity,
        client = %hello.client_version,
        platform = ?hello.platform,
        "session started"
    );
    metrics::increment(&shared.metrics.sessions_opened);
    Client::new(connection.clone(), shared, hello)
        .run(send, recv)
        .await;
    // The client may end its control stream and keep the connection; the
    // session is over either way.
    connection.close(close::NORMAL, b"session ended");
    let reason = connection.closed().await;
    info!(session = %session_id, %reason, "session ended");
    drop(address_share);
    drop(slot);
}

struct Admitted {
    hello: Hello,
    session_id: SessionId,
    slot: OwnedSemaphorePermit,
    address_share: admission::Session,
    send: SendStream,
    recv: RecvStream,
}

/// Why the server ended a connection during the handshake.
#[derive(Debug, Error)]
enum Refusal {
    #[error("client speaks protocol {0}")]
    VersionMismatch(u32),
    #[error("no free session slot")]
    ServerFull,
    #[error("the address holds its share of sessions")]
    TooManyConnections,
    #[error("the identity proof did not verify")]
    BadProof,
    #[error("the client broke the protocol: {0}")]
    Violation(#[from] NetError),
    #[error("the first message was not a Hello")]
    UnexpectedMessage,
    #[error("the connection was lost: {0}")]
    Lost(#[from] quinn::ConnectionError),
}

impl Refusal {
    fn close(&self, connection: &quinn::Connection) {
        let (code, reason): (quinn::VarInt, &[u8]) = match self {
            Self::VersionMismatch(_) => (close::VERSION_MISMATCH, b"protocol version mismatch"),
            Self::ServerFull => (close::REJECTED, b"server full"),
            Self::TooManyConnections => (close::REJECTED, b"too many connections"),
            Self::BadProof => (close::REJECTED, b"identity not verified"),
            Self::Violation(_) | Self::UnexpectedMessage => {
                (close::PROTOCOL_VIOLATION, b"protocol violation")
            }
            Self::Lost(_) => return,
        };
        connection.close(code, reason);
    }
}

async fn handshake(
    connection: &quinn::Connection,
    ticket: Handshake,
    shared: &Shared,
) -> Result<Admitted, Refusal> {
    let (mut send, mut recv) = connection.accept_bi().await?;
    let client_protocol = read_preamble(&mut recv).await?;
    // Always answer with our version, so the client can say which side is old.
    write_preamble(&mut send, PROTOCOL_VERSION).await?;
    if client_protocol != PROTOCOL_VERSION {
        linger(&mut send).await;
        return Err(Refusal::VersionMismatch(client_protocol));
    }
    let ClientMessage::Hello(hello) = read_message(&mut recv, CONTROL_MAX_FRAME).await? else {
        return Err(Refusal::UnexpectedMessage);
    };
    if !verify_proof(connection, &hello.identity, &hello.proof) {
        reject(&mut send, RejectReason::BadProof).await?;
        return Err(Refusal::BadProof);
    }
    let Ok(slot) = Arc::clone(&shared.sessions).try_acquire_owned() else {
        reject(&mut send, RejectReason::ServerFull).await?;
        return Err(Refusal::ServerFull);
    };
    let Some(address_share) = ticket.into_session() else {
        reject(&mut send, RejectReason::TooManyConnections).await?;
        return Err(Refusal::TooManyConnections);
    };
    let session_id = SessionId(random());
    let welcome = ServerMessage::Welcome(Welcome {
        server_version: shared.server_version.clone(),
        session_id,
    });
    write_message(&mut send, &welcome, CONTROL_MAX_FRAME).await?;
    Ok(Admitted {
        hello,
        session_id,
        slot,
        address_share,
        send,
        recv,
    })
}

async fn reject(send: &mut SendStream, reason: RejectReason) -> Result<(), NetError> {
    let reject = ServerMessage::Reject(Reject { reason });
    write_message(send, &reject, CONTROL_MAX_FRAME).await?;
    linger(send).await;
    Ok(())
}

/// Finishes the stream and waits, briefly, until the peer has received all of
/// it, so a final message is not lost when the connection closes.
async fn linger(send: &mut SendStream) {
    if send.finish().is_ok() {
        // Timing out only means the peer may miss the reason; the connection
        // closes either way.
        let _ = tokio::time::timeout(LINGER, send.stopped()).await;
    }
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).expect("the operating system's random source is available");
    bytes
}

/// Why the server stopped serving a client after the handshake.
#[derive(Debug, Error)]
enum Violation {
    #[error(transparent)]
    Stream(NetError),
    #[error("a second Hello")]
    SecondHello,
    #[error("a checkpoint with more than {MAX_CHECKPOINT_LANES} lanes")]
    TooManyLanes,
}

/// The server side of one admitted client.
struct Client {
    connection: quinn::Connection,
    shared: Arc<Shared>,
    origin: Origin,
    player: PlayerId,
    hello: Hello,
    link: MemberLink,
    room: Option<RoomHandle>,
    control: Option<mpsc::Receiver<ServerMessage>>,
    turns: Option<mpsc::Receiver<TurnFeed>>,
    requests: TokenBucket,
    joins: TokenBucket,
    game_messages: TokenBucket,
}

impl Client {
    fn new(connection: quinn::Connection, shared: Arc<Shared>, hello: Hello) -> Self {
        let (control_tx, control_rx) = mpsc::channel(CONTROL_QUEUE);
        let (turns_tx, turns_rx) = mpsc::channel(TURN_QUEUE);
        let link = MemberLink {
            id: NEXT_LINK.fetch_add(1, Ordering::Relaxed),
            control: control_tx,
            turns: turns_tx,
            connection: connection.clone(),
        };
        Self {
            origin: Origin::of(connection.remote_address().ip()),
            connection,
            shared,
            player: hello.identity,
            hello,
            link,
            room: None,
            control: Some(control_rx),
            turns: Some(turns_rx),
            requests: TokenBucket::new(REQUESTS_PER_SECOND, REQUEST_BURST),
            joins: TokenBucket::new(JOINS_PER_SECOND, JOIN_BURST),
            game_messages: TokenBucket::new(GAME_MESSAGES_PER_SECOND, GAME_MESSAGE_BURST),
        }
    }

    async fn run(mut self, send: SendStream, mut recv: RecvStream) {
        let (Some(control), Some(turns)) = (self.control.take(), self.turns.take()) else {
            return;
        };
        let control_writer = tokio::spawn(write_control(send, control));
        let turn_writer = tokio::spawn(write_turns(self.connection.clone(), turns));
        if let Err(violation) = self.serve(&mut recv).await {
            debug!(player = %self.player, %violation, "closing a client that broke the protocol");
            metrics::increment(&self.shared.metrics.protocol_violations);
            self.connection
                .close(close::PROTOCOL_VIOLATION, b"protocol violation");
        }
        if let Some(room) = self.room.take() {
            room.notify(RoomCommand::Disconnected {
                player: self.player,
                link: self.link.id,
            });
        }
        control_writer.abort();
        turn_writer.abort();
    }

    async fn serve(&mut self, recv: &mut RecvStream) -> Result<(), Violation> {
        let mut roomless_since = None;
        loop {
            let read = read_message::<ClientMessage>(recv, CONTROL_MAX_FRAME);
            let read = if self.room.is_some() {
                roomless_since = None;
                read.await
            } else {
                // A session outside any room holds a slot for nothing, so it
                // gets a deadline that requests do not extend.
                let since = *roomless_since.get_or_insert_with(Instant::now);
                match tokio::time::timeout_at(since + self.shared.roomless_timeout, read).await {
                    Ok(read) => read,
                    Err(_) => {
                        debug!(player = %self.player, "closing a session idle outside any room");
                        metrics::increment(&self.shared.metrics.idle_sessions_closed);
                        self.connection.close(close::IDLE, b"idle outside a room");
                        return Ok(());
                    }
                }
            };
            let message = match read {
                Ok(message) => message,
                Err(error) if error.is_disconnect() => return Ok(()),
                Err(error) => return Err(Violation::Stream(error)),
            };
            let now = std::time::Instant::now();
            match message {
                ClientMessage::Hello(_) => return Err(Violation::SecondHello),
                ClientMessage::Request { id, request } => {
                    let joining = matches!(request, Request::JoinRoom(_));
                    let result =
                        if !self.requests.take(now, 1) || (joining && !self.joins.take(now, 1)) {
                            Err(RequestError::RateLimited)
                        } else {
                            self.request(request).await
                        };
                    let response = ServerMessage::Response { id, result };
                    if self.link.control.send(response).await.is_err() {
                        return Ok(());
                    }
                }
                ClientMessage::Game(message) => {
                    if self.game_messages.take(now, 1) {
                        self.game(message)?;
                    } else if let GameMessage::Intent { client_seq, .. } = message {
                        self.reject_intent(client_seq, IntentRejection::RateLimited);
                    }
                }
            }
        }
    }

    fn new_member(&self) -> NewMember {
        NewMember {
            player: self.player,
            name: self.hello.name.clone(),
            platform: self.hello.platform,
            link: self.link.clone(),
        }
    }

    async fn request(&mut self, request: Request) -> Result<Response, RequestError> {
        match request {
            Request::CreateRoom(create) => {
                if self.room.is_some() {
                    return Err(RequestError::AlreadyInRoom);
                }
                let share = self
                    .shared
                    .admission
                    .room(self.origin)
                    .ok_or(RequestError::TooManyRooms)?;
                let (handle, invite, room) =
                    self.shared
                        .directory
                        .create(self.new_member(), create, share)?;
                self.room = Some(handle);
                Ok(Response::RoomCreated { invite, room })
            }
            Request::JoinRoom(join) => {
                if self.room.is_some() {
                    return Err(RequestError::AlreadyInRoom);
                }
                let handle = self
                    .shared
                    .directory
                    .get(&join.invite.room)
                    .ok_or(RequestError::BadInvite)?;
                let member = self.new_member();
                let view = handle
                    .request(|reply| RoomCommand::Join {
                        member,
                        token: join.invite.token,
                        password: join.password,
                        resume_after_turn: join.resume_after_turn,
                        reply,
                    })
                    .await?;
                self.room = Some(handle);
                Ok(Response::RoomJoined(view))
            }
            Request::LeaveRoom => {
                let handle = self.room.take().ok_or(RequestError::NotInRoom)?;
                let player = self.player;
                handle
                    .request(|reply| RoomCommand::Leave { player, reply })
                    .await?;
                Ok(Response::Done)
            }
            Request::SetReady(ready) => {
                self.in_room(|player, reply| RoomCommand::SetReady {
                    player,
                    ready,
                    reply,
                })
                .await
            }
            Request::DeclareContent(content) => {
                self.in_room(|player, reply| RoomCommand::DeclareContent {
                    player,
                    content,
                    reply,
                })
                .await
            }
            Request::StartGame => {
                self.in_room(|player, reply| RoomCommand::Start { player, reply })
                    .await
            }
            Request::SetSpeed(speed) => {
                self.in_room(|player, reply| RoomCommand::SetSpeed {
                    player,
                    speed,
                    reply,
                })
                .await
            }
        }
    }

    async fn in_room(
        &mut self,
        make: impl FnOnce(PlayerId, Reply) -> RoomCommand,
    ) -> Result<Response, RequestError> {
        let handle = self.room.as_ref().ok_or(RequestError::NotInRoom)?;
        let player = self.player;
        let result = handle.request(|reply| make(player, reply)).await;
        if result == Err(RequestError::NotInRoom) {
            // The room closed or no longer counts us as a member.
            self.room = None;
        }
        result.map(|()| Response::Done)
    }

    fn game(&mut self, message: GameMessage) -> Result<(), Violation> {
        let Some(room) = &self.room else {
            // Game traffic can still be in flight just after a player left.
            if let GameMessage::Intent { client_seq, .. } = message {
                self.reject_intent(client_seq, IntentRejection::GameNotRunning);
            }
            return Ok(());
        };
        match message {
            GameMessage::Intent {
                client_seq,
                payload,
            } => {
                let queued = room.notify(RoomCommand::Intent {
                    player: self.player,
                    client_seq,
                    payload,
                });
                if !queued {
                    self.reject_intent(client_seq, IntentRejection::RateLimited);
                }
            }
            GameMessage::Progress { step } => {
                room.notify(RoomCommand::Progress {
                    player: self.player,
                    link: self.link.id,
                    step,
                });
            }
            GameMessage::Checkpoint { step, lanes } => {
                if lanes.len() > MAX_CHECKPOINT_LANES {
                    return Err(Violation::TooManyLanes);
                }
                room.notify(RoomCommand::Checkpoint {
                    player: self.player,
                    link: self.link.id,
                    step,
                    lanes,
                });
            }
        }
        Ok(())
    }

    fn reject_intent(&self, client_seq: u64, reason: IntentRejection) {
        let _ = self
            .link
            .control
            .try_send(ServerMessage::IntentRejected { client_seq, reason });
    }
}

async fn write_control(mut send: SendStream, mut messages: mpsc::Receiver<ServerMessage>) {
    while let Some(message) = messages.recv().await {
        if write_message(&mut send, &message, CONTROL_MAX_FRAME)
            .await
            .is_err()
        {
            return;
        }
    }
}

/// Writes the client's turn stream. A new stream replaces the old one, which
/// is finished first so the client reads them in order.
async fn write_turns(connection: quinn::Connection, mut feed: mpsc::Receiver<TurnFeed>) {
    let mut stream: Option<SendStream> = None;
    while let Some(item) = feed.recv().await {
        let written = match item {
            TurnFeed::Open { start, backlog } => {
                if let Some(mut old) = stream.take() {
                    let _ = old.finish();
                }
                match open_turn_stream(&connection, start, &backlog).await {
                    Ok(opened) => {
                        stream = Some(opened);
                        true
                    }
                    Err(()) => false,
                }
            }
            TurnFeed::Frame(frame) => match stream.as_mut() {
                Some(send) => write_frame(send, &frame).await.is_ok(),
                None => true,
            },
            TurnFeed::Close => {
                if let Some(mut old) = stream.take() {
                    let _ = old.finish();
                }
                true
            }
        };
        if !written {
            return;
        }
    }
}

async fn open_turn_stream(
    connection: &quinn::Connection,
    start: TurnStart,
    backlog: &[Arc<[u8]>],
) -> Result<SendStream, ()> {
    let mut send = connection.open_uni().await.map_err(|_| ())?;
    write_preamble(&mut send, PROTOCOL_VERSION)
        .await
        .map_err(|_| ())?;
    write_message(&mut send, &TurnMessage::Start(start), TURN_MAX_FRAME)
        .await
        .map_err(|_| ())?;
    for frame in backlog {
        write_frame(&mut send, frame).await.map_err(|_| ())?;
    }
    Ok(send)
}
