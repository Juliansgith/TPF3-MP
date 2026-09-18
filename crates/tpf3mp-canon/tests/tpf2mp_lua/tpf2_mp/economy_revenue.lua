local util = require "tpf2_mp/util"

local M = {}

M.ACCUMULATOR_LIMIT = 1000000000000000
M.PASSENGER_COHORT_SCALE = 1000
M.CARGO_CENTS_PER_UNIT_KM = 100000
M.CARGO_REFERENCE_FARE_CENTS = 1000
M.DEFAULT_PASSENGER_BASE_FARE_CENTS = 500
M.DEFAULT_PASSENGER_FARE_CENTS_PER_KM = 150

local function bounded(value)
  return math.min(M.ACCUMULATOR_LIMIT, math.max(0, util.integer(value, 0)))
end

function M.saturatingMultiply(left, right)
  left, right = bounded(left), bounded(right)
  if left == 0 or right == 0 then return 0 end
  if left > math.floor(M.ACCUMULATOR_LIMIT / right) then return M.ACCUMULATOR_LIMIT end
  return left * right
end

function M.defaultFareCents(distanceMeters, kind)
  if kind == "cargo" then return M.CARGO_REFERENCE_FARE_CENTS end
  local distance = math.max(0, util.integer(distanceMeters, 0))
  return M.DEFAULT_PASSENGER_BASE_FARE_CENTS
    + math.floor((distance * M.DEFAULT_PASSENGER_FARE_CENTS_PER_KM + 500) / 1000)
end

function M.passengerDeliveryCents(passengers, fareCents)
  return M.saturatingMultiply(
    M.saturatingMultiply(passengers, fareCents), M.PASSENGER_COHORT_SCALE)
end

function M.modelDeliveryCents(market, service, delivered)
  if market and market.kind == "cargo" then
    local metadata = service and service.metadata or {}
    local km = math.max(1, math.floor(math.max(0,
      util.integer(metadata.distanceMeters, 1000)) / 1000))
    local base = M.saturatingMultiply(M.saturatingMultiply(
      delivered, km), M.CARGO_CENTS_PER_UNIT_KM)
    return math.floor(M.saturatingMultiply(base,
      service and service.fareCents or M.CARGO_REFERENCE_FARE_CENTS)
      / M.CARGO_REFERENCE_FARE_CENTS)
  end
  return M.passengerDeliveryCents(delivered, service and service.fareCents or 0)
end

return M
