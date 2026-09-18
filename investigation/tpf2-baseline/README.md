# TPF2 baseline: recon-tool outputs

These are the outputs of the `tools/re` and `tools/probe` tools run against the
**Transport Fever 2** build 35924 executable (SHA-256
`782b904a8f7bbdac1f7a18528f1a5c778691e5aa3087c37c351bf6912585175c`), kept as the
pre-release baseline. TPF2 is the only real binary available before TPF3 ships on
2026-09-29; on release day the same tools run against the TPF3 builds and the
results go in `../TPF3_RECON_<date>.md`.

The executable itself is **not** committed (read-only, never modified). Large
dumps (the full symbol CSV/JSON, the Ghidra/x64dbg/IDA scripts -- ~78 MB) are not
committed either; the numbers below summarise them.

| file | tool | what it is |
|---|---|---|
| `binary_survey_TransportFever2.md` | `binary_survey.py` | full static survey of the PE |
| `name_functions_TransportFever2.txt` | `name_functions.py` | naming coverage + validation of the 8 known RVAs |
| `tpf2_known_rvas.txt` | (input) | the known-RVA validation spec |
| `diff_builds_demo.md` | `diff_builds.py` | diff of the TPF2 map vs a **synthetic** day-two patch (no second real build exists yet) |
| `re_format_selftest.txt` | `selftest.py` | ELF x86-64 / ELF arm64 / Mach-O arm64 path validation on synthetic fixtures |
| `probe_lua_syntax_check.txt` | `check_lua.py` | Lua 5.2 (and 5.1) syntax check of both probe mods |
| `determinism_compare_selftest.md` | `compare_runs.py` | first-divergence report for two probe runs |
| `determinism_probe_{a,c}_selftest.log` | `determinism_probe` mod | sample probe logs |
| `script_api_dump_engine_selftest.txt` | `script_api_dump` mod | sample script-API dump (engine state) |

## Provenance of the probe outputs

The game is never launched by this tooling, so the two probe **mods**
(`script_api_dump`, `determinism_probe`) cannot run inside a real TPF2/TPF3 VM
here. The `*_selftest*` files were produced by driving the mods' Lua under a
**mock, TPF2-shaped `api`/`game.interface`** in an embedded Lua 5.2 (lupa) -- see
the self-test harness. They prove the probe Lua runs end to end and that
`compare_runs.py` pinpoints a divergence (the `c` run has one vehicle moved 5 m
and one extra construction, so the `p` and `c` lanes differ at the first sample).
The real determinism numbers for the DAY_ONE section-4 table come on release day
by running the mods inside two live game instances.

## Headline TPF2 findings

- **Packer:** Steam DRM (SteamStub) in the `.bind` section (entropy 7.95); no
  Denuvo/VMProtect/Themida. Static scanning of the game's own code must therefore
  run against the in-memory (unpacked) image, not the on-disk file. A proxy-DLL
  hook still works because it loads into the already-unpacked process.
- **Proxy-loader candidate:** `alut.dll` -- statically imported from the game
  folder, only 20 exports (the same role the TPF2 mods use).
- **RTTI:** present (~4,100 MSVC type descriptors) -> a third naming axis.
- **Lua:** 5.2.2 with sol2.
- **Naming:** ~18,900 `__FUNCSIG__` strings and 729 `__FILE__` paths yield
  ~24,200 functions named and ~54,000 attributed to 725 source files; all 8
  known RVAs validate (5 named exactly, 3 correctly declined -- the binary
  embeds no `__FUNCSIG__` for `CGameTime::GetSpeed`, `make_cmd::BuildProposal`
  or `CommandList::Add` -- and attributed to the right `.cpp`).
