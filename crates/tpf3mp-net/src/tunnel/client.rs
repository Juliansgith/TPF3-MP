//! The client end of a tunnel.

use std::{
    fmt, io,
    io::IoSliceMut,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    pin::Pin,
    sync::{Arc, Mutex, PoisonError},
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use futures_util::{StreamExt, stream::FuturesUnordered};
use quinn::{
    AsyncUdpSocket, UdpPoller,
    udp::{RecvMeta, Transmit},
};
use rustls::pki_types::ServerName;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    sync::mpsc,
    task::AbortHandle,
};
use tokio_rustls::TlsConnector;
use tokio_tungstenite::{
    WebSocketStream, client_async_with_config,
    tungstenite::{
        client::IntoClientRequest,
        handshake::client::Response,
        http::{HeaderValue, header::SEC_WEBSOCKET_PROTOCOL},
    },
};

use super::{
    PROTOCOL, QUEUE, TunnelError, TunnelUrl, fill, read_datagrams, segments, write_datagrams,
    ws_config,
};
use crate::{ServerTrust, tls::tunnel_client_tls};

/// How long one address of a tunnel's host has alone before the next joins
/// the race, as browsers do (RFC 8305).
const NEXT_ADDRESS_AFTER: Duration = Duration::from_millis(250);

/// A client's QUIC socket that sends and receives through a tunnel. Every
/// datagram goes to the tunnel's server, whatever address QUIC names, and
/// every datagram received comes from `peer`, the address the endpoint
/// connects to.
pub struct TunnelSocket {
    inbound: Mutex<mpsc::Receiver<Bytes>>,
    outbound: mpsc::Sender<Bytes>,
    peer: SocketAddr,
    tasks: [AbortHandle; 2],
}

/// Opens a tunnel to `url` for an endpoint that will connect to `peer`.
/// The tunnel's certificate is trusted as `trust` says, or if public
/// authorities vouch for it: a proxy in front of the server may show its
/// own. The QUIC connection inside authenticates the server either way.
pub async fn connect(
    url: &TunnelUrl,
    trust: &ServerTrust,
    peer: SocketAddr,
) -> Result<Arc<TunnelSocket>, TunnelError> {
    let tcp = connect_tcp(url.host(), url.port()).await?;
    tcp.set_nodelay(true)?;
    let mut request = url.uri().clone().into_client_request()?;
    request
        .headers_mut()
        .insert(SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static(PROTOCOL));
    if url.is_secure() {
        let name =
            ServerName::try_from(url.host().to_owned()).map_err(|_| TunnelError::ServerName)?;
        let tls = TlsConnector::from(tunnel_client_tls(trust)?)
            .connect(name, tcp)
            .await?;
        let (ws, response) = client_async_with_config(request, tls, Some(ws_config())).await?;
        check_protocol(&response)?;
        Ok(TunnelSocket::start(ws, peer))
    } else {
        let (ws, response) = client_async_with_config(request, tcp, Some(ws_config())).await?;
        check_protocol(&response)?;
        Ok(TunnelSocket::start(ws, peer))
    }
}

/// Connects to the first address of `host` that answers. Each address the
/// name resolves to, IPv4 first, gets a quarter second alone before the
/// next joins in, so one that never answers, or answers slowly with a
/// refusal as a closed port on Windows does, costs a quarter second rather
/// than a timeout.
async fn connect_tcp(host: &str, port: u16) -> io::Result<TcpStream> {
    let mut addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await?.collect();
    addresses.sort_by_key(|address| !address.is_ipv4());
    let mut untried = addresses.into_iter();
    let mut attempts = FuturesUnordered::new();
    let mut failure = io::Error::new(io::ErrorKind::NotFound, "the host has no address");
    loop {
        if attempts.is_empty() {
            match untried.next() {
                Some(address) => attempts.push(TcpStream::connect(address)),
                None => return Err(failure),
            }
        }
        tokio::select! {
            Some(result) = attempts.next() => match result {
                Ok(stream) => return Ok(stream),
                Err(error) => failure = error,
            },
            () = tokio::time::sleep(NEXT_ADDRESS_AFTER), if untried.len() > 0 => {
                if let Some(address) = untried.next() {
                    attempts.push(TcpStream::connect(address));
                }
            }
        }
    }
}

/// A server that answered the upgrade without choosing the tunnel protocol
/// is some other WebSocket service.
fn check_protocol(response: &Response) -> Result<(), TunnelError> {
    let chosen = response.headers().get(SEC_WEBSOCKET_PROTOCOL);
    if chosen.is_some_and(|value| value.as_bytes() == PROTOCOL.as_bytes()) {
        Ok(())
    } else {
        Err(TunnelError::Protocol)
    }
}

impl TunnelSocket {
    fn start<S>(ws: WebSocketStream<S>, peer: SocketAddr) -> Arc<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (sink, stream) = ws.split();
        let (inbound_tx, inbound) = mpsc::channel(QUEUE);
        let (outbound, outbound_rx) = mpsc::channel(QUEUE);
        let reader = tokio::spawn(read_datagrams(stream, inbound_tx, |datagram| datagram));
        let writer = tokio::spawn(write_datagrams(sink, outbound_rx));
        Arc::new(Self {
            inbound: Mutex::new(inbound),
            outbound,
            peer,
            tasks: [reader.abort_handle(), writer.abort_handle()],
        })
    }
}

impl Drop for TunnelSocket {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl fmt::Debug for TunnelSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TunnelSocket")
            .field("peer", &self.peer)
            .finish_non_exhaustive()
    }
}

impl AsyncUdpSocket for TunnelSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(Writable)
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        for datagram in segments(transmit) {
            // A full queue drops the datagram, as a full UDP socket would.
            let _ = self.outbound.try_send(Bytes::copy_from_slice(datagram));
        }
        Ok(())
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let mut inbound = self.inbound.lock().unwrap_or_else(PoisonError::into_inner);
        fill(&mut inbound, cx, bufs, meta, |datagram| {
            (self.peer, datagram)
        })
        .map(Ok)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        // The peer's family, so QUIC keeps the peer's address as given.
        Ok(match self.peer {
            SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
            SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
        })
    }

    fn may_fragment(&self) -> bool {
        false
    }
}

/// A tunnel never makes a sender wait: a full queue drops datagrams
/// instead.
#[derive(Debug)]
pub(super) struct Writable;

impl UdpPoller for Writable {
    fn poll_writable(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
