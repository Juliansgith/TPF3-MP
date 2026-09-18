local M = {}

function M.sortedKeys(t)
  local keys = {}
  for key, _ in pairs(t or {}) do keys[#keys + 1] = key end
  table.sort(keys, function(a, b)
    if type(a) == type(b) then return a < b end
    return type(a) < type(b)
  end)
  return keys
end

function M.deepCopy(value, seen)
  if type(value) ~= "table" then return value end
  seen = seen or {}
  if seen[value] then return seen[value] end
  local copy = {}
  seen[value] = copy
  for key, item in pairs(value) do
    copy[M.deepCopy(key, seen)] = M.deepCopy(item, seen)
  end
  return copy
end

function M.tableCount(t)
  local count = 0
  for _ in pairs(t or {}) do count = count + 1 end
  return count
end

function M.setDifference(after, before)
  local result = {}
  for entity in pairs(after or {}) do
    if not (before or {})[entity] then result[#result + 1] = entity end
  end
  table.sort(result)
  return result
end

function M.clamp(value, low, high)
  if value < low then return low end
  if value > high then return high end
  return value
end

function M.integer(value, fallback)
  local number = tonumber(value)
  if not number or number ~= number or number == math.huge or number == -math.huge then
    return fallback or 0
  end
  if number >= 0 then return math.floor(number + 0.0000001) end
  return math.ceil(number - 0.0000001)
end

function M.isArray(t)
  if type(t) ~= "table" then return false, 0 end
  local max, count = 0, 0
  for key, _ in pairs(t) do
    if type(key) ~= "number" or key < 1 or key ~= math.floor(key) then return false, 0 end
    if key > max then max = key end
    count = count + 1
  end
  if count == 0 then return false, 0 end
  if max ~= count then return false, 0 end
  return true, max
end

function M.contains(list, value)
  for _, item in ipairs(list or {}) do
    if item == value then return true end
  end
  return false
end

function M.isCallable(value)
  local valueType = type(value)
  return valueType == "function" or valueType == "table" or valueType == "userdata"
end

function M.safeCall(fn, ...)
  if not M.isCallable(fn) then return false, "function unavailable" end
  return pcall(fn, ...)
end

-- Build 35924 documents api.cmd.make in GUI, engine, and console states, but
-- live probing shows that several actual mod states omit those fields. The
-- exact-build native hook mirrors the closures it observes during command API
-- registration into same-state, namespaced globals. Prefer the public table
-- and use the mirror only when the documented field is absent.
function M.commandFactory(name)
  if type(name) ~= "string" or name == "" then return nil, "invalid command factory name" end
  local make = api and api.cmd and api.cmd.make
  local public = make and make[name]
  if type(public) == "function" or type(public) == "table" or type(public) == "userdata" then
    return public, "public"
  end
  local mirrored = rawget(_G, "tpf2mp_native_binding_" .. name)
  if type(mirrored) == "function" or type(mirrored) == "table" or type(mirrored) == "userdata" then
    return mirrored, "native-mirror"
  end
  return nil, "unavailable"
end

-- The native hook invokes its pre-issue observer synchronously, before the
-- original api.cmd.sendCommand closure. Keep a same-Lua-state marker around
-- commands issued by this mod so capture evidence can distinguish them from
-- unmarked game/player traffic without changing the game's command ABI.
local COMMAND_ORIGIN_KEY = "__tpf2mp_command_origin"

function M.currentCommandOrigin()
  local value = rawget(_G, COMMAND_ORIGIN_KEY)
  if value == nil or value == "" then return nil end
  return tostring(value)
end

function M.sendCommand(command, callback, origin)
  local send = api and api.cmd and api.cmd.sendCommand
  if type(send) ~= "function" then return false, "api.cmd.sendCommand is unavailable" end
  local previous = rawget(_G, COMMAND_ORIGIN_KEY)
  rawset(_G, COMMAND_ORIGIN_KEY, tostring(origin or "mod.unspecified"))
  local ok, first, second, third
  if callback ~= nil then
    ok, first, second, third = pcall(send, command, callback)
  else
    ok, first, second, third = pcall(send, command)
  end
  rawset(_G, COMMAND_ORIGIN_KEY, previous)
  return ok, first, second, third
end

return M
