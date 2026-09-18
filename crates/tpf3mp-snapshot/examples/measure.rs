//! Measures chunking, hashing, compression, the on-disk store and dedup after
//! edits, on synthetic save-like data. `docs/SNAPSHOTS.md` quotes its output.
//!
//! ```sh
//! cargo run --release -p tpf3mp-snapshot --example measure -- [SIZE_MIB] [SCRATCH_DIR]
//! cargo run --release -p tpf3mp-snapshot --example measure -- --pair OLD.sav NEW.sav
//! ```
//!
//! The data imitates a save: tables of fixed-layout entity records with float
//! positions, terrain grids, Lua-like text and incompressible blobs, in
//! sections of 64 KiB to 4 MiB. Edits are the kind successive saves differ
//! by: small in-place changes, and inserted and deleted records, at random
//! positions. `--pair` prints the dedup table for two real saves of one world
//! instead, which is how the parameters should be checked once TPF3 is out.

use std::{
    collections::HashSet,
    env,
    error::Error,
    fs::{self, File},
    path::{Path, PathBuf},
    time::Instant,
};

use fastcdc::v2020::{FastCDC, Normalization};
use tpf3mp_snapshot::{
    ChunkId, ChunkParams, ChunkSink, ChunkStore, Chunker, Manifest, StoreConfig,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const MIB: f64 = 1024.0 * 1024.0;
const SEED: u64 = 0x5450_4633_2d4d_5031;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if let [flag, old, new] = &args[..]
        && flag == "--pair"
    {
        machine();
        let (old, new) = (fs::read(old)?, fs::read(new)?);
        println!(
            "\nsaves: {:.1} MiB and {:.1} MiB\n",
            old.len() as f64 / MIB,
            new.len() as f64 / MIB
        );
        return dedup(&old, &[("second save".into(), new)]);
    }
    let size_mib: usize = args
        .first()
        .map(|arg| arg.parse())
        .transpose()?
        .unwrap_or(256);
    let scratch = args
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join(format!("tpf3mp-measure-{}", std::process::id()));
    fs::create_dir_all(&scratch)?;
    let result = run(size_mib << 20, &scratch);
    fs::remove_dir_all(&scratch)?;
    result
}

fn run(size: usize, scratch: &Path) -> Result<()> {
    machine();
    let data = synthetic_save(SEED, size);
    println!("\nsynthetic save: {:.0} MiB\n", size as f64 / MIB);
    cpu_pipeline(&data)?;
    store_pipeline(&data, scratch)?;
    let edited: Vec<(String, Vec<u8>)> = SCENARIOS
        .iter()
        .enumerate()
        .map(|(index, scenario)| {
            (
                scenario.describe(),
                scenario.apply(&data, SEED ^ index as u64),
            )
        })
        .collect();
    dedup(&data, &edited)
}

const SCENARIOS: [Scenario; 6] = [
    Scenario::InsertAtStart,
    Scenario::Random(10),
    Scenario::Random(100),
    Scenario::Random(1000),
    Scenario::Random(10_000),
    Scenario::Clustered(1000),
];

fn parameter_sets() -> Result<[ChunkParams; 4]> {
    let kib = 1 << 10;
    Ok([
        ChunkParams::new(16 * kib, 64 * kib, 256 * kib)?,
        ChunkParams::new(32 * kib, 128 * kib, 512 * kib)?,
        ChunkParams::new(64 * kib, 256 * kib, 1024 * kib)?,
        ChunkParams::new(128 * kib, 512 * kib, 2048 * kib)?,
    ])
}

fn machine() {
    println!("## Machine\n");
    println!("- OS: {} {}", env::consts::OS, env::consts::ARCH);
    println!("- CPU: {}", cpu_model().unwrap_or_else(|| "unknown".into()));
    println!(
        "- logical CPUs: {}",
        std::thread::available_parallelism().map_or(0, |n| n.get())
    );
    if cfg!(debug_assertions) {
        println!("- WARNING: debug build; run with --release for meaningful numbers");
    }
}

fn cpu_model() -> Option<String> {
    let output = if cfg!(windows) {
        std::process::Command::new("reg")
            .args([
                "query",
                r"HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\0",
                "/v",
                "ProcessorNameString",
            ])
            .output()
            .ok()?
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
            .ok()?
    } else {
        let info = fs::read_to_string("/proc/cpuinfo").ok()?;
        let line = info.lines().find(|line| line.starts_with("model name"))?;
        return Some(line.split(':').nth(1)?.trim().to_owned());
    };
    let text = String::from_utf8(output.stdout).ok()?;
    let line = text.lines().rev().find(|line| !line.trim().is_empty())?;
    let model = line.rsplit("REG_SZ").next()?.trim();
    Some(model.to_owned())
}

fn throughput(bytes: usize, seconds: f64) -> String {
    format!("{:.0} MiB/s", bytes as f64 / MIB / seconds)
}

/// Single-threaded CPU cost of each stage, from memory, with the default
/// parameters.
fn cpu_pipeline(data: &[u8]) -> Result<()> {
    let params = ChunkParams::DEFAULT;
    println!(
        "## CPU pipeline (one thread, in memory, {})\n",
        describe(params)
    );
    println!("| stage | throughput | result |");
    println!("|---|---|---|");

    let start = Instant::now();
    let count = FastCDC::with_level(
        data,
        params.min() as usize,
        params.avg() as usize,
        params.max() as usize,
        Normalization::Level1,
    )
    .count();
    let seconds = start.elapsed().as_secs_f64();
    println!(
        "| FastCDC cut points | {} | {count} chunks |",
        throughput(data.len(), seconds)
    );

    let start = Instant::now();
    let chunks = chunk_all(data, params)?;
    let seconds = start.elapsed().as_secs_f64();
    println!(
        "| chunk + BLAKE3 (chunk ids and file hash) | {} | mean chunk {:.0} KiB |",
        throughput(data.len(), seconds),
        data.len() as f64 / chunks.len() as f64 / 1024.0
    );

    let mut frames = Vec::new();
    for level in [1, 3, 6, 9] {
        let start = Instant::now();
        let mut compressor = zstd::bulk::Compressor::new(level)?;
        let mut chunker = Chunker::new(data, params);
        let mut compressed = 0;
        let mut level_frames = Vec::new();
        while let Some(chunk) = chunker.next_chunk()? {
            let frame = compressor.compress(&chunk.data)?;
            compressed += frame.len();
            if level == 3 {
                level_frames.push((chunk.id, chunk.data.len(), frame));
            }
        }
        chunker.finish()?;
        let seconds = start.elapsed().as_secs_f64();
        println!(
            "| chunk + BLAKE3 + zstd level {level} | {} | ratio {:.2} |",
            throughput(data.len(), seconds),
            data.len() as f64 / compressed as f64
        );
        if level == 3 {
            frames = level_frames;
        }
    }

    let start = Instant::now();
    let mut decompressor = zstd::bulk::Decompressor::new()?;
    let mut raw_bytes = 0;
    for (id, len, frame) in &frames {
        let mut raw = Vec::with_capacity(*len);
        decompressor.decompress_to_buffer(frame, &mut raw)?;
        if raw.len() != *len || ChunkId::of(&raw) != *id {
            return Err("decompressed chunk does not verify".into());
        }
        raw_bytes += raw.len();
    }
    let seconds = start.elapsed().as_secs_f64();
    println!(
        "| zstd decompress + BLAKE3 verify (level 3 frames) | {} | |",
        throughput(raw_bytes, seconds)
    );
    println!();
    Ok(())
}

/// The real store code paths, including file I/O and the per-chunk cost of
/// files and flushes, for each parameter set.
fn store_pipeline(data: &[u8], scratch: &Path) -> Result<()> {
    let source = scratch.join("v0.sav");
    fs::write(&source, data)?;
    println!("## Store and transfer (disk)\n");
    println!(
        "Ingest: file to empty store. Transfer: every chunk read_compressed from one store, \
         put into a sink on another, then assembled into a file.\n"
    );
    println!(
        "| parameters | chunks | ingest, synced | ingest, unsynced | transfer, synced | transfer, unsynced |"
    );
    println!("|---|---|---|---|---|---|");
    for (index, params) in parameter_sets()?.into_iter().enumerate() {
        let mut row = Vec::new();
        let mut chunks = 0;
        for sync in [true, false] {
            let config = StoreConfig {
                sync_chunks: sync,
                ..StoreConfig::new(u64::MAX)
            };
            let server = ChunkStore::open(
                scratch.join(format!("server-{index}-{sync}")),
                config.clone(),
            )?;
            let start = Instant::now();
            let manifest = server.ingest(File::open(&source)?, params)?;
            row.push(throughput(data.len(), start.elapsed().as_secs_f64()));
            chunks = manifest.chunks().len();

            let client = ChunkStore::open(scratch.join(format!("client-{index}-{sync}")), config)?;
            let dest = scratch.join(format!("received-{index}-{sync}.sav"));
            let start = Instant::now();
            let mut sink = ChunkSink::open(&client, manifest)?;
            for entry in sink.missing() {
                sink.put(&entry.id, &server.read_compressed(&entry.id)?)?;
            }
            sink.finish(&dest)?;
            row.push(throughput(data.len(), start.elapsed().as_secs_f64()));
        }
        println!(
            "| {} | {chunks} | {} | {} | {} | {} |",
            describe(params),
            row[0],
            row[2],
            row[1],
            row[3]
        );
    }
    println!();

    let params = ChunkParams::DEFAULT;
    println!("| operation, {} | throughput | notes |", describe(params));
    println!("|---|---|---|");
    let server = ChunkStore::open(scratch.join("server-default"), StoreConfig::new(u64::MAX))?;
    let manifest = server.ingest(File::open(&source)?, params)?;
    let start = Instant::now();
    server.ingest(File::open(&source)?, params)?;
    println!(
        "| ingest again, every chunk already stored | {} | chunk + hash + lookups |",
        throughput(data.len(), start.elapsed().as_secs_f64())
    );
    let start = Instant::now();
    for entry in manifest.unique_chunks() {
        server.read_compressed(&entry.id)?;
    }
    println!(
        "| read_compressed every chunk (serving side) | {} | read + decompress + verify |",
        throughput(data.len(), start.elapsed().as_secs_f64())
    );
    let dest = scratch.join("assembled.sav");
    let start = Instant::now();
    server.assemble(&manifest, &dest)?;
    let seconds = start.elapsed().as_secs_f64();
    if fs::read(&dest)? != data {
        return Err("assembled file differs".into());
    }
    println!(
        "| assemble from store | {} | read + decompress + verify + write + flush |",
        throughput(data.len(), seconds)
    );
    println!();
    Ok(())
}

/// How much of each edited save a receiver holding the original can reuse,
/// per parameter set.
fn dedup(original: &[u8], edited: &[(String, Vec<u8>)]) -> Result<()> {
    println!("## Dedup after edits\n");
    println!(
        "Transfer = zstd level 3 bytes of the edited save's chunks that the original lacks, \
         against the compressed size of the whole edited save.\n"
    );
    println!(
        "| parameters | edits | chunks | new chunks | bytes reused | transfer | full | manifest |"
    );
    println!("|---|---|---|---|---|---|---|---|");
    let mut compressor = zstd::bulk::Compressor::new(3)?;
    for params in parameter_sets()? {
        let before = Manifest::compute(original, params)?;
        let known: HashSet<ChunkId> = before.chunks().iter().map(|entry| entry.id).collect();
        for (scenario, data) in edited {
            let after = Manifest::compute(&data[..], params)?;
            let mut reused = 0u64;
            let mut transfer = 0usize;
            let mut full = 0usize;
            let mut new_chunks = 0;
            let unique = after.unique_chunks();
            for entry in &unique {
                let start = usize::try_from(entry.offset)?;
                let chunk = &data[start..start + entry.len as usize];
                let frame_len = compressor.compress(chunk)?.len();
                full += frame_len;
                if known.contains(&entry.id) {
                    reused += u64::from(entry.len);
                } else {
                    transfer += frame_len;
                    new_chunks += 1;
                }
            }
            let unique_bytes: u64 = unique.iter().map(|entry| u64::from(entry.len)).sum();
            println!(
                "| {} | {scenario} | {} | {new_chunks} | {:.1}% | {:.1} MiB | {:.1} MiB | {:.0} KiB |",
                describe(params),
                unique.len(),
                100.0 * reused as f64 / unique_bytes as f64,
                transfer as f64 / MIB,
                full as f64 / MIB,
                after.to_bytes().len() as f64 / 1024.0
            );
        }
    }
    println!();
    Ok(())
}

fn chunk_all(data: &[u8], params: ChunkParams) -> Result<Vec<ChunkId>> {
    let mut chunker = Chunker::new(data, params);
    let mut ids = Vec::new();
    while let Some(chunk) = chunker.next_chunk()? {
        ids.push(chunk.id);
    }
    chunker.finish()?;
    Ok(ids)
}

fn describe(params: ChunkParams) -> String {
    format!(
        "{}K/{}K/{}K",
        params.min() >> 10,
        params.avg() >> 10,
        params.max() >> 10
    )
}

#[derive(Clone, Copy)]
enum Scenario {
    /// One 1 KiB insertion at the very start, which shifts every byte.
    InsertAtStart,
    /// Edits at uniformly random positions.
    Random(usize),
    /// Edits at random positions within one tenth of the file, like a table
    /// that changes between saves while the rest stays put.
    Clustered(usize),
}

impl Scenario {
    fn describe(self) -> String {
        match self {
            Self::InsertAtStart => "1 KiB inserted at start".into(),
            Self::Random(count) => format!("{count} random"),
            Self::Clustered(count) => format!("{count} in one 10% region"),
        }
    }

    fn apply(self, data: &[u8], seed: u64) -> Vec<u8> {
        let mut rng = Rng(seed);
        let (count, window) = match self {
            Self::InsertAtStart => {
                let mut edited = Vec::with_capacity(data.len() + 1024);
                edited.extend((0..1024).map(|_| rng.next_u64() as u8));
                edited.extend_from_slice(data);
                return edited;
            }
            Self::Random(count) => (count, 0..data.len()),
            Self::Clustered(count) => {
                let start = rng.below(data.len() as u64 * 9 / 10) as usize;
                (count, start..start + data.len() / 10)
            }
        };
        let mut positions: Vec<usize> = (0..count)
            .map(|_| window.start + rng.below(window.len() as u64) as usize)
            .collect();
        positions.sort_unstable();
        let mut edited = Vec::with_capacity(data.len() + count * 4096);
        let mut cursor = 0;
        for position in positions {
            let position = position.max(cursor);
            edited.extend_from_slice(&data[cursor..position]);
            cursor = position;
            match rng.below(100) {
                // Small in-place change: a position, a counter, a state byte.
                0..70 => {
                    let len = (1 + rng.below(64) as usize).min(data.len() - cursor);
                    edited.extend((0..len).map(|_| rng.next_u64() as u8));
                    cursor += len;
                }
                // A new record, or several.
                70..85 => {
                    let len = 16 + rng.below(4080) as usize;
                    let start = edited.len();
                    entity_records(&mut rng, &mut edited, start + len);
                }
                // Removed records.
                _ => {
                    let len = 16 + rng.below(4080) as usize;
                    cursor = (cursor + len).min(data.len());
                }
            }
        }
        edited.extend_from_slice(&data[cursor..]);
        edited
    }
}

/// SplitMix64.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound.max(1)
    }

    fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
}

fn synthetic_save(seed: u64, len: usize) -> Vec<u8> {
    let mut rng = Rng(seed);
    let mut out = Vec::with_capacity(len + (4 << 20));
    while out.len() < len {
        let section = (64 << 10) + rng.below((4 << 20) - (64 << 10)) as usize;
        let end = (out.len() + section).min(len);
        match rng.below(100) {
            0..45 => entity_records(&mut rng, &mut out, end),
            45..65 => terrain(&mut rng, &mut out, end),
            65..85 => text(&mut rng, &mut out, end),
            _ => {
                while out.len() < end {
                    out.extend_from_slice(&rng.next_u64().to_le_bytes());
                }
            }
        }
    }
    out.truncate(len);
    out
}

/// Fixed-layout records: type tag, id, position, heading, a per-type block
/// of settings, state bytes, a reference and a name index. Neighbouring
/// records are similar.
fn entity_records(rng: &mut Rng, out: &mut Vec<u8>, end: usize) {
    let kind = rng.below(12) as u16;
    let settings: Vec<u8> = (0..24).map(|_| rng.below(4) as u8).collect();
    let name = rng.below(300) as u16;
    let mut id = rng.below(1 << 20) as u32;
    let mut position = [rng.unit() * 4096.0, rng.unit() * 4096.0, rng.unit() * 200.0];
    while out.len() < end {
        id += 1 + rng.below(3) as u32;
        for axis in &mut position {
            *axis += (rng.unit() - 0.5) * 8.0;
        }
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&id.to_le_bytes());
        for axis in position {
            out.extend_from_slice(&axis.to_le_bytes());
        }
        // One of 16 headings.
        out.extend_from_slice(&(rng.below(16) as f32 * std::f32::consts::FRAC_PI_8).to_le_bytes());
        out.extend_from_slice(&settings);
        for _ in 0..8 {
            out.push(if rng.below(8) == 0 {
                rng.below(8) as u8
            } else {
                0
            });
        }
        out.extend_from_slice(&(id - rng.below(64) as u32).to_le_bytes());
        out.extend_from_slice(&(name + rng.below(2) as u16).to_le_bytes());
        out.extend(std::iter::repeat_n(0, rng.below(24) as usize));
    }
}

/// A height grid: smooth waves plus noise, as 32-bit floats.
fn terrain(rng: &mut Rng, out: &mut Vec<u8>, end: usize) {
    let (fx, fy) = (0.01 + rng.unit() * 0.05, 0.01 + rng.unit() * 0.05);
    let base = rng.unit() * 300.0;
    let mut cell = 0u32;
    while out.len() < end {
        let (x, y) = ((cell % 1024) as f32, (cell / 1024) as f32);
        let height = base + 40.0 * (x * fx).sin() + 25.0 * (y * fy).cos() + rng.unit() * 0.5;
        out.extend_from_slice(&height.to_le_bytes());
        cell += 1;
    }
}

/// Lua-like text: resource paths, identifiers and numbers.
fn text(rng: &mut Rng, out: &mut Vec<u8>, end: usize) {
    const WORDS: [&str; 16] = [
        "vehicle/train/",
        "station/rail/modular_station",
        "street/town_medium_new.lua",
        "industry/coal_mine.con",
        ".mdl",
        "construction",
        "params = { ",
        "}, ",
        "name = \"",
        "\", ",
        "town_building_",
        "track/high_speed.lua",
        "cargo = \"PASSENGERS\"",
        "capacity = ",
        "model = \"",
        "\n",
    ];
    while out.len() < end {
        out.extend_from_slice(WORDS[rng.below(16) as usize].as_bytes());
        if rng.below(3) == 0 {
            out.extend_from_slice(rng.below(100_000).to_string().as_bytes());
        }
    }
}
