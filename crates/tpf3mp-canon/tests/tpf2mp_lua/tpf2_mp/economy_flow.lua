local util = require "tpf2_mp/util"
local costs = require "tpf2_mp/economy_costs"
local revenue = require "tpf2_mp/economy_revenue"
local difficulty = require "tpf2_mp/economy_difficulty"
local deliverySnapshot = require "tpf2_mp/delivery_snapshot"
local feederAccess = require "tpf2_mp/economy_feeder_access"
local allocation = require "tpf2_mp/economy_allocation"

local M = {}
local SHARE_SCALE = 1000000
local ACCUMULATOR_LIMIT = revenue.ACCUMULATOR_LIMIT

local EXP_TABLE = {
  65536, 59299, 53656, 48550, 43930, 39750, 35967, 32544, 29447,
  26645, 24109, 21815, 19739, 17861, 16161, 14623, 13231, 11972,
  10833, 9802, 8869, 8025, 7262, 6571, 5945, 5380, 4868,
  4404, 3985, 3606, 3263, 2952, 2671, 2417, 2187, 1979,
  1791, 1620, 1466, 1327, 1200, 1086, 983, 889, 805,
  728, 659, 596, 539, 488, 442, 400, 362, 327,
  296, 268, 242, 219, 198, 180, 162, 147, 133,
  120, 109, 99, 89, 81, 73, 66, 60, 54,
  49, 44, 40, 36, 33, 30, 27, 24, 22,
}

local function integer(value, fallback, low, high)
  local result = util.integer(value, fallback)
  if low then result = math.max(low, result) end
  if high then result = math.min(high, result) end
  return result
end

local function signedAdd(left, right)
  return util.clamp((tonumber(left) or 0) + (tonumber(right) or 0),
    -ACCUMULATOR_LIMIT, ACCUMULATOR_LIMIT)
end

function M.generalizedCost(params, market, service, feederAccessCents)
  local vot = market.votCentsPerHour
  local waitWeightPm = market.waitWeightPm
  if waitWeightPm == nil then waitWeightPm = 2000 end
  local transferSeconds = market.transferSeconds
  if transferSeconds == nil then transferSeconds = params.transferSeconds end
  local waitSeconds = math.min(math.floor(service.headwaySeconds / 2), params.maxWaitSeconds)
  local timeCostCents = math.floor(vot * service.journeySeconds / 3600)
  local waitCostCents = math.floor(vot * waitSeconds * waitWeightPm / 3600000)
  local transferCostCents = math.floor(vot * service.transfers * transferSeconds / 3600)
  local crowdSpan = SHARE_SCALE - params.crowdThresholdPpm
  local crowdExcess = util.clamp((service.lagLoadPpm or 0)
    - params.crowdThresholdPpm, 0, crowdSpan)
  local crowdCostCents = math.floor(timeCostCents * crowdExcess / crowdSpan)
  local accessModel = feederAccessCents ~= nil
  local baseComfortCents = service.quality
  feederAccessCents = math.max(0, util.integer(feederAccessCents, 0))
  local comfortCents = baseComfortCents + feederAccessCents
  local gcCents = math.max(1, service.fareCents + timeCostCents + waitCostCents
    + transferCostCents + crowdCostCents - comfortCents)
  local factors = {
    fareCents = service.fareCents, timeCostCents = timeCostCents,
    waitCostCents = waitCostCents, transferCostCents = transferCostCents,
    crowdCostCents = crowdCostCents,
    comfortCents = comfortCents, gcCents = gcCents,
  }
  if accessModel then
    factors.baseComfortCents = baseComfortCents
    factors.feederAccessCents = feederAccessCents
  end
  return gcCents, factors
end

local function logitWeight(gcCents, gcMinCents, thetaCents, cutoffWeight)
  local centinats = math.floor((gcCents - gcMinCents) * 100 / thetaCents)
  if centinats >= 800 then return cutoffWeight end
  if centinats < 0 then centinats = 0 end
  local index, fraction = math.floor(centinats / 10), centinats % 10
  local left, right = EXP_TABLE[index + 1], EXP_TABLE[index + 2]
  return math.max(cutoffWeight, left + math.floor((right - left) * fraction / 10))
end

function M.logitWeight(gcCents, gcMinCents, thetaCents)
  return logitWeight(gcCents, gcMinCents, thetaCents, 0)
end

local function scaledRate(hourly, residual, periodSeconds)
  local numerator = math.max(0, util.integer(hourly, 0)) * periodSeconds
    + math.max(0, util.integer(residual, 0))
  return math.floor(numerator / 3600), numerator % 3600
end

local function glide(actual, equilibrium, alphaPm, residual)
  local delta = (equilibrium - actual) * alphaPm + residual
  local step = math.floor(delta / 1000)
  return actual + step, delta - step * 1000
end

local function delivery(state, snapshot, market, service, allocated)
  if snapshot == nil then
    return allocated, revenue.modelDeliveryCents(market, service, allocated)
  end
  state.deliveryCursors = state.deliveryCursors or {}
  local cargo = market.kind == "cargo"
  local prior = state.deliveryCursors[service.lineCid] or (cargo
    and { deliveredCargo = 0, earnedRevenueCents = 0 }
    or { deliveredPassengers = 0, earnedRevenueCents = 0 })
  local rows = cargo and deliverySnapshot.cargoLines(snapshot)
    or deliverySnapshot.passengerLines(snapshot)
  local row = rows[service.lineCid] or prior
  local deliveredField = cargo and "deliveredUnits" or "deliveredPassengers"
  local priorField = cargo and "deliveredCargo" or "deliveredPassengers"
  local delivered = integer(row[deliveredField], prior[priorField], 0, 1000000000)
  local earned = integer(row.earnedRevenueCents,
    prior.earnedRevenueCents, 0, ACCUMULATOR_LIMIT)
  if delivered < prior[priorField] or earned < prior.earnedRevenueCents then
    error((cargo and "cargo" or "passenger") .. " delivery snapshot moved backwards")
  end
  local cursor = { earnedRevenueCents = earned }
  cursor[priorField] = delivered
  state.deliveryCursors[service.lineCid] = cursor
  return delivered - prior[priorField], earned - prior.earnedRevenueCents
end

function M.evaluateMarket(state, marketCid, deliverySnapshot, periodSeconds)
  local market = state.markets[marketCid]
  if not market then return nil, "unknown market" end
  local version = util.integer(state.version, 1)
  local v6 = version >= 6
  local v9 = version >= 9
  periodSeconds = v6 and integer(periodSeconds, 300, 60, 86400) or 3600
  local demand, demandResid = market.demand, 0
  if v6 then demand, demandResid = scaledRate(
    market.demand, market.demandResid, periodSeconds) end
  if v6 then market.demandResid = demandResid end
  local feederIndex = util.integer(state.version, 1) >= 8
    and feederAccess.buildIndex(state) or nil
  local services = {}
  for _, lineCid in ipairs(util.sortedKeys(state.services)) do
    local service = state.services[lineCid]
    if service.enabled and service.marketCid == marketCid then
      local accessCents, accessEndpoints
      if feederIndex then
        accessCents, accessEndpoints = feederAccess.cents(market, service, feederIndex)
      end
      local gcCents, factors = M.generalizedCost(
        state.params, market, service, accessCents)
      if feederIndex then factors.feederAccessEndpoints = accessEndpoints end
      local capacity, capacityResid = service.capacity, 0
      if v6 then capacity, capacityResid = scaledRate(
        service.capacity, service.capacityResid, periodSeconds) end
      if v6 then service.capacityResid = capacityResid end
      services[#services + 1] = {
        service = service, cid = lineCid, gcCents = gcCents,
        factors = factors, availableCapacity = capacity,
      }
    end
  end
  local gcMin = market.gcOutsideCents
  for _, option in ipairs(services) do gcMin = math.min(gcMin, option.gcCents) end
  local cutoff = util.integer(state.version, 1) >= 3 and 0 or 1
  local weights = { { cid = "~outside",
    weight = logitWeight(market.gcOutsideCents, gcMin, market.thetaCents, cutoff) } }
  for _, option in ipairs(services) do
    option.weight = logitWeight(option.gcCents, gcMin, market.thetaCents, cutoff)
    weights[#weights + 1] = { cid = option.cid, weight = option.weight }
  end
  local equilibria, serviceShare = allocation.proportional(SHARE_SCALE, weights), 0
  for _, option in ipairs(services) do
    local service, equilibrium = option.service, equilibria[option.cid] or 0
    option.equilibriumPpm = equilibrium
    if service.sharePpm == nil then service.sharePpm, service.shareResid = equilibrium, 0
    else
      local shock = util.integer(state.version, 1) >= 3
        and (service.lastFareCents == nil or service.fareCents > service.lastFareCents)
      local alpha = equilibrium >= service.sharePpm and state.params.alphaUpPm
        or (shock and 1000 or state.params.alphaDownPm)
      service.sharePpm, service.shareResid = glide(
        service.sharePpm, equilibrium, alpha, service.shareResid or 0)
      service.sharePpm = util.clamp(service.sharePpm, 0, SHARE_SCALE)
    end
    if util.integer(state.version, 1) >= 3 then service.lastFareCents = service.fareCents end
    serviceShare = serviceShare + service.sharePpm
  end
  local outsidePpm = math.max(0, SHARE_SCALE - serviceShare)
  -- Capacity must not erase the distinction between people who chose a
  -- service and people it can actually carry. Version 9 preserves the first
  -- proportional choice before the capacity loop redistributes its overflow
  -- to the outside option. Passenger presentation consumes this requested
  -- flow as a real station arrival rate; money still requires a synchronized
  -- completed trip.
  local allocations, requestedAllocations, queued = allocation.capacityConstrained(
    demand, services, outsidePpm, version)
  local result = {
    marketCid = marketCid, name = market.name, kind = market.kind,
    demand = demand, hourlyDemand = v6 and market.demand or nil,
    intervalSeconds = v6 and periodSeconds or nil,
    gcOutsideCents = market.gcOutsideCents, thetaCents = market.thetaCents,
    outside = allocations["~outside"] or 0, outsidePpm = outsidePpm, services = {},
  }
  if v9 then
    result.requestedOutside = requestedAllocations["~outside"] or 0
    result.queued = queued
  end
  for _, option in ipairs(services) do
    local service, amount = option.service, allocations[option.cid] or 0
    local requested = v9 and (requestedAllocations[option.cid] or 0) or amount
    service.lagLoadPpm = option.availableCapacity > 0
      and math.floor(requested * SHARE_SCALE / option.availableCapacity) or SHARE_SCALE
    local delivered, rawGross = delivery(state, deliverySnapshot, market, service, amount)
    local gross = rawGross
    if util.integer(state.version, 1) >= 7 then
      gross, service.revenueMultiplierResid = difficulty.apply(
        rawGross, state.params.revenueMultiplierPpm,
        service.revenueMultiplierResid)
    end
    local managed = false
    for _, vehicleCid in ipairs(service.metadata and service.metadata.vehicleCids or {}) do
      if state.vehicleCosts and state.vehicleCosts[vehicleCid] then managed = true; break end
    end
    local annual = managed and 0 or service.annualVehicleUpkeepCents
    local charge, residual = costs.charge(annual, managed and 0 or service.upkeepResid,
      periodSeconds, util.integer(state.version, 1))
    if not managed then service.upkeepResid = residual end
    result.services[option.cid] = {
      lineCid = option.cid, companyCid = service.companyCid, name = service.name,
      allocated = amount, delivered = delivered, capacity = service.capacity,
      availableCapacity = option.availableCapacity, fareCents = service.fareCents,
      revenueCents = gross, grossRevenueCents = gross,
      rawGrossRevenueCents = util.integer(state.version, 1) >= 7 and rawGross or nil,
      revenueMultiplierPpm = util.integer(state.version, 1) >= 7
        and state.params.revenueMultiplierPpm or nil,
      revenueMultiplierResid = util.integer(state.version, 1) >= 7
        and service.revenueMultiplierResid or nil,
      annualVehicleUpkeepCents = annual, vehicleUpkeepCents = charge,
      operatingCostCents = charge, netRevenueCents = signedAdd(gross, -charge),
      upkeepResid = service.upkeepResid,
      shareBasisPoints = demand > 0 and math.floor(amount * 10000 / demand) or 0,
      sharePpm = service.sharePpm, shareResid = service.shareResid,
      equilibriumPpm = option.equilibriumPpm, lagLoadPpm = service.lagLoadPpm,
      factors = option.factors,
    }
    if v9 then
      result.services[option.cid].requested = requested
      result.services[option.cid].capacityOverflow = math.max(0, requested - amount)
    end
  end
  return result
end

return M
