use thiserror::Error;

/// No manifest may declare a minimum chunk size below this. It bounds how many
/// chunks a file can have ([`MAX_CHUNKS`](crate::MAX_CHUNKS)).
pub const MIN_CHUNK_FLOOR: u32 = 16 << 10;

/// No manifest may declare a maximum chunk size above this, so no chunk is ever
/// longer. It bounds the memory one chunk takes and the size of a chunk on the
/// wire ([`MAX_COMPRESSED_CHUNK_LEN`](crate::MAX_COMPRESSED_CHUNK_LEN)).
pub const MAX_CHUNK_LEN: u32 = 4 << 20;

/// FastCDC chunk-size bounds in bytes, as recorded in every manifest.
///
/// Cut points also depend on the algorithm, which the manifest version fixes:
/// FastCDC 2020 with normalization level 1 and the unseeded gear table. So two
/// parties that chunk the same bytes with the same parameters get the same
/// chunks on every platform, which is what lets a receiver reuse the chunks of
/// a file it already has.
///
/// All three sizes are powers of two and strictly increase. Every chunk except
/// the last is between `min` and `max` bytes long; the last is between 1 and
/// `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkParams {
    min: u32,
    avg: u32,
    max: u32,
}

impl ChunkParams {
    /// The parameters the server chunks saves with: 32 KiB / 128 KiB / 512 KiB.
    /// `docs/SNAPSHOTS.md` has the measurements behind them. Receivers accept
    /// any valid parameters, so this can change without a protocol change.
    pub const DEFAULT: Self = Self {
        min: 32 << 10,
        avg: 128 << 10,
        max: 512 << 10,
    };

    pub fn new(min: u32, avg: u32, max: u32) -> Result<Self, ParamsError> {
        if !(min.is_power_of_two() && avg.is_power_of_two() && max.is_power_of_two()) {
            return Err(ParamsError::NotPowersOfTwo { min, avg, max });
        }
        if !(min < avg && avg < max) {
            return Err(ParamsError::NotIncreasing { min, avg, max });
        }
        if min < MIN_CHUNK_FLOOR {
            return Err(ParamsError::MinTooSmall { min });
        }
        if max > MAX_CHUNK_LEN {
            return Err(ParamsError::MaxTooLarge { max });
        }
        Ok(Self { min, avg, max })
    }

    pub fn min(self) -> u32 {
        self.min
    }

    pub fn avg(self) -> u32 {
        self.avg
    }

    pub fn max(self) -> u32 {
        self.max
    }

    /// The most chunks a file of `len` bytes can have, given that every chunk
    /// but the last is at least `min` bytes long.
    pub(crate) fn max_chunks(self, len: u64) -> u64 {
        len.div_ceil(u64::from(self.min))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ParamsError {
    #[error("chunk sizes {min}/{avg}/{max} are not all powers of two")]
    NotPowersOfTwo { min: u32, avg: u32, max: u32 },
    #[error("chunk sizes {min}/{avg}/{max} do not strictly increase")]
    NotIncreasing { min: u32, avg: u32, max: u32 },
    #[error("minimum chunk size {min} is below the {floor}-byte floor", floor = MIN_CHUNK_FLOOR)]
    MinTooSmall { min: u32 },
    #[error("maximum chunk size {max} is above the {ceiling}-byte ceiling", ceiling = MAX_CHUNK_LEN)]
    MaxTooLarge { max: u32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_parameters_are_valid() {
        let default = ChunkParams::DEFAULT;
        assert_eq!(
            ChunkParams::new(default.min(), default.avg(), default.max()),
            Ok(default)
        );
    }

    #[test]
    fn the_extremes_are_valid() {
        assert!(ChunkParams::new(MIN_CHUNK_FLOOR, MIN_CHUNK_FLOOR * 2, MAX_CHUNK_LEN).is_ok());
    }

    #[test]
    fn invalid_parameters_are_rejected() {
        let k = 1 << 10;
        assert_eq!(
            ChunkParams::new(64 * k, 200 * k, 1024 * k),
            Err(ParamsError::NotPowersOfTwo {
                min: 64 * k,
                avg: 200 * k,
                max: 1024 * k
            })
        );
        assert_eq!(
            ChunkParams::new(0, 256 * k, 1024 * k),
            Err(ParamsError::NotPowersOfTwo {
                min: 0,
                avg: 256 * k,
                max: 1024 * k
            })
        );
        assert_eq!(
            ChunkParams::new(256 * k, 256 * k, 1024 * k),
            Err(ParamsError::NotIncreasing {
                min: 256 * k,
                avg: 256 * k,
                max: 1024 * k
            })
        );
        assert_eq!(
            ChunkParams::new(64 * k, 1024 * k, 256 * k),
            Err(ParamsError::NotIncreasing {
                min: 64 * k,
                avg: 1024 * k,
                max: 256 * k
            })
        );
        assert_eq!(
            ChunkParams::new(8 * k, 32 * k, 128 * k),
            Err(ParamsError::MinTooSmall { min: 8 * k })
        );
        assert_eq!(
            ChunkParams::new(1024 * k, 2048 * k, 8192 * k),
            Err(ParamsError::MaxTooLarge { max: 8192 * k })
        );
    }

    #[test]
    fn chunk_count_bound_follows_the_minimum() {
        let params = ChunkParams::DEFAULT;
        let min = u64::from(params.min());
        assert_eq!(params.max_chunks(0), 0);
        assert_eq!(params.max_chunks(1), 1);
        assert_eq!(params.max_chunks(min), 1);
        assert_eq!(params.max_chunks(min + 1), 2);
        assert_eq!(params.max_chunks(10 * min), 10);
    }
}
