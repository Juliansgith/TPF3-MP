//! Handshake and identity: version checks, identity proofs, limits and
//! hostile clients.

#![allow(clippy::unwrap_used)]

mod common;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use common::{RunningServer, application_close_code, new_identity};
use tpf3mp_agent::{ConnectError, connect};
use tpf3mp_net::{
    Identity, ServerIdentity, ServerTrust, close, read_message, read_preamble, write_message,
    write_preamble,
};
use tpf3mp_proto::{
    CONTROL_MAX_FRAME, ClientMessage, Hello, PROTOCOL_VERSION, Platform, RejectReason, Request,
    ServerMessage, Text,
};

async fn closed_within(connection: &quinn::Connection) -> quinn::ConnectionError {
    tokio::time::timeout(common::WAIT, connection.closed())
        .await
        .expect("the server closes the connection")
}

#[tokio::test]
async fn handshake_opens_a_session_for_a_proven_identity() {
    let server = RunningServer::start(|_| {}).await;
    let identity = new_identity();
    let (client, _events) = connect(server.options(Arc::clone(&identity), "ann"))
        .await
        .unwrap();
    assert_eq!(client.player(), identity.player());
    assert_eq!(
        client.welcome().server_version.as_str(),
        env!("CARGO_PKG_VERSION")
    );
    let (other, _other_events) = connect(server.options(new_identity(), "bob"))
        .await
        .unwrap();
    assert_ne!(client.welcome().session_id, other.welcome().session_id);
    client.close().await;
    other.close().await;
    server.shut_down().await;
}

#[tokio::test]
async fn version_mismatch_is_reported_with_both_versions() {
    let server = RunningServer::start(|_| {}).await;
    let mut options = server.options(new_identity(), "ann");
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

/// Sends a hand-made Hello on a raw connection and returns the answer.
async fn raw_hello(
    server: &RunningServer,
    make: impl FnOnce(&quinn::Connection) -> Hello,
) -> (ServerMessage, quinn::Connection) {
    let (_endpoint, connection) = server.raw_connection().await;
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    write_preamble(&mut send, PROTOCOL_VERSION).await.unwrap();
    assert_eq!(read_preamble(&mut recv).await.unwrap(), PROTOCOL_VERSION);
    let hello = ClientMessage::Hello(make(&connection));
    write_message(&mut send, &hello, CONTROL_MAX_FRAME)
        .await
        .unwrap();
    let answer = read_message::<ServerMessage>(&mut recv, CONTROL_MAX_FRAME)
        .await
        .unwrap();
    (answer, connection)
}

fn hello(identity: &Identity, proof_by: &Identity, connection: &quinn::Connection) -> Hello {
    Hello {
        client_version: Text::new("test").unwrap(),
        platform: Platform::current(),
        name: Text::new("mallory").unwrap(),
        identity: identity.player(),
        proof: proof_by.prove(connection).unwrap(),
    }
}

#[tokio::test]
async fn a_proof_by_another_key_is_rejected() {
    let server = RunningServer::start(|_| {}).await;
    let victim = new_identity();
    let attacker = new_identity();
    let (answer, connection) =
        raw_hello(&server, |connection| hello(&victim, &attacker, connection)).await;
    assert!(
        matches!(&answer, ServerMessage::Reject(reject) if reject.reason == RejectReason::BadProof),
        "{answer:?}"
    );
    let reason = closed_within(&connection).await;
    assert_eq!(application_close_code(&reason), Some(close::REJECTED));
    server.shut_down().await;
}

#[tokio::test]
async fn a_proof_does_not_replay_on_another_connection() {
    let server = RunningServer::start(|_| {}).await;
    let victim = new_identity();
    // A genuine proof, captured from the victim's first connection...
    let (_first_endpoint, first) = server.raw_connection().await;
    let captured = victim.prove(&first).unwrap();
    // ...is worthless on any other connection: it signs that TLS session.
    let (answer, _connection) = raw_hello(&server, |_| Hello {
        client_version: Text::new("test").unwrap(),
        platform: Platform::current(),
        name: Text::new("mallory").unwrap(),
        identity: victim.player(),
        proof: captured,
    })
    .await;
    assert!(
        matches!(&answer, ServerMessage::Reject(reject) if reject.reason == RejectReason::BadProof),
        "{answer:?}"
    );
    server.shut_down().await;
}

#[tokio::test]
async fn full_server_rejects_with_a_reason_and_recovers() {
    let server = RunningServer::start(|config| config.max_sessions = 1).await;
    let (first, _events) = connect(server.options(new_identity(), "ann"))
        .await
        .unwrap();
    let error = connect(server.options(new_identity(), "bob"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, ConnectError::Rejected(RejectReason::ServerFull)),
        "{error}"
    );
    first.close().await;
    // The slot frees once the server has processed the close.
    let deadline = Instant::now() + common::WAIT;
    loop {
        match connect(server.options(new_identity(), "bob")).await {
            Ok((second, _events)) => {
                second.close().await;
                break;
            }
            Err(ConnectError::Rejected(RejectReason::ServerFull)) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("connect failed: {error}"),
        }
    }
    server.shut_down().await;
}

#[tokio::test]
async fn untrusted_certificate_is_refused() {
    let server = RunningServer::start(|_| {}).await;
    let impostor = ServerIdentity::self_signed(&["localhost"]).unwrap();
    let mut options = server.options(new_identity(), "ann");
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
    read_preamble(&mut recv).await.unwrap();
    let too_large = u32::try_from(CONTROL_MAX_FRAME + 1).unwrap();
    send.write_all(&too_large.to_le_bytes()).await.unwrap();
    let reason = closed_within(&connection).await;
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
    let reason = closed_within(&connection).await;
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
    let not_hello = ClientMessage::Request {
        id: 1,
        request: Request::LeaveRoom,
    };
    write_message(&mut send, &not_hello, CONTROL_MAX_FRAME)
        .await
        .unwrap();
    let reason = closed_within(&connection).await;
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
    let reason = closed_within(&connection).await;
    assert_eq!(
        application_close_code(&reason),
        Some(close::HANDSHAKE_TIMEOUT)
    );
    server.shut_down().await;
}

#[tokio::test]
async fn an_address_holds_only_its_share_of_sessions() {
    let server = RunningServer::start(|config| config.max_sessions_per_address = 2).await;
    let (first, _a) = connect(server.options(new_identity(), "ann"))
        .await
        .unwrap();
    let (_second, _b) = connect(server.options(new_identity(), "bob"))
        .await
        .unwrap();
    let error = connect(server.options(new_identity(), "eve"))
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            ConnectError::Rejected(RejectReason::TooManyConnections)
        ),
        "{error}"
    );
    first.close().await;
    // The share frees once the server has processed the close.
    let deadline = Instant::now() + common::WAIT;
    loop {
        match connect(server.options(new_identity(), "eve")).await {
            Ok((third, _events)) => {
                third.close().await;
                break;
            }
            Err(ConnectError::Rejected(RejectReason::TooManyConnections))
                if Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("connect failed: {error}"),
        }
    }
    server.shut_down().await;
}

#[tokio::test]
async fn an_address_has_only_so_many_handshakes_in_progress() {
    let server = RunningServer::start(|config| config.max_handshakes_per_address = 1).await;
    // Connected, but the handshake never goes further.
    let (_endpoint, _pending) = server.raw_connection().await;
    let error = connect(server.options(new_identity(), "ann"))
        .await
        .unwrap_err();
    assert!(matches!(error, ConnectError::Connection(_)), "{error}");
    server.shut_down().await;
}

#[tokio::test]
async fn under_load_a_client_proves_its_address_and_still_gets_in() {
    let server = RunningServer::start(|config| config.max_handshakes = 2).await;
    // One handshake in progress is half the capacity.
    let (_endpoint, _pending) = server.raw_connection().await;
    let (client, _events) = connect(server.options(new_identity(), "ann"))
        .await
        .unwrap();
    let metrics = server.stats.render_metrics();
    assert!(
        metrics.contains("tpf3mp_retries_sent_total 1\n"),
        "{metrics}"
    );
    client.close().await;
    server.shut_down().await;
}

#[tokio::test]
async fn a_session_idle_outside_any_room_is_closed() {
    let server = RunningServer::start(|config| {
        config.roomless_timeout = Duration::from_millis(300);
    })
    .await;
    let mut idle = server.client("idle").await;
    let host = server.client("host").await;
    host.client
        .create_room(common::room("kept", common::FAST))
        .await
        .unwrap();
    let reason = idle.closed().await;
    assert_eq!(application_close_code(&reason), Some(close::IDLE));
    // A member of a room is not idle.
    tokio::time::sleep(Duration::from_millis(400)).await;
    host.client.set_ready(true).await.unwrap();
    server.shut_down().await;
}

#[tokio::test]
async fn the_server_ends_a_session_whose_control_stream_ended() {
    let server = RunningServer::start(|_| {}).await;
    let identity = new_identity();
    let (_endpoint, connection) = server.raw_connection().await;
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    write_preamble(&mut send, PROTOCOL_VERSION).await.unwrap();
    read_preamble(&mut recv).await.unwrap();
    let hello = ClientMessage::Hello(hello(&identity, &identity, &connection));
    write_message(&mut send, &hello, CONTROL_MAX_FRAME)
        .await
        .unwrap();
    let _welcome = read_message::<ServerMessage>(&mut recv, CONTROL_MAX_FRAME)
        .await
        .unwrap();
    send.finish().unwrap();
    let reason = closed_within(&connection).await;
    assert_eq!(application_close_code(&reason), Some(close::NORMAL));
    server.shut_down().await;
}

#[tokio::test]
async fn shutdown_closes_open_sessions() {
    let server = RunningServer::start(|_| {}).await;
    let mut ann = server.client("ann").await;
    server.shut_down().await;
    let reason = ann.closed().await;
    assert_eq!(application_close_code(&reason), Some(close::SHUTTING_DOWN));
}
