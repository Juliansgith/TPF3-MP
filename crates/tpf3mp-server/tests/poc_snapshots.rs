//! Adversarial-review proofs of concept for the server's snapshot handling.
//!
//! These are `#[ignore]`d so the normal suite stays green; run them with
//! `cargo test -p tpf3mp-server --test poc_snapshots -- --ignored`.
//!
//! They exercise `ChunkStore` directly, because that is the one store the
//! server shares across every room (`Server::bind` opens one `Snapshots`,
//! whose `ChunkStore` is cloned into every room's `RoomEnv`). The operations
//! used here are exactly what `snapshots.rs` performs:
//! `Snapshots::release` is `store.release`, and `collect_released` is
//! `store.gc([])`.

#![allow(clippy::unwrap_used)]

use tpf3mp_snapshot::{ChunkParams, ChunkStore, StoreConfig};

/// SplitMix64 bytes: deterministic and incompressible, like the net bulk
/// tests use.
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

/// Small chunks so a modest world still has many, keeping the test quick.
fn params() -> ChunkParams {
    ChunkParams::new(16 << 10, 64 << 10, 256 << 10).unwrap()
}

fn server_store(dir: &tempfile::TempDir) -> ChunkStore {
    // The server's own configuration (see `Snapshots::open`).
    let mut config = StoreConfig::new(1 << 30);
    config.recompress_received = true;
    ChunkStore::open(dir.path(), config).unwrap()
}

/// PoC: the snapshot store is content-addressed and shared by every room, but
/// retention is a set keyed by `ManifestId` with no per-room reference count.
/// Two rooms whose agreed world is bit-identical therefore share one `roots/`
/// record. When the first room promotes a newer world it releases the old one
/// (`promote` -> `Snapshots::release`), and the next `collect_released`
/// (`store.gc([])`) deletes the shared chunks -- destroying the *other* room's
/// still-current world. The other room's in-memory `current` manifest is never
/// passed to `gc` as `live`, so it offers a snapshot whose chunks are gone.
///
/// Realistic trigger: two rooms started from the same base/scenario save, or
/// the newly added "start every player from the owner's world" when two owners
/// use the identical world file.
#[test]
#[ignore = "review PoC: releasing a snapshot one room holds deletes an identical world another room still serves"]
fn releasing_a_shared_snapshot_destroys_another_rooms_world() {
    let dir = tempfile::tempdir().unwrap();
    let store = server_store(&dir);

    let world = bytes(1, 800_000);
    // Room A receives/retains the world.
    let a = store.ingest(world.as_slice(), params()).unwrap();
    // Room B, on the same node, agrees on the SAME bytes. Identical content
    // hashes to the same ManifestId, so both rooms share ONE root record.
    let b = store.ingest(world.as_slice(), params()).unwrap();
    assert_eq!(a.id(), b.id(), "identical worlds share a snapshot id");
    assert_eq!(
        store.retained().unwrap(),
        vec![a.id()],
        "the shared world has a single root record, not one per room"
    );

    // Room A promotes a newer world: it releases this one (its `previous`)
    // and the server's timed collector runs.
    store.release(&a.id()).unwrap();
    store.gc([]).unwrap();

    // Room B still holds `b` as its current, agreed world, but the release by
    // Room A destroyed it: gc kept nothing, because Room B's in-memory
    // manifest is not a root record and is not passed to gc.
    let first_chunk = b.chunks()[0].id;
    assert!(
        store.read(&first_chunk).is_err(),
        "Room B's world survived Room A's release, so this PoC is stale"
    );
    assert_eq!(
        store.missing(&b).unwrap().len(),
        b.unique_chunks().len(),
        "every chunk of Room B's still-current world is now missing; \
         Room B can no longer serve the world it offers"
    );
}
