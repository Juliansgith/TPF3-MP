//! Persistence for running rooms: one append-only log per room, replayed at
//! start so games survive a server restart.
//!
//! A log is a sequence of records: a little-endian `u32` length, a
//! little-endian `u32` CRC32 of the payload, then the payload. The first
//! record is the room's [`StartRecord`]; every later one is a turn frame,
//! byte for byte as clients received it, so a recovered room serves resumes
//! from exactly the same bytes.
//!
//! A long game's log is compacted: once it outgrows a threshold, it is
//! rewritten to start from where the game stands. Its start record then
//! carries a [`Base`], the game's state, followed by the turns of the resume
//! window, kept for players who resume, and the turns after. The new log is
//! written and flushed under another name and renamed over the old one, so
//! a crash leaves one or the other whole.
//!
//! Recovery reads a log one record at a time and changes nothing on disk
//! until the room has been rebuilt. A crash can tear only the last record,
//! so a damaged final record is cut off before appending continues. Damage
//! anywhere else fails recovery and leaves the file exactly as it was, for
//! diagnosis.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tpf3mp_proto::{
    ContentFingerprint, Platform, PlayerId, RoomId, RoomSettings, Speed, TURN_MAX_FRAME, Text,
};

/// Version of the log's layout. Version 2 added histories: the start
/// record names the first, and each recovery logs a turn-stream start
/// naming the next. Version 3 has the events of protocol snapshots: joins
/// name the player's platform, departures say whether it was a kick, and
/// saves appear in the log. Version 4 lets the start record carry a
/// [`Base`], for compacted logs.
pub(crate) const FORMAT_VERSION: u16 = 4;
/// Largest start record: one whose base holds the rules' state.
const MAX_START_RECORD: usize = 16 << 20;
/// Largest record after the start record: a turn frame at its cap. Kept
/// tight, so damage to a record's length before the end of a log is seen
/// as damage, not as a torn final record.
const MAX_RECORD: usize = TURN_MAX_FRAME + 64;
const HEADER: usize = 8;
/// Bytes one room's log may reach. Past this the game keeps running but is
/// no longer logged, so one room cannot fill the disk. An honest game takes
/// days of play to get here.
pub(crate) const LOG_LIMIT: u64 = 1 << 30;

/// Everything about a room that its turns do not say.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StartRecord {
    pub(crate) version: u16,
    /// The game's first history.
    pub(crate) history: u64,
    pub(crate) id: RoomId,
    pub(crate) name: Text<48>,
    pub(crate) owner: PlayerId,
    pub(crate) max_players: u8,
    pub(crate) settings: RoomSettings,
    pub(crate) invite_tag: Vec<u8>,
    pub(crate) password_tag: Option<Vec<u8>>,
    pub(crate) members: Vec<StartMember>,
    /// For a compacted log, the game's state before its first turn.
    pub(crate) base: Option<Base>,
}

/// A running game's state after a turn, which a compacted log starts from
/// instead of the turns before.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Base {
    /// Where the log's turns begin: the first turn, the first event it
    /// carries, and the frontier before it. The turns up to `after_turn` are
    /// kept only for players who resume; the base covers them already.
    pub(crate) first_turn: u64,
    pub(crate) first_event: u64,
    pub(crate) sealed_before: u64,
    /// The last turn the base covers; the log's turns after it are replayed.
    pub(crate) after_turn: u64,
    /// The next event, the frontier and the speed after `after_turn`.
    pub(crate) next_event: u64,
    pub(crate) sealed_through: u64,
    pub(crate) speed: Speed,
    /// Every history of the game, oldest first: its ID and the last turn it
    /// shares with the one before.
    pub(crate) histories: Vec<(u64, u64)>,
    /// The game build and mods every player runs.
    pub(crate) content: Option<ContentFingerprint>,
    /// Who sat at the table, in join order.
    pub(crate) seated: Vec<(PlayerId, Text<32>, Platform)>,
    /// Players the owner removed.
    pub(crate) banned: Vec<PlayerId>,
    /// The canonical rules' state (`Ruleset::save`).
    pub(crate) rules: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StartMember {
    pub(crate) player: PlayerId,
    pub(crate) name: Text<32>,
    pub(crate) platform: Platform,
    pub(crate) content: Option<ContentFingerprint>,
}

/// Why a log cannot be read back.
#[derive(Debug, Error)]
pub(crate) enum LogError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("the log is not a regular file")]
    NotAFile,
    #[error("the log is {0} bytes, over the size limit")]
    TooLarge(u64),
    #[error("the record at byte {0} is damaged")]
    Damaged(u64),
}

pub(crate) struct RoomLog {
    file: File,
    path: PathBuf,
    /// Bytes in the file, for the size limit.
    written: u64,
    /// For a compacted log not yet [installed](RoomLog::install): the
    /// room's log it is to replace.
    replaces: Option<PathBuf>,
}

impl RoomLog {
    pub(crate) fn path_for(dir: &Path, room: &RoomId) -> PathBuf {
        dir.join(format!("{room}.log"))
    }

    /// Creates the log of a room that has just started. Fails if one exists.
    pub(crate) fn create(dir: &Path, start: &StartRecord) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let path = Self::path_for(dir, &start.id);
        let file = private_options().create_new(true).open(&path)?;
        let mut log = Self {
            file,
            path,
            written: 0,
            replaces: None,
        };
        let payload = postcard::to_stdvec(start).map_err(io::Error::other)?;
        log.append(&payload)?;
        log.file.sync_all()?;
        Ok(log)
    }

    /// Writes a compacted log of `start`'s room under another name and
    /// flushes it to disk: `start`, then `frames`. It becomes the room's log
    /// once [installed](RoomLog::install). Blocking.
    pub(crate) fn compacted(
        dir: &Path,
        start: &StartRecord,
        frames: &[impl AsRef<[u8]>],
    ) -> io::Result<Self> {
        let replaces = Self::path_for(dir, &start.id);
        let partial = replaces.with_extension("log.compacting");
        let _ = fs::remove_file(&partial);
        let file = private_options().create_new(true).open(&partial)?;
        let mut log = Self {
            file,
            path: partial,
            written: 0,
            replaces: Some(replaces),
        };
        let written = (|| {
            let payload = postcard::to_stdvec(start).map_err(io::Error::other)?;
            log.append(&payload)?;
            for frame in frames {
                log.append(frame.as_ref())?;
            }
            log.file.sync_all()
        })();
        match written {
            Ok(()) => Ok(log),
            Err(error) => {
                log.discard();
                Err(error)
            }
        }
    }

    /// Puts a compacted log in place of the room's log, in one rename, and
    /// keeps it open for appending. The log it replaces may stay open until
    /// then, and on failure it is still the room's log.
    pub(crate) fn install(&mut self) -> io::Result<()> {
        let Some(replaces) = self.replaces.take() else {
            return Ok(());
        };
        if let Err(error) = fs::rename(&self.path, &replaces) {
            self.replaces = Some(replaces);
            return Err(error);
        }
        if let Some(dir) = replaces.parent() {
            sync_dir(dir);
        }
        self.path = replaces;
        Ok(())
    }

    /// Deletes a compacted log that will not be installed. An installed log
    /// is only closed.
    pub(crate) fn discard(self) {
        let Self {
            file,
            path,
            replaces,
            ..
        } = self;
        drop(file);
        if replaces.is_some() {
            let _ = fs::remove_file(path);
        }
    }

    /// Bytes in the log.
    pub(crate) fn written(&self) -> u64 {
        self.written
    }

    /// Opens an existing log for reading, record by record. Symbolic links
    /// and files larger than any log can grow are refused.
    pub(crate) fn read(path: &Path) -> Result<LogReader, LogError> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() {
            return Err(LogError::NotAFile);
        }
        if metadata.len() > LOG_LIMIT + (HEADER + MAX_START_RECORD) as u64 {
            return Err(LogError::TooLarge(metadata.len()));
        }
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        Ok(LogReader {
            reader: BufReader::new(file),
            path: path.to_owned(),
            offset: 0,
            len,
            torn_at: None,
        })
    }

    /// Appends one record. The write reaches the operating system at once,
    /// so it survives the process crashing; surviving a machine crash would
    /// need an fsync per turn, which this deliberately leaves out.
    pub(crate) fn append(&mut self, payload: &[u8]) -> io::Result<()> {
        let limit = if self.written == 0 {
            MAX_START_RECORD
        } else {
            MAX_RECORD
        };
        if payload.len() > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "record exceeds the log's limit",
            ));
        }
        let size = (HEADER + payload.len()) as u64;
        if self.written + size > LOG_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "the room log reached its size limit",
            ));
        }
        let mut record = Vec::with_capacity(HEADER + payload.len());
        record.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        record.extend_from_slice(&crc32fast::hash(payload).to_le_bytes());
        record.extend_from_slice(payload);
        self.file.write_all(&record)?;
        self.written += size;
        Ok(())
    }

    pub(crate) fn delete(self) -> io::Result<()> {
        let Self { file, path, .. } = self;
        drop(file);
        fs::remove_file(path)
    }
}

/// Reads a log for recovery without changing it.
pub(crate) struct LogReader {
    reader: BufReader<File>,
    path: PathBuf,
    offset: u64,
    len: u64,
    /// Where a torn final record starts, once reading has reached it.
    torn_at: Option<u64>,
}

impl LogReader {
    /// The next intact record, or `None` at the end of the log. A damaged
    /// final record is taken for a write the crash tore, and ends the log;
    /// damage before it is an error.
    pub(crate) fn next_record(&mut self) -> Result<Option<Vec<u8>>, LogError> {
        if self.torn_at.is_some() || self.offset == self.len {
            return Ok(None);
        }
        let remaining = self.len - self.offset;
        if remaining < HEADER as u64 {
            return self.torn();
        }
        let mut header = [0; HEADER];
        self.reader.read_exact(&mut header)?;
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let crc = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        let limit = if self.offset == 0 {
            MAX_START_RECORD
        } else {
            MAX_RECORD
        };
        if len > limit {
            // The server never writes such a record, torn or not.
            return Err(LogError::Damaged(self.offset));
        }
        let end = self.offset + (HEADER + len) as u64;
        if end > self.len {
            return self.torn();
        }
        let mut payload = vec![0; len];
        self.reader.read_exact(&mut payload)?;
        if crc32fast::hash(&payload) != crc {
            return if end == self.len {
                self.torn()
            } else {
                Err(LogError::Damaged(self.offset))
            };
        }
        self.offset = end;
        Ok(Some(payload))
    }

    fn torn(&mut self) -> Result<Option<Vec<u8>>, LogError> {
        if self.offset == 0 {
            // Even the start record is damaged: nothing here is a game.
            return Err(LogError::Damaged(0));
        }
        self.torn_at = Some(self.offset);
        Ok(None)
    }

    /// Continues the log after its last intact record, cutting off a torn
    /// final record. Call only after reading every record.
    pub(crate) fn into_log(self) -> io::Result<RoomLog> {
        let Self {
            reader,
            path,
            offset,
            len,
            torn_at,
        } = self;
        drop(reader);
        if torn_at.is_none() && offset != len {
            return Err(io::Error::other("the log was not read to its end"));
        }
        let mut file = OpenOptions::new().write(true).open(&path)?;
        if torn_at.is_some() {
            file.set_len(offset)?;
            file.sync_all()?;
        }
        file.seek(SeekFrom::End(0))?;
        Ok(RoomLog {
            file,
            path,
            written: offset,
            replaces: None,
        })
    }

    /// Deletes the log, for a game everyone had left.
    pub(crate) fn delete(self) -> io::Result<()> {
        let Self { reader, path, .. } = self;
        drop(reader);
        fs::remove_file(path)
    }
}

/// Makes a rename in `dir` durable, where the platform allows: on Unix a
/// directory is flushed like a file; elsewhere the rename is left to the
/// file system.
fn sync_dir(dir: &Path) {
    if cfg!(unix)
        && let Ok(dir) = File::open(dir)
    {
        let _ = dir.sync_all();
    }
}

/// Logs hold invite and password tags and every player's commands, so only
/// the server's own user may read them.
fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

#[cfg(test)]
mod tests {
    use tpf3mp_proto::FixedBytes;

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tpf3mp-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn start() -> StartRecord {
        StartRecord {
            version: FORMAT_VERSION,
            history: 1,
            id: RoomId(FixedBytes([1; 16])),
            name: Text::new("room").unwrap(),
            owner: PlayerId(FixedBytes([2; 32])),
            max_players: 4,
            settings: RoomSettings::DEFAULT,
            invite_tag: vec![3; 32],
            password_tag: None,
            members: Vec::new(),
            base: None,
        }
    }

    fn read_all(path: &Path) -> Result<(LogReader, Vec<Vec<u8>>), LogError> {
        let mut reader = RoomLog::read(path)?;
        let mut records = Vec::new();
        while let Some(record) = reader.next_record()? {
            records.push(record);
        }
        Ok((reader, records))
    }

    #[test]
    fn records_survive_a_reopen() {
        let dir = temp_dir("log-reopen");
        let mut log = RoomLog::create(&dir, &start()).unwrap();
        log.append(b"turn one").unwrap();
        log.append(b"turn two").unwrap();
        let path = RoomLog::path_for(&dir, &start().id);
        drop(log);
        let (reader, records) = read_all(&path).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[2], b"turn two");
        let mut log = reader.into_log().unwrap();
        log.append(b"turn three").unwrap();
        drop(log);
        let (reader, records) = read_all(&path).unwrap();
        assert_eq!(records.len(), 4);
        reader.delete().unwrap();
        assert!(!path.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_torn_tail_is_cut_off_only_when_appending_resumes() {
        let dir = temp_dir("log-torn");
        let mut log = RoomLog::create(&dir, &start()).unwrap();
        log.append(b"complete").unwrap();
        let path = RoomLog::path_for(&dir, &start().id);
        drop(log);
        let intact = fs::metadata(&path).unwrap().len();
        // A crash in the middle of the next record.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&100u32.to_le_bytes()).unwrap();
        file.write_all(&[0; 7]).unwrap();
        drop(file);
        let torn = fs::metadata(&path).unwrap().len();
        let (reader, records) = read_all(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            torn,
            "reading changes nothing"
        );
        let mut log = reader.into_log().unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), intact);
        // Appending continues cleanly after the cut.
        log.append(b"after").unwrap();
        drop(log);
        let (_, records) = read_all(&path).unwrap();
        assert_eq!(records[2], b"after");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_damaged_final_record_counts_as_torn() {
        let dir = temp_dir("log-last");
        let mut log = RoomLog::create(&dir, &start()).unwrap();
        log.append(b"first").unwrap();
        log.append(b"second").unwrap();
        let path = RoomLog::path_for(&dir, &start().id);
        drop(log);
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(&path, &bytes).unwrap();
        let (_, records) = read_all(&path).unwrap();
        assert_eq!(records.len(), 2);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn damage_before_the_end_fails_and_changes_nothing() {
        let dir = temp_dir("log-middle");
        let mut log = RoomLog::create(&dir, &start()).unwrap();
        log.append(b"first").unwrap();
        log.append(b"second").unwrap();
        let path = RoomLog::path_for(&dir, &start().id);
        drop(log);
        let mut bytes = fs::read(&path).unwrap();
        // The last byte of "first", followed by the intact "second".
        let at = bytes.len() - (HEADER + b"second".len()) - 1;
        bytes[at] ^= 0xff;
        fs::write(&path, &bytes).unwrap();
        assert!(matches!(read_all(&path), Err(LogError::Damaged(_))));
        assert_eq!(fs::read(&path).unwrap(), bytes);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_damaged_start_record_is_never_cut() {
        let dir = temp_dir("log-start");
        let log = RoomLog::create(&dir, &start()).unwrap();
        let path = RoomLog::path_for(&dir, &start().id);
        drop(log);
        let mut bytes = fs::read(&path).unwrap();
        bytes[HEADER] ^= 0xff;
        fs::write(&path, &bytes).unwrap();
        assert!(matches!(read_all(&path), Err(LogError::Damaged(0))));
        assert_eq!(fs::read(&path).unwrap(), bytes);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_log_stops_at_its_size_limit() {
        let dir = temp_dir("log-limit");
        let mut log = RoomLog::create(&dir, &start()).unwrap();
        log.written = LOG_LIMIT - 20;
        log.append(b"fits").unwrap();
        let error = log.append(b"does not fit").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
        drop(log);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_rewritten_log_holds_exactly_the_new_start_and_frames() {
        let dir = temp_dir("log-rewrite");
        let mut log = RoomLog::create(&dir, &start()).unwrap();
        log.append(b"old turn").unwrap();
        drop(log);
        let mut compacted = start();
        compacted.base = Some(Base {
            first_turn: 39,
            first_event: 11,
            sealed_before: 280,
            after_turn: 40,
            next_event: 12,
            sealed_through: 300,
            speed: Speed::NORMAL,
            histories: vec![(1, 0)],
            content: None,
            seated: Vec::new(),
            banned: Vec::new(),
            rules: vec![9; 100],
        });
        let mut log = RoomLog::compacted(&dir, &compacted, &[b"turn 41", b"turn 42"]).unwrap();
        let path = RoomLog::path_for(&dir, &start().id);
        let (_, before) = read_all(&path).unwrap();
        assert_eq!(
            before.len(),
            2,
            "the room's log is untouched until installed"
        );
        log.install().unwrap();
        log.append(b"turn 43").unwrap();
        drop(log);
        let (_, records) = read_all(&path).unwrap();
        assert_eq!(records.len(), 4);
        let first: StartRecord = postcard::from_bytes(&records[0]).unwrap();
        assert_eq!(first.base, compacted.base);
        assert_eq!(
            records[1..],
            [
                b"turn 41".to_vec(),
                b"turn 42".to_vec(),
                b"turn 43".to_vec()
            ]
        );
        assert!(!path.with_extension("log.compacting").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Damage before the final record is damage: a turn record's length
    /// that runs past the end is no torn tail when it is over any turn's,
    /// even if a start record may be that long.
    #[test]
    fn a_damaged_length_before_the_end_is_not_a_torn_tail() {
        let dir = temp_dir("poc-length");
        let mut log = RoomLog::create(&dir, &start()).unwrap();
        log.append(b"first").unwrap();
        // 2 MiB of intact turns after it.
        let turn = vec![7u8; 512 << 10];
        for _ in 0..4 {
            log.append(&turn).unwrap();
        }
        let path = RoomLog::path_for(&dir, &start().id);
        drop(log);
        let mut bytes = fs::read(&path).unwrap();
        let start_len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        let at = HEADER + start_len;
        // The length of "first" becomes 8 MiB: past the end of the file,
        // over any turn frame, but under the new MAX_RECORD.
        bytes[at..at + 4].copy_from_slice(&(8u32 << 20).to_le_bytes());
        fs::write(&path, &bytes).unwrap();
        let outcome = read_all(&path).map(|(_, records)| records.len());
        fs::remove_dir_all(&dir).unwrap();
        assert!(
            matches!(outcome, Err(LogError::Damaged(_))),
            "damage before the end was read as a torn tail: {outcome:?} records kept of 6"
        );
    }

    /// Compaction keeps the room's log open until the new one is in place,
    /// and the new one open from writing it to appending to it.
    #[test]
    fn a_compacted_log_replaces_one_still_open() {
        let dir = temp_dir("replace-open");
        let mut old = RoomLog::create(&dir, &start()).unwrap();
        old.append(b"old turn").unwrap();
        let mut new = RoomLog::compacted(&dir, &start(), &[b"new turn"]).unwrap();
        old.append(b"turn while compacting").unwrap();
        new.append(b"turn while compacting").unwrap();
        new.install().unwrap();
        drop(old);
        new.append(b"turn after").unwrap();
        drop(new);
        let path = RoomLog::path_for(&dir, &start().id);
        let (_, records) = read_all(&path).unwrap();
        assert_eq!(
            records[1..],
            [
                b"new turn".to_vec(),
                b"turn while compacting".to_vec(),
                b"turn after".to_vec()
            ]
        );
        assert!(!path.with_extension("log.compacting").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_discarded_compaction_leaves_the_log_alone() {
        let dir = temp_dir("discard");
        let mut log = RoomLog::create(&dir, &start()).unwrap();
        log.append(b"turn").unwrap();
        RoomLog::compacted(&dir, &start(), &[b"other"])
            .unwrap()
            .discard();
        let path = RoomLog::path_for(&dir, &start().id);
        assert!(!path.with_extension("log.compacting").exists());
        drop(log);
        let (_, records) = read_all(&path).unwrap();
        assert_eq!(records[1], b"turn");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_log_is_never_silently_overwritten() {
        let dir = temp_dir("log-exists");
        let _log = RoomLog::create(&dir, &start()).unwrap();
        assert!(RoomLog::create(&dir, &start()).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
