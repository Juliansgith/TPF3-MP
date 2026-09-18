//! End-to-end proxy generation against a real Windows system DLL. Windows-only;
//! on other platforms this test file compiles to nothing.

#![cfg(windows)]
#![allow(clippy::unwrap_used)]

use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use tpf3mp_proxygen::{parse_exports, write_proxy};

fn scratch(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "tpf3mp-proxygen-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

#[test]
fn generates_and_builds_a_forwarding_proxy() {
    // A small, always-present system DLL. Copy it so we never touch the original.
    let system_dll = PathBuf::from(r"C:\Windows\System32\version.dll");
    if !system_dll.exists() {
        eprintln!("skipping: {} not present", system_dll.display());
        return;
    }
    let root = scratch("build");
    fs::create_dir_all(&root).unwrap();

    let source_bytes = fs::read(&system_dll).unwrap();
    let exports = parse_exports(&source_bytes).unwrap();
    assert!(!exports.entries.is_empty(), "version.dll exports functions");
    let source_names: BTreeSet<String> = exports.named().map(|(n, _)| n.to_owned()).collect();
    assert!(source_names.contains("GetFileVersionInfoW"));

    let crate_dir = root.join("proxy");
    write_proxy(&crate_dir, &exports, "version", "version_real").unwrap();
    // The .def has a forwarder line per named export.
    let def = fs::read_to_string(crate_dir.join("version.def")).unwrap();
    for name in &source_names {
        assert!(
            def.contains(&format!("{name}=version_real.{name}")),
            "def missing forwarder for {name}"
        );
    }

    // Build the proxy in an isolated target dir. `env!("CARGO")` is the cargo
    // that is running this test.
    let target_dir = root.join("target");
    let build = Command::new(env!("CARGO"))
        .arg("build")
        .arg("--offline")
        .arg("--manifest-path")
        .arg(crate_dir.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", &target_dir)
        .status();

    match build {
        Ok(status) if status.success() => {
            let dll = target_dir.join("debug").join("version.dll");
            assert!(dll.exists(), "the proxy DLL was built at {}", dll.display());
            let proxy_bytes = fs::read(&dll).unwrap();
            let proxy_exports = parse_exports(&proxy_bytes).unwrap();
            let proxy_names: BTreeSet<String> =
                proxy_exports.named().map(|(n, _)| n.to_owned()).collect();
            assert_eq!(
                proxy_names, source_names,
                "proxy exports match the original"
            );
            assert!(
                proxy_exports.entries.iter().all(|e| e.forwarder),
                "every proxy export is a forwarder"
            );
        }
        Ok(status) => panic!("proxy build failed with {status}"),
        Err(error) => eprintln!("skipping build check (could not run cargo: {error})"),
    }

    let _ = fs::remove_dir_all(&root);
}
