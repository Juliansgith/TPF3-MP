use std::fmt;

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, SeqAccess, Visitor},
    ser::SerializeTuple,
};

/// A fixed-size byte array. serde only implements arrays up to 32 elements,
/// and this encodes as exactly `N` bytes with postcard.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FixedBytes<const N: usize>(pub [u8; N]);

impl<const N: usize> fmt::Debug for FixedBytes<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(f, &self.0)
    }
}

impl<const N: usize> Serialize for FixedBytes<N> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut tuple = serializer.serialize_tuple(N)?;
        for byte in &self.0 {
            tuple.serialize_element(byte)?;
        }
        tuple.end()
    }
}

impl<'de, const N: usize> Deserialize<'de> for FixedBytes<N> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ArrayVisitor<const N: usize>;

        impl<'de, const N: usize> Visitor<'de> for ArrayVisitor<N> {
            type Value = FixedBytes<N>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{N} bytes")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut bytes = [0; N];
                for (index, slot) in bytes.iter_mut().enumerate() {
                    *slot = seq
                        .next_element()?
                        .ok_or_else(|| de::Error::invalid_length(index, &self))?;
                }
                Ok(FixedBytes(bytes))
            }
        }

        deserializer.deserialize_tuple(N, ArrayVisitor::<N>)
    }
}

/// Largest [`Payload`], in bytes. Provisional: set from measured TPF3
/// command sizes after release.
pub const MAX_PAYLOAD: usize = 48 * 1024;

/// Opaque bytes interpreted by the game adapter, at most [`MAX_PAYLOAD`].
/// The network layer orders and relays payloads but never interprets them.
#[derive(Clone, PartialEq, Eq)]
pub struct Payload(Vec<u8>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("payload of {0} bytes exceeds the {MAX_PAYLOAD}-byte limit")]
pub struct PayloadTooLarge(pub usize);

impl Payload {
    pub fn new(bytes: Vec<u8>) -> Result<Self, PayloadTooLarge> {
        if bytes.len() > MAX_PAYLOAD {
            return Err(PayloadTooLarge(bytes.len()));
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Payload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Payload({} bytes)", self.0.len())
    }
}

impl Serialize for Payload {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for Payload {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PayloadVisitor;

        impl<'de> Visitor<'de> for PayloadVisitor {
            type Value = Payload;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "at most {MAX_PAYLOAD} bytes")
            }

            fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> Result<Payload, E> {
                if bytes.len() > MAX_PAYLOAD {
                    return Err(E::invalid_length(bytes.len(), &self));
                }
                Ok(Payload(bytes.to_vec()))
            }

            fn visit_byte_buf<E: de::Error>(self, bytes: Vec<u8>) -> Result<Payload, E> {
                if bytes.len() > MAX_PAYLOAD {
                    return Err(E::invalid_length(bytes.len(), &self));
                }
                Ok(Payload(bytes))
            }
        }

        deserializer.deserialize_bytes(PayloadVisitor)
    }
}

pub(crate) fn write_hex(f: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    for byte in bytes {
        write!(f, "{byte:02x}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_bytes_encode_without_a_length_prefix() {
        let value = FixedBytes([7u8; 64]);
        let encoded = postcard::to_stdvec(&value).unwrap();
        assert_eq!(encoded, vec![7u8; 64]);
        let decoded: FixedBytes<64> = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn fixed_bytes_reject_short_input() {
        assert!(postcard::from_bytes::<FixedBytes<32>>(&[1; 31]).is_err());
    }

    #[test]
    fn payload_round_trips_as_length_and_raw_bytes() {
        let payload = Payload::new(vec![1, 2, 3]).unwrap();
        let encoded = postcard::to_stdvec(&payload).unwrap();
        assert_eq!(encoded, vec![3, 1, 2, 3]);
        assert_eq!(postcard::from_bytes::<Payload>(&encoded).unwrap(), payload);
    }

    #[test]
    fn payload_limit_holds_both_ways() {
        assert_eq!(
            Payload::new(vec![0; MAX_PAYLOAD + 1]),
            Err(PayloadTooLarge(MAX_PAYLOAD + 1))
        );
        // A peer cannot smuggle an oversized payload past the constructor.
        let oversized = postcard::to_stdvec(&vec![0u8; MAX_PAYLOAD + 1]).unwrap();
        assert!(postcard::from_bytes::<Payload>(&oversized).is_err());
    }
}
