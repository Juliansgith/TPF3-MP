//! Adversarial-review proof-of-concept for the detour engine's relocation.
//!
//! These probe iced-x86 directly (the same decode/encode the engine drives in
//! `detour/x86_64.rs`) to establish, on this exact iced-x86 version, what
//! happens when a stolen RIP-relative instruction's absolute target is farther
//! than a signed 32-bit displacement can reach from the trampoline. The engine
//! allocates the trampoline with a plain `VirtualAlloc(null, ..)` /
//! `mmap(null, ..)` (see `detour/sys.rs`), which places it anywhere in the
//! 64-bit address space - with no attempt to stay within +/-2 GiB of the target.
//!
//! Conclusion (documented by the two tests): the relocation is *sound* - it
//! never miscompiles; it either emits a correct form or returns an error the
//! engine surfaces as `DetourError::Encode`. But that error path is reachable in
//! production for a normal, high-loaded module, which is the robustness finding.
//!
//! `#[ignore]`d so the normal suite stays green; both are safe to run.
#![allow(clippy::unwrap_used)]

use iced_x86::{BlockEncoder, BlockEncoderOptions, Decoder, DecoderOptions, InstructionBlock};

/// `mov eax, [rip+0x3a]` - the RIP-relative fixture the engine's own tests use;
/// representative of a prologue that loads or addresses a global.
const RIP_REL_MOV: [u8; 6] = [0x8B, 0x05, 0x3A, 0x00, 0x00, 0x00];

fn decode_one(code: &[u8], ip: u64) -> iced_x86::Instruction {
    let mut decoder = Decoder::with_ip(64, code, ip, DecoderOptions::NONE);
    decoder.decode()
}

fn encode_at(insn: iced_x86::Instruction, rip: u64) -> Result<Vec<u8>, String> {
    BlockEncoder::encode(
        64,
        InstructionBlock::new(&[insn], rip),
        BlockEncoderOptions::NONE,
    )
    .map(|r| r.code_buffer)
    .map_err(|e| e.to_string())
}

/// Near relocation re-encodes fine and rewrites the displacement so it still
/// addresses the same absolute memory. Confirms the fixture and the sound path.
#[test]
#[ignore = "review PoC: near RIP-relative relocation stays correct; safe to run"]
fn near_rip_relative_relocation_stays_correct() {
    let insn = decode_one(&RIP_REL_MOV, 0x1000);
    let target = insn.memory_displacement64();
    let moved = encode_at(insn, 0x5000).expect("near move encodes");
    let re = decode_one(&moved, 0x5000);
    assert_eq!(
        re.memory_displacement64(),
        target,
        "the relocated instruction must address the same absolute memory"
    );
}

/// FINDING (Medium, robustness - fails closed, does not miscompile). For a
/// module loaded high in the address space (the norm on 64-bit Windows with
/// ASLR, e.g. base ~0x7FF6_xxxx_xxxx) whose stolen prologue contains a
/// RIP-relative operand, if the trampoline is allocated more than 2 GiB away -
/// which `sys::alloc`'s unhinted `VirtualAlloc(null)` / `mmap(null)` does not
/// prevent - the displacement cannot be re-encoded and iced-x86 returns an
/// error. The engine turns that into `DetourError::Encode` and installs nothing:
/// sound, but it means installing an otherwise-valid hook can *fail in
/// production* for exactly the functions the mod wants to hook.
///
/// The test also confirms the encoder never silently miscompiles: when it does
/// return `Ok` (target reachable via the 32-bit address-size wrap), the emitted
/// instruction re-decodes to the original absolute target.
#[test]
#[ignore = "review PoC (Medium): far RIP-relative relocation fails closed for a high module; safe to run"]
fn far_rip_relative_relocation_fails_closed_for_a_high_module() {
    // A module loaded above 4 GiB (typical), trampoline ~3 GiB above it.
    let insn = decode_one(&RIP_REL_MOV, 0x7FF6_0000_1000);
    let result = encode_at(insn, 0x7FF6_0000_1000 + 0xC000_0000);
    assert!(
        result.is_err(),
        "a high-loaded module's RIP-relative prologue cannot be relocated to a \
         trampoline > 2 GiB away; the engine must (and does) fail closed here"
    );

    // Where iced-x86 *can* encode it (low target reachable by the 32-bit wrap),
    // the result is correct, not a miscompile.
    let low = decode_one(&RIP_REL_MOV, 0x1000);
    let low_target = low.memory_displacement64();
    if let Ok(bytes) = encode_at(low, 0x1000 + 0xC000_0000) {
        let re = decode_one(&bytes, 0x1000 + 0xC000_0000);
        assert_eq!(
            re.memory_displacement64(),
            low_target,
            "when the encoder emits code it must address the same absolute memory"
        );
    }
}
