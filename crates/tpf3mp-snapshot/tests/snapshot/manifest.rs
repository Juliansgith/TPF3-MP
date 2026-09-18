//! Manifests from hostile peers: any input either decodes to a valid,
//! canonical manifest or is rejected, and never panics.

use std::sync::LazyLock;

use proptest::{collection::vec, prelude::*, sample::Index};
use tpf3mp_snapshot::{ChunkEntry, ChunkId, FileHash, MAX_CHUNK_LEN, Manifest, ManifestError};

use crate::common::{encode_manifest, random_bytes, small_params};

/// A decoded manifest must re-encode to exactly the input, and satisfy the
/// layout rules a well-behaved receiver relies on.
fn check_outcome(bytes: &[u8]) -> Result<(), TestCaseError> {
    if let Ok(manifest) = Manifest::from_bytes(bytes) {
        prop_assert_eq!(manifest.to_bytes(), bytes);
        let mut offset = 0;
        for entry in manifest.chunks() {
            prop_assert_eq!(entry.offset, offset);
            prop_assert!(entry.len >= 1 && entry.len <= manifest.params().max());
            prop_assert!(entry.len <= MAX_CHUNK_LEN);
            offset += u64::from(entry.len);
        }
        prop_assert_eq!(offset, manifest.total_size());
    }
    Ok(())
}

fn valid_manifest() -> Vec<u8> {
    static VALID: LazyLock<Vec<u8>> = LazyLock::new(|| {
        let data = random_bytes(7, 400_000);
        Manifest::compute(&data[..], small_params())
            .unwrap()
            .to_bytes()
    });
    VALID.clone()
}

#[derive(Debug, Clone)]
enum Mutation {
    Flip(Index, u8),
    Insert(Index, u8),
    Remove(Index),
    Truncate(Index),
}

fn mutation() -> impl Strategy<Value = Mutation> {
    prop_oneof![
        (any::<Index>(), 1..=255u8).prop_map(|(at, mask)| Mutation::Flip(at, mask)),
        (any::<Index>(), any::<u8>()).prop_map(|(at, byte)| Mutation::Insert(at, byte)),
        any::<Index>().prop_map(Mutation::Remove),
        any::<Index>().prop_map(Mutation::Truncate),
    ]
}

fn apply(bytes: &mut Vec<u8>, mutation: &Mutation) {
    if bytes.is_empty() {
        return;
    }
    match mutation {
        Mutation::Flip(at, mask) => {
            let at = at.index(bytes.len());
            bytes[at] ^= mask;
        }
        Mutation::Insert(at, byte) => {
            let at = at.index(bytes.len() + 1);
            bytes.insert(at, *byte);
        }
        Mutation::Remove(at) => {
            let at = at.index(bytes.len());
            bytes.remove(at);
        }
        Mutation::Truncate(at) => {
            let at = at.index(bytes.len());
            bytes.truncate(at);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn arbitrary_bytes_are_rejected_or_canonical(bytes in vec(any::<u8>(), 0..600)) {
        check_outcome(&bytes)?;
    }

    #[test]
    fn arbitrary_bytes_after_a_valid_header_are_handled(tail in vec(any::<u8>(), 0..600)) {
        let mut bytes = valid_manifest();
        bytes.truncate(4 + 1 + 9);
        bytes.extend_from_slice(&tail);
        check_outcome(&bytes)?;
    }

    #[test]
    fn mutated_manifests_are_rejected_or_canonical(mutations in vec(mutation(), 1..4)) {
        let mut bytes = valid_manifest();
        for mutation in &mutations {
            apply(&mut bytes, mutation);
        }
        check_outcome(&bytes)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Well-formed field values that break the layout rules in arbitrary
    /// ways: the parser must name a rule, not accept the manifest.
    #[test]
    fn structurally_valid_but_inconsistent_manifests_are_rejected(
        lens in vec(0u32..300_000, 1..12),
        skew in vec(0u64..3, 1..12),
        total_delta in -2i64..3,
        duplicate in any::<bool>(),
    ) {
        let mut offset = 0u64;
        let mut chunks = Vec::new();
        for (index, len) in lens.iter().enumerate() {
            let id = if duplicate && index > 0 { ChunkId([1; 32]) } else { ChunkId([index as u8 + 1; 32]) };
            let skew = skew.get(index).copied().unwrap_or(0);
            chunks.push(ChunkEntry { id, offset: offset + skew, len: *len });
            offset += u64::from(*len);
        }
        let total = offset.saturating_add_signed(total_delta);
        let bytes = encode_manifest([16 << 10, 64 << 10, 256 << 10], total, FileHash([0; 32]), &chunks);
        match Manifest::from_bytes(&bytes) {
            Ok(manifest) => {
                // Only possible when every rule happens to hold.
                prop_assert_eq!(manifest.to_bytes(), bytes);
                prop_assert_eq!(manifest.total_size(), offset);
                prop_assert!(skew.iter().take(lens.len()).all(|&s| s == 0));
            }
            Err(error) => prop_assert!(!matches!(
                error,
                ManifestError::TooLarge { .. } | ManifestError::BadMagic | ManifestError::UnsupportedVersion { .. }
            ), "{error}"),
        }
    }
}
