# TPF2MP Lua sources (test fixtures)

These files are byte-identical copies of TPF2MP sources. The differential
tests in `../differential/` run them under Lua 5.1 next to the Rust port in
`src/economy/` and require identical results (see `docs/ECONOMY.md`).

- Source: `tf2mod` (TPF2MP by Julian Cooper), commit
  `58da402ba144b2b5ad9f5615f7d686aadd34ffb2`.
- Licence: MIT, copyright (c) 2026 Julian Cooper, the same licence and holder
  as this repository (see `LICENSE` at the repository root).
- `.gitattributes` disables line-ending conversion so the copies stay
  identical on every platform.

| file here | path in `tf2mod` |
|---|---|
| `tpf2_mp/*.lua` | `tpf2_mp_1/res/scripts/tpf2_mp/*.lua` |
| `run_economy_parity_vectors.lua` | `tests/run_economy_parity_vectors.lua` |

`tpf2_mp/` holds the ported economy modules and everything `economy.lua`
requires, so that TPF2MP's own parity-vector generator runs unmodified:

- ported: `economy_flow`, `economy_allocation`, `economy_revenue`,
  `economy_costs`, `economy_difficulty`, `economy_town_demand`,
  `economy_feeder_access`, `util`, and the pure arithmetic of `economy`;
- dependencies only: `hash`, `json`, `delivery_snapshot`,
  `multihop_network`, `multihop_passenger`, `multihop_cargo`,
  `transport_network_graph`, `freight_path_pin`.

The harness does not edit these files on disk. When it loads a module it
inserts a short block before the module's final `return M` that hands the
module's local functions to the harness (Lua 5.1's `debug` library would do
the same, but mlua only loads it in unsafe mode). The block runs after every
definition and changes no behaviour; `instrument` in
`../differential/tpf2mp.rs` builds it.

## Updating to a newer TPF2MP commit

1. Copy the files again from the paths above and record the new commit here,
   in `src/economy/mod.rs` and in `docs/ECONOMY.md`.
2. Run `cargo test -p tpf3mp-canon`. The harness checks that every hooked
   local function still exists, and the replay requires every traced call
   to match the port. Any rule change surfaces as a failing call with its
   inputs.
