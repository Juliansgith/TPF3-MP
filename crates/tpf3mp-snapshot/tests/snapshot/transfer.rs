//! Transfers between a server's store and a client's through a `ChunkSink`.

use std::fs;

use proptest::prelude::*;
use tpf3mp_snapshot::{
    ChunkError, ChunkId, ChunkSink, ChunkStore, FileHash, Manifest, SinkError, StoreError,
};

use crate::common::{
    chunk_file, chunk_files, encode_manifest, random_bytes, repetitive_bytes, serve, small_params,
    store, store_with,
};

struct Setup {
    server: ChunkStore,
    client: ChunkStore,
    client_root: std::path::PathBuf,
    out: std::path::PathBuf,
    // Last, so the stores close their lock files before the directories go.
    _dirs: [tempfile::TempDir; 3],
}

fn setup() -> Setup {
    let dirs = [
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    ];
    Setup {
        server: store(dirs[0].path()),
        client: store(dirs[1].path()),
        client_root: dirs[1].path().to_owned(),
        out: dirs[2].path().join("world.sav"),
        _dirs: dirs,
    }
}

impl Setup {
    fn publish(&self, data: &[u8]) -> Manifest {
        self.server.ingest(data, small_params()).unwrap()
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Chunks arrive in any order, some of them twice, and the file comes
    /// out identical.
    #[test]
    fn chunks_arrive_in_any_order(seed: u64, len in 0usize..1_000_000, order: Vec<proptest::sample::Index>) {
        let setup = setup();
        let data = repetitive_bytes(seed, len);
        let manifest = setup.publish(&data);
        let mut sink = ChunkSink::open(&setup.client, manifest.clone()).unwrap();
        let mut wanted = sink.missing();
        let mut last = sink.progress();
        prop_assert_eq!(last.chunks_present, 0);
        // Shuffle by the generated indices, then resend a few.
        let count = wanted.len();
        if count > 1 {
            for (i, index) in order.iter().enumerate() {
                wanted.swap(i % count, index.index(count));
            }
        }
        let resent: Vec<_> = wanted.iter().take(3).copied().collect();
        for entry in wanted.iter().chain(&resent) {
            let frame = setup.server.read_compressed(&entry.id).unwrap();
            let progress = sink.put(&entry.id, &frame).unwrap();
            prop_assert!(progress.bytes_present >= last.bytes_present);
            last = progress;
        }
        prop_assert!(last.is_complete());
        prop_assert_eq!(last.bytes_total, manifest.unique_chunks().iter().map(|e| u64::from(e.len)).sum::<u64>());
        sink.finish(&setup.out).unwrap();
        prop_assert!(fs::read(&setup.out).unwrap() == data);
        prop_assert_eq!(setup.client.pending().unwrap(), []);
    }
}

#[test]
fn a_returning_player_downloads_only_changed_chunks() {
    let setup = setup();
    let old = random_bytes(1, 2_000_000);
    let first = setup.publish(&old);
    let mut sink = ChunkSink::open(&setup.client, first.clone()).unwrap();
    let missing = sink.missing();
    serve(&setup.server, &mut sink, &missing);
    sink.finish(&setup.out).unwrap();

    // The world moves on: a record is inserted and a few bytes change.
    let mut new = old[..700_000].to_vec();
    new.extend(random_bytes(2, 3_000));
    new.extend_from_slice(&old[700_000..]);
    new[1_500_000] ^= 0xff;
    let second = setup.publish(&new);
    let mut sink = ChunkSink::open(&setup.client, second.clone()).unwrap();
    let missing = sink.missing();
    assert!(
        !missing.is_empty() && missing.len() <= 6,
        "{} chunks to fetch",
        missing.len()
    );
    let progress = sink.progress();
    assert!(
        progress.bytes_present * 10 >= progress.bytes_total * 8,
        "{progress:?}"
    );
    serve(&setup.server, &mut sink, &missing);
    sink.finish(&setup.out).unwrap();
    assert!(fs::read(&setup.out).unwrap() == new);
}

#[test]
fn nothing_to_fetch_when_the_store_has_everything() {
    let setup = setup();
    let data = random_bytes(3, 800_000);
    let manifest = setup.publish(&data);
    // The client seeds its store from its own copy of the file.
    let local = setup.client.ingest(&data[..], manifest.params()).unwrap();
    assert_eq!(local, manifest);
    let mut sink = ChunkSink::open(&setup.client, manifest.clone()).unwrap();
    assert!(sink.missing().is_empty());
    assert!(sink.progress().is_complete());
    sink.finish(&setup.out).unwrap();
    assert!(fs::read(&setup.out).unwrap() == data);
    // Assembling straight from the store needs no sink at all.
    let again = setup.out.with_file_name("again.sav");
    setup.client.assemble(&manifest, &again).unwrap();
    assert!(fs::read(&again).unwrap() == data);
}

#[test]
fn empty_files_transfer() {
    let setup = setup();
    let manifest = setup.publish(&[]);
    let mut sink = ChunkSink::open(&setup.client, manifest).unwrap();
    assert!(sink.progress().is_complete());
    sink.finish(&setup.out).unwrap();
    assert_eq!(fs::read(&setup.out).unwrap(), b"");
}

#[test]
fn hostile_chunks_are_rejected_without_a_trace() {
    let setup = setup();
    let manifest = setup.publish(&random_bytes(4, 600_000));
    let mut sink = ChunkSink::open(&setup.client, manifest.clone()).unwrap();
    let wanted = sink.missing();
    let (first, second) = (wanted[0], wanted[1]);
    let frame = setup.server.read_compressed(&first.id).unwrap();
    let other = setup.server.read_compressed(&second.id).unwrap();
    let before = sink.progress();

    let stranger = ChunkId::of(b"not in this snapshot");
    assert!(matches!(
        sink.put(&stranger, &frame),
        Err(SinkError::NotInManifest(id)) if id == stranger
    ));
    let mut flipped = frame.clone();
    let middle = flipped.len() / 2;
    flipped[middle] ^= 0x10;
    let oversized = vec![0; frame.len() * 2 + 1024];
    let truncated = &frame[..frame.len() - 1];
    type Expected = fn(&ChunkError) -> bool;
    let cases: [(&[u8], Expected); 5] = [
        (&flipped, |e| {
            matches!(
                e,
                ChunkError::HashMismatch | ChunkError::Decompress(_) | ChunkError::NotOneFrame
            )
        }),
        (&other, |e| {
            matches!(
                e,
                ChunkError::HashMismatch
                    | ChunkError::LengthMismatch { .. }
                    | ChunkError::Decompress(_)
            )
        }),
        (&oversized, |e| matches!(e, ChunkError::TooLarge { .. })),
        (truncated, |e| matches!(e, ChunkError::NotOneFrame)),
        (b"", |e| matches!(e, ChunkError::NotOneFrame)),
    ];
    for (bad, expected) in cases {
        match sink.put(&first.id, bad) {
            Err(SinkError::Rejected { id, reason }) => {
                assert_eq!(id, first.id);
                assert!(expected(&reason), "{reason}");
            }
            other => panic!("a bad chunk was accepted: {other:?}"),
        }
    }
    assert_eq!(sink.progress(), before);
    assert_eq!(sink.missing(), wanted);
    assert!(chunk_files(&setup.client_root).is_empty());

    // The genuine chunk still goes through, and resending it is harmless.
    let progress = sink.put(&first.id, &frame).unwrap();
    assert_eq!(progress.chunks_present, 1);
    assert_eq!(sink.put(&first.id, &frame).unwrap(), progress);
    assert_eq!(
        sink.put(&first.id, b"ignored: already stored").unwrap(),
        progress
    );
}

#[test]
fn something_else_in_a_chunks_place_is_an_error_not_a_chunk() {
    let setup = setup();
    let manifest = setup.publish(&random_bytes(12, 100_000));
    let id = manifest.chunks()[0].id;
    fs::create_dir_all(chunk_file(&setup.client_root, &id)).unwrap();
    let mut sink = ChunkSink::open(&setup.client, manifest).unwrap();
    assert!(sink.missing().iter().any(|entry| entry.id == id));
    let frame = setup.server.read_compressed(&id).unwrap();
    let error = sink.put(&id, &frame).unwrap_err();
    assert!(
        matches!(error, SinkError::Store(StoreError::Io { .. })),
        "{error}"
    );
    assert!(sink.missing().iter().any(|entry| entry.id == id));
}

#[test]
fn finishing_requires_every_chunk_and_happens_once() {
    let setup = setup();
    let manifest = setup.publish(&random_bytes(5, 500_000));
    let mut sink = ChunkSink::open(&setup.client, manifest).unwrap();
    let wanted = sink.missing();
    serve(&setup.server, &mut sink, &wanted[1..]);
    assert!(matches!(
        sink.finish(&setup.out),
        Err(SinkError::Incomplete { missing: 1 })
    ));
    assert!(!setup.out.exists());
    serve(&setup.server, &mut sink, &wanted[..1]);
    sink.finish(&setup.out).unwrap();
    assert!(matches!(sink.finish(&setup.out), Err(SinkError::Finished)));
    let frame = setup.server.read_compressed(&wanted[0].id).unwrap();
    assert!(matches!(
        sink.put(&wanted[0].id, &frame),
        Err(SinkError::Finished)
    ));
}

#[test]
fn transfers_resume_after_a_restart() {
    let setup = setup();
    let data = repetitive_bytes(6, 1_500_000);
    let manifest = setup.publish(&data);
    let mut sink = ChunkSink::open(&setup.client, manifest.clone()).unwrap();
    let wanted = sink.missing();
    let half = wanted.len() / 2;
    serve(&setup.server, &mut sink, &wanted[..half]);
    let progress = sink.progress();

    // The process dies: no clean shutdown, and a write that never finished.
    drop(sink);
    drop(setup.client);
    fs::write(
        setup.client_root.join("tmp").join("00000000000000ff"),
        b"torn",
    )
    .unwrap();

    let client = store(&setup.client_root);
    assert_eq!(client.pending().unwrap(), [manifest.id()]);
    // Chunks of an unfinished transfer survive garbage collection.
    let stats = client.gc([]).unwrap();
    assert_eq!(stats.removed_chunks, 0);
    let mut sink = ChunkSink::resume(&client, &manifest.id()).unwrap();
    assert_eq!(sink.manifest(), &manifest);
    assert_eq!(sink.progress(), progress);
    assert_eq!(sink.missing(), &wanted[half..]);
    let mut rest = wanted[half..].to_vec();
    rest.reverse();
    serve(&setup.server, &mut sink, &rest);
    sink.finish(&setup.out).unwrap();
    assert!(fs::read(&setup.out).unwrap() == data);
    // The finished snapshot is retained, no longer pending.
    assert_eq!(client.pending().unwrap(), []);
    assert_eq!(client.retained().unwrap(), [manifest.id()]);
    assert!(matches!(
        ChunkSink::resume(&client, &manifest.id()),
        Err(SinkError::Store(StoreError::UnknownSnapshot(_)))
    ));
}

#[test]
fn a_damaged_chunk_found_at_the_end_is_fetched_again() {
    let setup = setup();
    let data = random_bytes(7, 900_000);
    let manifest = setup.publish(&data);
    let mut sink = ChunkSink::open(&setup.client, manifest.clone()).unwrap();
    let missing = sink.missing();
    serve(&setup.server, &mut sink, &missing);

    // Bit rot, or a power failure without flushing, damages a stored chunk.
    let victim = manifest.chunks()[1].id;
    let path = chunk_file(&setup.client_root, &victim);
    let mut file = fs::read(&path).unwrap();
    let last = file.len() - 1;
    file[last] ^= 1;
    fs::write(&path, &file).unwrap();

    let error = sink.finish(&setup.out).unwrap_err();
    assert!(
        matches!(error, SinkError::Store(StoreError::Corrupt { id, .. }) if id == victim),
        "{error}"
    );
    assert!(!setup.out.exists());
    let missing = sink.missing();
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].id, victim);
    serve(&setup.server, &mut sink, &missing);
    sink.finish(&setup.out).unwrap();
    assert!(fs::read(&setup.out).unwrap() == data);
}

#[test]
fn every_damaged_chunk_is_found_in_one_round() {
    let setup = setup();
    let data = random_bytes(14, 1_200_000);
    let manifest = setup.publish(&data);
    let mut sink = ChunkSink::open(&setup.client, manifest.clone()).unwrap();
    let missing = sink.missing();
    serve(&setup.server, &mut sink, &missing);

    // A power failure without flushing: several chunks come back empty or
    // garbled.
    let chunks = manifest.chunks();
    let victims = [chunks[1].id, chunks[3].id, chunks[chunks.len() - 1].id];
    fs::write(chunk_file(&setup.client_root, &victims[0]), b"").unwrap();
    for id in &victims[1..] {
        let path = chunk_file(&setup.client_root, id);
        let mut file = fs::read(&path).unwrap();
        let last = file.len() - 1;
        file[last] ^= 1;
        fs::write(&path, &file).unwrap();
    }
    // The empty file already counts as missing.
    assert_eq!(
        sink.refresh().unwrap().chunks_total - 1,
        sink.progress().chunks_present
    );
    let missing = sink.missing();
    serve(&setup.server, &mut sink, &missing);
    assert!(sink.finish(&setup.out).is_err());
    let missing: Vec<ChunkId> = sink.missing().iter().map(|entry| entry.id).collect();
    assert_eq!(missing, &victims[1..]);
    let missing = sink.missing();
    serve(&setup.server, &mut sink, &missing);
    sink.finish(&setup.out).unwrap();
    assert!(fs::read(&setup.out).unwrap() == data);
}

#[test]
fn a_failed_publish_leaves_nothing_behind_and_can_be_retried() {
    let setup = setup();
    let data = random_bytes(15, 300_000);
    let manifest = setup.publish(&data);
    let mut sink = ChunkSink::open(&setup.client, manifest.clone()).unwrap();
    let missing = sink.missing();
    serve(&setup.server, &mut sink, &missing);
    // Something in the way of the destination.
    fs::create_dir(&setup.out).unwrap();
    let error = sink.finish(&setup.out).unwrap_err();
    assert!(
        matches!(error, SinkError::Store(StoreError::Io { .. })),
        "{error}"
    );
    assert!(setup.out.is_dir());
    fs::remove_dir(&setup.out).unwrap();
    sink.finish(&setup.out).unwrap();
    assert!(fs::read(&setup.out).unwrap() == data);
    assert_eq!(setup.client.retained().unwrap(), [manifest.id()]);
}

#[test]
fn inconsistent_manifests_are_dropped() {
    let setup = setup();
    let real = setup.publish(&random_bytes(16, 300_000));
    // The right chunks under a false file hash can never be assembled.
    let bytes = encode_manifest(
        [16 << 10, 64 << 10, 256 << 10],
        real.total_size(),
        FileHash([7; 32]),
        real.chunks(),
    );
    let lying = Manifest::from_bytes(&bytes).unwrap();
    let mut sink = ChunkSink::open(&setup.client, lying.clone()).unwrap();
    let missing = sink.missing();
    serve(&setup.server, &mut sink, &missing);
    let error = sink.finish(&setup.out).unwrap_err();
    assert!(
        matches!(error, SinkError::Inconsistent(StoreError::FileHashMismatch)),
        "{error}"
    );
    assert!(!setup.out.exists());
    assert_eq!(setup.client.pending().unwrap(), []);
    assert!(matches!(sink.finish(&setup.out), Err(SinkError::Finished)));
    // Nothing pins its chunks any more.
    drop(sink);
    setup.client.gc([]).unwrap();
    assert!(chunk_files(&setup.client_root).is_empty());
}

/// A server passes snapshots on without loading them: it verifies and keeps
/// one without writing the file.
#[test]
fn a_snapshot_can_be_retained_without_writing_its_file() {
    let setup = setup();
    let data = repetitive_bytes(17, 400_000);
    let manifest = setup.publish(&data);
    let mut sink = ChunkSink::open(&setup.client, manifest.clone()).unwrap();
    assert!(matches!(sink.retain(), Err(SinkError::Incomplete { .. })));
    let missing = sink.missing();
    serve(&setup.server, &mut sink, &missing);
    sink.retain().unwrap();
    assert!(!setup.out.exists());
    assert_eq!(setup.client.retained().unwrap(), [manifest.id()]);
    assert_eq!(setup.client.pending().unwrap(), []);
    assert!(matches!(sink.retain(), Err(SinkError::Finished)));
    // What was retained assembles to the original.
    setup.client.assemble(&manifest, &setup.out).unwrap();
    assert!(fs::read(&setup.out).unwrap() == data);
}

/// A manifest whose chunks contradict it is dropped by `retain` as by
/// `finish`, so an uploader cannot pin chunks on a server forever.
#[test]
fn retaining_an_inconsistent_manifest_drops_it() {
    let setup = setup();
    let real = setup.publish(&random_bytes(18, 300_000));
    let bytes = encode_manifest(
        [16 << 10, 64 << 10, 256 << 10],
        real.total_size(),
        FileHash([9; 32]),
        real.chunks(),
    );
    let lying = Manifest::from_bytes(&bytes).unwrap();
    let mut sink = ChunkSink::open(&setup.client, lying).unwrap();
    let missing = sink.missing();
    serve(&setup.server, &mut sink, &missing);
    let error = sink.retain().unwrap_err();
    assert!(
        matches!(error, SinkError::Inconsistent(StoreError::FileHashMismatch)),
        "{error}"
    );
    assert_eq!(setup.client.retained().unwrap(), []);
    assert_eq!(setup.client.pending().unwrap(), []);
    drop(sink);
    setup.client.gc([]).unwrap();
    assert!(chunk_files(&setup.client_root).is_empty());
}

#[test]
fn one_transfer_per_snapshot_at_a_time() {
    let setup = setup();
    let manifest = setup.publish(&random_bytes(17, 200_000));
    let sink = ChunkSink::open(&setup.client, manifest.clone()).unwrap();
    assert!(matches!(
        ChunkSink::open(&setup.client, manifest.clone()),
        Err(SinkError::AlreadyOpen(id)) if id == manifest.id()
    ));
    assert!(matches!(
        ChunkSink::resume(&setup.client, &manifest.id()),
        Err(SinkError::AlreadyOpen(_))
    ));
    drop(sink);
    ChunkSink::resume(&setup.client, &manifest.id()).unwrap();
}

/// A valid zstd frame that does not compress at all: raw blocks only, as a
/// careless or hostile peer might send.
fn uncompressed_frame(data: &[u8]) -> Vec<u8> {
    const BLOCK: usize = 128 << 10;
    // Magic, then a single-segment header with a 4-byte content size.
    let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0xa0];
    frame.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
    let blocks: Vec<&[u8]> = data.chunks(BLOCK).collect();
    for (index, block) in blocks.iter().enumerate() {
        let last = u32::from(index + 1 == blocks.len());
        let header = (u32::try_from(block.len()).unwrap() << 3) | last;
        frame.extend_from_slice(&header.to_le_bytes()[..3]);
        frame.extend_from_slice(block);
    }
    frame
}

#[test]
fn received_chunks_can_be_compressed_again() {
    let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let keeper = store(dirs[0].path());
    let recompressor = store_with(dirs[1].path(), |config| config.recompress_received = true);
    // Compressible, so a raw-block frame is much larger than a real one.
    let data: Vec<u8> = (0..600_000usize)
        .map(|i| b"transport fever "[i % 16] ^ u8::from(i / 4096 % 3 == 0))
        .collect();
    let manifest = Manifest::compute(&data[..], small_params()).unwrap();
    for store in [&keeper, &recompressor] {
        let mut sink = ChunkSink::open(store, manifest.clone()).unwrap();
        for entry in sink.missing() {
            let start = entry.offset as usize;
            let raw = &data[start..start + entry.len as usize];
            let theirs = uncompressed_frame(raw);
            sink.put(&entry.id, &theirs).unwrap();
            let stored = store.read_compressed(&entry.id).unwrap();
            if store.config().recompress_received {
                assert_eq!(stored, zstd::bulk::compress(raw, 3).unwrap());
                assert!(stored.len() * 4 < theirs.len());
            } else {
                assert_eq!(stored, theirs);
            }
        }
        let out = store.root().join("world.sav");
        sink.finish(&out).unwrap();
        assert!(fs::read(&out).unwrap() == data);
    }
}

#[test]
fn abandoned_transfers_are_collected() {
    let setup = setup();
    let manifest = setup.publish(&random_bytes(8, 600_000));
    let mut sink = ChunkSink::open(&setup.client, manifest).unwrap();
    let missing = sink.missing();
    serve(&setup.server, &mut sink, &missing[..2]);
    assert_eq!(chunk_files(&setup.client_root).len(), 2);
    sink.abandon().unwrap();
    assert_eq!(setup.client.pending().unwrap(), []);
    setup.client.gc([]).unwrap();
    assert!(chunk_files(&setup.client_root).is_empty());
}

#[test]
fn damaged_transfer_state_is_reported_and_dropped() {
    let setup = setup();
    let manifest = setup.publish(&random_bytes(9, 300_000));
    let sink = ChunkSink::open(&setup.client, manifest.clone()).unwrap();
    drop(sink);
    let state = setup
        .client_root
        .join("pending")
        .join(manifest.id().to_string());
    let mut bytes = fs::read(&state).unwrap();
    bytes.truncate(bytes.len() - 1);
    fs::write(&state, &bytes).unwrap();

    assert!(matches!(
        ChunkSink::resume(&setup.client, &manifest.id()),
        Err(SinkError::Store(StoreError::DamagedRecord { .. }))
    ));
    let stats = setup.client.gc([]).unwrap();
    assert_eq!(stats.damaged_records, 1);
    assert_eq!(setup.client.pending().unwrap(), []);
    // Starting over from the real manifest works.
    ChunkSink::open(&setup.client, manifest).unwrap();
}

#[test]
fn a_full_store_refuses_chunks_until_collected() {
    let setup = setup();
    // An earlier snapshot fills most of the store.
    let earlier = setup
        .client
        .ingest(&random_bytes(10, 300_000)[..], small_params())
        .unwrap();
    let limit = setup.client.used_bytes() + 100_000;
    // Incompressible, so it needs about 350 kB: too much now, fine later.
    let manifest = setup.publish(&random_bytes(11, 350_000));
    drop(setup.client);
    let client = store_with(&setup.client_root, |config| config.max_bytes = limit);
    let mut sink = ChunkSink::open(&client, manifest).unwrap();
    let wanted = sink.missing();
    let mut refused = None;
    for entry in &wanted {
        let frame = setup.server.read_compressed(&entry.id).unwrap();
        if let Err(error) = sink.put(&entry.id, &frame) {
            refused = Some((entry.id, error));
            break;
        }
    }
    let (id, error) = refused.expect("the store should fill up");
    assert!(
        matches!(error, SinkError::Store(StoreError::Full { .. })),
        "{error}"
    );
    assert!(client.used_bytes() <= limit);
    assert!(sink.missing().iter().any(|entry| entry.id == id));

    // Releasing the earlier snapshot and collecting makes room; the
    // transfer's own chunks stay.
    let kept = sink.progress().chunks_present;
    client.release(&earlier.id()).unwrap();
    client.gc([]).unwrap();
    assert_eq!(sink.refresh().unwrap().chunks_present, kept);
    let missing = sink.missing();
    serve(&setup.server, &mut sink, &missing);
    sink.finish(&setup.out).unwrap();
}
