-- tools/probe/script_api_dump/mod.lua
--
-- A read-only Transport Fever *game-script* mod that dumps the script sandbox
-- and API surface from BOTH Lua states (engine + GUI). It answers docs/DAY_ONE.md
-- section 3 ("Script API recon") on release day.
--
-- It is written against the Transport Fever 2 script API, which is the best
-- available guess for TPF3 (docs/ARCHITECTURE.md open question: "What Lua version
-- and sandbox do game and GUI scripts get?"). Every access is wrapped in pcall
-- and records "absent"/"error" rather than assuming a call exists, so it runs to
-- completion even if TPF3's API differs. Nothing is written back to the game.
--
-- ASSUMPTION (TPF2, to be re-checked on TPF3): a Script Mod is a folder with this
-- mod.lua plus res/config/game_script/<name>.lua, and the game runs the returned
-- update() in the engine state and guiUpdate() in the separate GUI state.
function data()
  return {
    info = {
      minorVersion = 0,
      severityAdd = "NONE",
      severityRemove = "NONE",
      name = "TPF3-MP probe: script_api_dump",
      description = [[
Read-only recon probe. On the first engine tick and the first GUI tick it writes
one report per Lua state (script_api_dump_engine.txt / script_api_dump_gui.txt)
to the probe output directory, then goes quiet. It changes nothing in the game.
Output dir: $TPF3MP_PROBE_DIR, else $LOCALAPPDATA/tpf3mp/probe, else the working
directory. See tools/probe/README-equivalent notes in docs/DAY_ONE.md.]],
      tags = { "Script Mod" },
      authors = { { name = "TPF3-MP recon", role = "CREATOR" } },
      visible = true,
    },
    -- No resource modifiers: a recon probe must not perturb the world it measures.
    runFn = function(_settings) end,
  }
end
