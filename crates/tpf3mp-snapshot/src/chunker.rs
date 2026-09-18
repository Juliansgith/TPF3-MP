use std::io::{self, Read};

use fastcdc::v2020::{self, Normalization, StreamCDC};
use thiserror::Error;

use crate::{ChunkId, ChunkParams, FileHash, MAX_FILE_SIZE};

/// One content-defined chunk of a stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Position of the chunk's first byte in the stream.
    pub offset: u64,
    pub id: ChunkId,
    pub data: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("reading the source failed: {0}")]
    Read(#[from] io::Error),
    #[error("the source exceeds the {limit}-byte limit")]
    TooLarge { limit: u64 },
}

/// Cuts a stream into content-defined chunks and hashes each chunk and the
/// whole stream.
///
/// It reads through a buffer of one maximum-size chunk, so memory use does
/// not depend on the size of the stream.
pub struct Chunker<R: Read> {
    cdc: StreamCDC<RetryInterrupted<R>>,
    file_hash: blake3::Hasher,
    len: u64,
    limit: u64,
}

impl<R: Read> Chunker<R> {
    pub fn new(source: R, params: ChunkParams) -> Self {
        Self::with_limit(source, params, MAX_FILE_SIZE)
    }

    /// Lets tests exercise the size limit without gigabytes of input.
    pub(crate) fn with_limit(source: R, params: ChunkParams, limit: u64) -> Self {
        // `ChunkParams` guarantees what fastcdc only debug-asserts: even sizes
        // within its supported ranges. The normalization level is spelled out
        // because it is part of the format.
        let cdc = StreamCDC::with_level(
            RetryInterrupted(source),
            params.min() as usize,
            params.avg() as usize,
            params.max() as usize,
            Normalization::Level1,
        );
        Self {
            cdc,
            file_hash: blake3::Hasher::new(),
            len: 0,
            limit,
        }
    }

    /// The next chunk, or `None` at the end of the stream.
    pub fn next_chunk(&mut self) -> Result<Option<Chunk>, SourceError> {
        let Some(next) = self.cdc.next() else {
            return Ok(None);
        };
        let chunk = next.map_err(|error| match error {
            v2020::Error::IoError(error) => SourceError::Read(error),
            other => SourceError::Read(io::Error::other(other.to_string())),
        })?;
        let offset = self.len;
        self.len += chunk.data.len() as u64;
        if self.len > self.limit {
            return Err(SourceError::TooLarge { limit: self.limit });
        }
        self.file_hash.update(&chunk.data);
        Ok(Some(Chunk {
            offset,
            id: ChunkId::of(&chunk.data),
            data: chunk.data,
        }))
    }

    /// Reads the rest of the stream and returns its length and hash.
    pub fn finish(mut self) -> Result<(u64, FileHash), SourceError> {
        while self.next_chunk()?.is_some() {}
        Ok((self.len, FileHash(*self.file_hash.finalize().as_bytes())))
    }
}

/// Retries reads the OS interrupted: fastcdc treats any read error as fatal,
/// and a signal arriving mid-read must not abort a snapshot.
struct RetryInterrupted<R>(R);

impl<R: Read> Read for RetryInterrupted<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.0.read(buf) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SplitMix64: fast, deterministic test data without a dependency.
    fn random_bytes(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed;
        let mut data = Vec::with_capacity(len + 8);
        while data.len() < len {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            data.extend_from_slice(&(z ^ (z >> 31)).to_le_bytes());
        }
        data.truncate(len);
        data
    }

    fn small_params() -> ChunkParams {
        ChunkParams::new(16 << 10, 32 << 10, 64 << 10).unwrap()
    }

    fn chunks_of(source: impl Read, params: ChunkParams) -> (Vec<Chunk>, u64, FileHash) {
        let mut chunker = Chunker::new(source, params);
        let mut chunks = Vec::new();
        while let Some(chunk) = chunker.next_chunk().unwrap() {
            chunks.push(chunk);
        }
        let (len, hash) = chunker.finish().unwrap();
        (chunks, len, hash)
    }

    /// Hands out at most a few bytes per read, and interrupts every other read.
    struct Trickle<'a> {
        data: &'a [u8],
        calls: usize,
    }

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.calls.is_multiple_of(2) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let len = buf.len().min(self.data.len()).min(1 + self.calls % 7);
            buf[..len].copy_from_slice(&self.data[..len]);
            self.data = &self.data[len..];
            Ok(len)
        }
    }

    #[test]
    fn empty_stream_has_no_chunks() {
        let (chunks, len, hash) = chunks_of(&[][..], small_params());
        assert!(chunks.is_empty());
        assert_eq!(len, 0);
        assert_eq!(hash.0, *blake3::hash(b"").as_bytes());
    }

    #[test]
    fn chunks_tile_the_stream_within_bounds() {
        let data = random_bytes(1, 700_000);
        let params = small_params();
        let (chunks, len, hash) = chunks_of(&data[..], params);
        assert_eq!(len, 700_000);
        assert_eq!(hash.0, *blake3::hash(&data).as_bytes());
        let mut offset = 0;
        for (index, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.offset, offset);
            assert_eq!(chunk.id, ChunkId::of(&chunk.data));
            let chunk_len = u32::try_from(chunk.data.len()).unwrap();
            assert!(chunk_len <= params.max());
            if index + 1 < chunks.len() {
                assert!(chunk_len >= params.min());
            }
            offset += chunk.data.len() as u64;
        }
        assert_eq!(offset, 700_000);
        // Random data cuts close to the average size.
        assert!((10..=40).contains(&chunks.len()), "{} chunks", chunks.len());
    }

    #[test]
    fn short_reads_and_interruptions_do_not_change_the_chunks() {
        let data = random_bytes(2, 300_000);
        let whole = chunks_of(&data[..], small_params());
        let trickled = chunks_of(
            Trickle {
                data: &data,
                calls: 0,
            },
            small_params(),
        );
        assert_eq!(whole, trickled);
    }

    #[test]
    fn uniform_data_is_cut_at_the_maximum() {
        let params = small_params();
        let data = vec![0; 200_000];
        let (chunks, _, _) = chunks_of(&data[..], params);
        let max = params.max() as usize;
        assert_eq!(chunks.len(), 4);
        assert!(chunks[..3].iter().all(|chunk| chunk.data.len() == max));
        assert_eq!(chunks[3].data.len(), 200_000 - 3 * max);
        assert_eq!(chunks[0].id, chunks[1].id);
    }

    #[test]
    fn size_limit_is_enforced() {
        let data = random_bytes(3, 100_000);
        let mut chunker = Chunker::with_limit(&data[..], small_params(), 99_999);
        let error = loop {
            match chunker.next_chunk() {
                Ok(Some(_)) => {}
                Ok(None) => panic!("the limit was not enforced"),
                Err(error) => break error,
            }
        };
        assert!(matches!(error, SourceError::TooLarge { limit: 99_999 }));
        let exact = Chunker::with_limit(&data[..], small_params(), 100_000);
        assert_eq!(exact.finish().unwrap().0, 100_000);
    }

    #[test]
    fn read_errors_surface() {
        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("disk on fire"))
            }
        }
        let error = Chunker::new(Failing, small_params()).finish().unwrap_err();
        assert!(matches!(error, SourceError::Read(_)), "{error}");
    }

    /// Cut points are part of the format: a receiver can only reuse a local
    /// file's chunks if it cuts exactly where the server did. A fastcdc
    /// upgrade that moves them must fail here, not silently halve dedup.
    #[test]
    fn cut_points_are_frozen() {
        let data = random_bytes(0x7470_6633, 3 << 20);
        let params = ChunkParams::new(64 << 10, 256 << 10, 1 << 20).unwrap();
        let (chunks, _, hash) = chunks_of(&data[..], params);
        let lengths: Vec<usize> = chunks.iter().map(|chunk| chunk.data.len()).collect();
        assert_eq!(
            lengths,
            [
                215_000, 369_383, 269_042, 170_752, 112_420, 282_605, 487_276, 263_524, 184_645,
                474_386, 73_940, 242_755,
            ]
        );
        // Pins the input too, so a change to the generator is not mistaken
        // for a change to the chunker.
        assert_eq!(
            hash.to_string(),
            "583b750bc2c39e74d2bb357c130fb83c314f5036312f9a3176f74886e82eab56"
        );
    }
}
