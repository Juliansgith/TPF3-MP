local util = require "tpf2_mp/util"

local M = {}

local function pathDigest(value)
  return type(value) == "string" and #value == 8
    and value:match("^[0-9a-f]+$") ~= nil
end

function M.pinPath(economyState, digest)
  if type(economyState) ~= "table" or not pathDigest(digest) then
    return false, "freight path pin identity is invalid"
  end
  local targets = {}
  for _, lineCid in ipairs(util.sortedKeys(economyState.services or {})) do
    local service = economyState.services[lineCid]
    local metadata = type(service) == "table" and service.metadata or nil
    if type(metadata) == "table" and metadata.freightPathDigest == digest then
      if metadata.freightContractSchema ~= 2
        or (metadata.freightPinnedPathDigest ~= nil
          and metadata.freightPinnedPathDigest ~= digest) then
        return false, "freight path pin conflicts with the active contract"
      end
      targets[#targets + 1] = { lineCid = lineCid, metadata = metadata }
    end
  end
  if #targets == 0 then return false, "freight path pin has no active legs" end
  for _, target in ipairs(targets) do
    target.metadata.freightPinnedPathDigest = digest
  end
  local lines = {}
  for _, target in ipairs(targets) do lines[#lines + 1] = target.lineCid end
  return true, { pathDigest = digest, lines = lines, pinned = #targets }
end

function M.pinLine(economyState, lineCid)
  local service = economyState and economyState.services
    and economyState.services[lineCid] or nil
  local metadata = service and service.metadata or nil
  if type(metadata) ~= "table" or metadata.freightContractSchema ~= 2 then
    return true, { lineCid = lineCid, pinned = 0 }
  end
  return M.pinPath(economyState, metadata.freightPathDigest)
end

function M.pinMoved(economyState, cargoLines)
  local paths = {}
  for _, lineCid in ipairs(util.sortedKeys(cargoLines or {})) do
    local row = cargoLines[lineCid]
    if type(row) == "table" and row.transportSchema == 2
      and (util.integer(row.boardedUnits, 0) > 0
        or util.integer(row.deliveredUnits, 0) > 0) then
      local service = economyState and economyState.services
        and economyState.services[lineCid] or nil
      local metadata = service and service.metadata or nil
      if type(metadata) ~= "table" or metadata.freightContractSchema ~= 2
        or metadata.freightPathDigest ~= row.pathDigest then
        return false, "moved freight path disagrees with its active service"
      end
      paths[row.pathDigest] = true
    end
  end
  local pinned = 0
  for _, digest in ipairs(util.sortedKeys(paths)) do
    local ok, result = M.pinPath(economyState, digest)
    if not ok then return false, result end
    pinned = pinned + result.pinned
  end
  return true, { paths = util.tableCount(paths), pinned = pinned }
end

return M
