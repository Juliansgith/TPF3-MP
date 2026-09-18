use std::{
    collections::{BTreeSet, HashSet},
    ffi::OsString,
    fmt,
    fs::{self, File, Metadata, OpenOptions, TryLockError},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
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
const ROOTS_DIR: &str = "roots";
const PENDING_DIR: &str = "pending";
const TEMP_DIR: &str = "tmp";
const PARTIAL_SUFFIX: &str = ".tpf3mp-partial";

#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// Upper bound on the bytes of chunk files the store keeps. A chunk that
    /// would exceed it is refused with [`StoreError::Full`]; [`ChunkStore::gc`]
    /// makes room.
    pub max_bytes: u64,
    /// zstd level for chunks the store compresses itself. Any level produces
    /// the same format, so it can change at any time.
    pub compression_level: i32,
    /// Flush every chunk to stable storage before it becomes visible, and the
    /// directories of an ingested or received snapshot before it counts as
    /// retained. Without it, a power failure can leave damaged chunks behind.
    /// Reads detect and remove those, so the cost is downloading them again,
    /// not a wrong file.
    pub sync_chunks: bool,
    /// Compress chunks received through a [`ChunkSink`](crate::ChunkSink)
    /// again, at `compression_level`, instead of storing the sender's frame.
    /// A peer can send a valid but poorly compressed frame; a store that
    /// serves chunks on to others (the server receiving a save from a replica)
    /// should not pass that on. It costs compression time on every received
    /// chunk.
    pub recompress_received: bool,
}

impl StoreConfig {
    pub const DEFAULT_COMPRESSION_LEVEL: i32 = 3;

    pub fn new(max_bytes: u64) -> Self {
        Self {
            max_bytes,
            compression_level: Self::DEFAULT_COMPRESSION_LEVEL,
            sync_chunks: true,
            recompress_received: false,
        }
    }
}

/// What [`ChunkStore::gc`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcStats {
    /// Chunks still stored afterwards, including any that could not be
    /// removed.
    pub kept_chunks: u64,
    pub kept_bytes: u64,
    pub removed_chunks: u64,
    pub freed_bytes: u64,
    /// Unreferenced chunks whose removal failed. They stay counted as kept, and
    /// the next collection tries again.
    pub failed_removals: u64,
    /// Saved manifests, retained or of unfinished transfers, that could not be
    /// read back and were dropped.
    pub damaged_records: u64,
}

/// A directory of compressed chunks, each stored once under its [`ChunkId`],
/// and of the manifests that keep them alive.
///
/// Layout:
///
/// - `chunks/<first two hex digits>/<64 hex digits>`: chunks;
/// - `roots/<manifest id>`: retained snapshots, whose chunks garbage
///   collection keeps until they are [released](Self::release);
/// - `pending/<manifest id>`: unfinished [`ChunkSink`] transfers;
/// - `tmp/`: files being written.
///
/// File names are only ever formatted from ids, never taken from peers, so no
/// input can name a path outside the store.
///
/// Every write goes to `tmp/` first and is renamed into place, so no reader
/// sees a partly written chunk, and puts of a chunk that is already stored do
/// nothing. Every read decompresses the chunk and checks it against its id; a
/// damaged chunk (bit rot, or a power failure without
/// [`sync_chunks`](StoreConfig::sync_chunks)) is removed so that it counts as
/// missing again. [`has`](Self::has) and [`ingest`](Self::ingest) trust that
/// a file present under an id holds that chunk, unless it is too short to be
/// one; damage surfaces on the next read.
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
    /// Snapshots with an open `ChunkSink` in this process: two sinks for one
    /// snapshot would unpin each other's chunks when one of them ends.
    open_sinks: Mutex<HashSet<ManifestId>>,
    /// Destinations being assembled in this process: two assemblies must not
    /// race to publish one file.
    assembling: Mutex<HashSet<PathBuf>>,
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

/// A saved manifest: a retained snapshot, or an unfinished transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Record {
    Root,
    Pending,
}

impl Record {
    fn dir(self) -> &'static str {
        match self {
            Self::Root => ROOTS_DIR,
            Self::Pending => PENDING_DIR,
        }
    }
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
    #[error("{} is already being assembled", .0.display())]
    Busy(PathBuf),
    #[error("snapshot {0} is neither retained nor being transferred")]
    UnknownSnapshot(ManifestId),
    #[error("the saved manifest of snapshot {id} is damaged: {reason}")]
    DamagedRecord {
        id: ManifestId,
        reason: RecordDamage,
    },
    #[error("zstd failed: {0}")]
    Compression(io::Error),
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
}

/// Why a saved manifest could not be used.
#[derive(Debug, Error)]
pub enum RecordDamage {
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

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
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
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(io_error("open", &lock_path))?;
        match lock_file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(StoreError::Locked(root)),
            Err(TryLockError::Error(error)) => return Err(io_error("lock", &lock_path)(error)),
        }
        for dir in [CHUNKS_DIR, ROOTS_DIR, PENDING_DIR, TEMP_DIR] {
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
                open_sinks: Mutex::new(HashSet::new()),
                assembling: Mutex::new(HashSet::new()),
                next_temp: AtomicU64::new(0),
                _lock: lock_file,
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

    /// Whether a chunk file exists and is long enough to hold a chunk. Its
    /// contents are checked when it is read.
    pub fn has(&self, id: &ChunkId) -> Result<bool, StoreError> {
        let path = self.chunk_path(id);
        match fs::metadata(&path) {
            Ok(metadata) => Ok(holds_chunk(&metadata)),
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
    /// does not already hold, and returns its manifest, which the store
    /// retains until it is [released](Self::release).
    ///
    /// The manifest is retained before this returns, so a garbage collection
    /// that runs meanwhile cannot take its chunks. Memory use is one
    /// maximum-size chunk plus the manifest, whatever the size of the stream.
    /// If this fails partway, the chunks stored so far stay until the next
    /// [`gc`](Self::gc).
    pub fn ingest(&self, source: impl Read, params: ChunkParams) -> Result<Manifest, StoreError> {
        let _shared = self.shared();
        let mut chunker = Chunker::new(source, params);
        let mut compressor = codec::compressor(self.inner.config.compression_level)
            .map_err(StoreError::Compression)?;
        let mut chunks = Vec::new();
        while let Some(chunk) = chunker.next_chunk()? {
            let len = chunk_len(&chunk.data);
            if !self.has(&chunk.id)? {
                let frame = compressor
                    .compress(&chunk.data)
                    .map_err(StoreError::Compression)?;
                self.write_chunk(&chunk.id, len, &frame)?;
            }
            chunks.push(ChunkEntry {
                id: chunk.id,
                offset: chunk.offset,
                len,
            });
        }
        let (total_size, file_hash) = chunker.finish()?;
        let manifest = Manifest::from_chunks(params, total_size, file_hash, chunks)?;
        self.sync_chunk_dirs(&manifest);
        self.write_record(Record::Root, &manifest)?;
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
    /// file. If a chunk turns out to be damaged, every damaged chunk of the
    /// manifest is removed, which [`missing`](Self::missing) then reports, so
    /// one more fetch round repairs them all.
    pub fn assemble(&self, manifest: &Manifest, dest: &Path) -> Result<(), StoreError> {
        self.assemble_then(manifest, dest, || Ok(()))
    }

    /// The snapshots the store retains: every [`ingest`](Self::ingest) and
    /// every finished transfer until [`release`](Self::release).
    pub fn retained(&self) -> Result<Vec<ManifestId>, StoreError> {
        self.list_records(Record::Root)
    }

    /// The snapshots of unfinished transfers, which
    /// [`ChunkSink::resume`](crate::ChunkSink::resume) continues.
    pub fn pending(&self) -> Result<Vec<ManifestId>, StoreError> {
        self.list_records(Record::Pending)
    }

    /// The manifest of a retained snapshot or an unfinished transfer, as saved
    /// by the store: after a restart, this is how the server finds its
    /// snapshots again.
    pub fn manifest(&self, id: &ManifestId) -> Result<Manifest, StoreError> {
        match self.load_record(Record::Root, id) {
            Err(StoreError::UnknownSnapshot(_)) => self.load_record(Record::Pending, id),
            result => result,
        }
    }

    /// Stops retaining a snapshot. Its chunks go at the next garbage
    /// collection unless something else still references them.
    pub fn release(&self, id: &ManifestId) -> Result<(), StoreError> {
        self.remove_record(Record::Root, id)
    }

    /// Deletes every chunk that no retained snapshot, no unfinished transfer
    /// and no manifest in `live` references, and any leftover temporary
    /// files.
    ///
    /// Snapshots stay retained from the moment [`ingest`](Self::ingest) or
    /// [`ChunkSink::finish`](crate::ChunkSink::finish) produces them until
    /// they are [released](Self::release), so a collection can never take the
    /// chunks of a snapshot whose manifest the caller has not recorded yet.
    /// `live` adds manifests the store does not retain itself.
    ///
    /// Ingests, transfers and assemblies wait while it runs, and it waits for
    /// those that are running.
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
        for record in [Record::Root, Record::Pending] {
            for id in self.list_records(record)? {
                match self.load_record(record, &id) {
                    Ok(manifest) => keep.extend(manifest.chunks().iter().map(|entry| entry.id)),
                    // It cannot be used again, and what it protected is unknown.
                    Err(StoreError::DamagedRecord { .. }) => {
                        self.remove_record(record, &id)?;
                        stats.damaged_records += 1;
                    }
                    Err(StoreError::UnknownSnapshot(_)) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        let mut accounting = self.accounting();
        self.scan(|id, path, len| {
            if !keep.contains(&id) {
                match fs::remove_file(path) {
                    Ok(()) => {
                        stats.removed_chunks += 1;
                        stats.freed_bytes += len;
                        return Ok(());
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                    // Still on disk, so still counted; the next collection
                    // tries again.
                    Err(_) => stats.failed_removals += 1,
                }
            }
            stats.kept_chunks += 1;
            stats.kept_bytes += len;
            Ok(())
        })?;
        // Recount from what is on disk, so accounting cannot drift.
        accounting.used = stats.kept_bytes;
        drop(accounting);
        self.clear_temp()?;
        Ok(stats)
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
        self.write_record(Record::Pending, manifest)
    }

    /// Assembles like [`assemble`](Self::assemble), and runs `before_publish`
    /// once the file is complete and verified but before it replaces `dest`.
    /// If `before_publish` fails, nothing is published.
    pub(crate) fn assemble_then(
        &self,
        manifest: &Manifest,
        dest: &Path,
        before_publish: impl FnOnce() -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let _shared = self.shared();
        let _claim = self.claim_destination(dest)?;
        let missing = self.missing(manifest)?;
        if !missing.is_empty() {
            return Err(StoreError::Incomplete {
                missing: missing.len(),
            });
        }
        remove_stale_partials(dest);
        let partial = self.partial_path(dest)?;
        let published = self
            .write_file(manifest, &partial)
            .and_then(|()| before_publish())
            .and_then(|()| fs::rename(&partial, dest).map_err(io_error("publish", dest)));
        if let Err(error) = published {
            let _ = fs::remove_file(&partial);
            return Err(error);
        }
        sync_dir(parent_dir(dest));
        Ok(())
    }

    /// Turns a finished transfer into a retained snapshot. The caller holds
    /// the shared lock.
    pub(crate) fn promote(&self, manifest: &Manifest) -> Result<(), StoreError> {
        self.sync_chunk_dirs(manifest);
        let name = manifest.id().to_string();
        let pending = self.inner.root.join(PENDING_DIR).join(&name);
        let root = self.inner.root.join(ROOTS_DIR).join(&name);
        match fs::rename(&pending, &root) {
            Ok(()) => {
                sync_dir(&self.inner.root.join(ROOTS_DIR));
                sync_dir(&self.inner.root.join(PENDING_DIR));
                Ok(())
            }
            // Promoted by an earlier attempt, or never recorded as pending.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.write_record(Record::Root, manifest)
            }
            Err(error) => Err(io_error("retain", &root)(error)),
        }
    }

    pub(crate) fn load_record(
        &self,
        record: Record,
        id: &ManifestId,
    ) -> Result<Manifest, StoreError> {
        let path = self.record_path(record, id);
        let damaged = |reason| StoreError::DamagedRecord { id: *id, reason };
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(StoreError::UnknownSnapshot(*id));
            }
            Err(error) => return Err(io_error("open", &path)(error)),
        };
        let limit = MAX_MANIFEST_LEN as u64;
        let mut bytes = Vec::new();
        file.take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error("read", &path))?;
        if bytes.len() as u64 > limit {
            return Err(damaged(RecordDamage::TooLong(bytes.len() as u64)));
        }
        let manifest =
            Manifest::from_bytes(&bytes).map_err(|error| damaged(RecordDamage::Manifest(error)))?;
        if manifest.id() != *id {
            return Err(damaged(RecordDamage::OtherSnapshot(manifest.id())));
        }
        Ok(manifest)
    }

    pub(crate) fn remove_record(&self, record: Record, id: &ManifestId) -> Result<(), StoreError> {
        let path = self.record_path(record, id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error("remove", &path)(error)),
        }
    }

    /// Claims a snapshot for one `ChunkSink` in this process; the claim ends
    /// when the returned value is dropped.
    pub(crate) fn claim_sink(&self, id: &ManifestId) -> Option<SinkClaim> {
        lock(&self.inner.open_sinks).insert(*id).then(|| SinkClaim {
            inner: Arc::clone(&self.inner),
            id: *id,
        })
    }

    fn accounting(&self) -> MutexGuard<'_, Accounting> {
        lock(&self.inner.accounting)
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

    fn record_path(&self, record: Record, id: &ManifestId) -> PathBuf {
        self.inner.root.join(record.dir()).join(id.to_string())
    }

    fn list_records(&self, record: Record) -> Result<Vec<ManifestId>, StoreError> {
        let dir = self.inner.root.join(record.dir());
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

    /// Saves a manifest durably under its id. The caller holds the shared
    /// lock.
    fn write_record(&self, record: Record, manifest: &Manifest) -> Result<(), StoreError> {
        let dest = self.record_path(record, &manifest.id());
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
        sync_dir(parent_dir(&dest));
        Ok(())
    }

    /// Flushes the directory entries of a snapshot's chunks, if the store
    /// flushes chunks at all.
    fn sync_chunk_dirs(&self, manifest: &Manifest) {
        if !self.inner.config.sync_chunks {
            return;
        }
        let chunks_dir = self.inner.root.join(CHUNKS_DIR);
        let fans: BTreeSet<String> = manifest
            .chunks()
            .iter()
            .map(|entry| fan_out(&entry.id))
            .collect();
        for fan in fans {
            sync_dir(&chunks_dir.join(fan));
        }
        // Fan-out directories may be new too.
        sync_dir(&chunks_dir);
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
        let replaced = match fs::metadata(&dest) {
            Ok(metadata) if holds_chunk(&metadata) => return Ok(false),
            // Too short to be a chunk, as a power failure can leave one: it
            // counts as missing and is replaced. Anything else in the way
            // makes the rename below fail.
            Ok(metadata) if metadata.is_file() => metadata.len(),
            Ok(_) => 0,
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => return Err(io_error("inspect", &dest)(error)),
        };
        let limit = self.inner.config.max_bytes;
        let used = accounting.used.saturating_sub(replaced);
        if used.saturating_add(size) > limit {
            return Err(StoreError::Full {
                used,
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
        accounting.used = used + size;
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
        let identity = FileIdentity::of(&file.metadata().map_err(io_error("inspect", &path))?);
        let mut bytes = Vec::new();
        file.take(MAX_CHUNK_FILE_LEN + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error("read", &path))?;
        let verified = parse_chunk_file(&bytes)
            .and_then(|(raw_len, frame)| codec::decode(decompressor, id, raw_len, frame));
        match verified {
            Ok(raw) => Ok(Loaded { raw, file: bytes }),
            Err(reason) => {
                self.discard(&path, identity)?;
                Err(StoreError::Corrupt { id: *id, reason })
            }
        }
    }

    /// Removes a damaged chunk file, unless it was replaced since it was read:
    /// a put may have stored a good copy in the meantime.
    fn discard(&self, path: &Path, identity: FileIdentity) -> Result<(), StoreError> {
        let mut accounting = self.accounting();
        match fs::metadata(path) {
            Ok(metadata) if FileIdentity::of(&metadata) == identity => {}
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_error("inspect", path)(error)),
        }
        match fs::remove_file(path) {
            Ok(()) => {
                accounting.used = accounting.used.saturating_sub(identity.len);
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error("remove damaged chunk", path)(error)),
        }
    }

    fn write_file(&self, manifest: &Manifest, partial: &Path) -> Result<(), StoreError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(partial)
            .map_err(io_error("create", partial))?;
        let mut decompressor = codec::decompressor().map_err(StoreError::Compression)?;
        let mut hasher = blake3::Hasher::new();
        for entry in manifest.chunks() {
            let raw = match self.load(&mut decompressor, &entry.id) {
                Ok(loaded) => loaded.raw,
                Err(error @ StoreError::Corrupt { .. }) => {
                    // Find every other damaged chunk now, so that one more
                    // fetch round repairs them all.
                    self.discard_damaged(&mut decompressor, manifest)?;
                    return Err(error);
                }
                Err(error) => return Err(error),
            };
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

    /// Reads every chunk of `manifest`, which removes the damaged ones.
    fn discard_damaged(
        &self,
        decompressor: &mut Decompressor<'_>,
        manifest: &Manifest,
    ) -> Result<(), StoreError> {
        for entry in manifest.unique_chunks() {
            match self.load(decompressor, &entry.id) {
                Ok(_) | Err(StoreError::Corrupt { .. } | StoreError::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Claims `dest` for one assembly in this process.
    fn claim_destination(&self, dest: &Path) -> Result<DestinationClaim<'_>, StoreError> {
        let key = std::path::absolute(dest).map_err(io_error("resolve", dest))?;
        if !lock(&self.inner.assembling).insert(key.clone()) {
            return Err(StoreError::Busy(dest.to_owned()));
        }
        Ok(DestinationClaim {
            assembling: &self.inner.assembling,
            key,
        })
    }

    /// A hidden, unique sibling of `dest` to build it in, so the final rename
    /// stays within one directory and one filesystem, and no two writers ever
    /// share a file.
    fn partial_path(&self, dest: &Path) -> Result<PathBuf, StoreError> {
        let Some(name) = dest.file_name() else {
            return Err(StoreError::InvalidDestination(dest.to_owned()));
        };
        let number = self.inner.next_temp.fetch_add(1, Ordering::Relaxed);
        let mut partial = OsString::from(".");
        partial.push(name);
        partial.push(format!(
            ".{:x}-{number:x}{PARTIAL_SUFFIX}",
            std::process::id()
        ));
        Ok(dest.with_file_name(partial))
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

    /// Deletes what it can in `tmp/`. Only called while no write can be in
    /// flight: at open, and during garbage collection.
    fn clear_temp(&self) -> Result<(), StoreError> {
        let dir = self.inner.root.join(TEMP_DIR);
        for entry in fs::read_dir(&dir).map_err(io_error("list", &dir))? {
            // A leftover that cannot be removed now is tried again next time.
            let _ = fs::remove_file(entry.map_err(io_error("list", &dir))?.path());
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

/// One `ChunkSink`'s hold on its snapshot.
pub(crate) struct SinkClaim {
    inner: Arc<Inner>,
    id: ManifestId,
}

impl Drop for SinkClaim {
    fn drop(&mut self) {
        lock(&self.inner.open_sinks).remove(&self.id);
    }
}

struct DestinationClaim<'a> {
    assembling: &'a Mutex<HashSet<PathBuf>>,
    key: PathBuf,
}

impl Drop for DestinationClaim<'_> {
    fn drop(&mut self) {
        lock(self.assembling).remove(&self.key);
    }
}

struct Loaded {
    raw: Vec<u8>,
    file: Vec<u8>,
}

/// Enough of a file's metadata to tell whether it was replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    len: u64,
    modified: Option<SystemTime>,
}

impl FileIdentity {
    fn of(metadata: &Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        }
    }
}

/// A chunk file holds at least its header and one byte of frame. Anything
/// shorter is what a power failure leaves behind, and counts as missing.
fn holds_chunk(metadata: &Metadata) -> bool {
    metadata.is_file() && metadata.len() > CHUNK_HEADER_LEN as u64
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

/// Removes partial files an interrupted assembly of `dest` left behind. Best
/// effort: they only waste space.
fn remove_stale_partials(dest: &Path) {
    let Some(name) = dest.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let prefix = format!(".{name}.");
    let Ok(entries) = fs::read_dir(parent_dir(dest)) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .file_name()
            .to_str()
            .is_some_and(|file| file.starts_with(&prefix) && file.ends_with(PARTIAL_SUFFIX));
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
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

/// Windows offers no portable way to open a directory for flushing. NTFS
/// journals the rename, which makes it atomic, and commits the journal
/// shortly after.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {}
