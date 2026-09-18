//! The x86-64 inline detour engine (see the module docs in `mod.rs`).

#![allow(unsafe_code)]

use iced_x86::{
    BlockEncoder, BlockEncoderOptions, Decoder, DecoderOptions, FlowControl, Instruction,
    InstructionBlock,
};

use super::{DetourError, sys};

/// `FF 25 00000000` + an absolute 8-byte target: `jmp [rip+0]`, reachable
/// anywhere in the address space.
const ABS_JMP_LEN: usize = 14;
/// `E9` + a signed 32-bit relative displacement: `jmp rel32`.
const REL_JMP_LEN: usize = 5;
/// Enough bytes to decode any reasonable prologue we would steal.
const PROLOGUE_SCAN: usize = 32;

/// An installed inline detour. Dropping it (or calling [`InlineDetour::detach`])
/// restores the original bytes.
pub struct InlineDetour {
    target: *mut u8,
    steal_len: usize,
    original: Vec<u8>,
    trampoline: sys::ExecBuffer,
    active: bool,
}

impl InlineDetour {
    /// Redirects the function at `target` to `detour`, returning a handle whose
    /// [`trampoline`](Self::trampoline) calls the original.
    ///
    /// # Safety
    ///
    /// - `target` must point at a real function in this process's code.
    /// - No thread may execute `target` (or the bytes just after its prologue)
    ///   during this call. Install before the target's first run, or park its
    ///   threads first.
    /// - `detour` must be a function that is ABI-compatible with `target`.
    pub unsafe fn install(target: *mut u8, detour: *const u8) -> Result<Self, DetourError> {
        let target_addr = target as usize;
        let detour_addr = detour as usize;

        // A near detour is reached with a 5-byte relative jump; otherwise a
        // 14-byte absolute jump, which needs a longer prologue to steal.
        let rel_ok = fits_i32((detour_addr as i128) - ((target_addr + REL_JMP_LEN) as i128));
        let min_patch = if rel_ok { REL_JMP_LEN } else { ABS_JMP_LEN };

        // SAFETY: a function has at least `PROLOGUE_SCAN` readable code bytes.
        let code = unsafe { std::slice::from_raw_parts(target, PROLOGUE_SCAN) };
        let (instrs, steal_len) = decode_prologue(code, target_addr as u64, min_patch)?;
        let original = code[..steal_len].to_vec();

        // Trampoline: the relocated prologue, then an absolute jump back to the
        // first instruction after it.
        let trampoline = sys::alloc(steal_len + ABS_JMP_LEN + 64)?;
        let tramp_addr = trampoline.as_mut_ptr() as u64;
        let mut body = encode_block(&instrs, tramp_addr)?;
        body.extend_from_slice(&abs_jmp((target_addr + steal_len) as u64));
        if body.len() > trampoline.len() {
            return Err(DetourError::Encode(
                "relocated prologue does not fit the trampoline".to_owned(),
            ));
        }
        // SAFETY: `trampoline` is a fresh RWX buffer of at least `body.len()`.
        unsafe {
            std::ptr::copy_nonoverlapping(body.as_ptr(), trampoline.as_mut_ptr(), body.len());
            sys::flush_icache(trampoline.as_mut_ptr(), body.len());
        }

        // Patch the target with the jump to the detour.
        let patch = if rel_ok {
            rel_jmp_patch(target_addr + REL_JMP_LEN, detour_addr, steal_len)
        } else {
            abs_jmp_patch(detour_addr as u64, steal_len)
        };
        // SAFETY: the caller guarantees `target` is quiescent for `patch.len()`
        // bytes, which equals `steal_len`.
        unsafe {
            sys::write_code(target, &patch)?;
        }

        Ok(Self {
            target,
            steal_len,
            original,
            trampoline,
            active: true,
        })
    }

    /// The original function: run the stolen prologue, then continue in place.
    /// Cast this to the target's function type to call through.
    pub fn trampoline(&self) -> *const u8 {
        self.trampoline.as_ptr()
    }

    pub fn target(&self) -> *mut u8 {
        self.target
    }

    pub fn steal_len(&self) -> usize {
        self.steal_len
    }

    /// Removes the detour, restoring the original bytes.
    ///
    /// # Safety
    ///
    /// As with install, no thread may execute `target` during the call.
    pub unsafe fn detach(mut self) -> Result<(), DetourError> {
        // SAFETY: forwarded under the same quiescence contract.
        unsafe { self.restore() }
    }

    unsafe fn restore(&mut self) -> Result<(), DetourError> {
        if self.active {
            // SAFETY: writing the saved original bytes back over the patch.
            unsafe {
                sys::write_code(self.target, &self.original)?;
            }
            self.active = false;
        }
        Ok(())
    }
}

impl Drop for InlineDetour {
    fn drop(&mut self) {
        // SAFETY: same contract as install; a failure here can only mean the
        // OS refused to restore protection, which we cannot report from drop.
        let _ = unsafe { self.restore() };
    }
}

// SAFETY: the raw pointers are owned addresses of this process's own memory; a
// detour handle can be moved between threads. Sending it does not by itself make
// concurrent patching safe - the install/detach contract still applies.
unsafe impl Send for InlineDetour {}

fn fits_i32(value: i128) -> bool {
    i32::try_from(value).is_ok()
}

/// Decodes instructions at `ip` until at least `min` bytes are covered, on an
/// instruction boundary. Refuses anything that cannot be relocated verbatim: an
/// undecodable byte, or an instruction that is not straight-line
/// ([`FlowControl::Next`]). RIP-relative operands are fine - the block encoder
/// fixes their displacements - so this is exactly the "relocate RIP-relative,
/// refuse branches" rule.
fn decode_prologue(
    code: &[u8],
    ip: u64,
    min: usize,
) -> Result<(Vec<Instruction>, usize), DetourError> {
    let mut decoder = Decoder::with_ip(64, code, ip, DecoderOptions::NONE);
    let mut instrs = Vec::new();
    let mut covered = 0usize;
    while covered < min {
        if !decoder.can_decode() {
            return Err(DetourError::UnsupportedPrologue {
                reason: format!("only {covered} bytes decode before the patch needs {min}"),
            });
        }
        let insn = decoder.decode();
        if insn.is_invalid() {
            return Err(DetourError::UnsupportedPrologue {
                reason: format!("undecodable byte at offset {covered}"),
            });
        }
        if insn.flow_control() != FlowControl::Next {
            return Err(DetourError::UnsupportedPrologue {
                reason: format!(
                    "prologue branches at offset {covered} ({:?}, {:?})",
                    insn.mnemonic(),
                    insn.flow_control()
                ),
            });
        }
        instrs.push(insn);
        covered = (decoder.ip() - ip) as usize;
    }
    Ok((instrs, covered))
}

fn encode_block(instrs: &[Instruction], rip: u64) -> Result<Vec<u8>, DetourError> {
    let block = InstructionBlock::new(instrs, rip);
    match BlockEncoder::encode(64, block, BlockEncoderOptions::NONE) {
        Ok(result) => Ok(result.code_buffer),
        Err(error) => Err(DetourError::Encode(error.to_string())),
    }
}

fn abs_jmp(to: u64) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0x25, 0x00, 0x00, 0x00, 0x00];
    bytes.extend_from_slice(&to.to_le_bytes());
    bytes
}

fn abs_jmp_patch(to: u64, steal_len: usize) -> Vec<u8> {
    let mut bytes = abs_jmp(to);
    bytes.resize(steal_len, 0xCC);
    bytes
}

fn rel_jmp_patch(after: usize, to: usize, steal_len: usize) -> Vec<u8> {
    // The caller checked this fits; the cast cannot lose information.
    let displacement = (to as i128 - after as i128) as i32;
    let mut bytes = vec![0xE9];
    bytes.extend_from_slice(&displacement.to_le_bytes());
    bytes.resize(steal_len, 0xCC);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    type Fun = extern "C" fn() -> u32;

    /// Hand-assembles, into an RWX buffer, a function whose prologue is a
    /// RIP-relative load of a constant stored in the same buffer:
    ///
    /// ```text
    /// 0: 8B 05 3A 00 00 00   mov eax, [rip+0x3a]   ; -> dword at offset 0x40
    /// 6: 90 x8                nop                   ; padding to steal 14 bytes
    /// e: C3                   ret
    /// ...
    /// 40: EF BE 34 12         .dword 0x1234beef
    /// ```
    fn rip_relative_function(value: u32) -> sys::ExecBuffer {
        let mut code = [0u8; 0x80];
        code[0..6].copy_from_slice(&[0x8B, 0x05, 0x3A, 0x00, 0x00, 0x00]);
        for byte in code.iter_mut().take(0x0E).skip(6) {
            *byte = 0x90;
        }
        code[0x0E] = 0xC3;
        code[0x40..0x44].copy_from_slice(&value.to_le_bytes());
        let buffer = sys::alloc(code.len()).unwrap();
        // SAFETY: `buffer` is a fresh RWX region of exactly `code.len()` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(code.as_ptr(), buffer.as_mut_ptr(), code.len());
            sys::flush_icache(buffer.as_mut_ptr(), code.len());
        }
        buffer
    }

    extern "C" fn detour_fn() -> u32 {
        0xDEAD
    }

    #[test]
    fn hooks_and_restores_a_rip_relative_prologue() {
        let buffer = rip_relative_function(0x1234_BEEF);
        // SAFETY: the buffer holds our hand-written, ABI-`extern "C"` function.
        let target: Fun = unsafe { std::mem::transmute::<*mut u8, Fun>(buffer.as_mut_ptr()) };
        assert_eq!(target(), 0x1234_BEEF, "the fixture reads its own constant");

        let detour_ptr = (detour_fn as *const ()).cast::<u8>();
        // SAFETY: single-threaded test; the target is not running elsewhere.
        let hook = unsafe { InlineDetour::install(buffer.as_mut_ptr(), detour_ptr) }.unwrap();

        assert_eq!(target(), 0xDEAD, "the detour now runs instead");
        // SAFETY: the trampoline reproduces the original, including the
        // relocated RIP-relative load.
        let original: Fun = unsafe { std::mem::transmute::<*const u8, Fun>(hook.trampoline()) };
        assert_eq!(
            original(),
            0x1234_BEEF,
            "the relocated prologue still reads the same constant"
        );

        // SAFETY: still single-threaded.
        unsafe { hook.detach() }.unwrap();
        assert_eq!(target(), 0x1234_BEEF, "detach restores the original bytes");
    }

    #[test]
    fn refuses_a_prologue_that_branches_immediately() {
        // A buffer that starts with `ret`: it cannot be stolen.
        let buffer = sys::alloc(PROLOGUE_SCAN).unwrap();
        // SAFETY: fresh RWX buffer; fill it with `ret` bytes.
        unsafe {
            std::ptr::write_bytes(buffer.as_mut_ptr(), 0xC3, PROLOGUE_SCAN);
            sys::flush_icache(buffer.as_mut_ptr(), PROLOGUE_SCAN);
        }
        // SAFETY: the target is never executed; install refuses before patching.
        let result = unsafe {
            InlineDetour::install(buffer.as_mut_ptr(), (detour_fn as *const ()).cast::<u8>())
        };
        assert!(matches!(
            result,
            Err(DetourError::UnsupportedPrologue { .. })
        ));
    }

    #[test]
    fn decode_prologue_relocation_adjusts_rip_displacement() {
        // Decode `mov eax,[rip+0x3a]` at one address, re-encode it at another,
        // and confirm the displacement changed so it still targets the same
        // absolute address.
        let code = [0x8B, 0x05, 0x3A, 0x00, 0x00, 0x00, 0x90, 0x90];
        let (instrs, steal) = decode_prologue(&code, 0x1000, 5).unwrap();
        assert_eq!(
            steal, 6,
            "only the RIP-relative mov is needed for a 5-byte patch"
        );
        let here = encode_block(&instrs, 0x1000).unwrap();
        let moved = encode_block(&instrs, 0x5000).unwrap();
        assert_ne!(
            here, moved,
            "moving the instruction rewrote its displacement"
        );
        assert_eq!(here[0..2], moved[0..2], "the opcode is unchanged");
    }
}
