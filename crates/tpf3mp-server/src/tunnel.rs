//! The tunnel listener: QUIC over WebSocket, for players whose networks
//! block UDP. The tunnels feed the server's one QUIC endpoint, so a
//! tunneled player is served exactly like one on UDP. See
//! `tpf3mp_net::tunnel`.

use std::{
    fmt, io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    str::FromStr,
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
    /// Where the proxy connects from, when `trust_forwarded`: connections
    /// from anywhere else are refused, as they could claim any address.
    /// Empty accepts every address.
    pub proxies: Vec<AddressRange>,
}

impl TunnelConfig {
    /// TLS on `listen`, at the default path.
    pub fn new(listen: SocketAddr) -> Self {
        Self {
            listen,
            path: tunnel::DEFAULT_PATH.into(),
            tls: true,
            trust_forwarded: false,
            proxies: Vec::new(),
        }
    }

    /// Plain WebSocket on `listen`, at the default path, for a proxy in
    /// front that terminates TLS and sets `X-Forwarded-For`, and connects
    /// from loopback or a private network, as a proxy on the same host or
    /// a container's does.
    pub fn behind_proxy(listen: SocketAddr) -> Self {
        Self {
            tls: false,
            trust_forwarded: true,
            proxies: AddressRange::PRIVATE.to_vec(),
            ..Self::new(listen)
        }
    }
}

/// A range of addresses: `192.0.2.0/24`, `2001:db8::/32`, or one address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressRange {
    network: IpAddr,
    prefix: u8,
}

impl AddressRange {
    /// Loopback and the private networks, IPv4 and IPv6.
    pub const PRIVATE: [Self; 6] = [
        Self::v4(Ipv4Addr::new(127, 0, 0, 0), 8),
        Self::v4(Ipv4Addr::new(10, 0, 0, 0), 8),
        Self::v4(Ipv4Addr::new(172, 16, 0, 0), 12),
        Self::v4(Ipv4Addr::new(192, 168, 0, 0), 16),
        Self::v6(Ipv6Addr::LOCALHOST, 128),
        Self::v6(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
    ];

    const fn v4(network: Ipv4Addr, prefix: u8) -> Self {
        Self {
            network: IpAddr::V4(network),
            prefix,
        }
    }

    const fn v6(network: Ipv6Addr, prefix: u8) -> Self {
        Self {
            network: IpAddr::V6(network),
            prefix,
        }
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        match (self.network, ip) {
            (IpAddr::V4(network), IpAddr::V4(ip)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                network.to_bits() & mask == ip.to_bits() & mask
            }
            (IpAddr::V6(network), IpAddr::V6(ip)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                network.to_bits() & mask == ip.to_bits() & mask
            }
            _ => false,
        }
    }
}

impl FromStr for AddressRange {
    type Err = &'static str;

    fn from_str(range: &str) -> Result<Self, Self::Err> {
        let (address, prefix) = match range.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (range, None),
        };
        let network: IpAddr = address.parse().map_err(|_| "not an IP address")?;
        let network = network.to_canonical();
        let bits = if network.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(prefix) => prefix.parse::<u8>().map_err(|_| "not a prefix length")?,
            None => bits,
        };
        if prefix > bits {
            return Err("the prefix is longer than the address");
        }
        Ok(Self { network, prefix })
    }
}

impl fmt::Display for AddressRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
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
    proxies: Vec<AddressRange>,
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
            let proxies: Vec<String> = config.proxies.iter().map(ToString::to_string).collect();
            info!(
                proxies = %proxies.join(", "),
                "tunnels take players' addresses from X-Forwarded-For, set by a proxy connecting from these ranges"
            );
        }
        Ok(Self {
            tcp,
            acceptor,
            path: config.path.as_str().into(),
            trust_forwarded: config.trust_forwarded,
            proxies: config.proxies.clone(),
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
            if self.trust_forwarded
                && !self.proxies.is_empty()
                && !self.proxies.iter().any(|range| range.contains(peer.ip()))
            {
                // Not the proxy: whatever it forwards would be its own word.
                metrics::increment(&env.metrics.tunnels_refused);
                continue;
            }
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
    // Behind a proxy, the forwarded address takes its slot during the
    // handshake, so a refusal reaches the player as such.
    let admit = {
        let env = Arc::clone(&env);
        move |forwarded: Option<IpAddr>| match forwarded {
            Some(client) => {
                let slot = env.admission.tunnel(Origin::of(client));
                if slot.is_none() {
                    metrics::increment(&env.metrics.tunnels_refused);
                }
                slot.map(Some)
            }
            None => Some(None),
        }
    };
    let handshake = async {
        match acceptor {
            Some(acceptor) => {
                let tls = acceptor.accept(tcp).await.map_err(TunnelError::from)?;
                let (ws, forwarded, slot) =
                    tunnel::accept(tls, &path, trust_forwarded, admit).await?;
                Ok::<_, TunnelError>((Accepted::Tls(Box::new(ws)), forwarded, slot))
            }
            None => {
                let (ws, forwarded, slot) =
                    tunnel::accept(tcp, &path, trust_forwarded, admit).await?;
                Ok((Accepted::Plain(Box::new(ws)), forwarded, slot))
            }
        }
    };
    let (accepted, forwarded, forwarded_slot) =
        match tokio::time::timeout(env.handshake_timeout, handshake).await {
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
    let (_permit, direct_slot) = held;
    let _slot = forwarded_slot.or(direct_slot);
    let origin = forwarded.unwrap_or(peer.ip());
    metrics::increment(&env.metrics.tunnels_opened);
    debug!(forwarded = trust_forwarded, "a tunnel opened");
    match accepted {
        Accepted::Tls(ws) => tunnel::serve(*ws, &env.tunnels, origin).await,
        Accepted::Plain(ws) => tunnel::serve(*ws, &env.tunnels, origin).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_ranges_hold_what_they_say() {
        let range: AddressRange = "172.16.0.0/12".parse().unwrap();
        assert!(range.contains("172.17.0.1".parse().unwrap()));
        assert!(range.contains("::ffff:172.31.255.255".parse().unwrap()));
        assert!(!range.contains("172.32.0.1".parse().unwrap()));
        assert!(!range.contains("2001:db8::1".parse().unwrap()));
        let one: AddressRange = "2001:db8::7".parse().unwrap();
        assert!(one.contains("2001:db8::7".parse().unwrap()));
        assert!(!one.contains("2001:db8::8".parse().unwrap()));
        let all: AddressRange = "0.0.0.0/0".parse().unwrap();
        assert!(all.contains("203.0.113.9".parse().unwrap()));
        for bad in ["10.0.0.0/33", "10.0.0/8", "::/129", "proxy", "10.0.0.0/x"] {
            assert!(bad.parse::<AddressRange>().is_err(), "{bad}");
        }
        assert_eq!(range.to_string(), "172.16.0.0/12");
    }

    #[test]
    fn a_proxy_on_the_host_or_in_a_container_is_private() {
        let private = |ip: &str| {
            AddressRange::PRIVATE
                .iter()
                .any(|range| range.contains(ip.parse().unwrap()))
        };
        for ip in [
            "127.0.0.1",
            "::1",
            "172.17.0.1",
            "10.1.2.3",
            "192.168.1.1",
            "fd00::1",
        ] {
            assert!(private(ip), "{ip}");
        }
        for ip in ["203.0.113.9", "2001:db8::1", "8.8.8.8"] {
            assert!(!private(ip), "{ip}");
        }
    }
}
