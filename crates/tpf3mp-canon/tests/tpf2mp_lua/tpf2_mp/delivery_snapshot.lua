local util = require "tpf2_mp/util"

local M = {}

M.SCHEMA_VERSION = 3

local function exactFields(value, fields)
  if type(value) ~= "table" then return false end
  local expected, count = {}, 0
  for _, field in ipairs(fields) do expected[field] = true end
  for key in pairs(value) do
    if not expected[key] then return false end
    count = count + 1
  end
  return count == #fields
end

local function validCid(value, prefix)
  return type(value) == "string" and #value <= 320
    and value:sub(1, #prefix + 1) == prefix .. ":"
end

local function exactInteger(value, low, high)
  return type(value) == "number" and value == math.floor(value)
    and value >= low and value <= high
end

function M.combine(passenger, cargo)
  passenger = type(passenger) == "table" and passenger or {}
  cargo = type(cargo) == "table" and cargo or {}
  local passengerEpoch = util.integer(passenger.presentationEpoch, 0)
  local cargoEpoch = util.integer(cargo.presentationEpoch, passengerEpoch)
  if passengerEpoch ~= cargoEpoch then return nil, "delivery ledgers disagree on economy epoch" end
  local result = {
    schemaVersion = M.SCHEMA_VERSION,
    presentationEpoch = passengerEpoch,
    passengerLines = util.deepCopy(passenger.lines or {}),
    cargoLines = util.deepCopy(cargo.lines or {}),
  }
  local valid, validationError = M.validate(result)
  if not valid then return nil, validationError end
  return result, nil
end

function M.validate(value)
  if type(value) ~= "table" then return false, "delivery snapshot is not a table" end
  if value.schemaVersion == 1 then
    if not exactFields(value, { "schemaVersion", "presentationEpoch", "lines" })
      or not exactInteger(value.presentationEpoch, 0, 1000000000)
      or type(value.lines) ~= "table" then
      return false, "legacy delivery snapshot header is invalid"
    end
    for lineCid, row in pairs(value.lines) do
      if not validCid(lineCid, "line") or not exactFields(
          row, { "deliveredPassengers", "earnedRevenueCents" })
        or not exactInteger(row.deliveredPassengers, 0, 1000000000)
        or not exactInteger(row.earnedRevenueCents, 0, 1000000000000000) then
        return false, "legacy delivery snapshot line is invalid"
      end
    end
    return true, value
  end
  if value.schemaVersion ~= 2 and value.schemaVersion ~= M.SCHEMA_VERSION
    or not exactFields(value, {
      "schemaVersion", "presentationEpoch", "passengerLines", "cargoLines" })
    or not exactInteger(value.presentationEpoch, 0, 1000000000)
    or type(value.passengerLines) ~= "table" or type(value.cargoLines) ~= "table" then
    return false, "delivery snapshot header is invalid"
  end
  for lineCid, row in pairs(value.passengerLines) do
    if not validCid(lineCid, "line") or not exactFields(
        row, { "deliveredPassengers", "earnedRevenueCents" })
      or not exactInteger(row.deliveredPassengers, 0, 1000000000)
      or not exactInteger(row.earnedRevenueCents, 0, 1000000000000000) then
      return false, "passenger delivery snapshot line is invalid"
    end
  end
  for lineCid, row in pairs(value.cargoLines) do
    local multihop = type(row) == "table" and row.transportSchema == 2
    local fields = multihop and {
      "transportSchema", "contractDigest", "pathDigest", "legIndex", "legCount",
      "sourceKind", "destinationKind", "sourceIndustryCid", "destinationIndustryCid",
      "sourceStationGroupCid", "destinationStationGroupCid", "destinationStockIndex",
      "cargoType", "boardedUnits", "deliveredUnits", "earnedRevenueCents",
    } or {
      "contractDigest", "sourceIndustryCid", "destinationIndustryCid",
      "destinationStockIndex", "cargoType", "boardedUnits",
      "deliveredUnits", "earnedRevenueCents",
    }
    if not validCid(lineCid, "line") or not exactFields(row, fields)
      or type(row.contractDigest) ~= "string" or not row.contractDigest:match("^[0-9a-f]+$")
      or #row.contractDigest ~= 8
      or not validCid(row.sourceIndustryCid, "industry")
      or not validCid(row.destinationIndustryCid, "industry")
      or row.sourceIndustryCid == row.destinationIndustryCid
      or not exactInteger(row.destinationStockIndex, 0, 31)
      or type(row.cargoType) ~= "string"
      or not row.cargoType:match("^[A-Z][A-Z0-9_]*$") or #row.cargoType > 128
      or not exactInteger(row.boardedUnits, 0, 1000000000)
      or not exactInteger(row.deliveredUnits, 0, 1000000000)
      or row.deliveredUnits > row.boardedUnits
      or not exactInteger(row.earnedRevenueCents, 0, 1000000000000000) then
      return false, "cargo delivery snapshot line is invalid"
    end
    if multihop and (type(row.pathDigest) ~= "string"
      or not row.pathDigest:match("^[0-9a-f]+$") or #row.pathDigest ~= 8
      or not exactInteger(row.legIndex, 0, 15)
      or not exactInteger(row.legCount, 1, 16) or row.legIndex >= row.legCount
      or (row.sourceKind ~= "industry" and row.sourceKind ~= "station")
      or (row.destinationKind ~= "industry" and row.destinationKind ~= "station")
      or not validCid(row.sourceStationGroupCid, "station_group")
      or not validCid(row.destinationStationGroupCid, "station_group")) then
      return false, "multi-hop cargo delivery identity is invalid"
    end
  end
  return true, value
end

function M.passengerLines(value)
  if type(value) ~= "table" then return {} end
  return value.schemaVersion == 1 and (value.lines or {}) or (value.passengerLines or {})
end

function M.cargoLines(value)
  if type(value) ~= "table" or value.schemaVersion == 1 then return {} end
  return value.cargoLines or {}
end

return M
