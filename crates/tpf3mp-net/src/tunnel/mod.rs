//! QUIC over WebSocket, for networks that block UDP.
//!
//! A tunnel is a WebSocket connection whose binary messages each carry one
//! QUIC datagram. A client's endpoint runs on a [`TunnelSocket`] in place of
//! a UDP socket; the server's runs on a [`MuxSocket`], which merges its UDP
//! socket with every open tunnel. The QUIC connection inside is the same
//! either way, end-to-end encrypted and authenticated, so a proxy in front
//! of the server sees only ciphertext. The tunnel's own TLS gets the traffic
//! through networks that let nothing but HTTPS out.
//!
//! QUIC over TCP pays for TCP's in-order delivery: one lost segment holds up
//! every datagram behind it. Clients take a tunnel only when UDP does not
//! get through.

mod client;
mod server;

use std::{
    fmt,
    io::IoSliceMut,
    net::SocketAddr,
    str::FromStr,
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use futures_util::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use quinn::udp::{RecvMeta, Transmit};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{self, Message, http::Uri, protocol::WebSocketConfig},
};

pub use client::{TunnelSocket, connect};
pub use server::{Attached, MuxSocket, Tunnels, accept, serve};

use crate::TlsError;

/// The WebSocket subprotocol of a tunnel. A new tunnel format gets a new one.
pub const PROTOCOL: &str = "tpf3mp-quic-1";
/// The path a server serves tunnels at unless told otherwise, and where
/// clients look for one.
pub const DEFAULT_PATH: &str = "/tpf3mp";
/// Largest datagram a tunnel carries, well above any QUIC packet.
pub const MAX_DATAGRAM: usize = 2048;
/// A tunnel that carries no datagram for this long is closed. Pings and
/// pongs do not count. QUIC keep-alives cross an open connection's tunnel
/// every few seconds.
pub const IDLE: Duration = Duration::from_secs(60);
/// Datagrams queued in each direction of one tunnel. When a queue is full,
/// new datagrams are dropped, as a full UDP socket drops them, and QUIC's
/// congestion control slows down.
const QUEUE: usize = 256;

/// Where a tunnel opens: a `wss://` URL, or `ws://` behind a proxy on the
/// same machine and in tests.
#[derive(Clone, PartialEq, Eq)]
pub struct TunnelUrl {
    uri: Uri,
    secure: bool,
    host: String,
    port: u16,
}

impl TunnelUrl {
    /// The tunnel a server at `host` serves by default:
    /// `wss://<host>/tpf3mp` on port 443.
    pub fn default_for(host: &str) -> Result<Self, TunnelError> {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if host.contains(':') {
            format!("wss://[{host}]{DEFAULT_PATH}").parse()
        } else {
            format!("wss://{host}{DEFAULT_PATH}").parse()
        }
    }

    /// The host to connect to, without brackets around an IPv6 address.
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Whether the tunnel runs over TLS (`wss://`).
    pub fn is_secure(&self) -> bool {
        self.secure
    }

    pub(crate) fn uri(&self) -> &Uri {
        &self.uri
    }
}

impl FromStr for TunnelUrl {
    type Err = TunnelError;

    fn from_str(url: &str) -> Result<Self, Self::Err> {
        let uri: Uri = url.parse().map_err(|_| TunnelError::Url("not a URL"))?;
        let secure = match uri.scheme_str() {
            Some("wss") => true,
            Some("ws") => false,
            _ => return Err(TunnelError::Url("the scheme must be wss:// or ws://")),
        };
        let authority = uri
            .authority()
            .ok_or(TunnelError::Url("the URL names no host"))?;
        if authority.as_str().contains('@') {
            return Err(TunnelError::Url("the URL must not carry credentials"));
        }
        let host = authority
            .host()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        if host.is_empty() {
            return Err(TunnelError::Url("the URL names no host"));
        }
        let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
        Ok(Self {
            uri,
            secure,
            host,
            port,
        })
    }
}

impl fmt::Display for TunnelUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.uri.fmt(f)
    }
}

impl fmt::Debug for TunnelUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Why a tunnel could not be opened or accepted.
#[derive(Debug, Error)]
pub enum TunnelError {
    #[error("invalid tunnel URL: {0}")]
    Url(&'static str),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Tls(#[from] TlsError),
    #[error("the tunnel's host is not a valid TLS server name")]
    ServerName,
    #[error("WebSocket: {0}")]
    WebSocket(Box<tungstenite::Error>),
    #[error("the server does not speak the tunnel protocol")]
    Protocol,
    #[error("not a tunnel request: {0}")]
    Refused(&'static str),
}

impl From<tungstenite::Error> for TunnelError {
    fn from(error: tungstenite::Error) -> Self {
        Self::WebSocket(Box::new(error))
    }
}

/// What a tunnel's WebSocket accepts: messages the size of a datagram, and
/// nothing larger. Its buffers are sized for datagrams too, where the
/// defaults would hold 128 KiB each per tunnel.
fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_DATAGRAM))
        .max_frame_size(Some(MAX_DATAGRAM))
        .read_buffer_size(4 * 1024)
        .write_buffer_size(16 * 1024)
}

/// The datagrams of a transmit: several of `segment_size` bytes when the
/// socket batches them.
fn segments<'a>(transmit: &'a Transmit<'_>) -> impl Iterator<Item = &'a [u8]> {
    let size = transmit
        .segment_size
        .unwrap_or(transmit.contents.len())
        .max(1);
    transmit.contents.chunks(size)
}

/// Moves queued datagrams into quinn's receive buffers, one per buffer.
/// Pending, with the waker registered, while none is queued; a closed queue
/// stays pending, as its tunnel is gone.
fn fill<T>(
    queue: &mut mpsc::Receiver<T>,
    cx: &mut Context,
    bufs: &mut [IoSliceMut<'_>],
    meta: &mut [RecvMeta],
    mut open: impl FnMut(T) -> (SocketAddr, Bytes),
) -> Poll<usize> {
    let room = bufs.len().min(meta.len());
    let mut filled = 0;
    while filled < room {
        let item = if filled == 0 {
            match queue.poll_recv(cx) {
                Poll::Ready(Some(item)) => item,
                Poll::Ready(None) | Poll::Pending => return Poll::Pending,
            }
        } else {
            match queue.try_recv() {
                Ok(item) => item,
                Err(_) => break,
            }
        };
        let (addr, datagram) = open(item);
        let buf = &mut bufs[filled];
        if datagram.len() > buf.len() {
            // Larger than any QUIC packet: dropped, as UDP would.
            continue;
        }
        buf[..datagram.len()].copy_from_slice(&datagram);
        meta[filled] = RecvMeta {
            addr,
            len: datagram.len(),
            stride: datagram.len(),
            ecn: None,
            dst_ip: None,
        };
        filled += 1;
    }
    Poll::Ready(filled)
}

/// Reads a tunnel's datagrams into `queue` until the tunnel ends, the queue
/// closes, or no datagram arrives for [`IDLE`].
async fn read_datagrams<S, T>(
    mut stream: SplitStream<WebSocketStream<S>>,
    queue: mpsc::Sender<T>,
    wrap: impl Fn(Bytes) -> T,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut deadline = tokio::time::Instant::now() + IDLE;
    loop {
        let message = match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(message))) => message,
            // Silence, the end, or a broken stream: the tunnel is over.
            _ => return,
        };
        match message {
            Message::Binary(datagram) => {
                deadline = tokio::time::Instant::now() + IDLE;
                // Waiting here holds the TCP connection back, which is
                // better than dropping what already crossed it.
                if queue.send(wrap(datagram)).await.is_err() {
                    return;
                }
            }
            // They carry nothing, so they keep nothing open.
            Message::Ping(_) | Message::Pong(_) => {}
            // Text, a close, or a raw frame: not a tunnel's traffic.
            Message::Text(_) | Message::Close(_) | Message::Frame(_) => return,
        }
    }
}

/// Writes queued datagrams to a tunnel, flushing whenever the queue runs
/// dry, until the queue closes or the tunnel breaks.
async fn write_datagrams<S>(
    mut sink: SplitSink<WebSocketStream<S>, Message>,
    mut queue: mpsc::Receiver<Bytes>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    while let Some(first) = queue.recv().await {
        let mut next = Some(first);
        while let Some(datagram) = next {
            if sink.feed(Message::Binary(datagram)).await.is_err() {
                return;
            }
            next = queue.try_recv().ok();
        }
        if sink.flush().await.is_err() {
            return;
        }
    }
    let _ = sink.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_urls_are_ws_or_wss_with_a_host() {
        let url: TunnelUrl = "wss://play.example.net/tpf3mp".parse().unwrap();
        assert_eq!(
            (url.host(), url.port(), url.is_secure()),
            ("play.example.net", 443, true)
        );
        let url: TunnelUrl = "ws://127.0.0.1:8080/t".parse().unwrap();
        assert_eq!(
            (url.host(), url.port(), url.is_secure()),
            ("127.0.0.1", 8080, false)
        );
        let url: TunnelUrl = "wss://[2001:db8::1]:8443/x".parse().unwrap();
        assert_eq!((url.host(), url.port()), ("2001:db8::1", 8443));
        for bad in [
            "https://play.example.net/tpf3mp",
            "play.example.net:443",
            "wss:///tpf3mp",
            "wss://user:secret@play.example.net/tpf3mp",
            "not a url",
        ] {
            assert!(bad.parse::<TunnelUrl>().is_err(), "{bad}");
        }
    }

    #[test]
    fn a_servers_default_tunnel_is_wss_on_its_host() {
        let url = TunnelUrl::default_for("play.example.net").unwrap();
        assert_eq!(url.to_string(), "wss://play.example.net/tpf3mp");
        let url = TunnelUrl::default_for("[2001:db8::1]").unwrap();
        assert_eq!((url.host(), url.port()), ("2001:db8::1", 443));
        assert_eq!(url.to_string(), "wss://[2001:db8::1]/tpf3mp");
    }

    #[test]
    fn batched_transmits_split_into_their_datagrams() {
        let contents = [7u8; 2500];
        let transmit = Transmit {
            destination: "192.0.2.1:1".parse().unwrap(),
            ecn: None,
            contents: &contents,
            segment_size: Some(1200),
            src_ip: None,
        };
        let sizes: Vec<usize> = segments(&transmit).map(<[u8]>::len).collect();
        assert_eq!(sizes, [1200, 1200, 100]);
        let single = Transmit {
            segment_size: None,
            ..transmit
        };
        assert_eq!(segments(&single).count(), 1);
    }
}
