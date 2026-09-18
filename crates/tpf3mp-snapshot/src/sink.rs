use std::{
    collections::{HashMap, HashSet},
    fmt,
    path::Path,
};

use thiserror::Error;
use zstd::bulk::{Compressor, Decompressor};

use crate::{
    ChunkEntry, ChunkError, ChunkId, ChunkStore, Manifest, ManifestId, StoreError, codec,
    store::{Record, SinkClaim},
};

/// How much of a snapshot the local store holds, counting each distinct chunk
/// once. It starts above zero when the store already has chunks of an
/// earlier snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub chunks_present: usize,
    pub chunks_total: usize,
    /// Uncompressed bytes.
    pub bytes_present: u64,
    pub bytes_total: u64,
}

impl Progress {
    pub fn is_complete(&self) -> bool {
        self.chunks_present == self.chunks_total
    }
}

#[derive(Debug, Error)]
pub enum SinkError {
    #[error("chunk {0} is not part of this snapshot")]
    NotInManifest(ChunkId),
    #[error("chunk {id} was rejected: {reason}")]
    Rejected { id: ChunkId, reason: ChunkError },
    #[error("{missing} chunks are still missing")]
    Incomplete { missing: usize },
    #[error("a transfer of snapshot {0} is already open in this process")]
    AlreadyOpen(ManifestId),
    #[error("the snapshot can never be assembled, so the transfer was dropped: {0}")]
    Inconsistent(StoreError),
    #[error("the transfer is already finished")]
    Finished,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Receives the chunks of one snapshot, in any order, and assembles the file
/// once all are present.
///
/// Every chunk is checked before it is stored: it must belong to the
/// manifest, be one zstd frame within the bound for its length, decompress to
/// exactly that length, and hash to its id. Chunks the store already holds,
/// from earlier snapshots or other transfers, are never fetched again.
///
/// The transfer survives restarts. Opening a sink records the manifest in the
/// store, verified chunks go straight into the store, and
/// [`ChunkSink::resume`] picks up from whatever the store holds. Garbage
/// collection keeps the chunks of unfinished transfers. When the transfer
/// finishes, the store retains the snapshot until it is
/// [released](ChunkStore::release).
///
/// A process can have one sink per snapshot at a time. Its methods do blocking
/// file I/O; async callers run them on a blocking thread.
pub struct ChunkSink {
    store: ChunkStore,
    manifest: Manifest,
    /// The first entry of every distinct chunk, in file order.
    unique: Vec<ChunkEntry>,
    lengths: HashMap<ChunkId, u32>,
    /// Distinct chunks the store does not hold yet.
    missing: HashSet<ChunkId>,
    progress: Progress,
    decompressor: Decompressor<'static>,
    /// For stores that compress received chunks again.
    compressor: Option<Compressor<'static>>,
    finished: bool,
    /// This sink's hold on its snapshot, until the transfer is over.
    claim: Option<SinkClaim>,
}

impl ChunkSink {
    /// Starts, or restarts, the transfer of `manifest` into `store`.
    pub fn open(store: &ChunkStore, manifest: Manifest) -> Result<Self, SinkError> {
        let claim = claim(store, &manifest.id())?;
        store.save_pending(&manifest)?;
        Self::start(store, manifest, claim)
    }

    /// Continues an unfinished transfer after a restart, from the manifest the
    /// store saved. [`ChunkStore::pending`] lists the candidates.
    pub fn resume(store: &ChunkStore, id: &ManifestId) -> Result<Self, SinkError> {
        let claim = claim(store, id)?;
        let manifest = store.load_record(Record::Pending, id)?;
        Self::start(store, manifest, claim)
    }

    fn start(store: &ChunkStore, manifest: Manifest, claim: SinkClaim) -> Result<Self, SinkError> {
        let unique = manifest.unique_chunks();
        let lengths = unique.iter().map(|entry| (entry.id, entry.len)).collect();
        let decompressor = codec::decompressor().map_err(StoreError::Compression)?;
        let compressor = if store.config().recompress_received {
            let level = store.config().compression_level;
            Some(codec::compressor(level).map_err(StoreError::Compression)?)
        } else {
            None
        };
        let mut sink = Self {
            store: store.clone(),
            manifest,
            unique,
            lengths,
            missing: HashSet::new(),
            progress: Progress {
                chunks_present: 0,
                chunks_total: 0,
                bytes_present: 0,
                bytes_total: 0,
            },
            decompressor,
            compressor,
            finished: false,
            claim: Some(claim),
        };
        sink.refresh()?;
        Ok(sink)
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn progress(&self) -> Progress {
        self.progress
    }

    /// The chunks still to fetch, in file order.
    pub fn missing(&self) -> Vec<ChunkEntry> {
        self.unique
            .iter()
            .filter(|entry| self.missing.contains(&entry.id))
            .copied()
            .collect()
    }

    /// Checks one chunk as received from a peer, its zstd frame, and stores
    /// it.
    ///
    /// A chunk the store already holds is accepted without being read, so
    /// resending is harmless. Chunks outside the manifest, and chunks that
    /// fail verification, are rejected and leave no trace.
    pub fn put(&mut self, id: &ChunkId, frame: &[u8]) -> Result<Progress, SinkError> {
        if self.finished {
            return Err(SinkError::Finished);
        }
        let Some(&raw_len) = self.lengths.get(id) else {
            return Err(SinkError::NotInManifest(*id));
        };
        if !self.missing.contains(id) {
            return Ok(self.progress);
        }
        let raw = codec::decode(&mut self.decompressor, id, raw_len, frame)
            .map_err(|reason| SinkError::Rejected { id: *id, reason })?;
        match &mut self.compressor {
            Some(compressor) => {
                let own = compressor.compress(&raw).map_err(StoreError::Compression)?;
                self.store.insert_frame(id, raw_len, &own)?;
            }
            None => {
                self.store.insert_frame(id, raw_len, frame)?;
            }
        }
        self.missing.remove(id);
        self.progress.chunks_present += 1;
        self.progress.bytes_present += u64::from(raw_len);
        Ok(self.progress)
    }

    /// Assembles and verifies the file, retains the snapshot in the store, and
    /// publishes the file at `dest` atomically. If this fails, nothing was
    /// published.
    ///
    /// If stored chunks turn out to be damaged, they are removed and this
    /// fails; [`missing`](Self::missing) then lists them, so fetching them
    /// again and retrying completes the transfer. If the manifest contradicts
    /// its own chunks, no retry can succeed: the transfer is dropped and this
    /// returns [`SinkError::Inconsistent`].
    pub fn finish(&mut self, dest: &Path) -> Result<(), SinkError> {
        if self.finished {
            return Err(SinkError::Finished);
        }
        if !self.missing.is_empty() {
            return Err(SinkError::Incomplete {
                missing: self.missing.len(),
            });
        }
        let store = &self.store;
        let manifest = &self.manifest;
        match store.assemble_then(manifest, dest, || store.promote(manifest)) {
            Ok(()) => {
                self.end();
                Ok(())
            }
            Err(error @ (StoreError::FileHashMismatch | StoreError::LengthMismatch { .. })) => {
                // Left pending, it would pin its chunks forever.
                self.end();
                self.store
                    .remove_record(Record::Pending, &self.manifest.id())?;
                Err(SinkError::Inconsistent(error))
            }
            Err(error) => {
                // Damaged chunks were removed; asking the store again lists
                // them as missing. If that fails too, the original error says
                // more.
                let _ = self.refresh();
                Err(error.into())
            }
        }
    }

    /// Gives up the transfer. Its chunks stay in the store, where they still
    /// count for later transfers, until garbage collection removes them.
    pub fn abandon(self) -> Result<(), SinkError> {
        if !self.finished {
            self.store
                .remove_record(Record::Pending, &self.manifest.id())?;
        }
        Ok(())
    }

    /// Marks the transfer as over and lets another sink take the snapshot.
    fn end(&mut self) {
        self.finished = true;
        self.claim = None;
    }

    /// Asks the store again which chunks it holds.
    pub fn refresh(&mut self) -> Result<Progress, SinkError> {
        let missing = self.store.missing(&self.manifest)?;
        self.missing = missing.iter().map(|entry| entry.id).collect();
        let bytes_total = self.unique.iter().map(|entry| u64::from(entry.len)).sum();
        let bytes_missing: u64 = missing.iter().map(|entry| u64::from(entry.len)).sum();
        self.progress = Progress {
            chunks_present: self.unique.len() - missing.len(),
            chunks_total: self.unique.len(),
            bytes_present: bytes_total - bytes_missing,
            bytes_total,
        };
        Ok(self.progress)
    }
}

fn claim(store: &ChunkStore, id: &ManifestId) -> Result<SinkClaim, SinkError> {
    store.claim_sink(id).ok_or(SinkError::AlreadyOpen(*id))
}

impl fmt::Debug for ChunkSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkSink")
            .field("manifest", &self.manifest.id())
            .field("progress", &self.progress)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}
