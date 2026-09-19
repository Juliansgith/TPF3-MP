# Snapshots

Status: implemented in `crates/tpf3mp-snapshot` and wired into the protocol,
2026-09-19: rooms save, agree on a save, and hand it to players who join
late, can no longer resume, or diverged. "Snapshots" in
[PROTOCOL.md](PROTOCOL.md) describes the flow;
[Integration](#integration) below says where each part lives. Measured on
synthetic data only: real TPF3 saves are a release-day item (see
[Limitations](#limitations-and-open-questions)).

The server keeps the latest agreed native save of every room, and players who
hot-join, reconnect or are rebased download it (see
[ARCHITECTURE.md](ARCHITECTURE.md), "Snapshots"). Saves run from 50 to 500 MB
(a TPF2 autosave measured 113 MB), but successive saves of one world share most
of their bytes. So a save is stored and sent as content-defined chunks, and a
returning player downloads only the chunks their store lacks.

```
server                                             client (agent)
save ──ingest──▶ ChunkStore                         ChunkStore
                   │                                   ▲
                   ├─ Manifest ─── bulk stream ───▶ Manifest::from_bytes
                   │                                   │ ChunkSink::open
                   │                                   │ sink.missing() ─▶ requests
                   └─ read_compressed(id) ─ frames ─▶ sink.put(id, frame)
                                                       │ sink.finish(path)
                                                       ▼
                                                     save
```

## Format

### Chunking

- **Algorithm:** FastCDC 2020 (`fastcdc` 5.0, `v2020::StreamCDC`) with
  normalization level 1 and the unseeded gear table. Cut points depend only on
  content, so every platform cuts a file identically. A test pins the cut
  points of a fixed input, so a `fastcdc` upgrade that moves them fails CI
  instead of silently breaking dedup against existing stores.
- **Parameters:** `min`, `avg` and `max` chunk sizes, all powers of two with
  `min < avg < max`. Every chunk except the last is `min..=max` bytes; the
  last is `1..=max`.
- **Default: 32 KiB / 128 KiB / 512 KiB**, chosen from the measurements below.
- **Bounds on any manifest:** `min >= 16 KiB` (`MIN_CHUNK_FLOOR`) and
  `max <= 4 MiB` (`MAX_CHUNK_LEN`).
- **Parameters travel in every manifest.** The server can change them without a
  protocol change, and a client that seeds its store from a local file uses
  the manifest's parameters.
- **Streaming:** the chunker reads through one `max`-sized buffer. Ingesting a
  file holds that buffer, the chunk being compressed and the manifest in
  memory, whatever the size of the file.

### Names

| name | what it hashes | used for |
|---|---|---|
| `ChunkId` | BLAKE3-256 of the uncompressed chunk | manifests, file names, wire |
| `FileHash` | BLAKE3-256 of the whole file | final check before publishing |
| `ManifestId` | BLAKE3-256 of the canonical manifest bytes | naming a snapshot, transfer state |

### Compression and chunk files

- **One zstd frame per chunk**, level 3 by default
  (`StoreConfig::compression_level`). There is no dictionary and no zstd
  checksum; BLAKE3 covers integrity. Any frame that decompresses to the right
  bytes is valid, so the level can change at any time.
- **zstd is built without default features.** That leaves out the legacy
  (pre-1.0) frame decoders, which peer data would otherwise reach, and the
  dictionary builder.
- **On disk:** each chunk file is the magic `T3SC`, the uncompressed length
  (`u32`, little-endian), then the zstd frame.
- **On the wire:** the frame alone. The receiver takes the length from the
  manifest.
- **Exactly one standard frame.** zstd would also accept skippable frames, or
  several frames in a row. A chunk is rejected unless it is one standard frame
  and nothing else, so a peer cannot smuggle extra bytes into chunks that are
  stored and served on.

### Manifest

Postcard, in this order. Varints are postcard's LEB128.

| field | encoding |
|---|---|
| magic | the 4 bytes `T3SM` |
| version | `u8`, currently 1 (`MANIFEST_VERSION`) |
| `min`, `avg`, `max` | 3 × varint `u32` |
| file size | varint `u64` |
| file hash | 32 bytes |
| chunk count | varint |
| per chunk | id (32 bytes), offset (varint `u64`), length (varint `u32`) |

At the default parameters this is about 39 bytes per chunk: 61 KiB for a
256 MiB save. `Manifest::from_bytes` accepts a manifest only if all of these
hold:

- **Size:** at most `MAX_MANIFEST_LEN` (10,747,976) bytes, checked before
  decoding starts.
- **Header:** the magic and version match, and the parameters are valid.
- **File size:** at most 4 GiB (`MAX_FILE_SIZE`).
- **Chunk count:** at most `ceil(file size / min)`, checked before any entry is
  read. That is never more than `MAX_CHUNKS` (262,144).
- **Layout:** offsets start at 0 and are contiguous, lengths obey the bounds
  above, and the lengths sum to the file size.
- **Duplicates:** a repeated id always carries the same length.
- **Encoding:** there are no trailing bytes, and the encoding is canonical.
  Postcard accepts overlong varints, so the decoder re-encodes and compares.
  One manifest has one encoding and so one `ManifestId`.

Rejections name the rule, and the field or chunk index at fault
(`ManifestError`, `ManifestField`). A test pins the byte layout.

### Store layout and durability

```
<root>/lock                       exclusive lock: one process per store
<root>/chunks/<2 hex>/<64 hex>    chunk files, fan-out directories made on first use
<root>/roots/<64 hex>             manifests of retained snapshots
<root>/pending/<64 hex>           manifests of unfinished transfers
<root>/tmp/                       files being written; emptied at open and by gc
```

- **Puts are atomic and idempotent.**
  - A chunk is written to `tmp/`, then renamed into place under a lock that
    also checks the size bound.
  - If the chunk is already stored, the temporary file is dropped.
  - Concurrent puts of one chunk store it once and count it once.
  - A file too short to be a chunk, which a power failure can leave behind,
    counts as missing and is replaced.
- **Every read verifies.** A chunk is decompressed into a buffer of its
  recorded length and checked against its id. A damaged chunk is deleted and
  reported as `StoreError::Corrupt`, so it counts as missing again. It is not
  deleted if a good copy replaced it after it was read.
- **Size bound:** a chunk that would take the store past
  `StoreConfig::max_bytes` is refused with `StoreError::Full`. Releasing
  snapshots and running `gc` frees space.
- **Retention:** the store keeps the snapshots it produced until the caller
  releases them.
  - `ingest` and a finished `ChunkSink` record their manifest in `roots/`
    before they return, and `release` removes it.
  - A collection that waited for its lock during an ingest therefore cannot
    take the chunks of the snapshot that ingest produced, even before the
    caller has recorded it.
  - `retained()` and `manifest(id)` let a restarted process find its
    snapshots again.
- **Garbage collection** deletes every chunk that is referenced by no
  retained snapshot, no unfinished transfer and no manifest passed to `gc`.
  - A deletion that fails is counted, the chunk stays accounted for, and the
    next collection tries again.
  - It clears `tmp/` and recounts the size from disk.
  - A saved manifest that can no longer be read is dropped: it cannot be used,
    and what it protected is unknown.
- **Foreign files are left alone.** Files that are not a 64-digit lowercase
  id inside the matching fan-out directory are never counted or deleted.
- **Assembly** builds the file under a unique hidden name next to the
  destination and renames it into place.
  - An in-process claim stops two assemblies of one destination from racing.
  - Partial files left by a crashed assembly are removed the next time.

| written | flushed to stable storage |
|---|---|
| chunk data | before the rename, if `sync_chunks` (default `true`) |
| chunk directories of an ingested or received snapshot | before it counts as retained, if `sync_chunks`; best effort |
| retained and pending manifests | the file always, its directory best effort |
| assembled file | the file always before the rename, its directory best effort after |

Directory flushes are best effort, as in SQLite:

- **Windows:** they are skipped; there is no portable way to open a directory
  for flushing. NTFS journals the rename, so it is atomic, and the journal is
  committed shortly after. It may not survive a power failure in that window.
- **Unix:** a failure is ignored. Some file systems refuse to flush
  directories, and by then the rename has happened and the file data is on
  disk.

Without `sync_chunks`, a power failure can leave damaged chunks. Reads detect
and remove them, so the cost is fetching them again, never a wrong file.

## Parameters and measurements

### Setup

- **Machine:**
  - AMD Ryzen 9 5900X (12 cores, 24 threads), 128 GiB RAM;
  - Windows 11 Pro 10.0.26200;
  - Samsung SSD 970 EVO 1 TB (NVMe, NTFS);
  - Microsoft Defender real-time protection on.
- **Build:** Rust 1.98.1, release profile (thin LTO, one codegen unit). One
  thread.
- **Runs:** each number is the median of three. CPU rows varied by about 3%
  between runs; disk-bound rows by up to 40%. A fourth run after the last
  code changes stayed within that spread.
- **Data:** 256 MiB of synthetic save-like data. It is built from sections of
  64 KiB to 4 MiB:
  - fixed-layout entity records with float positions (45%);
  - float terrain grids (20%);
  - Lua-like text (20%);
  - incompressible blobs (15%).

  It compresses 2.12× at zstd level 3.
- **Edits:** the kind successive saves differ by:
  - 70% small in-place changes of 1 to 64 bytes;
  - 15% inserted records of 16 bytes to 4 KiB;
  - 15% deleted ranges of the same size.

  They land at uniformly random positions, or clustered in one tenth of the
  file.

Reproduce with:

```sh
cargo run --release -p tpf3mp-snapshot --example measure -- 256
# dedup table for two real saves of one world:
cargo run --release -p tpf3mp-snapshot --example measure -- --pair OLD.sav NEW.sav
```

### Throughput

In memory, one thread, default parameters:

| stage | throughput |
|---|---|
| FastCDC cut points only | 3,105 MiB/s |
| chunk + BLAKE3 (chunk ids and file hash) | 821 MiB/s |
| chunk + BLAKE3 + zstd level 1 | 312 MiB/s, ratio 2.13 |
| **chunk + BLAKE3 + zstd level 3 (default)** | **296 MiB/s**, ratio 2.12 |
| chunk + BLAKE3 + zstd level 6 | 131 MiB/s, ratio 2.17 |
| chunk + BLAKE3 + zstd level 9 | 74 MiB/s, ratio 2.18 |
| **zstd decompress + BLAKE3 verify** | **922 MiB/s** |

With the real store code paths on disk, the numbers include the per-chunk file
work. "Transfer" means:

- every chunk `read_compressed` from one store;
- `put` into a `ChunkSink` on another;
- then `finish`.

| parameters | chunks | ingest, synced | ingest, unsynced | transfer, synced | transfer, unsynced |
|---|---|---|---|---|---|
| 16/64/256 KiB | 3,322 | 29 MiB/s | 63 MiB/s | 25 MiB/s | 52 MiB/s |
| **32/128/512 KiB** | 1,599 | 53 MiB/s | 104 MiB/s | 49 MiB/s | 86 MiB/s |
| 64/256/1024 KiB | 813 | 79 MiB/s | 142 MiB/s | 75 MiB/s | 113 MiB/s |
| 128/512/2048 KiB | 397 | 112 MiB/s | 169 MiB/s | 87 MiB/s | 137 MiB/s |

At the default parameters, three more operations:

| operation | throughput |
|---|---|
| re-ingesting an unchanged save (chunk + hash + lookups) | 708 MiB/s |
| `read_compressed` of every chunk (the serving side) | 549 MiB/s |
| `assemble` from the store | 325 MiB/s |

### Dedup after edits

Each cell shows two things for a receiver that holds the original save:

- what it transfers for the edited save: zstd level 3 bytes of the chunks it
  lacks;
- in parentheses, the share of the edited save's bytes it already holds.

The whole save compresses to 120.2–120.7 MiB with every parameter set.

| parameters | manifest | 1 KiB inserted at start | 10 random | 100 random | 1,000 random | 10,000 random | 1,000 in one 10% region |
|---|---|---|---|---|---|---|---|
| 16/64/256 KiB | 127 KiB | 0.0 MiB (100%) | 0.6 MiB (99.5%) | 4.3 MiB (96.3%) | 35.9 MiB (68.6%) | 113.1 MiB (6.1%) | 11.3 MiB (90.5%) |
| **32/128/512 KiB** | 61 KiB | 0.1 MiB (99.9%) | 0.8 MiB (99.3%) | 8.9 MiB (92.5%) | 60.2 MiB (47.7%) | 119.7 MiB (0.9%) | 11.8 MiB (90.1%) |
| 64/256/1024 KiB | 31 KiB | 0.1 MiB (99.9%) | 2.3 MiB (98.0%) | 16.9 MiB (85.8%) | 87.1 MiB (24.6%) | 120.3 MiB (0.0%) | 12.0 MiB (89.9%) |
| 128/512/2048 KiB | 15 KiB | 0.2 MiB (99.8%) | 4.4 MiB (96.4%) | 33.9 MiB (71.6%) | 110.4 MiB (6.9%) | 120.3 MiB (0.0%) | 12.0 MiB (89.9%) |

### Why 32 KiB / 128 KiB / 512 KiB

- **Small chunks cost almost nothing in size.** The compressed save is the same
  size with every parameter set (±0.4%). Manifests stay under 0.05% of the
  save.
- **For scattered edits, transfers roughly halve with each halving of the chunk
  size.** Inserted or shifted content, and edits clustered in one region,
  deduplicate equally well at every size.
- **The price of small chunks is per-file work.** On this Windows machine each
  chunk file costs about 1 ms unsynced and 2.5 ms synced: create, write,
  rename, and most likely Defender scanning the new file. So local throughput
  drops by a quarter to a half with each halving of the chunk size. File
  creation on Linux is usually far cheaper, but has not been measured here.
- **32/128/512 KiB cuts the transfer of the suggested 64/256/1024 KiB by a
  third to a half** for moderately scattered edits: 8.9 against 16.9 MiB for
  100 edits, and 60 against 87 MiB for 1,000. Local throughput stays at
  49–104 MiB/s, above what a player's connection delivers (100 Mbit/s ≈
  12 MiB/s, 1 Gbit/s ≈ 119 MiB/s).
- **16/64/256 KiB would halve transfers again**, but it also cuts local
  throughput to 25–63 MiB/s and doubles the number of files. That only pays
  if real saves change in scattered places, which is unknown until release.
- **Retuning is cheap.** Clients accept any valid parameters. Switching costs
  each client one full transfer, because old chunks no longer line up.
- **zstd level 3 stays.** Level 1 is only 5% faster at the same ratio, and
  levels 6 and 9 gain under 3% ratio at 2–4× the cost. Decompression speed is
  the same for all levels.

## Security properties

Threat model: manifests and chunks may come from hostile or faulty peers.
Examples are a client uploading a save, a faulty server, or a future
peer-to-peer source. Nothing a peer sends is trusted beyond what it can be
checked against.

- **Bounded before allocation.**
  - A manifest's length is checked before it is decoded.
  - Its chunk count is checked before any entry is read, against both the file
    size and `MAX_CHUNKS`.
  - Preallocation never exceeds what the remaining input could hold.
  - A received chunk frame larger than zstd's worst case for the manifest's
    length (at most `MAX_COMPRESSED_CHUNK_LEN`, 4,210,688 bytes) is rejected
    unread.
  - Decompression writes into a buffer of exactly the manifest's length, at
    most 4 MiB, which zstd does not grow. So there are no decompression bombs,
    and decoding time is linear.
- **Strict, canonical parsing** with precise errors (see [Manifest](#manifest)).
  Property tests feed arbitrary bytes, mutated manifests and well-formed but
  inconsistent manifests. None panics. Each is either rejected or re-encodes
  to exactly its input.
- **Verified end to end.**
  - A received chunk must be listed in the manifest, be exactly one standard
    zstd frame, decompress to exactly its length and hash to its id before it
    is stored.
  - It is checked again on every read.
  - The assembled file must match the manifest's BLAKE3 before it atomically
    replaces the destination, so the destination only ever holds a complete,
    verified file.
  - A property test damages random bytes of stored chunks: every read either
    fails or returns the original bytes.
- **No passing on of a peer's padding.**
  - Skippable and extra frames are rejected.
  - With `recompress_received`, a store compresses received chunks itself
    instead of keeping the sender's frame. A valid but uncompressed frame,
    which would cost everyone downstream bandwidth, therefore goes no
    further.
- **Self-healing.**
  - A damaged chunk is deleted when found. When assembly finds one, it checks
    and clears every damaged chunk of the snapshot, so a single further fetch
    round repairs them all.
  - A damaged saved manifest is reported, and `gc` drops it.
- **No permanent pins.** A manifest whose chunks contradict it (a false file
  hash or chunk length) can never be assembled. `finish` reports
  `SinkError::Inconsistent` and drops the transfer, instead of keeping its
  chunks pinned forever.
- **No names from the network.**
  - Every path is formatted from a 32-byte id as 64 lowercase hex digits.
  - Directory scans accept only exact names in the matching fan-out directory.
  - Destination paths come from local callers only.
- **Resource bounds on disk.**
  - `max_bytes` caps the store.
  - A transfer accepts only chunks listed in its manifest, and at most 4 GiB
    in total.
  - `gc` reclaims released and abandoned data.
- **Smaller attack surface in C code.** zstd is built without its legacy
  decoders.

The integration has to provide the rest:

- **Manifest authenticity.** A manifest is only as trustworthy as the channel
  it came over. The agent must accept only a manifest the server offered in
  its session, and check its `ManifestId` against the offer. The file hash
  inside the manifest is what the download is verified against.
- **Authorization.** The chunk store is content-addressed and may be shared by
  every room on a node. The server must serve a session only the chunks of
  manifests offered to that session. Otherwise a client could probe other
  rooms' saves for known content.
- **Save contents.** A save's Lua sidecar is still validated as data before
  loading (ARCHITECTURE.md, "Saves are code"). This crate moves bytes and
  proves they are the ones the server published; it does not judge them.
- **Local access.** Stored chunks are plain files, readable by anyone who can
  read the store directory.

## Public API

Everything is synchronous and does blocking file I/O. Async callers use
`tokio::task::spawn_blocking` or a dedicated I/O thread.

| item | purpose |
|---|---|
| `ChunkParams::{DEFAULT, new}` | chunk-size bounds, validated |
| `Chunker::{new, next_chunk, finish}` | streaming FastCDC + BLAKE3 over any `Read` |
| `Manifest::{compute, from_bytes, to_bytes, id, chunks, unique_chunks, total_size, file_hash, params}` | the canonical chunk list |
| `ChunkStore::open(root, StoreConfig)` | open or create a store, taking its lock |
| `ChunkStore::{ingest, has, missing, read, read_compressed, assemble, used_bytes}` | the store; `missing` is the transfer plan |
| `ChunkStore::{retained, release, manifest, pending, gc}` | retention and garbage collection |
| `ChunkSink::{open, resume, missing, put, progress, refresh, finish, abandon}` | a resumable, verifying receiver; one per snapshot at a time |
| `StoreConfig { max_bytes, compression_level, sync_chunks, recompress_received }` | per-store policy |
| `ChunkId`, `FileHash`, `ManifestId` | 32-byte BLAKE3 names with hex `Display`/`from_hex` and serde |
| `ManifestError`, `StoreError`, `SinkError`, `ChunkError`, `ParamsError`, `SourceError` | typed errors |
| `MAX_FILE_SIZE`, `MAX_CHUNKS`, `MAX_CHUNK_LEN`, `MIN_CHUNK_FLOOR`, `MAX_MANIFEST_LEN`, `MAX_COMPRESSED_CHUNK_LEN` | limits for frame caps |

Server side:

```rust
let mut config = StoreConfig::new(64 << 30);
config.recompress_received = true; // saves arrive from untrusted replicas
let store = ChunkStore::open(data_dir.join("snapshots"), config)?;
let manifest = store.ingest(File::open(&save)?, ChunkParams::DEFAULT)?; // retained
let offer = (manifest.id(), manifest.to_bytes());
// For each chunk request, after checking the session may have `id`:
let frame = store.read_compressed(&id)?;
// Once no transfer of an older snapshot is in flight:
store.release(&older.id())?;
store.gc([])?;
// After a restart, the snapshots are still there:
let snapshots = store.retained()?; // store.manifest(&id) loads each one
```

Client side:

```rust
let mut config = StoreConfig::new(8 << 30);
config.sync_chunks = false; // see "Store layout and durability"
let store = ChunkStore::open(cache_dir, config)?;
let manifest = Manifest::from_bytes(&bytes)?; // bounded and strict
if manifest.id() != offered_id { /* protocol violation */ }
let mut sink = ChunkSink::open(&store, manifest)?;
let wanted = sink.missing(); // request these, in batches
// As frames arrive, in any order:
let progress = sink.put(&id, &frame)?; // rejects anything unverified
sink.finish(&save_path)?; // assemble, verify, retain, publish atomically
store.release(&previous_snapshot_of_this_world)?;
// After a restart:
for id in store.pending()? {
    let sink = ChunkSink::resume(&store, &id)?;
}
```

## Integration

The flow is specified under "Snapshots" in [PROTOCOL.md](PROTOCOL.md). The
parts:

| part | where |
|---|---|
| names on the wire (`SnapshotId`, `ChunkHash`), `BulkOpen`, `BulkRequest`, `BulkResponse`, `WorldOffer`, `SavedWorld`, frame caps | `tpf3mp-proto`, `snapshot.rs` |
| moving one snapshot over one QUIC stream: `bulk::fetch`, `bulk::serve` | `tpf3mp-net`, `bulk.rs` |
| when games save, save rounds, uploads, offers, rebases, the pointer file | `tpf3mp-server`, `snapshots.rs` and `room.rs` |
| bulk streams on the server, with their authorization | `tpf3mp-server`, `connection.rs` |
| the player's store (`Worlds`), fetching and uploading | `tpf3mp-agent`, `transfer.rs` |
| saving, fetching and loading around the game | `tpf3mp-agent`, `bridge.rs` |
| the game's side: `Game::save`, `StepGate::Load` | `tpf3mp-bridge`, `session.rs` |

How the duties this crate leaves to its users are met:

- **Manifest authenticity.** A fetch checks that the manifest hashes to the
  snapshot the turn stream offered (`BulkError::WrongManifest`), and the
  assembled file against the manifest's file hash.
- **Authorization.** The server serves a connection only the snapshot the
  room offered to that connection, and only chunks its manifest lists;
  anything else is `Unavailable` or a protocol violation.
- **Bounds.** One bulk stream per connection at a time, 32 at once across a
  server, requests of at most 256 chunks and 16 KiB, responses of at most
  11 MiB, and a minute of silence ends a stream. The server's store has a
  size bound (64 GiB by default).
- **Untrusted uploads.** The server receives a save through a `ChunkSink`
  with `recompress_received`, and keeps it only after `ChunkSink::retain`
  verified the whole file. Which save to fetch is decided by the room's
  save round, from lane digests, never by one client's say.
- **Blocking work** runs on Tokio's blocking pool.

Retention:

- **Server:** `sync_chunks` and `recompress_received` on. Each room keeps its
  current snapshot and the one before; promoting a new one releases the
  oldest and collects garbage. A closed room releases its snapshots, and a
  starting server releases snapshots no restored room refers to.
- **Agent:** `sync_chunks` off. It keeps the game's two newest saves and the
  world it last received, releases the rest after each save or fetch, and
  collects garbage then. Saves the game writes are deleted once cut into
  the store.

## Toolchain

- **zstd** 0.14 (zstd-sys 2.1, libzstd 1.5.7) compiles its vendored C sources
  with the `cc` crate. That needs a C compiler:
  - MSVC `cl.exe` on Windows (part of the Visual Studio Build Tools that the
    MSVC Rust toolchain already requires);
  - Xcode command line tools on macOS;
  - gcc or clang on Linux.

  No CMake, NASM or pkg-config is needed. zstd-sys builds its x86-64 assembly
  decoder on Linux and disables it on Windows.
- **blake3** 1.8 builds its SIMD assembly with `cc`:
  - on x86-64 Windows through `ml64.exe` (same Build Tools);
  - on macOS arm64 as NEON C.

  Without a C compiler it falls back to Rust intrinsics.
- **Verified builds:** warning-free on `x86_64-pc-windows-msvc` with Rust
  1.98.1. Linux and macOS arm64 are left to CI, whose GitHub runners ship
  these compilers.
- **Debug builds** compile the C code unoptimized: zstd level 3 runs at about
  28 MiB/s in a debug build, against 296 MiB/s in release. The tests keep
  their data small (84 tests, about 9 s in total single-threaded here).
  `[profile.dev.package.zstd-sys] opt-level = 3` in the root `Cargo.toml`
  would speed debug builds up, if wanted.

## Limitations and open questions

- **[needs game] Dedup assumes saves are not compressed or encrypted as a
  whole.**
  - If TPF3 compresses its save stream, successive saves share almost no bytes
    and every transfer is a full download. That is still correct and
    verified, only not smaller.
  - The release-day save investigation (DAY_ONE.md §7) should run
    `measure --pair` on two saves of one world taken minutes apart.
  - If saves turn out to be compressed, the options are chunking the
    uncompressed payload (only if the game loads it, or it can be
    recompressed bit-exactly) or accepting full transfers.
- **Measured on synthetic data.** The parameters and compression level should
  be rechecked with real saves. Retuning needs no protocol change.
- **Ingest is single-threaded** and compression-bound at about 300 MiB/s.
  Only new chunks are compressed. Parallel compression is possible if server
  load calls for it.
- **One file per chunk.**
  - This costs about 1 ms per chunk on Windows with Defender, which is what
    limits small chunk sizes.
  - Opening a store scans every chunk file to count its size.
  - Very large server stores may want pack files later.
- **`has` and `missing` trust that a file present under an id holds that
  chunk**, unless it is too short to be one. Other damage surfaces on read.
  After a crash without `sync_chunks`, that costs one more fetch round at
  `finish`.
- **Garbage collection is stop-the-world for the store.**
  - It waits for running ingests and assemblies, which can take seconds for a
    large save.
  - While it waits, new puts, ingests and assemblies wait too.
  - Run it when a room's snapshot changes rather than on a timer.
  - Per-chunk locking would lift this if a busy server needs it.
- **Size accounting counts file lengths**, not allocated disk blocks.
- **One process per store**, enforced by a lock file, and one `ChunkSink` per
  snapshot at a time within it.
- **Directory flushes are a no-op on Windows**, where NTFS journals metadata.
  A rename is atomic there, but may not survive a power failure in the moment
  before the journal commits.
