use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! blake3_name {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(pub [u8; 32]);

        impl $name {
            /// Parses exactly 64 lowercase hex digits, the only form
            /// [`Display`](fmt::Display) writes. Anything else is rejected, so
            /// a parsed name always maps back to the same file name.
            pub fn from_hex(text: &str) -> Option<Self> {
                parse_hex(text).map(Self)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write_hex(&self.0, f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({self})", stringify!($name))
            }
        }
    };
}

blake3_name!(
    /// The BLAKE3 hash of a chunk's uncompressed bytes. It names the chunk in
    /// manifests, on disk and on the wire.
    ChunkId
);

blake3_name!(
    /// The BLAKE3 hash of a whole file.
    FileHash
);

blake3_name!(
    /// The BLAKE3 hash of a manifest's canonical encoding. It names a
    /// snapshot, for example the transfer state a [`ChunkSink`](crate::ChunkSink)
    /// keeps.
    ManifestId
);

impl ChunkId {
    pub fn of(data: &[u8]) -> Self {
        Self(*blake3::hash(data).as_bytes())
    }
}

fn write_hex(bytes: &[u8], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    for byte in bytes {
        write!(f, "{byte:02x}")?;
    }
    Ok(())
}

fn parse_hex(text: &str) -> Option<[u8; 32]> {
    let digits = text.as_bytes();
    if digits.len() != 64 {
        return None;
    }
    let mut bytes = [0; 32];
    let (pairs, _) = digits.as_chunks::<2>();
    for (byte, [high, low]) in bytes.iter_mut().zip(pairs) {
        *byte = (hex_value(*high)? << 4) | hex_value(*low)?;
    }
    Some(bytes)
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let mut bytes = [0; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::try_from(index * 8).unwrap();
        }
        let id = ChunkId(bytes);
        let text = id.to_string();
        assert_eq!(text.len(), 64);
        assert!(text.starts_with("00081018"));
        assert_eq!(ChunkId::from_hex(&text), Some(id));
    }

    #[test]
    fn parsing_is_strict() {
        let valid = "ab".repeat(32);
        assert!(ChunkId::from_hex(&valid).is_some());
        for invalid in [
            String::new(),
            "ab".repeat(31),
            "ab".repeat(33),
            "AB".repeat(32),
            format!("{}g0", "ab".repeat(31)),
            format!("../{}", "ab".repeat(30)),
            format!("{}é", "a".repeat(62)),
        ] {
            assert_eq!(ChunkId::from_hex(&invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn chunk_id_is_the_blake3_hash() {
        // BLAKE3 of the empty input, from the reference test vectors.
        assert_eq!(
            ChunkId::of(b"").to_string(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }
}
