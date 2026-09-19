//! What keeps a tunnel open, and who may open one. Tunnels here run over
//! in-memory streams, with time paused where it matters.

#![allow(clippy::unwrap_used)]

use std::{net::SocketAddr, sync::Arc, time::Duration};

use bytes::Bytes;
use futures_util::SinkExt;
use tokio::io::DuplexStream;
use tokio_tungstenite::{
    WebSocketStream, client_async,
    tungstenite::{
        self, Message,
        client::IntoClientRequest,
        handshake::client::Request,
        http::{
            HeaderValue, StatusCode,
            header::{ORIGIN, SEC_WEBSOCKET_PROTOCOL},
        },
    },
};
use tpf3mp_net::tunnel::{self, IDLE, TunnelError, Tunnels};

fn request(origin: Option<&'static str>) -> Request {
    let mut request = "ws://play.example.net/tpf3mp"
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static(tunnel::PROTOCOL),
    );
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert(ORIGIN, HeaderValue::from_static(origin));
    }
    request
}

/// Serves one tunnel over an in-memory stream, as the server's listener
/// does, and returns the client's end.
async fn open(tunnels: &Arc<Tunnels>) -> WebSocketStream<DuplexStream> {
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    let serving = Arc::clone(tunnels);
    tokio::spawn(async move {
        let (ws, _, ()) = tunnel::accept(server_io, "/tpf3mp", false, |_| Some(()))
            .await
            .unwrap();
        tunnel::serve(ws, &serving, "192.0.2.1".parse().unwrap()).await;
    });
    let (ws, _) = client_async(request(None), client_io).await.unwrap();
    settle(tunnels, 1).await;
    ws
}

/// Lets the tunnel tasks run until `open` tunnels remain.
async fn settle(tunnels: &Tunnels, open: usize) {
    for _ in 0..1000 {
        if tunnels.len() == open {
            return;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(tunnels.len(), open);
}

/// The address QUIC sees the only open tunnel at.
fn only_tunnel() -> SocketAddr {
    // Tunnel addresses count up from 1 in a fresh table.
    let ip = std::net::Ipv6Addr::from_bits((0xfd74_7066_336d_7475u128 << 64) | 1);
    SocketAddr::new(ip.into(), 443)
}

#[tokio::test(start_paused = true)]
async fn a_silent_tunnel_closes() {
    let tunnels = Tunnels::new(None);
    let _ws = open(&tunnels).await;
    tokio::time::sleep(IDLE + Duration::from_secs(1)).await;
    settle(&tunnels, 0).await;
}

#[tokio::test(start_paused = true)]
async fn pings_alone_do_not_keep_a_tunnel_open() {
    let tunnels = Tunnels::new(None);
    let mut ws = open(&tunnels).await;
    // A ping every 50 s, and not one datagram.
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_secs(50)).await;
        if ws.send(Message::Ping(Bytes::new())).await.is_err() {
            break;
        }
    }
    settle(&tunnels, 0).await;
}

#[tokio::test(start_paused = true)]
async fn datagrams_keep_a_tunnel_open() {
    let tunnels = Tunnels::new(None);
    let mut ws = open(&tunnels).await;
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_secs(50)).await;
        ws.send(Message::Binary(Bytes::from_static(&[0; 40])))
            .await
            .unwrap();
    }
    settle(&tunnels, 1).await;
}

#[tokio::test(start_paused = true)]
async fn a_tunnel_carrying_no_connection_closes_after_the_grace() {
    let grace = Duration::from_secs(10);
    let tunnels = Tunnels::new(Some(grace));

    // Nothing ever attaches: the tunnel ends after the grace, however busy.
    let mut ws = open(&tunnels).await;
    for _ in 0..4 {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let _ = ws.send(Message::Binary(Bytes::from_static(&[1; 40]))).await;
    }
    settle(&tunnels, 0).await;

    // A connection attached holds it open; once it ends, the grace runs.
    let tunnels = Tunnels::new(Some(grace));
    let mut ws = open(&tunnels).await;
    let attached = tunnels.attach(only_tunnel()).expect("the tunnel is open");
    for _ in 0..4 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        ws.send(Message::Binary(Bytes::from_static(&[2; 40])))
            .await
            .unwrap();
    }
    settle(&tunnels, 1).await;
    drop(attached);
    tokio::time::sleep(grace + Duration::from_secs(1)).await;
    settle(&tunnels, 0).await;
    assert!(tunnels.attach(only_tunnel()).is_none());
}

#[tokio::test]
async fn browser_pages_cannot_open_tunnels() {
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    let server =
        tokio::spawn(
            async move { tunnel::accept(server_io, "/tpf3mp", false, |_| Some(())).await },
        );
    let client = client_async(request(Some("https://ads.example.com")), client_io).await;
    let refused = server.await.unwrap().err().unwrap();
    assert!(matches!(refused, TunnelError::Refused(_)), "{refused}");
    match client {
        Err(tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        other => panic!("a browser page got a tunnel: {:?}", other.map(|_| ())),
    }
}

#[tokio::test]
async fn an_address_over_its_share_is_told_so() {
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    let server =
        tokio::spawn(
            async move { tunnel::accept(server_io, "/tpf3mp", false, |_| None::<()>).await },
        );
    let client = client_async(request(None), client_io).await;
    assert!(server.await.unwrap().is_err());
    match client {
        Err(tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        }
        other => panic!("a refused address got a tunnel: {:?}", other.map(|_| ())),
    }
}
