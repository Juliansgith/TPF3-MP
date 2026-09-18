use std::{
    collections::{BTreeSet, HashSet},
    ffi::OsString,
    fmt,
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
        atomic::{AtomicU64, Ordering},
    },
};

use thiserror::Error;
use zstd::bulk::Decompressor;

use crate::{
    ChunkEntry, ChunkError, ChunkId, ChunkParams, Chunker, MAX_COMPRESSED_CHUNK_LEN,
    MAX_MANIFEST_LEN, Manifest, ManifestError, ManifestId, SourceError, codec, manifest::chunk_len,
};

/// Every chunk file starts with these bytes and the chunk's uncompressed
/// length (`u32`, little-endian); the zstd frame follows.
const CHUNK_MAGIC: [u8; 4] = *b"T3SC";
const CHUNK_HEADER_LEN: usize = 8;
const MAX_CHUNK_FILE_LEN: u64 = (CHUNK_HEADER_LEN + MAX_COMPRESSED_CHUNK_LEN) as u64;

const LOCK_FILE: &str = "lock";
const CHUNKS_DIR: &str = "chunks";
const PENDING_DIR: &str = "pending";
const TEMP_DIR: &str = "tmp";

#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// Upper bound on the bytes of chunk files the store keeps. A chunk that
    /// would exceed it is refused with [`StoreError::Full`]; [`ChunkStore::gc`]
    /// makes room.
    pub max_bytes: u64,
    /// zstd level for chunks the store compresses itself during
    /// [`ChunkStore::ingest`]. Any level produces the same format, so it can
    /// change at any time.
    pub compression_level: i32,
    /// Flush every chunk to stable storage before it becomes visible, and a
    /// finished [`ChunkStore::ingest`] to its directories. Without it, a power
    /// failure can leave damaged chunks behind. Reads detect and remove those,
    /// so the cost is downloading them again, not a wrong file.
    pub sync_chunks: bool,
}

impl StoreConfig {
    pub const DEFAULT_COMPRESSION_LEVEL: i32 = 3;

    pub fn new(max_bytes: u64) -> Self {
        Self {
            max_bytes,
            compression_level: Self::DEFAULT_COMPRESSION_LEVEL,
            sync_chunks: true,
        }
    }
}

/// What [`ChunkStore::gc`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcStats {
    pub kept_chunks: u64,
    pub kept_bytes: u64,
    pub removed_chunks: u64,
    pub freed_bytes: u64,
    /// Saved transfer states that could not be read back and were dropped.
    pub dropped_pending: u64,
}

/// A directory of compressed chunks, each stored once under its [`ChunkId`].
///
/// Layout: `chunks/<first two hex digits>/<64 hex digits>` for chunks,
/// `pending/<manifest id>` for the manifests of unfinished [`ChunkSink`]
/// transfers, and `tmp/` for files being written. File names are only ever
/// formatted from ids, never taken from peers, so no input can name a path
/// outside the store.
///
/// Every write goes to `tmp/` first and is renamed into place, so no reader
/// sees a partly written chunk, and puts of a chunk that is already stored do
/// nothing. Every read decompresses the chunk and checks it against its id; a
/// damaged chunk (bit rot, or a power failure without
/// [`sync_chunks`](StoreConfig::sync_chunks)) is removed so that it counts as
/// missing again. [`has`](Self::has) and [`ingest`](Self::ingest) trust that
/// a file present under an id holds that chunk; damage surfaces on the next
/// read.
///
/// One process at a time may open a store (a lock file enforces it). Within
/// that process the store is `Clone + Send + Sync`; clones share state.
///
/// [`ChunkSink`]: crate::ChunkSink
#[derive(Clone)]
pub struct ChunkStore {
    inner: Arc<Inner>,
}

struct Inner {
    root: PathBuf,
    config: StoreConfig,
    /// Checking the bound and renaming a chunk into place happen under this
    /// lock, so concurrent puts cannot overshoot the bound.
    accounting: Mutex<Accounting>,
    /// Shared by everything that must not see chunks or temporary files
    /// vanish midway (ingest, puts, assembly), exclusive for garbage
    /// collection.
    gc: RwLock<()>,
    next_temp: AtomicU64,
    _lock: File,
}

struct Accounting {
    /// Bytes of chunk files.
    used: u64,
    /// Fan-out directories known to exist. Each is created on first use,
    /// which keeps opening a new store cheap.
    fans: [bool; 256],
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("cannot {action} {}: {source}", path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    #[error("the chunk store at {} is in use by another process", .0.display())]
    Locked(PathBuf),
    #[error("the chunk store is full: {used} of {limit} bytes in use, {needed} more needed")]
    Full { used: u64, limit: u64, needed: u64 },
    #[error("chunk {0} is not in the store")]
    NotFound(ChunkId),
    #[error("chunk {id} was damaged and has been removed: {reason}")]
    Corrupt { id: ChunkId, reason: ChunkError },
    #[error("{missing} chunks of the file are not in the store")]
    Incomplete { missing: usize },
    #[error("chunk {id} is {actual} bytes, but the manifest says {expected}")]
    LengthMismatch {
        id: ChunkId,
        expected: u32,
        actual: usize,
    },
    #[error("the assembled file does not match the manifest's file hash")]
    FileHashMismatch,
    #[error("{} does not name a file", .0.display())]
    InvalidDestination(PathBuf),
    #[error("no saved transfer state for snapshot {0}")]
    NoState(ManifestId),
    #[error("the saved transfer state for snapshot {id} is damaged: {reason}")]
    DamagedState { id: ManifestId, reason: StateDamage },
    #[error("zstd failed: {0}")]
    Compression(io::Error),
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
}

/// Why a saved transfer state could not be used.
#[derive(Debug, Error)]
pub enum StateDamage {
    #[error("{0} bytes is longer than any manifest")]
    TooLong(u64),
    #[error(transparent)]
    Manifest(ManifestError),
    #[error("it holds snapshot {0}")]
    OtherSnapshot(ManifestId),
}

/// Wraps an I/O error with what was being done to which path. The path is
/// only copied if the error happens.
fn io_error<'a>(action: &'static str, path: &'a Path) -> impl FnOnce(io::Error) -> StoreError + 'a {
    move |source| StoreError::Io {
        action,
        path: path.to_owned(),
        source,
    }
}

impl ChunkStore {
    /// Opens the store at `root`, creating it if needed.
    ///
    /// Files left in `tmp/` by an interrupted process are deleted, and the
    /// size of the existing chunks is counted. A store already above
    /// `config.max_bytes` opens, but refuses new chunks until
    /// [`gc`](Self::gc) frees space.
    pub fn open(root: impl Into<PathBuf>, config: StoreConfig) -> Result<Self, StoreError> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(io_error("create", &root))?;
        let lock_path = root.join(LOCK_FILE);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(io_error("open", &lock_path))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(StoreError::Locked(root)),
            Err(TryLockError::Error(error)) => return Err(io_error("lock", &lock_path)(error)),
        }
        for dir in [CHUNKS_DIR, PENDING_DIR, TEMP_DIR] {
            let path = root.join(dir);
            fs::create_dir_all(&path).map_err(io_error("create", &path))?;
        }
        let store = Self {
            inner: Arc::new(Inner {
                root,
                config,
                accounting: Mutex::new(Accounting {
                    used: 0,
                    fans: [false; 256],
                }),
                gc: RwLock::new(()),
                next_temp: AtomicU64::new(0),
                _lock: lock,
            }),
        };
        store.clear_temp()?;
        let mut used = 0;
        store.scan(|_, _, len| {
            used += len;
            Ok(())
        })?;
        store.accounting().used = used;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    pub fn config(&self) -> &StoreConfig {
        &self.inner.config
    }

    /// Bytes of chunk files currently stored.
    pub fn used_bytes(&self) -> u64 {
        self.accounting().used
    }

    /// Whether a chunk file exists. Its contents are checked when it is read.
    pub fn has(&self, id: &ChunkId) -> Result<bool, StoreError> {
        let path = self.chunk_path(id);
        match fs::metadata(&path) {
            Ok(metadata) => Ok(metadata.is_file()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io_error("inspect", &path)(error)),
        }
    }

    /// The distinct chunks of `manifest` the store lacks, in file order: what
    /// a receiver has to fetch.
    pub fn missing(&self, manifest: &Manifest) -> Result<Vec<ChunkEntry>, StoreError> {
        let mut missing = Vec::new();
        for entry in manifest.unique_chunks() {
            if !self.has(&entry.id)? {
                missing.push(entry);
            }
        }
        Ok(missing)
    }

    /// Chunks, hashes and stores a stream, compressing only chunks the store
    /// does not already hold, and returns its manifest.
    ///
    /// Memory use is one maximum-size chunk plus the manifest, whatever the
    /// size of the stream. If this fails partway, the chunks stored so far
    /// stay until the next [`gc`](Self::gc).
    pub fn ingest(&self, source: impl Read, params: ChunkParams) -> Result<Manifest, StoreError> {
        let _shared = self.shared();
        let mut chunker = Chunker::new(source, params);
        let mut compressor = codec::compressor(self.inner.config.compression_level)
            .map_err(StoreError::Compression)?;
        let mut chunks = Vec::new();
        let mut written_dirs = BTreeSet::new();
        while let Some(chunk) = chunker.next_chunk()? {
            let len = chunk_len(&chunk.data);
            if !self.has(&chunk.id)? {
                let frame = compressor
                    .compress(&chunk.data)
                    .map_err(StoreError::Compression)?;
                if self.write_chunk(&chunk.id, len, &frame)? {
                    written_dirs.insert(fan_out(&chunk.id));
                }
            }
            chunks.push(ChunkEntry {
                id: chunk.id,
                offset: chunk.offset,
                len,
            });
        }
        let (total_size, file_hash) = chunker.finish()?;
        let manifest = Manifest::from_chunks(params, total_size, file_hash, chunks)?;
        if self.inner.config.sync_chunks && !written_dirs.is_empty() {
            let chunks_dir = self.inner.root.join(CHUNKS_DIR);
            for fan in written_dirs {
                sync_dir(&chunks_dir.join(fan));
            }
            // Fan-out directories may be new too.
            sync_dir(&chunks_dir);
        }
        Ok(manifest)
    }

    /// Reads a chunk and returns its uncompressed bytes, after checking them
    /// against `id`. A damaged chunk is removed and reported as
    /// [`StoreError::Corrupt`].
    pub fn read(&self, id: &ChunkId) -> Result<Vec<u8>, StoreError> {
        let mut decompressor = codec::decompressor().map_err(StoreError::Compression)?;
        Ok(self.load(&mut decompressor, id)?.raw)
    }

    /// Reads a chunk's zstd frame, the form it travels in, after checking that
    /// it decompresses to the bytes `id` names. A damaged chunk is removed and
    /// reported as [`StoreError::Corrupt`].
    pub fn read_compressed(&self, id: &ChunkId) -> Result<Vec<u8>, StoreError> {
        let mut decompressor = codec::decompressor().map_err(StoreError::Compression)?;
        let mut file = self.load(&mut decompressor, id)?.file;
        file.drain(..CHUNK_HEADER_LEN);
        Ok(file)
    }

    /// Writes the file `manifest` describes to `dest` from stored chunks alone.
    ///
    /// Nothing is written unless every chunk is present. The file is built
    /// next to `dest` under a temporary name, each chunk checked as it is
    /// read and the whole file against the manifest's hash, then flushed and
    /// renamed over `dest`. So `dest` only ever holds a complete, verified
    /// file. A damaged chunk is removed, which [`missing`](Self::missing)
    /// then reports.
    pub fn assemble(&self, manifest: &Manifest, dest: &Path) -> Result<(), StoreError> {
        let _shared = self.shared();
        let missing = self.missing(manifest)?;
        if !missing.is_empty() {
            return Err(StoreError::Incomplete {
                missing: missing.len(),
            });
        }
        let partial = partial_path(dest)?;
        let published = self
            .write_file(manifest, &partial)
            .and_then(|()| fs::rename(&partial, dest).map_err(io_error("publish", dest)));
        if let Err(error) = published {
            // Best effort: a leftover partial file is overwritten next time.
            let _ = fs::remove_file(&partial);
            return Err(error);
        }
        sync_dir(parent_dir(dest));
        Ok(())
    }

    /// Deletes every chunk that neither a manifest in `live` nor an unfinished
    /// transfer references, and any leftover temporary files.
    ///
    /// Unfinished transfers (see [`ChunkSink`](crate::ChunkSink)) are always
    /// live, so collecting never undoes a download in progress. Ingests,
    /// transfers and assemblies wait while it runs.
    pub fn gc<'a>(
        &self,
        live: impl IntoIterator<Item = &'a Manifest>,
    ) -> Result<GcStats, StoreError> {
        let _exclusive = self.exclusive();
        let mut stats = GcStats::default();
        let mut keep = HashSet::new();
        for manifest in live {
            keep.extend(manifest.chunks().iter().map(|entry| entry.id));
        }
        for id in self.pending()? {
            match self.load_pending(&id) {
                Ok(manifest) => keep.extend(manifest.chunks().iter().map(|entry| entry.id)),
                Err(StoreError::DamagedState { .. }) => {
                    self.remove_pending(&id)?;
                    stats.dropped_pending += 1;
                }
                Err(StoreError::NoState(_)) => {}
                Err(error) => return Err(error),
            }
        }
        let mut accounting = self.accounting();
        self.scan(|id, path, len| {
            if keep.contains(&id) {
                stats.kept_chunks += 1;
                stats.kept_bytes += len;
            } else {
                fs::remove_file(path).map_err(io_error("remove", path))?;
                stats.removed_chunks += 1;
                stats.freed_bytes += len;
            }
            Ok(())
        })?;
        // Recount from what is on disk, so accounting cannot drift.
        accounting.used = stats.kept_bytes;
        drop(accounting);
        self.clear_temp()?;
        Ok(stats)
    }

    /// The snapshots of unfinished transfers, which
    /// [`ChunkSink::resume`](crate::ChunkSink::resume) continues.
    pub fn pending(&self) -> Result<Vec<ManifestId>, StoreError> {
        let dir = self.inner.root.join(PENDING_DIR);
        let mut ids = Vec::new();
        for entry in fs::read_dir(&dir).map_err(io_error("list", &dir))? {
            let entry = entry.map_err(io_error("list", &dir))?;
            if let Some(id) = entry.file_name().to_str().and_then(ManifestId::from_hex) {
                ids.push(id);
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// Stores a zstd frame that decompresses to the `raw_len` bytes `id` names.
    /// The caller has checked that. Returns whether the chunk was new.
    pub(crate) fn insert_frame(
        &self,
        id: &ChunkId,
        raw_len: u32,
        frame: &[u8],
    ) -> Result<bool, StoreError> {
        let _shared = self.shared();
        self.write_chunk(id, raw_len, frame)
    }

    /// Records a transfer as unfinished, durably, so its chunks survive
    /// garbage collection and it can resume after a restart.
    pub(crate) fn save_pending(&self, manifest: &Manifest) -> Result<(), StoreError> {
        let _shared = self.shared();
        let dir = self.inner.root.join(PENDING_DIR);
        let dest = dir.join(manifest.id().to_string());
        let (temp, mut file) = self.temp_file()?;
        let written = file
            .write_all(&manifest.to_bytes())
            .and_then(|()| file.sync_all());
        drop(file);
        if let Err(source) = written {
            let _ = fs::remove_file(&temp);
            return Err(io_error("write", &temp)(source));
        }
        if let Err(source) = fs::rename(&temp, &dest) {
            let _ = fs::remove_file(&temp);
            return Err(io_error("save", &dest)(source));
        }
        sync_dir(&dir);
        Ok(())
    }

    pub(crate) fn load_pending(&self, id: &ManifestId) -> Result<Manifest, StoreError> {
        let path = self.inner.root.join(PENDING_DIR).join(id.to_string());
        let damaged = |reason| StoreError::DamagedState { id: *id, reason };
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(StoreError::NoState(*id));
            }
            Err(error) => return Err(io_error("open", &path)(error)),
        };
        let limit = MAX_MANIFEST_LEN as u64;
        let mut bytes = Vec::new();
        file.take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error("read", &path))?;
        if bytes.len() as u64 > limit {
            return Err(damaged(StateDamage::TooLong(bytes.len() as u64)));
        }
        let manifest =
            Manifest::from_bytes(&bytes).map_err(|error| damaged(StateDamage::Manifest(error)))?;
        if manifest.id() != *id {
            return Err(damaged(StateDamage::OtherSnapshot(manifest.id())));
        }
        Ok(manifest)
    }

    pub(crate) fn remove_pending(&self, id: &ManifestId) -> Result<(), StoreError> {
        let path = self.inner.root.join(PENDING_DIR).join(id.to_string());
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error("remove", &path)(error)),
        }
    }

    fn accounting(&self) -> MutexGuard<'_, Accounting> {
        self.inner
            .accounting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn shared(&self) -> RwLockReadGuard<'_, ()> {
        self.inner.gc.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn exclusive(&self) -> RwLockWriteGuard<'_, ()> {
        self.inner
            .gc
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn chunk_path(&self, id: &ChunkId) -> PathBuf {
        let hex = id.to_string();
        self.inner.root.join(CHUNKS_DIR).join(&hex[..2]).join(hex)
    }

    /// Creates a new file in `tmp/`. The caller holds the shared lock, so
    /// garbage collection cannot delete it midway.
    fn temp_file(&self) -> Result<(PathBuf, File), StoreError> {
        let dir = self.inner.root.join(TEMP_DIR);
        loop {
            let number = self.inner.next_temp.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!("{number:016x}"));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((path, file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(io_error("create", &path)(error)),
            }
        }
    }

    /// Writes a chunk file and moves it into place. The caller holds the
    /// shared lock. Returns whether the chunk was new.
    fn write_chunk(&self, id: &ChunkId, raw_len: u32, frame: &[u8]) -> Result<bool, StoreError> {
        let (temp, mut file) = self.temp_file()?;
        let mut header = [0; CHUNK_HEADER_LEN];
        header[..4].copy_from_slice(&CHUNK_MAGIC);
        header[4..].copy_from_slice(&raw_len.to_le_bytes());
        let written = file.write_all(&header).and_then(|()| file.write_all(frame));
        let synced = written.and_then(|()| {
            if self.inner.config.sync_chunks {
                file.sync_data()
            } else {
                Ok(())
            }
        });
        drop(file);
        if let Err(source) = synced {
            let _ = fs::remove_file(&temp);
            return Err(io_error("write", &temp)(source));
        }
        let size = (CHUNK_HEADER_LEN + frame.len()) as u64;
        let committed = self.commit(&temp, id, size);
        if !matches!(committed, Ok(true)) {
            let _ = fs::remove_file(&temp);
        }
        committed
    }

    /// Renames a finished chunk file into place unless the chunk is already
    /// stored, keeping the store within its bound.
    fn commit(&self, temp: &Path, id: &ChunkId, size: u64) -> Result<bool, StoreError> {
        let dest = self.chunk_path(id);
        let mut accounting = self.accounting();
        match fs::metadata(&dest) {
            // As in `has`: only a file counts. Anything else in the way makes
            // the rename below fail with an error.
            Ok(metadata) if metadata.is_file() => return Ok(false),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("inspect", &dest)(error)),
        }
        let limit = self.inner.config.max_bytes;
        if accounting.used.saturating_add(size) > limit {
            return Err(StoreError::Full {
                used: accounting.used,
                limit,
                needed: size,
            });
        }
        let fan = usize::from(id.0[0]);
        if !accounting.fans[fan] {
            let dir = self.inner.root.join(CHUNKS_DIR).join(fan_out(id));
            fs::create_dir_all(&dir).map_err(io_error("create", &dir))?;
            accounting.fans[fan] = true;
        }
        fs::rename(temp, &dest).map_err(io_error("store", &dest))?;
        accounting.used += size;
        Ok(true)
    }

    /// Reads and verifies a chunk file, removing it if it is damaged.
    fn load(
        &self,
        decompressor: &mut Decompressor<'_>,
        id: &ChunkId,
    ) -> Result<Loaded, StoreError> {
        let path = self.chunk_path(id);
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound(*id));
            }
            Err(error) => return Err(io_error("open", &path)(error)),
        };
        let mut bytes = Vec::new();
        file.take(MAX_CHUNK_FILE_LEN + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error("read", &path))?;
        let verified = parse_chunk_file(&bytes)
            .and_then(|(raw_len, frame)| codec::decode(decompressor, id, raw_len, frame));
        match verified {
            Ok(raw) => Ok(Loaded { raw, file: bytes }),
            Err(reason) => {
                self.discard(&path, bytes.len() as u64)?;
                Err(StoreError::Corrupt { id: *id, reason })
            }
        }
    }

    fn discard(&self, path: &Path, len: u64) -> Result<(), StoreError> {
        let mut accounting = self.accounting();
        match fs::remove_file(path) {
            Ok(()) => {
                accounting.used = accounting.used.saturating_sub(len);
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error("remove damaged chunk", path)(error)),
        }
    }

    fn write_file(&self, manifest: &Manifest, partial: &Path) -> Result<(), StoreError> {
        let mut file = File::create(partial).map_err(io_error("create", partial))?;
        let mut decompressor = codec::decompressor().map_err(StoreError::Compression)?;
        let mut hasher = blake3::Hasher::new();
        for entry in manifest.chunks() {
            let raw = self.load(&mut decompressor, &entry.id)?.raw;
            if raw.len() != entry.len as usize {
                return Err(StoreError::LengthMismatch {
                    id: entry.id,
                    expected: entry.len,
                    actual: raw.len(),
                });
            }
            hasher.update(&raw);
            file.write_all(&raw).map_err(io_error("write", partial))?;
        }
        if *hasher.finalize().as_bytes() != manifest.file_hash().0 {
            return Err(StoreError::FileHashMismatch);
        }
        file.sync_all().map_err(io_error("flush", partial))
    }

    /// Visits every chunk file: its id, path and length. Files whose names
    /// are not chunk ids in the right directory are not the store's and are
    /// left alone.
    fn scan(
        &self,
        mut visit: impl FnMut(ChunkId, &Path, u64) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let chunks = self.inner.root.join(CHUNKS_DIR);
        for fan in fs::read_dir(&chunks).map_err(io_error("list", &chunks))? {
            let fan = fan.map_err(io_error("list", &chunks))?;
            let dir = fan.path();
            let prefix = fan.file_name();
            let Some(prefix) = prefix.to_str().filter(|name| is_fan_name(name)) else {
                continue;
            };
            if !fan.file_type().map_err(io_error("inspect", &dir))?.is_dir() {
                continue;
            }
            for entry in fs::read_dir(&dir).map_err(io_error("list", &dir))? {
                let entry = entry.map_err(io_error("list", &dir))?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                let Some(id) = ChunkId::from_hex(name).filter(|_| name.starts_with(prefix)) else {
                    continue;
                };
                let path = entry.path();
                let metadata = entry.metadata().map_err(io_error("inspect", &path))?;
                if metadata.is_file() {
                    visit(id, &path, metadata.len())?;
                }
            }
        }
        Ok(())
    }

    /// Deletes everything in `tmp/`. Only called while no write can be in
    /// flight: at open, and during garbage collection.
    fn clear_temp(&self) -> Result<(), StoreError> {
        let dir = self.inner.root.join(TEMP_DIR);
        for entry in fs::read_dir(&dir).map_err(io_error("list", &dir))? {
            let path = entry.map_err(io_error("list", &dir))?.path();
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error("remove", &path)(error)),
            }
        }
        Ok(())
    }
}

impl fmt::Debug for ChunkStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkStore")
            .field("root", &self.inner.root)
            .field("used_bytes", &self.used_bytes())
            .field("max_bytes", &self.inner.config.max_bytes)
            .finish_non_exhaustive()
    }
}

struct Loaded {
    raw: Vec<u8>,
    file: Vec<u8>,
}

fn fan_out(id: &ChunkId) -> String {
    format!("{:02x}", id.0[0])
}

fn is_fan_name(name: &str) -> bool {
    name.len() == 2
        && name
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn parse_chunk_file(bytes: &[u8]) -> Result<(u32, &[u8]), ChunkError> {
    if bytes.len() as u64 > MAX_CHUNK_FILE_LEN {
        return Err(ChunkError::BadFile("longer than any valid chunk"));
    }
    let Some((header, frame)) = bytes.split_first_chunk::<CHUNK_HEADER_LEN>() else {
        return Err(ChunkError::BadFile("truncated header"));
    };
    let (magic, raw_len) = header.split_at(4);
    if magic != CHUNK_MAGIC {
        return Err(ChunkError::BadFile("not a chunk file"));
    }
    let mut len = [0; 4];
    len.copy_from_slice(raw_len);
    let raw_len = u32::from_le_bytes(len);
    if raw_len == 0 || raw_len > crate::MAX_CHUNK_LEN {
        return Err(ChunkError::BadFile("impossible chunk length"));
    }
    Ok((raw_len, frame))
}

/// A hidden sibling of `dest` to build it in, so the final rename stays
/// within one directory and one filesystem.
fn partial_path(dest: &Path) -> Result<PathBuf, StoreError> {
    let Some(name) = dest.file_name() else {
        return Err(StoreError::InvalidDestination(dest.to_owned()));
    };
    let mut partial = OsString::from(".");
    partial.push(name);
    partial.push(".tpf3mp-partial");
    Ok(dest.with_file_name(partial))
}

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Makes renames into `dir` durable, as far as the platform allows. It is best
/// effort, as in SQLite: some file systems refuse to flush a directory (and on
/// macOS a flush is `F_FULLFSYNC`), while the rename it would make durable has
/// already happened and the file itself was flushed. Failing here would report
/// an error for work that succeeded.
#[cfg(unix)]
fn sync_dir(dir: &Path) {
    if let Ok(dir) = File::open(dir) {
        let _ = dir.sync_all();
    }
}

/// Windows offers no portable way to open a directory for flushing, and NTFS
/// journals metadata.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {}
