//! zstd compression of single chunks, and verification of every chunk that is
//! read back or received.

use std::io;

use thiserror::Error;
use zstd::bulk::{Compressor, Decompressor};

use crate::{ChunkId, MAX_CHUNK_LEN};

/// The largest compressed chunk any valid manifest allows, so the largest
/// chunk payload a peer ever needs to send.
pub const MAX_COMPRESSED_CHUNK_LEN: usize = compress_bound(MAX_CHUNK_LEN as usize);

/// zstd's worst-case compressed size for `len` input bytes: the
/// `ZSTD_COMPRESSBOUND` macro of `zstd.h` (1.5.x). A frame beyond it cannot
/// come from a standard encoder, so a receiver rejects it unread.
pub(crate) const fn compress_bound(len: usize) -> usize {
    const BLOCK: usize = 128 << 10;
    let margin = if len < BLOCK { (BLOCK - len) >> 11 } else { 0 };
    len + (len >> 8) + margin
}

/// Why a chunk was rejected.
#[derive(Debug, Error)]
pub enum ChunkError {
    #[error("{len}-byte compressed chunk exceeds the {max}-byte bound for its size")]
    TooLarge { len: usize, max: usize },
    #[error("chunk is not exactly one standard zstd frame")]
    NotOneFrame,
    #[error("chunk does not decompress: {0}")]
    Decompress(io::Error),
    #[error("chunk decompresses to {actual} bytes instead of {expected}")]
    LengthMismatch { expected: usize, actual: usize },
    #[error("chunk contents do not match its id")]
    HashMismatch,
    #[error("damaged chunk file: {0}")]
    BadFile(&'static str),
}

pub(crate) fn compressor(level: i32) -> io::Result<Compressor<'static>> {
    Compressor::new(level)
}

pub(crate) fn decompressor() -> io::Result<Decompressor<'static>> {
    Decompressor::new()
}

/// Decompresses `frame` and checks that it holds exactly the `raw_len` bytes
/// that `id` names.
///
/// zstd decodes into a buffer of `raw_len` bytes and fails rather than grow
/// it, so a hostile frame cannot make this allocate more than the manifest
/// allows, and decoding time stays linear in the frame and output sizes.
///
/// The input must be exactly one standard frame. zstd would also accept
/// skippable frames and several frames in a row; stored as received and served
/// on, those would let a peer smuggle arbitrary bytes to everyone who later
/// downloads the chunk.
pub(crate) fn decode(
    decompressor: &mut Decompressor<'_>,
    id: &ChunkId,
    raw_len: u32,
    frame: &[u8],
) -> Result<Vec<u8>, ChunkError> {
    let expected = raw_len as usize;
    let max = compress_bound(expected);
    if frame.len() > max {
        return Err(ChunkError::TooLarge {
            len: frame.len(),
            max,
        });
    }
    let standard = frame
        .first_chunk::<4>()
        .is_some_and(|magic| u32::from_le_bytes(*magic) == zstd::zstd_safe::MAGICNUMBER);
    if !standard || zstd::zstd_safe::find_frame_compressed_size(frame) != Ok(frame.len()) {
        return Err(ChunkError::NotOneFrame);
    }
    let mut raw = Vec::with_capacity(expected);
    decompressor
        .decompress_to_buffer(frame, &mut raw)
        .map_err(ChunkError::Decompress)?;
    if raw.len() != expected {
        return Err(ChunkError::LengthMismatch {
            expected,
            actual: raw.len(),
        });
    }
    if ChunkId::of(&raw) != *id {
        return Err(ChunkError::HashMismatch);
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index % 251) as u8 ^ 0x5a).collect()
    }

    fn frame_of(data: &[u8]) -> Vec<u8> {
        compressor(3).unwrap().compress(data).unwrap()
    }

    fn decode_new(id: &ChunkId, raw_len: u32, frame: &[u8]) -> Result<Vec<u8>, ChunkError> {
        decode(&mut decompressor().unwrap(), id, raw_len, frame)
    }

    #[test]
    fn round_trips() {
        let data = sample(100_000);
        let frame = frame_of(&data);
        assert!(frame.len() < data.len());
        let raw_len = u32::try_from(data.len()).unwrap();
        assert_eq!(
            decode_new(&ChunkId::of(&data), raw_len, &frame).unwrap(),
            data
        );
    }

    #[test]
    fn compress_bound_matches_zstd() {
        for len in [
            0,
            1,
            100,
            2047,
            2048,
            128 << 10,
            (128 << 10) + 1,
            1 << 20,
            MAX_CHUNK_LEN as usize,
        ] {
            assert_eq!(compress_bound(len), zstd::zstd_safe::compress_bound(len));
        }
    }

    #[test]
    fn incompressible_chunks_fit_the_bound() {
        // Every byte value once per 256 bytes, shuffled by a multiplier, so
        // zstd finds nothing to compress.
        let data: Vec<u8> = (0u32..65_536)
            .map(|index| (index.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let frame = frame_of(&data);
        assert!(frame.len() <= compress_bound(data.len()));
        assert!(decode_new(&ChunkId::of(&data), 65_536, &frame).is_ok());
    }

    #[test]
    fn wrong_id_is_rejected() {
        let data = sample(10_000);
        let frame = frame_of(&data);
        let other = ChunkId::of(b"something else");
        assert!(matches!(
            decode_new(&other, 10_000, &frame),
            Err(ChunkError::HashMismatch)
        ));
    }

    #[test]
    fn length_is_checked_both_ways() {
        let data = sample(10_000);
        let id = ChunkId::of(&data);
        let frame = frame_of(&data);
        // Shorter content than the manifest claims.
        assert!(matches!(
            decode_new(&id, 10_001, &frame),
            Err(ChunkError::LengthMismatch {
                expected: 10_001,
                actual: 10_000
            })
        ));
        // More content than the manifest allows never fits the buffer. (Were
        // the buffer allocated larger, the length check would catch it.)
        assert!(matches!(
            decode_new(&id, 9_999, &frame),
            Err(ChunkError::Decompress(_) | ChunkError::LengthMismatch { .. })
        ));
    }

    #[test]
    fn exactly_one_standard_frame_is_accepted() {
        let data = sample(10_000);
        let id = ChunkId::of(&data);
        let frame = frame_of(&data);
        // A skippable frame: magic 0x184D2A50, a length, then anything.
        let mut skippable = vec![0x50, 0x2a, 0x4d, 0x18, 4, 0, 0, 0];
        skippable.extend_from_slice(b"evil");
        let with_trailer = [frame.as_slice(), &skippable].concat();
        let with_prefix = [skippable.as_slice(), &frame].concat();
        let half = frame_of(&data[..5_000]);
        let rest = frame_of(&data[5_000..]);
        let two_frames = [half.as_slice(), &rest].concat();
        for input in [with_trailer, with_prefix, two_frames] {
            assert!(
                matches!(
                    decode_new(&id, 10_000, &input),
                    Err(ChunkError::NotOneFrame)
                ),
                "{} bytes accepted",
                input.len()
            );
        }
        assert!(decode_new(&id, 10_000, &frame).is_ok());
    }

    #[test]
    fn oversized_frames_are_rejected_unread() {
        let frame = vec![0; compress_bound(100) + 1];
        assert!(matches!(
            decode_new(&ChunkId::of(b""), 100, &frame),
            Err(ChunkError::TooLarge { .. })
        ));
    }

    #[test]
    fn garbage_and_truncated_frames_are_rejected() {
        let data = sample(10_000);
        let id = ChunkId::of(&data);
        let frame = frame_of(&data);
        assert!(decode_new(&id, 10_000, &frame[..frame.len() - 1]).is_err());
        assert!(decode_new(&id, 10_000, &[]).is_err());
        assert!(decode_new(&id, 10_000, b"not a zstd frame").is_err());
    }
}
