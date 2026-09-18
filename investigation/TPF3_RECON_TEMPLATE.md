# TPF3 release-day recon -- YYYY-MM-DD (build <steam-build-id>)

> Copy this file to `investigation/TPF3_RECON_<date>.md` and fill it in as the
> tools run. This is the deliverable `docs/DAY_ONE.md` asks for. Every claim
> carries one label:
>
> - **CONFIRMED** -- decompiled or named by the binary's own strings, and
>   consistent with live behaviour.
> - **MEASURED** -- observed in the running game.
> - **INFERRED** -- placed by elimination only.
>
> Leave a value as `UNKNOWN` rather than guessing; the tools are written to
> detect and report "unknown" where something can only be seen on release day.
> The tool that produces each value is named in the section. Run order and exact
> commands: see the "Tools" section of `docs/DAY_ONE.md`.

Participants / machines / save used: ______

---

## 1. Archive every build  (DAY_ONE section 1)

For every platform build obtained today (`binary_survey.py` gives hashes/sizes;
Steam client gives build & manifest IDs):

| platform | steam build id | depot / manifest id | executable SHA-256 | file size | notes |
|---|---|---|---|---|---|
| Windows x64 | | | | | PE timestamp: |
| Linux x64 | | | | | build-id: |
| macOS arm64 | | | | | |

- Private copies stored at: ______
- Day-one patch expected: yes / no -- observed at: ______  (re-run everything and
  `diff_builds.py` the two symbol maps if it lands.)

---

## 2. Static recon  (DAY_ONE section 2)   -- `binary_survey.py`, `name_functions.py`

Attach the per-platform `binary_survey.py` reports. Summary:

### 2.1 Loader / proxy candidate  (per platform)

| platform | mechanism | candidate | why | label |
|---|---|---|---|---|
| Windows | proxy DLL next to the exe | ____ (survey "proxy score") | statically imported, game-folder, few exports | |
| Linux | `LD_PRELOAD` | n/a (preload) | | |
| macOS | proxied bundled dylib / re-sign | ____ | | |

### 2.2 Anti-tamper / packer

| platform | packer/DRM | evidence | consequence | label |
|---|---|---|---|---|
| Windows | ____ (e.g. SteamStub `.bind`) | survey section 6 | static scan vs in-memory image? | |
| Linux | | | | |
| macOS | | | | |

- TLS callbacks / initializers that run before entry: ______
- Is the on-disk code unpacked (signatures scannable statically)? Windows __ / Linux __ / macOS __

### 2.3 Symbols  -- `name_functions.py`

| platform | funcsig/pretty strings | __FILE__ paths | functions named | files attributed | label |
|---|---|---|---|---|---|
| Windows | | | | | |
| Linux | | | | | |
| macOS | | | | | |

- RTTI present? Windows __ / Linux __ / macOS __  (survey section 7)
- Source-path prefix (from `name_functions.py`): ______
- TPF2-era engine names found (`make_cmd::`, `CommandList::Add`, `GameSim::Step`,
  `CGame::RunGameSimLoop`, `CGameTime::`)?  ______  (survey section 10)
  -> **Is TPF3 the same engine lineage as TPF2?**  CONFIRMED / INFERRED: ______

### 2.4 Key function RVAs (fill the hook targets; validate with a spec file)

| function | Windows RVA | Linux RVA | macOS RVA | how found | label |
|---|---|---|---|---|---|
| GameSim::Step | | | | funcsig / RTTI / signature | |
| CGame::RunGameSimLoop | | | | | |
| CGame::Step | | | | | |
| CGameTime::GetSpeed | | | | | |
| command factory (make_cmd / api.cmd.make) | | | | | |
| CommandList::Add | | | | | |
| save / load entry (StartSavegame) | | | | | |

---

## 3. Script API recon  (DAY_ONE section 3)   -- `script_api_dump` mod

Attach `script_api_dump_engine.txt` and `script_api_dump_gui.txt` per platform.

| item | engine state | GUI state | label |
|---|---|---|---|
| `_VERSION` | | | |
| io / os / require / package / debug / load(loadstring) | | | |
| `api.cmd.make.*` factory count | | | |
| `game.interface.sendScriptEvent` present | | | |
| number format `%.17g` of 0.1 | | | |
| tie rounding (`%.0f` of 0.5/1.5/2.5) | | | |
| `math.random` seedable / sequence | | | |
| `pairs()` order stable for string keys | | | |

- Lua version / sandbox verdict (docs/ARCHITECTURE.md open question): ______
- Cross-platform: same `_VERSION` and sandbox on all three? ______

---

## 4. Determinism measurement (the D2 calibration)  (DAY_ONE section 4)
`determinism_probe` mod + `compare_runs.py`

Two instances from the same save, no input, sampled every 100 steps for 60
in-game days; then repeated with a scripted input sequence. Per lane, the step
where two runs first differ (blank = identical for the whole run):

| pair | vehicles | positions | edges | constructions | towns | money | people | label |
|---|---|---|---|---|---|---|---|---|
| same PC, same binary | | | | | | | | |
| Intel vs AMD, Windows | | | | | | | | |
| Windows vs Linux native | | | | | | | | |
| Windows vs Linux (Proton) | | | | | | | | |
| Windows vs macOS arm64 | | | | | | | | |

- Expectation (from DAY_ONE): same PC identical; Win/Linux-native probably drifts;
  macOS arm64 expected to drift (FMA contraction). Observed vs expected: ______
- **Consequence for same-binary rooms** (may drift control relax? optimisation only):
  ______
- This does **not** change D2 (canonical state is authoritative). Confirm: ______

---

## 5. Hook feasibility, per platform  (DAY_ONE section 5)

| platform | loads before title menu? | can patch code pages? | verdict |
|---|---|---|---|
| Windows (proxy DLL) | | | GO / NO-GO |
| Linux (`LD_PRELOAD`) | | | GO / NO-GO |
| macOS (dylib proxy / re-sign) | | `DYLD_INSERT_LIBRARIES` allowed? hardened runtime? library validation? | GO / NO-GO |

- macOS code-signing detail (`binary_survey.py` section 11 + the `codesign` command
  it prints): CS flags ____, hardened runtime ____, entitlements ____.  label: ______

---

## 6. Command pipeline and time  (DAY_ONE section 6)

- Command factories located: ______  (ground-truth sweep through `api.cmd.make.*`)
- Command queue / `CommandList::Add` equivalent: ______
- Capture + cancel prototyped for one command (road build) on Windows: yes/no -- notes: ______
- Simulation step function / step size in game time / speed & pause control: ______
- Injection point just before a step runs: ______   label: ______

---

## 7. Saves  (DAY_ONE section 7)

- Force-save from native code: works? ______
- Load a named save from native code: works? ______
- Cross-platform save load (Win<->Linux<->macOS, both directions): ______
- Save sizes (small / medium / large map): ______   label: ______

---

## Deliverable summary  (DAY_ONE "Deliverable")

### Go / no-go per platform for the hook

| platform | GO / NO-GO | blocking issue (if any) |
|---|---|---|
| Windows x64 | | |
| Linux x64 | | |
| macOS arm64 | | |

### First build's signature profile (for the hook's fail-closed build check)

- Windows: SHA-256 ____, PE timestamp ____, SizeOfImage ____, + N pinned byte
  signatures at their RVAs (from `name_functions.py` / `binary_survey.py`).
- Linux: SHA-256 ____, build-id ____.
- macOS: SHA-256 ____, cdhash ____.

### Script API capabilities (one-line verdict)

______

### Open questions from docs/ARCHITECTURE.md -- resolved?

- Same engine lineage as TPF2 (command pipeline, RTTI, assert strings)? ______
- Lua version and sandbox (game + GUI)? ______
- Any anti-tamper? ______
- Native determinism per platform pair? ______
- macOS hook feasible, and saves really cross-platform? ______
