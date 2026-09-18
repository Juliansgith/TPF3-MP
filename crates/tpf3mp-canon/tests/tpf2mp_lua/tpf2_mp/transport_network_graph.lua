local util = require "tpf2_mp/util"

local M = {
  MAX_LEGS = 4,
  TRANSFER_SECONDS = 480,
  CARGO_TRANSFER_SECONDS = 1800,
}

function M.integer(value, fallback, low, high)
  return util.clamp(util.integer(value, fallback), low, high)
end

function M.append(map, key, value)
  map[key] = map[key] or {}
  map[key][#map[key] + 1] = value
end

function M.sortedRows(rows, key)
  table.sort(rows, function(a, b) return key(a) < key(b) end)
  return rows
end

function M.routeKey(route)
  local lines = {}
  for _, edge in ipairs(route.edges or {}) do lines[#lines + 1] = edge.lineCid end
  return table.concat({ route.kind or "", route.sourceCid or "",
    route.destinationCid or "", route.cargoType or "", table.concat(lines, ">") }, "|")
end

local function edgeCost(service, segments, totalSegments)
  local journey = M.integer(service.journeySeconds, 3600, 30, 604800)
  return math.max(30, math.floor(journey * segments / totalSegments))
    + math.floor(M.integer(service.headwaySeconds, 1800, 30, 86400) / 2)
end

function M.edgeCapacity(service, cargoType)
  local metadata = service.metadata or {}
  if cargoType then
    return M.integer(metadata.cargoHourlyCapacityByType
      and metadata.cargoHourlyCapacityByType[cargoType], 0, 0, 1000000000)
  end
  return M.integer(service.capacity, 0, 0, 1000000000)
end

function M.directedEdges(economyState, kind)
  local edges, byNode = {}, {}
  for _, lineCid in ipairs(util.sortedKeys(economyState.services or {})) do
    local service = economyState.services[lineCid]
    local market = economyState.markets and economyState.markets[service.marketCid] or nil
    local metadata, stops = service.metadata or {}, service.metadata
      and service.metadata.stationGroupCids or {}
    local cargo = market and market.kind == "cargo"
    if service.enabled ~= false and #stops >= 2
      and ((kind == "cargo" and metadata.freightNetworkSchema == 1)
        or (kind == "passenger" and market and not cargo
          and type(metadata.endpointTownCids) == "table"
          and #metadata.endpointTownCids == 2)) then
      for fromIndex = 1, #stops do
        for toIndex = 1, #stops do if fromIndex ~= toIndex then
        local segments, totalSegments = math.abs(toIndex - fromIndex), #stops - 1
        local edge = {
          lineCid = lineCid, companyCid = service.companyCid,
          marketCid = service.marketCid, carrier = metadata.carrier,
          fromStationGroupCid = stops[fromIndex],
          toStationGroupCid = stops[toIndex],
          fromStopIndex = fromIndex - 1, toStopIndex = toIndex - 1,
          fromTownCid = kind == "passenger"
            and (fromIndex == 1 and metadata.endpointTownCids[1]
              or fromIndex == #stops and metadata.endpointTownCids[2] or nil) or nil,
          toTownCid = kind == "passenger"
            and (toIndex == 1 and metadata.endpointTownCids[1]
              or toIndex == #stops and metadata.endpointTownCids[2] or nil) or nil,
          costSeconds = edgeCost(service, segments, totalSegments),
          distanceMeters = math.floor(M.integer(
            metadata.distanceMeters, 0, 0, 1000000000) * segments / totalSegments),
          service = service,
        }
        edges[#edges + 1] = edge
        M.append(byNode, edge.fromStationGroupCid, edge)
        end end
      end
    end
  end
  for _, rows in pairs(byNode) do
    M.sortedRows(rows, function(value)
      return table.concat({ value.lineCid, value.toStationGroupCid,
        tostring(value.fromStopIndex), tostring(value.toStopIndex) }, "|")
    end)
  end
  return edges, byNode
end

function M.enumeratePaths(byNode, startNode, accept, cargoType, maxLegs)
  local result = {}
  local function visit(node, path, usedLines, visitedNodes, cost, capacity)
    if #path > 0 then
      local accepted = accept(node, path)
      if accepted then
        local edges = {}
        for index, edge in ipairs(path) do edges[index] = edge end
        result[#result + 1] = { edges = edges,
          accepted = util.deepCopy(accepted), costSeconds = cost, capacity = capacity }
      end
    end
    if #path >= maxLegs then return end
    for _, edge in ipairs(byNode[node] or {}) do
      if not usedLines[edge.lineCid] and not visitedNodes[edge.toStationGroupCid] then
        local available = M.edgeCapacity(edge.service, cargoType)
        if not cargoType or available > 0 then
          usedLines[edge.lineCid], visitedNodes[edge.toStationGroupCid] = true, true
          path[#path + 1] = edge
          local transfer = #path > 1 and (cargoType
            and M.CARGO_TRANSFER_SECONDS or M.TRANSFER_SECONDS) or 0
          visit(edge.toStationGroupCid, path, usedLines, visitedNodes,
            cost + edge.costSeconds + transfer,
            math.min(capacity, cargoType and available or M.edgeCapacity(edge.service)))
          path[#path] = nil
          usedLines[edge.lineCid], visitedNodes[edge.toStationGroupCid] = nil, nil
        end
      end
    end
  end
  visit(startNode, {}, {}, { [startNode] = true }, 0, 1000000000)
  return result
end

return M
