//! Versioned per-build profiles and their resolution.
//!
//! A profile binds one game build (by executable SHA-256, and optionally size
//! and PE timestamp) to a set of named [`TargetSpec`]s. Each target is a
//! [`Pattern`] signature, an `offset` from the match to the target address, the
//! exact `prologue` bytes expected there, and a `required` flag.
//!
//! [`resolve`] turns a profile plus a module image into either a complete,
//! verified [`ResolvedProfile`] or a precise [`Refusal`]. The rule is
//! fail-closed: if any *required* target is missing, ambiguous, or has the
//! wrong prologue, resolution fails as a whole and installs nothing. This is
//! what keeps the hook off an unknown or patched build instead of patching
//! blindly (see `docs/HOOKS.md`).

use std::{fs::File, io, io::Read, path::Path};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::pattern::{Pattern, PatternError, ScanError};
use crate::pe::PeHeaders;

/// Identifies one exact game build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildIdentity {
    /// Lowercase hex SHA-256 of the executable file.
    pub sha256: String,
    /// File size in bytes, if known.
    pub size: Option<u64>,
    /// PE `TimeDateStamp`, if known.
    pub pe_timestamp: Option<u32>,
}

impl BuildIdentity {
    /// Computes the identity of an executable file: its SHA-256 and size, plus
    /// the PE timestamp when the file is a PE image (a non-PE file just leaves
    /// that field `None`).
    pub fn of_file(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 1 << 16];
        let mut size = 0u64;
        // Also keep the first bytes so the PE timestamp can be read without a
        // second pass over a large executable.
        let mut head = Vec::new();
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            size += read as u64;
            if head.len() < 4096 {
                head.extend_from_slice(&buffer[..read.min(4096 - head.len())]);
            }
        }
        let sha256 = hex_lower(&hasher.finalize());
        let pe_timestamp = PeHeaders::parse(&head).ok().map(|pe| pe.timestamp);
        Ok(Self {
            sha256,
            size: Some(size),
            pe_timestamp,
        })
    }

    /// Computes the identity of an in-memory image.
    pub fn of_bytes(bytes: &[u8]) -> Self {
        let sha256 = hex_lower(&Sha256::digest(bytes));
        let pe_timestamp = PeHeaders::parse(bytes).ok().map(|pe| pe.timestamp);
        Self {
            sha256,
            size: Some(bytes.len() as u64),
            pe_timestamp,
        }
    }
}

/// One hook target, with its signature already compiled.
#[derive(Debug, Clone)]
pub struct TargetSpec {
    pub name: String,
    /// The IDA-style signature source, kept for diagnostics.
    pub signature: String,
    pattern: Pattern,
    /// Byte offset from the match start to the target address. Usually 0, but a
    /// signature may begin before or after the function it names.
    pub offset: i64,
    /// Exact bytes expected at the target address, re-checked after the scan.
    pub prologue: Vec<u8>,
    /// A required target must resolve, or the whole profile is refused.
    pub required: bool,
}

impl TargetSpec {
    pub fn pattern(&self) -> &Pattern {
        &self.pattern
    }
}

/// A parsed, validated profile.
#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub build: BuildIdentity,
    /// The image base the target addresses are expressed against (informational).
    pub image_base: Option<u64>,
    /// The section a resolver is expected to scan, e.g. `.text` (informational).
    pub region: Option<String>,
    pub targets: Vec<TargetSpec>,
}

#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("the profile is not valid TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("the profile lists no targets")]
    NoTargets,
    #[error("target {name:?} appears more than once")]
    DuplicateTarget { name: String },
    #[error("target {name:?} has an invalid signature: {source}")]
    Signature {
        name: String,
        #[source]
        source: PatternError,
    },
    #[error("target {name:?} has an invalid prologue: {source}")]
    Prologue {
        name: String,
        #[source]
        source: HexError,
    },
}

impl Profile {
    /// Parses a profile from TOML (see `docs/HOOKS.md` for the format).
    pub fn from_toml(text: &str) -> Result<Self, ProfileError> {
        let raw: RawProfile = toml::from_str(text)?;
        if raw.targets.is_empty() {
            return Err(ProfileError::NoTargets);
        }
        let mut targets = Vec::with_capacity(raw.targets.len());
        for target in raw.targets {
            if targets
                .iter()
                .any(|existing: &TargetSpec| existing.name == target.name)
            {
                return Err(ProfileError::DuplicateTarget { name: target.name });
            }
            let pattern =
                Pattern::parse(&target.signature).map_err(|source| ProfileError::Signature {
                    name: target.name.clone(),
                    source,
                })?;
            let prologue =
                parse_hex_bytes(&target.prologue).map_err(|source| ProfileError::Prologue {
                    name: target.name.clone(),
                    source,
                })?;
            targets.push(TargetSpec {
                name: target.name,
                signature: target.signature,
                pattern,
                offset: target.offset,
                prologue,
                required: target.required,
            });
        }
        Ok(Self {
            name: raw.name,
            build: BuildIdentity {
                sha256: raw.build.sha256.to_ascii_lowercase(),
                size: raw.build.size,
                pe_timestamp: raw.build.pe_timestamp,
            },
            image_base: raw.image_base,
            region: raw.region,
            targets,
        })
    }

    /// Checks a running build's identity against this profile. The SHA-256 must
    /// match; declared size and timestamp must match when the profile pins them.
    pub fn verify_identity(&self, actual: &BuildIdentity) -> Result<(), Refusal> {
        if self.build.sha256 != actual.sha256.to_ascii_lowercase() {
            return Err(Refusal::UnknownBuild {
                expected_sha256: self.build.sha256.clone(),
                actual_sha256: actual.sha256.to_ascii_lowercase(),
            });
        }
        if let (Some(expected), Some(actual_size)) = (self.build.size, actual.size)
            && expected != actual_size
        {
            return Err(Refusal::UnknownBuild {
                expected_sha256: self.build.sha256.clone(),
                actual_sha256: format!("size {actual_size}"),
            });
        }
        if let (Some(expected), Some(actual_ts)) = (self.build.pe_timestamp, actual.pe_timestamp)
            && expected != actual_ts
        {
            return Err(Refusal::UnknownBuild {
                expected_sha256: self.build.sha256.clone(),
                actual_sha256: format!("timestamp {actual_ts:#010x}"),
            });
        }
        Ok(())
    }
}

/// A target located and verified within an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    pub name: String,
    /// Absolute address: `region_base + match_index + offset` (see [`resolve`]).
    pub address: u64,
    /// Index of the target within the scanned image slice.
    pub image_index: usize,
    pub required: bool,
}

/// The result of resolving every target in a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProfile {
    pub name: String,
    pub targets: Vec<ResolvedTarget>,
    /// Optional targets that were absent. Never contains a required target: a
    /// missing required target is a [`Refusal`] instead.
    pub absent_optional: Vec<String>,
}

impl ResolvedProfile {
    pub fn get(&self, name: &str) -> Option<&ResolvedTarget> {
        self.targets.iter().find(|t| t.name == name)
    }
}

/// Why a profile could not be resolved. Every variant names the target (or
/// build) at fault so the refusal can be logged precisely.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Refusal {
    #[error("unknown build: profile is for {expected_sha256}, running image is {actual_sha256}")]
    UnknownBuild {
        expected_sha256: String,
        actual_sha256: String,
    },
    #[error("required target {target:?} was not found")]
    Missing { target: String },
    #[error("target {target:?} matched {count} times; the signature is not unique")]
    Ambiguous { target: String, count: usize },
    #[error("target {target:?} resolves outside the scanned image")]
    OutOfBounds { target: String },
    #[error(
        "target {target:?} prologue mismatch at image index {image_index}: \
         expected {expected}, found {found}"
    )]
    PrologueMismatch {
        target: String,
        image_index: usize,
        expected: String,
        found: String,
    },
}

/// Resolves every target in `profile` against `image`, a contiguous slice whose
/// first byte corresponds to address `region_base` (for a section this is the
/// section's virtual address; for a whole on-disk file it is 0).
///
/// On success every required target - and every optional target that is
/// present - is verified against its expected prologue. On the first required
/// failure it returns a [`Refusal`] and nothing is resolved, so required hooks
/// are all-or-nothing. An *ambiguous* optional target is also refused: an
/// unexpected second match is a corruption signal, not something to skip.
pub fn resolve(
    profile: &Profile,
    image: &[u8],
    region_base: u64,
) -> Result<ResolvedProfile, Refusal> {
    let mut targets = Vec::with_capacity(profile.targets.len());
    let mut absent_optional = Vec::new();
    for spec in &profile.targets {
        let match_index = match spec.pattern.find_unique(image) {
            Ok(index) => index,
            Err(ScanError::NotFound) => {
                if spec.required {
                    return Err(Refusal::Missing {
                        target: spec.name.clone(),
                    });
                }
                absent_optional.push(spec.name.clone());
                continue;
            }
            Err(ScanError::Ambiguous { count }) => {
                return Err(Refusal::Ambiguous {
                    target: spec.name.clone(),
                    count,
                });
            }
        };
        let image_index = i64::try_from(match_index)
            .ok()
            .and_then(|base| base.checked_add(spec.offset))
            .and_then(|index| usize::try_from(index).ok())
            .ok_or_else(|| Refusal::OutOfBounds {
                target: spec.name.clone(),
            })?;
        let end = image_index
            .checked_add(spec.prologue.len())
            .ok_or_else(|| Refusal::OutOfBounds {
                target: spec.name.clone(),
            })?;
        let found = image
            .get(image_index..end)
            .ok_or_else(|| Refusal::OutOfBounds {
                target: spec.name.clone(),
            })?;
        if found != spec.prologue.as_slice() {
            return Err(Refusal::PrologueMismatch {
                target: spec.name.clone(),
                image_index,
                expected: hex_spaced(&spec.prologue),
                found: hex_spaced(found),
            });
        }
        targets.push(ResolvedTarget {
            name: spec.name.clone(),
            address: region_base + image_index as u64,
            image_index,
            required: spec.required,
        });
    }
    Ok(ResolvedProfile {
        name: profile.name.clone(),
        targets,
        absent_optional,
    })
}

#[derive(Deserialize)]
struct RawProfile {
    name: String,
    image_base: Option<u64>,
    region: Option<String>,
    build: RawBuild,
    #[serde(default, rename = "target")]
    targets: Vec<RawTarget>,
}

#[derive(Deserialize)]
struct RawBuild {
    sha256: String,
    size: Option<u64>,
    pe_timestamp: Option<u32>,
}

#[derive(Deserialize)]
struct RawTarget {
    name: String,
    signature: String,
    #[serde(default)]
    offset: i64,
    prologue: String,
    #[serde(default = "default_required")]
    required: bool,
}

fn default_required() -> bool {
    true
}

/// Error parsing a concrete (wildcard-free) hex byte string.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HexError {
    #[error("token {token:?} is not two hex digits")]
    BadToken { token: String },
    #[error("prologue has no bytes")]
    Empty,
}

/// Parses `"40 53 41"` into bytes. Unlike a [`Pattern`], a prologue must be
/// fully concrete: it is the exact code the detour engine will relocate.
fn parse_hex_bytes(text: &str) -> Result<Vec<u8>, HexError> {
    let mut bytes = Vec::new();
    for token in text.split_whitespace() {
        if token.len() != 2 {
            return Err(HexError::BadToken {
                token: token.to_owned(),
            });
        }
        let value = u8::from_str_radix(token, 16).map_err(|_| HexError::BadToken {
            token: token.to_owned(),
        })?;
        bytes.push(value);
    }
    if bytes.is_empty() {
        return Err(HexError::Empty);
    }
    Ok(bytes)
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn hex_spaced(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        out.push_str(&format!("{byte:02X}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
name = "Test build"
image_base = 0x140000000
region = ".text"

[build]
sha256 = "AABBCC"
size = 1024
pe_timestamp = 0x675ABCC6

[[target]]
name = "alpha"
signature = "40 53 41 56"
prologue = "40 53 41 56"
required = true

[[target]]
name = "beta"
signature = "E8 ?? ?? ?? ?? 8B 40 04"
offset = 5
prologue = "8B 40 04"
required = false
"#;

    fn image() -> Vec<u8> {
        // 0..: alpha match, then a call whose rel32 is wildcarded, then beta.
        let mut bytes = vec![0x90; 8];
        bytes.extend_from_slice(&[0x40, 0x53, 0x41, 0x56]); // alpha at 8
        bytes.extend_from_slice(&[0xE8, 0x11, 0x22, 0x33, 0x44, 0x8B, 0x40, 0x04]); // beta sig at 12, target at 17
        bytes.extend_from_slice(&[0x90; 8]);
        bytes
    }

    #[test]
    fn parses_a_profile() {
        let profile = Profile::from_toml(SAMPLE).unwrap();
        assert_eq!(profile.name, "Test build");
        assert_eq!(profile.image_base, Some(0x1_4000_0000));
        assert_eq!(profile.build.sha256, "aabbcc");
        assert_eq!(profile.build.pe_timestamp, Some(0x675A_BCC6));
        assert_eq!(profile.targets.len(), 2);
        assert!(profile.targets[0].required);
        assert!(!profile.targets[1].required);
    }

    #[test]
    fn resolves_all_targets_with_offsets() {
        let profile = Profile::from_toml(SAMPLE).unwrap();
        let resolved = resolve(&profile, &image(), 0x1000).unwrap();
        assert_eq!(resolved.get("alpha").unwrap().address, 0x1000 + 8);
        // beta's signature starts at 12; its target is +5, i.e. index 17.
        assert_eq!(resolved.get("beta").unwrap().address, 0x1000 + 17);
        assert!(resolved.absent_optional.is_empty());
    }

    #[test]
    fn missing_required_target_is_refused() {
        let profile = Profile::from_toml(SAMPLE).unwrap();
        let mut broken = image();
        broken[8] = 0x00; // corrupt alpha's signature
        assert_eq!(
            resolve(&profile, &broken, 0x1000),
            Err(Refusal::Missing {
                target: "alpha".into()
            })
        );
    }

    #[test]
    fn missing_optional_target_is_recorded_not_refused() {
        let profile = Profile::from_toml(SAMPLE).unwrap();
        let mut bytes = image();
        // Break beta's fixed opcode; alpha still resolves.
        bytes[12] = 0x00;
        let resolved = resolve(&profile, &bytes, 0x1000).unwrap();
        assert!(resolved.get("beta").is_none());
        assert_eq!(resolved.absent_optional, vec!["beta".to_string()]);
    }

    #[test]
    fn prologue_mismatch_is_refused() {
        // A signature that matches but whose target bytes are not the prologue.
        let toml = r#"
name = "x"
[build]
sha256 = "00"
[[target]]
name = "t"
signature = "90 90"
prologue = "40 53"
"#;
        let profile = Profile::from_toml(toml).unwrap();
        let image = [0x90u8, 0x90, 0x41, 0x42];
        assert!(matches!(
            resolve(&profile, &image, 0),
            Err(Refusal::PrologueMismatch { .. })
        ));
    }

    #[test]
    fn ambiguous_signature_is_refused() {
        let toml = r#"
name = "x"
[build]
sha256 = "00"
[[target]]
name = "t"
signature = "90 90"
prologue = "90 90"
"#;
        let profile = Profile::from_toml(toml).unwrap();
        let image = [0x90u8, 0x90, 0x90, 0x90];
        assert!(matches!(
            resolve(&profile, &image, 0),
            Err(Refusal::Ambiguous { count: 3, .. })
        ));
    }

    #[test]
    fn unknown_build_is_refused_by_identity() {
        let profile = Profile::from_toml(SAMPLE).unwrap();
        let actual = BuildIdentity {
            sha256: "ffffff".into(),
            size: Some(1024),
            pe_timestamp: None,
        };
        assert!(matches!(
            profile.verify_identity(&actual),
            Err(Refusal::UnknownBuild { .. })
        ));
    }

    #[test]
    fn identity_accepts_the_declared_build() {
        let profile = Profile::from_toml(SAMPLE).unwrap();
        let actual = BuildIdentity {
            sha256: "aabbcc".into(),
            size: Some(1024),
            pe_timestamp: Some(0x675A_BCC6),
        };
        assert_eq!(profile.verify_identity(&actual), Ok(()));
    }

    #[test]
    fn duplicate_target_names_are_rejected() {
        let toml = r#"
name = "x"
[build]
sha256 = "00"
[[target]]
name = "t"
signature = "90"
prologue = "90"
[[target]]
name = "t"
signature = "91"
prologue = "91"
"#;
        assert!(matches!(
            Profile::from_toml(toml),
            Err(ProfileError::DuplicateTarget { .. })
        ));
    }
}
