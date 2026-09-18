-- tools/probe/determinism_probe/res/config/game_script/determinism_probe.lua
--
-- Samples the docs/DAY_ONE.md section 4 determinism lanes every N engine steps
-- and appends one line per sample to determinism_probe_<instance>.log. Only the
-- authoritative engine state samples (update()); the GUI state is not defined.
--
-- Rules: pcall around every probe; no globals; deterministic digests (pure-Lua
-- hash, sorted keys, never hash-order iteration); "err" logged for a lane whose
-- API is absent rather than a guess.
--
-- ASSUMPTIONS (TPF2, re-checked on TPF3 by the very fact that the lanes either
-- produce a digest or log "err"):
--   * one update() per simulation step (game time also logged, so sampling can
--     be re-derived if TPF3's cadence differs);
--   * entity enumeration via game.interface.getEntities / api.engine.* mirrors
--     TPF2; several edge-enumeration strategies are tried and the one that
--     worked is recorded on the first sample;
--   * getGameTime().time is the shared simulation clock (advances 0.2/step on TPF2).

-- Pure-Lua stable hash. require() first (the module is the "small module" form);
-- fall back to an inline copy so the probe still runs if require is unavailable.
local hash
do
  local ok, mod = pcall(require, "probe/stablehash")
  if ok and type(mod) == "table" and mod.hashStr then
    hash = mod
  else
    local M1, A1 = 2147483647, 48271
    local M2, A2 = 2147483629, 40692
    hash = {
      hashStr = function(s)
        local h1, h2 = 2166136261 % M1, 2166136261 % M2
        for i = 1, #s do
          local b = string.byte(s, i)
          h1 = (h1 * A1 + b) % M1
          h2 = (h2 * A2 + b) % M2
        end
        return string.format("%010d-%010d", h1, h2)
      end,
    }
    hash.hashList = function(list) return hash.hashStr(table.concat(list, "\30")) end
  end
  if not hash.hashList then
    hash.hashList = function(list) return hash.hashStr(table.concat(list, "\30")) end
  end
end

local function getenv(name)
  local ok, v = pcall(function() return os and os.getenv and os.getenv(name) end)
  if ok and type(v) == "string" and #v > 0 then return v end
  return nil
end

local STRIDE = tonumber(getenv("TPF3MP_PROBE_STRIDE") or "") or 100
local INSTANCE = getenv("TPF3MP_PROBE_INSTANCE") or "a"
local OUTDIR = getenv("TPF3MP_PROBE_DIR")
  or (getenv("LOCALAPPDATA") and (getenv("LOCALAPPDATA") .. "/tpf3mp/probe"))
  or "."
local LOGPATH = OUTDIR .. "/determinism_probe_" .. INSTANCE .. ".log"

local BIG = { radius = 1e9 }  -- "the whole map" for game.interface.getEntities
local steps = 0
local headerWritten = false
local edgeStrategyUsed = nil

-- ---------- quantisation ----------
local function q1(v) return math.floor((v or 0) + 0.5) end               -- 1 m
local function q01(v) return math.floor((v or 0) * 10 + 0.5) / 10 end     -- 0.1 m

local function ct() return api and api.type and api.type.ComponentType or {} end
local function getComp(id, comp)
  if not comp then return nil end
  local ok, c = pcall(api.engine.getComponent, id, comp)
  return ok and c or nil
end
local function getEntities(kind, withData)
  local ok, t = pcall(function()
    return game.interface.getEntities(BIG, { type = kind, includeData = withData and true or false })
  end)
  if ok and type(t) == "table" then return t end
  return nil
end

-- ---------- lanes ----------
-- Each returns a digest string; on any failure the caller records "err".

local function laneVehicles()
  local t = getEntities("VEHICLE", true)
  if not t then return nil, nil end
  local n, pos = 0, {}
  for vid, e in pairs(t) do
    n = n + 1
    local p = type(e) == "table" and e.position or nil
    if p then
      pos[#pos + 1] = string.format("%d,%d,%d",
        q1(p[1] or p.x or 0), q1(p[2] or p.y or 0), q1(p[3] or p.z or 0))
    else
      local id = (type(e) == "table" and tonumber(e.id)) or tonumber(vid)
      local tv = id and getComp(id, ct().TRANSPORT_VEHICLE)
      -- vehicles in a depot have no map position; that is itself part of state
      pos[#pos + 1] = "nopos:" .. tostring(id or vid)
    end
  end
  table.sort(pos)
  return tostring(n), hash.hashList(pos)  -- count lane, positions lane (1 m)
end

-- Edge enumeration is not guaranteed under any one name; try several and record
-- which worked (prior art mp/hash.lua learned this the hard way).
local function collectEdges()
  local strategies = {
    { "gi:BASE_EDGE", function()
        local t = getEntities("BASE_EDGE", false); if not t then return nil end
        local o = {}; for _, e in pairs(t) do o[#o + 1] = tonumber(e) or e end; return o
      end },
    { "gi:EDGE", function()
        local t = getEntities("EDGE", false); if not t then return nil end
        local o = {}; for _, e in pairs(t) do o[#o + 1] = tonumber(e) or e end; return o
      end },
    { "node2segment", function()
        local ok, m = pcall(function() return api.engine.system.streetSystem.getNode2SegmentMap() end)
        if not ok or type(m) ~= "table" then return nil end
        local o = {}
        for _, segs in pairs(m) do
          if type(segs) == "table" then for _, s in pairs(segs) do o[#o + 1] = s end end
        end
        return o
      end },
  }
  for _, s in ipairs(strategies) do
    local ok, list = pcall(s[2])
    if ok and type(list) == "table" and #list > 0 then
      edgeStrategyUsed = s[1]
      return list
    end
  end
  return {}
end

local function nodePos(nid, cache)
  local s = cache[nid]
  if s then return s end
  local c = getComp(nid, ct().BASE_NODE)
  if c and c.position then
    local p = c.position
    s = string.format("%s,%s,%s", q01(p.x or p[1]), q01(p.y or p[2]), q01(p.z or p[3] or 0))
  else
    s = "?"
  end
  cache[nid] = s
  return s
end

local function laneEdges()
  local edges = collectEdges()
  if #edges == 0 then return nil end
  local cache, geo = {}, {}
  for _, eid in ipairs(edges) do
    local c = getComp(eid, ct().BASE_EDGE)
    if c and c.node0 ~= nil then
      local a, b = nodePos(c.node0, cache), nodePos(c.node1, cache)
      if a > b then a, b = b, a end  -- direction-independent
      geo[#geo + 1] = a .. ">" .. b
    end
  end
  table.sort(geo)
  return hash.hashList(geo)
end

local function laneConstructions()
  local t = getEntities("CONSTRUCTION", false)
  if not t then return nil end
  local cons = {}
  for _, cid in pairs(t) do
    local alive = true
    pcall(function() alive = api.engine.entityExists(cid) end)
    if alive then
      local co = getComp(cid, ct().CONSTRUCTION)
      local fn = co and co.fileName and tostring(co.fileName) or "?"
      local x, y = 0, 0
      if co and co.transf then x, y = co.transf[13] or 0, co.transf[14] or 0 end
      cons[#cons + 1] = string.format("%s@%s,%s", fn, q01(x), q01(y))
    end
  end
  table.sort(cons)
  return hash.hashList(cons)
end

local function listWith(comp)
  local out = {}
  if comp and api.engine and api.engine.forEachEntityWithComponent then
    pcall(function()
      api.engine.forEachEntityWithComponent(function(e) out[#out + 1] = tonumber(e) or e end, comp)
    end)
  end
  return out
end

local function laneTowns()
  -- town -> building count. Town ids are stable within a save on one instance.
  local towns = listWith(ct().TOWN)
  if #towns == 0 then
    local t = getEntities("TOWN", false)
    if t then for _, id in pairs(t) do towns[#towns + 1] = tonumber(id) or id end end
  end
  if #towns == 0 then return nil end
  local map = nil
  pcall(function() map = api.engine.system.townBuildingSystem.getTown2BuildingMap() end)
  local rows = {}
  for _, tid in ipairs(towns) do
    local count = 0
    if type(map) == "table" then
      local b = map[tid]
      if type(b) == "table" then for _ in pairs(b) do count = count + 1 end
      elseif type(b) == "userdata" then
        local ok, len = pcall(function() return #b end); count = ok and tonumber(len) or 0
      end
    end
    rows[#rows + 1] = string.format("%s:%d", tostring(tid), count)
  end
  table.sort(rows)
  return hash.hashList(rows)
end

local function laneMoney()
  -- balance per player, sorted by player id.
  local players = listWith(ct().PLAYER)
  if #players == 0 then
    local ok, p = pcall(function() return api.engine.util.getPlayer() end)
    if ok and p then players = { tonumber(p) or p } end
  end
  if #players == 0 then return nil end
  local rows = {}
  for _, pid in ipairs(players) do
    local bal = nil
    pcall(function()
      local e = game.interface.getEntity(pid)
      bal = e and e.balance or nil
    end)
    if bal == nil then
      local acc = getComp(pid, ct().ACCOUNT) or getComp(pid, ct().PLAYER)
      bal = acc and (acc.balance or acc.money) or nil
    end
    rows[#rows + 1] = string.format("%s:%s", tostring(pid), bal ~= nil and string.format("%d", bal) or "?")
  end
  table.sort(rows)
  return hash.hashList(rows)
end

local function lanePeople()
  local ok, n = pcall(function() return api.engine.system.simPersonSystem.getCount() end)
  if ok and type(n) == "number" then return tostring(math.floor(n)) end
  local t = getEntities("SIM_PERSON", false)
  if not t then return nil end
  local c = 0
  for _ in pairs(t) do c = c + 1 end
  return tostring(c)
end

local function gameTime()
  local t = nil
  pcall(function() t = game.interface.getGameTime().time end)
  return t
end

-- ---------- io ----------
local function append(path, text)
  local ok, f = pcall(io.open, path, "a")
  if ok and f then f:write(text); f:close(); return true end
  return false
end

local function sample()
  local vcount, vpos = laneVehicles()
  local function dig(fn) local ok, v = pcall(fn); return (ok and v) or "err" end
  local t = gameTime()
  if not headerWritten then
    headerWritten = true
    append(LOGPATH, string.format(
      "# determinism_probe instance=%s stride=%d lanes=v,p,e,c,t,m,n started\n", INSTANCE, STRIDE))
  end
  local line = string.format(
    "step=%d time=%s v=%s p=%s e=%s c=%s t=%s m=%s n=%s edgeStrategy=%s\n",
    steps,
    t and string.format("%.6f", t) or "?",
    vcount or "err",
    vpos or "err",
    dig(laneEdges),
    dig(laneConstructions),
    dig(laneTowns),
    dig(laneMoney),
    dig(lanePeople),
    tostring(edgeStrategyUsed or "?"))
  if not append(LOGPATH, line) then
    pcall(print, "[determinism_probe] cannot write " .. LOGPATH .. "; sample: " .. line)
  end
end

function data()
  return {
    update = function()
      steps = steps + 1
      -- sample at step 1 and every STRIDE steps thereafter
      if steps == 1 or (steps % STRIDE) == 0 then
        pcall(sample)
      end
    end,
  }
end
