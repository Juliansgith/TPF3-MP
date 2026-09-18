//! End-to-end handshake tests: a real server and real clients over loopback QUIC.

// `allow-unwrap-in-tests` covers `#[test]` functions only, not the helpers here.
#![allow(clippy::unwrap_used)]

use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

use tokio::{sync::oneshot, task::JoinHandle};
use tpf3mp_agent::{ConnectError, ConnectOptions, Session, connect};
use tpf3mp_net::{
    ServerIdentity, ServerTrust, client_config, close, read_preamble, write_message, write_preamble,
};
use tpf3mp_proto::{
    CONTROL_MAX_FRAME, Message, PROTOCOL_VERSION, RejectReason, SessionId, Text, Welcome,
};
use tpf3mp_server::{Server, ServerConfig};

struct RunningServer {
    address: SocketAddr,
    trust: ServerTrust,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl RunningServer {
    async fn start(configure: impl FnOnce(&mut ServerConfig)) -> Self {
        let identity = ServerIdentity::self_signed(&["localhost"]).unwrap();
        let trust = ServerTrust::Pinned(identity.leaf().clone());
        let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), identity);
        configure(&mut config);
        let server = Server::bind(config).unwrap();
        let address = server.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(server.run(async {
            let _ = stopped.await;
        }));
        Self {
            address,
            trust,
            stop: Some(stop),
            task,
        }
    }

    fn options(&self) -> ConnectOptions {
        ConnectOptions::new(self.address, "localhost", self.trust.clone())
    }

    async fn shut_down(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        tokio::time::timeout(Duration::from_secs(10), &mut self.task)
            .await
            .expect("the server drains within 10 s")
            .unwrap();
    }

    /// A bare QUIC connection that speaks whatever the test sends.
    async fn raw_connection(&self) -> (quinn::Endpoint, quinn::Connection) {
        let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        endpoint.set_default_client_config(client_config(self.trust.clone()).unwrap());
        let connection = endpoint
            .connect(self.address, "localhost")
            .unwrap()
            .await
            .unwrap();
        (endpoint, connection)
    }
}

fn application_close_code(error: &quinn::ConnectionError) -> Option<quinn::VarInt> {
    match error {
        quinn::ConnectionError::ApplicationClosed(close) => Some(close.error_code),
        _ => None,
    }
}

async fn closed_within(connection: &quinn::Connection, limit: Duration) -> quinn::ConnectionError {
    tokio::time::timeout(limit, connection.closed())
        .await
        .expect("the server closes the connection")
}

/// Connects, retrying while the server still counts a session that is
/// closing: slots are freed asynchronously when the close arrives.
async fn connect_when_a_slot_frees(server: &RunningServer) -> Session {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match connect(server.options()).await {
            Ok(session) => return session,
            Err(ConnectError::Rejected(RejectReason::ServerFull)) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("connect failed: {error}"),
        }
    }
}

#[tokio::test]
async fn handshake_opens_a_session() {
    let server = RunningServer::start(|_| {}).await;
    let session = connect(server.options()).await.unwrap();
    assert_eq!(
        session.welcome().server_version.as_str(),
        env!("CARGO_PKG_VERSION")
    );
    let other = connect(server.options()).await.unwrap();
    assert_ne!(
        session.welcome().session_id,
        other.welcome().session_id,
        "every session gets its own ID"
    );
    session.close().await;
    other.close().await;
    server.shut_down().await;
}

#[tokio::test]
async fn version_mismatch_is_reported_with_both_versions() {
    let server = RunningServer::start(|_| {}).await;
    let mut options = server.options();
    options.protocol_version = PROTOCOL_VERSION + 1;
    let error = connect(options).await.unwrap_err();
    assert!(
        matches!(
            error,
            ConnectError::VersionMismatch { client, server }
                if client == PROTOCOL_VERSION + 1 && server == PROTOCOL_VERSION
        ),
        "{error}"
    );
    assert!(error.to_string().contains("not been updated"), "{error}");
    server.shut_down().await;
}

#[tokio::test]
async fn full_server_rejects_with_a_reason_and_recovers() {
    let server = RunningServer::start(|config| config.max_sessions = 1).await;
    let first = connect(server.options()).await.unwrap();
    let error = connect(server.options()).await.unwrap_err();
    assert!(
        matches!(error, ConnectError::Rejected(RejectReason::ServerFull)),
        "{error}"
    );
    first.close().await;
    let second = connect_when_a_slot_frees(&server).await;
    second.close().await;
    server.shut_down().await;
}

#[tokio::test]
async fn untrusted_certificate_is_refused() {
    let server = RunningServer::start(|_| {}).await;
    let impostor = ServerIdentity::self_signed(&["localhost"]).unwrap();
    let mut options = server.options();
    options.trust = ServerTrust::Pinned(impostor.leaf().clone());
    let error = connect(options).await.unwrap_err();
    assert!(matches!(error, ConnectError::Connection(_)), "{error}");
    server.shut_down().await;
}

#[tokio::test]
async fn oversized_frame_is_a_protocol_violation() {
    let server = RunningServer::start(|_| {}).await;
    let (_endpoint, connection) = server.raw_connection().await;
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    write_preamble(&mut send, PROTOCOL_VERSION).await.unwrap();
    assert_eq!(read_preamble(&mut recv).await.unwrap(), PROTOCOL_VERSION);
    let too_large = u32::try_from(CONTROL_MAX_FRAME + 1).unwrap();
    send.write_all(&too_large.to_le_bytes()).await.unwrap();
    let reason = closed_within(&connection, Duration::from_secs(5)).await;
    assert_eq!(
        application_close_code(&reason),
        Some(close::PROTOCOL_VIOLATION)
    );
    server.shut_down().await;
}

#[tokio::test]
async fn foreign_protocol_is_a_protocol_violation() {
    let server = RunningServer::start(|_| {}).await;
    let (_endpoint, connection) = server.raw_connection().await;
    let (mut send, _recv) = connection.open_bi().await.unwrap();
    send.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
    let reason = closed_within(&connection, Duration::from_secs(5)).await;
    assert_eq!(
        application_close_code(&reason),
        Some(close::PROTOCOL_VIOLATION)
    );
    server.shut_down().await;
}

#[tokio::test]
async fn first_message_must_be_hello() {
    let server = RunningServer::start(|_| {}).await;
    let (_endpoint, connection) = server.raw_connection().await;
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    write_preamble(&mut send, PROTOCOL_VERSION).await.unwrap();
    read_preamble(&mut recv).await.unwrap();
    let not_hello = Message::Welcome(Welcome {
        server_version: Text::new("0.0.0").unwrap(),
        session_id: SessionId([0; 16]),
    });
    write_message(&mut send, &not_hello, CONTROL_MAX_FRAME)
        .await
        .unwrap();
    let reason = closed_within(&connection, Duration::from_secs(5)).await;
    assert_eq!(
        application_close_code(&reason),
        Some(close::PROTOCOL_VIOLATION)
    );
    server.shut_down().await;
}

#[tokio::test]
async fn silent_client_is_dropped_after_the_handshake_timeout() {
    let server = RunningServer::start(|config| {
        config.handshake_timeout = Duration::from_millis(200);
    })
    .await;
    let (_endpoint, connection) = server.raw_connection().await;
    let reason = closed_within(&connection, Duration::from_secs(5)).await;
    assert_eq!(
        application_close_code(&reason),
        Some(close::HANDSHAKE_TIMEOUT)
    );
    server.shut_down().await;
}

#[tokio::test]
async fn shutdown_closes_open_sessions() {
    let server = RunningServer::start(|_| {}).await;
    let session = connect(server.options()).await.unwrap();
    server.shut_down().await;
    let reason = tokio::time::timeout(Duration::from_secs(5), session.closed())
        .await
        .expect("the session learns about the shutdown");
    assert_eq!(application_close_code(&reason), Some(close::SHUTTING_DOWN));
}
