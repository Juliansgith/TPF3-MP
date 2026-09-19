//! World snapshots on the server: the chunk store, a running game's saves,
//! which save its room agrees on, and where that save stands in the log.
//! The room drives them (see `room.rs`); "Snapshots" in `docs/PROTOCOL.md`
//! describes the flow.
//!
//! A save starts as a `Save` event sealed alone at the end of its turn.
//! Every client that plays through it saves its world and reports the
//! world's lanes and the snapshot it made. Once the players pacing the room
//! have reported, or the deadline passes, the room judges the lanes like a
//! checkpoint's. A player whose lanes agree uploads its snapshot. That
//! snapshot is then what players who need a world receive.

use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tpf3mp_net::bulk;
use tpf3mp_proto::{PlayerId, RoomId, SavedWorld, SnapshotId, WorldOffer};
use tpf3mp_snapshot::{ChunkStore, Manifest, ManifestId, StoreConfig, StoreError};
use tracing::warn;

use crate::verdict::{self, Report};

/// How long a save round waits for every player pacing the room before it
/// decides with the reports it has. Saving a large world and cutting it
/// into chunks takes a while on a slow disk.
pub(crate) const SAVE_DEADLINE: Duration = Duration::from_secs(120);
/// How long a player asked to upload has to start.
pub(crate) const UPLOAD_START: Duration = Duration::from_secs(30);
/// Silence after which a bulk stream is given up.
pub(crate) const BULK_IDLE: Duration = Duration::from_secs(60);
/// Bulk streams the server runs at once, so snapshots cannot take all of
/// its disk and bandwidth.
const TRANSFERS: usize = 32;
/// How often the server deletes chunks no snapshot uses any more.
/// Collecting walks the whole store and holds it meanwhile, so it runs on
/// a timer and only after something was released, not at every release.
pub(crate) const COLLECT_EVERY: Duration = Duration::from_secs(600);
/// Version of the snapshot pointer file.
const POINTER_VERSION: u16 = 1;
/// Largest pointer file read back.
const POINTER_MAX: u64 = 4096;

/// How a server keeps world snapshots: for players who join a running game,
/// return too late to resume, or diverge. See "Snapshots" in
/// `docs/PROTOCOL.md`.
#[derive(Debug, Clone)]
pub struct SnapshotConfig {
    /// The chunk store's directory.
    pub dir: PathBuf,
    /// Bytes the store may hold. A save that does not fit is not kept.
    pub max_bytes: u64,
    /// How often a running game saves.
    pub every: Duration,
    /// The least time between two saves of one game, however many players
    /// are waiting for a world.
    pub min_gap: Duration,
}

impl SnapshotConfig {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            max_bytes: 64 << 30,
            every: Duration::from_secs(600),
            min_gap: Duration::from_secs(60),
        }
    }
}

/// The server's snapshots, shared by every room.
pub(crate) struct Snapshots {
    pub(crate) store: ChunkStore,
    pub(crate) every: Duration,
    pub(crate) min_gap: Duration,
    /// Bulk streams running at once, across the server.
    pub(crate) transfers: Arc<Semaphore>,
    /// Whether a snapshot was released since the last collection.
    released: AtomicBool,
}

impl Snapshots {
    pub(crate) fn open(config: &SnapshotConfig) -> Result<Self, StoreError> {
        let mut store_config = StoreConfig::new(config.max_bytes);
        // Saves arrive from players: compress them ourselves rather than
        // pass on whatever frames an uploader chose.
        store_config.recompress_received = true;
        Ok(Self {
            store: ChunkStore::open(&config.dir, store_config)?,
            every: config.every,
            min_gap: config.min_gap.min(config.every),
            transfers: Arc::new(Semaphore::new(TRANSFERS)),
            released: AtomicBool::new(false),
        })
    }

    /// Stops keeping these snapshots. Their chunks go at the next
    /// collection. Blocking.
    pub(crate) fn release(&self, ids: &[ManifestId]) {
        for id in ids {
            if let Err(error) = self.store.release(id) {
                warn!(%error, "cannot release a snapshot");
            }
        }
        if !ids.is_empty() {
            self.released.store(true, Ordering::Relaxed);
        }
    }

    /// Collects the chunks of released snapshots, if any were released
    /// since the last collection. Blocking.
    pub(crate) fn collect_released(&self) {
        if self.released.swap(false, Ordering::Relaxed) {
            self.collect();
        }
    }

    /// Stops keeping snapshots no room refers to, for example those of
    /// rooms that closed while the server was down, and collects their
    /// chunks.
    pub(crate) fn release_all_but(&self, keep: &[ManifestId]) {
        match self.store.retained() {
            Ok(retained) => {
                for id in retained.iter().filter(|id| !keep.contains(id)) {
                    if let Err(error) = self.store.release(id) {
                        warn!(%error, "cannot release a snapshot no room uses");
                    }
                }
            }
            Err(error) => warn!(%error, "cannot list the snapshot store"),
        }
        self.collect();
    }

    /// Deletes chunks no retained snapshot uses.
    pub(crate) fn collect(&self) {
        if let Err(error) = self.store.gc([]) {
            warn!(%error, "cannot collect unused snapshot chunks");
        }
    }
}

/// Where a save stands in its game's log. The world it holds has run every
/// step up to `sealed_through` and applied every event up to and including
/// the save event `event`, the last event of turn `after_turn`. A stream
/// from the save starts right after that turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SavePoint {
    pub(crate) event: u64,
    pub(crate) after_turn: u64,
    pub(crate) history: u64,
    pub(crate) sealed_through: u64,
}

/// A snapshot the room agreed on and the server holds.
#[derive(Debug, Clone)]
pub(crate) struct Agreed {
    pub(crate) manifest: Arc<Manifest>,
    pub(crate) point: SavePoint,
}

impl Agreed {
    pub(crate) fn id(&self) -> SnapshotId {
        bulk::snapshot_id(&self.manifest.id())
    }

    pub(crate) fn offer(&self) -> WorldOffer {
        WorldOffer {
            snapshot: self.id(),
            size: self.manifest.total_size(),
        }
    }
}

/// The reports of one save.
#[derive(Debug)]
pub(crate) struct SaveRound {
    pub(crate) point: SavePoint,
    pub(crate) opened: Instant,
    pub(crate) reports: Vec<SaveReport>,
}

#[derive(Debug, Clone)]
pub(crate) struct SaveReport {
    pub(crate) report: Report,
    pub(crate) world: Option<SavedWorld>,
}

/// Players whose saves agreed, best first, with the save each holds.
pub(crate) type Candidates = VecDeque<(PlayerId, SavedWorld)>;
/// A player whose world differs from the verdict, in these lanes.
pub(crate) type Divergence = (PlayerId, Vec<u16>);

/// A save being fetched from a player whose lanes agreed.
#[derive(Debug)]
pub(crate) struct Upload {
    pub(crate) point: SavePoint,
    pub(crate) from: PlayerId,
    pub(crate) world: SavedWorld,
    /// Other players whose saves agreed, to ask if this one fails.
    pub(crate) rest: Candidates,
    pub(crate) asked: Instant,
    /// Whether the player opened its stream.
    pub(crate) receiving: bool,
}

/// A running game's saves.
#[derive(Debug, Default)]
pub(crate) struct Saves {
    /// Rounds not decided yet, by save event.
    pub(crate) rounds: BTreeMap<u64, SaveRound>,
    pub(crate) upload: Option<Upload>,
    /// The snapshot players who need a world receive.
    pub(crate) current: Option<Agreed>,
    /// The one before, kept while downloads of it may still run.
    pub(crate) previous: Option<Agreed>,
    /// When the last save was sealed.
    pub(crate) last_save: Option<Instant>,
    /// Where the last save was sealed, to skip saving a world that has not
    /// changed.
    pub(crate) last_point: Option<SavePoint>,
    /// Someone needs a world the current snapshot cannot give.
    pub(crate) wanted: bool,
}

impl Saves {
    /// Whether the game, which started (or was restored) at `started`,
    /// should save now. `sealed_through` and `next_event` describe its log
    /// as it stands.
    pub(crate) fn due(
        &self,
        now: Instant,
        every: Duration,
        min_gap: Duration,
        started: Instant,
        (sealed_through, next_event): (u64, u64),
    ) -> bool {
        if !self.rounds.is_empty() || self.upload.is_some() {
            return false;
        }
        let changed = self.current.is_none()
            || self.last_point.is_none_or(|point| {
                sealed_through > point.sealed_through || next_event > point.event + 1
            });
        if !changed {
            return false;
        }
        let last = self.last_save.unwrap_or(started);
        let periodic = now.saturating_duration_since(last) >= every;
        // Someone waiting for a world gets one soon, but no player can make
        // the room save more often than every `min_gap`.
        let needed = self.wanted
            && self
                .last_save
                .is_none_or(|last| now.saturating_duration_since(last) >= min_gap);
        periodic || needed
    }

    /// The ids of the snapshots this game holds, newest first.
    pub(crate) fn held(&self) -> Vec<ManifestId> {
        [&self.current, &self.previous]
            .into_iter()
            .flatten()
            .map(|agreed| agreed.manifest.id())
            .collect()
    }

    /// The agreed snapshot with this id, if the game holds it.
    pub(crate) fn find(&self, snapshot: &SnapshotId) -> Option<&Agreed> {
        [&self.current, &self.previous]
            .into_iter()
            .flatten()
            .find(|agreed| agreed.id() == *snapshot)
    }
}

/// Decides a save round: which saves may be uploaded, best first, and who
/// diverged. With a single report there is nothing to compare, and the one
/// save stands; that is how a player alone in a room hands its world on.
pub(crate) fn decide(reports: &[SaveReport]) -> (Candidates, Vec<Divergence>) {
    let judged: Vec<Report> = reports.iter().map(|save| save.report.clone()).collect();
    let diverged = if judged.len() >= 2 {
        verdict::decide(&judged).1
    } else {
        Vec::new()
    };
    let mut agreeing: Vec<&SaveReport> = reports
        .iter()
        .filter(|save| {
            !diverged
                .iter()
                .any(|(player, _)| *player == save.report.player)
        })
        .collect();
    agreeing.sort_by_key(|save| save.report.order);
    let candidates = agreeing
        .into_iter()
        .filter_map(|save| save.world.map(|world| (save.report.player, world)))
        .collect();
    (candidates, diverged)
}

/// What survives a restart of a room's current snapshot: the file next to
/// its log that says which snapshot it is and where it stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Pointer {
    version: u16,
    pub(crate) snapshot: SnapshotId,
    pub(crate) point: SavePoint,
}

impl Pointer {
    pub(crate) fn new(snapshot: SnapshotId, point: SavePoint) -> Self {
        Self {
            version: POINTER_VERSION,
            snapshot,
            point,
        }
    }

    pub(crate) fn path_for(dir: &Path, room: &RoomId) -> PathBuf {
        dir.join(format!("{room}.snapshot"))
    }

    /// Writes the pointer so that a crash leaves either the old one or the
    /// new one, never a mix.
    pub(crate) fn write(&self, dir: &Path, room: &RoomId) -> io::Result<()> {
        let path = Self::path_for(dir, room);
        let partial = path.with_extension("snapshot.partial");
        let bytes = postcard::to_stdvec(self).map_err(io::Error::other)?;
        {
            let mut file = File::create(&partial)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        fs::rename(&partial, &path)
    }

    /// Reads a room's pointer, if it has a readable one.
    pub(crate) fn read(dir: &Path, room: &RoomId) -> Option<Self> {
        let path = Self::path_for(dir, room);
        let metadata = fs::symlink_metadata(&path).ok()?;
        if !metadata.file_type().is_file() || metadata.len() > POINTER_MAX {
            return None;
        }
        let mut bytes = Vec::new();
        File::open(&path).ok()?.read_to_end(&mut bytes).ok()?;
        let pointer: Self = postcard::from_bytes(&bytes).ok()?;
        (pointer.version == POINTER_VERSION).then_some(pointer)
    }

    pub(crate) fn remove(dir: &Path, room: &RoomId) {
        let path = Self::path_for(dir, room);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => warn!(path = %path.display(), %error, "cannot remove a snapshot pointer"),
        }
    }
}

#[cfg(test)]
mod tests {
    use tpf3mp_proto::{Arch, FixedBytes, LaneDigest, Os, Platform};

    use super::*;

    const PC: Platform = Platform {
        os: Os::Windows,
        arch: Arch::X86_64,
    };

    fn save(player: u8, lane: u8, world: Option<u8>) -> SaveReport {
        SaveReport {
            report: Report {
                player: PlayerId(FixedBytes([player; 32])),
                platform: PC,
                order: usize::from(player),
                lanes: vec![LaneDigest {
                    lane: 0,
                    digest: FixedBytes([lane; 32]),
                }],
            },
            world: world.map(|id| SavedWorld {
                snapshot: SnapshotId(FixedBytes([id; 32])),
                size: 100,
            }),
        }
    }

    fn point() -> SavePoint {
        SavePoint {
            event: 10,
            after_turn: 5,
            history: 1,
            sealed_through: 20,
        }
    }

    #[test]
    fn agreeing_saves_upload_in_join_order_and_the_odd_one_out_diverges() {
        let reports = [
            save(3, 1, Some(3)),
            save(1, 1, Some(1)),
            save(2, 9, Some(2)),
        ];
        let (candidates, diverged) = decide(&reports);
        let order: Vec<u8> = candidates.iter().map(|(player, _)| player.0.0[0]).collect();
        assert_eq!(order, [1, 3]);
        assert_eq!(diverged.len(), 1);
        assert_eq!(diverged[0].0, PlayerId(FixedBytes([2; 32])));
    }

    #[test]
    fn a_failed_save_is_no_candidate_and_a_lone_save_stands() {
        let (candidates, diverged) = decide(&[save(1, 1, None), save(2, 1, Some(2))]);
        assert_eq!(candidates.len(), 1);
        assert!(diverged.is_empty());
        let (candidates, _) = decide(&[save(1, 7, Some(1))]);
        assert_eq!(candidates.len(), 1, "one player alone hands its world on");
    }

    #[test]
    fn saves_are_due_by_interval_or_need_and_only_when_something_changed() {
        let every = Duration::from_secs(600);
        let gap = Duration::from_secs(60);
        let start = Instant::now();
        let mut saves = Saves::default();
        let due = |saves: &Saves, at: Duration, log| saves.due(start + at, every, gap, start, log);
        assert!(
            !due(&saves, Duration::ZERO, (0, 1)),
            "no need, no interval yet"
        );
        assert!(due(&saves, every, (0, 1)), "the first interval passed");
        saves.wanted = true;
        assert!(
            due(&saves, Duration::ZERO, (0, 1)),
            "someone needs a world at once"
        );
        saves.last_save = Some(start);
        saves.last_point = Some(point());
        saves.current = Some(Agreed {
            manifest: Arc::new(
                Manifest::compute(&[1u8; 10][..], tpf3mp_snapshot::ChunkParams::DEFAULT).unwrap(),
            ),
            point: point(),
        });
        let soon = Duration::from_secs(30);
        let later = Duration::from_secs(61);
        assert!(!due(&saves, soon, (30, 11)), "within the gap");
        assert!(due(&saves, later, (30, 11)), "past the gap");
        assert!(
            !due(&saves, later, (20, 11)),
            "nothing happened since the last save"
        );
        assert!(due(&saves, later, (20, 12)), "an event happened since");
        saves.wanted = false;
        assert!(!due(&saves, later, (30, 11)));
        assert!(due(&saves, every, (30, 11)), "the interval passed");
        saves.upload = Some(Upload {
            point: point(),
            from: PlayerId(FixedBytes([1; 32])),
            world: SavedWorld {
                snapshot: SnapshotId(FixedBytes([1; 32])),
                size: 1,
            },
            rest: VecDeque::new(),
            asked: start,
            receiving: false,
        });
        assert!(!due(&saves, every, (30, 11)), "one save at a time");
    }

    #[test]
    fn pointers_survive_a_round_trip_and_damage_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let room = RoomId(FixedBytes([4; 16]));
        let pointer = Pointer::new(SnapshotId(FixedBytes([8; 32])), point());
        pointer.write(dir.path(), &room).unwrap();
        assert_eq!(Pointer::read(dir.path(), &room), Some(pointer));
        fs::write(Pointer::path_for(dir.path(), &room), b"\xff\xff").unwrap();
        assert_eq!(Pointer::read(dir.path(), &room), None);
        Pointer::remove(dir.path(), &room);
        assert_eq!(Pointer::read(dir.path(), &room), None);
        Pointer::remove(dir.path(), &room);
    }
}
