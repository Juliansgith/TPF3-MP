local util = require "tpf2_mp/util"
local hash = require "tpf2_mp/hash"
local graph = require "tpf2_mp/transport_network_graph"

local M = {}

local function reset(economyState)
  for _, marketCid in ipairs(util.sortedKeys(economyState.markets or {})) do
    local market = economyState.markets[marketCid]
    if market.kind ~= "cargo" and type(market.metadata) == "table"
      and type(market.metadata.townA) == "string"
      and type(market.metadata.townB) == "string" then
      local prior = graph.integer(market.metadata.networkDemand, 0, 0, 1000000000)
      local observedDirect = math.max(0,
        graph.integer(market.demand, 0, 0, 1000000000) - prior)
      market.metadata.directDemand = math.max(graph.integer(
        market.metadata.directDemand, observedDirect, 0, 1000000000), observedDirect)
      market.metadata.networkDemand, market.metadata.networkRouteCount = 0, 0
      market.demand = market.metadata.directDemand
    end
  end
  for _, service in pairs(economyState.services or {}) do
    local metadata = service.metadata or {}
    local market = economyState.markets and economyState.markets[service.marketCid]
    if market and market.kind ~= "cargo"
      and type(metadata.endpointTownCids) == "table"
      and #metadata.endpointTownCids == 2 then
      metadata.networkPathDigests, metadata.networkOriginRoutes = nil, nil
      metadata.networkPathCount, metadata.networkMaxTransfers = 0, 0
      service.metadata = metadata
    end
  end
end

local function demand(economyState, firstTown, secondTown, path)
  local towns = economyState.towns or {}
  local first = graph.integer(towns[firstTown] and towns[firstTown].size, 200, 1, 100000)
  local second = graph.integer(towns[secondTown] and towns[secondTown].size, 200, 1, 100000)
  local distance = 0
  for _, edge in ipairs(path.edges) do distance = distance + edge.distanceMeters end
  local kilometres = math.max(1, math.floor(distance / 1000))
  local transfers = math.max(1, #path.edges - 1)
  return util.clamp(math.floor(first * second
    / (50 * kilometres * (transfers + 1))), 10, 100000)
end

function M.rebuild(economyState, schemaVersion)
  reset(economyState)
  local edges, byNode = graph.directedEdges(economyState, "passenger")
  local best, origins = {}, {}
  for _, edge in ipairs(edges) do if type(edge.fromTownCid) == "string" then
    origins[edge.fromTownCid .. "\0" .. edge.fromStationGroupCid] = edge
  end end
  for _, originKey in ipairs(util.sortedKeys(origins)) do
    local source = origins[originKey]
    local paths = graph.enumeratePaths(byNode, source.fromStationGroupCid,
      function(_, path)
        local last = path[#path]
        if #path >= 2 and last and type(last.toTownCid) == "string"
          and last.toTownCid ~= source.fromTownCid then
          return { destinationTownCid = last.toTownCid }
        end
      end, nil, graph.MAX_LEGS)
    for _, path in ipairs(paths) do
      path.kind, path.sourceCid = "passenger", source.fromTownCid
      path.destinationCid = path.accepted.destinationTownCid
      local first, second = path.sourceCid, path.destinationCid
      if second < first then first, second = second, first end
      local key = first .. "|" .. second
      local candidateKey = string.format("%012d|%s", path.costSeconds, graph.routeKey(path))
      if not best[key] or candidateKey < best[key].sortKey then
        path.sortKey, best[key] = candidateKey, path
      end
    end
  end
  local routes = {}
  for _, key in ipairs(util.sortedKeys(best)) do
    local path = best[key]
    local routeDemand = demand(economyState, path.sourceCid, path.destinationCid, path)
    local routeLines, routeStations = {}, { path.edges[1].fromStationGroupCid }
    for _, edge in ipairs(path.edges) do
      routeLines[#routeLines + 1] = edge.lineCid
      routeStations[#routeStations + 1] = edge.toStationGroupCid
    end
    local segments = {}
    for _, edge in ipairs(path.edges) do
      segments[#segments + 1] = { edge.fromStopIndex, edge.toStopIndex }
    end
    local digest = hash.value({ schemaVersion = schemaVersion, kind = "passenger",
      sourceTownCid = path.sourceCid, destinationTownCid = path.destinationCid,
      lines = routeLines, segments = segments })
    local route = { schemaVersion = schemaVersion, kind = "passenger", digest = digest,
      sourceTownCid = path.sourceCid, destinationTownCid = path.destinationCid,
      transfers = #path.edges - 1, demand = routeDemand, costSeconds = path.costSeconds,
      lines = routeLines, stations = routeStations, segments = segments }
    for index, edge in ipairs(path.edges) do
      local market, metadata = economyState.markets[edge.marketCid], edge.service.metadata
      market.metadata.networkDemand = math.min(1000000000,
        graph.integer(market.metadata.networkDemand, 0, 0, 1000000000) + routeDemand)
      market.metadata.networkRouteCount = graph.integer(
        market.metadata.networkRouteCount, 0, 0, 1000000000) + 1
      market.demand = math.min(1000000000,
        graph.integer(market.metadata.directDemand, market.demand, 0, 1000000000)
          + market.metadata.networkDemand)
      metadata.networkPathDigests = metadata.networkPathDigests or {}
      metadata.networkPathDigests[#metadata.networkPathDigests + 1] = digest
      metadata.networkPathCount = graph.integer(metadata.networkPathCount, 0, 0, 1000000000) + 1
      metadata.networkMaxTransfers = math.max(
        graph.integer(metadata.networkMaxTransfers, 0, 0, 8), route.transfers)
      if index == 1 then
        metadata.networkOriginRoutes = metadata.networkOriginRoutes or {}
        metadata.networkOriginRoutes[#metadata.networkOriginRoutes + 1] = util.deepCopy(route)
      end
    end
    routes[#routes + 1] = route
  end
  return { schemaVersion = schemaVersion, routes = routes, routeCount = #routes }
end

return M
