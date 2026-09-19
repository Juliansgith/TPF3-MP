//! A single-producer, single-consumer byte ring over a shared memory region.
//!
//! One ring carries length-prefixed messages in one direction. The producer and
//! the consumer are different processes viewing the same bytes, so the head and
//! tail live in the shared header as atomics and everything is index arithmetic
//! on a fixed byte buffer - no allocation on either side of a `push`/`pop`.
//!
//! # Layout
//!
//! `head` and `tail` are free-running `u32` counters (they wrap at `2^32`, not
//! at the capacity). The number of bytes in the ring is `tail - head` using
//! wrapping subtraction, so it is correct as long as capacity is a power of two
//! at most `2^31`. A message is a little-endian `u32` length followed by that
//! many payload bytes; both may wrap around the end of the buffer, which is why
//! the copies are done in up to two parts.
//!
//! # Ordering
//!
//! The producer writes the payload bytes and then stores `tail` with `Release`;
//! the consumer loads `tail` with `Acquire` before reading, so it always sees a
//! completed message. Symmetrically the consumer stores `head` with `Release`
//! after reading and the producer loads it with `Acquire`, so freed space is
//! published safely. Only the producer writes `tail`; only the consumer writes
//! `head`.

#![allow(unsafe_code)]

use core::sync::atomic::{AtomicU32, Ordering};

use thiserror::Error;

/// The message length is prefixed as a little-endian `u32`.
pub const LENGTH_PREFIX: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PushError {
    #[error("the ring is full")]
    Full,
    #[error("message of {len} bytes exceeds the {max}-byte ring limit")]
    TooLarge { len: usize, max: usize },
    #[error("the ring is corrupt: the consumer's index is inconsistent with the ring state")]
    Corrupt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PopError {
    #[error("the ring is corrupt: a frame header is inconsistent with the ring state")]
    Corrupt,
    #[error("the caller's buffer of {have} bytes is too small for a {needed}-byte message")]
    BufferTooSmall { have: usize, needed: usize },
}

/// A view of one direction's ring. Cheap to construct; holds no ownership of the
/// shared memory, only pointers into it.
pub struct Ring {
    data: *mut u8,
    capacity: u32,
    max_message: u32,
    head: *mut u32,
    tail: *mut u32,
}

impl Ring {
    /// # Safety
    ///
    /// `data` must point at `capacity` writable, shared bytes; `head` and `tail`
    /// must point at 4-byte-aligned shared `u32` cells used only through this
    /// ring. `capacity` must be a power of two `<= 2^31`, and `max_message`
    /// leaves room for the length prefix (`max_message + LENGTH_PREFIX <=
    /// capacity`). The caller guarantees at most one producer and one consumer.
    pub unsafe fn new(
        data: *mut u8,
        capacity: u32,
        max_message: u32,
        head: *mut u32,
        tail: *mut u32,
    ) -> Self {
        Self {
            data,
            capacity,
            max_message,
            head,
            tail,
        }
    }

    fn head(&self) -> &AtomicU32 {
        // SAFETY: `head` is a valid, aligned, shared u32 cell used only as an
        // atomic (see `new`).
        unsafe { AtomicU32::from_ptr(self.head) }
    }

    fn tail(&self) -> &AtomicU32 {
        // SAFETY: as above for `tail`.
        unsafe { AtomicU32::from_ptr(self.tail) }
    }

    /// Appends one message. Called only by the producer.
    pub fn push(&self, payload: &[u8]) -> Result<(), PushError> {
        if payload.len() > self.max_message as usize {
            return Err(PushError::TooLarge {
                len: payload.len(),
                max: self.max_message as usize,
            });
        }
        let need = LENGTH_PREFIX as u32 + payload.len() as u32;
        let tail = self.tail().load(Ordering::Relaxed);
        let head = self.head().load(Ordering::Acquire);
        let used = tail.wrapping_sub(head);
        // The consumer owns `head`; one that claims more bytes in use than
        // the ring holds is broken or hostile, and nothing is written.
        let Some(free) = self.capacity.checked_sub(used) else {
            return Err(PushError::Corrupt);
        };
        if need > free {
            return Err(PushError::Full);
        }
        let length = (payload.len() as u32).to_le_bytes();
        // SAFETY: `need <= free`, so both writes stay within the ring; the two
        // ranges do not overlap the consumer's live region.
        unsafe {
            self.write_wrapping(tail, &length);
            self.write_wrapping(tail.wrapping_add(LENGTH_PREFIX as u32), payload);
        }
        self.tail()
            .store(tail.wrapping_add(need), Ordering::Release);
        Ok(())
    }

    /// The length of the next message without consuming it: never more than
    /// `max_message`. `None` if the ring is empty, or if the next frame is
    /// corrupt, which [`Ring::pop_into`] then reports.
    pub fn peek_len(&self) -> Option<u32> {
        let tail = self.tail().load(Ordering::Acquire);
        let head = self.head().load(Ordering::Relaxed);
        let available = tail.wrapping_sub(head);
        if available < LENGTH_PREFIX as u32 || available > self.capacity {
            return None;
        }
        let mut length = [0u8; LENGTH_PREFIX];
        // SAFETY: at least LENGTH_PREFIX bytes are available.
        unsafe {
            self.read_wrapping(head, &mut length);
        }
        let len = u32::from_le_bytes(length);
        (len <= self.max_message).then_some(len)
    }

    /// Reads the next message into `out`. Called only by the consumer. Returns
    /// the message length, `None` when the ring is empty, or an error. On
    /// [`PopError::BufferTooSmall`] the message stays in the ring.
    pub fn pop_into(&self, out: &mut [u8]) -> Result<Option<usize>, PopError> {
        let tail = self.tail().load(Ordering::Acquire);
        let head = self.head().load(Ordering::Relaxed);
        let available = tail.wrapping_sub(head);
        if available == 0 {
            return Ok(None);
        }
        // The producer owns `tail`: it cannot have published more than the
        // ring holds, nor less than a whole length prefix.
        if available < LENGTH_PREFIX as u32 || available > self.capacity {
            return Err(PopError::Corrupt);
        }
        let mut length = [0u8; LENGTH_PREFIX];
        // SAFETY: at least LENGTH_PREFIX bytes are available.
        unsafe {
            self.read_wrapping(head, &mut length);
        }
        let len = u32::from_le_bytes(length);
        if len > self.max_message || available < LENGTH_PREFIX as u32 + len {
            return Err(PopError::Corrupt);
        }
        let len = len as usize;
        if out.len() < len {
            return Err(PopError::BufferTooSmall {
                have: out.len(),
                needed: len,
            });
        }
        // SAFETY: the message's bytes are all available.
        unsafe {
            self.read_wrapping(head.wrapping_add(LENGTH_PREFIX as u32), &mut out[..len]);
        }
        self.head().store(
            head.wrapping_add(LENGTH_PREFIX as u32 + len as u32),
            Ordering::Release,
        );
        Ok(Some(len))
    }

    /// Copies `src` into the ring starting at counter `at`, wrapping the end.
    ///
    /// # Safety
    ///
    /// The caller must have reserved `src.len()` free bytes at `at`.
    unsafe fn write_wrapping(&self, at: u32, src: &[u8]) {
        debug_assert!(
            src.len() <= self.capacity as usize,
            "a copy wraps once at most"
        );
        let mask = self.capacity - 1;
        let start = (at & mask) as usize;
        let first = core::cmp::min(src.len(), self.capacity as usize - start);
        // SAFETY: `start + first <= capacity`, and the remainder starts at 0.
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), self.data.add(start), first);
            if first < src.len() {
                core::ptr::copy_nonoverlapping(
                    src.as_ptr().add(first),
                    self.data,
                    src.len() - first,
                );
            }
        }
    }

    /// Copies `dst.len()` bytes out of the ring starting at counter `at`.
    ///
    /// # Safety
    ///
    /// The caller must know `dst.len()` bytes are present at `at`.
    unsafe fn read_wrapping(&self, at: u32, dst: &mut [u8]) {
        debug_assert!(
            dst.len() <= self.capacity as usize,
            "a copy wraps once at most"
        );
        let mask = self.capacity - 1;
        let start = (at & mask) as usize;
        let first = core::cmp::min(dst.len(), self.capacity as usize - start);
        // SAFETY: `start + first <= capacity`, and the remainder starts at 0.
        unsafe {
            core::ptr::copy_nonoverlapping(self.data.add(start), dst.as_mut_ptr(), first);
            if first < dst.len() {
                core::ptr::copy_nonoverlapping(
                    self.data,
                    dst.as_mut_ptr().add(first),
                    dst.len() - first,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ring backed by a plain heap buffer, for single-process unit tests.
    struct TestRing {
        _data: Vec<u8>,
        _cells: Box<[u32; 2]>,
        ring: Ring,
    }

    impl TestRing {
        fn new(capacity: u32, max_message: u32) -> Self {
            let mut data = vec![0u8; capacity as usize];
            let mut cells = Box::new([0u32; 2]);
            // SAFETY: the buffers outlive `ring` (held in the same struct); the
            // capacity is a power of two and the cells are `u32`-aligned.
            let ring = unsafe {
                Ring::new(
                    data.as_mut_ptr(),
                    capacity,
                    max_message,
                    &mut cells[0] as *mut u32,
                    &mut cells[1] as *mut u32,
                )
            };
            Self {
                _data: data,
                _cells: cells,
                ring,
            }
        }
    }

    #[test]
    fn round_trips_a_message() {
        let ring = TestRing::new(64, 32);
        ring.ring.push(b"hello").unwrap();
        let mut out = [0u8; 32];
        assert_eq!(ring.ring.pop_into(&mut out).unwrap(), Some(5));
        assert_eq!(&out[..5], b"hello");
        assert_eq!(ring.ring.pop_into(&mut out).unwrap(), None);
    }

    #[test]
    fn rejects_oversized_messages() {
        let ring = TestRing::new(64, 8);
        assert_eq!(
            ring.ring.push(&[0u8; 9]),
            Err(PushError::TooLarge { len: 9, max: 8 })
        );
    }

    #[test]
    fn reports_full() {
        let ring = TestRing::new(16, 12);
        // 4-byte header + 8 payload = 12 bytes fits once in a 16-byte ring.
        ring.ring.push(&[1u8; 8]).unwrap();
        assert_eq!(ring.ring.push(&[2u8; 8]), Err(PushError::Full));
    }

    #[test]
    fn wraps_around_the_end() {
        let ring = TestRing::new(16, 12);
        let mut out = [0u8; 16];
        // Push/pop repeatedly so the counters advance past the buffer end.
        for i in 0..20u8 {
            let payload = [i; 6];
            ring.ring.push(&payload).unwrap();
            assert_eq!(ring.ring.pop_into(&mut out).unwrap(), Some(6));
            assert_eq!(&out[..6], &payload);
        }
    }

    #[test]
    fn buffer_too_small_leaves_the_message() {
        let ring = TestRing::new(64, 32);
        ring.ring.push(b"abcdef").unwrap();
        let mut tiny = [0u8; 3];
        assert_eq!(
            ring.ring.pop_into(&mut tiny),
            Err(PopError::BufferTooSmall { have: 3, needed: 6 })
        );
        // Still there.
        let mut out = [0u8; 8];
        assert_eq!(ring.ring.pop_into(&mut out).unwrap(), Some(6));
    }

    #[test]
    fn peek_len_does_not_consume() {
        let ring = TestRing::new(64, 32);
        ring.ring.push(b"xyz").unwrap();
        assert_eq!(ring.ring.peek_len(), Some(3));
        assert_eq!(ring.ring.peek_len(), Some(3));
        let mut out = [0u8; 8];
        assert_eq!(ring.ring.pop_into(&mut out).unwrap(), Some(3));
        assert_eq!(ring.ring.peek_len(), None);
    }

    #[test]
    fn empty_payload_round_trips() {
        let ring = TestRing::new(64, 32);
        ring.ring.push(b"").unwrap();
        let mut out = [0u8; 8];
        assert_eq!(ring.ring.pop_into(&mut out).unwrap(), Some(0));
    }
}
