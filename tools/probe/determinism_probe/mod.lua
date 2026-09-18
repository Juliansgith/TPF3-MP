-- tools/probe/determinism_probe/mod.lua
--
-- A read-only Transport Fever game-script mod that hashes the simulation lanes
-- from docs/DAY_ONE.md section 4 every N simulation steps and appends one line
-- per sample to a log. Run two instances from the same save with no input; feed
-- the two logs to tools/probe/compare_runs.py to find the first step at which a
-- lane diverges. This is the D2 determinism calibration.
--
-- Written against the TPF2 script API (best guess for TPF3). Every lane is in a
-- pcall and logs "err" rather than crashing, so a missing API narrows the probe
-- instead of stopping it. No globals are created; nothing is written to the world.
--
-- ASSUMPTION (TPF2, re-checked on TPF3): Script Mod folder = mod.lua +
-- res/config/game_script/<name>.lua; update() runs in the authoritative engine
-- state once per simulation step.
function data()
  return {
    info = {
      minorVersion = 0,
      severityAdd = "NONE",
      severityRemove = "NONE",
      name = "TPF3-MP probe: determinism_probe",
      description = [[
Read-only recon probe. Every N engine steps (N = $TPF3MP_PROBE_STRIDE, default
100) it appends a digest of the determinism lanes (vehicle count/positions, edge
geometry, constructions, town building counts, money per player, people count)
to determinism_probe_<instance>.log in the probe output dir. Set a distinct
$TPF3MP_PROBE_INSTANCE per running game. It changes nothing in the world.]],
      tags = { "Script Mod" },
      authors = { { name = "TPF3-MP recon", role = "CREATOR" } },
      visible = true,
    },
    runFn = function(_settings) end,  -- no modifiers: never perturb the measured world
  }
end
