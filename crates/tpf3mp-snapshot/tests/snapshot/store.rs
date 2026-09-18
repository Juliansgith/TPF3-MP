//! The chunk store: puts, reads, integrity, garbage collection and limits.

use std::{collections::BTreeSet, fs, path::Path, sync::LazyLock, thread};

use proptest::{prelude::*, sample::Index};
use tpf3mp_snapshot::{
    ChunkEntry, ChunkError, ChunkId, ChunkParams, ChunkStore, FileHash, Manifest, StoreConfig,
    StoreError,
};

use crate::common::{
    chunk_file, chunk_files, encode_manifest, random_bytes, repetitive_bytes, small_params, store,
    store_with,
};

fn ingest(store: &ChunkStore, data: &[u8]) -> Manifest {
    store.ingest(data, small_params()).unwrap()
}

fn ids(manifest: &Manifest) -> BTreeSet<ChunkId> {
    manifest.chunks().iter().map(|entry| entry.id).collect()
}

/// The first chunk of a manifest, and its stored file.
fn first_chunk(root: &Path, manifest: &Manifest) -> (ChunkEntry, std::path::PathBuf) {
    let entry = manifest.chunks()[0];
    (entry, chunk_file(root, &entry.id))
}

#[test]
fn ingest_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let data = random_bytes(1, 900_000);
    let first = ingest(&store, &data);
    let used = store.used_bytes();
    assert!(used > 0);
    let second = ingest(&store, &data);
    assert_eq!(first, second);
    assert_eq!(store.used_bytes(), used);
    assert_eq!(chunk_files(dir.path()), ids(&first));
}

#[test]
fn concurrent_ingests_store_each_chunk_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let data = random_bytes(2, 1_500_000);
    let manifests: Vec<Manifest> = thread::scope(|scope| {
        let workers: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| ingest(&store.clone(), &data)))
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect()
    });
    assert!(manifests.windows(2).all(|pair| pair[0] == pair[1]));
    let on_disk: u64 = chunk_files(dir.path())
        .iter()
        .map(|id| fs::metadata(chunk_file(dir.path(), id)).unwrap().len())
        .sum();
    assert_eq!(store.used_bytes(), on_disk);
    assert!(
        fs::read_dir(dir.path().join("tmp"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn has_and_missing_track_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let data = random_bytes(3, 600_000);
    let manifest = Manifest::compute(&data[..], small_params()).unwrap();
    assert_eq!(store.missing(&manifest).unwrap(), manifest.unique_chunks());
    assert!(!store.has(&manifest.chunks()[0].id).unwrap());
    ingest(&store, &data[..300_000]);
    let missing = store.missing(&manifest).unwrap();
    assert!(!missing.is_empty() && missing.len() < manifest.chunks().len());
    ingest(&store, &data);
    assert_eq!(store.missing(&manifest).unwrap(), []);
    assert!(store.has(&manifest.chunks()[0].id).unwrap());
}

#[test]
fn reads_return_verified_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let data = random_bytes(4, 500_000);
    let manifest = ingest(&store, &data);
    for entry in manifest.chunks() {
        let start = entry.offset as usize;
        let expected = &data[start..start + entry.len as usize];
        assert_eq!(store.read(&entry.id).unwrap(), expected);
        let frame = store.read_compressed(&entry.id).unwrap();
        let raw = zstd::bulk::decompress(&frame, entry.len as usize).unwrap();
        assert_eq!(raw, expected);
    }
    let absent = ChunkId([0xee; 32]);
    assert!(matches!(store.read(&absent), Err(StoreError::NotFound(id)) if id == absent));
}

#[test]
fn flipped_byte_fails_the_read_and_removes_the_chunk() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let manifest = ingest(&store, &random_bytes(5, 400_000));
    let used = store.used_bytes();
    let (entry, path) = first_chunk(dir.path(), &manifest);
    let mut file = fs::read(&path).unwrap();
    let middle = file.len() / 2;
    file[middle] ^= 0x01;
    fs::write(&path, &file).unwrap();

    let error = store.read(&entry.id).unwrap_err();
    assert!(
        matches!(error, StoreError::Corrupt { id, .. } if id == entry.id),
        "{error}"
    );
    assert!(!path.exists());
    assert!(!store.has(&entry.id).unwrap());
    assert_eq!(store.missing(&manifest).unwrap()[0].id, entry.id);
    assert_eq!(store.used_bytes(), used - file.len() as u64);
}

#[test]
fn damaged_chunk_files_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let manifest = ingest(&store, &random_bytes(6, 400_000));
    let (entry, path) = first_chunk(dir.path(), &manifest);
    let original = fs::read(&path).unwrap();
    let mut huge_length = original.clone();
    huge_length[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut zero_length = original.clone();
    zero_length[4..8].copy_from_slice(&0u32.to_le_bytes());
    let mut foreign = original.clone();
    foreign[..4].copy_from_slice(b"ZZZZ");
    type Expected = fn(&ChunkError) -> bool;
    let damaged: [(Vec<u8>, Expected); 7] = [
        (Vec::new(), |e| matches!(e, ChunkError::BadFile(_))),
        (original[..6].to_vec(), |e| {
            matches!(e, ChunkError::BadFile(_))
        }),
        (original[..8].to_vec(), |e| {
            matches!(e, ChunkError::NotOneFrame)
        }),
        (original[..original.len() - 1].to_vec(), |e| {
            matches!(e, ChunkError::NotOneFrame)
        }),
        (huge_length, |e| matches!(e, ChunkError::BadFile(_))),
        (zero_length, |e| matches!(e, ChunkError::BadFile(_))),
        (foreign, |e| matches!(e, ChunkError::BadFile(_))),
    ];
    for (bytes, expected) in damaged {
        fs::write(&path, &bytes).unwrap();
        match store.read_compressed(&entry.id) {
            Err(StoreError::Corrupt { reason, .. }) => assert!(expected(&reason), "{reason}"),
            other => panic!("damage went unnoticed: {other:?}"),
        }
        assert!(!path.exists());
    }
}

#[test]
fn another_chunk_under_the_wrong_name_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let manifest = ingest(&store, &random_bytes(7, 400_000));
    let [first, second, ..] = manifest.chunks() else {
        panic!("expected several chunks");
    };
    fs::copy(
        chunk_file(dir.path(), &second.id),
        chunk_file(dir.path(), &first.id),
    )
    .unwrap();
    assert!(matches!(
        store.read(&first.id),
        Err(StoreError::Corrupt {
            reason: ChunkError::HashMismatch
                | ChunkError::LengthMismatch { .. }
                | ChunkError::Decompress(_),
            ..
        })
    ));
}

/// One stored chunk of compressible data: its entry, its content and its
/// chunk file, made once for all cases below.
fn sample_chunk_file() -> &'static (ChunkEntry, Vec<u8>, Vec<u8>) {
    static SAMPLE: LazyLock<(ChunkEntry, Vec<u8>, Vec<u8>)> = LazyLock::new(|| {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let data = repetitive_bytes(8, 200_000);
        let (entry, path) = first_chunk(dir.path(), &ingest(&store, &data));
        let raw = data[..entry.len as usize].to_vec();
        (entry, raw, fs::read(path).unwrap())
    });
    &SAMPLE
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Whatever byte is damaged, a read either fails or returns exactly the
    /// original chunk. (zstd ignores a few header bits, so not every flip
    /// changes the content.)
    #[test]
    fn damaged_chunks_never_read_back_wrong(at: Index, mask in 1..=255u8) {
        let (entry, raw, file) = sample_chunk_file();
        let dir = tempfile::tempdir().unwrap();
        let path = chunk_file(dir.path(), &entry.id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut damaged = file.clone();
        damaged[at.index(file.len())] ^= mask;
        fs::write(&path, &damaged).unwrap();
        let store = store(dir.path());
        match store.read(&entry.id) {
            Ok(read) => prop_assert!(&read == raw),
            Err(StoreError::Corrupt { id, .. }) => prop_assert_eq!(id, entry.id),
            Err(other) => prop_assert!(false, "unexpected error: {other}"),
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Garbage collection keeps every chunk of the live and the retained
    /// manifests, and nothing else.
    #[test]
    fn gc_keeps_exactly_the_live_chunks(
        seeds in proptest::collection::vec(any::<u64>(), 1..5),
        live_mask: u8,
        retain_mask: u8,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        // Files that share a prefix, so chunks are shared between manifests,
        // and end in their own index, so no two are the same snapshot.
        let base = repetitive_bytes(9, 200_000);
        let manifests: Vec<Manifest> = seeds
            .iter()
            .enumerate()
            .map(|(index, &seed)| {
                let mut data = base.clone();
                data.extend(random_bytes(seed, 20_000 + (seed % 150_000) as usize));
                data.push(index as u8);
                ingest(&store, &data)
            })
            .collect();
        let chosen = |mask: u8| {
            manifests
                .iter()
                .enumerate()
                .filter(move |(index, _)| mask & (1 << index) != 0)
                .map(|(_, manifest)| manifest)
        };
        // Ingest retains every snapshot; release the ones this case does not.
        for manifest in chosen(!retain_mask) {
            store.release(&manifest.id()).unwrap();
        }
        let expected: BTreeSet<ChunkId> = chosen(live_mask | retain_mask).flat_map(ids).collect();
        let before = chunk_files(dir.path());
        let stats = store.gc(chosen(live_mask)).unwrap();
        let after = chunk_files(dir.path());
        prop_assert_eq!(&after, &expected);
        prop_assert_eq!(stats.kept_chunks as usize, expected.len());
        prop_assert_eq!(stats.removed_chunks as usize, before.len() - expected.len());
        prop_assert_eq!(store.used_bytes(), stats.kept_bytes);
        for manifest in chosen(live_mask | retain_mask) {
            prop_assert_eq!(store.missing(manifest).unwrap(), []);
        }
    }
}

#[test]
fn ingested_snapshots_are_retained_until_released() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let data = random_bytes(21, 400_000);
    let manifest = ingest(&store, &data);
    assert_eq!(store.retained().unwrap(), [manifest.id()]);
    assert_eq!(store.manifest(&manifest.id()).unwrap(), manifest);
    // A collection with nothing live keeps what is retained.
    let stats = store.gc([]).unwrap();
    assert_eq!(stats.removed_chunks, 0);
    assert_eq!(store.missing(&manifest).unwrap(), []);
    // After a restart, the store still knows its snapshots.
    drop(store);
    let store = self::store(dir.path());
    assert_eq!(store.retained().unwrap(), [manifest.id()]);
    store.release(&manifest.id()).unwrap();
    assert!(store.retained().unwrap().is_empty());
    assert!(matches!(
        store.manifest(&manifest.id()),
        Err(StoreError::UnknownSnapshot(id)) if id == manifest.id()
    ));
    store.gc([]).unwrap();
    assert!(chunk_files(dir.path()).is_empty());
}

#[test]
fn chunk_files_too_short_to_be_chunks_count_as_missing() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let data = random_bytes(22, 400_000);
    let manifest = ingest(&store, &data);
    let id = manifest.chunks()[1].id;
    // What a power failure without flushing can leave behind.
    fs::write(chunk_file(dir.path(), &id), b"").unwrap();
    assert!(!store.has(&id).unwrap());
    assert_eq!(store.missing(&manifest).unwrap()[0].id, id);
    // Storing the chunk again replaces the empty file.
    ingest(&store, &data);
    assert!(store.has(&id).unwrap());
    let dest = dir.path().join("out.sav");
    store.assemble(&manifest, &dest).unwrap();
    assert!(fs::read(&dest).unwrap() == data);
}

#[test]
fn gc_with_nothing_live_empties_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let manifest = ingest(&store, &random_bytes(10, 500_000));
    store.release(&manifest.id()).unwrap();
    fs::write(dir.path().join("tmp").join("leftover"), b"partial write").unwrap();
    let stats = store.gc([]).unwrap();
    assert!(stats.removed_chunks > 0);
    assert_eq!(stats.kept_chunks, 0);
    assert_eq!(store.used_bytes(), 0);
    assert!(chunk_files(dir.path()).is_empty());
    assert!(
        fs::read_dir(dir.path().join("tmp"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn size_bound_is_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let limit = 300_000;
    let store = store_with(dir.path(), |config| config.max_bytes = limit);
    // Random data does not compress, so it cannot fit.
    let error = store
        .ingest(&random_bytes(11, 1_000_000)[..], small_params())
        .unwrap_err();
    assert!(
        matches!(error, StoreError::Full { limit: l, used, needed } if l == limit && used + needed > limit),
        "{error}"
    );
    assert!(store.used_bytes() <= limit);
    // Collecting makes room again.
    store.gc([]).unwrap();
    assert_eq!(store.used_bytes(), 0);
    let small = ingest(&store, &random_bytes(12, 100_000));
    assert_eq!(store.missing(&small).unwrap(), []);
}

#[test]
fn a_second_process_cannot_open_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let first = store(dir.path());
    let clone = first.clone();
    let error = ChunkStore::open(dir.path(), StoreConfig::new(u64::MAX)).unwrap_err();
    assert!(matches!(error, StoreError::Locked(_)), "{error}");
    drop(first);
    // Clones share the lock.
    assert!(ChunkStore::open(dir.path(), StoreConfig::new(u64::MAX)).is_err());
    drop(clone);
    ChunkStore::open(dir.path(), StoreConfig::new(u64::MAX)).unwrap();
}

#[test]
fn reopening_counts_chunks_and_clears_temporary_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let manifest = ingest(&store, &random_bytes(13, 700_000));
    let used = store.used_bytes();
    drop(store);
    // What a crash between writing and renaming leaves behind.
    let leftover = dir.path().join("tmp").join("0000000000000007");
    fs::write(&leftover, b"half a chunk").unwrap();
    let store = self::store(dir.path());
    assert!(!leftover.exists());
    assert_eq!(store.used_bytes(), used);
    assert_eq!(store.missing(&manifest).unwrap(), []);
}

#[test]
fn files_the_store_did_not_write_are_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let manifest = ingest(&store, &random_bytes(14, 300_000));
    let used = store.used_bytes();
    let id = manifest.chunks()[0].id;
    let fan = dir.path().join("chunks").join("ab");
    let strangers = [
        fan.join("notes.txt"),
        fan.join("AB".repeat(32)),
        // A valid name in the wrong fan-out directory.
        dir.path()
            .join("chunks")
            .join(if id.0[0] == 0 { "01" } else { "00" })
            .join(id.to_string()),
    ];
    for path in &strangers {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"not ours").unwrap();
    }
    // Neither is a directory that does not look like a fan-out directory.
    let odd = dir.path().join("chunks").join("zz");
    fs::create_dir_all(&odd).unwrap();
    fs::write(odd.join(id.to_string()), b"not ours").unwrap();
    drop(store);
    let store = self::store(dir.path());
    assert_eq!(store.used_bytes(), used);
    store.gc([]).unwrap();
    for path in &strangers {
        assert!(path.exists(), "{} was removed", path.display());
    }
}

#[test]
fn chunk_paths_are_formatted_from_ids_only() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let data = random_bytes(15, 300_000);
    let manifest = ingest(&store, &data);
    for id in ids(&manifest) {
        let path = chunk_file(dir.path(), &id);
        assert!(path.is_file());
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(
            name.len() == 64
                && name
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
    }
    // Ids that look like paths in any encoding are just 32 bytes.
    for id in [
        ChunkId(*b"../../../../../../../etc/passwd\0"),
        ChunkId([b'/'; 32]),
        ChunkId([0; 32]),
    ] {
        assert!(!store.has(&id).unwrap());
        assert!(matches!(store.read(&id), Err(StoreError::NotFound(_))));
    }
}

#[test]
fn assembly_writes_nothing_while_chunks_are_missing() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let manifest = ingest(&store, &random_bytes(16, 500_000));
    fs::remove_file(chunk_file(dir.path(), &manifest.chunks()[1].id)).unwrap();
    let out = tempfile::tempdir().unwrap();
    let error = store
        .assemble(&manifest, &out.path().join("out.sav"))
        .unwrap_err();
    assert!(
        matches!(error, StoreError::Incomplete { missing: 1 }),
        "{error}"
    );
    assert!(fs::read_dir(out.path()).unwrap().next().is_none());
}

#[test]
fn assembly_replaces_the_destination_only_when_verified() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let data = random_bytes(17, 500_000);
    let manifest = ingest(&store, &data);
    let dest = dir.path().join("world.sav");
    fs::write(&dest, b"the previous save").unwrap();

    // A damaged chunk stops assembly and leaves the old file in place.
    let path = chunk_file(dir.path(), &manifest.chunks()[2].id);
    let mut file = fs::read(&path).unwrap();
    let last = file.len() - 1;
    file[last] ^= 0x80;
    fs::write(&path, &file).unwrap();
    let error = store.assemble(&manifest, &dest).unwrap_err();
    assert!(matches!(error, StoreError::Corrupt { .. }), "{error}");
    assert_eq!(fs::read(&dest).unwrap(), b"the previous save");
    assert!(partial_files(dir.path()).is_empty());

    // What an assembly interrupted by a crash leaves behind is cleared.
    let stale = dir.path().join(".world.sav.2a-7.tpf3mp-partial");
    fs::write(&stale, b"half a save").unwrap();
    ingest(&store, &data);
    store.assemble(&manifest, &dest).unwrap();
    assert!(fs::read(&dest).unwrap() == data);
    assert!(partial_files(dir.path()).is_empty());
}

fn partial_files(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.ends_with(".tpf3mp-partial"))
        .collect()
}

#[test]
fn assembly_checks_the_manifest_against_the_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    // Shorter than the minimum chunk size, so exactly one chunk.
    let data = random_bytes(18, 10_000);
    let real = store.ingest(&data[..], small_params()).unwrap();
    let id = real.chunks()[0].id;
    let dest = dir.path().join("out.sav");

    // The right chunk under a false length.
    let lying = encode_manifest(
        [16 << 10, 64 << 10, 256 << 10],
        9_000,
        real.file_hash(),
        &[ChunkEntry {
            id,
            offset: 0,
            len: 9_000,
        }],
    );
    let lying = Manifest::from_bytes(&lying).unwrap();
    assert!(matches!(
        store.assemble(&lying, &dest),
        Err(StoreError::LengthMismatch {
            expected: 9_000,
            actual: 10_000,
            ..
        })
    ));

    // The right chunks under a false file hash.
    let wrong_hash = encode_manifest(
        [16 << 10, 64 << 10, 256 << 10],
        10_000,
        FileHash([9; 32]),
        real.chunks(),
    );
    let wrong_hash = Manifest::from_bytes(&wrong_hash).unwrap();
    assert!(matches!(
        store.assemble(&wrong_hash, &dest),
        Err(StoreError::FileHashMismatch)
    ));
    assert!(!dest.exists());
}

#[test]
fn assembly_needs_a_file_name() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let manifest = ingest(&store, &random_bytes(19, 1_000));
    let error = store.assemble(&manifest, Path::new("..")).unwrap_err();
    assert!(
        matches!(error, StoreError::InvalidDestination(_)),
        "{error}"
    );
}

#[test]
fn flushed_stores_work_the_same() {
    let dir = tempfile::tempdir().unwrap();
    let store = ChunkStore::open(dir.path(), StoreConfig::new(u64::MAX)).unwrap();
    assert!(store.config().sync_chunks);
    let data = repetitive_bytes(20, 1_000_000);
    let manifest = store.ingest(&data[..], ChunkParams::DEFAULT).unwrap();
    let dest = dir.path().join("out.sav");
    store.assemble(&manifest, &dest).unwrap();
    assert!(fs::read(&dest).unwrap() == data);
}
