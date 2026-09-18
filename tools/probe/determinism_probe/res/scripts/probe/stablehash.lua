-- probe/stablehash.lua -- deterministic, pure-Lua string hashing and number
-- formatting for the TPF3-MP probe mods. No engine dependency, no globals.
--
-- ASSUMPTION: the script VM is Lua 5.1/5.2-compatible (TPF2 is Lua 5.2.2). Only
-- string.byte/format, table.concat and integer arithmetic under 2^53 are used,
-- so the digest is identical on every 5.1+ VM and every platform -- which is the
-- whole point: two runs may only differ because the GAME differs, never because
-- the hash did. A weaker hash that collides would report agreement between
-- genuinely different worlds, the worst failure for a determinism probe.
--
-- (This file is intentionally identical to the copy in the script_api_dump-free
-- probe layout; each mod is self-contained so it can be installed on its own.)
local M = {}

-- Two Lehmer-style lanes (same construction as the prior art's mp/hash.lua).
-- Products stay below 2^53 so IEEE-754 double arithmetic is exact.
local M1, A1 = 2147483647, 48271
local M2, A2 = 2147483629, 40692

function M.hashStr(s)
  local h1, h2 = 2166136261 % M1, 2166136261 % M2
  for i = 1, #s do
    local b = string.byte(s, i)
    h1 = (h1 * A1 + b) % M1
    h2 = (h2 * A2 + b) % M2
  end
  return string.format("%010d-%010d", h1, h2)
end

-- Exact decimal for a number: %.17g round-trips an IEEE-754 double.
function M.num(x)
  if type(x) == "number" then return string.format("%.17g", x) end
  return tostring(x)
end

-- Hash an ordered list of strings, separated by 0x1E (record separator), which
-- never appears in the quantised numeric strings the probe builds.
function M.hashList(list)
  return M.hashStr(table.concat(list, "\30"))
end

return M
