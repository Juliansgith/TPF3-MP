//! Adversarial-review proof of concept for `bulk::fetch`.
//!
//! `#[ignore]`d so the normal suite stays green; run with
//! `cargo test -p tpf3mp-net --test poc_bulk_leak -- --ignored`.
//!
//! `bulk::fetch` opens a `ChunkSink` (which records a durable `pending/`
//! manifest) and, on any mid-transfer failure, propagates the error with `?`
//! and lets the sink drop. Dropping a `ChunkSink` does NOT abandon it, and
//! `ChunkStore::gc` deliberately KEEPS the chunks and record of every pending
//! transfer. So a fetch that fails partway leaves its `pending/` record and
//! the chunks it stored on disk, and no garbage collection ever reclaims them.
//!
//! A hostile or flaky server can drive this against the agent's world store:
//! offer a world, serve part of it, then fail (or just let the agent supersede
//! the fetch with a newer offer, which aborts the task and drops the sink).
//! Each distinct never-completed world permanently pins disk in the store,
//! up to `max_bytes`. The same happens in the server's store for uploads that
//! fail partway (`connection.rs` `bulk_stream`'s `Serve` arm also calls
//! `bulk::fetch`).

#![allow(clippy::unwrap_used)]

use std::{net::SocketAddr, time::Duration};

use quinn::{Connection, Endpoint, RecvStream, SendStream};
use tpf3mp_net::{
    ServerIdentity, ServerTrust,
    bulk::{self, Completion},
    client_config, server_config,
};
use tpf3mp_snapshot::{ChunkParams, ChunkSink, ChunkStore, StoreConfig};

const IDLE: Duration = Duration::from_secs(10);

fn bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed;
    let mut data = Vec::with_capacity(len + 8);
    while data.len() < len {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        data.extend_from_slice(&(z ^ (z >> 31)).to_le_bytes());
    }
    data.truncate(len);
    data
}

fn params() -> ChunkParams {
    ChunkParams::new(16 << 10, 64 << 10, 256 << 10).unwrap()
}

fn store(dir: &tempfile::TempDir) -> ChunkStore {
    let mut config = StoreConfig::new(1 << 30);
    config.sync_chunks = false;
    ChunkStore::open(dir.path(), config).unwrap()
}

struct Pipe {
    fetcher: (SendStream, RecvStream),
    server: (SendStream, RecvStream),
    _alive: (Endpoint, Endpoint, Connection, Connection),
}

async fn pipe() -> Pipe {
    let identity = ServerIdentity::self_signed(&["localhost"]).unwrap();
    let leaf = identity.leaf().clone();
    let server = Endpoint::server(
        server_config(identity).unwrap(),
        SocketAddr::from(([127, 0, 0, 1], 0)),
    )
    .unwrap();
    let mut client = Endpoint::client(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
    client.set_default_client_config(client_config(ServerTrust::Pinned(leaf)).unwrap());
    let address = server.local_addr().unwrap();
    let accepting = tokio::spawn({
        let server = server.clone();
        async move { server.accept().await.unwrap().await.unwrap() }
    });
    let connection = client.connect(address, "localhost").unwrap().await.unwrap();
    let server_connection = accepting.await.unwrap();
    let (mut send, recv) = connection.open_bi().await.unwrap();
    send.write_all(b"x").await.unwrap();
    let (server_send, mut server_recv) = server_connection.accept_bi().await.unwrap();
    let mut first = [0; 1];
    server_recv.read_exact(&mut first).await.unwrap();
    Pipe {
        fetcher: (send, recv),
        server: (server_send, server_recv),
        _alive: (client, server, connection, server_connection),
    }
}

/// PoC: a fetch that fails after storing some chunks leaves a `pending/`
/// record and partial chunks that `gc` never reclaims.
#[tokio::test]
#[ignore = "review PoC: bulk::fetch leaks a pending transfer (record + partial chunks) on mid-transfer failure; gc never reclaims it"]
async fn a_failed_fetch_pins_partial_chunks_forever() {
    let (server_dir, client_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (server, client) = (store(&server_dir), store(&client_dir));

    // A world with several chunks; the server then loses its LAST chunk, so it
    // can serve the earlier ones but fails partway (as a flaky/hostile server,
    // or a superseded fetch, would).
    let world = bytes(1, 900_000);
    let manifest = server.ingest(world.as_slice(), params()).unwrap();
    let unique = manifest.unique_chunks();
    assert!(unique.len() >= 3, "need a multi-chunk world");
    let last = unique.last().unwrap().id;
    let hex = last.to_string();
    let path = server
        .root()
        .join("chunks")
        .join(&hex[..2])
        .join(&hex);
    std::fs::remove_file(&path).unwrap();

    let Pipe {
        fetcher: (mut send, mut recv),
        server: (mut server_send, mut server_recv),
        _alive,
    } = pipe().await;
    let serving = tokio::spawn({
        let server = server.clone();
        let manifest = manifest.clone();
        async move {
            let _ = bulk::serve(&mut server_send, &mut server_recv, &server, &manifest, IDLE).await;
            let _ = server_send.finish();
        }
    });
    let fetched = bulk::fetch(
        &mut send,
        &mut recv,
        &client,
        &manifest.id(),
        Completion::Retain,
        IDLE,
        |_| {},
    )
    .await;
    let _ = send.finish();
    serving.await.unwrap();

    // The fetch failed (the server was missing a chunk).
    assert!(fetched.is_err(), "the fetch should not have completed");

    // It left a durable pending record and the chunks it did store.
    assert_eq!(
        client.pending().unwrap(),
        vec![manifest.id()],
        "the aborted transfer left a pending record"
    );
    let leaked = client.used_bytes();
    assert!(leaked > 0, "the aborted transfer stored some chunks");

    // Garbage collection does NOT reclaim it: pending records and their chunks
    // are kept. This is the leak -- nothing in the server or agent ever calls
    // `abandon` on a fetch that `bulk::fetch` failed and dropped.
    client.gc([]).unwrap();
    assert_eq!(
        client.pending().unwrap(),
        vec![manifest.id()],
        "gc kept the abandoned pending record"
    );
    assert_eq!(
        client.used_bytes(),
        leaked,
        "gc reclaimed none of the partial chunks the failed fetch left behind"
    );

    // For contrast: the ONLY way to reclaim it is an explicit abandon, which
    // `bulk::fetch` never performs on failure.
    ChunkSink::resume(&client, &manifest.id())
        .unwrap()
        .abandon()
        .unwrap();
    client.gc([]).unwrap();
    assert!(client.pending().unwrap().is_empty());
    assert_eq!(
        client.used_bytes(),
        0,
        "an explicit abandon + gc is what it would take to reclaim the leak"
    );
}
