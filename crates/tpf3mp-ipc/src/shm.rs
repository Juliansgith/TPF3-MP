//! Named shared memory, created by the owner and opened by the other side.
//!
//! - Windows: `CreateFileMappingW`/`MapViewOfFile` in the per-session `Local\`
//!   namespace, with a per-user object name. Passing no explicit security
//!   descriptor gives the mapping the process token's default DACL, which
//!   grants access to the creating user and SYSTEM only - restrictive to this
//!   user - and the `Local\` namespace already keeps it out of other sessions.
//! - Linux/macOS: `shm_open`/`mmap` with mode `0600` (owner only). The object
//!   name is kept within macOS's 31-character `shm_open` limit by hashing the
//!   logical name (and the uid, so it is per-user) into a short fixed form.

#![allow(unsafe_code)]

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ShmError {
    #[error("the shared-memory name is not usable")]
    BadName,
    #[error("{op} failed (os error {code})")]
    Os { op: &'static str, code: i32 },
    #[error("the shared region is {found} bytes, smaller than the required {expected}")]
    TooSmall { found: usize, expected: usize },
}

fn os_err(op: &'static str) -> ShmError {
    ShmError::Os {
        op,
        code: std::io::Error::last_os_error().raw_os_error().unwrap_or(-1),
    }
}

/// FNV-1a, 32-bit. Used only to shorten a logical name into a fixed object name.
fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for &byte in bytes {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// A mapped shared region. Unmaps (and, for the owner on Unix, unlinks) on drop.
pub struct SharedRegion {
    inner: imp::Region,
}

impl SharedRegion {
    /// Creates (or re-initialises) the named region of `len` bytes as the owner.
    pub fn create(logical_name: &str, len: usize) -> Result<Self, ShmError> {
        Ok(Self {
            inner: imp::create(logical_name, len)?,
        })
    }

    /// Opens an existing named region, requiring at least `min_len` bytes.
    pub fn open(logical_name: &str, min_len: usize) -> Result<Self, ShmError> {
        Ok(Self {
            inner: imp::open(logical_name, min_len)?,
        })
    }

    pub fn base(&self) -> *mut u8 {
        self.inner.base
    }

    #[allow(clippy::len_without_is_empty)] // a mapped region is never empty
    pub fn len(&self) -> usize {
        self.inner.len
    }
}

// SAFETY: the region is a bundle of an OS handle/fd and a mapping pointer;
// moving ownership between threads is sound. Concurrent access is disciplined by
// the ring's SPSC contract and the header's atomics, not by this type.
unsafe impl Send for SharedRegion {}

#[cfg(windows)]
mod imp {
    use super::{ShmError, fnv1a_32, os_err};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_BASIC_INFORMATION,
        MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, OpenFileMappingW, PAGE_READWRITE,
        UnmapViewOfFile, VirtualQuery,
    };

    pub struct Region {
        handle: HANDLE,
        view: MEMORY_MAPPED_VIEW_ADDRESS,
        pub base: *mut u8,
        pub len: usize,
    }

    fn wide_name(logical: &str) -> Vec<u16> {
        // Per user (the object name includes the user) and per session (the
        // `Local\` namespace).
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".to_owned());
        let mut seed = user.into_bytes();
        seed.push(0);
        seed.extend_from_slice(logical.as_bytes());
        let hash = fnv1a_32(&seed);
        let name = format!("Local\\tpf3mp.{hash:08x}");
        name.encode_utf16().chain(core::iter::once(0)).collect()
    }

    pub fn create(logical: &str, len: usize) -> Result<Region, ShmError> {
        let name = wide_name(logical);
        let high = (len as u64 >> 32) as u32;
        let low = (len as u64 & 0xffff_ffff) as u32;
        // SAFETY: a pagefile-backed mapping (INVALID_HANDLE_VALUE) with a valid
        // wide name; null security attributes use the token's default DACL.
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                core::ptr::null(),
                PAGE_READWRITE,
                high,
                low,
                name.as_ptr(),
            )
        };
        if handle.is_null() {
            return Err(os_err("CreateFileMappingW"));
        }
        // SAFETY: `handle` is valid; map the whole `len`-byte object.
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, len) };
        if view.Value.is_null() {
            let error = os_err("MapViewOfFile");
            // SAFETY: closing the handle we just created.
            unsafe {
                CloseHandle(handle);
            }
            return Err(error);
        }
        Ok(Region {
            handle,
            view,
            base: view.Value.cast::<u8>(),
            len,
        })
    }

    pub fn open(logical: &str, min_len: usize) -> Result<Region, ShmError> {
        let name = wide_name(logical);
        // SAFETY: a valid wide name; do not inherit the handle.
        let handle = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(os_err("OpenFileMappingW"));
        }
        // SAFETY: map the whole object (size 0 means "to the end").
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, 0) };
        if view.Value.is_null() {
            let error = os_err("MapViewOfFile");
            // SAFETY: closing the handle we just opened.
            unsafe {
                CloseHandle(handle);
            }
            return Err(error);
        }
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { core::mem::zeroed() };
        // SAFETY: `view.Value` is a mapped address; query its region size.
        let queried = unsafe {
            VirtualQuery(
                view.Value,
                &mut info,
                core::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        let len = if queried == 0 { 0 } else { info.RegionSize };
        if len < min_len {
            // SAFETY: unmap and close what we opened.
            unsafe {
                UnmapViewOfFile(view);
                CloseHandle(handle);
            }
            return Err(ShmError::TooSmall {
                found: len,
                expected: min_len,
            });
        }
        Ok(Region {
            handle,
            view,
            base: view.Value.cast::<u8>(),
            len,
        })
    }

    impl Drop for Region {
        fn drop(&mut self) {
            // SAFETY: `view`/`handle` came from a successful map/create above.
            unsafe {
                UnmapViewOfFile(self.view);
                CloseHandle(self.handle);
            }
            let _ = &self.base;
        }
    }
}

#[cfg(unix)]
mod imp {
    use super::{ShmError, fnv1a_32, os_err};
    use core::ffi::c_void;
    use std::ffi::CString;

    pub struct Region {
        fd: i32,
        name: CString,
        owner: bool,
        pub base: *mut u8,
        pub len: usize,
    }

    fn shm_name(logical: &str) -> Result<CString, ShmError> {
        // Per user (uid in the hash) and short enough for macOS's 31-char limit.
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let hash = fnv1a_32(format!("{uid}:{logical}").as_bytes());
        CString::new(format!("/tpf3mp.{hash:08x}")).map_err(|_| ShmError::BadName)
    }

    pub fn create(logical: &str, len: usize) -> Result<Region, ShmError> {
        let name = shm_name(logical)?;
        // SAFETY: a valid C name; create read-write, owner-only mode.
        let fd = unsafe {
            libc::shm_open(
                name.as_ptr(),
                libc::O_CREAT | libc::O_RDWR,
                0o600 as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(os_err("shm_open"));
        }
        // SAFETY: size the object to `len`.
        if unsafe { libc::ftruncate(fd, len as libc::off_t) } != 0 {
            let error = os_err("ftruncate");
            // SAFETY: clean up the object we created.
            unsafe {
                libc::close(fd);
                libc::shm_unlink(name.as_ptr());
            }
            return Err(error);
        }
        map(fd, len, name, true)
    }

    pub fn open(logical: &str, min_len: usize) -> Result<Region, ShmError> {
        let name = shm_name(logical)?;
        // SAFETY: open an existing object read-write.
        let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR, 0o600 as libc::c_uint) };
        if fd < 0 {
            return Err(os_err("shm_open"));
        }
        let mut stat: libc::stat = unsafe { core::mem::zeroed() };
        // SAFETY: `fd` is valid; read its size.
        if unsafe { libc::fstat(fd, &mut stat) } != 0 {
            let error = os_err("fstat");
            // SAFETY: closing the fd we opened.
            unsafe {
                libc::close(fd);
            }
            return Err(error);
        }
        let len = stat.st_size as usize;
        if len < min_len {
            // SAFETY: closing the fd we opened.
            unsafe {
                libc::close(fd);
            }
            return Err(ShmError::TooSmall {
                found: len,
                expected: min_len,
            });
        }
        map(fd, len, name, false)
    }

    fn map(fd: i32, len: usize, name: CString, owner: bool) -> Result<Region, ShmError> {
        // SAFETY: `fd` is a valid shared-memory object of `len` bytes.
        let base = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            let error = os_err("mmap");
            // SAFETY: closing the fd; unlink if we created it.
            unsafe {
                libc::close(fd);
                if owner {
                    libc::shm_unlink(name.as_ptr());
                }
            }
            return Err(error);
        }
        Ok(Region {
            fd,
            name,
            owner,
            base: base.cast::<u8>(),
            len,
        })
    }

    impl Drop for Region {
        fn drop(&mut self) {
            // SAFETY: `base`/`len`/`fd` came from a successful map above; the
            // owner also removes the name so it does not leak.
            unsafe {
                libc::munmap(self.base.cast::<c_void>(), self.len);
                libc::close(self.fd);
                if self.owner {
                    libc::shm_unlink(self.name.as_ptr());
                }
            }
        }
    }
}
