-- tools/probe/script_api_dump/res/config/game_script/script_api_dump.lua
--
-- Dumps the Lua sandbox and the api.* / game.interface.* surface from whichever
-- state runs this chunk, once per state, then goes quiet. update() runs in the
-- engine state and guiUpdate() in the GUI state (TPF2 behaviour; re-verified for
-- TPF3 by the fact that each writes its own file and tags which state it was).
--
-- Design rules (docs/DAY_ONE.md style guide): pcall around every probe, no global
-- pollution (every name below is a file-scope local), deterministic output
-- (sorted keys), and "unknown"/"absent" instead of a guess when a call is missing.
--
-- ASSUMPTIONS, all re-checked at runtime rather than trusted:
--   * the API mirrors TPF2 (sol2 usertypes that refuse pairs(); api.cmd.make.*
--     factories are callable tables; game.interface.* free functions);
--   * os.getenv and io.open exist and the working directory is the game folder
--     (TPF2). If not, the probe still writes what it can and records the gap.

local OUTDIR_ENV = "TPF3MP_PROBE_DIR"        -- optional pin from the launcher
local writtenEngine = false
local writtenGui = false

-- ---------- tiny, self-contained helpers (no require: this probe REPORTS
-- ---------- whether require works, so it must not depend on it) --------------

local function getenv(name)
  local ok, v = pcall(function() return os and os.getenv and os.getenv(name) end)
  if ok and type(v) == "string" and #v > 0 then return v end
  return nil
end

local function outDir()
  local d = getenv(OUTDIR_ENV)
  if d then return d end
  local lad = getenv("LOCALAPPDATA")
  if lad then return lad .. "/tpf3mp/probe" end
  return "."  -- working directory (the game folder on TPF2)
end

-- Everything the probe emits goes through here; a report is a list of lines.
local out = {}
local function line(fmt, ...)
  if select("#", ...) == 0 then out[#out + 1] = fmt
  else out[#out + 1] = string.format(fmt, ...) end
end

local function sortedKeys(t)
  local keys = {}
  local ok = pcall(function()
    for k in pairs(t) do keys[#keys + 1] = k end
  end)
  if not ok then return nil end  -- sol2 usertypes refuse pairs(): signal "opaque"
  table.sort(keys, function(a, b) return tostring(a) < tostring(b) end)
  return keys
end

-- ---------- sandbox capabilities ---------------------------------------------

local function probeGlobal(name)
  local ok, v = pcall(function() return _G[name] end)
  if not ok then return "error" end
  return type(v)
end

local function probeLibFields(libName, fields)
  local lib = _G[libName]
  if type(lib) ~= "table" then
    line("  %s = %s (no fields enumerable)", libName, type(lib))
    return
  end
  local present = {}
  for _, f in ipairs(fields) do
    local ok, v = pcall(function() return lib[f] end)
    present[#present + 1] = string.format("%s:%s", f, ok and type(v) or "error")
  end
  line("  %s = table { %s }", libName, table.concat(present, ", "))
end

local function dumpSandbox()
  line("## Sandbox")
  line("_VERSION = %s", tostring(pcall(function() return _VERSION end) and _VERSION or "?"))
  line("globals: io=%s os=%s require=%s package=%s debug=%s load=%s loadstring=%s collectgarbage=%s",
    probeGlobal("io"), probeGlobal("os"), probeGlobal("require"), probeGlobal("package"),
    probeGlobal("debug"), probeGlobal("load"), probeGlobal("loadstring"), probeGlobal("collectgarbage"))
  -- 5.1 vs 5.2 markers: loadstring/unpack are 5.1; load(string)/table.unpack are 5.2+
  line("dialect markers: unpack=%s table.unpack=%s bit32=%s goto(compiles)=%s",
    probeGlobal("unpack"),
    tostring(pcall(function() return table.unpack end) and type(table.unpack) or "?"),
    probeGlobal("bit32"),
    tostring((function()
      local mk = load or loadstring
      if not mk then return "no-load" end
      local ok = pcall(mk, "::x:: goto x")
      return ok and "yes" or "no"
    end)()))
  probeLibFields("os", { "time", "clock", "date", "getenv", "setenv", "remove",
    "rename", "tmpname", "difftime", "execute", "exit", "setlocale" })
  probeLibFields("io", { "open", "lines", "read", "write", "close", "type",
    "input", "output", "popen", "tmpfile" })
  -- Where does require look, and does it work at all?
  if type(require) == "function" then
    local ok, err = pcall(require, "probe/does_not_exist_probe")
    line("require('missing') -> %s: %s", ok and "loaded?!" or "error(expected)",
      tostring(err):gsub("\n", " "):sub(1, 200))
  end
  if type(package) == "table" then
    local ok, p = pcall(function() return package.path end)
    line("package.path = %s", ok and tostring(p) or "n/a")
    ok, p = pcall(function() return package.cpath end)
    line("package.cpath = %s", ok and tostring(p) or "n/a")
  end
  line("")
end

-- ---------- number formatting & RNG (determinism inputs) ---------------------

local function dumpNumbers()
  line("## Number formatting")
  local function fmt(f, v) local ok, s = pcall(string.format, f, v); return ok and s or "err" end
  line("string.format('%%.17g', 0.1)      = %s", fmt("%.17g", 0.1))
  line("string.format('%%.17g', 1/3)      = %s", fmt("%.17g", 1 / 3))
  line("string.format('%%.17g', 2^53)     = %s", fmt("%.17g", 2 ^ 53))
  line("tie rounding %%.0f: 0.5=%s 1.5=%s 2.5=%s 3.5=%s (half-even => 0,2,2,4)",
    fmt("%.0f", 0.5), fmt("%.0f", 1.5), fmt("%.0f", 2.5), fmt("%.0f", 3.5))
  line("integer div behaviour: 7/2=%s  math.floor(7/2)=%s  5%%3=%s",
    fmt("%.17g", 7 / 2), tostring(math.floor(7 / 2)), tostring(5 % 3))
  line("")
  line("## math.random")
  line("NOTE: RNG state is per-VM. These document behaviour, not a shared stream.")
  local function seq(n)
    local t = {}
    for _ = 1, n do
      local ok, v = pcall(math.random)
      t[#t + 1] = ok and string.format("%.6f", v) or "err"
    end
    return table.concat(t, ", ")
  end
  line("math.random() x5 (unseeded)              = %s", seq(5))
  pcall(math.randomseed, 1)
  line("after randomseed(1): math.random() x5    = %s", seq(5))
  pcall(math.randomseed, 1)
  local function iseq(n, hi)
    local t = {}
    for _ = 1, n do
      local ok, v = pcall(math.random, hi)
      t[#t + 1] = ok and tostring(v) or "err"
    end
    return table.concat(t, ", ")
  end
  line("after randomseed(1): math.random(6) x10  = %s", iseq(10, 6))
  line("")
end

-- ---------- pairs() order for string keys ------------------------------------

local function dumpPairsOrder()
  line("## pairs() order for string keys")
  line("NOTE: Lua makes no ordering guarantee for pairs(); this records what THIS")
  line("build does. Lockstep code must never depend on it (sort keys instead).")
  local function orderOf()
    local t = {}
    -- insert in a fixed, non-alphabetical order
    for _, k in ipairs({ "zebra", "alpha", "mike", "bravo", "yankee", "charlie",
                         "november", "delta", "oscar", "echo" }) do
      t[k] = true
    end
    local seen = {}
    for k in pairs(t) do seen[#seen + 1] = k end
    return table.concat(seen, ",")
  end
  local first = orderOf()
  line("run 1: %s", first)
  local same = true
  for _ = 1, 4 do if orderOf() ~= first then same = false end end
  line("stable across 5 constructions in this state: %s", tostring(same))
  line("")
end

-- ---------- bounded-depth api.* / game.interface.* tree ----------------------

local MAX_DEPTH = 3
local MAX_BREADTH = 400

local function typeTag(v)
  local t = type(v)
  if t == "function" then return "fn" end
  if t == "userdata" then return "ud" end
  if t == "boolean" then return "bool=" .. tostring(v) end
  if t == "number" then return "num=" .. tostring(v) end
  if t == "string" then return "str" end
  return t
end

local function walk(name, value, depth)
  local tag = typeTag(value)
  if type(value) ~= "table" or depth >= MAX_DEPTH then
    line("%s%s : %s", string.rep("  ", depth), name, tag)
    return
  end
  local keys = sortedKeys(value)
  if keys == nil then
    -- opaque (sol2 usertype: pairs() refused). Record and stop.
    line("%s%s : table(opaque, pairs refused)", string.rep("  ", depth), name)
    return
  end
  line("%s%s : table(%d keys)", string.rep("  ", depth), name, #keys)
  local shown = 0
  for _, k in ipairs(keys) do
    shown = shown + 1
    if shown > MAX_BREADTH then
      line("%s... (%d more keys)", string.rep("  ", depth + 1), #keys - MAX_BREADTH)
      break
    end
    local ok, child = pcall(function() return value[k] end)
    if ok then
      walk(tostring(k), child, depth + 1)
    else
      line("%s%s : <error reading>", string.rep("  ", depth + 1), tostring(k))
    end
  end
end

local function dumpApiTrees()
  line("## api.* tree (depth %d)", MAX_DEPTH)
  if type(api) == "table" or type(api) == "userdata" then
    walk("api", api, 0)
  else
    line("api is %s (absent)", type(api))
  end
  line("")
  line("## game.interface.* tree (depth %d)", MAX_DEPTH)
  local gi = nil
  pcall(function() gi = game and game.interface end)
  if gi ~= nil then
    walk("game.interface", gi, 0)
  else
    line("game.interface is absent")
  end
  line("")
  -- api.cmd.make.* factory list, called out explicitly (the command surface).
  line("## api.cmd.make.* factories")
  local make = nil
  pcall(function() make = api and api.cmd and api.cmd.make end)
  local keys = make and sortedKeys(make) or nil
  if keys then
    line("count=%d", #keys)
    for _, k in ipairs(keys) do
      local ok, v = pcall(function() return make[k] end)
      line("  %s : %s", tostring(k), ok and typeTag(v) or "error")
    end
  else
    line("api.cmd.make is %s", make ~= nil and "opaque/uncounted" or "absent")
  end
  line("")
end

-- ---------- report driver ----------------------------------------------------

local function writeReport(stateName)
  out = {}
  line("# TPF3-MP script_api_dump -- state: %s", stateName)
  line("# Read-only probe. Written once per Lua state. All values measured at runtime.")
  line("")
  pcall(dumpSandbox)
  pcall(dumpNumbers)
  pcall(dumpPairsOrder)
  pcall(dumpPairsOrder)  -- twice: the second run's "stable across" also cross-checks
  pcall(dumpApiTrees)
  local dir = outDir()
  local path = dir .. "/script_api_dump_" .. stateName .. ".txt"
  local ok, f = pcall(io.open, path, "w")
  if ok and f then
    f:write(table.concat(out, "\n"))
    f:write("\n")
    f:close()
    pcall(print, "[script_api_dump] wrote " .. path)
  else
    -- Fall back to stdout (buffered until exit on TPF2, but better than nothing).
    pcall(print, "[script_api_dump] could not open " .. path .. "; dumping to stdout:")
    pcall(print, table.concat(out, "\n"))
  end
end

function data()
  return {
    update = function()
      if writtenEngine then return end
      writtenEngine = true
      pcall(writeReport, "engine")
    end,
    guiUpdate = function()
      if writtenGui then return end
      writtenGui = true
      pcall(writeReport, "gui")
    end,
  }
end
