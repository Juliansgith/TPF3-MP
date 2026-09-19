//! The server end of tunnels.

use std::{
    collections::HashMap,
    fmt, io,
    io::IoSliceMut,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Context, Poll},
};

use bytes::Bytes;
use futures_util::StreamExt;
use quinn::{
    AsyncUdpSocket, UdpPoller,
    udp::{RecvMeta, Transmit},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async_with_config,
    tungstenite::{
        handshake::server::{ErrorResponse, Request, Response},
        http::{HeaderMap, HeaderValue, StatusCode, header::SEC_WEBSOCKET_PROTOCOL},
    },
};

use super::{
    PROTOCOL, QUEUE, TunnelError, fill, read_datagrams, segments, write_datagrams, ws_config,
};

/// Datagrams from every tunnel queued for the endpoint.
const INBOUND: usize = 4096;
/// The /64 of the addresses QUIC sees tunnels at: private (fd00::/8), so
/// never a UDP peer's.
const PREFIX: u64 = 0xfd74_7066_336d_7475;

/// A server's open tunnels, shared by its QUIC socket and the tasks that
/// serve them. QUIC sees each tunnel at an address of its own.
pub struct Tunnels {
    inbound: mpsc::Sender<(SocketAddr, Bytes)>,
    queue: Mutex<mpsc::Receiver<(SocketAddr, Bytes)>>,
    peers: Mutex<HashMap<SocketAddr, Peer>>,
    next: AtomicU64,
}

struct Peer {
    outbound: mpsc::Sender<Bytes>,
    origin: IpAddr,
}

impl Tunnels {
    pub fn new() -> Arc<Self> {
        let (inbound, queue) = mpsc::channel(INBOUND);
        Arc::new(Self {
            inbound,
            queue: Mutex::new(queue),
            peers: Mutex::default(),
            next: AtomicU64::new(1),
        })
    }

    /// Whether QUIC sees `addr` as a tunnel rather than a UDP peer.
    pub fn is_tunnel(addr: SocketAddr) -> bool {
        match addr {
            SocketAddr::V6(v6) => (v6.ip().to_bits() >> 64) as u64 == PREFIX,
            SocketAddr::V4(_) => false,
        }
    }

    /// Where the client of the tunnel QUIC sees at `addr` connected from,
    /// while the tunnel is open.
    pub fn origin(&self, addr: SocketAddr) -> Option<IpAddr> {
        self.peers().get(&addr).map(|peer| peer.origin)
    }

    /// Tunnels open now.
    pub fn len(&self) -> usize {
        self.peers().len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers().is_empty()
    }

    fn peers(&self) -> MutexGuard<'_, HashMap<SocketAddr, Peer>> {
        self.peers.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Gives a new tunnel its address; it keeps it until the returned
    /// guard drops.
    fn open(self: &Arc<Self>, origin: IpAddr) -> (SocketAddr, mpsc::Receiver<Bytes>, Opened) {
        let index = self.next.fetch_add(1, Ordering::Relaxed);
        let ip = Ipv6Addr::from_bits((u128::from(PREFIX) << 64) | u128::from(index));
        let addr = SocketAddr::new(ip.into(), 443);
        let (outbound, queue) = mpsc::channel(QUEUE);
        self.peers().insert(addr, Peer { outbound, origin });
        let opened = Opened {
            tunnels: Arc::clone(self),
            addr,
        };
        (addr, queue, opened)
    }

    fn send(&self, transmit: &Transmit) {
        let peers = self.peers();
        // A tunnel that has closed takes its datagrams with it.
        let Some(peer) = peers.get(&transmit.destination) else {
            return;
        };
        for datagram in segments(transmit) {
            // A full queue drops the datagram, as a full UDP socket would.
            let _ = peer.outbound.try_send(Bytes::copy_from_slice(datagram));
        }
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<usize> {
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        fill(&mut queue, cx, bufs, meta, |item| item)
    }
}

impl fmt::Debug for Tunnels {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tunnels")
            .field("open", &self.len())
            .finish_non_exhaustive()
    }
}

/// Takes a tunnel out of the table when it ends.
struct Opened {
    tunnels: Arc<Tunnels>,
    addr: SocketAddr,
}

impl Drop for Opened {
    fn drop(&mut self) {
        self.tunnels.peers().remove(&self.addr);
    }
}

/// Completes a client's WebSocket handshake for a tunnel at `path`. With
/// `forwarded`, the client's address is the last `X-Forwarded-For` entry,
/// which the trusted proxy in front sets, and a request without one is
/// refused. Returns the tunnel, and that address if it was asked for.
#[allow(
    clippy::result_large_err,
    reason = "tungstenite's handshake callback returns its error response by value"
)]
pub async fn accept<S>(
    stream: S,
    path: &str,
    forwarded: bool,
) -> Result<(WebSocketStream<S>, Option<IpAddr>), TunnelError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut client = None;
    let mut refusal = None;
    let callback = |request: &Request, mut response: Response| {
        let mut refuse = |status: StatusCode, why: &'static str| {
            refusal = Some(why);
            let mut error = ErrorResponse::new(None);
            *error.status_mut() = status;
            Err(error)
        };
        if request.uri().path() != path {
            return refuse(StatusCode::NOT_FOUND, "another path");
        }
        if !offers_protocol(request.headers()) {
            return refuse(StatusCode::BAD_REQUEST, "no tunnel protocol offered");
        }
        if forwarded {
            match forwarded_client(request.headers()) {
                Some(ip) => client = Some(ip),
                None => return refuse(StatusCode::BAD_REQUEST, "no X-Forwarded-For"),
            }
        }
        response
            .headers_mut()
            .insert(SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static(PROTOCOL));
        Ok(response)
    };
    let accepted = accept_hdr_async_with_config(stream, callback, Some(ws_config())).await;
    match accepted {
        Ok(ws) => Ok((ws, client)),
        Err(error) => Err(refusal.map_or_else(|| error.into(), TunnelError::Refused)),
    }
}

fn offers_protocol(headers: &HeaderMap) -> bool {
    headers
        .get_all(SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|offered| offered.trim() == PROTOCOL)
}

/// The last `X-Forwarded-For` entry: the address the proxy in front saw,
/// whatever the client claimed before it.
fn forwarded_client(headers: &HeaderMap) -> Option<IpAddr> {
    let last = headers
        .get_all("x-forwarded-for")
        .iter()
        .next_back()?
        .to_str()
        .ok()?
        .rsplit(',')
        .next()?
        .trim();
    last.parse::<IpAddr>()
        .ok()
        .or_else(|| last.parse::<SocketAddr>().ok().map(|addr| addr.ip()))
}

/// Carries one accepted tunnel of a client at `origin` until it closes, or
/// nothing crosses it for [`IDLE`](super::IDLE).
pub async fn serve<S>(ws: WebSocketStream<S>, tunnels: &Arc<Tunnels>, origin: IpAddr)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (addr, outbound, _opened) = tunnels.open(origin);
    let (sink, stream) = ws.split();
    let inbound = tunnels.inbound.clone();
    tokio::select! {
        () = read_datagrams(stream, inbound, move |datagram| (addr, datagram)) => {}
        () = write_datagrams(sink, outbound) => {}
    }
}

/// A server's QUIC socket: its UDP socket and every open tunnel.
pub struct MuxSocket {
    udp: Arc<dyn AsyncUdpSocket>,
    tunnels: Arc<Tunnels>,
    /// Which source the next receive tries first.
    tunnels_first: AtomicBool,
}

impl MuxSocket {
    pub fn new(udp: Arc<dyn AsyncUdpSocket>, tunnels: Arc<Tunnels>) -> Self {
        Self {
            udp,
            tunnels,
            tunnels_first: AtomicBool::new(false),
        }
    }
}

impl fmt::Debug for MuxSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MuxSocket")
            .field("udp", &self.udp)
            .field("tunnels", &self.tunnels)
            .finish_non_exhaustive()
    }
}

impl AsyncUdpSocket for MuxSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        // Tunnels never make a sender wait, so only UDP can.
        Arc::clone(&self.udp).create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        if Tunnels::is_tunnel(transmit.destination) {
            self.tunnels.send(transmit);
            Ok(())
        } else {
            self.udp.try_send(transmit)
        }
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        // The sources take turns at going first, so a busy one cannot
        // starve the other. Both register the waker when neither has data.
        if self.tunnels_first.fetch_xor(true, Ordering::Relaxed) {
            if let Poll::Ready(count) = self.tunnels.poll_recv(cx, bufs, meta) {
                return Poll::Ready(Ok(count));
            }
            self.udp.poll_recv(cx, bufs, meta)
        } else {
            match self.udp.poll_recv(cx, bufs, meta) {
                Poll::Pending => self.tunnels.poll_recv(cx, bufs, meta).map(Ok),
                ready => ready,
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.udp.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.udp.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.udp.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.udp.may_fragment()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_addresses_never_look_like_udp_peers() {
        let tunnels = Tunnels::new();
        let (addr, _queue, opened) = tunnels.open("192.0.2.7".parse().unwrap());
        assert!(Tunnels::is_tunnel(addr));
        assert_eq!(tunnels.origin(addr), Some("192.0.2.7".parse().unwrap()));
        for udp in [
            "192.0.2.7:443",
            "[2001:db8::1]:443",
            "[fd74:7066:336d:7476::1]:443",
        ] {
            assert!(!Tunnels::is_tunnel(udp.parse().unwrap()), "{udp}");
        }
        let (other, _queue, _opened) = tunnels.open("192.0.2.8".parse().unwrap());
        assert_ne!(addr, other, "every tunnel has its own address");
        drop(opened);
        assert_eq!(tunnels.origin(addr), None, "closed tunnels are forgotten");
        assert_eq!(tunnels.len(), 1);
    }

    #[test]
    fn the_client_is_the_last_forwarded_address() {
        let mut headers = HeaderMap::new();
        assert_eq!(forwarded_client(&headers), None);
        headers.append(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.9, 198.51.100.2"),
        );
        assert_eq!(forwarded_client(&headers), "198.51.100.2".parse().ok());
        headers.append("x-forwarded-for", HeaderValue::from_static("2001:db8::7"));
        assert_eq!(forwarded_client(&headers), "2001:db8::7".parse().ok());
        headers.append(
            "x-forwarded-for",
            HeaderValue::from_static("[2001:db8::8]:4711"),
        );
        assert_eq!(forwarded_client(&headers), "2001:db8::8".parse().ok());
        headers.append("x-forwarded-for", HeaderValue::from_static("unknown"));
        assert_eq!(forwarded_client(&headers), None);
    }

    #[test]
    fn only_the_tunnel_protocol_is_accepted() {
        let mut headers = HeaderMap::new();
        assert!(!offers_protocol(&headers));
        headers.insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("chat, superchat"),
        );
        assert!(!offers_protocol(&headers));
        headers.insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("chat, tpf3mp-quic-1"),
        );
        assert!(offers_protocol(&headers));
    }
}
