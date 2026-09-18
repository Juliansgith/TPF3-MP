//! The shared-memory header layout and typed access to it.
//!
//! Every field lives at a fixed byte offset so the agent - written separately -
//! can lay out the same bytes. The scalar fields and the ring indices are
//! `u32`; the two heartbeats are `u64` and 8-byte aligned. `docs/HOOKS.md` is
//! the byte-exact specification; this module is its one implementation.

#![allow(unsafe_code)]

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::ring::Ring;
use crate::shm::SharedRegion;

/// Size of the fixed header that precedes the two ring buffers.
pub const HEADER_SIZE: usize = 64;
/// The ABI version this build speaks.
pub const ABI_VERSION: u32 = 1;
/// `b"T3MP"` read as a little-endian `u32`; a readiness flag written last.
pub const MAGIC: u32 = u32::from_le_bytes(*b"T3MP");

const OFF_MAGIC: usize = 0;
const OFF_ABI: usize = 4;
const OFF_HEADER_SIZE: usize = 8;
const OFF_RING_CAPACITY: usize = 12;
const OFF_MAX_MESSAGE: usize = 16;
const OFF_SESSION: usize = 20;
const OFF_HOOK_PID: usize = 24;
const OFF_AGENT_PID: usize = 28;
const OFF_HOOK_HEARTBEAT: usize = 32;
const OFF_AGENT_HEARTBEAT: usize = 40;
const OFF_H2A_HEAD: usize = 48;
const OFF_H2A_TAIL: usize = 52;
const OFF_A2H_HEAD: usize = 56;
const OFF_A2H_TAIL: usize = 60;

/// A typed view of a mapped region's header and rings. Holds only pointers.
#[derive(Clone, Copy)]
pub struct Layout {
    base: *mut u8,
    ring_capacity: u32,
    max_message: u32,
}

impl Layout {
    /// # Safety
    ///
    /// `base` must point at a mapping of at least `HEADER_SIZE + 2 *
    /// ring_capacity` bytes that lives as long as this `Layout` is used.
    pub unsafe fn new(base: *mut u8, ring_capacity: u32, max_message: u32) -> Self {
        Self {
            base,
            ring_capacity,
            max_message,
        }
    }

    /// Builds a `Layout` over a mapped region. The region guarantees a valid
    /// mapping of `region.len()` bytes, and the sizes are checked to fit, so
    /// this is safe to call; only later dereferences of the returned view rely
    /// on the region still being alive (the [`Link`](crate::Link) that holds
    /// both keeps them together).
    pub fn over(region: &SharedRegion, ring_capacity: u32, max_message: u32) -> Self {
        debug_assert!(
            HEADER_SIZE + 2 * ring_capacity as usize <= region.len(),
            "region too small for the requested rings"
        );
        // SAFETY: `region.base()` is valid for `region.len()` bytes, which the
        // assertion confirms covers the header and both rings.
        unsafe { Self::new(region.base(), ring_capacity, max_message) }
    }

    fn u32_at(&self, offset: usize) -> &AtomicU32 {
        // SAFETY: `offset` is a 4-aligned header field within the mapping.
        unsafe { AtomicU32::from_ptr(self.base.add(offset).cast::<u32>()) }
    }

    fn u64_at(&self, offset: usize) -> &AtomicU64 {
        // SAFETY: `offset` is an 8-aligned header field within the mapping.
        unsafe { AtomicU64::from_ptr(self.base.add(offset).cast::<u64>()) }
    }

    /// Zeroes the header (resetting the rings and counters) before a fresh init.
    pub fn zero_header(&self) {
        // SAFETY: the first HEADER_SIZE bytes are within the mapping.
        unsafe {
            core::ptr::write_bytes(self.base, 0, HEADER_SIZE);
        }
    }

    pub fn magic(&self) -> u32 {
        self.u32_at(OFF_MAGIC).load(Ordering::Acquire)
    }

    /// Publishes the magic last, releasing all earlier header writes to a peer
    /// that reads the magic with `Acquire`.
    pub fn publish_magic(&self) {
        self.u32_at(OFF_MAGIC).store(MAGIC, Ordering::Release);
    }

    pub fn abi(&self) -> u32 {
        self.u32_at(OFF_ABI).load(Ordering::Relaxed)
    }

    pub fn header_size(&self) -> u32 {
        self.u32_at(OFF_HEADER_SIZE).load(Ordering::Relaxed)
    }

    pub fn ring_capacity_field(&self) -> u32 {
        self.u32_at(OFF_RING_CAPACITY).load(Ordering::Relaxed)
    }

    pub fn max_message_field(&self) -> u32 {
        self.u32_at(OFF_MAX_MESSAGE).load(Ordering::Relaxed)
    }

    pub fn session(&self) -> u32 {
        self.u32_at(OFF_SESSION).load(Ordering::Acquire)
    }

    /// Writes the fixed identity/size fields (not the magic, which is published
    /// afterwards).
    pub fn write_fields(&self, session: u32) {
        self.u32_at(OFF_ABI).store(ABI_VERSION, Ordering::Relaxed);
        self.u32_at(OFF_HEADER_SIZE)
            .store(HEADER_SIZE as u32, Ordering::Relaxed);
        self.u32_at(OFF_RING_CAPACITY)
            .store(self.ring_capacity, Ordering::Relaxed);
        self.u32_at(OFF_MAX_MESSAGE)
            .store(self.max_message, Ordering::Relaxed);
        self.u32_at(OFF_SESSION).store(session, Ordering::Release);
    }

    pub fn set_hook_pid(&self, pid: u32) {
        self.u32_at(OFF_HOOK_PID).store(pid, Ordering::Relaxed);
    }

    pub fn set_agent_pid(&self, pid: u32) {
        self.u32_at(OFF_AGENT_PID).store(pid, Ordering::Relaxed);
    }

    pub fn hook_pid(&self) -> u32 {
        self.u32_at(OFF_HOOK_PID).load(Ordering::Relaxed)
    }

    pub fn agent_pid(&self) -> u32 {
        self.u32_at(OFF_AGENT_PID).load(Ordering::Relaxed)
    }

    pub fn bump_hook_heartbeat(&self) {
        self.u64_at(OFF_HOOK_HEARTBEAT)
            .fetch_add(1, Ordering::Release);
    }

    pub fn bump_agent_heartbeat(&self) {
        self.u64_at(OFF_AGENT_HEARTBEAT)
            .fetch_add(1, Ordering::Release);
    }

    pub fn set_hook_heartbeat(&self, value: u64) {
        self.u64_at(OFF_HOOK_HEARTBEAT)
            .store(value, Ordering::Release);
    }

    pub fn set_agent_heartbeat(&self, value: u64) {
        self.u64_at(OFF_AGENT_HEARTBEAT)
            .store(value, Ordering::Release);
    }

    pub fn hook_heartbeat(&self) -> u64 {
        self.u64_at(OFF_HOOK_HEARTBEAT).load(Ordering::Acquire)
    }

    pub fn agent_heartbeat(&self) -> u64 {
        self.u64_at(OFF_AGENT_HEARTBEAT).load(Ordering::Acquire)
    }

    /// The hook->agent ring.
    pub fn h2a(&self) -> Ring {
        // SAFETY: the data area and index cells are within the mapping; the
        // capacity is a validated power of two.
        unsafe {
            Ring::new(
                self.base.add(HEADER_SIZE),
                self.ring_capacity,
                self.max_message,
                self.base.add(OFF_H2A_HEAD).cast::<u32>(),
                self.base.add(OFF_H2A_TAIL).cast::<u32>(),
            )
        }
    }

    /// The agent->hook ring.
    pub fn a2h(&self) -> Ring {
        // SAFETY: as `h2a`, for the second data area after the first ring.
        unsafe {
            Ring::new(
                self.base.add(HEADER_SIZE + self.ring_capacity as usize),
                self.ring_capacity,
                self.max_message,
                self.base.add(OFF_A2H_HEAD).cast::<u32>(),
                self.base.add(OFF_A2H_TAIL).cast::<u32>(),
            )
        }
    }
}

// SAFETY: `Layout` is a bundle of pointers into a shared mapping; moving it
// between threads is sound because every field access goes through atomics or
// the SPSC ring discipline.
unsafe impl Send for Layout {}
