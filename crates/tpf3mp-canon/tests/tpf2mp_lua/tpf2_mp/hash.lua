local json = require "tpf2_mp/json"

local M = {}

function M.adler32(text)
  local a, b = 1, 0
  for index = 1, #text do
    a = (a + string.byte(text, index)) % 65521
    b = (b + a) % 65521
  end
  return b * 65536 + a
end

function M.hex(number)
  return string.format("%08x", number)
end

function M.text(text)
  return M.hex(M.adler32(text))
end

function M.value(value)
  return M.text(json.encode(value))
end

return M
