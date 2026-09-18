//! Round trips and the stability of chunk boundaries under edits.

use std::{collections::HashSet, fs};

use proptest::{prelude::*, sample::Index, test_runner::RngSeed};
use tpf3mp_snapshot::{ChunkId, ChunkParams, Manifest};

use crate::common::{random_bytes, repetitive_bytes, small_params, store};

fn params() -> impl Strategy<Value = ChunkParams> {
    prop_oneof![
        Just(small_params()),
        Just(ChunkParams::new(16 << 10, 32 << 10, 64 << 10).unwrap()),
        Just(ChunkParams::DEFAULT),
    ]
}

fn data() -> impl Strategy<Value = Vec<u8>> {
    (any::<u64>(), 0usize..1_200_000, any::<bool>()).prop_map(|(seed, len, repetitive)| {
        if repetitive {
            repetitive_bytes(seed, len)
        } else {
            random_bytes(seed, len)
        }
    })
}

/// Chunks `before` and `after` and checks what an edit at `at` (an offset in
/// `before`) may change: chunks that end before it must be untouched, since a
/// cut point depends only on bytes up to it, and later boundaries must
/// resynchronize so most chunks survive.
fn check_edit(before: &[u8], after: &[u8], at: usize) -> Result<(), TestCaseError> {
    let params = small_params();
    let old = Manifest::compute(before, params).unwrap();
    let new = Manifest::compute(after, params).unwrap();
    for entry in old.chunks() {
        if entry.offset + u64::from(entry.len) < at as u64 {
            prop_assert!(new.chunks().contains(entry), "chunk before the edit moved");
        }
    }
    let known: HashSet<ChunkId> = old.chunks().iter().map(|entry| entry.id).collect();
    let changed = new
        .chunks()
        .iter()
        .filter(|entry| !known.contains(&entry.id))
        .count();
    // One changed chunk is typical (97% of 3000 sampled insertions); the
    // boundaries after it resynchronize within a few more.
    prop_assert!(
        changed * 4 <= new.chunks().len(),
        "{changed} of {} chunks changed",
        new.chunks().len()
    );
    Ok(())
}

/// How far resynchronization runs is a matter of probability, with a tail
/// that thins out by roughly half per extra chunk. A fixed seed keeps these
/// cases, and so CI, deterministic; any seed passes with overwhelming
/// probability.
fn fixed_seed(cases: u32) -> ProptestConfig {
    ProptestConfig {
        cases,
        rng_seed: RngSeed::Fixed(0x7470_6633_2d6d_7031),
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn any_stream_round_trips(data in data(), params in params()) {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let manifest = store.ingest(&data[..], params).unwrap();
        prop_assert_eq!(manifest.total_size(), data.len() as u64);
        prop_assert_eq!(manifest.file_hash().0, *blake3::hash(&data).as_bytes());
        prop_assert_eq!(&Manifest::from_bytes(&manifest.to_bytes()).unwrap(), &manifest);
        prop_assert_eq!(&Manifest::compute(&data[..], params).unwrap(), &manifest);
        let dest = dir.path().join("restored.sav");
        store.assemble(&manifest, &dest).unwrap();
        prop_assert!(fs::read(&dest).unwrap() == data);
    }
}

proptest! {
    #![proptest_config(fixed_seed(32))]

    #[test]
    fn boundaries_survive_insertion(
        seed: u64,
        len in 1_500_000usize..2_500_000,
        at: Index,
        inserted in 1usize..4096,
    ) {
        let before = random_bytes(seed, len);
        let at = at.index(len);
        let mut after = before[..at].to_vec();
        after.extend(random_bytes(!seed, inserted));
        after.extend_from_slice(&before[at..]);
        check_edit(&before, &after, at)?;
    }

    #[test]
    fn boundaries_survive_deletion(
        seed: u64,
        len in 1_500_000usize..2_500_000,
        at: Index,
        removed in 1usize..4096,
    ) {
        let before = random_bytes(seed, len);
        let at = at.index(len - removed);
        let mut after = before[..at].to_vec();
        after.extend_from_slice(&before[at + removed..]);
        check_edit(&before, &after, at)?;
    }

    #[test]
    fn boundaries_survive_overwrites(
        seed: u64,
        len in 1_500_000usize..2_500_000,
        at: Index,
        changed in 1usize..64,
    ) {
        let before = random_bytes(seed, len);
        let at = at.index(len - changed);
        let mut after = before.clone();
        for (offset, byte) in after[at..at + changed].iter_mut().enumerate() {
            *byte ^= 1 + (offset % 255) as u8;
        }
        check_edit(&before, &after, at)?;
    }
}

#[test]
fn repeated_content_is_stored_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let block = random_bytes(1, 1_000_000);
    let mut data = block.clone();
    data.extend_from_slice(&block);
    let manifest = store.ingest(&data[..], small_params()).unwrap();
    let unique = manifest.unique_chunks();
    assert!(unique.len() < manifest.chunks().len());
    // The second copy reuses the first copy's chunks once its boundaries
    // line up again, typically after the chunk that spans the seam.
    let unique_bytes: u64 = unique.iter().map(|entry| u64::from(entry.len)).sum();
    let max = u64::from(small_params().max());
    assert!(
        unique_bytes <= 1_000_000 + 2 * max,
        "{unique_bytes} distinct bytes"
    );
    assert_eq!(store.missing(&manifest).unwrap(), []);
    // Long runs of one byte value cut into identical maximum-size chunks.
    let zeros = store
        .ingest(&vec![0; 3_000_000][..], small_params())
        .unwrap();
    assert_eq!(zeros.unique_chunks().len(), 2);
}
