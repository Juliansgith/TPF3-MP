//! Moving worlds between this client and the server: fetching the snapshot
//! a turn stream starts from, and uploading a save the room asked for. See
//! "Snapshots" in `docs/PROTOCOL.md`.

use std::{
    fs,
    io::{self, BufReader},
    path::{Path, PathBuf},
    time::Duration,
};

use quinn::{RecvStream, SendStream};
use tpf3mp_net::{
    bulk::{self, BulkError, Completion, Served},
    read_preamble, write_message, write_preamble,
};
use tpf3mp_proto::{
    BULK_REQUEST_MAX_FRAME, BulkOpen, PROTOCOL_VERSION, SavedWorld, SnapshotId, WorldOffer,
};
use tpf3mp_snapshot::{ChunkParams, ChunkStore, Manifest, ManifestId, Progress, StoreConfig};

/// Silence after which a bulk stream is given up.
pub const BULK_IDLE: Duration = Duration::from_secs(60);

/// Where a player keeps worlds: every snapshot saved or received, and the
/// files the game writes and loads.
#[derive(Debug, Clone)]
pub struct Worlds {
    store: ChunkStore,
    /// Where the game writes its saves.
    saves: PathBuf,
    /// Where received worlds are put together for the game to load.
    received: PathBuf,
}

impl Worlds {
    /// Opens (or creates) the worlds kept under `dir`, taking at most
    /// `max_bytes` for snapshots.
    pub fn open(dir: &Path, max_bytes: u64) -> io::Result<Self> {
        let mut config = StoreConfig::new(max_bytes);
        // A damaged chunk after a power cut only costs fetching it again, and
        // flushing every chunk almost halves transfer speed on Windows.
        config.sync_chunks = false;
        let store = ChunkStore::open(dir.join("store"), config).map_err(io::Error::other)?;
        let saves = dir.join("saves");
        let received = dir.join("received");
        fs::create_dir_all(&saves)?;
        fs::create_dir_all(&received)?;
        Ok(Self {
            store,
            saves,
            received,
        })
    }

    pub fn store(&self) -> &ChunkStore {
        &self.store
    }

    /// Where the game writes its saves.
    pub fn saves(&self) -> &Path {
        &self.saves
    }

    /// Where the world of `snapshot` is put together for the game to load.
    pub fn received_file(&self, snapshot: &SnapshotId) -> PathBuf {
        self.received
            .join(format!("{}.sav", bulk::manifest_id(snapshot)))
    }

    /// Cuts a save the game wrote into the store, and deletes the file: the
    /// store holds it now. Blocking.
    pub fn ingest(&self, file: &Path) -> io::Result<(Manifest, SavedWorld)> {
        let source = BufReader::new(fs::File::open(file)?);
        let manifest = self
            .store
            .ingest(source, ChunkParams::DEFAULT)
            .map_err(io::Error::other)?;
        // The game may still hold the file open; the store has what it needs.
        let _ = fs::remove_file(file);
        let world = SavedWorld {
            snapshot: bulk::snapshot_id(&manifest.id()),
            size: manifest.total_size(),
        };
        Ok((manifest, world))
    }

    /// Stops keeping snapshots other than `keep`, deletes received world
    /// files other than theirs, and collects unused chunks. Blocking.
    pub fn keep_only(&self, keep: &[ManifestId]) -> io::Result<()> {
        for id in self.store.retained().map_err(io::Error::other)? {
            if !keep.contains(&id) {
                self.store.release(&id).map_err(io::Error::other)?;
            }
        }
        let wanted: Vec<String> = keep.iter().map(|id| format!("{id}.sav")).collect();
        for entry in fs::read_dir(&self.received)?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !wanted.contains(&name) {
                let _ = fs::remove_file(entry.path());
            }
        }
        self.store.gc([]).map_err(io::Error::other)?;
        Ok(())
    }
}

/// Opens bulk streams on a client's connection.
#[derive(Debug, Clone)]
pub struct BulkOpener {
    pub(crate) connection: quinn::Connection,
}

impl BulkOpener {
    /// Opens a bulk stream for `open`, exchanging version preambles first.
    pub async fn open(&self, open: BulkOpen) -> Result<(SendStream, RecvStream), BulkError> {
        let (mut send, mut recv) = self.connection.open_bi().await?;
        write_preamble(&mut send, PROTOCOL_VERSION).await?;
        write_message(&mut send, &open, BULK_REQUEST_MAX_FRAME).await?;
        if read_preamble(&mut recv).await? != PROTOCOL_VERSION {
            return Err(BulkError::Violation("a bulk stream of another version"));
        }
        Ok((send, recv))
    }
}

/// Fetches the world a turn stream offered into `worlds`, and puts its file
/// together. Returns the file and the snapshot's manifest.
pub async fn fetch_world(
    opener: &BulkOpener,
    worlds: &Worlds,
    offer: WorldOffer,
    progress: impl FnMut(Progress),
) -> Result<(PathBuf, Manifest), BulkError> {
    let file = worlds.received_file(&offer.snapshot);
    let (mut send, mut recv) = opener
        .open(BulkOpen::Fetch {
            snapshot: offer.snapshot,
        })
        .await?;
    let manifest = bulk::fetch(
        &mut send,
        &mut recv,
        &worlds.store,
        &bulk::manifest_id(&offer.snapshot),
        Completion::File(file.clone()),
        BULK_IDLE,
        progress,
    )
    .await?;
    Ok((file, manifest))
}

/// Uploads a save this client made, which the room asked for.
pub async fn upload_world(
    opener: &BulkOpener,
    worlds: &Worlds,
    snapshot: SnapshotId,
) -> Result<Served, BulkError> {
    let store = worlds.store.clone();
    let id = bulk::manifest_id(&snapshot);
    let manifest = tokio::task::spawn_blocking(move || store.manifest(&id))
        .await
        .map_err(|error| BulkError::Task(error.to_string()))??;
    let (mut send, mut recv) = opener.open(BulkOpen::Serve { snapshot }).await?;
    let served = bulk::serve(&mut send, &mut recv, &worlds.store, &manifest, BULK_IDLE).await;
    let _ = send.finish();
    served
}
