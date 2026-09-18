local util = require "tpf2_mp/util"
local hash = require "tpf2_mp/hash"
local graph = require "tpf2_mp/transport_network_graph"

local M = {}

local function endpointFacts(service)
  local facts = (service.metadata or {}).cargoEndpointFacts
  return type(facts) == "table" and facts or {}
end

local function sourcesAndSinks(edges)
  local sources, sinks, seen = {}, {}, {}
  for _, edge in ipairs(edges) do
    for _, endpoint in ipairs(endpointFacts(edge.service)) do
      if endpoint.stationGroupCid == edge.fromStationGroupCid then
        for _, source in ipairs(endpoint.sources or {}) do
          local key = table.concat({ edge.fromStationGroupCid,
            source.industryCid, source.cargoType }, "|")
          if not seen["s:" .. key] then
            sources[#sources + 1] = util.deepCopy(source)
            sources[#sources].stationGroupCid = edge.fromStationGroupCid
            seen["s:" .. key] = true
          end
        end
        for _, sink in ipairs(endpoint.destinations or {}) do
          local key = table.concat({ edge.fromStationGroupCid,
            sink.industryCid, sink.cargoType, tostring(sink.stockIndex) }, "|")
          if not seen["d:" .. key] then
            sinks[#sinks + 1] = util.deepCopy(sink)
            sinks[#sinks].stationGroupCid = edge.fromStationGroupCid
            seen["d:" .. key] = true
          end
        end
      end
    end
  end
  graph.sortedRows(sources, function(value)
    return table.concat({ value.cargoType, value.industryCid, value.stationGroupCid }, "|")
  end)
  graph.sortedRows(sinks, function(value)
    return table.concat({ value.cargoType, value.industryCid,
      tostring(value.stockIndex), value.stationGroupCid }, "|")
  end)
  return sources, sinks
end

local function reset(economyState)
  for _, lineCid in ipairs(util.sortedKeys(economyState.services or {})) do
    local service, metadata = economyState.services[lineCid],
      economyState.services[lineCid].metadata or {}
    if metadata.freightNetworkSchema == 1 then
      service.capacity, service.transfers = 0, 0
      for _, key in ipairs({ "freightContractSchema", "freightContractDigest",
          "freightPathDigest", "freightLegIndex", "freightLegCount",
          "sourceIndustryCid", "destinationIndustryCid", "destinationStockIndex",
          "cargoType", "sourceStationGroupCid", "destinationStationGroupCid",
          "sourceStopIndex", "destinationStopIndex", "sourceTransportKind",
          "destinationTransportKind", "networkOriginRoute" }) do metadata[key] = nil end
      metadata.networkStatus = metadata.freightPinnedPathDigest
        and "pinned-path-unavailable" or "awaiting-compatible-path"
      service.metadata = metadata
      local market = economyState.markets and economyState.markets[service.marketCid]
      if market then
        market.demand, market.metadata = 0, market.metadata or {}
        market.metadata.networkStatus, market.metadata.routeDigest = metadata.networkStatus, nil
      end
    end
  end
end

local function pathIdentity(path, schemaVersion)
  local lines, stations = {}, { path.edges[1].fromStationGroupCid }
  local segments = {}
  for _, edge in ipairs(path.edges) do
    lines[#lines + 1], stations[#stations + 1] = edge.lineCid, edge.toStationGroupCid
    segments[#segments + 1] = { edge.fromStopIndex, edge.toStopIndex }
  end
  local digest = hash.value({ schemaVersion = schemaVersion, kind = "cargo",
    sourceIndustryCid = path.sourceCid, destinationIndustryCid = path.destinationCid,
    destinationStockIndex = path.sink.stockIndex, cargoType = path.cargoType,
    lines = lines, stations = stations, segments = segments })
  return digest, lines, stations, segments
end

local function pinnedPathAllows(path, digest)
  for _, edge in ipairs(path.edges) do
    local pinned = edge.service.metadata and edge.service.metadata.freightPinnedPathDigest
    if pinned ~= nil and pinned ~= digest then return false end
  end
  return true
end

local function candidates(edges, byNode, schemaVersion)
  local sources, sinks = sourcesAndSinks(edges)
  local sinksByCargoAndNode, result = {}, {}
  for _, sink in ipairs(sinks) do
    graph.append(sinksByCargoAndNode, sink.cargoType .. "\0" .. sink.stationGroupCid, sink)
  end
  for _, source in ipairs(sources) do
    local paths = graph.enumeratePaths(byNode, source.stationGroupCid, function(node)
      for _, sink in ipairs(sinksByCargoAndNode[source.cargoType .. "\0" .. node] or {}) do
        if sink.industryCid ~= source.industryCid then return sink end
      end
    end, source.cargoType, graph.MAX_LEGS)
    for _, path in ipairs(paths) do
      local sink = path.accepted
      path.kind, path.sourceCid, path.destinationCid = "cargo", source.industryCid, sink.industryCid
      path.cargoType, path.source, path.sink = source.cargoType, source, sink
      path.demand = math.min(graph.integer(source.ratePerHour, 0, 1, 1000000000),
        graph.integer(sink.ratePerHour, 0, 1, 1000000000))
      path.digest, path.lines, path.stations, path.segments = pathIdentity(path, schemaVersion)
      if path.capacity > 0 and pinnedPathAllows(path, path.digest) then
        result[#result + 1] = path
      end
    end
  end
  return graph.sortedRows(result, function(path)
    return string.format("%012d|%s", path.costSeconds, graph.routeKey(path))
  end)
end

function M.rebuild(economyState, schemaVersion)
  reset(economyState)
  local edges, byNode = graph.directedEdges(economyState, "cargo")
  local choices, claimed, routes = candidates(edges, byNode, schemaVersion), {}, {}
  for _, path in ipairs(choices) do
    local available = true
    for _, edge in ipairs(path.edges) do
      if claimed[edge.lineCid] then available = false; break end
    end
    if available then
      local digest, lines, stations = path.digest, path.lines, path.stations
      local route = { schemaVersion = schemaVersion, kind = "cargo", digest = digest,
        sourceIndustryCid = path.sourceCid, destinationIndustryCid = path.destinationCid,
        destinationStockIndex = path.sink.stockIndex, cargoType = path.cargoType,
        transfers = #path.edges - 1, demand = path.demand, capacity = path.capacity,
        costSeconds = path.costSeconds, lines = lines, stations = stations,
        segments = path.segments }
      for index, edge in ipairs(path.edges) do
        claimed[edge.lineCid] = true
        local service, metadata = edge.service, edge.service.metadata
        service.capacity = math.min(path.capacity, graph.edgeCapacity(service, path.cargoType))
        service.transfers = index == 1 and route.transfers or 0
        metadata.freightContractSchema = 2
        metadata.freightContractDigest = hash.value({ pathDigest = digest,
          legIndex = index - 1, lineCid = edge.lineCid,
          from = edge.fromStationGroupCid, to = edge.toStationGroupCid,
          fromStopIndex = edge.fromStopIndex, toStopIndex = edge.toStopIndex })
        metadata.freightPathDigest, metadata.freightLegIndex = digest, index - 1
        metadata.freightLegCount = #path.edges
        metadata.sourceIndustryCid, metadata.destinationIndustryCid = path.sourceCid, path.destinationCid
        metadata.destinationStockIndex, metadata.cargoType = path.sink.stockIndex, path.cargoType
        metadata.sourceStationGroupCid = edge.fromStationGroupCid
        metadata.destinationStationGroupCid = edge.toStationGroupCid
        metadata.sourceStopIndex, metadata.destinationStopIndex = edge.fromStopIndex, edge.toStopIndex
        metadata.sourceTransportKind = index == 1 and "industry" or "station"
        metadata.destinationTransportKind = index == #path.edges and "industry" or "station"
        metadata.networkStatus, metadata.contractAlternatives = "routed", #choices
        metadata.factsSource = #path.edges == 1 and "computed-direct-freight-contract"
          or "computed-multihop-freight-contract"
        if index == 1 then metadata.networkOriginRoute = util.deepCopy(route) end
        local market = economyState.markets[service.marketCid]
        market.demand, market.name = path.demand,
          tostring(path.cargoType) .. " leg " .. tostring(index) .. "/" .. tostring(#path.edges)
        market.metadata = { freightPathDigest = digest, freightLegIndex = index - 1,
          freightLegCount = #path.edges, sourceIndustryCid = path.sourceCid,
          destinationIndustryCid = path.destinationCid,
          destinationStockIndex = path.sink.stockIndex, cargoType = path.cargoType,
          corridorMeters = metadata.distanceMeters, networkStatus = "routed" }
      end
      routes[#routes + 1] = route
    end
  end
  local unrouted = 0
  for _, service in pairs(economyState.services or {}) do
    if service.metadata and service.metadata.freightNetworkSchema == 1
      and service.metadata.networkStatus ~= "routed" then unrouted = unrouted + 1 end
  end
  return { schemaVersion = schemaVersion, routes = routes,
    routeCount = #routes, unroutedLines = unrouted }
end

return M
