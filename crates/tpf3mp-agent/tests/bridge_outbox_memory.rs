//! A memory bound on the bridge, with a hostile server and a real client in
//! one process and a counting allocator (the scaffold of
//! `poc_client_memory.rs`).
//!
//! Events for the step after the frontier need no playout time, so they
//! leave the `TurnFollower` as soon as they arrive. While the hook is not
//! reading (it has not attached yet, or the game is loading a world), they
//! wait for it in the bridge's outbox, which takes nothing more from the
//! room past its bound: the rest waits in the client, whose own budget then
//! holds back the server's stream. Found by the client-side security review.

#![allow(unsafe_code, clippy::unwrap_used)]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

use tpf3mp_agent::{
    ConnectOptions,
    bridge::{Bridge, BridgeFault, BridgeOptions, HookLink},
    connect,
};
use tpf3mp_net::{
    Identity, ServerIdentity, ServerTrust, read_message, read_preamble, server_config,
    write_message, write_preamble,
};
use tpf3mp_proto::{
    CONTROL_MAX_FRAME, ClientMessage, Event, EventBody, FixedBytes, MAX_PAYLOAD, PROTOCOL_VERSION,
    Payload, PlayerId, RoomId, ServerMessage, SessionId, Speed, TURN_MAX_FRAME, Text, Turn,
    TurnMessage, TurnStart, Welcome,
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

/// A game whose hook takes nothing yet: not attached, or loading a world.
struct IdleHook;

impl HookLink for IdleHook {
    fn send(&mut self, _message: &[u8]) -> Result<bool, BridgeFault> {
        Ok(false)
    }

    fn recv(&mut self, _buf: &mut Vec<u8>) -> Result<bool, BridgeFault> {
        Ok(false)
    }

    fn heartbeat(&mut self) {}

    fn peer_heartbeat(&self) -> u64 {
        0
    }
}

const TURNS: u64 = 400;
/// Commands with the largest payload that fit a 1 MiB turn frame.
const EVENTS_PER_TURN: u64 = 20;
const MIB: f64 = 1024.0 * 1024.0;
/// Enough to exceed every bound the client has, without using up the machine.
const STOP_AT: f64 = 320.0 * MIB;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hostile_server_cannot_fill_the_bridge_outbox() {
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
        // A new game (no world to fetch), with the frontier held at 0.
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
        for number in 1..=TURNS {
            // Every event is for step 1, the step after the frontier: valid
            // for the follower, and applied without waiting for a step.
            let events = (0..EVENTS_PER_TURN)
                .map(|_| {
                    seq += 1;
                    Event {
                        seq: seq - 1,
                        step: 1,
                        body: EventBody::Command {
                            player: PlayerId(FixedBytes([7; 32])),
                            client_seq: seq,
                            payload: Payload::new(vec![0xab; MAX_PAYLOAD]).unwrap(),
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
            if turns.write_all(&frame).await.is_err() {
                break;
            }
        }
        // Hold the connection open until the test ends.
        std::future::pending::<()>().await;
        drop((endpoint, connection, send, turns));
    });

    let player = Arc::new(Identity::generate().unwrap().0);
    let (client, mut events) = connect(ConnectOptions::new(
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
    let bridge = tokio::spawn(async move {
        let mut bridge = Bridge::new(IdleHook, BridgeOptions::default());
        let ended = bridge.run(&client, &mut events).await;
        // Keep everything alive so the measurement sees what the bridge held.
        std::future::pending::<()>().await;
        drop((ended, bridge, client, events));
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut held;
    loop {
        held = LIVE.load(Relaxed).saturating_sub(before) as f64;
        if held > STOP_AT || Instant::now() > deadline || bridge.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bridge.abort();
    server.abort();
    println!(
        "the bridge held {:.1} MiB for a hook that was not reading",
        held / MIB
    );
    // The bound `poc_client_memory.rs` holds the client to.
    assert!(
        held < 192.0 * MIB,
        "a hostile server made the bridge hold {:.1} MiB of events for a hook that was not \
         reading (still growing when measured; the client's turn budget is 64 MiB and the \
         follower's backlog bound 256 MiB)",
        held / MIB
    );
}
