use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use serde::Serialize;
use tpf3mp_snapshot::{
    ChunkEntry, ChunkId, ChunkParams, ChunkSink, ChunkStore, FileHash, StoreConfig,
};

/// SplitMix64 bytes: fast, deterministic, incompressible.
pub fn random_bytes(seed: u64, len: usize) -> Vec<u8> {
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

/// Data that repeats itself: fresh random runs, zero runs and copies of
/// earlier runs, so the same chunk occurs more than once.
pub fn repetitive_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(len);
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut step = 0u64;
    while data.len() < len {
        let choice = random_bytes(seed ^ step, 8);
        let run = 10_000 + usize::from(u16::from_le_bytes([choice[0], choice[1]])) * 6;
        let start = data.len();
        match choice[2] % 3 {
            0 => data.extend(random_bytes(seed.wrapping_add(step), run)),
            1 => data.resize(start + run, 0),
            _ => match runs.get(usize::from(choice[3]) % runs.len().max(1)) {
                Some(&(from, to)) => data.extend_from_within(from..to),
                None => data.resize(start + run, 0),
            },
        }
        runs.push((start, data.len()));
        step += 1;
    }
    data.truncate(len);
    data
}

/// 16 KiB / 64 KiB / 256 KiB: many chunks from little data, which keeps
/// debug-build tests fast.
pub fn small_params() -> ChunkParams {
    ChunkParams::new(16 << 10, 64 << 10, 256 << 10).unwrap()
}

/// An unbounded store that skips flushing, which only matters on power loss.
pub fn store(root: &Path) -> ChunkStore {
    store_with(root, |_| {})
}

pub fn store_with(root: &Path, configure: impl FnOnce(&mut StoreConfig)) -> ChunkStore {
    let mut config = StoreConfig::new(u64::MAX);
    config.sync_chunks = false;
    configure(&mut config);
    ChunkStore::open(root, config).unwrap()
}

pub fn chunk_file(root: &Path, id: &ChunkId) -> PathBuf {
    let hex = id.to_string();
    root.join("chunks").join(&hex[..2]).join(hex)
}

/// Every chunk file in a store, read from the directory itself.
pub fn chunk_files(root: &Path) -> BTreeSet<ChunkId> {
    let mut ids = BTreeSet::new();
    for fan in fs::read_dir(root.join("chunks")).unwrap() {
        let fan = fan.unwrap();
        let prefix = fan.file_name().into_string().unwrap();
        for entry in fs::read_dir(fan.path()).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            if let Some(id) = ChunkId::from_hex(&name).filter(|_| name.starts_with(&prefix)) {
                ids.insert(id);
            }
        }
    }
    ids
}

/// Feeds `sink` the chunks it lacks from `server`, as a network layer would.
pub fn serve(server: &ChunkStore, sink: &mut ChunkSink, entries: &[ChunkEntry]) {
    for entry in entries {
        let frame = server.read_compressed(&entry.id).unwrap();
        sink.put(&entry.id, &frame).unwrap();
    }
}

#[derive(Serialize)]
struct WireManifest<'a> {
    magic: [u8; 4],
    version: u8,
    params: [u32; 3],
    total_size: u64,
    file_hash: FileHash,
    chunks: &'a [ChunkEntry],
}

/// Encodes a manifest from any field values, as a hostile peer might.
pub fn encode_manifest(
    params: [u32; 3],
    total_size: u64,
    file_hash: FileHash,
    chunks: &[ChunkEntry],
) -> Vec<u8> {
    postcard::to_stdvec(&WireManifest {
        magic: *b"T3SM",
        version: 1,
        params,
        total_size,
        file_hash,
        chunks,
    })
    .unwrap()
}
