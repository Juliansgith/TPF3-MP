//! Game-agnostic building blocks for the in-process native hook.
//!
//! Transport Fever 3 is patched often, so the hook never pins raw addresses.
//! Instead it carries one [`profile`] per game build: a build identity plus a
//! set of named targets located by byte [`pattern`]s. [`profile::resolve`]
//! turns a profile and a module image into either a fully verified target
//! table or a precise refusal, and refuses to install a partial set of required
//! hooks. Only once a target is resolved does the [`detour`] engine patch it.
//!
//! The crate is deliberately free of any TPF3 knowledge: the signatures live in
//! profile files (see `docs/HOOKS.md`), so a new build is a data change, not a
//! code change. Everything here is host-independent except [`detour`], which is
//! x86-64 only and reports [`detour::DetourError::UnsupportedArchitecture`]
//! elsewhere.

pub mod detour;
pub mod pattern;
pub mod pe;
pub mod profile;

pub use pattern::{Pattern, PatternError, ScanError};
pub use profile::{
    BuildIdentity, Profile, ProfileError, Refusal, ResolvedProfile, ResolvedTarget, TargetSpec,
};
