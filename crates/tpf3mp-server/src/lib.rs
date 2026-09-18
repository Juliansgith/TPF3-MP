//! The TPF3-MP dedicated server: handshake and identity, rooms, and the
//! turn sequencer. The protocol is specified in `docs/PROTOCOL.md`.

mod admin;
mod connection;
mod directory;
mod metrics;
mod pacing;
mod room;
mod ruleset;
mod verdict;

use std::{fmt, future::Future, io, net::SocketAddr, sync::Arc, time::Duration};

use thiserror::Error;
use tokio::sync::Semaphore;
use tpf3mp_net::{ServerIdentity, TlsError, close};
use tpf3mp_proto::Text;

pub use crate::{
    admin::serve_admin,
    ruleset::{AcceptAll, Ruleset, RulesetFactory},
};
use crate::{
    directory::Directory,
    metrics::{Gauges, Metrics},
};

pub struct ServerConfig {
    pub listen: SocketAddr,
    pub identity: ServerIdentity,
    /// Sessions served at once. Clients beyond this receive `Reject(ServerFull)`.
    pub max_sessions: usize,
    /// Time from connecting to a completed handshake. Slower clients are
    /// closed with `HANDSHAKE_TIMEOUT`, so idle sockets cannot pin resources.
    pub handshake_timeout: Duration,
    /// Rooms hosted at once.
    pub max_rooms: usize,
    /// Key for the HMAC tags of invites and room passwords. An invite stays
    /// valid only while the server keeps this key.
    pub secret: [u8; 32],
    /// Creates the canonical rules of each new room.
    pub ruleset: RulesetFactory,
    /// Interval at which running rooms seal turns.
    pub tick: Duration,
}

impl ServerConfig {
    /// A configuration with defaults and a fresh random secret.
    pub fn new(listen: SocketAddr, identity: ServerIdentity) -> Self {
        let mut secret = [0; 32];
        getrandom::fill(&mut secret).expect("the operating system's random source is available");
        Self {
            listen,
            identity,
            max_sessions: 4096,
            handshake_timeout: Duration::from_secs(10),
            max_rooms: 10_000,
            secret,
            ruleset: Arc::new(|| Box::new(AcceptAll)),
            tick: Duration::from_millis(100),
        }
    }
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The secret stays out of logs.
        f.debug_struct("ServerConfig")
            .field("listen", &self.listen)
            .field("max_sessions", &self.max_sessions)
            .field("handshake_timeout", &self.handshake_timeout)
            .field("max_rooms", &self.max_rooms)
            .field("tick", &self.tick)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error(transparent)]
    Tls(#[from] TlsError),
    #[error("cannot open the UDP socket: {0}")]
    Bind(#[from] io::Error),
}

/// State shared by every connection.
pub(crate) struct Shared {
    pub(crate) sessions: Arc<Semaphore>,
    pub(crate) max_sessions: usize,
    pub(crate) handshake_timeout: Duration,
    pub(crate) directory: Arc<Directory>,
    pub(crate) server_version: Text<64>,
    pub(crate) metrics: Arc<Metrics>,
}

pub struct Server {
    endpoint: quinn::Endpoint,
    shared: Arc<Shared>,
}

impl Server {
    pub fn bind(config: ServerConfig) -> Result<Self, ServerError> {
        let quic = tpf3mp_net::server_config(config.identity)?;
        let endpoint = quinn::Endpoint::server(quic, config.listen)?;
        let metrics = Arc::new(Metrics::default());
        let shared = Arc::new(Shared {
            sessions: Arc::new(Semaphore::new(config.max_sessions)),
            max_sessions: config.max_sessions,
            handshake_timeout: config.handshake_timeout,
            directory: Arc::new(Directory::new(
                &config.secret,
                config.max_rooms,
                config.ruleset,
                config.tick,
                Arc::clone(&metrics),
            )),
            server_version: Text::new(env!("CARGO_PKG_VERSION"))
                .expect("the crate version is short printable text"),
            metrics,
        });
        Ok(Self { endpoint, shared })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// A handle for observing the server while it runs.
    pub fn stats(&self) -> ServerStats {
        ServerStats {
            shared: Arc::clone(&self.shared),
        }
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
                    tokio::spawn(connection::serve(incoming, Arc::clone(&self.shared)));
                }
            }
        }
        self.endpoint
            .close(close::SHUTTING_DOWN, b"server shutting down");
        self.endpoint.wait_idle().await;
    }
}

#[derive(Clone)]
pub struct ServerStats {
    shared: Arc<Shared>,
}

impl ServerStats {
    pub fn rooms(&self) -> usize {
        self.shared.directory.len()
    }

    pub fn sessions(&self) -> usize {
        self.shared.max_sessions - self.shared.sessions.available_permits()
    }

    /// Every counter and gauge in the Prometheus text format.
    pub fn render_metrics(&self) -> String {
        self.shared.metrics.render(&Gauges {
            sessions: self.sessions(),
            rooms: self.rooms(),
        })
    }
}
