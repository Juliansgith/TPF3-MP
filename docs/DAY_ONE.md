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
- Find what a save can make the game run (script state, mod code), and
  write the check the agent applies to a received save before the game
  loads it, as TPF2MP's `save_metadata.py` did. Until then a received save
  is only as trustworthy as the player who uploaded it.
- Run `measure --pair` on two saves of one world taken minutes apart, to
  see how well snapshots deduplicate (SNAPSHOTS.md).

## Deliverable

A dated recon report containing:

- the determinism table;
- a go or no-go per platform for the hook;
- the first build's signature profile;
- the list of script API capabilities.

Start it from `investigation/TPF3_RECON_TEMPLATE.md`.

## Tools

The tools that turn this plan into findings live under `tools/`. They are
read-only (no game is launched or modified) and their output is deterministic.
They were built and validated against Transport Fever 2 build 35924; the baseline
outputs are in `investigation/tpf2-baseline/`. One-time setup:
`python -m venv .venv && .venv/Scripts/pip install -r tools/requirements.txt`.

| tool | what it does | feeds |
|---|---|---|
| `tools/re/binary_survey.py <bin> -o out.md` | identity, sections+entropy, imports (system vs game-folder, with proxy-loader ranking), exports, TLS callbacks, packer/anti-tamper, RTTI, Lua version, `__FUNCSIG__`/`__FILE__` counts, TPF2-era names, and macOS code-signing posture. Handles PE, ELF and Mach-O. | §1, §2, §5 |
| `tools/re/name_functions.py <bin> -o dir [--validate spec]` | recovers a symbol map (RVA -> name, source file) from assert strings, build-independent: `.pdata` bounds on PE, LIEF function starts on ELF/Mach-O, x86-64 and arm64 reference resolution. Emits JSON+CSV and Ghidra/x64dbg/IDA scripts. | §2, §6 |
| `tools/re/diff_builds.py OLD.symbols.json NEW.symbols.json` | which named functions moved, resized, appeared or disappeared between two builds -- run it after a day-two patch to re-verify hook signatures. | §1, §2 |
| `tools/re/selftest.py` | validates the ELF / Mach-O / arm64 code paths on synthetic fixtures (no game binary needed). | -- |
| `tools/probe/script_api_dump/` | game-script mod: dumps the Lua sandbox and `api.*`/`game.interface.*` from the engine and GUI states. | §3 |
| `tools/probe/determinism_probe/` | game-script mod: hashes the §4 lanes every N steps to a per-instance log. | §4 |
| `tools/probe/compare_runs.py A.log B.log` | first per-lane divergence between two determinism logs. | §4 |
| `tools/probe/check_lua.py` | syntax-checks the probe Lua with a real Lua 5.2. | §3, §4 |

Release-day order:

1. **Archive + static (per platform):** run `binary_survey.py` on each build
   (§1 hashes/sizes, §2 loader/packer/Lua/RTTI), then `name_functions.py` on each
   to get the symbol maps and hook-target RVAs (§2.4). Validate known targets
   with a `--validate` spec once they are located.
2. **On a patch:** re-run `name_functions.py` and `diff_builds.py` the old and new
   maps; re-verify any hook whose target is listed resized/appeared/disappeared.
3. **Script API (per platform):** `check_lua.py` first, then install the
   `script_api_dump` mod, launch, and collect the engine/GUI dumps (§3).
4. **Determinism (the D2 calibration):** install `determinism_probe` on two
   instances from the same save (set a distinct `$TPF3MP_PROBE_INSTANCE` each),
   run the pairs in the §4 table, then `compare_runs.py` the logs.
5. Fill `investigation/TPF3_RECON_TEMPLATE.md` as results arrive.
