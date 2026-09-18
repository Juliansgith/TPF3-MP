local util = require "tpf2_mp/util"

local M = {}

-- The native game quotes vehicle running costs per year, while infrastructure is
-- charged by this ruleset at ten percent of invested capital per year.  Keep
-- the conversion here so Lua, tests, and the companion replayer share one
-- explicit contract instead of scattering magic divisors through runtimes.
M.HOURS_PER_YEAR = 365 * 24
-- Competitive time is deliberately compressed relative to the stock calendar:
-- one financial year is three authored operating hours. This keeps exact
-- native annual maintenance values meaningful in a match without requiring
-- hundreds of real-time hours to feel their cost.
M.FINANCIAL_YEAR_SECONDS = 3 * 3600
-- Last-resort compatibility basis for an old/malformed vehicle record whose
-- resolved native MAINTENANCE_COST cannot be read. New purchases, replacements
-- and uniquely manifest-bound starting vehicles use the native annual value.
M.VEHICLE_PURCHASE_TO_ANNUAL_DIVISOR = 6
M.INFRASTRUCTURE_CAPITAL_TO_ANNUAL_DIVISOR = 10
M.ACCUMULATOR_LIMIT = 1000000000000000

local function nonNegativeInteger(value)
  return math.min(M.ACCUMULATOR_LIMIT, math.max(0, util.integer(value, 0)))
end

function M.vehicleAnnualUpkeepCents(purchasePriceDollars)
  local purchaseCents = nonNegativeInteger(purchasePriceDollars) * 100
  return math.floor(purchaseCents / M.VEHICLE_PURCHASE_TO_ANNUAL_DIVISOR)
end

function M.infrastructureAnnualUpkeepCents(capitalCents)
  return math.floor(nonNegativeInteger(capitalCents)
    / M.INFRASTRUCTURE_CAPITAL_TO_ANNUAL_DIVISOR)
end

-- Legacy integer residual carry makes 8,760 hourly charges add back to the exact
-- annual number.  Without it, inexpensive assets would lose up to $87.59 per
-- year independently on each service merely because cents cannot be split.
function M.hourlyCharge(annualCents, residual)
  local numerator = nonNegativeInteger(annualCents)
    + math.max(0, util.integer(residual, 0))
  return math.floor(numerator / M.HOURS_PER_YEAR), numerator % M.HOURS_PER_YEAR
end

-- Exact interval proration without multiplying a potentially large annual
-- aggregate by the complete interval in one IEEE-754 operation. The residual
-- is a numerator remainder in financial-year seconds.
function M.periodCharge(annualCents, residual, periodSeconds)
  local annual = nonNegativeInteger(annualCents)
  local period = math.max(0, util.integer(periodSeconds, 0))
  local quotient = math.floor(annual / M.FINANCIAL_YEAR_SECONDS)
  local remainder = annual % M.FINANCIAL_YEAR_SECONDS
  local tail = remainder * period + math.max(0, util.integer(residual, 0))
  local charge = quotient * period + math.floor(tail / M.FINANCIAL_YEAR_SECONDS)
  return math.min(M.ACCUMULATOR_LIMIT, charge), tail % M.FINANCIAL_YEAR_SECONDS
end

function M.charge(annualCents, residual, periodSeconds, economyVersion)
  if util.integer(economyVersion, 1) < 6 then
    return M.hourlyCharge(annualCents, residual)
  end
  return M.periodCharge(annualCents, residual, periodSeconds)
end

-- Allocate replacement/build capital across canonical output identities in a
-- stable order.  The sum is exact and independent of machine-local entity IDs.
function M.allocateCapital(cids, totalCents)
  local ordered, seen = {}, {}
  for _, cid in ipairs(cids or {}) do
    cid = tostring(cid or "")
    if cid ~= "" and not seen[cid] then
      seen[cid] = true
      ordered[#ordered + 1] = cid
    end
  end
  table.sort(ordered)
  local result = {}
  if #ordered == 0 then return result end
  local total = nonNegativeInteger(totalCents)
  local base, remainder = math.floor(total / #ordered), total % #ordered
  for index, cid in ipairs(ordered) do
    result[cid] = base + (index <= remainder and 1 or 0)
  end
  return result
end

return M
