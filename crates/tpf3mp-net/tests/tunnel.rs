//! QUIC over WebSocket: a server endpoint on a UDP socket merged with
//! tunnels serves clients on either at once, over plain WebSocket and TLS.

#![allow(clippy::unwrap_used)]

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use quinn::{Endpoint, EndpointConfig, Runtime, TokioRuntime};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tpf3mp_net::{
    ServerIdentity, ServerTrust, client_config, server_config,
    tunnel::{self, MuxSocket, TunnelUrl, Tunnels},
    tunnel_server_tls,
};

struct Server {
    endpoint: Endpoint,
    udp: SocketAddr,
    tunnel: SocketAddr,
    tunnels: Arc<Tunnels>,
    trust: ServerTrust,
}

/// A QUIC echo server whose endpoint also takes tunnels, served on a TCP
/// listener with TLS or without.
async fn start(tls: bool) -> Server {
    let identity = ServerIdentity::self_signed(&["localhost"]).unwrap();
    let trust = ServerTrust::Pinned(identity.leaf().clone());
    let runtime = Arc::new(TokioRuntime);
    let socket = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let udp = socket.local_addr().unwrap();
    let tunnels = Tunnels::new();
    let mux = MuxSocket::new(
        runtime.wrap_udp_socket(socket).unwrap(),
        Arc::clone(&tunnels),
    );
    let endpoint = Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
        Some(server_config(identity.clone()).unwrap()),
        Arc::new(mux),
        runtime,
    )
    .unwrap();

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let tunnel = listener.local_addr().unwrap();
    let acceptor = tls.then(|| TlsAcceptor::from(tunnel_server_tls(identity).unwrap()));
    let serving = Arc::clone(&tunnels);
    tokio::spawn(async move {
        loop {
            let (tcp, peer) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            let tunnels = Arc::clone(&serving);
            tokio::spawn(async move {
                match acceptor {
                    Some(acceptor) => {
                        let tls = acceptor.accept(tcp).await.unwrap();
                        let (ws, _) = tunnel::accept(tls, "/tpf3mp", false).await.unwrap();
                        tunnel::serve(ws, &tunnels, peer.ip()).await;
                    }
                    None => {
                        let (ws, _) = tunnel::accept(tcp, "/tpf3mp", false).await.unwrap();
                        tunnel::serve(ws, &tunnels, peer.ip()).await;
                    }
                }
            });
        }
    });

    // Echo every bidirectional stream back.
    let accepting = endpoint.clone();
    tokio::spawn(async move {
        while let Some(incoming) = accepting.accept().await {
            tokio::spawn(async move {
                let connection = incoming.await.unwrap();
                while let Ok((mut send, mut recv)) = connection.accept_bi().await {
                    tokio::spawn(async move {
                        let data = recv.read_to_end(64 << 20).await.unwrap();
                        send.write_all(&data).await.unwrap();
                        send.finish().unwrap();
                        let _ = send.stopped().await;
                    });
                }
            });
        }
    });
    Server {
        endpoint,
        udp,
        tunnel,
        tunnels,
        trust,
    }
}

/// Sends `data` on a new stream and returns what came back.
async fn echo(connection: &quinn::Connection, data: &[u8]) -> Vec<u8> {
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    send.write_all(data).await.unwrap();
    send.finish().unwrap();
    recv.read_to_end(64 << 20).await.unwrap()
}

async fn through_tunnel(server: &Server, url: &str) -> (Endpoint, quinn::Connection) {
    let url: TunnelUrl = url.parse().unwrap();
    let socket = tunnel::connect(&url, &server.trust, server.udp)
        .await
        .unwrap();
    let mut endpoint = Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
        None,
        socket,
        Arc::new(TokioRuntime),
    )
    .unwrap();
    endpoint.set_default_client_config(client_config(server.trust.clone()).unwrap());
    let connection = endpoint
        .connect(server.udp, "localhost")
        .unwrap()
        .await
        .unwrap();
    (endpoint, connection)
}

async fn over_udp(server: &Server) -> (Endpoint, quinn::Connection) {
    let mut endpoint = Endpoint::client((Ipv4Addr::LOCALHOST, 0).into()).unwrap();
    endpoint.set_default_client_config(client_config(server.trust.clone()).unwrap());
    let connection = endpoint
        .connect(server.udp, "localhost")
        .unwrap()
        .await
        .unwrap();
    (endpoint, connection)
}

#[tokio::test]
async fn a_tunneled_client_and_a_udp_client_share_one_endpoint() {
    let server = start(false).await;
    let url = format!("ws://{}/tpf3mp", server.tunnel);
    let (_tunnel_endpoint, tunneled) = through_tunnel(&server, &url).await;
    let (_udp_endpoint, direct) = over_udp(&server).await;

    assert_eq!(
        echo(&tunneled, b"through the tunnel").await,
        b"through the tunnel"
    );
    assert_eq!(echo(&direct, b"over UDP").await, b"over UDP");
    assert_eq!(server.tunnels.len(), 1);

    // The server sees the tunnel at an address of its own, and knows where
    // its client really is.
    let seen: Vec<SocketAddr> = {
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while seen.is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
            seen = (1..=16u64)
                .map(|index| {
                    let ip = std::net::Ipv6Addr::from_bits(
                        (0xfd74_7066_336d_7475u128 << 64) | u128::from(index),
                    );
                    SocketAddr::new(ip.into(), 443)
                })
                .filter(|addr| server.tunnels.origin(*addr).is_some())
                .collect();
        }
        seen
    };
    assert_eq!(seen.len(), 1);
    assert!(Tunnels::is_tunnel(seen[0]));
    assert_eq!(
        server.tunnels.origin(seen[0]),
        Some(Ipv4Addr::LOCALHOST.into())
    );
    server.endpoint.close(0u32.into(), b"");
}

#[tokio::test]
async fn megabytes_cross_a_tls_tunnel_intact() {
    let server = start(true).await;
    let url = format!("wss://localhost:{}/tpf3mp", server.tunnel.port());
    let (_endpoint, connection) = through_tunnel(&server, &url).await;
    let data: Vec<u8> = (0..8u32 << 20).map(|i| (i * 31 % 251) as u8).collect();
    let back = tokio::time::timeout(Duration::from_secs(60), echo(&connection, &data))
        .await
        .expect("the transfer finishes");
    assert!(back == data, "the echo matches");
    server.endpoint.close(0u32.into(), b"");
}

#[tokio::test]
async fn a_tunnel_ends_with_its_client() {
    let server = start(false).await;
    let url = format!("ws://{}/tpf3mp", server.tunnel);
    let (endpoint, connection) = through_tunnel(&server, &url).await;
    assert_eq!(echo(&connection, b"hello").await, b"hello");
    assert_eq!(server.tunnels.len(), 1);
    connection.close(0u32.into(), b"");
    endpoint.wait_idle().await;
    drop(connection);
    drop(endpoint);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !server.tunnels.is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "the tunnel closed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    server.endpoint.close(0u32.into(), b"");
}

#[tokio::test]
async fn a_tunnel_to_the_wrong_place_is_refused() {
    let server = start(false).await;
    let trust = server.trust.clone();
    let wrong: TunnelUrl = format!("ws://{}/elsewhere", server.tunnel).parse().unwrap();
    assert!(tunnel::connect(&wrong, &trust, server.udp).await.is_err());
    // A TLS client meets a plain listener: no tunnel either.
    let tls: TunnelUrl = format!("wss://localhost:{}/tpf3mp", server.tunnel.port())
        .parse()
        .unwrap();
    assert!(tunnel::connect(&tls, &trust, server.udp).await.is_err());
    server.endpoint.close(0u32.into(), b"");
}
