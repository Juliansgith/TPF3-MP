//! Static proof of the resolver against a real game binary.
//!
//! Resolves the TPF2 build-35924 profile against the executable on disk and
//! checks that every target is found at its known RVA, uniquely, and that a
//! modified copy is refused. The executable is READ-ONLY: this test only reads
//! it. It is skipped when the file is absent (for example in CI).
//!
//! Scanning the on-disk file is a development convenience. It is valid here
//! because build 35924's `.text` is not encrypted (the SteamStub `.bind`
//! section leaves the code readable on disk), which this test's exact prologue
//! matches confirm. The production resolver scans the running process's mapped,
//! unpacked module image instead - the same [`resolve`] call, a different byte
//! source (see `docs/HOOKS.md`).

#![allow(clippy::unwrap_used)]

use std::path::Path;

use tpf3mp_hookcore::pe::PeHeaders;
use tpf3mp_hookcore::profile::{self, BuildIdentity, Profile, Refusal};

const EXE: &str = r"E:\SteamLibrary\steamapps\common\Transport Fever 2\TransportFever2.exe";
const PROFILE: &str = include_str!("data/tpf2_build35924.toml");

/// Known function RVAs (image base 0x140000000).
const TARGETS: &[(&str, u64)] = &[
    ("GameSim::Step", 0x15aa00),
    ("CGame::Step", 0x118e90),
    ("CGameTime::GetSpeed", 0x2877a0),
    ("UI::CMenuUI::StartSavegame", 0x6785c0),
    ("UI::CMenuUI::CreatePage", 0x663370),
];

#[test]
fn resolves_every_tpf2_target_uniquely_and_refuses_tampering() {
    let exe = Path::new(EXE);
    if !exe.exists() {
        eprintln!("skipping: {EXE} is not present (this is expected in CI)");
        return;
    }
    let image = std::fs::read(exe).unwrap();
    let profile = Profile::from_toml(PROFILE).unwrap();

    // The profile is for exactly this build.
    let identity = BuildIdentity::of_file(exe).unwrap();
    profile
        .verify_identity(&identity)
        .expect("the on-disk executable matches the pinned build identity");

    // A different build is refused up front.
    let stranger = BuildIdentity {
        sha256: "00".repeat(32),
        size: identity.size,
        pe_timestamp: identity.pe_timestamp,
    };
    assert!(matches!(
        profile.verify_identity(&stranger),
        Err(Refusal::UnknownBuild { .. })
    ));

    // Resolve over the on-disk .text. `region_base` is the section's RVA, so a
    // resolved address is the function's RVA.
    let pe = PeHeaders::parse(&image).unwrap();
    let text = pe.section(".text").expect("a .text section");
    let text_bytes = text.raw(&image).expect(".text raw bytes");
    let base = u64::from(text.virtual_address);

    let resolved = profile::resolve(&profile, text_bytes, base).unwrap();
    assert_eq!(resolved.targets.len(), TARGETS.len());
    assert!(resolved.absent_optional.is_empty());
    for &(name, rva) in TARGETS {
        let target = resolved
            .get(name)
            .unwrap_or_else(|| panic!("{name} did not resolve"));
        assert_eq!(target.address, rva, "{name} resolved to the wrong RVA");
    }

    // Every signature matched exactly once: corrupting one target's bytes makes
    // that required target vanish, and resolution fails closed.
    let mut tampered = text_bytes.to_vec();
    let index = (0x15aa00u64 - base) as usize; // GameSim::Step
    tampered[index] ^= 0xFF;
    let refusal = profile::resolve(&profile, &tampered, base).unwrap_err();
    assert!(
        matches!(refusal, Refusal::Missing { ref target } if target == "GameSim::Step"),
        "expected a Missing refusal, got {refusal:?}"
    );
}
