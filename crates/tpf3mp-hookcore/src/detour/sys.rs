//! Platform executable-memory primitives for the x86-64 detour engine:
//! allocate a trampoline buffer near its target, seal it read-and-execute
//! once written, and overwrite live code bytes.
//!
//! On x86-64 the instruction cache is coherent with data writes on the same
//! core, and both loaders install detours before the target runs on any core,
//! so a store fence is enough on Unix; Windows still calls
//! `FlushInstructionCache`, which is free when nothing needs flushing.

#![allow(unsafe_code)]

use super::DetourError;

/// How far from its target a trampoline may be. A relocated RIP-relative
/// operand keeps a 32-bit displacement to data near the target, so the
/// trampoline stays well within 2 GiB of it.
const NEAR: usize = 1 << 30;

fn last_os_error() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
}

/// A memory region that holds a trampoline: writable until
/// [`make_executable`], then read-and-execute only. Freed on drop.
pub struct ExecBuffer {
    ptr: *mut u8,
    len: usize,
}

impl ExecBuffer {
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr
    }

    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr
    }

    #[allow(clippy::len_without_is_empty)] // a trampoline buffer is never empty
    pub fn len(&self) -> usize {
        self.len
    }
}

#[cfg(windows)]
mod imp {
    use super::{DetourError, ExecBuffer, NEAR, last_os_error};
    use core::ffi::c_void;
    use core::ptr;
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_FREE, MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION,
        PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, PAGE_GUARD, PAGE_NOACCESS, PAGE_READWRITE,
        VirtualAlloc, VirtualFree, VirtualProtect, VirtualQuery,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    /// Windows reserves memory in 64 KiB units.
    const GRANULARITY: usize = 0x1_0000;

    fn query(address: usize) -> Option<MEMORY_BASIC_INFORMATION> {
        // SAFETY: an all-zero MEMORY_BASIC_INFORMATION is a valid value to
        // overwrite, and VirtualQuery writes at most its size.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { core::mem::zeroed() };
        // SAFETY: VirtualQuery only inspects the address space.
        let written = unsafe {
            VirtualQuery(
                address as *const c_void,
                &mut info,
                core::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        (written != 0).then_some(info)
    }

    fn commit_at(base: usize, len: usize) -> Option<ExecBuffer> {
        // SAFETY: asking for a specific base in a free region; VirtualAlloc
        // returns null if it is not free after all.
        let ptr = unsafe {
            VirtualAlloc(
                base as *const c_void,
                len,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        (!ptr.is_null()).then(|| ExecBuffer {
            ptr: ptr.cast::<u8>(),
            len,
        })
    }

    /// The base in a free region closest to the target: its highest fitting
    /// base below the target, its lowest above.
    fn base_in(info: &MEMORY_BASIC_INFORMATION, len: usize, below: bool) -> Option<usize> {
        if info.State != MEM_FREE {
            return None;
        }
        let start = info.BaseAddress as usize;
        let end = start.checked_add(info.RegionSize)?;
        let base = if below {
            end.checked_sub(len)? & !(GRANULARITY - 1)
        } else {
            start.checked_add(GRANULARITY - 1)? & !(GRANULARITY - 1)
        };
        (base >= start && base.checked_add(len)? <= end).then_some(base)
    }

    /// A read-write buffer within [`NEAR`] of `target`, walking the free
    /// regions below it and then above it; anywhere if none is free.
    pub fn alloc_near(target: usize, len: usize) -> Result<ExecBuffer, DetourError> {
        let low = target.saturating_sub(NEAR).max(GRANULARITY);
        let high = target.saturating_add(NEAR);
        let mut address = target;
        while address >= low {
            let Some(info) = query(address) else { break };
            if let Some(base) = base_in(&info, len, true)
                && base >= low
                && let Some(buffer) = commit_at(base, len)
            {
                return Ok(buffer);
            }
            let Some(below) = (info.BaseAddress as usize).checked_sub(1) else {
                break;
            };
            address = below;
        }
        let mut address = target;
        while address < high {
            let Some(info) = query(address) else { break };
            if let Some(base) = base_in(&info, len, false)
                && base.saturating_add(len) <= high
                && let Some(buffer) = commit_at(base, len)
            {
                return Ok(buffer);
            }
            let Some(above) = (info.BaseAddress as usize).checked_add(info.RegionSize) else {
                break;
            };
            address = above;
        }
        alloc(len)
    }

    pub fn alloc(len: usize) -> Result<ExecBuffer, DetourError> {
        // SAFETY: a fresh reservation with a null base; VirtualAlloc returns a
        // valid committed read-write region of `len` bytes or null.
        let ptr =
            unsafe { VirtualAlloc(ptr::null(), len, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
        if ptr.is_null() {
            return Err(DetourError::Alloc(last_os_error()));
        }
        Ok(ExecBuffer {
            ptr: ptr.cast::<u8>(),
            len,
        })
    }

    pub fn make_executable(buffer: &ExecBuffer) -> Result<(), DetourError> {
        let mut old = 0u32;
        // SAFETY: the buffer is our own committed allocation.
        let ok = unsafe {
            VirtualProtect(
                buffer.ptr.cast::<c_void>(),
                buffer.len,
                PAGE_EXECUTE_READ,
                &mut old,
            )
        };
        if ok == 0 {
            return Err(DetourError::Protect(last_os_error()));
        }
        Ok(())
    }

    /// How many of the `want` bytes at `address` can be read: up to the end
    /// of its committed, accessible region.
    pub fn readable(address: usize, want: usize) -> usize {
        match query(address) {
            Some(info)
                if info.State == MEM_COMMIT && info.Protect & (PAGE_NOACCESS | PAGE_GUARD) == 0 =>
            {
                let end = (info.BaseAddress as usize).saturating_add(info.RegionSize);
                want.min(end.saturating_sub(address))
            }
            _ => 0,
        }
    }

    pub fn free(buffer: &mut ExecBuffer) {
        // SAFETY: `ptr` came from VirtualAlloc; MEM_RELEASE requires size 0.
        unsafe {
            VirtualFree(buffer.ptr.cast::<c_void>(), 0, MEM_RELEASE);
        }
    }

    pub unsafe fn write_code(dst: *mut u8, bytes: &[u8]) -> Result<(), DetourError> {
        let mut old = 0u32;
        // SAFETY: `dst..dst+len` is live code the caller has made quiescent.
        // Make it writable, copy, then restore the previous protection.
        let ok = unsafe {
            VirtualProtect(
                dst.cast::<c_void>(),
                bytes.len(),
                PAGE_EXECUTE_READWRITE,
                &mut old,
            )
        };
        if ok == 0 {
            return Err(DetourError::Protect(last_os_error()));
        }
        // SAFETY: dst is now writable for `bytes.len()` bytes.
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
        }
        let mut ignored = 0u32;
        // SAFETY: restore the region's original protection flags.
        unsafe {
            VirtualProtect(dst.cast::<c_void>(), bytes.len(), old, &mut ignored);
        }
        // SAFETY: flush the modified range so the CPU refetches it.
        unsafe {
            FlushInstructionCache(GetCurrentProcess(), dst.cast::<c_void>(), bytes.len());
        }
        Ok(())
    }

    pub unsafe fn flush_icache(addr: *mut u8, len: usize) {
        // SAFETY: flushing an already-written, valid range is always sound.
        unsafe {
            FlushInstructionCache(GetCurrentProcess(), addr.cast::<c_void>(), len);
        }
    }
}

#[cfg(unix)]
mod imp {
    use super::{DetourError, ExecBuffer, NEAR, last_os_error};
    use core::ffi::c_void;
    use core::ptr;
    use libc::{
        _SC_PAGESIZE, MAP_ANON, MAP_FAILED, MAP_PRIVATE, PROT_EXEC, PROT_READ, PROT_WRITE, mmap,
        mprotect, munmap, sysconf,
    };

    /// Steps between the addresses tried near a target.
    const STEP: usize = 64 << 20;

    fn page_size() -> usize {
        // SAFETY: sysconf(_SC_PAGESIZE) has no preconditions.
        let value = unsafe { sysconf(_SC_PAGESIZE) };
        if value > 0 { value as usize } else { 4096 }
    }

    fn map(hint: *mut c_void, len: usize) -> Option<ExecBuffer> {
        // SAFETY: an anonymous private read-write mapping of `len` bytes; the
        // hint is only a suggestion, and MAP_FAILED signals failure.
        let ptr = unsafe {
            mmap(
                hint,
                len,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANON,
                -1,
                0,
            )
        };
        (ptr != MAP_FAILED).then(|| ExecBuffer {
            ptr: ptr.cast::<u8>(),
            len,
        })
    }

    /// A read-write buffer within [`NEAR`] of `target`: the kernel places a
    /// mapping at a free hinted address, so hints step away from the target
    /// in both directions. Anywhere if none lands near.
    pub fn alloc_near(target: usize, len: usize) -> Result<ExecBuffer, DetourError> {
        let page = page_size();
        for distance in (STEP..NEAR).step_by(STEP) {
            for hint in [target.checked_sub(distance), target.checked_add(distance)] {
                let Some(hint) = hint else { continue };
                let Some(buffer) = map((hint & !(page - 1)) as *mut c_void, len) else {
                    continue;
                };
                if (buffer.ptr as usize).abs_diff(target) + len <= NEAR {
                    return Ok(buffer);
                }
                // Placed far away: dropping it unmaps it.
            }
        }
        alloc(len)
    }

    pub fn alloc(len: usize) -> Result<ExecBuffer, DetourError> {
        map(ptr::null_mut(), len).ok_or_else(|| DetourError::Alloc(last_os_error()))
    }

    pub fn make_executable(buffer: &ExecBuffer) -> Result<(), DetourError> {
        // SAFETY: the buffer is our own page-aligned mapping of `len` bytes.
        if unsafe {
            mprotect(
                buffer.ptr.cast::<c_void>(),
                buffer.len,
                PROT_READ | PROT_EXEC,
            )
        } != 0
        {
            return Err(DetourError::Protect(last_os_error()));
        }
        Ok(())
    }

    /// Unix offers no cheap query of a code region's end; functions are
    /// taken to have `want` readable bytes.
    pub fn readable(_address: usize, want: usize) -> usize {
        want
    }

    pub fn free(buffer: &mut ExecBuffer) {
        // SAFETY: `ptr`/`len` came from a successful mmap above.
        unsafe {
            munmap(buffer.ptr.cast::<c_void>(), buffer.len);
        }
    }

    pub unsafe fn write_code(dst: *mut u8, bytes: &[u8]) -> Result<(), DetourError> {
        let ps = page_size();
        let start = (dst as usize) & !(ps - 1);
        let end = ((dst as usize) + bytes.len() + ps - 1) & !(ps - 1);
        let size = end - start;
        // SAFETY: make the page(s) covering the range writable and executable.
        if unsafe {
            mprotect(
                start as *mut c_void,
                size,
                PROT_READ | PROT_WRITE | PROT_EXEC,
            )
        } != 0
        {
            return Err(DetourError::Protect(last_os_error()));
        }
        // SAFETY: the range is now writable; the caller keeps it quiescent.
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
        }
        // SAFETY: restore code pages to read + execute.
        unsafe {
            mprotect(start as *mut c_void, size, PROT_READ | PROT_EXEC);
        }
        Ok(())
    }

    pub unsafe fn flush_icache(_addr: *mut u8, _len: usize) {
        // x86-64 keeps the instruction cache coherent with these stores; a
        // compiler fence prevents the copy from being reordered past a later use.
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for ExecBuffer {
    fn drop(&mut self) {
        imp::free(self);
    }
}

/// A writable buffer anywhere in the address space, for test fixtures.
#[cfg(test)]
pub fn alloc(len: usize) -> Result<ExecBuffer, DetourError> {
    imp::alloc(len)
}

/// A writable trampoline buffer within reach of `target`'s neighbourhood,
/// or anywhere when nothing near is free. Seal it with [`make_executable`]
/// once written.
pub fn alloc_near(target: usize, len: usize) -> Result<ExecBuffer, DetourError> {
    imp::alloc_near(target, len)
}

/// Turns a written trampoline read-and-execute: never writable and
/// executable at once.
pub fn make_executable(buffer: &ExecBuffer) -> Result<(), DetourError> {
    imp::make_executable(buffer)
}

/// How many of the `want` bytes at `address` may be read without leaving
/// its memory region.
pub fn readable(address: usize, want: usize) -> usize {
    imp::readable(address, want)
}

/// Overwrites `bytes.len()` bytes at `dst` with `bytes`, toggling protection.
///
/// # Safety
///
/// `dst` must point at `bytes.len()` bytes of the current process's own code
/// that no other thread can execute for the duration of the call.
pub unsafe fn write_code(dst: *mut u8, bytes: &[u8]) -> Result<(), DetourError> {
    // SAFETY: forwarded to the platform implementation under the same contract.
    unsafe { imp::write_code(dst, bytes) }
}

/// Makes a freshly written executable range visible to the CPU.
///
/// # Safety
///
/// `addr..addr+len` must be a range this process may execute.
pub unsafe fn flush_icache(addr: *mut u8, len: usize) {
    // SAFETY: forwarded to the platform implementation under the same contract.
    unsafe { imp::flush_icache(addr, len) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Code of this test binary, standing in for a game function.
    fn some_code() -> usize {
        alloc_near as fn(usize, usize) -> Result<ExecBuffer, DetourError> as usize
    }

    #[test]
    fn a_trampoline_lands_near_its_target() {
        let target = some_code();
        let buffer = alloc_near(target, 256).unwrap();
        let distance = (buffer.as_ptr() as usize).abs_diff(target);
        assert!(distance <= NEAR, "{distance:#x} bytes away");
        make_executable(&buffer).unwrap();
    }

    #[test]
    fn this_code_is_readable_but_nothing_past_nowhere() {
        let target = some_code();
        assert_eq!(readable(target, 32), 32);
        #[cfg(windows)]
        assert_eq!(readable(0, 32), 0, "the null page is never readable");
    }
}
