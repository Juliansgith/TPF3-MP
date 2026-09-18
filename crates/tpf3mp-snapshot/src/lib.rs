//! Content-addressed, deduplicated storage and transfer planning for large
//! world saves.
//!
//! The dedicated server keeps the latest agreed native save of every room, and
//! players who hot-join, reconnect or are rebased download it (see
//! `docs/ARCHITECTURE.md`). Saves run to hundreds of megabytes, but successive
//! saves of one world share most of their bytes. So a save is stored and sent
//! as chunks:
//!
//! - [`Chunker`] cuts a stream into content-defined chunks with FastCDC 2020,
//!   so an insertion or deletion only changes the chunks around it;
//! - each chunk is named by the BLAKE3 hash of its bytes ([`ChunkId`]) and
//!   kept once, compressed with zstd, in a [`ChunkStore`];
//! - a [`Manifest`] lists a file's chunks in order, with its size and BLAKE3
//!   hash, in a canonical encoding;
//! - a receiver asks its own store which chunks it lacks
//!   ([`ChunkStore::missing`]), fetches only those, and hands them in any order
//!   to a [`ChunkSink`], which verifies each one, survives restarts, and
//!   finally assembles the file and verifies it against the manifest.
//!
//! Manifests and chunks may come from hostile peers. Decoding is bounded
//! before anything is allocated, every chunk is checked against its id after
//! decompression, and every assembled file against the manifest's hash. The
//! only file names derived from peer input are hex ids.
//!
//! Everything here is synchronous and does blocking file I/O; async callers
//! run it on a blocking thread. `docs/SNAPSHOTS.md` describes the format, the
//! parameters and their measurements, and what the network layer must provide.

mod chunker;
mod codec;
mod id;
mod manifest;
mod params;
mod sink;
mod store;

pub use chunker::{Chunk, Chunker, SourceError};
pub use codec::{ChunkError, MAX_COMPRESSED_CHUNK_LEN};
pub use id::{ChunkId, FileHash, ManifestId};
pub use manifest::{
    ChunkEntry, ChunkingError, MANIFEST_VERSION, MAX_MANIFEST_LEN, Manifest, ManifestError,
    ManifestField,
};
pub use params::{ChunkParams, MAX_CHUNK_LEN, MIN_CHUNK_FLOOR, ParamsError};
pub use sink::{ChunkSink, Progress, SinkError};
pub use store::{ChunkStore, GcStats, StateDamage, StoreConfig, StoreError};

/// The largest file a manifest may describe. Saves are expected to stay below
/// 500 MB.
pub const MAX_FILE_SIZE: u64 = 4 << 30;

/// The most chunks a manifest may list: a file of [`MAX_FILE_SIZE`] cut at
/// the smallest allowed minimum chunk size.
pub const MAX_CHUNKS: usize = (MAX_FILE_SIZE / MIN_CHUNK_FLOOR as u64) as usize;
