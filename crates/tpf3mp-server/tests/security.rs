//! Proof-of-concept tests from the security review.
//!
//! Every test here asserts the behaviour the server *should* have and fails
//! today because of the finding named in its doc comment. They are ignored so
//! the regular suite stays green; run them with
//!
//! ```sh
//! cargo test -p tpf3mp-server --test security -- --ignored
//! ```
//!
//! and un-ignore each one together with its fix.

#![allow(clippy::unwrap_used)]

mod common;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use common::{FAST, Player, RunningServer, TestClient, content, join, new_identity, room, seat};
use quinn::{RecvStream, SendStream};
use tpf3mp_agent::{ClientError, ConnectError, connect};
use tpf3mp_net::{
    Identity, client_config, read_message, read_preamble, write_message, write_preamble,
};
use tpf3mp_proto::{
    CONTROL_MAX_FRAME, ClientMessage, EventBody, FRAME_HEADER_LEN, FixedBytes, Hello, JoinRoom,
    LaneDigest, PROTOCOL_VERSION, Payload, Platform, PlayerId, RejectReason, Request, RequestError,
    Response, RoomSettings, ServerMessage, Signature, Text, TurnMessage, decode_frame,
};
use tpf3mp_server::ServerConfig;

fn data_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tpf3mp-sec-{name}-{}", std::process::id()));
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

fn payload(bytes: &[u8]) -> Payload {
    Payload::new(bytes.to_vec()).unwrap()
}

fn commands(player: &Player) -> Vec<Vec<u8>> {
    player
        .applied
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::Command { payload, .. } => Some(payload.as_bytes().to_vec()),
            _ => None,
        })
        .collect()
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

/// Completes the handshake as `identity` on a bare QUIC connection.
async fn raw_session(
    server: &RunningServer,
    identity: &Identity,
) -> (quinn::Endpoint, quinn::Connection, SendStream, RecvStream) {
    let (endpoint, connection) = server.raw_connection().await;
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    write_preamble(&mut send, PROTOCOL_VERSION).await.unwrap();
    assert_eq!(read_preamble(&mut recv).await.unwrap(), PROTOCOL_VERSION);
    let hello = ClientMessage::Hello(Hello {
        client_version: Text::new("poc").unwrap(),
        platform: Platform::current(),
        name: Text::new("mallory").unwrap(),
        identity: identity.player(),
        proof: identity.prove(&connection).unwrap(),
    });
    write_message(&mut send, &hello, CONTROL_MAX_FRAME)
        .await
        .unwrap();
    match read_message::<ServerMessage>(&mut recv, CONTROL_MAX_FRAME)
        .await
        .unwrap()
    {
        ServerMessage::Welcome(_) => {}
        other => panic!("expected Welcome, got {other:?}"),
    }
    (endpoint, connection, send, recv)
}

async fn raw_request(
    send: &mut SendStream,
    recv: &mut RecvStream,
    id: u32,
    request: Request,
) -> Result<Response, RequestError> {
    let message = ClientMessage::Request { id, request };
    write_message(send, &message, CONTROL_MAX_FRAME)
        .await
        .unwrap();
    loop {
        let message = read_message::<ServerMessage>(recv, CONTROL_MAX_FRAME)
            .await
            .unwrap();
        if let ServerMessage::Response { id: got, result } = message
            && got == id
        {
            return result;
        }
    }
}

// ---------------------------------------------------------------------------
// Rooms that are never reclaimed
// ---------------------------------------------------------------------------

/// FINDING: a running room whose players all *disconnected* (rather than
/// left) holds its seats forever and is restored after every restart. There
/// is no expiry, so anyone can fill `max_rooms` with one-player games: a
/// fresh key, CreateRoom, DeclareContent, SetReady, StartGame, disconnect.
/// With `--data-dir` (the image default) the lock-out survives restarts.
///
/// Fixed: a running game nobody is connected to closes after the abandon
/// timeout (shortened here), and its log goes with it.
#[tokio::test]
async fn abandoned_games_do_not_lock_everyone_out_of_the_server() {
    let dir = data_dir("zombies");
    let secret = [3; 32];
    let config = |dir: PathBuf| {
        move |config: &mut ServerConfig| {
            config.max_rooms = 3;
            config.data_dir = Some(dir);
            config.secret = secret;
            config.abandoned_timeout = Duration::from_millis(200);
        }
    };
    let server = RunningServer::start(config(dir.clone())).await;
    for index in 0..3 {
        // A throwaway identity per room: nothing ties the rooms together.
        let mallory = server.client(&format!("mallory{index}")).await;
        mallory
            .client
            .create_room(room("zombie", FAST))
            .await
            .unwrap();
        mallory.client.declare_content(content(1)).await.unwrap();
        mallory.client.set_ready(true).await.unwrap();
        mallory.client.start_game().await.unwrap();
        mallory.client.close().await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let ann = server.client("ann").await;
    let before_restart = ann.client.create_room(room("honest", FAST)).await;
    ann.client.close().await;
    server.shut_down().await;

    let server = RunningServer::start(config(dir.clone())).await;
    let restored = server.stats.rooms();
    let bob = server.client("bob").await;
    let after_restart = bob.client.create_room(room("honest", FAST)).await;
    bob.client.close().await;
    server.shut_down().await;
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        before_restart.is_ok() && after_restart.is_ok(),
        "abandoned one-player games locked out an honest player: {before_restart:?}; \
         after a restart the server restored {restored} abandoned games and answered: \
         {after_restart:?}"
    );
}

// ---------------------------------------------------------------------------
// Lobby seats that are never freed
// ---------------------------------------------------------------------------

/// FINDING: `Room::drop_link` clears a member's link without removing the
/// member, and `Room::disconnected` only matches a member whose link is
/// still set. In the lobby, a member whose link was dropped (control queue
/// closed or full) therefore keeps the seat forever: its later
/// `Disconnected` is ignored. Here Mallory stops the server's half of her
/// control stream (STOP_SENDING), two broadcasts make the room drop her
/// link, and she disconnects. Her seat, never ready, blocks the start for
/// good; the same happens to an honest member the room drops as a slow
/// consumer, and to an owner (ownership then never passes on).
#[tokio::test]
async fn a_lobby_seat_is_freed_even_after_its_link_was_dropped() {
    let server = RunningServer::start(|_| {}).await;
    let mut ann = server.client("ann").await;
    let (invite, _) = ann.client.create_room(room("table", FAST)).await.unwrap();
    ann.client.declare_content(content(1)).await.unwrap();

    let mallory = new_identity();
    let (_endpoint, connection, mut send, mut recv) = raw_session(&server, &mallory).await;
    raw_request(&mut send, &mut recv, 1, Request::JoinRoom(join(&invite)))
        .await
        .unwrap();
    ann.room_where(|room| room.members.len() == 2).await;

    recv.stop(0u32.into()).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    // The first broadcast kills the server's writer for Mallory; the second
    // finds her queue closed and drops her link.
    ann.client.set_ready(true).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    ann.client.set_ready(true).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    connection.close(0u32.into(), b"gone");

    let freed = tokio::time::timeout(
        Duration::from_secs(3),
        ann.room_where(|room| room.members.len() == 1),
    )
    .await
    .is_ok();
    let start = ann.client.start_game().await;
    server.shut_down().await;
    assert!(
        freed && start.is_ok(),
        "Mallory disconnected but keeps her lobby seat (seat freed: {freed}); start: {start:?}"
    );
}

/// FINDING: requests are not rate-limited, and every accepted `SetReady` or
/// `DeclareContent` broadcasts the full `RoomView` to every member, even
/// when nothing changed. A member's burst of 9-byte requests becomes one
/// full view per member per request (in a 64-seat lobby about 6.5 KB x 64,
/// some 400 KB of server output per request). Any member that cannot absorb
/// that, here Bob whose game is busy for a moment, is disconnected as a slow
/// consumer, and in the lobby his seat then stays behind (see
/// `a_lobby_seat_is_freed_even_after_its_link_was_dropped`).
#[tokio::test]
async fn one_member_cannot_flood_the_others_off_the_lobby() {
    const BURST: u32 = 12_000;
    let server = RunningServer::start(|_| {}).await;
    let ann = server.client("ann").await;
    let (invite, _) = ann.client.create_room(room("table", FAST)).await.unwrap();
    let mut bob = server.client("bob").await;
    bob.client.join_room(join(&invite)).await.unwrap();
    // Ann reads everything; Bob's game does not read events for a moment.
    let TestClient {
        client: ann_client,
        events: mut ann_events,
        ..
    } = ann;
    let seen_by_ann = Arc::new(std::sync::Mutex::new(0));
    let ann_drain = tokio::spawn({
        let seen_by_ann = Arc::clone(&seen_by_ann);
        async move {
            while let Some(event) = ann_events.recv().await {
                if let tpf3mp_agent::ClientEvent::RoomUpdate(room) = event {
                    *seen_by_ann.lock().unwrap() = room.members.len();
                }
            }
        }
    });

    let mallory = new_identity();
    let (_endpoint, _connection, mut send, mut recv) = raw_session(&server, &mallory).await;
    raw_request(&mut send, &mut recv, 1, Request::JoinRoom(join(&invite)))
        .await
        .unwrap();
    // Mallory reads her own traffic, so she is never the slow one.
    let reader = tokio::spawn(async move {
        let mut answered = 0;
        while answered < BURST {
            match read_message::<ServerMessage>(&mut recv, CONTROL_MAX_FRAME).await {
                Ok(ServerMessage::Response { .. }) => answered += 1,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        answered
    });
    let mut burst = Vec::new();
    for id in 0..BURST {
        let request = ClientMessage::Request {
            id: id + 2,
            request: Request::SetReady(true),
        };
        burst.extend(tpf3mp_proto::encode_frame(&request, CONTROL_MAX_FRAME).unwrap());
    }
    let sent = burst.len();
    send.write_all(&burst).await.unwrap();
    let answered = reader.await.unwrap();

    let bob_closed = tokio::time::timeout(Duration::from_secs(3), bob.closed())
        .await
        .ok()
        .map(|reason| common::application_close_code(&reason));
    // Had Bob's seat been freed, the room would have told Ann (two members).
    tokio::time::sleep(Duration::from_secs(1)).await;
    let members_left = *seen_by_ann.lock().unwrap();
    ann_drain.abort();
    drop(ann_client);
    server.shut_down().await;
    assert!(
        bob_closed.is_none(),
        "Mallory's {answered} no-op ready requests ({sent} bytes) made the server send \
         {answered} room views to every member; Bob was disconnected with close code \
         {bob_closed:?} (SLOW_CONSUMER is 6), and Ann's lobby still lists {members_left} \
         members"
    );
}

/// FINDING: `JoinRoom` attempts are not limited per connection, per room or
/// per invite, and a connection may pipeline them. Whoever holds a leaked
/// invite can guess the room password online as fast as the room answers;
/// here every 4-digit PIN over one connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_room_password_cannot_be_guessed_at_line_rate() {
    let server = RunningServer::start(|_| {}).await;
    let ann = server.client("ann").await;
    let mut create = room("secret", FAST);
    create.password = Some(Text::new("7391").unwrap());
    let (invite, _) = ann.client.create_room(create).await.unwrap();

    let mallory = new_identity();
    let (_endpoint, _connection, mut send, mut recv) = raw_session(&server, &mallory).await;
    let mut burst = Vec::new();
    for pin in 0..10_000u32 {
        let request = ClientMessage::Request {
            id: pin,
            request: Request::JoinRoom(JoinRoom {
                invite: invite.clone(),
                password: Some(Text::new(format!("{pin:04}")).unwrap()),
                resume_after_turn: None,
            }),
        };
        burst.extend(tpf3mp_proto::encode_frame(&request, CONTROL_MAX_FRAME).unwrap());
    }
    let started = Instant::now();
    send.write_all(&burst).await.unwrap();
    let mut guesses = 0;
    let mut found = None;
    while found.is_none() && guesses < 10_000 {
        if let ServerMessage::Response { id, result } =
            read_message::<ServerMessage>(&mut recv, CONTROL_MAX_FRAME)
                .await
                .unwrap()
        {
            guesses += 1;
            if result.is_ok() {
                found = Some(format!("{id:04}"));
            }
        }
    }
    let elapsed = started.elapsed();
    server.shut_down().await;
    assert!(
        found.is_none(),
        "{guesses} password guesses answered in {elapsed:?} ({:.0} per second on one \
         connection); the password is {found:?}",
        f64::from(guesses) / elapsed.as_secs_f64()
    );
}

// ---------------------------------------------------------------------------
// Checkpoint verdicts switched off by one member
// ---------------------------------------------------------------------------

/// Fast pacing so a test reaches hundreds of checkpoints in seconds.
const CHECKS: RoomSettings = RoomSettings {
    steps_per_second: 240,
    input_delay_ms: 40,
    checkpoint_interval: 10,
};

fn lanes(value: u8) -> Vec<LaneDigest> {
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
}

/// Plays a four-player game in which Cat's world diverges from step 1000,
/// and returns the divergences Cat was told about. With `attack`, Mallory
/// (who otherwise plays honestly) keeps re-reporting checkpoints for old,
/// already pruned steps from step 800 on.
async fn divergences_reported_to_cat(attack: bool) -> Vec<(u64, Vec<u16>)> {
    let server = RunningServer::start(|_| {}).await;
    let mut clients = vec![
        server.client("ann").await,
        server.client("bob").await,
        server.client("cat").await,
        server.client("mallory").await,
    ];
    let mut seats: Vec<&mut TestClient> = clients.iter_mut().collect();
    seat(&mut seats, CHECKS).await;
    clients[0].client.start_game().await.unwrap();
    let mut players: Vec<Player> = clients.into_iter().map(Player::new).collect();
    for (index, player) in players.iter_mut().enumerate() {
        player.lanes = Some(Box::new(move |step| {
            lanes(if index == 2 && step >= 1000 { 2 } else { 1 })
        }));
    }
    let mut mallory = players.pop().unwrap();
    let honest: Vec<_> = players
        .into_iter()
        .map(|mut player| {
            tokio::spawn(async move {
                player.play_until(|p| p.executed >= 1500).await;
                player
            })
        })
        .collect();
    let deadline = Instant::now() + common::WAIT;
    while mallory.executed < 1500 && Instant::now() < deadline {
        mallory.play_for(Duration::from_millis(20)).await;
        if attack && mallory.executed >= 800 {
            // Steps 10..=720 were decided and pruned long ago: each report
            // opens a fresh round that no other member will ever complete,
            // until MAX_OPEN_ROUNDS (64) is reached.
            for step in (1..=72).map(|k| k * 10) {
                let _ = mallory.client().report_checkpoint(step, lanes(1)).await;
            }
        }
    }
    let mut honest_players = Vec::new();
    for task in honest {
        honest_players.push(task.await.unwrap());
    }
    let mut cat = honest_players.remove(2);
    cat.play_for(Duration::from_millis(500)).await;
    server.shut_down().await;
    cat.diverged
}

/// Control for the test below: without the attack, Cat's divergence at
/// step 1000 is reported.
#[tokio::test]
#[ignore = "security PoC control (passes); slow"]
async fn without_interference_a_divergence_is_reported() {
    let diverged = divergences_reported_to_cat(false).await;
    assert!(
        diverged
            .iter()
            .any(|(step, lanes)| *step >= 1000 && lanes == &[3]),
        "{diverged:?}"
    );
}

/// FINDING: `Room::checkpoint` opens a round for any due step up to the
/// frontier, including steps whose rounds were decided and pruned long ago,
/// and refuses new rounds once 64 are open. One member can therefore keep
/// 64 bogus rounds open (each waits 30 s for reports that never come, and
/// the member refills them), and every honest checkpoint is silently
/// dropped: divergence detection is off for the whole room.
#[tokio::test]
#[ignore = "security PoC: fails until checkpoint rounds cannot be exhausted by one member"]
async fn one_member_cannot_switch_off_divergence_detection() {
    let diverged = divergences_reported_to_cat(true).await;
    assert!(
        diverged.iter().any(|(step, _)| *step >= 1000),
        "Cat's world diverged from step 1000 but no verdict reached her: {diverged:?}"
    );
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

fn only_log(dir: &Path) -> PathBuf {
    let logs: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "log"))
        .collect();
    assert_eq!(logs.len(), 1, "{logs:?}");
    logs.into_iter().next().unwrap()
}

/// `(offset, payload)` of every record in a room log.
fn records(bytes: &[u8]) -> Vec<(usize, &[u8])> {
    let mut found = Vec::new();
    let mut offset = 0;
    while offset + 8 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let Some(payload) = bytes.get(offset + 8..offset + 8 + len) else {
            break;
        };
        found.push((offset, payload));
        offset += 8 + len;
    }
    found
}

fn dir_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| std::fs::metadata(entry.unwrap().path()).unwrap().len())
        .sum()
}

/// Runs a one-player game with a log in `dir` until it has a few dozen
/// turns, then stops the server.
async fn logged_game(dir: &Path, secret: [u8; 32]) {
    let server = RunningServer::start(persistent(dir, secret)).await;
    let mut clients = vec![server.client("ann").await];
    let mut seats: Vec<&mut TestClient> = clients.iter_mut().collect();
    seat(&mut seats, FAST).await;
    clients[0].client.start_game().await.unwrap();
    let mut player = Player::new(clients.pop().unwrap());
    player.play_until(|p| p.executed >= 40).await;
    drop(player);
    server.shut_down().await;
}

/// FINDING: `RoomLog::open` cuts the file at the first record whose length
/// or CRC does not hold *before* anyone decides whether the log is usable.
/// A damaged start record therefore truncates the whole log to zero bytes,
/// and only then is the (now empty) file renamed to `*.broken`. OPERATIONS.md
/// promises the opposite: "renamed to `*.broken` and kept for diagnosis.
/// Nothing is deleted."
#[tokio::test]
async fn a_damaged_log_is_kept_for_diagnosis() {
    let dir = data_dir("broken-start");
    let secret = [4; 32];
    logged_game(&dir, secret).await;
    let log = only_log(&dir);
    let mut bytes = std::fs::read(&log).unwrap();
    let original = bytes.len();
    bytes[8] ^= 0x01; // one flipped bit in the start record
    std::fs::write(&log, &bytes).unwrap();

    let server = RunningServer::start(persistent(&dir, secret)).await;
    assert_eq!(server.stats.rooms(), 0);
    server.shut_down().await;
    let broken = log.with_extension("broken");
    let kept = std::fs::metadata(&broken).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        kept, original as u64,
        "the damaged log ({original} bytes) was kept as {kept} bytes"
    );
}

/// FINDING: the same cut applies to a record in the middle of a log. One
/// damaged turn silently deletes every later turn from disk, and the room is
/// restored at the damage instead of being set aside.
#[tokio::test]
async fn one_damaged_turn_does_not_erase_the_rest_of_the_log() {
    let dir = data_dir("broken-middle");
    let secret = [6; 32];
    logged_game(&dir, secret).await;
    let log = only_log(&dir);
    let mut bytes = std::fs::read(&log).unwrap();
    let original = bytes.len() as u64;
    let offsets: Vec<usize> = records(&bytes).iter().map(|(offset, _)| *offset).collect();
    let damaged = offsets.len() / 2;
    let turns_before = offsets.len() - 1;
    bytes[offsets[damaged] + 8] ^= 0x01;
    std::fs::write(&log, &bytes).unwrap();

    let server = RunningServer::start(persistent(&dir, secret)).await;
    let restored = server.stats.rooms();
    server.shut_down().await;
    let on_disk = dir_bytes(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        on_disk >= original,
        "one flipped bit in turn record {damaged} of {turns_before}: {restored} room restored \
         and {} of {original} log bytes deleted from disk",
        original - on_disk
    );
}

/// FINDING: after a lossy recovery (power loss; OPERATIONS.md says the last
/// turns can be lost) the restored room seals *new* turns under the numbers
/// of the lost ones. A client that applied the lost turns is refused
/// (`ResumeUnavailable`) only until the new timeline passes its turn; then
/// its resume is accepted, `TurnFollower::restart` sees matching turn and
/// event numbers, and it continues on a different history without any
/// error. Here Ann applied command X at event 3; after recovery the room
/// ordered Bob's command Y as event 3; Ann resumes anyway.
#[tokio::test]
#[ignore = "security PoC: fails until lost turn numbers are never reused for a resume"]
async fn turns_lost_in_a_crash_are_not_replaced_under_a_client_that_saw_them() {
    let dir = data_dir("fork");
    let secret = [5; 32];
    let first = RunningServer::start(persistent(&dir, secret)).await;
    let mut clients = vec![first.client("ann").await, first.client("bob").await];
    let mut seats: Vec<&mut TestClient> = clients.iter_mut().collect();
    let invite = seat(&mut seats, FAST).await;
    clients[0].client.start_game().await.unwrap();
    let players: Vec<Player> = clients.into_iter().map(Player::new).collect();
    let players = play_all(players, |p| p.executed >= 10).await;
    players[0]
        .client()
        .send_intent(1, payload(b"X"))
        .await
        .unwrap();
    let mut players = play_all(players, |p| commands(p).len() == 1 && p.executed >= 40).await;
    first.shut_down().await;

    // Power loss: the turns from the one carrying X onwards never reached
    // the disk.
    let log = only_log(&dir);
    let bytes = std::fs::read(&log).unwrap();
    let cut = records(&bytes)
        .into_iter()
        .skip(1)
        .find(|(_, frame)| {
            matches!(
                decode_frame::<TurnMessage>(&frame[FRAME_HEADER_LEN..]),
                Ok(TurnMessage::Turn(turn))
                    if turn.events.iter().any(|e| matches!(e.body, EventBody::Command { .. }))
            )
        })
        .map(|(offset, _)| offset)
        .unwrap();
    std::fs::write(&log, &bytes[..cut]).unwrap();

    let second = RunningServer::start(persistent(&dir, secret)).await;
    let bob_old = players.pop().unwrap();
    let ann_old = players.pop().unwrap();
    let ann_last = ann_old.follower.as_ref().unwrap().last_turn().unwrap();

    // Bob is told his turns are gone, reloads and follows from turn 1...
    let mut bob = Player::new(
        second
            .client_as(Arc::clone(&bob_old.test.identity), "bob")
            .await,
    );
    let refused = bob
        .client()
        .join_room(JoinRoom {
            invite: invite.clone(),
            password: None,
            resume_after_turn: bob_old.follower.as_ref().unwrap().last_turn(),
        })
        .await;
    assert_eq!(
        refused.unwrap_err(),
        ClientError::Refused(RequestError::ResumeUnavailable)
    );
    bob.client().join_room(join(&invite)).await.unwrap();
    // ...and builds Y where X used to be.
    bob.client().send_intent(1, payload(b"Y")).await.unwrap();
    bob.play_until(|p| {
        commands(p).len() == 1
            && p.follower
                .as_ref()
                .and_then(|f| f.last_turn())
                .is_some_and(|turn| turn > ann_last + 5)
    })
    .await;

    // Ann comes back and resumes after the last turn she applied.
    let mut ann = Player::new(
        second
            .client_as(Arc::clone(&ann_old.test.identity), "ann")
            .await,
    );
    ann.follower = ann_old.follower;
    ann.start = ann_old.start;
    ann.applied = ann_old.applied;
    ann.executed = ann_old.executed;
    let resumed = ann
        .client()
        .join_room(JoinRoom {
            invite,
            password: None,
            resume_after_turn: Some(ann_last),
        })
        .await;
    if resumed.is_ok() {
        // The follower accepts the new history without complaint.
        ann.play_for(Duration::from_millis(300)).await;
    }
    second.shut_down().await;
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        resumed.is_err(),
        "Ann resumed after turn {ann_last} on a history she never saw: she applied {:?}, \
         the room applied {:?}",
        commands(&ann),
        commands(&bob),
    );
}

// ---------------------------------------------------------------------------
// Sessions and identity
// ---------------------------------------------------------------------------

/// FINDING: sessions are held until the connection closes, and the server's
/// own QUIC keep-alive (every 5 s) keeps an idle connection open forever:
/// a peer only has to acknowledge the pings. There is no per-address cap and
/// no eviction of sessions that do nothing, so `max_sessions` (4096) idle
/// connections with throwaway keys lock every player out. This client even
/// disables its own keep-alive and still holds the slot past the 30 s idle
/// timeout.
#[tokio::test]
async fn an_idle_session_does_not_hold_a_slot_forever() {
    let server = RunningServer::start(|config| config.max_sessions = 1).await;
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    let mut config = client_config(server.trust.clone()).unwrap();
    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(None);
    transport.max_idle_timeout(Some(Duration::from_secs(30).try_into().unwrap()));
    config.transport_config(Arc::new(transport));
    endpoint.set_default_client_config(config);
    let connection = endpoint
        .connect(server.address, "localhost")
        .unwrap()
        .await
        .unwrap();
    let mallory = new_identity();
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    write_preamble(&mut send, PROTOCOL_VERSION).await.unwrap();
    read_preamble(&mut recv).await.unwrap();
    let hello = ClientMessage::Hello(Hello {
        client_version: Text::new("poc").unwrap(),
        platform: Platform::current(),
        name: Text::new("idle").unwrap(),
        identity: mallory.player(),
        proof: mallory.prove(&connection).unwrap(),
    });
    write_message(&mut send, &hello, CONTROL_MAX_FRAME)
        .await
        .unwrap();
    let _welcome = read_message::<ServerMessage>(&mut recv, CONTROL_MAX_FRAME)
        .await
        .unwrap();

    // Do nothing at all for longer than the idle timeout.
    tokio::time::sleep(Duration::from_secs(40)).await;
    let still_open = connection.close_reason().is_none();
    let honest = connect(server.options(new_identity(), "ann")).await;
    let refused = matches!(
        honest,
        Err(ConnectError::Rejected(RejectReason::ServerFull))
    );
    drop(honest);
    server.shut_down().await;
    assert!(
        !(still_open && refused),
        "an idle session held the server's only slot for 40 s (open: {still_open}), \
         and an honest player was refused"
    );
}

/// FINDING (low): ring's Ed25519 verification accepts small-order public
/// keys. For the neutral point `01 00..00`, the signature `R = 01 00..00,
/// s = 0` verifies for *every* message, so anyone can prove this identity on
/// any connection. Honest keys are unaffected, but such a "player" is shared
/// by everyone (for example, anyone can take over its room seat or owner
/// rights).
#[tokio::test]
#[ignore = "security PoC: fails until small-order identity keys are refused"]
async fn a_small_order_identity_key_is_refused() {
    let server = RunningServer::start(|_| {}).await;
    let (_endpoint, connection) = server.raw_connection().await;
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    write_preamble(&mut send, PROTOCOL_VERSION).await.unwrap();
    read_preamble(&mut recv).await.unwrap();
    let mut neutral = [0; 32];
    neutral[0] = 1;
    let mut forged = [0; 64];
    forged[0] = 1;
    let hello = ClientMessage::Hello(Hello {
        client_version: Text::new("poc").unwrap(),
        platform: Platform::current(),
        name: Text::new("anyone").unwrap(),
        identity: PlayerId(FixedBytes(neutral)),
        proof: Signature(FixedBytes(forged)),
    });
    write_message(&mut send, &hello, CONTROL_MAX_FRAME)
        .await
        .unwrap();
    let answer = read_message::<ServerMessage>(&mut recv, CONTROL_MAX_FRAME)
        .await
        .unwrap();
    server.shut_down().await;
    assert!(
        matches!(&answer, ServerMessage::Reject(reject) if reject.reason == RejectReason::BadProof),
        "a proof forged without any private key was accepted: {answer:?}"
    );
}
