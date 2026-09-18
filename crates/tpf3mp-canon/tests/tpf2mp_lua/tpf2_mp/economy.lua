local util = require "tpf2_mp/util"
local hash = require "tpf2_mp/hash"
local costs = require "tpf2_mp/economy_costs"
local flow = require "tpf2_mp/economy_flow"
local revenue = require "tpf2_mp/economy_revenue"
local difficulty = require "tpf2_mp/economy_difficulty"
local townDemand = require "tpf2_mp/economy_town_demand"
local multihopNetwork = require "tpf2_mp/multihop_network"

local M = {}

M.VERSION = 10
M.EPOCH_SECONDS = 300
M.HOURS_PER_YEAR = costs.HOURS_PER_YEAR
M.FINANCIAL_YEAR_SECONDS = costs.FINANCIAL_YEAR_SECONDS
M.PASSENGER_COHORT_SCALE = revenue.PASSENGER_COHORT_SCALE
M.DIFFICULTIES = difficulty.PRESETS
M.DEFAULT_DIFFICULTY = difficulty.DEFAULT_KEY

-- Market kinds share one evaluator; only the cents weighting differs. Cargo
-- barely minds waiting (inventory, not a person on a platform), pays heavily
-- for transshipment, values time far below passengers, and competes against
-- trucking rather than staying home.
M.MARKET_KINDS = {
  passenger = {
    waitWeightPm = 2000,
    transferSeconds = 480,
    votCentsPerHour = 450,
    gcOutsideCents = 2500,
  },
  cargo = {
    waitWeightPm = 1000,
    transferSeconds = 1800,
    votCentsPerHour = 60,
    gcOutsideCents = 1800,
  },
}
M.SHARE_SCALE = 1000000
-- Lua 5.1 stores every number as an IEEE-754 double. Keep authored aggregate
-- values well below 2^53 so the Python replayer cannot retain precision that
-- the game has already lost. This is $10 trillion and unreachable in normal
-- play, but the public input clamps permit adversarial larger products.
M.ACCUMULATOR_LIMIT = 1000000000000000

local function copyInteger(value, fallback, low, high)
  local number = util.integer(value, fallback)
  if low then number = math.max(low, number) end
  if high then number = math.min(high, number) end
  return number
end

local function saturatingAdd(left, right)
  return math.min(M.ACCUMULATOR_LIMIT, math.max(0, left or 0) + math.max(0, right or 0))
end

local function saturatingMultiply(left, right)
  return math.min(M.ACCUMULATOR_LIMIT, math.max(0, left or 0) * math.max(0, right or 0))
end

local function signedAdd(left, right)
  return util.clamp((tonumber(left) or 0) + (tonumber(right) or 0),
    -M.ACCUMULATOR_LIMIT, M.ACCUMULATOR_LIMIT)
end

local function emptyResults()
  return {
    markets = {}, companies = {}, totalDemand = 0,
    totalRevenueCents = 0, totalGrossRevenueCents = 0,
    totalVehicleUpkeepCents = 0, totalInfrastructureUpkeepCents = 0,
    totalOperatingCostCents = 0, totalNetRevenueCents = 0,
  }
end

local function emptyLedger()
  return {
    settledEpochs = {}, companies = {}, settlementCount = 0, totalDemand = 0,
    totalRevenueCents = 0, totalGrossRevenueCents = 0,
    totalVehicleUpkeepCents = 0, totalInfrastructureUpkeepCents = 0,
    totalOperatingCostCents = 0, totalNetRevenueCents = 0,
  }
end

function M.defaultParams()
  return {
    alphaUpPm = 350,
    alphaDownPm = 500,
    maxWaitSeconds = 1800,
    transferSeconds = 480,
    crowdThresholdPpm = 700000,
    economyDifficulty = difficulty.DEFAULT_KEY,
    revenueMultiplierPpm = difficulty.multiplier(difficulty.DEFAULT_KEY),
  }
end

function M.newState()
  return {
    version = M.VERSION,
    epoch = 0,
    params = M.defaultParams(),
    markets = {},
    towns = {},
    services = {},
    companyCosts = {},
    vehicleCosts = {},
    deliveryCursors = {},
    payoutResidCents = {},
    scheduler = {
      schemaVersion = 2,
      automatic = true,
      epochSeconds = M.EPOCH_SECONDS,
      startGameTimeSeconds = nil,
      lastBoundaryGameTimeSeconds = nil,
      nextBoundaryGameTimeSeconds = nil,
    },
    lastResults = emptyResults(),
    ledger = emptyLedger(),
  }
end

-- A version-1 state carries additive weights and no share stocks. Markets get
-- the cents-denominated defaults; services keep their operating facts and
-- initialise their stocks lazily on the first evaluation.
function M.migrate(state)
  if type(state) ~= "table" then return M.newState() end
  local previousVersion = util.integer(state.version, 1)
  if previousVersion <= 1 then
    state.params = M.defaultParams()
    for _, market in pairs(state.markets or {}) do
      market.outsideWeight = nil
      market.votCentsPerHour = copyInteger(market.votCentsPerHour, 450, 30, 100000)
      market.gcOutsideCents = copyInteger(market.gcOutsideCents, 2500, 1, 100000000)
      market.thetaCents = copyInteger(market.thetaCents,
        math.max(200, math.floor(market.gcOutsideCents * 8 / 100)), 50, 1000000)
    end
    for _, service in pairs(state.services or {}) do
      service.transfers = copyInteger(service.transfers, 0, 0, 8)
      service.sharePpm = nil
      service.shareResid = 0
      service.lagLoadPpm = 0
    end
  end
  if previousVersion <= 2 then
    local defaults = M.defaultParams()
    state.params = state.params or {}
    for key, value in pairs(defaults) do
      if state.params[key] == nil then state.params[key] = value end
    end
    -- Version 2 did not record the fare used by the preceding settlement.
    -- Nil deliberately makes the first downward glide fail safe as a fare
    -- shock; the evaluator then records the current fare.
    for _, service in pairs(state.services or {}) do service.lastFareCents = nil end
  end
  -- Version 4: kind-weighted markets. Passenger-equivalent values keep every
  -- migrated market's generalized cost bit-identical to its version-3 result.
  for _, market in pairs(state.markets or {}) do
    market.kind = M.MARKET_KINDS[market.kind] and market.kind or "passenger"
    market.waitWeightPm = copyInteger(market.waitWeightPm, 2000, 0, 10000)
    market.transferSeconds = copyInteger(market.transferSeconds,
      util.integer(state.params and state.params.transferSeconds, 480), 0, 14400)
  end
  if previousVersion < 6 then
    state.params = state.params or M.defaultParams()
    state.params.alphaUpPm, state.params.alphaDownPm = 350, 500
    local serviceCounts = {}
    for _, service in pairs(state.services or {}) do
      serviceCounts[service.marketCid] = (serviceCounts[service.marketCid] or 0) + 1
    end
    for _, market in pairs(state.markets or {}) do market.demandResid = 0 end
    for _, service in pairs(state.services or {}) do
      service.capacityResid, service.upkeepResid = 0, 0
      if serviceCounts[service.marketCid] == 1 and util.integer(service.sharePpm, 0) == 0 then
        service.sharePpm, service.shareResid = nil, 0
      end
    end
    for _, value in pairs(state.companyCosts or {}) do value.upkeepResid = 0 end
    for _, value in pairs(state.vehicleCosts or {}) do value.upkeepResid = 0 end
  end
  if previousVersion < 7 then
    state.params = state.params or M.defaultParams()
    state.params.economyDifficulty = difficulty.DEFAULT_KEY
    state.params.revenueMultiplierPpm = difficulty.multiplier(difficulty.DEFAULT_KEY)
    for _, service in pairs(state.services or {}) do
      service.revenueMultiplierResid = 0
    end
  end
  -- Version 5 added authored hourly scheduling plus operating costs. Existing
  -- saves retain their earned shares and start with a zero cost basis until a
  -- newly ordered purchase/build supplies an authoritative finance delta.
  state.companyCosts = state.companyCosts or {}
  for companyCid, value in pairs(state.companyCosts) do
    if type(value) ~= "table" then value = {}; state.companyCosts[companyCid] = value end
    value.companyCid = tostring(value.companyCid or companyCid)
    value.infrastructureCapitalCents = copyInteger(
      value.infrastructureCapitalCents, 0, 0, M.ACCUMULATOR_LIMIT)
    value.annualInfrastructureUpkeepCents = costs.infrastructureAnnualUpkeepCents(
      value.infrastructureCapitalCents)
    value.upkeepResid = copyInteger(value.upkeepResid, 0, 0,
      costs.FINANCIAL_YEAR_SECONDS - 1)
  end
  state.vehicleCosts = state.vehicleCosts or {}
  for vehicleCid, value in pairs(state.vehicleCosts) do
    if type(value) ~= "table" then value = {}; state.vehicleCosts[vehicleCid] = value end
    value.vehicleCid = tostring(value.vehicleCid or vehicleCid)
    value.companyCid = tostring(value.companyCid or "")
    value.annualVehicleUpkeepCents = copyInteger(
      value.annualVehicleUpkeepCents, 0, 0, M.ACCUMULATOR_LIMIT)
    value.upkeepResid = copyInteger(value.upkeepResid, 0, 0,
      costs.FINANCIAL_YEAR_SECONDS - 1)
  end
  state.payoutResidCents = state.payoutResidCents or {}
  for companyCid, value in pairs(state.payoutResidCents) do
    state.payoutResidCents[companyCid] = copyInteger(value, 0, -99, 99)
  end
  for _, service in pairs(state.services or {}) do
    service.annualVehicleUpkeepCents = copyInteger(
      service.annualVehicleUpkeepCents, 0, 0, M.ACCUMULATOR_LIMIT)
    service.upkeepResid = copyInteger(service.upkeepResid, 0, 0,
      costs.FINANCIAL_YEAR_SECONDS - 1)
    service.capacityResid = copyInteger(service.capacityResid, 0, 0, 3599)
    service.revenueMultiplierResid = copyInteger(
      service.revenueMultiplierResid, 0, 0, difficulty.SCALE - 1)
  end
  for _, market in pairs(state.markets or {}) do
    market.demandResid = copyInteger(market.demandResid, 0, 0, 3599)
  end
  state.deliveryCursors = state.deliveryCursors or {}
  for lineCid, cursor in pairs(state.deliveryCursors) do
    if type(cursor) ~= "table" then cursor = {}; state.deliveryCursors[lineCid] = cursor end
    if cursor.deliveredCargo ~= nil then
      cursor.deliveredCargo = copyInteger(cursor.deliveredCargo, 0, 0, 1000000000)
      cursor.deliveredPassengers = nil
    else
      cursor.deliveredPassengers = copyInteger(
        cursor.deliveredPassengers, 0, 0, 1000000000)
    end
    cursor.earnedRevenueCents = copyInteger(
      cursor.earnedRevenueCents, 0, 0, M.ACCUMULATOR_LIMIT)
  end
  state.scheduler = state.scheduler or {}
  if previousVersion < 6 then
    local last = tonumber(state.scheduler.lastBoundaryGameTimeSeconds)
    state.scheduler.epochSeconds = M.EPOCH_SECONDS
    if last then
      state.scheduler.startGameTimeSeconds = math.max(0,
        util.integer(last, 0) - math.max(0, util.integer(state.epoch, 0)) * M.EPOCH_SECONDS)
      state.scheduler.nextBoundaryGameTimeSeconds = util.integer(last, 0) + M.EPOCH_SECONDS
    end
  end
  state.scheduler.schemaVersion = 2
  if state.scheduler.automatic == nil then state.scheduler.automatic = true end
  state.scheduler.epochSeconds = copyInteger(
    state.scheduler.epochSeconds, M.EPOCH_SECONDS, 60, 86400)
  state.lastResults = state.lastResults or emptyResults()
  state.ledger = state.ledger or emptyLedger()
  for key, value in pairs(emptyResults()) do
    if state.lastResults[key] == nil then state.lastResults[key] = util.deepCopy(value) end
  end
  for key, value in pairs(emptyLedger()) do
    if state.ledger[key] == nil then state.ledger[key] = util.deepCopy(value) end
  end
  state.params = state.params or M.defaultParams()
  local difficultyKey = difficulty.normaliseKey(state.params.economyDifficulty)
  state.params.economyDifficulty = difficultyKey
  state.params.revenueMultiplierPpm = difficulty.multiplier(difficultyKey)
  state.markets = state.markets or {}
  townDemand.migrate(state)
  state.services = state.services or {}
  if previousVersion < 10 then
    -- Pre-v10 passenger services were allowed to infer a whole town's demand
    -- from station-to-town membership alone. Until the owning peer re-registers
    -- the line with an exact native street-catchment proof, quarantine it and
    -- discard its old delivery cursor. This makes old saves fail closed rather
    -- than preserving the disconnected-station revenue exploit for one epoch.
    for lineCid, service in pairs(state.services) do
      local market = state.markets[service.marketCid]
      if market and market.kind ~= "cargo" then
        local metadata = type(service.metadata) == "table" and service.metadata or {}
        if metadata.stationAccessSchema ~= 1 then
          service.enabled = false
          metadata.stationAccessSchema = 1
          metadata.stationAccessSource = "legacy-unverified"
          metadata.endpointAccessReady = { false, false }
          metadata.endpointReachableBuildings = { 0, 0 }
          metadata.endpointTownBuildings = { 0, 0 }
          metadata.stationAccessEligible = false
          service.metadata = metadata
          state.deliveryCursors[lineCid] = nil
        end
      end
    end
  end
  state.version = M.VERSION
  return state
end

function M.setDifficulty(state, key)
  state.params = state.params or M.defaultParams()
  local preset = difficulty.preset(key)
  state.params.economyDifficulty = preset.key
  state.params.revenueMultiplierPpm = preset.revenueMultiplierPpm
  return util.deepCopy(preset)
end

function M.difficultyRule(key)
  return util.deepCopy(difficulty.preset(key))
end

function M.validateDifficultyRule(rules)
  if type(rules) ~= "table" then return true end
  local key, multiplier = rules.economyDifficulty, rules.revenueMultiplierPpm
  if key == nil and multiplier == nil then return true end -- archived pre-v7 action
  local preset = type(key) == "string" and M.DIFFICULTIES[key] or nil
  if not preset then return false, "match rules require a known economy difficulty" end
  if util.integer(multiplier, -1) ~= preset.revenueMultiplierPpm then
    return false, "match economy difficulty multiplier is inconsistent"
  end
  return true
end

function M.configureMatch(state, rules)
  rules = type(rules) == "table" and rules or {}
  M.setDifficulty(state, rules.economyDifficulty)
  return M.startScheduler(state,
    rules.economyStartGameTimeSeconds, rules.economyEpochSeconds)
end

function M.startScheduler(state, startGameTimeSeconds, epochSeconds)
  state.scheduler = state.scheduler or {}
  local period = copyInteger(epochSeconds, M.EPOCH_SECONDS, 60, 86400)
  local start = math.max(0, util.integer(startGameTimeSeconds, 0))
  local settled = math.max(0, util.integer(state.epoch, 0))
  state.scheduler.schemaVersion = 2
  state.scheduler.automatic = true
  state.scheduler.epochSeconds = period
  state.scheduler.startGameTimeSeconds = start
  state.scheduler.lastBoundaryGameTimeSeconds = start + settled * period
  state.scheduler.nextBoundaryGameTimeSeconds = start + (settled + 1) * period
  return state.scheduler
end

function M.nextBoundary(state)
  return state.scheduler and tonumber(state.scheduler.nextBoundaryGameTimeSeconds) or nil
end

function M.applyInfrastructureChange(state, companyCid, retiredCapitalCents, addedCapitalCents)
  assert(type(companyCid) == "string" and companyCid ~= "", "company cid required")
  state.companyCosts = state.companyCosts or {}
  local record = state.companyCosts[companyCid] or {
    companyCid = companyCid, infrastructureCapitalCents = 0,
    annualInfrastructureUpkeepCents = 0, upkeepResid = 0,
  }
  local capital = math.max(0, util.integer(record.infrastructureCapitalCents, 0)
    - math.max(0, util.integer(retiredCapitalCents, 0))
    + math.max(0, util.integer(addedCapitalCents, 0)))
  record.infrastructureCapitalCents = math.min(M.ACCUMULATOR_LIMIT, capital)
  record.annualInfrastructureUpkeepCents = costs.infrastructureAnnualUpkeepCents(
    record.infrastructureCapitalCents)
  record.upkeepResid = copyInteger(record.upkeepResid, 0, 0,
    costs.FINANCIAL_YEAR_SECONDS - 1)
  state.companyCosts[companyCid] = record
  return record
end

function M.upsertVehicleCost(state, vehicleCid, companyCid, annualVehicleUpkeepCents)
  assert(type(vehicleCid) == "string" and vehicleCid ~= "", "vehicle cid required")
  assert(type(companyCid) == "string" and companyCid ~= "", "company cid required")
  state.vehicleCosts = state.vehicleCosts or {}
  local existing = state.vehicleCosts[vehicleCid]
  if existing and existing.companyCid ~= companyCid then
    error("vehicle upkeep company cannot change without an authored transfer")
  end
  local record = existing or { vehicleCid = vehicleCid, companyCid = companyCid, upkeepResid = 0 }
  record.annualVehicleUpkeepCents = copyInteger(
    annualVehicleUpkeepCents, 0, 0, M.ACCUMULATOR_LIMIT)
  record.upkeepResid = copyInteger(record.upkeepResid, 0, 0,
    costs.FINANCIAL_YEAR_SECONDS - 1)
  state.vehicleCosts[vehicleCid] = record
  return record
end

function M.removeVehicleCost(state, vehicleCid)
  if type(vehicleCid) ~= "string" or vehicleCid == "" then return false end
  state.vehicleCosts = state.vehicleCosts or {}
  local existed = state.vehicleCosts[vehicleCid] ~= nil
  state.vehicleCosts[vehicleCid] = nil
  return existed
end

-- Native company wallets are integer dollars while the authored economy is
-- exact cents. Carry a signed sub-dollar residual across settlements so a
-- sequence of profits and losses cannot create money through per-hour
-- rounding (and a one-cent loss does not immediately become a one-dollar
-- debit). The quotient deliberately truncates toward zero; the residual keeps
-- the entire signed remainder.
function M.walletDeltaDollars(state, companyCid, netRevenueCents)
  assert(type(companyCid) == "string" and companyCid ~= "", "company cid required")
  state.payoutResidCents = state.payoutResidCents or {}
  local combined = util.clamp(util.integer(netRevenueCents, 0)
    + util.integer(state.payoutResidCents[companyCid], 0),
    -M.ACCUMULATOR_LIMIT, M.ACCUMULATOR_LIMIT)
  local dollars = combined >= 0 and math.floor(combined / 100)
    or -math.floor(-combined / 100)
  local residual = combined - dollars * 100
  state.payoutResidCents[companyCid] = residual
  return dollars, residual
end

function M.upsertMarket(state, market)
  assert(type(market.cid) == "string" and market.cid ~= "", "market cid required")
  local v4 = util.integer(state.version, M.VERSION) >= 4
  local kind = "passenger"
  if v4 and type(market.kind) == "string" and M.MARKET_KINDS[market.kind] then
    kind = market.kind
  end
  local kindDefaults = M.MARKET_KINDS[kind]
  local gcOutside = copyInteger(market.gcOutsideCents,
    v4 and kindDefaults.gcOutsideCents or 2500, 1, 100000000)
  local existing = state.markets[market.cid]
  local record = {
    cid = market.cid,
    name = tostring(market.name or market.cid),
    demand = copyInteger(market.demand, 100, 0, 1000000000),
    votCentsPerHour = copyInteger(market.votCentsPerHour,
      v4 and kindDefaults.votCentsPerHour or 450, 30, 100000),
    gcOutsideCents = gcOutside,
    thetaCents = copyInteger(market.thetaCents,
      math.max(200, math.floor(gcOutside * 8 / 100)), 50, 1000000),
    metadata = util.deepCopy(market.metadata or {}),
    demandResid = existing and copyInteger(existing.demandResid, 0, 0, 3599)
      or copyInteger(market.demandResid, 0, 0, 3599),
  }
  if v4 then
    record.kind = kind
    record.waitWeightPm = copyInteger(market.waitWeightPm, kindDefaults.waitWeightPm, 0, 10000)
    record.transferSeconds = copyInteger(market.transferSeconds, kindDefaults.transferSeconds, 0, 14400)
  end
  state.markets[market.cid] = record
  townDemand.observeMarket(state, record)
  townDemand.refreshMarkets(state)
  return record
end

function M.upsertService(state, service)
  assert(type(service.lineCid) == "string" and service.lineCid ~= "", "lineCid required")
  assert(type(service.marketCid) == "string" and state.markets[service.marketCid], "known marketCid required")
  assert(type(service.companyCid) == "string" and service.companyCid ~= "", "companyCid required")
  local existing = state.services[service.lineCid]
  local sameMarket = existing and existing.marketCid == service.marketCid
  local enabled = service.enabled ~= false
  local enabledChanged = existing
    and (existing.enabled ~= false) ~= enabled
  if existing and (not sameMarket or enabledChanged) and state.deliveryCursors then
    -- Passenger and cargo presentation counters are independent monotonic
    -- spaces. Reusing a line for a different market, or retiring/reactivating
    -- its presentation after an access change, must not compare the new
    -- ledger with an old cursor or retain an unpayable completed-trip residue.
    state.deliveryCursors[service.lineCid] = nil
  end
  local fareCents = copyInteger(service.fareCents, 1000, 0, 100000000)
  local lastFareCents = nil
  if util.integer(state.version, M.VERSION) >= 3 then
    lastFareCents = sameMarket and existing.lastFareCents
      or copyInteger(service.lastFareCents, fareCents, 0, 100000000)
    if sameMarket and existing.lastFareCents == nil then lastFareCents = nil end
  end
  local initialShare
  if sameMarket then initialShare = existing.sharePpm
  elseif service.sharePpm ~= nil then
    initialShare = copyInteger(service.sharePpm, 0, 0, M.SHARE_SCALE)
  elseif util.integer(state.version, M.VERSION) < 6 then initialShare = 0
  else
    local rivals = 0
    for lineCid, candidate in pairs(state.services) do
      if lineCid ~= service.lineCid and candidate.enabled ~= false
        and candidate.marketCid == service.marketCid then rivals = rivals + 1 end
    end
    if rivals > 0 then initialShare = 0 end
  end
  local residueLimit = util.integer(state.version, M.VERSION) >= 6
    and costs.FINANCIAL_YEAR_SECONDS - 1 or costs.HOURS_PER_YEAR - 1
  state.services[service.lineCid] = {
    lineCid = service.lineCid,
    marketCid = service.marketCid,
    companyCid = service.companyCid,
    name = tostring(service.name or service.lineCid),
    headwaySeconds = copyInteger(service.headwaySeconds, 1800, 30, 86400),
    journeySeconds = copyInteger(service.journeySeconds, 3600, 30, 604800),
    fareCents = fareCents,
    capacity = copyInteger(service.capacity, 100, 0, 1000000000),
    quality = copyInteger(service.quality, 100, 0, 1000),
    transfers = copyInteger(service.transfers, 0, 0, 8),
    enabled = enabled,
    annualVehicleUpkeepCents = copyInteger(service.annualVehicleUpkeepCents,
      existing and existing.annualVehicleUpkeepCents or 0, 0, M.ACCUMULATOR_LIMIT),
    upkeepResid = existing and copyInteger(existing.upkeepResid, 0, 0, residueLimit)
      or copyInteger(service.upkeepResid, 0, 0, residueLimit),
    capacityResid = sameMarket and copyInteger(existing.capacityResid, 0, 0, 3599)
      or copyInteger(service.capacityResid, 0, 0, 3599),
    revenueMultiplierResid = util.integer(state.version, M.VERSION) >= 7
      and (sameMarket and copyInteger(
        existing.revenueMultiplierResid, 0, 0, difficulty.SCALE - 1)
        or copyInteger(service.revenueMultiplierResid, 0, 0, difficulty.SCALE - 1))
      or nil,
    -- Share is a stock: a re-upserted service keeps its earned position, a
    -- brand-new one starts from nothing and must climb at alphaUp.
    sharePpm = initialShare,
    shareResid = sameMarket and existing.shareResid or 0,
    lagLoadPpm = sameMarket and existing.lagLoadPpm or 0,
    -- Fare hikes cannot monetize yesterday's retained share. Other causes of
    -- a lower equilibrium (notably lagged crowding) keep the smooth down-glide.
    lastFareCents = lastFareCents,
    metadata = util.deepCopy(service.metadata or {}),
  }
  return state.services[service.lineCid]
end

function M.removeService(state, lineCid)
  state.services[lineCid] = nil
  if state.deliveryCursors then state.deliveryCursors[lineCid] = nil end
  multihopNetwork.rebuild(state)
end

function M.setFare(state, lineCid, fareCents)
  local service = state.services[lineCid]
  if not service then return false, "unknown service" end
  service.fareCents = copyInteger(fareCents, service.fareCents, 0, 100000000)
  return true, service.fareCents
end

-- Everything the shipper or passenger experiences is converted into cents so
-- every factor shares a unit the player can read. The wait weight and
-- transshipment time are market-kind data (passengers wait at double weight;
-- cargo tolerates waiting but pays dearly per transfer); crowding scales the
-- in-vehicle cost from the previous epoch's load. Markets recorded before
-- version 4 carry no kind fields and keep the passenger-equivalent formula.
M.generalizedCost = flow.generalizedCost
M.logitWeight = flow.logitWeight

function M.evaluateMarket(state, marketCid, deliverySnapshot, periodSeconds)
  return flow.evaluateMarket(state, marketCid, deliverySnapshot, periodSeconds)
end

function M.evaluateAll(state, boundaryGameTimeSeconds, deliverySnapshot)
  -- Validate the authored clock before touching share stocks or upkeep
  -- residuals. A malformed/replayed boundary must be a transactional reject,
  -- not an error that leaves half an economy hour applied locally.
  local scheduler = state.scheduler
  local scheduledBoundary
  if scheduler and tonumber(scheduler.nextBoundaryGameTimeSeconds) then
    local expected = util.integer(scheduler.nextBoundaryGameTimeSeconds, 0)
    scheduledBoundary = util.integer(boundaryGameTimeSeconds, expected)
    if scheduledBoundary ~= expected then
      error("economy boundary is not the next scheduled accounting interval")
    end
  end
  local periodSeconds = copyInteger(scheduler and scheduler.epochSeconds,
    M.EPOCH_SECONDS, 60, 86400)
  local results = emptyResults()
  if util.integer(state.version, 1) >= 6 then results.intervalSeconds = periodSeconds end
  -- Purchased consists remain expensive while unassigned or parked. Resolve
  -- each costed canonical vehicle onto at most one enabled registered line for
  -- line-level presentation, but charge every owned vehicle exactly once at
  -- company level regardless of native operating state.
  local vehicleLine, lineVehicleAnnual, lineVehicleUpkeep = {}, {}, {}
  local companyVehicleAnnual, companyVehicleUpkeep, companyAssignedUpkeep = {}, {}, {}
  for _, lineCid in ipairs(util.sortedKeys(state.services or {})) do
    local service = state.services[lineCid]
    if service.enabled ~= false then
      for _, vehicleCid in ipairs(service.metadata and service.metadata.vehicleCids or {}) do
        local record = state.vehicleCosts and state.vehicleCosts[vehicleCid] or nil
        if record and record.companyCid == service.companyCid and not vehicleLine[vehicleCid] then
          vehicleLine[vehicleCid] = lineCid
        end
      end
    end
  end
  for _, vehicleCid in ipairs(util.sortedKeys(state.vehicleCosts or {})) do
    local record = state.vehicleCosts[vehicleCid]
    local charge, residual = costs.charge(record.annualVehicleUpkeepCents,
      record.upkeepResid, periodSeconds, state.version)
    record.upkeepResid = residual
    local companyCid = record.companyCid
    companyVehicleAnnual[companyCid] = saturatingAdd(
      companyVehicleAnnual[companyCid], record.annualVehicleUpkeepCents)
    companyVehicleUpkeep[companyCid] = saturatingAdd(
      companyVehicleUpkeep[companyCid], charge)
    local lineCid = vehicleLine[vehicleCid]
    if lineCid then
      lineVehicleAnnual[lineCid] = saturatingAdd(
        lineVehicleAnnual[lineCid], record.annualVehicleUpkeepCents)
      lineVehicleUpkeep[lineCid] = saturatingAdd(lineVehicleUpkeep[lineCid], charge)
      companyAssignedUpkeep[companyCid] = saturatingAdd(
        companyAssignedUpkeep[companyCid], charge)
    end
  end
  for _, marketCid in ipairs(util.sortedKeys(state.markets)) do
    local result = assert(M.evaluateMarket(
      state, marketCid, deliverySnapshot, periodSeconds))
    results.markets[marketCid] = result
    results.totalDemand = saturatingAdd(results.totalDemand, result.demand)
    for _, lineCid in ipairs(util.sortedKeys(result.services)) do
      local service = result.services[lineCid]
      if lineVehicleUpkeep[lineCid] ~= nil then
        service.annualVehicleUpkeepCents = lineVehicleAnnual[lineCid] or 0
        service.vehicleUpkeepCents = lineVehicleUpkeep[lineCid]
        service.operatingCostCents = service.vehicleUpkeepCents
        service.netRevenueCents = signedAdd(
          service.grossRevenueCents, -service.vehicleUpkeepCents)
        service.upkeepResid = 0 -- residuals are per canonical vehicle in v5
      end
      local company = results.companies[service.companyCid] or {
        companyCid = service.companyCid, demand = 0,
        revenueCents = 0, grossRevenueCents = 0,
        vehicleUpkeepCents = 0, infrastructureUpkeepCents = 0,
        operatingCostCents = 0, netRevenueCents = 0,
      }
      company.demand = saturatingAdd(company.demand, service.delivered or service.allocated)
      company.revenueCents = saturatingAdd(company.revenueCents, service.revenueCents)
      company.grossRevenueCents = saturatingAdd(
        company.grossRevenueCents, service.grossRevenueCents)
      company.vehicleUpkeepCents = saturatingAdd(
        company.vehicleUpkeepCents, service.vehicleUpkeepCents)
      results.companies[service.companyCid] = company
      results.totalRevenueCents = saturatingAdd(results.totalRevenueCents, service.revenueCents)
      results.totalGrossRevenueCents = saturatingAdd(
        results.totalGrossRevenueCents, service.grossRevenueCents)
      results.totalVehicleUpkeepCents = saturatingAdd(
        results.totalVehicleUpkeepCents, service.vehicleUpkeepCents)
    end
  end
  for _, companyCid in ipairs(util.sortedKeys(companyVehicleUpkeep)) do
    local company = results.companies[companyCid] or {
      companyCid = companyCid, demand = 0,
      revenueCents = 0, grossRevenueCents = 0,
      vehicleUpkeepCents = 0, infrastructureUpkeepCents = 0,
      operatingCostCents = 0, netRevenueCents = 0,
    }
    local unassigned = math.max(0, companyVehicleUpkeep[companyCid]
      - (companyAssignedUpkeep[companyCid] or 0))
    company.vehicleUpkeepCents = saturatingAdd(company.vehicleUpkeepCents, unassigned)
    company.annualVehicleUpkeepCents = companyVehicleAnnual[companyCid] or 0
    company.unassignedVehicleUpkeepCents = unassigned
    results.companies[companyCid] = company
    results.totalVehicleUpkeepCents = saturatingAdd(
      results.totalVehicleUpkeepCents, unassigned)
  end
  -- Infrastructure is company-wide: it is paid even when no registered line
  -- runs in this hour, and it must be charged exactly once rather than once per
  -- market served by the company.
  for _, companyCid in ipairs(util.sortedKeys(state.companyCosts or {})) do
    local costRecord = state.companyCosts[companyCid]
    local infrastructureUpkeepCents, upkeepResid = costs.charge(
      costRecord.annualInfrastructureUpkeepCents, costRecord.upkeepResid,
      periodSeconds, state.version)
    costRecord.upkeepResid = upkeepResid
    local company = results.companies[companyCid] or {
      companyCid = companyCid, demand = 0,
      revenueCents = 0, grossRevenueCents = 0,
      vehicleUpkeepCents = 0, infrastructureUpkeepCents = 0,
      operatingCostCents = 0, netRevenueCents = 0,
    }
    company.infrastructureCapitalCents = costRecord.infrastructureCapitalCents
    company.annualInfrastructureUpkeepCents = costRecord.annualInfrastructureUpkeepCents
    company.infrastructureUpkeepCents = infrastructureUpkeepCents
    company.infrastructureUpkeepResid = upkeepResid
    results.companies[companyCid] = company
    results.totalInfrastructureUpkeepCents = saturatingAdd(
      results.totalInfrastructureUpkeepCents, infrastructureUpkeepCents)
  end
  for _, companyCid in ipairs(util.sortedKeys(results.companies)) do
    local company = results.companies[companyCid]
    company.operatingCostCents = saturatingAdd(
      company.vehicleUpkeepCents, company.infrastructureUpkeepCents)
    company.netRevenueCents = signedAdd(
      company.grossRevenueCents, -company.operatingCostCents)
    results.totalOperatingCostCents = saturatingAdd(
      results.totalOperatingCostCents, company.operatingCostCents)
    results.totalNetRevenueCents = signedAdd(
      results.totalNetRevenueCents, company.netRevenueCents)
  end
  if util.integer(state.version, 1) >= 7 then
    results.townGrowth = townDemand.advance(state, results)
    -- Through demand depends on the endpoint town sizes. Recompute it after
    -- authored growth so A-B benefits immediately from destinations reached
    -- through B, while retaining the same deterministic graph on every peer.
    results.townGrowth.network = multihopNetwork.rebuildPassenger(state)
  end
  state.epoch = (state.epoch or 0) + 1
  results.epoch = state.epoch
  if scheduledBoundary then
    results.boundaryGameTimeSeconds = scheduledBoundary
    scheduler.lastBoundaryGameTimeSeconds = scheduledBoundary
    scheduler.nextBoundaryGameTimeSeconds = scheduledBoundary
      + copyInteger(scheduler.epochSeconds, M.EPOCH_SECONDS, 60, 86400)
  end
  state.lastResults = results
  return results
end

-- The model is deterministic integer arithmetic over replicated state, so the
-- strongest validation of host results is an independent local evaluation and
-- a canonical checksum comparison. On success the local mutation is already
-- the authoritative one; on mismatch the model state has diverged and the
-- caller must fail the session closed.
function M.acceptAuthoritativeResults(state, results, deliverySnapshot)
  if type(results) ~= "table" or type(results.markets) ~= "table" or type(results.companies) ~= "table" then
    return false, "authoritative results are malformed"
  end
  if tonumber(results.epoch) ~= (state.epoch or 0) + 1 then
    return false, "authoritative epoch is not the next epoch"
  end
  -- Evaluate on a copy so a diverging or tampered result rejects without
  -- advancing local stocks; adopt the copy only after the digests agree.
  local preview = util.deepCopy(state)
  local localResults = M.evaluateAll(
    preview, results.boundaryGameTimeSeconds, deliverySnapshot)
  local localDigest = hash.value(localResults)
  local authoritativeDigest = hash.value(results)
  if localDigest ~= authoritativeDigest then
    return false, "authoritative results diverge from deterministic local evaluation: "
      .. tostring(authoritativeDigest) .. " ~= " .. tostring(localDigest)
  end
  state.epoch = preview.epoch
  state.markets = preview.markets
  state.towns = preview.towns
  state.services = preview.services
  state.companyCosts = preview.companyCosts
  state.vehicleCosts = preview.vehicleCosts
  state.deliveryCursors = preview.deliveryCursors
  state.payoutResidCents = preview.payoutResidCents
  state.scheduler = preview.scheduler
  state.lastResults = preview.lastResults
  return true, state.lastResults
end

function M.recordSettlement(state, results)
  state.ledger = state.ledger or emptyLedger()
  state.ledger.settledEpochs = state.ledger.settledEpochs or {}
  local epoch = assert(tonumber(results.epoch), "results epoch required")
  local epochKey = tostring(epoch)
  if state.ledger.settledEpochs[epochKey] then return false, "epoch already settled" end
  state.ledger.settledEpochs[epochKey] = true
  state.ledger.settlementCount = (state.ledger.settlementCount or 0) + 1
  state.ledger.totalDemand = saturatingAdd(state.ledger.totalDemand, results.totalDemand)
  state.ledger.totalRevenueCents = saturatingAdd(state.ledger.totalRevenueCents, results.totalRevenueCents)
  state.ledger.totalGrossRevenueCents = saturatingAdd(
    state.ledger.totalGrossRevenueCents, results.totalGrossRevenueCents)
  state.ledger.totalVehicleUpkeepCents = saturatingAdd(
    state.ledger.totalVehicleUpkeepCents, results.totalVehicleUpkeepCents)
  state.ledger.totalInfrastructureUpkeepCents = saturatingAdd(
    state.ledger.totalInfrastructureUpkeepCents, results.totalInfrastructureUpkeepCents)
  state.ledger.totalOperatingCostCents = saturatingAdd(
    state.ledger.totalOperatingCostCents, results.totalOperatingCostCents)
  state.ledger.totalNetRevenueCents = signedAdd(
    state.ledger.totalNetRevenueCents, results.totalNetRevenueCents)
  state.ledger.companies = state.ledger.companies or {}
  for _, companyCid in ipairs(util.sortedKeys(results.companies or {})) do
    local result = results.companies[companyCid]
    local total = state.ledger.companies[companyCid] or {
      companyCid = companyCid, demand = 0, revenueCents = 0,
      grossRevenueCents = 0, vehicleUpkeepCents = 0,
      infrastructureUpkeepCents = 0, operatingCostCents = 0,
      netRevenueCents = 0, wins = 0,
    }
    total.demand = saturatingAdd(total.demand, result.demand)
    total.revenueCents = saturatingAdd(total.revenueCents, result.revenueCents)
    total.grossRevenueCents = saturatingAdd(
      total.grossRevenueCents, result.grossRevenueCents)
    total.vehicleUpkeepCents = saturatingAdd(
      total.vehicleUpkeepCents, result.vehicleUpkeepCents)
    total.infrastructureUpkeepCents = saturatingAdd(
      total.infrastructureUpkeepCents, result.infrastructureUpkeepCents)
    total.operatingCostCents = saturatingAdd(
      total.operatingCostCents, result.operatingCostCents)
    total.netRevenueCents = signedAdd(total.netRevenueCents, result.netRevenueCents)
    state.ledger.companies[companyCid] = total
  end
  for _, market in pairs(results.markets or {}) do
    local bestCompany, bestDemand = nil, -1
    for _, service in pairs(market.services or {}) do
      if service.allocated > bestDemand or (service.allocated == bestDemand and service.companyCid < (bestCompany or "~")) then
        bestCompany, bestDemand = service.companyCid, service.allocated
      end
    end
    if bestCompany and state.ledger.companies[bestCompany] then
      state.ledger.companies[bestCompany].wins = (state.ledger.companies[bestCompany].wins or 0) + 1
    end
  end
  return true, state.ledger
end

function M.scoreboard(state, companies)
  local marketsByCompany, linesByCompany = {}, {}
  for _, service in pairs(state.services or {}) do
    if service.enabled then
      linesByCompany[service.companyCid] = (linesByCompany[service.companyCid] or 0) + 1
      marketsByCompany[service.companyCid] = marketsByCompany[service.companyCid] or {}
      marketsByCompany[service.companyCid][service.marketCid] = true
    end
  end
  local result = {}
  for _, companyCid in ipairs(util.sortedKeys(companies or {})) do
    local totals = state.ledger and state.ledger.companies and state.ledger.companies[companyCid] or {}
    local reach = util.tableCount(marketsByCompany[companyCid])
    local lines = linesByCompany[companyCid] or 0
    local revenue = totals.revenueCents or 0
    local gross = totals.grossRevenueCents or revenue
    local operatingCost = totals.operatingCostCents or 0
    local net = totals.netRevenueCents
    if net == nil then net = gross - operatingCost end
    local demand = totals.demand or 0
    result[companyCid] = {
      companyCid = companyCid,
      name = companies[companyCid].name or companyCid,
      settledDemand = demand,
      settledRevenueCents = revenue,
      settledGrossRevenueCents = gross,
      settledOperatingCostCents = operatingCost,
      settledNetRevenueCents = net,
      marketsReached = reach,
      activeLines = lines,
      marketWins = totals.wins or 0,
      modelValueCents = math.max(0, signedAdd(
        signedAdd(net * 10, saturatingMultiply(demand, 100)),
        saturatingAdd(saturatingMultiply(reach, 500000), saturatingMultiply(lines, 250000))
      )),
    }
  end
  return result
end

return M
