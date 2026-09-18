//! Persistence for running rooms: one append-only log per room, replayed at
//! start so games survive a server restart.
//!
//! A log is a sequence of records: a little-endian `u32` length, a
//! little-endian `u32` CRC32 of the payload, then the payload. The first
//! record is the room's [`StartRecord`]; every later one is a turn frame,
//! byte for byte as clients received it, so a recovered room serves resumes
//! from exactly the same bytes. A crash can tear the last record; reading
//! stops at the first record whose length or checksum does not hold, and the
//! file is cut there before anything is appended.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use tpf3mp_proto::{
    ContentFingerprint, Platform, PlayerId, RoomId, RoomSettings, TURN_MAX_FRAME, Text,
};

/// Version of the start record's layout.
pub(crate) const FORMAT_VERSION: u16 = 1;
/// Largest record a log may hold: a turn frame at its cap.
const MAX_RECORD: usize = TURN_MAX_FRAME + 64;
const HEADER: usize = 8;

/// Everything about a room that its turns do not say.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StartRecord {
    pub(crate) version: u16,
    pub(crate) id: RoomId,
    pub(crate) name: Text<48>,
    pub(crate) owner: PlayerId,
    pub(crate) max_players: u8,
    pub(crate) settings: RoomSettings,
    pub(crate) invite_tag: Vec<u8>,
    pub(crate) password_tag: Option<Vec<u8>>,
    pub(crate) members: Vec<StartMember>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StartMember {
    pub(crate) player: PlayerId,
    pub(crate) name: Text<32>,
    pub(crate) platform: Platform,
    pub(crate) content: Option<ContentFingerprint>,
}

pub(crate) struct RoomLog {
    file: File,
    path: PathBuf,
}

impl RoomLog {
    pub(crate) fn path_for(dir: &Path, room: &RoomId) -> PathBuf {
        dir.join(format!("{room}.log"))
    }

    /// Creates the log of a room that has just started. Fails if one exists.
    pub(crate) fn create(dir: &Path, start: &StartRecord) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let path = Self::path_for(dir, &start.id);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let mut log = Self { file, path };
        let payload = postcard::to_stdvec(start).map_err(io::Error::other)?;
        log.append(&payload)?;
        log.file.sync_all()?;
        Ok(log)
    }

    /// Opens an existing log, returning every intact record. A torn tail is
    /// cut off so that later appends follow the last intact record.
    pub(crate) fn open(path: &Path) -> io::Result<(Self, Vec<Vec<u8>>)> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let (records, intact) = parse(&bytes);
        if intact < bytes.len() {
            file.set_len(intact as u64)?;
            file.sync_all()?;
        }
        file.seek(SeekFrom::End(0))?;
        Ok((
            Self {
                file,
                path: path.to_owned(),
            },
            records,
        ))
    }

    /// Appends one record. The write reaches the operating system at once,
    /// so it survives the process crashing; surviving a machine crash would
    /// need an fsync per turn, which this deliberately leaves out.
    pub(crate) fn append(&mut self, payload: &[u8]) -> io::Result<()> {
        if payload.len() > MAX_RECORD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "record exceeds the log's limit",
            ));
        }
        let mut record = Vec::with_capacity(HEADER + payload.len());
        record.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        record.extend_from_slice(&crc32fast::hash(payload).to_le_bytes());
        record.extend_from_slice(payload);
        self.file.write_all(&record)
    }

    pub(crate) fn delete(self) -> io::Result<()> {
        let Self { file, path } = self;
        drop(file);
        fs::remove_file(path)
    }
}

/// Splits `bytes` into intact records and returns them with the length of
/// the intact prefix.
fn parse(bytes: &[u8]) -> (Vec<Vec<u8>>, usize) {
    let mut records = Vec::new();
    let mut offset = 0;
    while let Some(header) = bytes.get(offset..offset + HEADER) {
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let crc = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        if len > MAX_RECORD {
            break;
        }
        let Some(payload) = bytes.get(offset + HEADER..offset + HEADER + len) else {
            break;
        };
        if crc32fast::hash(payload) != crc {
            break;
        }
        records.push(payload.to_vec());
        offset += HEADER + len;
    }
    (records, offset)
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
            id: RoomId(FixedBytes([1; 16])),
            name: Text::new("room").unwrap(),
            owner: PlayerId(FixedBytes([2; 32])),
            max_players: 4,
            settings: RoomSettings::DEFAULT,
            invite_tag: vec![3; 32],
            password_tag: None,
            members: Vec::new(),
        }
    }

    #[test]
    fn records_survive_a_reopen() {
        let dir = temp_dir("log-reopen");
        let mut log = RoomLog::create(&dir, &start()).unwrap();
        log.append(b"turn one").unwrap();
        log.append(b"turn two").unwrap();
        let path = RoomLog::path_for(&dir, &start().id);
        drop(log);
        let (mut log, records) = RoomLog::open(&path).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[2], b"turn two");
        log.append(b"turn three").unwrap();
        drop(log);
        let (log, records) = RoomLog::open(&path).unwrap();
        assert_eq!(records.len(), 4);
        log.delete().unwrap();
        assert!(!path.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_torn_tail_is_cut_off() {
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
        let (mut log, records) = RoomLog::open(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(fs::metadata(&path).unwrap().len(), intact);
        // Appending continues cleanly after the cut.
        log.append(b"after").unwrap();
        drop(log);
        let (_, records) = RoomLog::open(&path).unwrap();
        assert_eq!(records[2], b"after");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_corrupted_record_ends_the_log() {
        let dir = temp_dir("log-corrupt");
        let mut log = RoomLog::create(&dir, &start()).unwrap();
        log.append(b"first").unwrap();
        log.append(b"second").unwrap();
        let path = RoomLog::path_for(&dir, &start().id);
        drop(log);
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(&path, &bytes).unwrap();
        let (_, records) = RoomLog::open(&path).unwrap();
        assert_eq!(
            records.len(),
            2,
            "the damaged record and anything after it are dropped"
        );
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
