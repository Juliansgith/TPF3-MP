local util = require "tpf2_mp/util"

local M = {}

M.SCHEMA_VERSION = 1
M.NOMINAL_CAPACITY_PER_BUILDING = 4
M.FALLBACK_TOWN_BUILDINGS = 50
M.GRAVITY_DIVISOR = 25
M.MIN_DEMAND = 50
M.MAX_DEMAND = 100000
M.MAX_TOWN_SIZE = 100000
-- Keep authored demographic growth aligned with visible development: the
-- physical policy spends 400 carried-passenger points per new building, and
-- one building represents four model-capacity units.
M.GROWTH_PASSENGERS_PER_BUILDING = 400

local function bounded(value, fallback, low, high)
  return util.clamp(util.integer(value, fallback), low, high)
end

function M.marketSizeFromBuildings(buildings)
  local count = tonumber(buildings)
  if not count or count <= 0 then count = M.FALLBACK_TOWN_BUILDINGS end
  return bounded(count * M.NOMINAL_CAPACITY_PER_BUILDING,
    M.FALLBACK_TOWN_BUILDINGS * M.NOMINAL_CAPACITY_PER_BUILDING,
    1, M.MAX_TOWN_SIZE)
end

function M.gravityDemand(sizeA, sizeB, distanceMeters)
  local first = bounded(sizeA, M.FALLBACK_TOWN_BUILDINGS
    * M.NOMINAL_CAPACITY_PER_BUILDING, 1, M.MAX_TOWN_SIZE)
  local second = bounded(sizeB, M.FALLBACK_TOWN_BUILDINGS
    * M.NOMINAL_CAPACITY_PER_BUILDING, 1, M.MAX_TOWN_SIZE)
  local km = math.max(1, math.floor(math.max(0,
    tonumber(distanceMeters) or 1000) / 1000))
  return util.clamp(math.floor((first * second) / (M.GRAVITY_DIVISOR * km)),
    M.MIN_DEMAND, M.MAX_DEMAND)
end

local function upsertTown(state, townCid, observedSize)
  if type(townCid) ~= "string" or townCid == "" then return nil end
  state.towns = state.towns or {}
  local record = state.towns[townCid]
  local size = bounded(observedSize,
    M.FALLBACK_TOWN_BUILDINGS * M.NOMINAL_CAPACITY_PER_BUILDING,
    1, M.MAX_TOWN_SIZE)
  if not record then
    record = {
      schemaVersion = M.SCHEMA_VERSION,
      cid = townCid,
      size = size,
      growthResid = 0,
      totalGrowth = 0,
    }
  else
    record.schemaVersion = M.SCHEMA_VERSION
    record.cid = townCid
    -- A later line registration may observe additional native buildings. It
    -- may raise the authored baseline, but a stale peer-local read can never
    -- shrink model population or erase earned growth.
    record.size = math.max(bounded(record.size, size, 1, M.MAX_TOWN_SIZE), size)
    record.growthResid = bounded(record.growthResid, 0, 0,
      M.GROWTH_PASSENGERS_PER_BUILDING - 1)
    record.totalGrowth = bounded(record.totalGrowth, 0, 0, M.MAX_TOWN_SIZE)
  end
  state.towns[townCid] = record
  return record
end

function M.observeMarket(state, market)
  local metadata = type(market) == "table" and market.metadata or nil
  if type(metadata) ~= "table" then return false end
  local townA, townB = metadata.townA, metadata.townB
  if type(townA) ~= "string" or type(townB) ~= "string" then return false end
  local first = upsertTown(state, townA, metadata.townSizeA)
  local second = upsertTown(state, townB, metadata.townSizeB)
  metadata.townSizeA, metadata.townSizeB = first.size, second.size
  return true
end

function M.refreshMarkets(state)
  local changes = {}
  for _, marketCid in ipairs(util.sortedKeys(state.markets or {})) do
    local market = state.markets[marketCid]
    local metadata = market.metadata or {}
    local first = state.towns and state.towns[metadata.townA] or nil
    local second = state.towns and state.towns[metadata.townB] or nil
    if first and second then
      local previous = bounded(market.demand, M.MIN_DEMAND, 0, 1000000000)
      local computed = M.gravityDemand(
        first.size, second.size, metadata.corridorMeters)
      -- Population growth and a newly discovered shorter route may enlarge a
      -- market. Re-registration or an incomplete legacy baseline may never
      -- silently destroy demand players already invested against.
      local priorNetwork = bounded(metadata.networkDemand, 0, 0, 1000000000)
      local priorDirect = bounded(metadata.directDemand,
        math.max(0, previous - priorNetwork), 0, 1000000000)
      local direct = math.max(priorDirect, computed)
      metadata.directDemand = direct
      local updated = math.min(1000000000, direct + priorNetwork)
      market.demand = updated
      metadata.townSizeA, metadata.townSizeB = first.size, second.size
      market.metadata = metadata
      if updated ~= previous then
        changes[marketCid] = { previousDemand = previous, demand = updated }
      end
    end
  end
  return changes
end

local function carriedByTown(state, results)
  local carried = {}
  for _, marketCid in ipairs(util.sortedKeys(results and results.markets or {})) do
    local result = results.markets[marketCid]
    local market = state.markets and state.markets[marketCid] or nil
    local metadata = market and market.metadata or {}
    if market and market.kind ~= "cargo"
      and type(metadata.townA) == "string" and type(metadata.townB) == "string" then
      local total = 0
      for _, lineCid in ipairs(util.sortedKeys(result.services or {})) do
        local row = result.services[lineCid]
        total = total + math.max(0, util.integer(row.delivered or row.allocated, 0))
      end
      local half = math.floor(total / 2)
      carried[metadata.townA] = (carried[metadata.townA] or 0) + half
      carried[metadata.townB] = (carried[metadata.townB] or 0) + total - half
    end
  end
  return carried
end

function M.advance(state, results)
  state.towns = state.towns or {}
  local carried = carriedByTown(state, results)
  local changes = {}
  for _, townCid in ipairs(util.sortedKeys(carried)) do
    local record = state.towns[townCid]
      or upsertTown(state, townCid, nil)
    local previous = record.size
    local numerator = math.max(0, util.integer(record.growthResid, 0))
      + math.max(0, util.integer(carried[townCid], 0))
        * M.NOMINAL_CAPACITY_PER_BUILDING
    local gain = math.floor(numerator / M.GROWTH_PASSENGERS_PER_BUILDING)
    record.growthResid = numerator % M.GROWTH_PASSENGERS_PER_BUILDING
    record.size = math.min(M.MAX_TOWN_SIZE, record.size + gain)
    record.totalGrowth = math.min(M.MAX_TOWN_SIZE,
      math.max(0, util.integer(record.totalGrowth, 0)) + record.size - previous)
    changes[townCid] = {
      carried = carried[townCid], previousSize = previous,
      size = record.size, gained = record.size - previous,
      growthResid = record.growthResid,
    }
  end
  return {
    schemaVersion = M.SCHEMA_VERSION,
    towns = changes,
    markets = M.refreshMarkets(state),
  }
end

function M.migrate(state)
  state.towns = type(state.towns) == "table" and state.towns or {}
  local prior = state.towns
  state.towns = {}
  for townCid, record in pairs(prior) do
    if type(record) ~= "table" then
      -- Drop malformed pre-release rows.
    else
      local observed = record.size
      local migrated = upsertTown(state, townCid, observed)
      migrated.growthResid = bounded(record.growthResid, 0, 0,
        M.GROWTH_PASSENGERS_PER_BUILDING - 1)
      migrated.totalGrowth = bounded(record.totalGrowth, 0, 0, M.MAX_TOWN_SIZE)
    end
  end
  for _, marketCid in ipairs(util.sortedKeys(state.markets or {})) do
    M.observeMarket(state, state.markets[marketCid])
  end
  return state.towns
end

return M
