//! Players whose networks block UDP play through a WebSocket tunnel into the
//! same endpoint, rooms and limits as everyone else.

#![allow(clippy::unwrap_used)]

mod common;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use common::{FAST, Player, RunningServer, TestClient, new_identity, seat};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tpf3mp_agent::{ConnectError, Route, connect};
use tpf3mp_net::tunnel::TunnelUrl;
use tpf3mp_proto::{EventBody, Payload, RejectReason};
use tpf3mp_server::{ServerConfig, TunnelConfig};

fn tunneling(config: &mut ServerConfig) {
    config.tunnel = Some(TunnelConfig::new("127.0.0.1:0".parse().unwrap()));
}

fn tls_url(server: &RunningServer) -> TunnelUrl {
    let port = server.tunnel.unwrap().port();
    format!("wss://localhost:{port}/tpf3mp").parse().unwrap()
}

async fn connect_via(
    server: &RunningServer,
    name: &str,
    route: Route,
) -> Result<TestClient, ConnectError> {
    let identity = new_identity();
    let mut options = server.options(Arc::clone(&identity), name);
    options.route = route;
    let (client, events) = connect(options).await?;
    Ok(TestClient {
        client,
        events,
        identity,
    })
}

fn commands(player: &Player) -> usize {
    player
        .applied
        .iter()
        .filter(|event| matches!(event.body, EventBody::Command { .. }))
        .count()
}

#[tokio::test]
async fn a_tunneled_player_and_a_udp_player_share_a_game() {
    let server = RunningServer::start(tunneling).await;
    let mut ann = connect_via(&server, "ann", Route::Tunnel(tls_url(&server)))
        .await
        .unwrap();
    let mut bob = server.client("bob").await;
    assert!(ann.client.tunneled());
    assert!(!bob.client.tunneled());
    assert_eq!(server.stats.tunnels(), 1);

    seat(&mut [&mut ann, &mut bob], FAST).await;
    ann.client.start_game().await.unwrap();
    let mut players = [Player::new(ann), Player::new(bob)];
    players[0]
        .client()
        .send_intent(1, Payload::new(vec![1]).unwrap())
        .await
        .unwrap();
    players[1]
        .client()
        .send_intent(1, Payload::new(vec![2]).unwrap())
        .await
        .unwrap();
    // Lockstep: both play at once, or neither gets far.
    let (ann, bob) = players.split_at_mut(1);
    let done = |p: &Player| commands(p) == 2 && p.executed >= 30;
    tokio::join!(ann[0].play_until(done), bob[0].play_until(done));
    let shared = players[0].applied.len().min(players[1].applied.len());
    assert_eq!(players[0].applied[..shared], players[1].applied[..shared]);
    let metrics = server.stats.render_metrics();
    assert!(
        metrics.contains("tpf3mp_tunnels_opened_total 1\n"),
        "{metrics}"
    );
    assert!(metrics.contains("tpf3mp_tunnels 1\n"), "{metrics}");
    server.shut_down().await;
}

#[tokio::test]
async fn udp_that_gets_no_answer_falls_back_to_the_tunnel() {
    let server = RunningServer::start(tunneling).await;
    // UDP to this socket goes unanswered, as when a network drops it.
    let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let identity = new_identity();
    let mut options = server.options(identity, "ann");
    options.server = silent.local_addr().unwrap();
    options.route = Route::UdpOrTunnel(tls_url(&server));
    options.fallback_after = Duration::from_millis(200);
    let (client, _events) = connect(options).await.unwrap();
    assert!(client.tunneled(), "the tunnel won the race");

    // Where UDP gets through, it is kept.
    let identity = new_identity();
    let mut options = server.options(identity, "bob");
    options.route = Route::UdpOrTunnel(tls_url(&server));
    let (client, _events) = connect(options).await.unwrap();
    assert!(!client.tunneled());
    server.shut_down().await;
}

#[tokio::test]
async fn a_tunnel_nobody_serves_fails_to_connect() {
    let server = RunningServer::start(|_| {}).await;
    let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    // Nothing listens for tunnels here either.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url: TunnelUrl = format!(
        "wss://localhost:{}/tpf3mp",
        closed.local_addr().unwrap().port()
    )
    .parse()
    .unwrap();
    drop(closed);
    let mut options = server.options(new_identity(), "ann");
    options.server = silent.local_addr().unwrap();
    options.route = Route::Tunnel(url);
    let error = connect(options).await.unwrap_err();
    assert!(matches!(error, ConnectError::Tunnel(_)), "{error}");
    server.shut_down().await;
}

#[tokio::test]
async fn a_tunneled_client_counts_against_its_own_address() {
    let server = RunningServer::start(|config| {
        tunneling(config);
        config.max_sessions_per_address = 1;
    })
    .await;
    let _ann = connect_via(&server, "ann", Route::Tunnel(tls_url(&server)))
        .await
        .unwrap();
    // A second session from the same address, tunneled or not, is one too
    // many.
    for route in [Route::Tunnel(tls_url(&server)), Route::Udp] {
        let error = connect_via(&server, "eve", route).await.err().unwrap();
        assert!(
            matches!(
                error,
                ConnectError::Rejected(RejectReason::TooManyConnections)
            ),
            "{error}"
        );
    }
    server.shut_down().await;
}

/// A reverse proxy in front of a tunnel listener: it forwards one client's
/// connections and says they come from `client`.
async fn proxy(upstream: SocketAddr, client: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut downstream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8];
                while !head.ends_with(b"\r\n\r\n") {
                    if downstream.read_exact(&mut byte).await.is_err() {
                        return;
                    }
                    head.push(byte[0]);
                }
                head.truncate(head.len() - 2);
                // A client's own claim comes first; the proxy's goes last.
                head.extend_from_slice(b"X-Forwarded-For: 192.0.2.66\r\n");
                head.extend_from_slice(format!("X-Forwarded-For: {client}\r\n\r\n").as_bytes());
                let mut up = TcpStream::connect(upstream).await.unwrap();
                up.write_all(&head).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut downstream, &mut up).await;
            });
        }
    });
    address
}

#[tokio::test]
async fn behind_a_proxy_the_forwarded_address_is_the_clients() {
    let server = RunningServer::start(|config| {
        config.tunnel = Some(TunnelConfig::behind_proxy("127.0.0.1:0".parse().unwrap()));
        config.max_sessions_per_address = 1;
    })
    .await;
    let listener = server.tunnel.unwrap();
    let via = |proxy: SocketAddr| Route::Tunnel(format!("ws://{proxy}/tpf3mp").parse().unwrap());
    let first = proxy(listener, "203.0.113.1").await;
    let second = proxy(listener, "203.0.113.2").await;

    // Two players behind the proxy are two addresses, though every
    // connection reaches the server from the proxy's.
    let _ann = connect_via(&server, "ann", via(first)).await.unwrap();
    let _bob = connect_via(&server, "bob", via(second)).await.unwrap();
    let error = connect_via(&server, "eve", via(first)).await.err().unwrap();
    assert!(
        matches!(
            error,
            ConnectError::Rejected(RejectReason::TooManyConnections)
        ),
        "{error}"
    );

    // Straight to the listener, without the proxy's word, nobody gets in.
    let direct = connect_via(&server, "mallory", via(listener))
        .await
        .err()
        .unwrap();
    assert!(matches!(direct, ConnectError::Tunnel(_)), "{direct}");
    server.shut_down().await;
}
