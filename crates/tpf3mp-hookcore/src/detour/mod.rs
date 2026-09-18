//! An x86-64 inline detour engine.
//!
//! Installing a detour overwrites a function's first instructions with a jump to
//! a replacement, after copying those instructions into a *trampoline* that ends
//! by jumping back into the function. Calling the trampoline therefore runs the
//! original code. The stolen instructions are relocated with iced-x86's block
//! encoder, so a RIP-relative operand still addresses the same absolute memory
//! from its new home; a prologue that cannot be relocated (it branches, returns,
//! or is undecodable) is refused rather than patched wrong.
//!
//! # Why iced-x86 and our own trampoline, not `retour`
//!
//! `retour` (stable 0.3.1; its 0.4 line is alpha) wraps this whole dance behind
//! `GenericDetour`, but it owns trampoline allocation and instruction relocation
//! internally, which is exactly the part we must be able to test and reason
//! about byte for byte on a game binary that shifts every patch. iced-x86 is a
//! pure-Rust decoder and block encoder with no build script and no runtime
//! allocation of its own; we drive the decode/relocate step directly and keep
//! the trampoline and patch code here, where the tests can see them. The engine
//! is x86-64 only by construction; iced-x86 is pulled in only on that target.
//!
//! # Thread-safety
//!
//! Patching overwrites up to fourteen live code bytes with a non-atomic copy.
//! The caller must guarantee the target cannot execute during install or
//! uninstall: either install before the target's first run (the proxy-DLL and
//! `LD_PRELOAD` loaders both run before the game's entry point), or park every
//! thread that could reach it first. The engine does not itself stop threads.

use thiserror::Error;

/// Why a detour could not be installed or removed.
#[derive(Debug, Error)]
pub enum DetourError {
    #[error("the inline detour engine supports x86-64 only; this build targets {arch}")]
    UnsupportedArchitecture { arch: &'static str },
    #[error("unsupported prologue: {reason}")]
    UnsupportedPrologue { reason: String },
    #[error("could not re-encode the relocated prologue: {0}")]
    Encode(String),
    #[error("could not change memory protection (os error {0})")]
    Protect(i32),
    #[error("could not allocate executable memory (os error {0})")]
    Alloc(i32),
}

#[cfg(target_arch = "x86_64")]
mod sys;
#[cfg(target_arch = "x86_64")]
mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64::InlineDetour;

#[cfg(not(target_arch = "x86_64"))]
mod unsupported;
#[cfg(not(target_arch = "x86_64"))]
pub use unsupported::InlineDetour;
