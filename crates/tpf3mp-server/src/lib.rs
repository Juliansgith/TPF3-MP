//! The TPF3-MP dedicated server.
//!
//! Milestone M0 implements the connection handshake: the version preamble,
//! then `Hello`, answered by `Welcome` or `Reject`. Rooms, the sequencer and
//! the canonical state machine follow in M1 (see `docs/ARCHITECTURE.md`).

use std::{future::Future, io, net::SocketAddr, sync::Arc, time::Duration};

use quinn::{RecvStream, SendStream};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tpf3mp_net::{
    NetError, ServerIdentity, TlsError, close, read_message, read_preamble, write_message,
    write_preamble,
};
use tpf3mp_proto::{
    CONTROL_MAX_FRAME, Message, PROTOCOL_VERSION, Platform, Reject, RejectReason, SessionId, Text,
    Welcome,
};
use tracing::{debug, info};

/// How long a peer gets to acknowledge a final message (a `Reject`, or the
/// preamble after a version mismatch) before the server closes the connection.
const LINGER: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub identity: ServerIdentity,
    /// Sessions served at once. Clients beyond this receive `Reject(ServerFull)`.
    pub max_sessions: usize,
    /// Time from connecting to a completed handshake. Slower clients are
    /// closed with `HANDSHAKE_TIMEOUT`, so idle sockets cannot pin resources.
    pub handshake_timeout: Duration,
}

impl ServerConfig {
    pub fn new(listen: SocketAddr, identity: ServerIdentity) -> Self {
        Self {
            listen,
            identity,
            max_sessions: 4096,
            handshake_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error(transparent)]
    Tls(#[from] TlsError),
    #[error("cannot open the UDP socket: {0}")]
    Bind(#[from] io::Error),
}

pub struct Server {
    endpoint: quinn::Endpoint,
    sessions: Arc<Semaphore>,
    handshake_timeout: Duration,
}

impl Server {
    pub fn bind(config: ServerConfig) -> Result<Self, ServerError> {
        let quic = tpf3mp_net::server_config(config.identity)?;
        let endpoint = quinn::Endpoint::server(quic, config.listen)?;
        Ok(Self {
            endpoint,
            sessions: Arc::new(Semaphore::new(config.max_sessions)),
            handshake_timeout: config.handshake_timeout,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// Serves connections until `shutdown` completes, then closes every
    /// connection with `SHUTTING_DOWN` and waits for the endpoint to drain.
    pub async fn run(self, shutdown: impl Future<Output = ()>) {
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => break,
                incoming = self.endpoint.accept() => {
                    let Some(incoming) = incoming else { break };
                    tokio::spawn(serve(
                        incoming,
                        Arc::clone(&self.sessions),
                        self.handshake_timeout,
                    ));
                }
            }
        }
        self.endpoint
            .close(close::SHUTTING_DOWN, b"server shutting down");
        self.endpoint.wait_idle().await;
    }
}

/// A client that completed the handshake. Dropping it frees the session slot.
struct Session {
    id: SessionId,
    client_version: Text<64>,
    platform: Platform,
    _slot: OwnedSemaphorePermit,
    _control: (SendStream, RecvStream),
}

/// Why the server ended a connection during the handshake.
#[derive(Debug, Error)]
enum Refusal {
    #[error("client speaks protocol {0}")]
    VersionMismatch(u32),
    #[error("no free session slot")]
    ServerFull,
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
            Self::Violation(_) | Self::UnexpectedMessage => {
                (close::PROTOCOL_VIOLATION, b"protocol violation")
            }
            Self::Lost(_) => return,
        };
        connection.close(code, reason);
    }
}

async fn serve(incoming: quinn::Incoming, sessions: Arc<Semaphore>, handshake_timeout: Duration) {
    let connection = match incoming.await {
        Ok(connection) => connection,
        Err(error) => {
            debug!(%error, "connection attempt failed");
            return;
        }
    };
    // Addresses stay out of the logs; the stable ID correlates log lines.
    let connection_id = connection.stable_id();
    let session =
        match tokio::time::timeout(handshake_timeout, handshake(&connection, &sessions)).await {
            Ok(Ok(session)) => session,
            Ok(Err(refusal)) => {
                debug!(connection = connection_id, %refusal, "handshake refused");
                refusal.close(&connection);
                return;
            }
            Err(_) => {
                debug!(connection = connection_id, "handshake timed out");
                connection.close(close::HANDSHAKE_TIMEOUT, b"handshake timed out");
                return;
            }
        };
    info!(
        connection = connection_id,
        session = %session.id,
        client = %session.client_version,
        platform = ?session.platform,
        "session started"
    );
    let reason = connection.closed().await;
    info!(session = %session.id, %reason, "session ended");
}

async fn handshake(
    connection: &quinn::Connection,
    sessions: &Arc<Semaphore>,
) -> Result<Session, Refusal> {
    let (mut send, mut recv) = connection.accept_bi().await?;
    let client_protocol = read_preamble(&mut recv).await?;
    // Always answer with our version, so the client can say which side is old.
    write_preamble(&mut send, PROTOCOL_VERSION).await?;
    if client_protocol != PROTOCOL_VERSION {
        linger(&mut send).await;
        return Err(Refusal::VersionMismatch(client_protocol));
    }
    let Message::Hello(hello) = read_message(&mut recv, CONTROL_MAX_FRAME).await? else {
        return Err(Refusal::UnexpectedMessage);
    };
    let Ok(slot) = Arc::clone(sessions).try_acquire_owned() else {
        let reject = Message::Reject(Reject {
            reason: RejectReason::ServerFull,
        });
        write_message(&mut send, &reject, CONTROL_MAX_FRAME).await?;
        linger(&mut send).await;
        return Err(Refusal::ServerFull);
    };
    let id = new_session_id();
    let welcome = Message::Welcome(Welcome {
        server_version: server_version(),
        session_id: id,
    });
    write_message(&mut send, &welcome, CONTROL_MAX_FRAME).await?;
    Ok(Session {
        id,
        client_version: hello.client_version,
        platform: hello.platform,
        _slot: slot,
        _control: (send, recv),
    })
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

fn new_session_id() -> SessionId {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).expect("the operating system's random source is available");
    SessionId(bytes)
}

fn server_version() -> Text<64> {
    Text::new(env!("CARGO_PKG_VERSION")).expect("the crate version is short printable text")
}
