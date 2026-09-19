//! From the adversarial review of the unsafe code: a hostile peer that
//! creates the shared mapping itself and writes an arbitrary 64-byte header
//! and ring bytes, which a normal `Link::open` on this side then consumes.
//!
//! The threat model (see the review brief and `docs/HOOKS.md`) treats the peer
//! as untrusted: it may write any header field and any ring index. In TPF3-MP
//! the agent is the owner/creator and the in-game hook is the opener/consumer,
//! so a malicious or crashed agent - or any same-user process that pre-creates
//! the predictably named object - is exactly this hostile peer, and the reads it
//! provokes happen inside the game process.
//!
//! These tests do not create the mapping through `Link::create` (which validates
//! its inputs); they lay out the raw shared object by hand, the way a foreign
//! agent would. The Win32 calls are declared here directly so the tests add no
//! dependency to the crate. Each test failed before its fix and now guards it.
#![cfg(windows)]
#![allow(unsafe_code, clippy::unwrap_used)]

use core::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};

use tpf3mp_ipc::{Link, Role};

// Win32 memory API, declared directly (kernel32) to avoid a dev-dependency.
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileMappingW(
        h_file: *mut c_void,
        attrs: *const c_void,
        protect: u32,
        max_high: u32,
        max_low: u32,
        name: *const u16,
    ) -> *mut c_void;
    fn MapViewOfFile(
        h: *mut c_void,
        access: u32,
        off_high: u32,
        off_low: u32,
        bytes: usize,
    ) -> *mut c_void;
}

const PAGE_READWRITE: u32 = 0x04;
const FILE_MAP_ALL_ACCESS: u32 = 0x000F_001F;

// Header offsets, mirroring `tpf3mp-ipc/src/header.rs` (the ABI in HOOKS.md).
const OFF_MAGIC: usize = 0;
const OFF_ABI: usize = 4;
const OFF_HEADER_SIZE: usize = 8;
const OFF_RING_CAPACITY: usize = 12;
const OFF_MAX_MESSAGE: usize = 16;
const OFF_SESSION: usize = 20;
const OFF_H2A_HEAD: usize = 48;
const OFF_A2H_HEAD: usize = 56;
const OFF_A2H_TAIL: usize = 60;
const HEADER_SIZE: usize = 64;
const MAGIC: u32 = u32::from_le_bytes(*b"T3MP");

/// Replicates `shm::fnv1a_32`, used only to derive the OS object name.
fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for &byte in bytes {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Replicates `shm::imp::wide_name` (Windows) so the PoC creates the very object
/// `Link::open` will look up for `logical`.
fn wide_name(logical: &str) -> Vec<u16> {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".to_owned());
    let mut seed = user.into_bytes();
    seed.push(0);
    seed.extend_from_slice(logical.as_bytes());
    let hash = fnv1a_32(&seed);
    let name = format!("Local\\tpf3mp.{hash:08x}");
    name.encode_utf16().chain(core::iter::once(0)).collect()
}

fn unique_logical(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("poc.{tag}.{}.{nanos}", std::process::id())
}

/// A hand-built hostile mapping. The view is intentionally kept alive (and the
/// handle leaked) for the lifetime of the test process; the peer writes the
/// header/ring through `base`, while `Link::open` maps its own view of the same
/// object.
struct HostileMapping {
    base: *mut u8,
}

impl HostileMapping {
    /// Creates the object for `logical` and writes a header that passes
    /// `Link::open`'s validation for everything it checks, with the given
    /// `ring_capacity` and `max_message` (which `open` does *not* cross-check).
    fn create(logical: &str, ring_capacity: u32, max_message: u32) -> Self {
        let total = HEADER_SIZE + 2 * ring_capacity as usize;
        let name = wide_name(logical);
        let invalid = core::ptr::without_provenance_mut::<c_void>(usize::MAX); // INVALID_HANDLE_VALUE
        // SAFETY: a pagefile-backed mapping with a valid wide name; the OS
        // zero-fills the object.
        let handle = unsafe {
            CreateFileMappingW(
                invalid,
                core::ptr::null(),
                PAGE_READWRITE,
                (total as u64 >> 32) as u32,
                (total as u64 & 0xffff_ffff) as u32,
                name.as_ptr(),
            )
        };
        assert!(!handle.is_null(), "CreateFileMappingW failed");
        // SAFETY: `handle` is valid; map the whole object.
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, total) };
        assert!(!view.is_null(), "MapViewOfFile failed");
        let base = view.cast::<u8>();

        let put = |off: usize, v: u32| {
            // SAFETY: `off` is within the mapped `total` bytes.
            unsafe { base.add(off).cast::<u32>().write_unaligned(v) };
        };
        put(OFF_ABI, 1);
        put(OFF_HEADER_SIZE, HEADER_SIZE as u32);
        put(OFF_RING_CAPACITY, ring_capacity);
        put(OFF_MAX_MESSAGE, max_message);
        put(OFF_SESSION, 1);
        // Publish the magic last, as the ABI requires.
        put(OFF_MAGIC, MAGIC);
        Self { base }
    }

    /// Writes a little-endian u32 at a header offset after `Link::open`.
    fn put_u32(&self, off: usize, v: u32) {
        // SAFETY: header offsets are within the mapping.
        unsafe { self.base.add(off).cast::<u32>().write_unaligned(v) };
    }

    /// Writes raw bytes into the agent->hook ring's data area at `data_offset`.
    fn put_a2h_bytes(&self, ring_capacity: u32, data_offset: usize, bytes: &[u8]) {
        let at = HEADER_SIZE + ring_capacity as usize + data_offset;
        // SAFETY: `data_offset < ring_capacity`, so `at` is within the mapping.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.add(at), bytes.len());
        }
    }
}

/// Review finding 1 (High). `Link::open` did not check `max_message` against
/// `ring_capacity`, unlike `Link::create`. A hostile owner could make
/// `max_message` larger than the ring, and a frame of up to that length then
/// passed every check in `pop_into`: its copy read past the ring, off the
/// end of the mapping, inside the game. `open` now refuses such a header.
#[test]
fn open_refuses_max_message_larger_than_ring() {
    let logical = unique_logical("badmax");
    let _map = HostileMapping::create(&logical, 4096, 5 * 4096);
    assert!(matches!(
        Link::open(&logical, Role::Hook),
        Err(tpf3mp_ipc::IpcError::InvalidConfig(_))
    ));
}

/// Review finding 1, the exploit itself: a frame of three rings' length,
/// sized to pass a hostile `max_message`. It can no longer be reached,
/// because the link never opens.
#[test]
fn a_frame_longer_than_the_ring_is_never_read() {
    const CAP: u32 = 4096;
    let logical = unique_logical("oob");
    let map = HostileMapping::create(&logical, CAP, 8 * CAP);
    let len = 3 * CAP;
    map.put_a2h_bytes(CAP, 0, &len.to_le_bytes());
    map.put_u32(OFF_A2H_HEAD, 0);
    map.put_u32(OFF_A2H_TAIL, len + 4);
    assert!(Link::open(&logical, Role::Hook).is_err());
}

/// Review finding 2 (Medium). `next_len` returned the raw length prefix, so a
/// caller sizing a buffer from it could be made to allocate 4 GiB. It now
/// never exceeds `max_message`: a corrupt frame reads as nothing waiting,
/// and `recv_into` reports the corruption.
#[test]
fn next_len_never_exceeds_max_message() {
    const CAP: u32 = 4096;
    let logical = unique_logical("nextlen");
    let map = HostileMapping::create(&logical, CAP, 1024);
    let link = Link::open(&logical, Role::Hook).unwrap();

    map.put_u32(OFF_A2H_HEAD, 0);
    map.put_u32(OFF_A2H_TAIL, 16);
    map.put_a2h_bytes(CAP, 0, &0xFFFF_FFFFu32.to_le_bytes());

    assert_eq!(link.next_len(), None);
    let mut out = vec![0u8; 1024];
    assert_eq!(
        link.recv_into(&mut out),
        Err(tpf3mp_ipc::RecvError::Corrupt)
    );
}

/// Review finding 2's counterpart on the read side: a producer claiming more
/// bytes than the ring holds is corrupt, however the lengths look.
#[test]
fn a_tail_beyond_the_ring_is_corrupt() {
    const CAP: u32 = 4096;
    let logical = unique_logical("bigtail");
    let map = HostileMapping::create(&logical, CAP, 1024);
    let link = Link::open(&logical, Role::Hook).unwrap();
    map.put_a2h_bytes(CAP, 0, &8u32.to_le_bytes());
    map.put_u32(OFF_A2H_HEAD, 0);
    map.put_u32(OFF_A2H_TAIL, CAP + 12);
    let mut out = vec![0u8; 1024];
    assert_eq!(
        link.recv_into(&mut out),
        Err(tpf3mp_ipc::RecvError::Corrupt)
    );
}

/// Review finding 3 (Medium). The producer computed its free space as
/// `capacity - used` with `used` from the peer's `head`: a hostile head
/// underflowed it, which panicked in debug builds and in release builds
/// let the producer overwrite unread bytes. It is now refused as corruption.
#[test]
fn a_hostile_head_is_refused_not_underflowed() {
    const CAP: u32 = 4096;
    let logical = unique_logical("underflow");
    let map = HostileMapping::create(&logical, CAP, 1024);
    let link = Link::open(&logical, Role::Hook).unwrap();
    map.put_u32(OFF_H2A_HEAD, CAP + 4);
    let result = catch_unwind(AssertUnwindSafe(|| link.send(b"x")));
    assert_eq!(
        result.expect("no panic"),
        Err(tpf3mp_ipc::SendError::Corrupt)
    );
}
