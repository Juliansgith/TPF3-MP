//! Snapshots at the edges: who may join a running game, and who may fetch or
//! upload a world. The whole flow, with games that really save and load, is
//! in `tpf3mp-testkit`'s scenarios.

#![allow(clippy::unwrap_used)]

mod common;

use std::path::Path;

use common::{FAST, Player, RunningServer, TestClient, content, join, seat};
use tpf3mp_agent::ClientError;
use tpf3mp_net::read_message;
use tpf3mp_proto::{
    BULK_REQUEST_MAX_FRAME, BULK_RESPONSE_MAX_FRAME, BulkOpen, BulkRequest, BulkResponse,
    FixedBytes, Invite, JoinRoom, RequestError, SnapshotId,
};
use tpf3mp_server::{ServerConfig, SnapshotConfig};

fn saving(dir: &Path) -> impl FnOnce(&mut ServerConfig) + use<> {
    let dir = dir.to_owned();
    move |config: &mut ServerConfig| {
        config.snapshots = Some(SnapshotConfig::new(dir.join("snapshots")));
    }
}

fn saving_and_logging(dir: &Path, secret: [u8; 32]) -> impl FnOnce(&mut ServerConfig) + use<> {
    let dir = dir.to_owned();
    move |config: &mut ServerConfig| {
        config.snapshots = Some(SnapshotConfig::new(dir.join("snapshots")));
        config.data_dir = Some(dir.join("rooms"));
        config.secret = secret;
    }
}

/// Seats the clients, starts the game and plays it a little, every player
/// at once: the room holds its clock until all have loaded.
async fn running(mut clients: Vec<TestClient>) -> (Vec<Player>, Invite) {
    let mut seats: Vec<&mut TestClient> = clients.iter_mut().collect();
    let invite = seat(&mut seats, FAST).await;
    clients[0].client.start_game().await.unwrap();
    let tasks: Vec<_> = clients
        .into_iter()
        .map(|client| {
            tokio::spawn(async move {
                let mut player = Player::new(client);
                player.play_until(|p| p.executed >= 1).await;
                player
            })
        })
        .collect();
    let mut players = Vec::new();
    for task in tasks {
        players.push(task.await.unwrap());
    }
    (players, invite)
}

fn newcomer(invite: &Invite, content_of: Option<u8>) -> JoinRoom {
    JoinRoom {
        content: content_of.map(content),
        ..join(invite)
    }
}

#[tokio::test]
async fn a_newcomer_must_run_the_games_content() {
    let dir = tempfile::tempdir().unwrap();
    let server = RunningServer::start(saving(dir.path())).await;
    let (_players, invite) = running(vec![server.client("ann").await]).await;
    let cat = server.client("cat").await;
    for wrong in [None, Some(2)] {
        assert_eq!(
            cat.client
                .join_room(newcomer(&invite, wrong))
                .await
                .unwrap_err(),
            ClientError::Refused(RequestError::ContentMismatch),
            "content {wrong:?}"
        );
    }
    let room = cat
        .client
        .join_room(newcomer(&invite, Some(1)))
        .await
        .unwrap();
    assert_eq!(room.members.len(), 2, "a seat at the running game");
    server.shut_down().await;
}

#[tokio::test]
async fn a_kicked_player_stays_out_even_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let secret = [3; 32];
    let server = RunningServer::start(saving_and_logging(dir.path(), secret)).await;
    let clients = vec![server.client("ann").await, server.client("bob").await];
    let (mut players, invite) = running(clients).await;
    let bob = players.pop().unwrap();
    let bob_identity = std::sync::Arc::clone(&bob.test.identity);
    players[0]
        .client()
        .kick(bob.test.client.player())
        .await
        .unwrap();
    drop(bob);
    let refused = ClientError::Refused(RequestError::BadInvite);
    let bob = server
        .client_as(std::sync::Arc::clone(&bob_identity), "bob")
        .await;
    assert_eq!(
        bob.client
            .join_room(newcomer(&invite, Some(1)))
            .await
            .unwrap_err(),
        refused
    );
    drop(bob);
    // Ann plays on, so the kick is logged.
    players[0].play_until(|p| p.executed >= 20).await;
    drop(players);
    server.shut_down().await;

    let server = RunningServer::start(saving_and_logging(dir.path(), secret)).await;
    let bob = server.client_as(bob_identity, "bob").await;
    assert_eq!(
        bob.client
            .join_room(newcomer(&invite, Some(1)))
            .await
            .unwrap_err(),
        refused,
        "the restored room remembers the kick"
    );
    server.shut_down().await;
}

#[tokio::test]
async fn nobody_fetches_a_world_they_were_not_offered() {
    let dir = tempfile::tempdir().unwrap();
    let server = RunningServer::start(saving(dir.path())).await;
    let (players, _invite) = running(vec![server.client("ann").await]).await;
    // A member of a running game, and someone in no room at all.
    let outsider = server.client("eve").await;
    for client in [players[0].client(), &outsider.client] {
        let (_send, mut recv) = client
            .bulk()
            .open(BulkOpen::Fetch {
                snapshot: SnapshotId(FixedBytes([0x5a; 32])),
            })
            .await
            .unwrap();
        let answer = read_message::<BulkResponse>(&mut recv, BULK_RESPONSE_MAX_FRAME)
            .await
            .unwrap();
        assert_eq!(answer, BulkResponse::Unavailable);
    }
    server.shut_down().await;
}

#[tokio::test]
async fn nobody_uploads_a_world_nobody_asked_for() {
    let dir = tempfile::tempdir().unwrap();
    let server = RunningServer::start(saving(dir.path())).await;
    let (players, _invite) = running(vec![server.client("ann").await]).await;
    let (_send, mut recv) = players[0]
        .client()
        .bulk()
        .open(BulkOpen::Serve {
            snapshot: SnapshotId(FixedBytes([0x5a; 32])),
        })
        .await
        .unwrap();
    // The server ends the stream without asking for anything.
    let request = read_message::<BulkRequest>(&mut recv, BULK_REQUEST_MAX_FRAME).await;
    assert!(
        request.as_ref().is_err_and(|error| error.is_disconnect()),
        "{request:?}"
    );
    server.shut_down().await;
}
