-- Differential-test harness for TPF3-MP's Rust port of TPF2MP's economy.
-- This file is TPF3-MP code, not part of TPF2MP.
--
-- `hook` wraps a ported Lua function. While `tracing` is on, the wrapper
-- records a copy of each call's arguments and results (and, for functions
-- that mutate their arguments, a copy of the arguments afterwards), so the
-- Rust side can replay every call against the port. Otherwise it calls
-- straight through. `originals` keeps every unwrapped function; `run` calls
-- one directly and returns the same kind of record, for the property tests.
--
-- Module functions are wrapped by `install`. Local functions and values
-- reach the harness through the block that `tpf2mp.rs` inserts before a
-- module's final `return M`.

local H = { originals = {}, exports = {}, trace = {}, tracing = false }
TPF3MP_HARNESS = H

local function copy(value, seen)
  if type(value) ~= "table" then return value end
  seen = seen or {}
  if seen[value] then return seen[value] end
  local result = {}
  seen[value] = result
  for key, item in pairs(value) do result[copy(key, seen)] = copy(item, seen) end
  return result
end

local function pack(...)
  return { n = select("#", ...), ... }
end

-- Copies of the named fields of a state table: the parts a replay reads.
local function pick(state, fields)
  local result = {}
  for _, field in ipairs(fields) do result[field] = copy(state[field]) end
  return result
end

-- How to copy the arguments of functions that take whole economy states.
-- Any other function's arguments are copied in full.
local CAPTURE = {
  ["economy_flow.evaluateMarket"] = function(a)
    return { n = a.n, pick(a[1], { "version", "params", "markets", "services",
      "vehicleCosts", "deliveryCursors" }), a[2], copy(a[3]), a[4] }
  end,
  ["economy_feeder_access.buildIndex"] = function(a)
    return { n = a.n, pick(a[1], { "markets", "services" }) }
  end,
  ["economy_town_demand.advance"] = function(a)
    return { n = a.n, pick(a[1], { "towns", "markets" }), { markets = copy(a[2].markets) } }
  end,
  ["economy_town_demand.refreshMarkets"] = function(a)
    return { n = a.n, pick(a[1], { "towns", "markets" }) }
  end,
  ["economy_town_demand.observeMarket"] = function(a)
    return { n = a.n, pick(a[1], { "towns" }), copy(a[2]) }
  end,
  ["economy_town_demand.upsertTown"] = function(a)
    return { n = a.n, pick(a[1], { "towns" }), a[2], a[3] }
  end,
  ["economy_town_demand.carriedByTown"] = function(a)
    return { n = a.n, pick(a[1], { "markets" }), { markets = copy(a[2] and a[2].markets) } }
  end,
  ["economy.walletDeltaDollars"] = function(a)
    return { n = a.n, pick(a[1], { "payoutResidCents" }), a[2], a[3] }
  end,
}

-- Functions whose replay also needs their arguments after the call.
local MUTATES = {
  ["economy_flow.evaluateMarket"] = true,
  ["economy_town_demand.advance"] = true,
  ["economy_town_demand.refreshMarkets"] = true,
  ["economy_town_demand.observeMarket"] = true,
  ["economy_town_demand.upsertTown"] = true,
  ["economy.walletDeltaDollars"] = true,
}

-- Call `fn` and return a record of the call, and its results.
local function record(label, fn, ...)
  local capture = CAPTURE[label] or copy
  local args = pack(...)
  local before = capture(args)
  local results = pack(fn(...))
  return {
    label = label, args = before, results = copy(results),
    after = MUTATES[label] and capture(args) or nil,
  }, results
end

function H.hook(label, fn)
  assert(type(fn) == "function", "no function to hook for " .. label)
  assert(H.originals[label] == nil, "hooked twice: " .. label)
  H.originals[label] = fn
  return function(...)
    if not H.tracing then return fn(...) end
    local entry, results = record(label, fn, ...)
    H.trace[#H.trace + 1] = entry
    return unpack(results, 1, results.n)
  end
end

-- Call the original of a hooked function and return the record of the call.
function H.run(label, ...)
  local fn = assert(H.originals[label], "no hooked function " .. label)
  return (record(label, fn, ...))
end

function H.hookLocal(module, name, fn)
  return H.hook(module .. "." .. name, fn)
end

local MODULE_HOOKS = {
  { "economy_flow", { "generalizedCost", "evaluateMarket" } },
  { "economy_allocation", { "proportional", "capacityConstrained" } },
  { "economy_revenue", { "saturatingMultiply", "defaultFareCents",
    "passengerDeliveryCents", "modelDeliveryCents" } },
  { "economy_costs", { "vehicleAnnualUpkeepCents", "infrastructureAnnualUpkeepCents",
    "hourlyCharge", "periodCharge", "charge", "allocateCapital" } },
  { "economy_difficulty", { "normaliseKey", "multiplier", "apply" } },
  { "economy_town_demand", { "marketSizeFromBuildings", "gravityDemand",
    "observeMarket", "refreshMarkets", "advance" } },
  { "economy_feeder_access", { "buildIndex", "cents" } },
  { "economy", { "walletDeltaDollars" } },
}

-- Load every module and wrap its ported functions. Other modules keep
-- calling through the module tables, so they reach the wrappers.
function H.install()
  for _, entry in ipairs(MODULE_HOOKS) do
    local module = require("tpf2_mp/" .. entry[1])
    for _, name in ipairs(entry[2]) do
      module[name] = H.hook(entry[1] .. "." .. name, module[name])
    end
  end
end

-- Run TPF2MP's unmodified parity-vector generator with tracing on. The
-- generator writes its vectors to the file named by arg[2]; they are
-- captured from its `json.encode` call instead, and nothing touches the
-- file system.
function H.runParityVectors(generator)
  local json = require "tpf2_mp/json"
  local encode, open, print_ = json.encode, io.open, print
  local vectors
  json.encode = function(value)
    if type(value) == "table" and value.scenarios ~= nil then vectors = value end
    return encode(value)
  end
  io.open = function()
    return { write = function() end, close = function() end }
  end
  print = function(...) H.printed = table.concat({ ... }, " ") end
  arg = { "tf2mod", "economy-parity-vectors.json" }
  H.tracing = true
  local ok, failure = pcall(generator)
  H.tracing = false
  json.encode, io.open, print, arg = encode, open, print_, nil
  assert(ok, failure)
  return assert(vectors, "the generator produced no vectors")
end

return H
