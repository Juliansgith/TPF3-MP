//! The tunnel listener: QUIC over WebSocket, for players whose networks
//! block UDP. The tunnels feed the server's one QUIC endpoint, so a
//! tunneled player is served exactly like one on UDP. See
//! `tpf3mp_net::tunnel`.

use std::{
    io,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use tokio::{
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};
use tokio_tungstenite::WebSocketStream;
use tpf3mp_net::{
    ServerIdentity,
    tunnel::{self, TunnelError, Tunnels},
    tunnel_server_tls,
};
use tracing::{debug, info, warn};

use crate::{
    ServerError,
    admission::{Admission, Origin, TunnelSlot},
    metrics::{self, Metrics},
};

/// Where and how the server accepts tunnels.
#[derive(Debug, Clone)]
pub struct TunnelConfig {
    /// TCP address the listener binds.
    pub listen: SocketAddr,
    /// The URL path tunnels open. Players look for
    /// [`DEFAULT_PATH`](tunnel::DEFAULT_PATH) unless told otherwise.
    pub path: String,
    /// Serve TLS with the server's own certificate. Off behind a proxy that
    /// terminates TLS, such as Caddy or nginx.
    pub tls: bool,
    /// Take each player's address from the last `X-Forwarded-For` entry,
    /// which the proxy in front sets; requests without one are refused.
    /// Only for a listener nothing but the proxy can reach.
    pub trust_forwarded: bool,
}

impl TunnelConfig {
    /// TLS on `listen`, at the default path.
    pub fn new(listen: SocketAddr) -> Self {
        Self {
            listen,
            path: tunnel::DEFAULT_PATH.into(),
            tls: true,
            trust_forwarded: false,
        }
    }

    /// Plain WebSocket on `listen`, at the default path, for a proxy in
    /// front that terminates TLS and sets `X-Forwarded-For`.
    pub fn behind_proxy(listen: SocketAddr) -> Self {
        Self {
            tls: false,
            trust_forwarded: true,
            ..Self::new(listen)
        }
    }
}

/// What the listener shares with the rest of the server.
pub(crate) struct TunnelEnv {
    pub(crate) tunnels: Arc<Tunnels>,
    pub(crate) admission: Arc<Admission>,
    pub(crate) metrics: Arc<Metrics>,
    /// Tunnels open at once, handshakes included.
    pub(crate) capacity: usize,
    pub(crate) handshake_timeout: Duration,
}

/// A bound tunnel listener, ready to run.
pub(crate) struct Listener {
    tcp: TcpListener,
    acceptor: Option<TlsAcceptor>,
    path: Arc<str>,
    trust_forwarded: bool,
}

/// A tunnel whose WebSocket handshake is complete.
enum Accepted {
    Tls(Box<WebSocketStream<TlsStream<TcpStream>>>),
    Plain(Box<WebSocketStream<TcpStream>>),
}

impl Listener {
    /// Binds the listener; `identity` serves TLS when the configuration
    /// asks for it.
    pub(crate) fn bind(
        config: &TunnelConfig,
        identity: ServerIdentity,
    ) -> Result<Self, ServerError> {
        let acceptor = if config.tls {
            Some(TlsAcceptor::from(tunnel_server_tls(identity)?))
        } else {
            None
        };
        let tcp = std::net::TcpListener::bind(config.listen).map_err(ServerError::Tunnel)?;
        tcp.set_nonblocking(true).map_err(ServerError::Tunnel)?;
        let tcp = TcpListener::from_std(tcp).map_err(ServerError::Tunnel)?;
        if config.trust_forwarded {
            info!(
                "tunnels take players' addresses from X-Forwarded-For: keep the listener reachable by the proxy only"
            );
        }
        Ok(Self {
            tcp,
            acceptor,
            path: config.path.as_str().into(),
            trust_forwarded: config.trust_forwarded,
        })
    }

    pub(crate) fn local_addr(&self) -> io::Result<SocketAddr> {
        self.tcp.local_addr()
    }

    /// Accepts tunnels until the task running this is aborted, which ends
    /// every tunnel with it.
    pub(crate) async fn run(self, env: TunnelEnv) {
        let env = Arc::new(env);
        let open = Arc::new(Semaphore::new(env.capacity.max(1)));
        let mut running = JoinSet::new();
        loop {
            let accepted = tokio::select! {
                accepted = self.tcp.accept() => accepted,
                Some(_) = running.join_next(), if !running.is_empty() => continue,
            };
            let (tcp, peer) = match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    // Out of file descriptors, most likely: let some close.
                    warn!(%error, "cannot accept a tunnel connection");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Ok(permit) = Arc::clone(&open).try_acquire_owned() else {
                metrics::increment(&env.metrics.tunnels_refused);
                continue;
            };
            // A direct client counts against its own address from the
            // start. Behind a proxy, only the forwarded address says who
            // it is, once the handshake is done.
            let slot = if self.trust_forwarded {
                None
            } else {
                match env.admission.tunnel(Origin::of(peer.ip())) {
                    Some(slot) => Some(slot),
                    None => {
                        metrics::increment(&env.metrics.tunnels_refused);
                        continue;
                    }
                }
            };
            running.spawn(serve_one(
                tcp,
                peer,
                self.acceptor.clone(),
                Arc::clone(&self.path),
                self.trust_forwarded,
                Arc::clone(&env),
                (permit, slot),
            ));
        }
    }
}

async fn serve_one(
    tcp: TcpStream,
    peer: SocketAddr,
    acceptor: Option<TlsAcceptor>,
    path: Arc<str>,
    trust_forwarded: bool,
    env: Arc<TunnelEnv>,
    held: (OwnedSemaphorePermit, Option<TunnelSlot>),
) {
    // Logs name no addresses, as everywhere in the server.
    let _ = tcp.set_nodelay(true);
    let handshake = async {
        match acceptor {
            Some(acceptor) => {
                let tls = acceptor.accept(tcp).await.map_err(TunnelError::from)?;
                let (ws, forwarded) = tunnel::accept(tls, &path, trust_forwarded).await?;
                Ok::<_, TunnelError>((Accepted::Tls(Box::new(ws)), forwarded))
            }
            None => {
                let (ws, forwarded) = tunnel::accept(tcp, &path, trust_forwarded).await?;
                Ok((Accepted::Plain(Box::new(ws)), forwarded))
            }
        }
    };
    let (accepted, forwarded) = match tokio::time::timeout(env.handshake_timeout, handshake).await {
        Ok(Ok(accepted)) => accepted,
        Ok(Err(error)) => {
            debug!(%error, "a tunnel handshake failed");
            return;
        }
        Err(_) => {
            debug!("a tunnel handshake timed out");
            return;
        }
    };
    let (_permit, mut slot) = held;
    let origin: IpAddr = match forwarded {
        Some(client) => {
            match env.admission.tunnel(Origin::of(client)) {
                Some(forwarded_slot) => slot = Some(forwarded_slot),
                None => {
                    metrics::increment(&env.metrics.tunnels_refused);
                    return;
                }
            }
            client
        }
        None => peer.ip(),
    };
    metrics::increment(&env.metrics.tunnels_opened);
    debug!(forwarded = trust_forwarded, "a tunnel opened");
    match accepted {
        Accepted::Tls(ws) => tunnel::serve(*ws, &env.tunnels, origin).await,
        Accepted::Plain(ws) => tunnel::serve(*ws, &env.tunnels, origin).await,
    }
    drop(slot);
}
