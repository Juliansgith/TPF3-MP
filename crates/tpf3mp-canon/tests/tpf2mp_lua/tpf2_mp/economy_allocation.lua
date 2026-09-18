local M = {}

-- Canonical largest-remainder proportional allocation. Ties are resolved by
-- canonical id so Lua and the companion replayer make the same indivisible
-- passenger/cargo decisions.
function M.proportional(total, items)
  local weight = 0
  for _, item in ipairs(items) do weight = weight + item.weight end
  local allocations, ranked, used = {}, {}, 0
  if total <= 0 or weight <= 0 then return allocations end
  for _, item in ipairs(items) do
    local numerator = total * item.weight
    local base = math.floor(numerator / weight)
    allocations[item.cid], used = base, used + base
    ranked[#ranked + 1] = { cid = item.cid, remainder = numerator % weight }
  end
  table.sort(ranked, function(a, b)
    return a.remainder == b.remainder and a.cid < b.cid or a.remainder > b.remainder
  end)
  for index = 1, total - used do
    local cid = ranked[((index - 1) % #ranked) + 1].cid
    allocations[cid] = allocations[cid] + 1
  end
  return allocations
end

local function choiceItems(services, outsidePpm)
  local items = { { cid = "~outside", weight = outsidePpm } }
  for _, option in ipairs(services) do
    items[#items + 1] = { cid = option.cid, weight = option.service.sharePpm }
  end
  return items
end

function M.capacityConstrained(demand, services, outsidePpm, version)
  local requested
  if version >= 9 then
    requested = M.proportional(demand, choiceItems(services, outsidePpm))
    local allocations = { ["~outside"] = requested["~outside"] or 0 }
    local queued = 0
    for _, option in ipairs(services) do
      local amount = requested[option.cid] or 0
      allocations[option.cid] = math.min(amount, option.availableCapacity)
      queued = queued + math.max(0, amount - allocations[option.cid])
    end
    return allocations, requested, queued
  end

  -- Legacy models immediately reallocated a capped service's rejected riders
  -- among surviving services and the outside option. Retain this path so old
  -- checkpoints remain independently replayable.
  local active, allocations, remaining = {}, {}, demand
  for _, option in ipairs(services) do active[#active + 1] = option end
  while remaining > 0 and #active > 0 do
    local preview = M.proportional(remaining, choiceItems(active, outsidePpm))
    local capped, survivors = {}, {}
    for _, option in ipairs(active) do
      if (preview[option.cid] or 0) > option.availableCapacity then
        capped[#capped + 1] = option
      else
        survivors[#survivors + 1] = option
      end
    end
    if #capped == 0 then
      for cid, amount in pairs(preview) do
        allocations[cid] = (allocations[cid] or 0) + amount
      end
      remaining = 0
    else
      for _, option in ipairs(capped) do
        allocations[option.cid] = option.availableCapacity
        remaining = remaining - option.availableCapacity
      end
      active = survivors
    end
  end
  if remaining > 0 then
    allocations["~outside"] = (allocations["~outside"] or 0) + remaining
  end
  return allocations, nil, 0
end

return M
