//! A memory bound from the security review: a hostile server and a real
//! `tpf3mp-agent` client in one process, with a counting allocator.

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

const FRAMES: u64 = 200;
/// Small events that fill most of a 1 MiB turn frame.
const EVENTS_PER_TURN: u64 = 26_000;
const MIB: f64 = 1024.0 * 1024.0;

/// Review finding M7: the agent's event channel bounded the number of
/// queued events, not their size, and a turn filling a 1 MiB frame decodes
/// to about twice that. A server could make a client hold gigabytes while
/// the game was not draining events, such as during a loading screen. Turns
/// now count against a byte budget until the application receives them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hostile_server_cannot_fill_the_client_with_turns() {
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
            rules: Vec::new(),
        });
        write_message(&mut send, &welcome, CONTROL_MAX_FRAME)
            .await
            .unwrap();
        wait_for_go.await.unwrap();
        let mut turns = connection.open_uni().await.unwrap();
        write_preamble(&mut turns, PROTOCOL_VERSION).await.unwrap();
        let start = TurnMessage::Start(TurnStart {
            room: RoomId(FixedBytes([0; 16])),
            rules: tpf3mp_proto::Text::new("native").unwrap(),
            next_turn: 1,
            next_event: 1,
            steps_per_second: 5,
            checkpoint_interval: 10,
            history: 0,
            sealed_through: 0,
            world: None,
        });
        write_message(&mut turns, &start, TURN_MAX_FRAME)
            .await
            .unwrap();
        let mut seq = 1;
        for number in 1..=FRAMES {
            let events = (0..EVENTS_PER_TURN)
                .map(|_| {
                    seq += 1;
                    Event {
                        seq: seq - 1,
                        step: 1,
                        body: EventBody::PlayerLeft {
                            player: PlayerId(FixedBytes([7; 32])),
                            kicked: false,
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
            // Blocks for good once the client stops reading.
            if turns.write_all(&frame).await.is_err() {
                break;
            }
        }
        drop((endpoint, connection, send, turns));
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
    // The game is busy (a loading screen) and does not read events; the
    // server pushes as fast as the client takes them.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let held = LIVE.load(Relaxed).saturating_sub(before) as f64;
    let queued = events.len();
    server.abort();
    println!(
        "{queued} of {FRAMES} oversized turns queued while the game was busy: {:.1} MiB held",
        held / MIB
    );
    drop(events);
    drop(client);
    assert!(queued < 100, "the client queued {queued} turns");
    // The 64 MiB budget, weighed conservatively, plus flow control windows.
    assert!(held < 192.0 * MIB, "the client held {:.1} MiB", held / MIB);
}
