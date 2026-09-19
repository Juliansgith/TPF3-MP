//! Running games: the ordered event log, pacing, validation, resumption and
//! checkpoint verdicts. Every turn is checked by `TurnFollower`, so any turn
//! the server sends that breaks a protocol invariant fails these tests.

#![allow(clippy::unwrap_used)]

mod common;

use std::{sync::Arc, time::Duration};

use common::{FAST, Player, RunningServer, TestClient, application_close_code, join, seat};
use tpf3mp_agent::ClientError;
use tpf3mp_net::close;
use tpf3mp_proto::{
    Event, EventBody, FixedBytes, IntentRejection, Invite, JoinRoom, LaneDigest, Payload, PlayerId,
    RequestError, Resume, RoomSettings, Speed, Text,
};
use tpf3mp_server::{RulesChoice, RulesMenu, Ruleset};

fn payload(bytes: &[u8]) -> Payload {
    Payload::new(bytes.to_vec()).unwrap()
}

fn commands(player: &Player) -> Vec<(PlayerId, u64, Vec<u8>)> {
    player
        .applied
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::Command {
                player,
                client_seq,
                payload,
            } => Some((*player, *client_seq, payload.as_bytes().to_vec())),
            _ => None,
        })
        .collect()
}

/// Seats the clients (the first owns the room), starts the game and returns
/// the players with the room's invite.
async fn start(mut clients: Vec<TestClient>, settings: RoomSettings) -> (Vec<Player>, Invite) {
    let mut seats: Vec<&mut TestClient> = clients.iter_mut().collect();
    let invite = seat(&mut seats, settings).await;
    clients[0].client.start_game().await.unwrap();
    (clients.into_iter().map(Player::new).collect(), invite)
}

/// Runs every player until `done` holds for it, concurrently, as separate
/// machines would.
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

#[tokio::test]
async fn everyone_applies_the_same_log_in_the_same_order() {
    let server = RunningServer::start(|_| {}).await;
    let clients = vec![
        server.client("ann").await,
        server.client("bob").await,
        server.client("cat").await,
    ];
    let order: Vec<PlayerId> = clients.iter().map(|c| c.client.player()).collect();
    let (players, _) = start(clients, FAST).await;

    let mut tasks = Vec::new();
    for (index, mut player) in players.into_iter().enumerate() {
        tasks.push(tokio::spawn(async move {
            let sender = u8::try_from(index).unwrap();
            for seq in 0..10u8 {
                player
                    .client()
                    .send_intent(u64::from(seq), payload(&[sender, seq]))
                    .await
                    .unwrap();
                player.play_for(Duration::from_millis(15)).await;
            }
            player
                .play_until(|p| commands(p).len() == 30 && p.executed >= 40)
                .await;
            player
        }));
    }
    let mut players = Vec::new();
    for task in tasks {
        players.push(task.await.unwrap());
    }

    let reference: &Vec<Event> = &players[0].applied;
    for player in &players[1..] {
        assert_eq!(&player.applied, reference, "every replica applies one log");
    }
    // The log opens with the table, in join order.
    let joined: Vec<PlayerId> = reference[..3]
        .iter()
        .map(|event| match &event.body {
            EventBody::PlayerJoined { player, .. } => *player,
            other => panic!("expected PlayerJoined, got {other:?}"),
        })
        .collect();
    assert_eq!(joined, order);
    // Each sender's intents keep their order, and steps never go back.
    for sender in &order {
        let seqs: Vec<u64> = commands(&players[0])
            .into_iter()
            .filter(|(player, ..)| player == sender)
            .map(|(_, seq, _)| seq)
            .collect();
        assert_eq!(seqs, (0..10).collect::<Vec<_>>());
    }
    assert!(
        reference
            .windows(2)
            .all(|pair| pair[0].step <= pair[1].step)
    );
    server.shut_down().await;
}

#[tokio::test]
async fn the_clock_waits_until_everyone_has_loaded() {
    let server = RunningServer::start(|_| {}).await;
    let clients = vec![server.client("ann").await, server.client("bob").await];
    let (mut players, _) = start(clients, FAST).await;
    let mut bob = players.pop().unwrap();
    let mut ann = players.pop().unwrap();
    bob.reports_progress = false;

    ann.play_for(Duration::from_millis(400)).await;
    let follower = ann.follower.as_ref().expect("the game started");
    assert_eq!(follower.sealed_through(), 0, "held while bob is loading");

    // Bob finishes loading.
    bob.test.client.report_progress(0).await.unwrap();
    ann.play_until(|p| p.executed >= 10).await;
    server.shut_down().await;
}

#[tokio::test]
async fn a_slow_player_holds_the_room() {
    let server = RunningServer::start(|_| {}).await;
    let clients = vec![server.client("ann").await, server.client("bob").await];
    let (mut players, _) = start(clients, FAST).await;
    let bob = players.pop().unwrap();
    let mut ann = players.pop().unwrap();
    // Bob loads, then never executes a step: his machine cannot keep up.
    bob.test.client.report_progress(0).await.unwrap();

    // Three seconds at 50 steps per second would be 150 steps.
    ann.play_for(Duration::from_secs(3)).await;
    let sealed = ann.follower.as_ref().unwrap().sealed_through();
    // The window: a 2-step lead plus two seconds (100 steps) past step 0.
    assert!(
        sealed <= 102,
        "the frontier ran {sealed} steps ahead of bob"
    );
    assert!(sealed >= 90, "the frontier stalled early at {sealed}");
    drop(bob);
    server.shut_down().await;
}

#[tokio::test]
async fn a_stalled_player_stops_holding_the_room() {
    let server = RunningServer::start(|config| {
        config.stall_timeout = Duration::from_millis(500);
    })
    .await;
    let clients = vec![server.client("ann").await, server.client("bob").await];
    let (mut players, _) = start(clients, FAST).await;
    let bob = players.pop().unwrap();
    let mut ann = players.pop().unwrap();
    // Bob loads, then his game freezes while his connection stays up.
    bob.test.client.report_progress(0).await.unwrap();

    ann.play_for(Duration::from_secs(3)).await;
    let sealed = ann.follower.as_ref().unwrap().sealed_through();
    // Held, the room would stop at 102 steps (see a_slow_player_holds_the_room).
    // Released after half a second, it runs on at 50 steps per second.
    assert!(
        sealed > 120,
        "the room is still held at {sealed} by a frozen player"
    );
    drop(bob);
    server.shut_down().await;
}

#[tokio::test]
async fn a_player_that_never_loads_is_released_after_the_load_timeout() {
    let server = RunningServer::start(|config| {
        config.load_timeout = Duration::from_millis(500);
    })
    .await;
    let clients = vec![server.client("ann").await, server.client("bob").await];
    let (mut players, _) = start(clients, FAST).await;
    let mut bob = players.pop().unwrap();
    let mut ann = players.pop().unwrap();
    bob.reports_progress = false;

    ann.play_for(Duration::from_millis(300)).await;
    assert_eq!(ann.follower.as_ref().unwrap().sealed_through(), 0);
    ann.play_until(|p| p.executed >= 20).await;
    drop(bob);
    server.shut_down().await;
}

#[tokio::test]
async fn bursts_are_rate_limited() {
    let server = RunningServer::start(|_| {}).await;
    let (mut players, _) = start(vec![server.client("ann").await], FAST).await;
    let mut ann = players.pop().unwrap();
    ann.play_until(|p| p.executed >= 1).await;
    for seq in 0..100 {
        ann.client().send_intent(seq, payload(&[1])).await.unwrap();
    }
    ann.play_until(|p| commands(p).len() + p.rejections.len() == 100)
        .await;
    assert!(
        !ann.rejections.is_empty(),
        "a burst of 100 exceeds the limit"
    );
    assert!(
        ann.rejections
            .iter()
            .all(|(_, reason)| *reason == IntentRejection::RateLimited)
    );
    assert!(
        commands(&ann).len() >= 40,
        "the burst allowance is honoured"
    );
    server.shut_down().await;
}

struct RefuseFf;

impl Ruleset for RefuseFf {
    fn validate(&self, _player: &PlayerId, payload: &Payload) -> Result<(), u16> {
        match payload.as_bytes().first() {
            Some(0xff) => Err(7),
            _ => Ok(()),
        }
    }

    fn apply(&mut self, _event: &Event) {}
}

#[tokio::test]
async fn ruleset_refusals_reach_only_the_sender() {
    let server = RunningServer::start(|config| {
        config.rules = RulesMenu::single(RulesChoice {
            name: Text::new("refuse-ff").unwrap(),
            description: Text::new("Refuses intents starting with 0xff").unwrap(),
            factory: Arc::new(|| Box::new(RefuseFf)),
        });
    })
    .await;
    let clients = vec![server.client("ann").await, server.client("bob").await];
    let (players, _) = start(clients, FAST).await;
    players[0]
        .client()
        .send_intent(1, payload(&[0xff]))
        .await
        .unwrap();
    players[0]
        .client()
        .send_intent(2, payload(&[0x01]))
        .await
        .unwrap();
    let mut players = play_all(players, |p| commands(p).len() == 1).await;
    // The refusal travels on the control stream, the command on the turn
    // stream, so either may arrive first.
    players[0].play_until(|p| !p.rejections.is_empty()).await;
    assert_eq!(
        players[0].rejections,
        vec![(1, IntentRejection::Refused { code: 7 })]
    );
    assert!(players[1].rejections.is_empty());
    assert_eq!(commands(&players[1]), commands(&players[0]));
    assert_eq!(commands(&players[1])[0].2, vec![0x01]);
    server.shut_down().await;
}

#[tokio::test]
async fn pausing_stops_the_clock_but_not_building() {
    let server = RunningServer::start(|_| {}).await;
    let clients = vec![server.client("ann").await, server.client("bob").await];
    let (players, _) = start(clients, FAST).await;
    let players = play_all(players, |p| p.executed >= 5).await;
    assert_eq!(
        players[1].client().set_speed(Speed::PAUSED).await,
        Err(ClientError::Refused(RequestError::NotOwner))
    );
    players[0].client().set_speed(Speed::PAUSED).await.unwrap();
    // Let the pause reach everyone and every in-flight step drain.
    let players = play_all(players, |p| {
        p.follower.as_ref().is_some_and(|f| f.speed().is_paused())
    })
    .await;
    let mut players = players;
    for player in &mut players {
        player.play_for(Duration::from_millis(200)).await;
    }
    let paused_at = players[0].follower.as_ref().unwrap().sealed_through();

    // Build while paused: the command applies without any step running.
    players[0]
        .client()
        .send_intent(1, payload(&[9]))
        .await
        .unwrap();
    let players = play_all(players, |p| commands(p).len() == 1).await;
    for player in &players {
        let follower = player.follower.as_ref().unwrap();
        assert_eq!(follower.sealed_through(), paused_at, "still paused");
        assert_eq!(player.executed, paused_at, "no step ran");
    }
    server.shut_down().await;
}

#[tokio::test]
async fn a_reconnecting_player_resumes_exactly() {
    let server = RunningServer::start(|_| {}).await;
    let clients = vec![server.client("ann").await, server.client("bob").await];
    let (players, invite) = start(clients, FAST).await;
    let mut players = play_all(players, |p| p.executed >= 10).await;
    let bob = players.pop().unwrap();
    let mut ann = players.pop().unwrap();

    // Bob's connection drops; his game keeps its state.
    let identity = Arc::clone(&bob.test.identity);
    let Player {
        follower,
        start: stream_start,
        applied,
        executed,
        ..
    } = bob;
    let resume = Some(follower.as_ref().unwrap().resume_point());

    // Ann builds while Bob is away.
    for seq in 0..5u8 {
        ann.client()
            .send_intent(u64::from(seq), payload(&[seq]))
            .await
            .unwrap();
    }
    ann.play_until(|p| commands(p).len() == 5).await;

    // Bob returns with the same identity and resumes after his last turn.
    let mut bob = Player::new(server.client_as(identity, "bob").await);
    bob.follower = follower;
    bob.start = stream_start;
    bob.applied = applied;
    bob.executed = executed;
    bob.client()
        .join_room(JoinRoom {
            invite,
            password: None,
            resume,
            content: None,
        })
        .await
        .unwrap();
    bob.play_until(|p| commands(p).len() == 5).await;

    let shared = bob.applied.len().min(ann.applied.len());
    assert_eq!(bob.applied[..shared], ann.applied[..shared]);
    ann.play_until(|p| {
        p.room
            .as_ref()
            .is_some_and(|room| room.members.iter().all(|member| member.connected))
    })
    .await;
    server.shut_down().await;
}

#[tokio::test]
async fn a_kicked_player_leaves_the_running_game() {
    let server = RunningServer::start(|_| {}).await;
    let clients = vec![server.client("ann").await, server.client("bob").await];
    let (players, _) = start(clients, FAST).await;
    let mut players = play_all(players, |p| p.executed >= 5).await;
    let mut bob = players.pop().unwrap();
    let mut ann = players.pop().unwrap();
    let bob_id = bob.client().player();

    ann.client().kick(bob_id).await.unwrap();
    bob.play_until(|p| p.kicked).await;
    // Every replica learns of it through the log, at one step.
    ann.play_until(|p| {
        p.applied.iter().any(
            |event| matches!(&event.body, EventBody::PlayerLeft { player, .. } if *player == bob_id),
        )
    })
    .await;
    // The room runs on without him.
    let executed = ann.executed;
    ann.play_until(|p| p.executed >= executed + 20).await;
    server.shut_down().await;
}

#[tokio::test]
async fn resuming_from_the_future_is_refused() {
    let server = RunningServer::start(|_| {}).await;
    let clients = vec![server.client("ann").await, server.client("bob").await];
    let (players, invite) = start(clients, FAST).await;
    let players = play_all(players, |p| p.executed >= 1).await;
    let identity = Arc::clone(&players[1].test.identity);
    let history = players[1].follower.as_ref().unwrap().resume_point().history;
    drop(players);
    let bob = server.client_as(identity, "bob").await;
    let error = bob
        .client
        .join_room(JoinRoom {
            invite,
            password: None,
            resume: Some(Resume {
                after_turn: 1_000_000,
                history,
            }),
            content: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error, ClientError::Refused(RequestError::ResumeUnavailable));
    server.shut_down().await;
}

#[tokio::test]
async fn a_newcomer_cannot_join_a_running_game_yet() {
    let server = RunningServer::start(|_| {}).await;
    let (_players, invite) = start(vec![server.client("ann").await], FAST).await;
    let cat = server.client("cat").await;
    assert_eq!(
        cat.client.join_room(join(&invite)).await.unwrap_err(),
        ClientError::Refused(RequestError::GameRunning)
    );
    server.shut_down().await;
}

#[tokio::test]
async fn leaving_mid_game_is_an_event() {
    let server = RunningServer::start(|_| {}).await;
    let clients = vec![server.client("ann").await, server.client("bob").await];
    let bob_id = clients[1].client.player();
    let (players, _) = start(clients, FAST).await;
    let mut players = play_all(players, |p| p.executed >= 3).await;
    players[1].client().leave_room().await.unwrap();
    let mut ann = players.remove(0);
    ann.play_until(|p| {
        p.applied.iter().any(
            |event| matches!(&event.body, EventBody::PlayerLeft { player, .. } if *player == bob_id),
        )
    })
    .await;
    server.shut_down().await;
}

#[tokio::test]
async fn checkpoints_single_out_the_diverged_player() {
    let server = RunningServer::start(|_| {}).await;
    let clients = vec![
        server.client("ann").await,
        server.client("bob").await,
        server.client("cat").await,
    ];
    let (mut players, _) = start(clients, FAST).await;
    for (index, player) in players.iter_mut().enumerate() {
        // Cat's world computes a different digest in lane 3.
        let value = if index == 2 { 2 } else { 1 };
        player.lanes = Some(Box::new(move |_step| {
            vec![
                LaneDigest {
                    lane: 0,
                    digest: FixedBytes([7; 32]),
                },
                LaneDigest {
                    lane: 3,
                    digest: FixedBytes([value; 32]),
                },
            ]
        }));
    }
    let mut players = play_all(players, |p| p.executed >= 25).await;
    let mut cat = players.pop().unwrap();
    cat.play_until(|p| !p.diverged.is_empty()).await;
    assert_eq!(cat.diverged[0], (10, vec![3]));
    for player in &mut players {
        player.play_for(Duration::from_millis(100)).await;
        assert!(player.diverged.is_empty(), "the majority agrees");
    }
    server.shut_down().await;
}

#[tokio::test]
async fn progress_beyond_the_frontier_is_a_violation() {
    let server = RunningServer::start(|_| {}).await;
    let (mut players, _) = start(vec![server.client("ann").await], FAST).await;
    let mut ann = players.pop().unwrap();
    ann.play_until(|p| p.executed >= 1).await;
    ann.client().report_progress(1_000_000).await.unwrap();
    let reason = ann.test.closed().await;
    assert_eq!(
        application_close_code(&reason),
        Some(close::PROTOCOL_VIOLATION)
    );
    server.shut_down().await;
}
