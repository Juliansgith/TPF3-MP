//! Memory bounds, from the security review's proofs of concept. A counting
//! global allocator measures what the server keeps alive; server and
//! clients share the process, so each test only measures after the client
//! side has settled (all its data acknowledged and freed), and the tests
//! take turns.

#![allow(unsafe_code, clippy::unwrap_used)]

mod common;

use std::{
    alloc::{GlobalAlloc, Layout, System},
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering::Relaxed},
    time::{Duration, Instant},
};

use common::{FAST, RunningServer, TestClient, content, new_identity, room};
use tpf3mp_net::{read_message, read_preamble, write_message, write_preamble};
use tpf3mp_proto::{
    CONTROL_MAX_FRAME, ClientMessage, Hello, MAX_PAYLOAD, PROTOCOL_VERSION, Payload, Platform,
    ServerMessage, Text,
};
use tpf3mp_server::ServerConfig;

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(bytes: usize) {
    let now = LIVE.fetch_add(bytes, Relaxed) + bytes;
    PEAK.fetch_max(now, Relaxed);
}

// SAFETY: every method forwards to the system allocator unchanged and only
// updates counters.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            grew(layout.size());
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
                grew(new_size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Relaxed);
            }
        }
        new
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn live() -> usize {
    LIVE.load(Relaxed)
}

fn mib(bytes: f64) -> f64 {
    bytes / (1024.0 * 1024.0)
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(700)).await;
}

fn data_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tpf3mp-mem-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn dir_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|entry| std::fs::metadata(entry.unwrap().path()).unwrap().len())
                .sum()
        })
        .unwrap_or(0)
}

fn persistent(dir: &Path, secret: [u8; 32]) -> impl FnOnce(&mut ServerConfig) {
    let dir = dir.to_owned();
    move |config: &mut ServerConfig| {
        config.data_dir = Some(dir);
        config.secret = secret;
    }
}

/// The counting allocator sees the whole process, so these tests take turns.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Review finding H3: with quinn's default receive buffers, a peer with a
/// throwaway key pinned about 21 MiB per connection in streams and datagrams
/// the server never reads. The server now grants a client its one control
/// stream and nothing else, with small receive windows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_cannot_pin_server_memory_with_unread_streams() {
    let _serial = SERIAL.lock().await;
    let server = RunningServer::start(|_| {}).await;
    {
        // Warm up allocations a first connection makes once.
        let warm = server.client("warm").await;
        warm.client.close().await;
    }
    settle().await;
    let before = live();

    let (endpoint, connection) = server.raw_connection().await;
    let (mut send, mut recv) = connection.open_bi().await.unwrap();
    write_preamble(&mut send, PROTOCOL_VERSION).await.unwrap();
    read_preamble(&mut recv).await.unwrap();
    let mallory = new_identity();
    let hello = ClientMessage::Hello(Hello {
        client_version: Text::new("poc").unwrap(),
        platform: Platform::current(),
        name: Text::new("mallory").unwrap(),
        identity: mallory.player(),
        proof: mallory.prove(&connection).unwrap(),
    });
    write_message(&mut send, &hello, CONTROL_MAX_FRAME)
        .await
        .unwrap();
    let _welcome = read_message::<ServerMessage>(&mut recv, CONTROL_MAX_FRAME)
        .await
        .unwrap();

    let refused = Duration::from_millis(500);
    assert!(
        tokio::time::timeout(refused, connection.open_uni())
            .await
            .is_err(),
        "no unidirectional streams"
    );
    assert!(
        tokio::time::timeout(refused, connection.open_bi())
            .await
            .is_err(),
        "no second bidirectional stream"
    );
    assert_eq!(connection.max_datagram_size(), None, "no datagrams");
    settle().await;
    let held = live().saturating_sub(before);

    connection.close(0u32.into(), b"done");
    endpoint.wait_idle().await;
    server.shut_down().await;
    assert!(held < 2 * 1024 * 1024, "one connection held {held} bytes");
}

/// Review finding H2: a room kept every sealed turn in memory and on disk
/// with no cap. One player sending 20 intents of `MAX_PAYLOAD` (48 KiB) per
/// second grew the server by about 1 MB/s, and recovery read every log back
/// into memory twice over. Payload bytes are now budgeted per player, the
/// resume window is bounded in bytes, and recovery streams the log.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_player_cannot_grow_a_room_quickly() {
    let _serial = SERIAL.lock().await;
    let dir = data_dir("growth");
    let secret = [9; 32];
    let server = RunningServer::start(persistent(&dir, secret)).await;
    let TestClient {
        client, mut events, ..
    } = server.client("mallory").await;
    client.create_room(room("mine", FAST)).await.unwrap();
    client.declare_content(content(1)).await.unwrap();
    client.set_ready(true).await.unwrap();
    client.start_game().await.unwrap();
    // Read everything like a real client, so the server never sees a slow
    // consumer.
    let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });
    settle().await;
    let heap_before = live();
    let disk_before = dir_bytes(&dir);

    let big = Payload::new(vec![0xab; MAX_PAYLOAD]).unwrap();
    let started = Instant::now();
    let mut seq = 0;
    while started.elapsed() < Duration::from_secs(6) {
        client.send_intent(seq, big.clone()).await.unwrap();
        seq += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let seconds = started.elapsed().as_secs_f64();
    settle().await;
    let heap = live().saturating_sub(heap_before) as f64;
    let disk = dir_bytes(&dir).saturating_sub(disk_before) as f64;
    let log_bytes = dir_bytes(&dir) as f64;

    client.close().await;
    drain.abort();
    server.shut_down().await;
    settle().await;

    // Recovery reads the log back: measure its peak.
    let base = live();
    PEAK.store(base, Relaxed);
    let restarted = RunningServer::start(persistent(&dir, secret)).await;
    let peak = PEAK.load(Relaxed).saturating_sub(base) as f64;
    let resident = live().saturating_sub(base) as f64;
    restarted.shut_down().await;
    let _ = std::fs::remove_dir_all(&dir);

    println!(
        "{seq} intents in {seconds:.1} s: server heap +{:.2} MiB, log +{:.2} MiB",
        mib(heap),
        mib(disk),
    );
    println!(
        "restart with a {:.2} MiB log: recovery peak {:.2} MiB, {:.2} MiB stays resident",
        mib(log_bytes),
        mib(peak),
        mib(resident),
    );
    // The burst (256 KiB) and 32 KiB per second, plus turn overhead.
    let mib = 1024.0 * 1024.0;
    assert!(disk < 1.0 * mib, "log grew {disk} bytes in {seconds:.1} s");
    assert!(heap < 2.0 * mib, "heap grew {heap} bytes");
    assert!(
        peak < log_bytes + 2.0 * mib,
        "recovery peak {peak} for a {log_bytes}-byte log"
    );
}
