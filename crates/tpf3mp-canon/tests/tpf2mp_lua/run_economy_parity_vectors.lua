local projectRoot = assert(arg[1], "project root argument required")
local outputPath = assert(arg[2], "output path argument required")

package.path = projectRoot .. "/tpf2_mp_1/res/scripts/?.lua;" .. package.path

local economy = require "tpf2_mp/economy"
local json = require "tpf2_mp/json"
local util = require "tpf2_mp/util"

local function digestView(state)
  local markets, services = {}, {}
  for _, cid in ipairs(util.sortedKeys(state.markets or {})) do
    local value = state.markets[cid]
    markets[cid] = {
      cid = value.cid,
      name = value.name,
      kind = value.kind,
      demand = value.demand,
      outsideWeight = value.outsideWeight,
      votCentsPerHour = value.votCentsPerHour,
      gcOutsideCents = value.gcOutsideCents,
      thetaCents = value.thetaCents,
      waitWeightPm = value.waitWeightPm,
      transferSeconds = value.transferSeconds,
      demandResid = value.demandResid,
      metadata = util.deepCopy(value.metadata or {}),
    }
  end
  for _, cid in ipairs(util.sortedKeys(state.services or {})) do
    local value = state.services[cid]
    services[cid] = {
      lineCid = value.lineCid,
      marketCid = value.marketCid,
      companyCid = value.companyCid,
      name = value.name,
      headwaySeconds = value.headwaySeconds,
      journeySeconds = value.journeySeconds,
      fareCents = value.fareCents,
      capacity = value.capacity,
      quality = value.quality,
      transfers = value.transfers,
      enabled = value.enabled,
      sharePpm = value.sharePpm,
      shareResid = value.shareResid,
      lagLoadPpm = value.lagLoadPpm,
      lastFareCents = value.lastFareCents,
      annualVehicleUpkeepCents = value.annualVehicleUpkeepCents,
      upkeepResid = value.upkeepResid,
      capacityResid = value.capacityResid,
      revenueMultiplierResid = value.revenueMultiplierResid,
      metadata = util.deepCopy(value.metadata or {}),
    }
  end
  return {
    version = state.version,
    epoch = state.epoch,
    params = util.deepCopy(state.params),
    markets = markets,
    towns = util.deepCopy(state.towns or {}),
    services = services,
    companyCosts = util.deepCopy(state.companyCosts or {}),
    vehicleCosts = util.deepCopy(state.vehicleCosts or {}),
    deliveryCursors = util.deepCopy(state.deliveryCursors or {}),
    payoutResidCents = util.deepCopy(state.payoutResidCents or {}),
    scheduler = util.deepCopy(state.scheduler or {}),
    lastResults = util.deepCopy(state.lastResults),
    ledger = util.deepCopy(state.ledger),
  }
end

local function applyOverrides(state, overrides)
  for lineCid, values in pairs(overrides or {}) do
    local service = assert(state.services[lineCid], "override references missing service " .. tostring(lineCid))
    if values.sharePpmNull then
      service.sharePpm = nil
    elseif values.sharePpm ~= nil then
      service.sharePpm = values.sharePpm
    end
    if values.shareResid ~= nil then service.shareResid = values.shareResid end
    if values.lagLoadPpm ~= nil then service.lagLoadPpm = values.lagLoadPpm end
  end
end

local scenarios = {}

local function addScenario(spec)
  local state = economy.newState()
  if spec.version then
    state.version = spec.version
    if spec.version == 2 then state.params.alphaDownPm = 250 end
  end
  if spec.params then
    for key, value in pairs(spec.params) do state.params[key] = value end
  end
  for _, market in ipairs(spec.markets) do economy.upsertMarket(state, market) end
  for _, service in ipairs(spec.services) do economy.upsertService(state, service) end
  for vehicleCid, value in pairs(spec.vehicleCosts or {}) do
    local record = economy.upsertVehicleCost(state, vehicleCid, value.companyCid,
      value.annualVehicleUpkeepCents)
    if value.upkeepResid ~= nil then record.upkeepResid = value.upkeepResid end
  end
  for companyCid, value in pairs(spec.companyCosts or {}) do
    local capital = type(value) == "table" and value.infrastructureCapitalCents or value
    local record = economy.applyInfrastructureChange(state, companyCid, 0, capital)
    if type(value) == "table" and value.upkeepResid ~= nil then
      record.upkeepResid = value.upkeepResid
    end
  end
  if spec.scheduler then
    economy.startScheduler(state, spec.scheduler.startGameTimeSeconds,
      spec.scheduler.epochSeconds)
  end
  applyOverrides(state, spec.overrides)
  state.deliveryCursors = util.deepCopy(spec.deliveryCursors or {})
  for _, service in ipairs(spec.reupserts or {}) do economy.upsertService(state, service) end

  local companies = {}
  for _, service in ipairs(spec.services) do
    companies[service.companyCid] = companies[service.companyCid]
      or { cid = service.companyCid, name = "Company " .. service.companyCid }
  end
  for companyCid in pairs(spec.companyCosts or {}) do
    companies[companyCid] = companies[companyCid]
      or { cid = companyCid, name = "Company " .. companyCid }
  end
  for _, value in pairs(spec.vehicleCosts or {}) do
    local companyCid = value.companyCid
    companies[companyCid] = companies[companyCid]
      or { cid = companyCid, name = "Company " .. companyCid }
  end

  local record = {
    id = spec.id,
    params = util.deepCopy(spec.params or {}),
    markets = util.deepCopy(spec.markets),
    services = util.deepCopy(spec.services),
    overrides = util.deepCopy(spec.overrides or {}),
    reupserts = util.deepCopy(spec.reupserts or {}),
    deliveryCursors = util.deepCopy(spec.deliveryCursors or {}),
    companyCosts = util.deepCopy(spec.companyCosts or {}),
    vehicleCosts = util.deepCopy(spec.vehicleCosts or {}),
    scheduler = util.deepCopy(spec.scheduler),
    fareSchedule = util.deepCopy(spec.fareSchedule or {}),
    epochs = spec.epochs,
    initial = digestView(state),
    results = {},
    companies = companies,
  }
  for index = 1, spec.epochs do
    for lineCid, fareCents in pairs((spec.fareSchedule or {})[index] or {}) do
      assert(economy.setFare(state, lineCid, fareCents))
    end
    local result = economy.evaluateAll(state, economy.nextBoundary(state))
    record.results[index] = util.deepCopy(result)
    assert(economy.recordSettlement(state, result))
    for _, companyCid in ipairs(util.sortedKeys(result.companies or {})) do
      local company = result.companies[companyCid]
      economy.walletDeltaDollars(state, companyCid,
        company.netRevenueCents ~= nil and company.netRevenueCents or company.revenueCents)
    end
  end
  record.final = digestView(state)
  record.scoreboard = economy.scoreboard(state, companies)
  scenarios[#scenarios + 1] = record
end

addScenario({
  id = "demo-trajectory",
  markets = {
    { cid = "market:demo", name = "Demo", demand = 1000, votCentsPerHour = 450,
      gcOutsideCents = 2500, thetaCents = 250, metadata = { source = "parity" } },
  },
  services = {
    { lineCid = "line:a", marketCid = "market:demo", companyCid = "company:1", name = "A",
      headwaySeconds = 900, journeySeconds = 2400, fareCents = 1000, capacity = 600,
      quality = 100, transfers = 0 },
    { lineCid = "line:b", marketCid = "market:demo", companyCid = "company:2", name = "B",
      headwaySeconds = 1100, journeySeconds = 2200, fareCents = 900, capacity = 600,
      quality = 100, transfers = 0 },
  },
  epochs = 40,
})

addScenario({
  id = "v10-access-retirement-clears-delivery-cursor",
  markets = {
    { cid = "market:access", name = "Access", demand = 100 },
  },
  services = {
    { lineCid = "line:access", marketCid = "market:access", companyCid = "company:1",
      name = "Access line", headwaySeconds = 600, journeySeconds = 1200,
      fareCents = 1000, capacity = 100, quality = 100, enabled = true },
  },
  deliveryCursors = {
    ["line:access"] = { deliveredPassengers = 12, earnedRevenueCents = 12000 },
  },
  reupserts = {
    { lineCid = "line:access", marketCid = "market:access", companyCid = "company:1",
      name = "Access line", headwaySeconds = 600, journeySeconds = 1200,
      fareCents = 1000, capacity = 100, quality = 100, enabled = false },
  },
  epochs = 1,
})

addScenario({
  id = "v5-operating-cost-clock",
  markets = {
    { cid = "market:cost", name = "Cost", demand = 100,
      gcOutsideCents = 2500, thetaCents = 250 },
  },
  services = {
    { lineCid = "line:cost", marketCid = "market:cost", companyCid = "company:1",
      name = "Costed consist", headwaySeconds = 600, journeySeconds = 1200,
      fareCents = 0, capacity = 100, quality = 100,
      metadata = { vehicleCids = { "vehicle:cost:assigned" } } },
  },
  vehicleCosts = {
    ["vehicle:cost:assigned"] = {
      companyCid = "company:1", annualVehicleUpkeepCents = 43800001,
      upkeepResid = 8759,
    },
    ["vehicle:cost:depot"] = {
      companyCid = "company:1", annualVehicleUpkeepCents = 8760001,
      upkeepResid = 8759,
    },
  },
  companyCosts = {
    ["company:1"] = { infrastructureCapitalCents = 87600001, upkeepResid = 8759 },
  },
  scheduler = { startGameTimeSeconds = 123, epochSeconds = 3600 },
  epochs = 3,
})

addScenario({
  id = "legacy-v2-cutoff-replay",
  version = 2,
  markets = {
    { cid = "market:legacy", demand = 1000, votCentsPerHour = 450,
      gcOutsideCents = 2500, thetaCents = 200 },
  },
  services = {
    { lineCid = "line:legacy", marketCid = "market:legacy", companyCid = "company:1",
      headwaySeconds = 900, journeySeconds = 2400, fareCents = 100000000,
      capacity = 600, quality = 100, transfers = 0 },
  },
  overrides = { ["line:legacy"] = { sharePpmNull = true } },
  epochs = 2,
})

addScenario({
  id = "v3-fare-shock-and-zero-cutoff",
  markets = {
    { cid = "market:fare-guard", demand = 1000, votCentsPerHour = 450,
      gcOutsideCents = 2500, thetaCents = 200 },
  },
  services = {
    { lineCid = "line:guard", marketCid = "market:fare-guard", companyCid = "company:1",
      headwaySeconds = 900, journeySeconds = 2400, fareCents = 1000,
      capacity = 600, quality = 100, transfers = 0 },
    { lineCid = "line:rival", marketCid = "market:fare-guard", companyCid = "company:2",
      headwaySeconds = 1100, journeySeconds = 2200, fareCents = 900,
      capacity = 600, quality = 100, transfers = 0 },
  },
  overrides = {
    ["line:guard"] = { sharePpm = 400000, shareResid = 731, lagLoadPpm = 600000 },
    ["line:rival"] = { sharePpm = 500000, shareResid = 197, lagLoadPpm = 700000 },
  },
  fareSchedule = {
    { ["line:guard"] = 100000000 },
    { ["line:guard"] = 1000 },
  },
  epochs = 2,
})

addScenario({
  id = "maximum-revenue-boundary",
  markets = {
    { cid = "market:max", demand = 999999937, votCentsPerHour = 30,
      gcOutsideCents = 100000000, thetaCents = 50 },
  },
  services = {
    { lineCid = "line:max", marketCid = "market:max", companyCid = "company:1",
      headwaySeconds = 30, journeySeconds = 30, fareCents = 99999989,
      capacity = 1000000000, quality = 1000, transfers = 0 },
  },
  overrides = { ["line:max"] = { sharePpmNull = true } },
  epochs = 3,
})

addScenario({
  id = "negative-glide-and-capacity-cascade",
  params = { alphaUpPm = 83, alphaDownPm = 257, maxWaitSeconds = 1799,
    transferSeconds = 481, crowdThresholdPpm = 699999 },
  markets = {
    { cid = "market:cascade", demand = 10007, votCentsPerHour = 99999,
      gcOutsideCents = 7777777, thetaCents = 913 },
  },
  services = {
    { lineCid = "line:c", marketCid = "market:cascade", companyCid = "company:1",
      headwaySeconds = 86399, journeySeconds = 604799, fareCents = 1000003,
      capacity = 7, quality = 1, transfers = 8 },
    { lineCid = "line:d", marketCid = "market:cascade", companyCid = "company:2",
      headwaySeconds = 31, journeySeconds = 31, fareCents = 3,
      capacity = 17, quality = 999, transfers = 1 },
    { lineCid = "line:e", marketCid = "market:cascade", companyCid = "company:3",
      headwaySeconds = 1777, journeySeconds = 3555, fareCents = 4567,
      capacity = 999999, quality = 301, transfers = 3 },
  },
  overrides = {
    ["line:c"] = { sharePpm = 900001, shareResid = 999, lagLoadPpm = 1000000 },
    ["line:d"] = { sharePpm = 90000, shareResid = 1, lagLoadPpm = 999999 },
    ["line:e"] = { sharePpm = 9999, shareResid = 731, lagLoadPpm = 0 },
  },
  epochs = 12,
})

addScenario({
  id = "v4-cargo-kind-defaults-and-fare-latch",
  markets = {
    -- Only the kind is given: both languages must resolve the same freight
    -- defaults (vot 60, outside 1800, wait weight 1000, transfer 1800).
    { cid = "market:freight", name = "Freight", kind = "cargo", demand = 800 },
  },
  services = {
    { lineCid = "line:f1", marketCid = "market:freight", companyCid = "company:1", name = "F1",
      headwaySeconds = 3600, journeySeconds = 5400, fareCents = 700, capacity = 400,
      quality = 100, transfers = 0 },
    { lineCid = "line:f2", marketCid = "market:freight", companyCid = "company:2", name = "F2",
      headwaySeconds = 2700, journeySeconds = 4800, fareCents = 800, capacity = 400,
      quality = 100, transfers = 2 },
  },
  overrides = {
    ["line:f1"] = { sharePpm = 480000, shareResid = 313, lagLoadPpm = 960000 },
    ["line:f2"] = { sharePpm = 350000, shareResid = 77, lagLoadPpm = 700000 },
  },
  fareSchedule = {
    { ["line:f1"] = 5000 },
    { ["line:f1"] = 700 },
  },
  epochs = 12,
})

addScenario({
  id = "v4-mixed-kinds-and-explicit-weighting",
  markets = {
    { cid = "market:pax", kind = "passenger", demand = 1000, votCentsPerHour = 450,
      gcOutsideCents = 2500, thetaCents = 250 },
    { cid = "market:bulk", kind = "cargo", demand = 600, votCentsPerHour = 90,
      gcOutsideCents = 2200, thetaCents = 300, waitWeightPm = 500, transferSeconds = 2400 },
  },
  services = {
    { lineCid = "line:p", marketCid = "market:pax", companyCid = "company:1", name = "P",
      headwaySeconds = 900, journeySeconds = 2400, fareCents = 1000, capacity = 600,
      quality = 100, transfers = 0 },
    { lineCid = "line:q", marketCid = "market:bulk", companyCid = "company:1", name = "Q",
      headwaySeconds = 7200, journeySeconds = 9000, fareCents = 900, capacity = 300,
      quality = 50, transfers = 1 },
    { lineCid = "line:r", marketCid = "market:bulk", companyCid = "company:2", name = "R",
      headwaySeconds = 3600, journeySeconds = 7200, fareCents = 1100, capacity = 300,
      quality = 150, transfers = 3 },
  },
  overrides = {
    ["line:q"] = { sharePpm = 250000, shareResid = 111, lagLoadPpm = 850000 },
  },
  epochs = 10,
})

addScenario({
  id = "upsert-clamps-and-stock-preservation",
  markets = {
    { cid = "market:clamp", demand = -17, votCentsPerHour = 1,
      gcOutsideCents = 0, thetaCents = 9999999 },
  },
  services = {
    { lineCid = "line:clamp", marketCid = "market:clamp", companyCid = "company:1",
      headwaySeconds = 0, journeySeconds = 9999999, fareCents = -1,
      capacity = 2000000000, quality = 2000, transfers = -3 },
  },
  overrides = {
    ["line:clamp"] = { sharePpm = 123456, shareResid = 789, lagLoadPpm = 987654 },
  },
  reupserts = {
    { lineCid = "line:clamp", marketCid = "market:clamp", companyCid = "company:1",
      name = "Re-upserted", headwaySeconds = 31, journeySeconds = 61,
      fareCents = 101, capacity = 202, quality = 303, transfers = 4 },
  },
  epochs = 4,
})

addScenario({
  id = "migrated-null-stock-reupsert",
  markets = {
    { cid = "market:migrated", demand = 101, votCentsPerHour = 451,
      gcOutsideCents = 2501, thetaCents = 251 },
  },
  services = {
    { lineCid = "line:migrated", marketCid = "market:migrated", companyCid = "company:2",
      headwaySeconds = 901, journeySeconds = 2401, fareCents = 1001,
      capacity = 601, quality = 101, transfers = 1 },
  },
  overrides = {
    ["line:migrated"] = { sharePpmNull = true, shareResid = 0, lagLoadPpm = 0 },
  },
  reupserts = {
    { lineCid = "line:migrated", marketCid = "market:migrated", companyCid = "company:2",
      headwaySeconds = 902, journeySeconds = 2402, fareCents = 1002,
      capacity = 602, quality = 102, transfers = 2 },
  },
  epochs = 2,
})

addScenario({
  id = "v7-easy-town-growth",
  params = { economyDifficulty = "easy", revenueMultiplierPpm = 1500000 },
  markets = {
    { cid = "market:growing", kind = "passenger", demand = 533,
      gcOutsideCents = 100000000, thetaCents = 200,
      metadata = { townA = "town:alpha", townB = "town:beta",
        -- Deliberately below the missing-observation fallback so parity catches
        -- accidental reapplication of that fallback during later settlements.
        townSizeA = 80, townSizeB = 120, corridorMeters = 3000 } },
  },
  services = {
    { lineCid = "line:growing", marketCid = "market:growing",
      companyCid = "company:1", name = "Growing",
      headwaySeconds = 900, journeySeconds = 600, fareCents = 950,
      capacity = 320, quality = 100, transfers = 0 },
  },
  epochs = 24,
})

addScenario({
  id = "v8-company-owned-local-feeder-access",
  markets = {
    { cid = "market:corridor", kind = "passenger", demand = 1200,
      gcOutsideCents = 2500, thetaCents = 250,
      metadata = { marketScope = "corridor", townA = "town:a", townB = "town:b" } },
    { cid = "market:local:a", kind = "passenger", demand = 400,
      gcOutsideCents = 2500, thetaCents = 250,
      metadata = { marketScope = "local", townA = "town:a", townB = "town:a" } },
  },
  services = {
    { lineCid = "line:rail:a", marketCid = "market:corridor", companyCid = "company:1",
      headwaySeconds = 600, journeySeconds = 1200, fareCents = 1000,
      capacity = 600, quality = 100, transfers = 0,
      metadata = { carrier = "RAIL", marketScope = "corridor",
        endpointTownCids = { "town:a", "town:b" },
        stationGroupCids = { "station:a", "station:b" } } },
    { lineCid = "line:rail:b", marketCid = "market:corridor", companyCid = "company:2",
      headwaySeconds = 600, journeySeconds = 1200, fareCents = 1000,
      capacity = 600, quality = 100, transfers = 0,
      metadata = { carrier = "RAIL", marketScope = "corridor",
        endpointTownCids = { "town:a", "town:b" },
        stationGroupCids = { "station:a", "station:b" } } },
    { lineCid = "line:bus:a", marketCid = "market:local:a", companyCid = "company:1",
      headwaySeconds = 300, journeySeconds = 900, fareCents = 300,
      capacity = 300, quality = 80, transfers = 0,
      metadata = { carrier = "ROAD", marketScope = "local",
        endpointTownCids = { "town:a", "town:a" },
        stationGroupCids = { "station:suburb", "station:a" } } },
  },
  epochs = 12,
})

local seed = 20260804
local function nextValue(limit)
  seed = (seed * 48271) % 2147483647
  return seed % limit
end
local demands = { 0, 1, 17, 999, 1000, 10007, 999999, 999999937 }
local fares = { 0, 1, 99, 1000, 99999, 1000003, 99999989 }
local capacities = { 0, 1, 7, 17, 599, 600, 10007, 999999937 }
local headways = { 30, 31, 899, 900, 1799, 3601, 86399, 86400 }
local journeys = { 30, 31, 1799, 2400, 3601, 77777, 604799, 604800 }
local qualities = { 0, 1, 99, 100, 333, 999, 1000 }
local outsideCosts = { 1, 50, 2499, 2500, 999999, 100000000 }
local thetas = { 50, 51, 249, 250, 913, 999999 }
local vots = { 30, 31, 449, 450, 12345, 99999, 100000 }

-- Randomized model-v8 multimodal cases exercise best-feeder selection,
-- endpoint/company isolation, carrier filtering, duplicate-stop rejection,
-- capacity/frequency caps, and same-town growth across Lua/Python replay.
local feederCarriers = { "ROAD", "TRAM", "RAIL", "WATER", "UNKNOWN" }
local feederCapacities = { 0, 1, 17, 149, 150, 151, 600 }
local feederHeadways = { 30, 299, 300, 599, 600, 3601, 86400 }
for scenarioIndex = 1, 32 do
  local townA = "town:feeder-" .. scenarioIndex .. "-a"
  local townB = "town:feeder-" .. scenarioIndex .. "-b"
  local stationA = "station_group:feeder-" .. scenarioIndex .. "-a"
  local stationB = "station_group:feeder-" .. scenarioIndex .. "-b"
  local corridorCid = "market:feeder-" .. scenarioIndex
  local markets = {
    { cid = corridorCid, name = "Feeder corridor " .. scenarioIndex,
      kind = "passenger", demand = demands[nextValue(#demands) + 1],
      votCentsPerHour = vots[nextValue(#vots) + 1],
      gcOutsideCents = outsideCosts[nextValue(#outsideCosts) + 1],
      thetaCents = thetas[nextValue(#thetas) + 1],
      metadata = { marketScope = "corridor", townA = townA, townB = townB,
        townSizeA = 80 + nextValue(500), townSizeB = 80 + nextValue(500),
        corridorMeters = 500 + nextValue(25000) } },
    { cid = corridorCid .. ":local-a", name = "Local A", kind = "passenger",
      demand = 100 + nextValue(1000), gcOutsideCents = 2500, thetaCents = 250,
      metadata = { marketScope = "local", townA = townA, townB = townA,
        townSizeA = 80 + nextValue(500), townSizeB = 80 + nextValue(500),
        corridorMeters = 1000 + nextValue(5000) } },
    { cid = corridorCid .. ":local-b", name = "Local B", kind = "passenger",
      demand = 100 + nextValue(1000), gcOutsideCents = 2500, thetaCents = 250,
      metadata = { marketScope = "local", townA = townB, townB = townB,
        townSizeA = 80 + nextValue(500), townSizeB = 80 + nextValue(500),
        corridorMeters = 1000 + nextValue(5000) } },
  }
  local services = {}
  for companyIndex = 1, 2 do
    services[#services + 1] = {
      lineCid = "line:feeder-corridor-" .. scenarioIndex .. "-" .. companyIndex,
      marketCid = corridorCid, companyCid = "company:" .. companyIndex,
      name = "Corridor " .. companyIndex,
      headwaySeconds = feederHeadways[nextValue(#feederHeadways) + 1],
      journeySeconds = 300 + nextValue(7200), fareCents = nextValue(5001),
      capacity = feederCapacities[nextValue(#feederCapacities) + 1],
      quality = nextValue(501), transfers = nextValue(3),
      metadata = { carrier = "RAIL", marketScope = "corridor",
        endpointTownCids = { townA, townB },
        stationGroupCids = { stationA, stationB } },
    }
  end
  for feederIndex = 1, 6 do
    local atA = nextValue(2) == 0
    local townCid, endpoint = atA and townA or townB, atA and stationA or stationB
    local other = "station_group:feeder-" .. scenarioIndex .. "-local-" .. feederIndex
    local duplicate = nextValue(5) == 0
    local connects = nextValue(4) ~= 0
    services[#services + 1] = {
      lineCid = "line:feeder-local-" .. scenarioIndex .. "-" .. feederIndex,
      marketCid = corridorCid .. (atA and ":local-a" or ":local-b"),
      companyCid = "company:" .. (nextValue(2) + 1), name = "Local " .. feederIndex,
      headwaySeconds = feederHeadways[nextValue(#feederHeadways) + 1],
      journeySeconds = 120 + nextValue(2400), fareCents = nextValue(1001),
      capacity = feederCapacities[nextValue(#feederCapacities) + 1],
      quality = nextValue(201), transfers = nextValue(2), enabled = nextValue(5) ~= 0,
      metadata = {
        carrier = feederCarriers[nextValue(#feederCarriers) + 1], marketScope = "local",
        endpointTownCids = { townCid, townCid },
        stationGroupCids = duplicate and { other, other }
          or { other, connects and endpoint or (other .. "-miss") },
      },
    }
  end
  addScenario({
    id = "feeder-fuzz-" .. scenarioIndex,
    markets = markets, services = services, epochs = nextValue(4) + 1,
  })
end

for scenarioIndex = 1, 64 do
  local marketCid = "market:fuzz-" .. scenarioIndex
  local market = {
    cid = marketCid,
    name = "Fuzz " .. scenarioIndex,
    demand = demands[nextValue(#demands) + 1],
    votCentsPerHour = vots[nextValue(#vots) + 1],
    gcOutsideCents = outsideCosts[nextValue(#outsideCosts) + 1],
    thetaCents = thetas[nextValue(#thetas) + 1],
    metadata = { vector = scenarioIndex },
  }
  local services, overrides = {}, {}
  local serviceCount = nextValue(4) + 1
  for serviceIndex = 1, serviceCount do
    local lineCid = "line:fuzz-" .. scenarioIndex .. "-" .. serviceIndex
    services[#services + 1] = {
      lineCid = lineCid,
      marketCid = marketCid,
      companyCid = "company:" .. ((serviceIndex % 3) + 1),
      name = "Service " .. serviceIndex,
      headwaySeconds = headways[nextValue(#headways) + 1],
      journeySeconds = journeys[nextValue(#journeys) + 1],
      fareCents = fares[nextValue(#fares) + 1],
      capacity = capacities[nextValue(#capacities) + 1],
      quality = qualities[nextValue(#qualities) + 1],
      transfers = nextValue(9),
      enabled = nextValue(5) ~= 0,
    }
    local stockKind = nextValue(5)
    if stockKind == 0 then
      overrides[lineCid] = { sharePpmNull = true, shareResid = 0, lagLoadPpm = 0 }
    else
      overrides[lineCid] = {
        sharePpm = nextValue(1000001),
        shareResid = nextValue(1000),
        lagLoadPpm = nextValue(1200001),
      }
    end
  end
  addScenario({
    id = "fuzz-" .. scenarioIndex,
    markets = { market },
    services = services,
    overrides = overrides,
    epochs = nextValue(6) + 1,
  })
end

local handle = assert(io.open(outputPath, "wb"))
handle:write(json.encode({ schema = 1, scenarios = scenarios }))
handle:write("\n")
handle:close()
print("PASS generated " .. #scenarios .. " Lua economy parity scenarios")
