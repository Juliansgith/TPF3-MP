local util
do
  local ok, value = pcall(require, "tpf2_mp/util")
  if ok then
    util = value
  else
    -- The exact-build disposable probe copies this codec under an isolated
    -- namespace so it can validate the hook without enabling the full mod.
    util = require "tpf2_mp_probe/util"
  end
end

local M = {}

-- Lua has only tables, so an empty JSON array and an empty JSON object would
-- otherwise both decode to `{}`.  Remember empty arrays by identity while the
-- decoded tree is alive.  The weak keys do not retain protocol messages, and
-- the marker is deliberately consulted only while the table is still empty.
local decodedEmptyArrays = setmetatable({}, { __mode = "k" })

local escapes = {
  ["\b"] = "\\b",
  ["\f"] = "\\f",
  ["\n"] = "\\n",
  ["\r"] = "\\r",
  ["\t"] = "\\t",
  ["\""] = "\\\"",
  ["\\"] = "\\\\",
}

local function encodeString(value)
  return '"' .. value:gsub('[%z\1-\31\\"]', function(char)
    return escapes[char] or string.format("\\u%04x", string.byte(char))
  end) .. '"'
end

local function encodeNumber(value)
  if value ~= value or value == math.huge or value == -math.huge then
    error("JSON cannot encode a non-finite number")
  end
  -- JSON has only one zero value. Windows Lua preserves the IEEE-754 sign in
  -- string.format and would otherwise emit "-0" for transforms containing a
  -- negative zero. JSON decoders are allowed to erase that sign, which made a
  -- valid game envelope fail its cross-language checksum after parsing.
  if value == 0 then return "0" end
  if value == math.floor(value) then return string.format("%.0f", value) end
  return string.format("%.17g", value)
end

local encodeValue

local function encodeTable(value, stack)
  if stack[value] then error("JSON cannot encode cyclic tables") end
  stack[value] = true

  local array, length = util.isArray(value)
  if not array and next(value) == nil and decodedEmptyArrays[value] then
    array, length = true, 0
  end
  local parts = {}
  if array then
    for index = 1, length do parts[index] = encodeValue(value[index], stack) end
    stack[value] = nil
    return "[" .. table.concat(parts, ",") .. "]"
  end

  for _, key in ipairs(util.sortedKeys(value)) do
    if type(key) ~= "string" then error("JSON object keys must be strings") end
    parts[#parts + 1] = encodeString(key) .. ":" .. encodeValue(value[key], stack)
  end
  stack[value] = nil
  return "{" .. table.concat(parts, ",") .. "}"
end

encodeValue = function(value, stack)
  local kind = type(value)
  if value == nil then return "null" end
  if kind == "boolean" then return value and "true" or "false" end
  if kind == "number" then return encodeNumber(value) end
  if kind == "string" then return encodeString(value) end
  if kind == "table" then return encodeTable(value, stack) end
  error("JSON cannot encode type " .. kind)
end

function M.encode(value)
  return encodeValue(value, {})
end

local function utf8(code)
  if code <= 0x7f then return string.char(code) end
  if code <= 0x7ff then
    return string.char(0xc0 + math.floor(code / 0x40), 0x80 + (code % 0x40))
  end
  if code <= 0xffff then
    return string.char(
      0xe0 + math.floor(code / 0x1000),
      0x80 + (math.floor(code / 0x40) % 0x40),
      0x80 + (code % 0x40)
    )
  end
  return string.char(
    0xf0 + math.floor(code / 0x40000),
    0x80 + (math.floor(code / 0x1000) % 0x40),
    0x80 + (math.floor(code / 0x40) % 0x40),
    0x80 + (code % 0x40)
  )
end

local function decoder(text)
  local pos, length = 1, #text

  local function fail(message)
    error("JSON decode error at byte " .. pos .. ": " .. message)
  end

  local function skipWhitespace()
    while pos <= length and text:sub(pos, pos):match("%s") do pos = pos + 1 end
  end

  local parseValue

  local function parseString()
    pos = pos + 1
    local parts, start = {}, pos
    while pos <= length do
      local char = text:sub(pos, pos)
      if char == '"' then
        parts[#parts + 1] = text:sub(start, pos - 1)
        pos = pos + 1
        return table.concat(parts)
      elseif char == "\\" then
        parts[#parts + 1] = text:sub(start, pos - 1)
        pos = pos + 1
        local escaped = text:sub(pos, pos)
        local simple = { ['"']='"', ["\\"]="\\", ["/"]="/", b="\b", f="\f", n="\n", r="\r", t="\t" }
        if simple[escaped] then
          parts[#parts + 1] = simple[escaped]
          pos = pos + 1
        elseif escaped == "u" then
          local hex = text:sub(pos + 1, pos + 4)
          if #hex ~= 4 or not hex:match("^%x%x%x%x$") then fail("invalid unicode escape") end
          parts[#parts + 1] = utf8(tonumber(hex, 16))
          pos = pos + 5
        else
          fail("invalid string escape")
        end
        start = pos
      elseif string.byte(char) < 32 then
        fail("control character in string")
      else
        pos = pos + 1
      end
    end
    fail("unterminated string")
  end

  local function parseNumber()
    local start = pos
    while pos <= length and text:sub(pos, pos):match("[-+0-9eE%.]") do pos = pos + 1 end
    local raw = text:sub(start, pos - 1)
    local value = tonumber(raw)
    if not value then fail("invalid number") end
    return value
  end

  local function parseArray()
    pos = pos + 1
    skipWhitespace()
    local result = {}
    if text:sub(pos, pos) == "]" then
      pos = pos + 1
      decodedEmptyArrays[result] = true
      return result
    end
    while true do
      result[#result + 1] = parseValue()
      skipWhitespace()
      local char = text:sub(pos, pos)
      if char == "]" then pos = pos + 1; return result end
      if char ~= "," then fail("expected ',' or ']'") end
      pos = pos + 1
      skipWhitespace()
    end
  end

  local function parseObject()
    pos = pos + 1
    skipWhitespace()
    local result = {}
    if text:sub(pos, pos) == "}" then pos = pos + 1; return result end
    while true do
      if text:sub(pos, pos) ~= '"' then fail("expected object key") end
      local key = parseString()
      skipWhitespace()
      if text:sub(pos, pos) ~= ":" then fail("expected ':'") end
      pos = pos + 1
      skipWhitespace()
      result[key] = parseValue()
      skipWhitespace()
      local char = text:sub(pos, pos)
      if char == "}" then pos = pos + 1; return result end
      if char ~= "," then fail("expected ',' or '}'") end
      pos = pos + 1
      skipWhitespace()
    end
  end

  parseValue = function()
    skipWhitespace()
    local char = text:sub(pos, pos)
    if char == '"' then return parseString() end
    if char == "{" then return parseObject() end
    if char == "[" then return parseArray() end
    if char == "-" or char:match("%d") then return parseNumber() end
    if text:sub(pos, pos + 3) == "true" then pos = pos + 4; return true end
    if text:sub(pos, pos + 4) == "false" then pos = pos + 5; return false end
    if text:sub(pos, pos + 3) == "null" then pos = pos + 4; return nil end
    fail("unexpected token")
  end

  local value = parseValue()
  skipWhitespace()
  if pos <= length then fail("trailing content") end
  return value
end

function M.decode(text)
  if type(text) ~= "string" then error("JSON input must be a string") end
  return decoder(text)
end

return M
