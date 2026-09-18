# Release-day investigation

Transport Fever 3 releases on 2026-09-29. Until then, nothing in this project
has been checked against the real game. This plan settles the **[needs game]**
questions in [ARCHITECTURE.md](ARCHITECTURE.md), in order of how much they can
change the design. Results go into `investigation/TPF3_RECON_<date>.md`, and
each finding carries one label:

- **CONFIRMED**: decompiled or named by the binary's own strings, and
  consistent with live behaviour.
- **MEASURED**: observed in the running game.
- **INFERRED**: placed by elimination only.

## 1. Archive every build

- Record for every build:
  - Steam build ID and depot manifest IDs;
  - executable SHA-256, PE timestamp and image size (Windows);
  - the Linux and macOS binaries' hashes.
- Keep a private copy of every executable. Signature profiles are verified
  against it, and hooks must fail closed on anything unrecorded.
- Expect a day-one patch and frequent patches after it.

## 2. Static recon (Windows executable first)

- **Loader:** find a proxy candidate (a small DLL the executable imports
  statically from its own folder, like TPF2's `alut.dll`).
- **Anti-tamper:** packer or protection sections, section entropy, TLS
  callbacks. Anti-tamper changes the native plan and must be known first.
- **Symbols:**
  - RTTI;
  - MSVC `__FUNCSIG__` and `__FILE__` assert strings, run through the naming
    pipeline from `tpf2-multiplayer/tools/re` and `tools/ghidra`, made
    build-independent first;
  - TPF2-era names: `make_cmd::`, `CommandList::Add`, `GameSim::Step`,
    `CGame::RunGameSimLoop`.
- **Lua:** Lua version strings and the script API table names.
- **Linux and macOS:** repeat the symbol and string survey. Record whether the
  macOS binary is hardened-runtime, library-validated, and allows
  `DYLD_INSERT_LIBRARIES` (`codesign -dv --entitlements - <binary>`).

## 3. Script API recon (a probe mod, all three platforms)

- `_VERSION`; availability of `io`, `os`, `require`, `package`, `debug`,
  `load`/`loadstring` in both the game-script and GUI states.
- Dump `api.*` and `game.interface.*`; list `api.cmd.make.*` factories.
- Number formatting (`%.17g`), `math.random` behaviour, `pairs` order
  stability for string keys across runs.
- The mod layout and `mod.lua` format, and what a Mod Hub script mod may
  contain.

## 4. Determinism measurement (the D2 calibration)

Run two instances from the same save with no input. Hash these lanes every
100 steps for 60 in-game days (the TPF2 baseline):

- vehicle count;
- vehicle positions at 1 m;
- edge geometry at 0.1 m;
- the construction list;
- town building counts;
- money per player;
- people count.

Then repeat with a scripted input sequence. Record, per lane, the step where
two runs first differ:

| pair | expectation |
|---|---|
| same PC, same binary | identical (TPF2 was) |
| Intel vs AMD, Windows | identical if no CPU-dispatched math paths |
| Windows vs Linux native | probably drifts (different compilers and C runtime) |
| Windows vs Linux under Proton | measure: same binary, but Wine's math library |
| Windows vs macOS arm64 | expected to drift |

The results decide how far same-binary rooms may relax drift control. They
do not change D2.

## 5. Hook feasibility, per platform

- **Windows:** proxy DLL loads before the title menu.
- **Linux:** `LD_PRELOAD` from Steam launch options.
- **macOS:** proxy of a bundled dylib, or re-signed insertion. Test whether
  code pages can be patched under the process's code-signing flags.

## 6. Command pipeline and time

- Locate the command factories and the command queue, then prototype capture
  and cancel for one command (road build) on Windows. Use ground-truth sweeps
  through `api.cmd.make.*` rather than inferring from player clicks.
- Locate the simulation step, the step size in game time, speed and pause
  control, and the injection point just before a step runs.

## 7. Saves

- Force a save and load a named save from native code.
- Load a save made on Windows on Linux and macOS, and the reverse.
- Measure save sizes for small, medium and large maps.

## Deliverable

A dated recon report containing:

- the determinism table;
- a go or no-go per platform for the hook;
- the first build's signature profile;
- the list of script API capabilities.
