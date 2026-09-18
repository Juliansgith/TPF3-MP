local util = require "tpf2_mp/util"

local M = {}

M.CENTS_PER_ENDPOINT = 150

local function metadata(value)
  return type(value) == "table" and value or {}
end

local function scope(market, service)
  local serviceMetadata = metadata(service.metadata)
  local marketMetadata = metadata(market.metadata)
  return serviceMetadata.marketScope or marketMetadata.marketScope
end

local function endpointTowns(market, service)
  local serviceMetadata = metadata(service.metadata)
  local towns = serviceMetadata.endpointTownCids
  if type(towns) == "table" then return towns end
  local marketMetadata = metadata(market.metadata)
  return { marketMetadata.townA, marketMetadata.townB }
end

local function add(index, companyCid, townCid, stationGroupCid, cents)
  if type(companyCid) ~= "string" or type(townCid) ~= "string"
      or type(stationGroupCid) ~= "string" or cents <= 0 then return end
  index[companyCid] = index[companyCid] or {}
  index[companyCid][townCid] = index[companyCid][townCid] or {}
  local previous = index[companyCid][townCid][stationGroupCid] or 0
  index[companyCid][townCid][stationGroupCid] = math.max(previous, cents)
end

-- A feeder is intentionally a derived fact, not authored state. Rebuilding
-- this index at settlement means assignment, disablement, and capacity
-- changes take effect without another ordered action or a stale cache.
function M.buildIndex(state)
  local index = {}
  for _, lineCid in ipairs(util.sortedKeys(state.services or {})) do
    local service = state.services[lineCid]
    local market = state.markets and state.markets[service.marketCid]
    local serviceMetadata = metadata(service.metadata)
    local carrier = serviceMetadata.carrier
    if market and market.kind ~= "cargo" and scope(market, service) == "local"
        and service.enabled ~= false and (tonumber(service.capacity) or 0) > 0
        and (carrier == "ROAD" or carrier == "TRAM") then
      local towns = endpointTowns(market, service)
      local townCid = towns[1]
      local groups, distinct, groupCount = serviceMetadata.stationGroupCids or {}, {}, 0
      for _, groupCid in ipairs(groups) do
        if type(groupCid) == "string" and not distinct[groupCid] then
          distinct[groupCid], groupCount = true, groupCount + 1
        end
      end
      -- The weaker of frequency and hourly capacity controls access quality.
      -- Multiple feeders at one station use the best service; they never stack.
      local frequencyCents = math.floor(90000 / math.max(1,
        util.integer(service.headwaySeconds, 86400)))
      local accessCents = math.min(M.CENTS_PER_ENDPOINT,
        math.max(0, util.integer(service.capacity, 0)), frequencyCents)
      if groupCount >= 2 then
        for groupCid in pairs(distinct) do
          add(index, service.companyCid, townCid, groupCid, accessCents)
        end
      end
    end
  end
  return index
end

function M.cents(market, service, index)
  if market.kind == "cargo" or scope(market, service) ~= "corridor" then return 0, 0 end
  local serviceMetadata = metadata(service.metadata)
  local groups = serviceMetadata.stationGroupCids or {}
  if #groups < 2 then return 0, 0 end
  local towns = endpointTowns(market, service)
  local company = index[service.companyCid] or {}
  local count, total, seen = 0, 0, {}
  for _, endpoint in ipairs({ 1, #groups }) do
    local townCid, groupCid = towns[endpoint == 1 and 1 or 2], groups[endpoint]
    local key = tostring(townCid) .. "\0" .. tostring(groupCid)
    local accessCents = company[townCid] and company[townCid][groupCid] or 0
    if not seen[key] and accessCents > 0 then
      seen[key], count = true, count + 1
      total = total + accessCents
    end
  end
  return total, count
end

return M
