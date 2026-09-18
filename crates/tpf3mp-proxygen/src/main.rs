//! Command line front end for the proxy generator.
//!
//! ```text
//! tpf3mp-proxygen <source.dll> <out_dir> [proxy_stem] [real_stem]
//! ```
//!
//! Reads `source.dll`'s exports and writes a proxy crate into `out_dir` that
//! forwards each one to `real_stem.dll` (default: `<proxy_stem>_real`). Build
//! the crate with cargo to produce `proxy_stem.dll`.

use std::path::PathBuf;
use std::process::ExitCode;

use tpf3mp_proxygen::{parse_exports, write_proxy};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let program = args
        .first()
        .map(String::as_str)
        .unwrap_or("tpf3mp-proxygen");
    if args.len() < 3 {
        eprintln!("usage: {program} <source.dll> <out_dir> [proxy_stem] [real_stem]");
        return ExitCode::FAILURE;
    }

    let source = PathBuf::from(&args[1]);
    let out_dir = PathBuf::from(&args[2]);
    let proxy_stem = args.get(3).cloned().unwrap_or_else(|| {
        source
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("proxy")
            .to_owned()
    });
    let real_stem = args
        .get(4)
        .cloned()
        .unwrap_or_else(|| format!("{proxy_stem}_real"));

    let bytes = match std::fs::read(&source) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("cannot read {}: {error}", source.display());
            return ExitCode::FAILURE;
        }
    };
    let exports = match parse_exports(&bytes) {
        Ok(exports) => exports,
        Err(error) => {
            eprintln!("cannot parse exports of {}: {error}", source.display());
            return ExitCode::FAILURE;
        }
    };
    match write_proxy(&out_dir, &exports, &proxy_stem, &real_stem) {
        Ok(dir) => {
            println!(
                "wrote proxy crate to {} ({} exports forwarded to {real_stem}.dll)",
                dir.display(),
                exports.entries.len()
            );
            println!(
                "build it with: cargo build --manifest-path {}",
                dir.join("Cargo.toml").display()
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("cannot write proxy crate: {error}");
            ExitCode::FAILURE
        }
    }
}
