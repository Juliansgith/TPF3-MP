use std::{
    collections::{HashMap, HashSet},
    fmt,
    io::Read,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    ChunkId, ChunkParams, Chunker, FileHash, MAX_CHUNKS, MAX_FILE_SIZE, ManifestId, ParamsError,
    SourceError,
};

const MAGIC: [u8; 4] = *b"T3SM";

/// Manifest format version. It fixes the chunking algorithm, the hash and the
/// compression, along with the layout below.
pub const MANIFEST_VERSION: u8 = 1;

/// Longest encoded field before the chunk list: magic, version, three `u32`
/// varints, a `u64` varint, the file hash and the chunk count.
const HEADER_MAX: usize = 4 + 1 + 3 * 5 + 10 + 32 + 10;
/// Longest canonical chunk entry: the id, an offset below 4 GiB (5 varint
/// bytes) and a length of at most 4 MiB (4 varint bytes).
const ENTRY_MAX: usize = 32 + 5 + 4;
/// Shortest possible chunk entry: the id and two one-byte varints.
const ENTRY_MIN: usize = 32 + 1 + 1;

/// The longest valid manifest. Longer input is rejected before decoding.
pub const MAX_MANIFEST_LEN: usize = HEADER_MAX + MAX_CHUNKS * ENTRY_MAX;

/// One chunk of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChunkEntry {
    pub id: ChunkId,
    /// Position of the chunk's first byte in the file.
    pub offset: u64,
    /// Uncompressed length.
    pub len: u32,
}

/// The ordered chunk list of one file, with its size and hash.
///
/// A `Manifest` always satisfies the rules [`Manifest::from_bytes`] checks, and
/// its encoding is canonical: one manifest has exactly one encoding, so its
/// [`ManifestId`] is well defined.
///
/// The encoding is postcard, in this order: the magic bytes `T3SM`, the
/// version byte, the chunking parameters (`min`, `avg`, `max` as `u32`), the
/// file size (`u64`), the file hash (32 bytes) and the chunk list (a count,
/// then `id`, `offset` (`u64`), `len` (`u32`) per chunk).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    params: ChunkParams,
    total_size: u64,
    file_hash: FileHash,
    chunks: Vec<ChunkEntry>,
    id: ManifestId,
}

#[derive(Serialize)]
struct Wire<'a> {
    magic: [u8; 4],
    version: u8,
    params: WireParams,
    total_size: u64,
    file_hash: FileHash,
    chunks: &'a [ChunkEntry],
}

#[derive(Serialize, Deserialize)]
struct WireParams {
    min: u32,
    avg: u32,
    max: u32,
}

impl Manifest {
    /// Chunks and hashes `source` without storing anything, for example to
    /// compare a local file with a server's manifest.
    pub fn compute(source: impl Read, params: ChunkParams) -> Result<Self, ChunkingError> {
        let mut chunker = Chunker::new(source, params);
        let mut chunks = Vec::new();
        while let Some(chunk) = chunker.next_chunk()? {
            chunks.push(ChunkEntry {
                id: chunk.id,
                offset: chunk.offset,
                len: chunk_len(&chunk.data),
            });
        }
        let (total_size, file_hash) = chunker.finish()?;
        Ok(Self::from_chunks(params, total_size, file_hash, chunks)?)
    }

    /// Builds the manifest of chunks the [`Chunker`] cut. They satisfy every
    /// rule by construction; checking anyway means a chunker bug fails here
    /// instead of publishing a manifest that every receiver rejects.
    pub(crate) fn from_chunks(
        params: ChunkParams,
        total_size: u64,
        file_hash: FileHash,
        chunks: Vec<ChunkEntry>,
    ) -> Result<Self, ManifestError> {
        check(params, total_size, &chunks)?;
        let encoded = encode(params, total_size, file_hash, &chunks);
        Ok(Self {
            params,
            total_size,
            file_hash,
            chunks,
            id: ManifestId(*blake3::hash(&encoded).as_bytes()),
        })
    }

    /// Decodes and validates a manifest from an untrusted source.
    ///
    /// Decoding is strict and bounded: the input length is checked before
    /// anything is decoded, the declared chunk count before any entry is
    /// read, and allocation never exceeds what the input itself could hold.
    /// Every entry must continue where the previous one ended, fit the
    /// declared chunking parameters, and the chunks must cover the file
    /// exactly. Encodings other than the canonical one are rejected.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ManifestError> {
        if bytes.len() > MAX_MANIFEST_LEN {
            return Err(ManifestError::TooLarge {
                len: bytes.len(),
                max: MAX_MANIFEST_LEN,
            });
        }
        let mut input = Input(bytes);
        let magic: [u8; 4] = input.take(ManifestField::Magic)?;
        if magic != MAGIC {
            return Err(ManifestError::BadMagic);
        }
        let version: u8 = input.take(ManifestField::Version)?;
        if version != MANIFEST_VERSION {
            return Err(ManifestError::UnsupportedVersion { found: version });
        }
        let wire: WireParams = input.take(ManifestField::Params)?;
        let params = ChunkParams::new(wire.min, wire.avg, wire.max)?;
        let total_size: u64 = input.take(ManifestField::TotalSize)?;
        if total_size > MAX_FILE_SIZE {
            return Err(ManifestError::FileTooLarge { size: total_size });
        }
        let file_hash: FileHash = input.take(ManifestField::FileHash)?;
        let count: u64 = input.take(ManifestField::ChunkCount)?;
        let max = params.max_chunks(total_size);
        if count > max {
            return Err(ManifestError::TooManyChunks { count, max });
        }
        // `max` is at most MAX_FILE_SIZE / MIN_CHUNK_FLOOR, so this fits.
        let count = usize::try_from(count).unwrap_or(MAX_CHUNKS);
        let mut chunks = Vec::with_capacity(count.min(input.0.len() / ENTRY_MIN));
        let mut rules = Rules::new(params, total_size, count);
        for index in 0..count {
            let entry: ChunkEntry = input.take(ManifestField::Chunk(index))?;
            rules.check(index, &entry)?;
            chunks.push(entry);
        }
        if !input.0.is_empty() {
            return Err(ManifestError::TrailingBytes(input.0.len()));
        }
        rules.finish()?;
        if encode(params, total_size, file_hash, &chunks) != bytes {
            return Err(ManifestError::NonCanonical);
        }
        Ok(Self {
            params,
            total_size,
            file_hash,
            chunks,
            id: ManifestId(*blake3::hash(bytes).as_bytes()),
        })
    }

    /// The canonical encoding.
    pub fn to_bytes(&self) -> Vec<u8> {
        encode(self.params, self.total_size, self.file_hash, &self.chunks)
    }

    /// The BLAKE3 hash of [`to_bytes`](Self::to_bytes).
    pub fn id(&self) -> ManifestId {
        self.id
    }

    pub fn params(&self) -> ChunkParams {
        self.params
    }

    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    pub fn file_hash(&self) -> FileHash {
        self.file_hash
    }

    /// Every chunk in file order. An id repeats when content repeats.
    pub fn chunks(&self) -> &[ChunkEntry] {
        &self.chunks
    }

    /// The first entry of each distinct chunk, in file order: what a receiver
    /// has to hold to assemble the file.
    pub fn unique_chunks(&self) -> Vec<ChunkEntry> {
        let mut seen = HashSet::with_capacity(self.chunks.len());
        self.chunks
            .iter()
            .filter(|entry| seen.insert(entry.id))
            .copied()
            .collect()
    }
}

#[derive(Debug, Error)]
pub enum ChunkingError {
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error("the chunker produced an invalid manifest: {0}")]
    Invalid(#[from] ManifestError),
}

fn check(params: ChunkParams, total_size: u64, chunks: &[ChunkEntry]) -> Result<(), ManifestError> {
    if total_size > MAX_FILE_SIZE {
        return Err(ManifestError::FileTooLarge { size: total_size });
    }
    let count = chunks.len() as u64;
    let max = params.max_chunks(total_size);
    if count > max {
        return Err(ManifestError::TooManyChunks { count, max });
    }
    let mut rules = Rules::new(params, total_size, chunks.len());
    for (index, entry) in chunks.iter().enumerate() {
        rules.check(index, entry)?;
    }
    rules.finish()
}

/// The length of a chunk the [`Chunker`] cut, which never exceeds
/// [`MAX_CHUNK_LEN`](crate::MAX_CHUNK_LEN).
pub(crate) fn chunk_len(data: &[u8]) -> u32 {
    u32::try_from(data.len()).unwrap_or(u32::MAX)
}

fn encode(
    params: ChunkParams,
    total_size: u64,
    file_hash: FileHash,
    chunks: &[ChunkEntry],
) -> Vec<u8> {
    let wire = Wire {
        magic: MAGIC,
        version: MANIFEST_VERSION,
        params: WireParams {
            min: params.min(),
            avg: params.avg(),
            max: params.max(),
        },
        total_size,
        file_hash,
        chunks,
    };
    postcard::to_stdvec(&wire).expect("postcard encodes plain structs, arrays and slices")
}

/// The layout rules for chunk entries, checked one entry at a time so decoding
/// stops at the first bad one.
struct Rules {
    params: ChunkParams,
    total_size: u64,
    count: usize,
    next_offset: u64,
    lengths: HashMap<ChunkId, u32>,
}

impl Rules {
    fn new(params: ChunkParams, total_size: u64, count: usize) -> Self {
        Self {
            params,
            total_size,
            count,
            next_offset: 0,
            lengths: HashMap::with_capacity(count.min(1 << 16)),
        }
    }

    fn check(&mut self, index: usize, entry: &ChunkEntry) -> Result<(), ManifestError> {
        if entry.offset != self.next_offset {
            return Err(ManifestError::NonContiguous {
                index,
                expected: self.next_offset,
                found: entry.offset,
            });
        }
        if entry.len == 0 {
            return Err(ManifestError::EmptyChunk { index });
        }
        if entry.len > self.params.max() {
            return Err(ManifestError::ChunkTooLong {
                index,
                len: entry.len,
                max: self.params.max(),
            });
        }
        let last = index + 1 == self.count;
        if !last && entry.len < self.params.min() {
            return Err(ManifestError::ChunkTooShort {
                index,
                len: entry.len,
                min: self.params.min(),
            });
        }
        let end = entry.offset + u64::from(entry.len);
        if end > self.total_size {
            return Err(ManifestError::PastEnd {
                index,
                end,
                total: self.total_size,
            });
        }
        if let Some(previous) = self.lengths.insert(entry.id, entry.len)
            && previous != entry.len
        {
            return Err(ManifestError::InconsistentDuplicate {
                index,
                id: entry.id,
            });
        }
        self.next_offset = end;
        Ok(())
    }

    fn finish(self) -> Result<(), ManifestError> {
        if self.next_offset != self.total_size {
            return Err(ManifestError::SizeMismatch {
                covered: self.next_offset,
                total: self.total_size,
            });
        }
        Ok(())
    }
}

/// The undecoded rest of a manifest.
struct Input<'a>(&'a [u8]);

impl<'a> Input<'a> {
    fn take<T: Deserialize<'a>>(&mut self, field: ManifestField) -> Result<T, ManifestError> {
        match postcard::take_from_bytes(self.0) {
            Ok((value, rest)) => {
                self.0 = rest;
                Ok(value)
            }
            Err(postcard::Error::DeserializeUnexpectedEnd) => Err(ManifestError::Truncated(field)),
            Err(source) => Err(ManifestError::Malformed { field, source }),
        }
    }
}

/// A part of the manifest encoding, for error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestField {
    Magic,
    Version,
    Params,
    TotalSize,
    FileHash,
    ChunkCount,
    Chunk(usize),
}

impl fmt::Display for ManifestField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Magic => f.write_str("magic bytes"),
            Self::Version => f.write_str("format version"),
            Self::Params => f.write_str("chunking parameters"),
            Self::TotalSize => f.write_str("file size"),
            Self::FileHash => f.write_str("file hash"),
            Self::ChunkCount => f.write_str("chunk count"),
            Self::Chunk(index) => write!(f, "entry of chunk {index}"),
        }
    }
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("manifest of {len} bytes exceeds the {max}-byte limit")]
    TooLarge { len: usize, max: usize },
    #[error("not a snapshot manifest")]
    BadMagic,
    #[error(
        "manifest version {found} is not supported; this build reads version {MANIFEST_VERSION}"
    )]
    UnsupportedVersion { found: u8 },
    #[error("the manifest ends inside the {0}")]
    Truncated(ManifestField),
    #[error("malformed {field}: {source}")]
    Malformed {
        field: ManifestField,
        source: postcard::Error,
    },
    #[error("{0} unexpected bytes after the manifest")]
    TrailingBytes(usize),
    #[error("the manifest is not canonically encoded")]
    NonCanonical,
    #[error("invalid chunking parameters: {0}")]
    Params(#[from] ParamsError),
    #[error("file size {size} exceeds the {max}-byte limit", max = MAX_FILE_SIZE)]
    FileTooLarge { size: u64 },
    #[error("{count} chunks declared, but the file fits at most {max}")]
    TooManyChunks { count: u64, max: u64 },
    #[error("chunk {index} starts at byte {found} instead of {expected}")]
    NonContiguous {
        index: usize,
        expected: u64,
        found: u64,
    },
    #[error("chunk {index} is empty")]
    EmptyChunk { index: usize },
    #[error("chunk {index} is {len} bytes, below the {min}-byte minimum")]
    ChunkTooShort { index: usize, len: u32, min: u32 },
    #[error("chunk {index} is {len} bytes, above the {max}-byte maximum")]
    ChunkTooLong { index: usize, len: u32, max: u32 },
    #[error("chunk {index} ends at byte {end}, past the {total}-byte file")]
    PastEnd { index: usize, end: u64, total: u64 },
    #[error("the chunks cover {covered} of the file's {total} bytes")]
    SizeMismatch { covered: u64, total: u64 },
    #[error("chunk {index} repeats id {id} with a different length")]
    InconsistentDuplicate { index: usize, id: ChunkId },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MIN_CHUNK_FLOOR;

    const K: u32 = 1 << 10;

    fn entry(fill: u8, offset: u64, len: u32) -> ChunkEntry {
        ChunkEntry {
            id: ChunkId([fill; 32]),
            offset,
            len,
        }
    }

    /// Encodes any field values, including invalid ones.
    fn raw(params: [u32; 3], total_size: u64, chunks: &[ChunkEntry]) -> Vec<u8> {
        let [min, avg, max] = params;
        postcard::to_stdvec(&Wire {
            magic: MAGIC,
            version: MANIFEST_VERSION,
            params: WireParams { min, avg, max },
            total_size,
            file_hash: FileHash([0x11; 32]),
            chunks,
        })
        .unwrap()
    }

    const SMALL: [u32; 3] = [16 * K, 32 * K, 64 * K];

    /// Two chunks: a full minimum-size one and a short tail.
    fn two_chunks() -> Vec<u8> {
        raw(
            SMALL,
            20_000,
            &[entry(0xaa, 0, 16_384), entry(0xbb, 16_384, 3_616)],
        )
    }

    fn decode(bytes: &[u8]) -> ManifestError {
        Manifest::from_bytes(bytes).unwrap_err()
    }

    /// Guards against accidental format changes such as reordered fields.
    /// Update deliberately, together with `MANIFEST_VERSION`.
    #[test]
    fn wire_format_is_stable() {
        let mut expected = vec![b'T', b'3', b'S', b'M', 1];
        expected.extend_from_slice(&[0x80, 0x80, 0x01]); // min 16384
        expected.extend_from_slice(&[0x80, 0x80, 0x02]); // avg 32768
        expected.extend_from_slice(&[0x80, 0x80, 0x04]); // max 65536
        expected.extend_from_slice(&[0xa0, 0x9c, 0x01]); // total_size 20000
        expected.extend_from_slice(&[0x11; 32]); // file_hash
        expected.push(2); // chunk count
        expected.extend_from_slice(&[0xaa; 32]);
        expected.extend_from_slice(&[0x00, 0x80, 0x80, 0x01]); // offset 0, len 16384
        expected.extend_from_slice(&[0xbb; 32]);
        expected.extend_from_slice(&[0x80, 0x80, 0x01, 0xa0, 0x1c]); // offset 16384, len 3616
        assert_eq!(two_chunks(), expected);
        let manifest = Manifest::from_bytes(&expected).unwrap();
        assert_eq!(manifest.to_bytes(), expected);
        assert_eq!(manifest.id().0, *blake3::hash(&expected).as_bytes());
    }

    #[test]
    fn computed_manifests_round_trip() {
        let data: Vec<u8> = (0..300_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect();
        let params = ChunkParams::new(16 * K, 32 * K, 64 * K).unwrap();
        let manifest = Manifest::compute(&data[..], params).unwrap();
        assert!(manifest.chunks().len() > 2);
        assert_eq!(manifest.total_size(), 300_000);
        assert_eq!(manifest.file_hash().0, *blake3::hash(&data).as_bytes());
        let decoded = Manifest::from_bytes(&manifest.to_bytes()).unwrap();
        assert_eq!(decoded, manifest);
    }

    #[test]
    fn empty_files_have_empty_manifests() {
        let manifest = Manifest::compute(&[][..], ChunkParams::DEFAULT).unwrap();
        assert!(manifest.chunks().is_empty());
        assert_eq!(
            Manifest::from_bytes(&manifest.to_bytes()).unwrap(),
            manifest
        );
    }

    #[test]
    fn repeated_chunks_are_listed_once_as_unique() {
        let bytes = raw(
            SMALL,
            3 * 16_384,
            &[
                entry(1, 0, 16_384),
                entry(2, 16_384, 16_384),
                entry(1, 32_768, 16_384),
            ],
        );
        let manifest = Manifest::from_bytes(&bytes).unwrap();
        let unique: Vec<u8> = manifest.unique_chunks().iter().map(|e| e.id.0[0]).collect();
        assert_eq!(unique, [1, 2]);
    }

    #[test]
    fn oversized_input_is_rejected_before_decoding() {
        let bytes = vec![0; MAX_MANIFEST_LEN + 1];
        assert!(matches!(
            decode(&bytes),
            ManifestError::TooLarge { len, max: MAX_MANIFEST_LEN } if len == MAX_MANIFEST_LEN + 1
        ));
    }

    #[test]
    fn header_errors_are_precise() {
        let mut bytes = two_chunks();
        bytes[0] = b'X';
        assert!(matches!(decode(&bytes), ManifestError::BadMagic));

        let mut bytes = two_chunks();
        bytes[4] = 2;
        assert!(matches!(
            decode(&bytes),
            ManifestError::UnsupportedVersion { found: 2 }
        ));

        let bytes = raw([16 * K, 48 * K, 64 * K], 20_000, &[]);
        assert!(matches!(
            decode(&bytes),
            ManifestError::Params(ParamsError::NotPowersOfTwo { .. })
        ));

        let bytes = raw(SMALL, MAX_FILE_SIZE + 1, &[]);
        assert!(matches!(
            decode(&bytes),
            ManifestError::FileTooLarge { size } if size == MAX_FILE_SIZE + 1
        ));
    }

    #[test]
    fn every_truncation_is_rejected() {
        let bytes = two_chunks();
        for len in 0..bytes.len() {
            assert!(
                Manifest::from_bytes(&bytes[..len]).is_err(),
                "prefix of {len} bytes"
            );
        }
        assert!(matches!(
            decode(&bytes[..0]),
            ManifestError::Truncated(ManifestField::Magic)
        ));
        assert!(matches!(
            decode(&bytes[..4]),
            ManifestError::Truncated(ManifestField::Version)
        ));
        assert!(matches!(
            decode(&bytes[..bytes.len() - 1]),
            ManifestError::Truncated(ManifestField::Chunk(1))
        ));
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut bytes = two_chunks();
        bytes.push(0);
        assert!(matches!(decode(&bytes), ManifestError::TrailingBytes(1)));
    }

    #[test]
    fn overlong_varints_are_rejected() {
        // The chunk count 2 as the two-byte varint 0x82 0x00. Postcard
        // accepts it, so only the canonical-form check catches it.
        let mut bytes = two_chunks();
        let count_at = 4 + 1 + 9 + 3 + 32;
        assert_eq!(bytes[count_at], 2);
        bytes.splice(count_at..=count_at, [0x82, 0x00]);
        assert!(matches!(decode(&bytes), ManifestError::NonCanonical));
    }

    #[test]
    fn broken_varints_are_malformed() {
        let mut bytes = raw(SMALL, 0, &[]);
        let total_at = 4 + 1 + 9;
        bytes.splice(total_at..=total_at, [0xff; 11]);
        assert!(matches!(
            decode(&bytes),
            ManifestError::Malformed {
                field: ManifestField::TotalSize,
                source: postcard::Error::DeserializeBadVarint
            }
        ));
    }

    #[test]
    fn huge_chunk_counts_are_rejected_before_allocating() {
        let mut bytes = raw(SMALL, MAX_FILE_SIZE, &[]);
        bytes.pop();
        bytes.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]);
        let max = MAX_FILE_SIZE / u64::from(MIN_CHUNK_FLOOR);
        assert!(matches!(
            decode(&bytes),
            ManifestError::TooManyChunks { count: u64::MAX, max: m } if m == max
        ));
        // More chunks than the file size allows at the declared minimum.
        let bytes = raw(
            SMALL,
            20_000,
            &[
                entry(1, 0, 16_384),
                entry(2, 16_384, 3_000),
                entry(3, 19_384, 616),
            ],
        );
        assert!(matches!(
            decode(&bytes),
            ManifestError::TooManyChunks { count: 3, max: 2 }
        ));
    }

    #[test]
    fn chunk_layout_errors_are_precise() {
        let reject = |chunks: &[ChunkEntry], total| decode(&raw(SMALL, total, chunks));
        assert!(matches!(
            reject(&[entry(1, 0, 16_384), entry(2, 16_385, 3_615)], 20_000),
            ManifestError::NonContiguous {
                index: 1,
                expected: 16_384,
                found: 16_385
            }
        ));
        assert!(matches!(
            reject(&[entry(1, 0, 16_384), entry(2, 16_384, 0)], 20_000),
            ManifestError::EmptyChunk { index: 1 }
        ));
        assert!(matches!(
            reject(&[entry(1, 0, 8_000), entry(2, 8_000, 12_000)], 20_000),
            ManifestError::ChunkTooShort {
                index: 0,
                len: 8_000,
                min: 16_384
            }
        ));
        assert!(matches!(
            reject(&[entry(1, 0, 70_000)], 70_000),
            ManifestError::ChunkTooLong {
                index: 0,
                len: 70_000,
                max: 65_536
            }
        ));
        assert!(matches!(
            reject(&[entry(1, 0, 16_384), entry(2, 16_384, 8_000)], 20_000),
            ManifestError::PastEnd {
                index: 1,
                end: 24_384,
                total: 20_000
            }
        ));
        assert!(matches!(
            reject(&[entry(1, 0, 16_384)], 20_000),
            ManifestError::SizeMismatch {
                covered: 16_384,
                total: 20_000
            }
        ));
        assert!(matches!(
            reject(&[entry(1, 0, 16_384), entry(1, 16_384, 3_616)], 20_000),
            ManifestError::InconsistentDuplicate { index: 1, .. }
        ));
    }

    #[test]
    fn built_manifests_obey_the_same_rules() {
        let params = ChunkParams::new(16 * K, 32 * K, 64 * K).unwrap();
        let hash = FileHash([0; 32]);
        assert!(Manifest::from_chunks(params, 20_000, hash, vec![entry(1, 0, 20_000)]).is_ok());
        assert!(matches!(
            Manifest::from_chunks(params, 20_000, hash, vec![entry(1, 0, 19_999)]),
            Err(ManifestError::SizeMismatch { .. })
        ));
    }

    /// The bound on manifest length has to admit the largest valid manifest.
    #[test]
    fn largest_manifest_fits_the_limit() {
        let params =
            ChunkParams::new(MIN_CHUNK_FLOOR, 2 * MIN_CHUNK_FLOOR, 4 * MIN_CHUNK_FLOOR).unwrap();
        let chunks = (0..MAX_CHUNKS)
            .map(|index| {
                let mut id = [0; 32];
                id[..8].copy_from_slice(&(index as u64).to_le_bytes());
                ChunkEntry {
                    id: ChunkId(id),
                    offset: index as u64 * u64::from(MIN_CHUNK_FLOOR),
                    len: MIN_CHUNK_FLOOR,
                }
            })
            .collect();
        let manifest =
            Manifest::from_chunks(params, MAX_FILE_SIZE, FileHash([0; 32]), chunks).unwrap();
        let bytes = manifest.to_bytes();
        assert!(bytes.len() <= MAX_MANIFEST_LEN, "{} bytes", bytes.len());
        assert_eq!(
            Manifest::from_bytes(&bytes).unwrap().chunks().len(),
            MAX_CHUNKS
        );
    }
}
