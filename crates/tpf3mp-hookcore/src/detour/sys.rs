//! Platform executable-memory primitives for the x86-64 detour engine:
//! allocate an RWX buffer for the trampoline, and overwrite live code bytes.
//!
//! On x86-64 the instruction cache is coherent with data writes on the same
//! core, and both loaders install detours before the target runs on any core,
//! so a store fence is enough on Unix; Windows still calls
//! `FlushInstructionCache`, which is free when nothing needs flushing.

#![allow(unsafe_code)]

use super::DetourError;

fn last_os_error() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
}

/// An RWX memory region that holds a trampoline. Freed on drop.
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
    use super::{DetourError, ExecBuffer, last_os_error};
    use core::ffi::c_void;
    use core::ptr;
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc, VirtualFree,
        VirtualProtect,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    pub fn alloc(len: usize) -> Result<ExecBuffer, DetourError> {
        // SAFETY: a fresh reservation with a null base; VirtualAlloc returns a
        // valid committed RWX region of `len` bytes or null on failure.
        let ptr = unsafe {
            VirtualAlloc(
                ptr::null(),
                len,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_EXECUTE_READWRITE,
            )
        };
        if ptr.is_null() {
            return Err(DetourError::Alloc(last_os_error()));
        }
        Ok(ExecBuffer {
            ptr: ptr.cast::<u8>(),
            len,
        })
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
    use super::{DetourError, ExecBuffer, last_os_error};
    use core::ffi::c_void;
    use core::ptr;
    use libc::{
        _SC_PAGESIZE, MAP_ANON, MAP_FAILED, MAP_PRIVATE, PROT_EXEC, PROT_READ, PROT_WRITE, mmap,
        mprotect, munmap, sysconf,
    };

    fn page_size() -> usize {
        // SAFETY: sysconf(_SC_PAGESIZE) has no preconditions.
        let value = unsafe { sysconf(_SC_PAGESIZE) };
        if value > 0 { value as usize } else { 4096 }
    }

    pub fn alloc(len: usize) -> Result<ExecBuffer, DetourError> {
        // SAFETY: an anonymous private RWX mapping of `len` bytes; MAP_FAILED
        // signals failure.
        let ptr = unsafe {
            mmap(
                ptr::null_mut(),
                len,
                PROT_READ | PROT_WRITE | PROT_EXEC,
                MAP_PRIVATE | MAP_ANON,
                -1,
                0,
            )
        };
        if ptr == MAP_FAILED {
            return Err(DetourError::Alloc(last_os_error()));
        }
        Ok(ExecBuffer {
            ptr: ptr.cast::<u8>(),
            len,
        })
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

pub fn alloc(len: usize) -> Result<ExecBuffer, DetourError> {
    imp::alloc(len)
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
