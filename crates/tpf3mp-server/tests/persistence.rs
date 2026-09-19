//! Running games survive a server restart: the room is restored from its log,
//! players reconnect with their identities and resume after their last turn.

#![allow(clippy::unwrap_used)]

mod common;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use common::{FAST, Player, RunningServer, TestClient, seat};
use tpf3mp_agent::ClientError;
use tpf3mp_proto::{EventBody, Invite, JoinRoom, Payload, RequestError};
use tpf3mp_server::ServerConfig;

fn data_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tpf3mp-persist-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn persistent(dir: &Path, secret: [u8; 32]) -> impl FnOnce(&mut ServerConfig) {
    let dir = dir.to_owned();
    move |config: &mut ServerConfig| {
        config.data_dir = Some(dir);
        config.secret = secret;
    }
}

fn commands(player: &Player) -> usize {
    player
        .applied
        .iter()
        .filter(|event| matches!(event.body, EventBody::Command { .. }))
        .count()
}

async fn play_all(players: Vec<Player>, done: fn(&Player) -> bool) -> Vec<Player> {
    let tasks: Vec<_> = players
        .into_iter()
        .map(|mut player| {
            tokio::spawn(async move {
                player.play_until(done).await;
                player
            })
        })
        .collect();
    let mut finished = Vec::new();
    for task in tasks {
        finished.push(task.await.unwrap());
    }
    finished
}

/// Reconnects every player to `server` with the same identity and resumes
/// after its last applied turn, keeping its game state.
async fn resume(server: &RunningServer, players: Vec<Player>, invite: &Invite) -> Vec<Player> {
    let mut resumed = Vec::new();
    for old in players {
        let identity = Arc::clone(&old.test.identity);
        let Player {
            follower,
            start,
            applied,
            executed,
            ..
        } = old;
        let mut player = Player::new(server.client_as(identity, "back").await);
        let resume = follower.as_ref().map(|f| f.resume_point());
        player.follower = follower;
        player.start = start;
        player.applied = applied;
        player.executed = executed;
        player
            .client()
            .join_room(JoinRoom {
                invite: invite.clone(),
                password: None,
                resume,
                content: None,
            })
            .await
            .unwrap();
        resumed.push(player);
    }
    resumed
}

#[tokio::test]
async fn a_running_game_survives_a_server_restart() {
    let dir = data_dir("restart");
    let secret = [7; 32];

    let first = RunningServer::start(persistent(&dir, secret)).await;
    let mut clients: Vec<TestClient> = vec![first.client("ann").await, first.client("bob").await];
    let mut seats: Vec<&mut TestClient> = clients.iter_mut().collect();
    let invite = seat(&mut seats, FAST).await;
    clients[0].client.start_game().await.unwrap();
    let players: Vec<Player> = clients.into_iter().map(Player::new).collect();
    players[0]
        .client()
        .send_intent(1, Payload::new(vec![1]).unwrap())
        .await
        .unwrap();
    let players = play_all(players, |p| commands(p) == 1 && p.executed >= 20).await;
    first.shut_down().await;

    // A new server process on the same data directory and secret.
    let second = RunningServer::start(persistent(&dir, secret)).await;
    assert_eq!(second.stats.rooms(), 1, "the room was restored");
    let players = resume(&second, players, &invite).await;
    players[1]
        .client()
        .send_intent(2, Payload::new(vec![2]).unwrap())
        .await
        .unwrap();
    let players = play_all(players, |p| commands(p) == 2 && p.executed >= 60).await;

    // Both replicas continued one uninterrupted log across the restart.
    let shared = players[0].applied.len().min(players[1].applied.len());
    assert_eq!(players[0].applied[..shared], players[1].applied[..shared]);
    let seqs: Vec<u64> = players[0].applied.iter().map(|event| event.seq).collect();
    assert!(
        seqs.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "{seqs:?}"
    );
    second.shut_down().await;
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn a_finished_game_leaves_no_log_behind() {
    let dir = data_dir("finished");
    let secret = [8; 32];
    let server = RunningServer::start(persistent(&dir, secret)).await;
    let mut clients = vec![server.client("ann").await];
    let mut seats: Vec<&mut TestClient> = clients.iter_mut().collect();
    seat(&mut seats, FAST).await;
    clients[0].client.start_game().await.unwrap();
    let mut player = Player::new(clients.pop().unwrap());
    player.play_until(|p| p.executed >= 5).await;
    let logs = || std::fs::read_dir(&dir).unwrap().count();
    assert_eq!(logs(), 1, "a running game is logged");
    player.client().leave_room().await.unwrap();
    server.wait_for_rooms(0).await;
    assert_eq!(logs(), 0, "the log went with the game");
    server.shut_down().await;
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn a_restored_game_nobody_returns_to_closes_with_its_log() {
    let dir = data_dir("abandoned");
    let secret = [5; 32];
    let server = RunningServer::start(persistent(&dir, secret)).await;
    let mut clients = vec![server.client("ann").await];
    let mut seats: Vec<&mut TestClient> = clients.iter_mut().collect();
    seat(&mut seats, FAST).await;
    clients[0].client.start_game().await.unwrap();
    let mut player = Player::new(clients.pop().unwrap());
    player.play_until(|p| p.executed >= 3).await;
    drop(player);
    server.shut_down().await;

    let abandoned = |config: &mut ServerConfig| {
        persistent(&dir, secret)(config);
        config.abandoned_timeout = Duration::from_millis(300);
    };
    let server = RunningServer::start(abandoned).await;
    assert_eq!(server.stats.rooms(), 1, "restored");
    server.wait_for_rooms(0).await;
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "log deleted");
    server.shut_down().await;
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn restored_rooms_need_the_same_secret() {
    let dir = data_dir("secret");
    let server = RunningServer::start(persistent(&dir, [1; 32])).await;
    let mut clients = vec![server.client("ann").await];
    let mut seats: Vec<&mut TestClient> = clients.iter_mut().collect();
    let invite = seat(&mut seats, FAST).await;
    clients[0].client.start_game().await.unwrap();
    let mut player = Player::new(clients.pop().unwrap());
    player.play_until(|p| p.executed >= 3).await;
    let identity = Arc::clone(&player.test.identity);
    drop(player);
    server.shut_down().await;

    // Restarted with a different key, the old invite no longer verifies.
    let server = RunningServer::start(persistent(&dir, [2; 32])).await;
    assert_eq!(server.stats.rooms(), 1);
    let ann = server.client_as(identity, "ann").await;
    let error = ann
        .client
        .join_room(JoinRoom {
            invite,
            password: None,
            resume: None,
            content: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error, ClientError::Refused(RequestError::BadInvite));
    tokio::time::sleep(Duration::from_millis(10)).await;
    server.shut_down().await;
    std::fs::remove_dir_all(&dir).unwrap();
}
