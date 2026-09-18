local util = require "tpf2_mp/util"
local graph = require "tpf2_mp/transport_network_graph"
local passenger = require "tpf2_mp/multihop_passenger"
local cargo = require "tpf2_mp/multihop_cargo"
local freightPathPin = require "tpf2_mp/freight_path_pin"

local M = {
  SCHEMA_VERSION = 1,
  MAX_LEGS = graph.MAX_LEGS,
  TRANSFER_SECONDS = graph.TRANSFER_SECONDS,
  CARGO_TRANSFER_SECONDS = graph.CARGO_TRANSFER_SECONDS,
}

function M.rebuildPassenger(economyState)
  return passenger.rebuild(economyState, M.SCHEMA_VERSION)
end

function M.rebuildCargo(economyState)
  return cargo.rebuild(economyState, M.SCHEMA_VERSION)
end

function M.rebuild(economyState)
  return { schemaVersion = M.SCHEMA_VERSION,
    passenger = M.rebuildPassenger(economyState),
    cargo = M.rebuildCargo(economyState) }
end

M.pinCargoLine = freightPathPin.pinLine
M.pinMovedCargo = freightPathPin.pinMoved

local function displayName(registry, cid)
  local binding = registry and registry.byCanonical and registry.byCanonical[cid]
  local name = binding and binding.metadata and binding.metadata.name
  return type(name) == "string" and name ~= "" and name or cid
end

local function decorateRoute(route, registry)
  local copy = util.deepCopy(route)
  copy.sourceName = displayName(registry, copy.sourceTownCid or copy.sourceIndustryCid)
  copy.destinationName = displayName(registry,
    copy.destinationTownCid or copy.destinationIndustryCid)
  copy.stationNames = {}
  for _, cid in ipairs(copy.stations or {}) do
    copy.stationNames[#copy.stationNames + 1] = displayName(registry, cid)
  end
  return copy
end

function M.publicView(economyState, registry)
  local passengerRoutes, cargoRoutes, seenPassenger, seenCargo = {}, {}, {}, {}
  local unresolved = {}
  for _, lineCid in ipairs(util.sortedKeys(economyState.services or {})) do
    local service, metadata = economyState.services[lineCid],
      economyState.services[lineCid].metadata or {}
    for _, route in ipairs(metadata.networkOriginRoutes or {}) do
      if not seenPassenger[route.digest] then
        passengerRoutes[#passengerRoutes + 1] = decorateRoute(route, registry)
        seenPassenger[route.digest] = true
      end
    end
    local route = metadata.networkOriginRoute
    if type(route) == "table" and not seenCargo[route.digest] then
      cargoRoutes[#cargoRoutes + 1] = decorateRoute(route, registry)
      seenCargo[route.digest] = true
    end
    if metadata.freightNetworkSchema == 1 and metadata.networkStatus ~= "routed" then
      unresolved[#unresolved + 1] = { lineCid = lineCid,
        companyCid = service.companyCid, name = service.name,
        reason = metadata.networkStatus }
    end
  end
  return { schemaVersion = M.SCHEMA_VERSION,
    passengerRoutes = passengerRoutes, cargoRoutes = cargoRoutes,
    unresolvedCargoLines = unresolved,
    passengerRouteCount = #passengerRoutes, cargoRouteCount = #cargoRoutes,
    unresolvedCargoCount = #unresolved }
end

return M
