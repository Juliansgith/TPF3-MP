//! The detour engine on non-x86-64 targets: it compiles, and every install
//! reports [`DetourError::UnsupportedArchitecture`] so the caller can fail
//! closed (see `docs/HOOKS.md`, macOS arm64).

// `install`/`detach` keep the `unsafe fn` signature they have on x86-64 so the
// public API is identical across architectures, even though nothing here is
// actually unsafe.
#![allow(unsafe_code)]

use super::DetourError;

/// A placeholder that can never be constructed on this architecture.
pub enum InlineDetour {}

impl InlineDetour {
    /// Always fails on a non-x86-64 build.
    ///
    /// # Safety
    ///
    /// Never installs anything, so there is nothing to uphold; the `unsafe`
    /// keeps one signature across architectures.
    pub unsafe fn install(_target: *mut u8, _detour: *const u8) -> Result<Self, DetourError> {
        Err(DetourError::UnsupportedArchitecture {
            arch: std::env::consts::ARCH,
        })
    }

    pub fn target(&self) -> *mut u8 {
        match *self {}
    }

    pub fn trampoline(&self) -> *const u8 {
        match *self {}
    }

    pub fn steal_len(&self) -> usize {
        match *self {}
    }

    /// # Safety
    ///
    /// Unreachable: `Self` cannot be constructed on this architecture.
    pub unsafe fn detach(self) -> Result<(), DetourError> {
        match self {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_reports_unsupported_architecture() {
        // SAFETY: on this architecture `install` returns before touching either
        // pointer, so dangling pointers are fine.
        let result = unsafe { InlineDetour::install(core::ptr::null_mut(), core::ptr::null()) };
        assert!(matches!(
            result,
            Err(DetourError::UnsupportedArchitecture { .. })
        ));
    }
}
