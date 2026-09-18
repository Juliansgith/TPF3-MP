local util = require "tpf2_mp/util"

local M = {}

M.SCALE = 1000000
M.ACCUMULATOR_LIMIT = 1000000000000000
M.DEFAULT_KEY = "normal"
M.ORDER = { "normal", "hard", "easy", "relaxed" }
M.PRESETS = {
  hard = { key = "hard", label = "Hard", revenueMultiplierPpm = 600000 },
  normal = { key = "normal", label = "Normal", revenueMultiplierPpm = 1000000 },
  easy = { key = "easy", label = "Easy", revenueMultiplierPpm = 1500000 },
  relaxed = { key = "relaxed", label = "Relaxed", revenueMultiplierPpm = 2000000 },
}

function M.normaliseKey(value)
  value = tostring(value or ""):lower()
  return M.PRESETS[value] and value or M.DEFAULT_KEY
end

function M.preset(value)
  return M.PRESETS[M.normaliseKey(value)]
end

function M.multiplier(value)
  return M.preset(value).revenueMultiplierPpm
end

-- Exact integer scaling without ever forming rawCents * multiplier as one
-- potentially imprecise Lua-double product. The sub-cent-in-ppm remainder is
-- authored service state, so many small payments equal one large payment.
function M.apply(rawCents, multiplierPpm, residual)
  local raw = util.clamp(util.integer(rawCents, 0), 0, M.ACCUMULATOR_LIMIT)
  local multiplier = util.clamp(util.integer(
    multiplierPpm, M.PRESETS.normal.revenueMultiplierPpm), 0, 4000000)
  local carried = util.clamp(util.integer(residual, 0), 0, M.SCALE - 1)
  local whole, remainder = math.floor(raw / M.SCALE), raw % M.SCALE
  local base = whole * multiplier
  if base >= M.ACCUMULATOR_LIMIT then return M.ACCUMULATOR_LIMIT, 0 end
  local tail = remainder * multiplier + carried
  local scaled = base + math.floor(tail / M.SCALE)
  if scaled >= M.ACCUMULATOR_LIMIT then return M.ACCUMULATOR_LIMIT, 0 end
  return scaled, tail % M.SCALE
end

function M.preview(rawCents, multiplierPpm, residual)
  local scaled = M.apply(rawCents, multiplierPpm, residual)
  return scaled
end

return M
