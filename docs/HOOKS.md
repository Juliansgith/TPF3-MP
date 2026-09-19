# The native hook

The native hook is the small library that runs *inside* the game process. It
captures and cancels player commands, gates the simulation step, controls speed
and save/load, and talks to the [agent](ARCHITECTURE.md#components) over
shared memory. This document specifies the parts built at milestone M0: the
build-signature engine, the per-build profile format, the detour engine, the
shared-memory ABI, and the release-day procedure. Locating and detouring the
actual TPF3 functions comes with the release-day profile (see
[DAY_ONE.md](DAY_ONE.md)); everything the hook needs to do it is here and
tested.

Crates:

| crate | contents |
|---|---|
| `tpf3mp-hookcore` | pattern scanning, per-build profiles + resolution, the x86-64 inline detour engine, a small read-only PE reader |
| `tpf3mp-ipc` | the shared-memory link (this document's ABI) |
| `tpf3mp-hook` | the `cdylib` the game loads: platform entry points, profile loading, agent connection |
| `tpf3mp-proxygen` | generates a Windows proxy DLL that forwards every export to the renamed original |

## Design and the fail-closed rules

TPF3 will be patched often after launch, so the hook never pins raw addresses.
It carries one **profile** per game build. A profile binds a build identity
(executable SHA-256, and optionally file size and PE timestamp) to a set of
named **targets**, each located by a byte **signature** rather than an address.

Resolution is **fail-closed**. `tpf3mp_hookcore::profile::resolve` returns either
a complete, verified target table or a precise refusal, and it installs nothing
on the way to a refusal:

- **Unknown build** - the running executable's identity does not match the
  profile. The hook does not scan at all; multiplayer is disabled.
- **Missing** - a *required* target's signature is not found. Resolution fails
  as a whole, so required hooks are all-or-nothing; a partial install never
  happens.
- **Ambiguous** - a signature matches more than once. Refused, even for an
  optional target: a second, unexpected match is a corruption signal, not
  something to skip.
- **Prologue mismatch** - the signature matched but the exact bytes at the
  target are not the ones the profile expects. Refused.

An *optional* target that is simply absent is recorded and does not fail the
profile. Everything else fails closed. The hook logs the precise reason and
leaves the game untouched.

### Where the resolver scans (production vs. this repo's test)

`resolve` takes a byte slice plus the address its first byte corresponds to, so
it does not care whether those bytes come from a file or from memory.

- **Production**: the hook scans the running process's **mapped, unpacked module
  image** - the bytes the loader (and any DRM stub) produced in memory - passing
  the module's base address as the region base. This is the only correct source
  when a build's code section is packed or encrypted on disk.
- **This repo's static proof** (`tpf3mp-hookcore/tests/tpf2_static_proof.rs`)
  scans the executable **on disk**. That is a development convenience, valid
  only when the build's `.text` is readable on disk (see
  [the TPF2 verification](#what-was-verified-on-the-tpf2-binary)). The call is
  identical; only the byte source differs.

## Signatures

A signature is an IDA-style pattern: two hex digits per fixed byte and `??` (or
`?`) for a byte that may be anything, for example `48 8B ?? ?? E8`. Wildcards
exist so a signature skips the bytes that move between builds - RIP-relative
displacements, call targets, absolute addresses - and matches only the opcodes
and operands that identify the code. A signature must be **unique** across the
scanned region; the scanner reports zero, one, or many matches, and the resolver
treats "many" as a refusal.

Rules of thumb, applied to the TPF2 profile below:

- Prefer register/immediate operands; wildcard every relative or absolute
  displacement.
- Extend the pattern only as far as needed to make it unique. Two functions can
  share a prologue (the two TPF2 menu functions share a seven-`push` opening);
  run the signature to the first distinguishing bytes.
- Keep the **prologue** field free of wildcards: it is the exact code the detour
  engine relocates, and it is re-checked byte-for-byte after the scan.

## The profile format

A profile is TOML. `tpf3mp_hookcore::profile::Profile::from_toml` parses and
validates it.

```toml
name = "Transport Fever 2 Build 35924 (Windows x64)"
image_base = 0x140000000   # informational: what RVAs are relative to
region = ".text"           # informational: the section the resolver scans

[build]
sha256 = "782b904a8f7bbdac1f7a18528f1a5c778691e5aa3087c37c351bf6912585175c"
size = 72843280            # optional; checked when present
pe_timestamp = 0x675ABCC6  # optional; checked when present

[[target]]
name = "GameSim::Step"
signature = "40 53 41 56 48 83 EC 68 48 8B DA 4C 8B F1 48 81 FA E8 03 00 00"
offset = 0                 # bytes from the match to the target (default 0)
prologue = "40 53 41 56 48 83 EC 68 48 8B DA 4C 8B F1 48 81 FA E8 03 00 00"
required = true            # default true
```

- **`signature`** locates the target. **`offset`** (signed, default 0) is added
  to the match position to reach the target address, for the case where a
  signature must begin before or after the function it names.
- **`prologue`** is the exact, wildcard-free bytes expected at the target; the
  resolver verifies them and the detour engine relocates them.
- **`required`** (default `true`): a required target that does not resolve
  refuses the whole profile.

A resolved target's address is `region_base + match_index + offset`.

## The detour engine

`tpf3mp_hookcore::detour::InlineDetour` is an x86-64 inline hook. Installing it
overwrites a function's first instructions with a jump to a replacement, after
copying those instructions into a **trampoline** that ends by jumping back into
the function; calling the trampoline therefore runs the original.

- **Relocation.** The stolen prologue is decoded and re-encoded at the
  trampoline's address with iced-x86's block encoder, so a RIP-relative operand
  keeps addressing the same absolute memory from its new home. A prologue that
  cannot be relocated - it branches, returns, or does not decode - is **refused**
  (`DetourError::UnsupportedPrologue`) rather than patched wrong. Only
  straight-line instructions are stolen.
- **Patch form.** A near replacement (within 2 GiB) is reached with a 5-byte
  `jmp rel32`; otherwise a 14-byte `jmp [rip+0]` absolute jump. The trampoline
  always returns with an absolute jump, so it works at any distance.
- **iced-x86, not `retour`.** `retour`'s stable line is 0.3.1 (0.4 is alpha) and
  it owns trampoline allocation and instruction relocation internally - exactly
  the part that must be inspectable and testable on a binary that shifts every
  patch. iced-x86 is a pure-Rust decoder/encoder with no build script; the
  engine drives decode/relocate directly and keeps the trampoline and patch
  bytes in this crate, where tests read them.
- **Architecture.** The engine is x86-64 only. On any other architecture it
  compiles to a stub that returns `DetourError::UnsupportedArchitecture`, so the
  workspace still builds and the caller fails closed (see
  [macOS arm64](#macos-arm64)).

### Thread-safety assumptions

Installing overwrites up to fourteen live code bytes with a non-atomic copy. The
caller must guarantee the target cannot execute during install or uninstall:
**install before the target's first run**, or **park every thread that could
reach it first**. Both loaders satisfy the first condition - the Windows proxy
DLL and the Linux `LD_PRELOAD` library are in the process before its entry
point runs. The engine does not stop threads itself. Detours are removed by
dropping the handle (or `detach`), under the same quiescence rule. The engine's
own tests only ever hook functions inside the test binary, never another
process.

## The `tpf3mp-ipc` ABI

The hook and the agent share one memory mapping: a fixed 64-byte header followed
by two single-producer/single-consumer ring buffers. This section is
byte-exact, because the agent is written separately.

### Object naming and security

The logical link name is mapped to a per-user OS object:

- **Windows**: `CreateFileMappingW`/`MapViewOfFile` in the per-session `Local\`
  namespace, object name `Local\tpf3mp.<hash>` where `<hash>` includes the user
  name. No explicit security descriptor is passed, so the mapping gets the
  process token's default DACL - access for the creating user and SYSTEM only.
- **Linux/macOS**: `shm_open`/`mmap` with mode `0600` (owner only). The name is
  `/tpf3mp.<hash>`, where `<hash>` includes the uid; it is kept within macOS's
  31-character `shm_open` limit.

### Header layout (little-endian)

Total mapping size is `64 + 2 * ring_capacity` bytes.

| offset | size | field | notes |
|---|---|---|---|
| 0  | 4 | `magic` | `T3MP` (bytes `54 33 4D 50`), written **last** as a readiness flag |
| 4  | 4 | `abi_version` | currently `1` |
| 8  | 4 | `header_size` | `64` |
| 12 | 4 | `ring_capacity` | bytes per ring; power of two, `<= 2^31` |
| 16 | 4 | `max_message` | largest payload per message |
| 20 | 4 | `session` | non-zero link generation; changes on re-create |
| 24 | 4 | `hook_pid` | 0 until the hook attaches |
| 28 | 4 | `agent_pid` | 0 until the agent attaches |
| 32 | 8 | `hook_heartbeat` | `u64`, bumped by the hook |
| 40 | 8 | `agent_heartbeat` | `u64`, bumped by the agent |
| 48 | 4 | `h2a_head` | hook->agent read index (consumer: agent) |
| 52 | 4 | `h2a_tail` | hook->agent write index (producer: hook) |
| 56 | 4 | `a2h_head` | agent->hook read index (consumer: hook) |
| 60 | 4 | `a2h_tail` | agent->hook write index (producer: agent) |

Then the data areas: hook->agent at `[64, 64 + ring_capacity)`, agent->hook at
`[64 + ring_capacity, 64 + 2 * ring_capacity)`.

### Rings

Each ring is a byte stream carrying length-prefixed messages: a little-endian
`u32` payload length followed by that many payload bytes. Both the length and
the payload may wrap around the end of the buffer.

- `head` and `tail` are **free-running** `u32` counters (they wrap at `2^32`,
  not at the capacity). Bytes in the ring = `tail - head` with wrapping
  subtraction; this is correct because capacity is a power of two `<= 2^31`. The
  index into the data area is `counter & (capacity - 1)`.
- **Producer**: writes the payload bytes, then stores `tail` with **Release**.
  It reads `head` with **Acquire** to compute free space; it only writes `tail`.
- **Consumer**: reads `tail` with **Acquire**, reads the bytes, then stores
  `head` with **Release**. It only writes `head`.
- A message larger than `max_message` is rejected by the producer; a full ring
  returns "full". Nothing is allocated on either side of a send or receive.

### Startup, heartbeat and restart

- **Startup.** One side (in TPF3-MP, the agent) is the owner: it creates the
  mapping, zeroes the header, writes the ABI version, ring sizes and a fresh
  non-zero `session`, sets its pid and heartbeat, and **publishes `magic` last**
  with a Release store. The other side opens the mapping and reads `magic` with
  an Acquire load; until it appears the open returns "not ready". The opener
  then checks `abi_version` and `header_size`, reads the ring sizes, and sets
  its own pid and heartbeat. The hook fails closed (runs solo) if no mapping is
  present.
- **Heartbeat.** Each side bumps its own counter and reads the peer's. A counter
  that stops advancing means the peer is gone.
- **Restart.** The owner re-creates the mapping with a new `session`. A peer
  that sees `session` change knows the rings were reset and drops anything in
  flight, then re-syncs from the new generation.

## The bridge: what travels over the link

`tpf3mp-bridge` defines the messages, postcard-encoded, one per ring frame,
at most 60 KiB each. It has no async runtime or network code, so the hook can
link it. The agent's side is `tpf3mp_agent::bridge`.

- **From the agent (`ToHook`):**
  - `Hello`: always first.
  - `Begin`: a game starts; load the world.
  - `Apply(event)`: apply this event before its step.
  - `Release { through }`: steps up to and including this one may run.
  - `Speed`: the room's speed, for display only.
  - `Diverged`, `Refused`: tell the player.
  - `End`: the session is over.
- **From the hook (`ToAgent`):**
  - `Hello`: always first, with the game build.
  - `Loaded { next_step }`: the world is ready.
  - `Command { payload }`: the player acted; the room orders it.
  - `Ran { step }`: the game ran this step.
  - `Checkpoint { step, lanes }`: digests at a checkpoint.
  - `Log`: a line for the agent's log.
- **The step gate.** The game asks the hook's `Gate` before every step. Until
  the step is released, the hook reads messages and applies each event the
  gate hands over, so an event for step `s` is applied after step `s - 1`
  and before step `s`, never mid-step.
- **Ordering.** The agent sends every event for step `s` after the release of
  step `s - 1` and before the release of step `s`. It only merges releases
  of consecutive steps with no event between them. The hook stops reading
  once its next step is released. The gate refuses anything that breaks
  this: an event for another step, an event after its step's release, or a
  release that goes back. The hook must then stop following and say so.
- **Pacing.** The agent releases steps on its jitter-buffered schedule
  (`Playout`). The game runs a released step at its own speed and waits at
  the gate for the next one. It reports each step it ran; the agent reports
  progress to the server from that, at most every 20 ms.
- **Liveness.** The hook must beat its heartbeat from a thread of its own,
  since the game thread blocks while loading. The agent gives up on a hook
  whose heartbeat stands still for 60 s.

`tpf3mp_testkit::fake_hook` is a complete hook for the toy game: the real
link, the real gate, and a world that steps only when released. The
`games_behind_the_bridge_and_gate_agree` scenario runs three of them in
one room end to end. On release day, the game-specific part of the hook
does what the fake hook does with the toy world, applying commands and
running steps through the real game.

## Release-day procedure: adding a target for a new build

1. **Archive the build.** Record the executable SHA-256, file size and PE
   timestamp (`BuildIdentity::of_file`), plus the Steam build/manifest ids. Keep
   a private copy (see [DAY_ONE.md](DAY_ONE.md)).
2. **Find the function** with the RE pipeline, and note its RVA and the bytes at
   its start.
3. **Write a signature.** Take the opening bytes; replace every relative or
   absolute displacement with `??`; extend only until the pattern is unique
   across the scanned section. Record the exact, wildcard-free `prologue` (at
   least the number of bytes the detour must steal - 5 for a near hook, 14 for a
   far one, on an instruction boundary).
4. **Add a `[[target]]`** to the build's profile with `name`, `signature`,
   `offset`, `prologue` and `required`.
5. **Verify.** Resolve the profile against the **in-memory module image** of the
   running build and confirm the target resolves uniquely to the expected
   address and that the prologue matches. Keep a static check against an
   archived copy where the code section is readable on disk.
6. **Never widen a signature to force a match** on a build you have not archived.
   An unknown build must stay unknown, so the hook fails closed.

## What was verified on the TPF2 binary

Against `TransportFever2.exe`, Steam build 35924 (SHA-256
`782b904a...585175c`, size 72,843,280, PE timestamp `0x675ABCC6`, image base
`0x140000000`), the profile in `tpf3mp-hookcore/tests/data/tpf2_build35924.toml`
resolves all five targets, each **matching exactly once** across `.text`:

| target | RVA |
|---|---|
| `GameSim::Step` | `0x15aa00` |
| `CGame::Step` | `0x118e90` |
| `CGameTime::GetSpeed` | `0x2877a0` |
| `UI::CMenuUI::StartSavegame` | `0x6785c0` |
| `UI::CMenuUI::CreatePage` | `0x663370` |

`CGameTime::GetSpeed` sits next to two near-identical siblings, so its signature
runs past the (wildcarded) call to the distinguishing `mov eax,[rax+4]`;
`StartSavegame` and `CreatePage` share a seven-`push` prologue, so each signature
runs to its distinct `lea`/frame bytes (and `CreatePage` to the `mov
[rsp+0x330],rbx` store that separates it from a twin at `0x215c480`). The test
also confirms the resolver refuses a modified copy (corrupting one target's
bytes yields a `Missing` refusal) and refuses a mismatched build identity.

**DRM note.** This build carries a SteamStub section (`.bind`, high entropy),
which can decrypt code at load time. For build 35924 the code section is
nonetheless **readable on disk**: all five prologues match the on-disk `.text`
exactly, consistent with the RE survey's ~88,000 assert-string references found
in the same on-disk section. On-disk verification is therefore valid *for this
build*. It is not guaranteed in general - a future build could encrypt `.text` -
which is why the production resolver scans the in-memory, unpacked module image,
and why on-disk scanning is documented as a development convenience only.
