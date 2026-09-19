//! World snapshots: naming them, offering them, and the bulk streams that
//! carry them. See "Snapshots" in `docs/PROTOCOL.md`.
//!
//! A snapshot is a native world save, stored and sent as content-defined
//! chunks by `tpf3mp-snapshot`. This crate names snapshots and chunks by
//! their BLAKE3 digests without depending on that crate, so the hook, which
//! links this crate, stays free of it. Variants are identified by position:
//! append, never reorder.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::bytes::{FixedBytes, write_hex};

/// Largest frame a client sends on a bulk stream: one request for
/// [`MAX_CHUNKS_PER_REQUEST`] chunks fits with room to spare.
pub const BULK_REQUEST_MAX_FRAME: usize = 16 * 1024;

/// Largest frame on a bulk stream from the side that serves a snapshot. It
/// holds the largest manifest `tpf3mp-snapshot` accepts
/// (`MAX_MANIFEST_LEN`, 10,747,976 bytes) and the largest compressed chunk
/// (`MAX_COMPRESSED_CHUNK_LEN`, 4,210,688 bytes), each with its message
/// overhead. Typical frames are far smaller: a manifest takes about 61 KiB
/// per 256 MiB of save, a chunk about 77 KiB.
pub const BULK_RESPONSE_MAX_FRAME: usize = 11 << 20;

/// Most chunks one [`BulkRequest::Chunks`] may ask for.
pub const MAX_CHUNKS_PER_REQUEST: usize = 256;

/// Names a snapshot: the BLAKE3 digest of its canonical manifest
/// (`tpf3mp_snapshot::ManifestId`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SnapshotId(pub FixedBytes<32>);

/// Names one chunk of a snapshot: the BLAKE3 digest of its uncompressed
/// bytes (`tpf3mp_snapshot::ChunkId`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ChunkHash(pub FixedBytes<32>);

impl fmt::Display for SnapshotId {
    /// A short prefix for logs; the full digest is in `Debug`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("w-")?;
        write_hex(f, &self.0.0[..8])
    }
}

impl fmt::Debug for SnapshotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("w-")?;
        write_hex(f, &self.0.0)
    }
}

impl fmt::Debug for ChunkHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("c-")?;
        write_hex(f, &self.0.0)
    }
}

/// A world a turn stream starts from: the client loads it, then follows the
/// stream (see `TurnStart::world`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldOffer {
    pub snapshot: SnapshotId,
    /// Bytes of the save file, for the player's progress display.
    pub size: u64,
}

/// A world a client saved and holds ready to upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedWorld {
    pub snapshot: SnapshotId,
    /// Bytes of the save file.
    pub size: u64,
}

/// The first message on a bulk stream, from the client that opened it. The
/// side that holds the snapshot serves it, and the other side fetches it,
/// with the same requests and responses in either case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BulkOpen {
    /// The client fetches this snapshot, which the server offered it.
    Fetch { snapshot: SnapshotId },
    /// The client serves this snapshot, which the server asked it to
    /// upload.
    Serve { snapshot: SnapshotId },
}

/// From the side that fetches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BulkRequest {
    /// The snapshot's manifest.
    Manifest,
    /// These chunks of it, answered in this order. At most
    /// [`MAX_CHUNKS_PER_REQUEST`], all listed in the manifest.
    Chunks { ids: Vec<ChunkHash> },
}

/// From the side that serves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BulkResponse {
    /// The manifest's canonical bytes.
    Manifest { bytes: Vec<u8> },
    /// One chunk: a single zstd frame of its bytes.
    Chunk { id: ChunkHash, frame: Vec<u8> },
    /// The snapshot is no longer available here, for example because a newer
    /// one replaced it. The stream ends after this.
    Unavailable,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FRAME_HEADER_LEN, decode_frame, encode_frame};

    #[test]
    fn a_full_chunk_request_fits_its_frame() {
        let request = BulkRequest::Chunks {
            ids: vec![ChunkHash(FixedBytes([0xff; 32])); MAX_CHUNKS_PER_REQUEST],
        };
        let frame = encode_frame(&request, BULK_REQUEST_MAX_FRAME).unwrap();
        assert_eq!(
            decode_frame::<BulkRequest>(&frame[FRAME_HEADER_LEN..]).unwrap(),
            request
        );
    }

    #[test]
    fn the_largest_manifest_and_chunk_fit_a_response_frame() {
        // The limits of tpf3mp-snapshot, which this crate does not depend on;
        // the agent's tests check that they have not grown.
        for len in [10_747_976, 4_210_688] {
            let response = BulkResponse::Manifest {
                bytes: vec![0xab; len],
            };
            assert!(encode_frame(&response, BULK_RESPONSE_MAX_FRAME).is_ok());
            let response = BulkResponse::Chunk {
                id: ChunkHash(FixedBytes([0xff; 32])),
                frame: vec![0xab; len],
            };
            assert!(encode_frame(&response, BULK_RESPONSE_MAX_FRAME).is_ok());
        }
    }

    #[test]
    fn snapshot_ids_display_short_and_debug_in_full() {
        let id = SnapshotId(FixedBytes([0xab; 32]));
        assert_eq!(id.to_string(), "w-abababababababab");
        assert_eq!(format!("{id:?}").len(), 2 + 64);
    }
}
