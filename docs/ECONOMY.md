# Economy: the TPF2MP port

`tpf3mp-canon` starts its economy from TPF2MP's (see "Economy and custom rules"
in [ARCHITECTURE.md](ARCHITECTURE.md)). This document describes the port of
TPF2MP's deterministic economy core to Rust, and how it is proven identical
to the Lua original.

- **Source:** `tf2mod` (TPF2MP by Julian Cooper, MIT), commit
  `58da402ba144b2b5ad9f5615f7d686aadd34ffb2`, economy model version 10.
- **Code:** `crates/tpf3mp-canon/src/economy/` and `crates/tpf3mp-canon/src/lua.rs`.
- **Guarantee:** every ported function returns exactly what the Lua original
  returns, or `None` where Lua's own arithmetic would leave the range in which
  doubles are exact, or divide by zero. It never returns a different number.
- **Evidence:** differential tests that run the original Lua under Lua 5.1 next
  to the port (`crates/tpf3mp-canon/tests/differential/`):
  - TPF2MP's parity-vector generator, run unmodified: all 46,048 calls it makes
    to ported functions across its 109 scenarios replay identically, including
    594 complete `evaluateMarket` settlements;
  - 237,344 generated property-test cases per run;
  - explicit edge cases.

## What is ported

All of it is integer arithmetic on `i64`. Money is in cents and shares in
parts per million (`SHARE_SCALE = 1_000_000`).

### `economy_flow.lua` → `economy::flow`

| Lua | Rust | notes |
|---|---|---|
| `EXP_TABLE` | `EXP_TABLE` | verified against Lua and `round(65536·e^(−k/10))` |
| `generalizedCost` | `generalized_cost` | factors in `GeneralizedCost` |
| local `logitWeight` | `logit_weight` | the exported `M.logitWeight` is `cutoff_weight = 0` |
| `cutoff` in `evaluateMarket` | `logit_cutoff_weight` | 0 from model version 3, 1 before |
| local `scaledRate` | `scaled_rate` | |
| local `glide` | `glide` | |
| share loop of `evaluateMarket` | `move_share`, `outside_share_ppm` | includes the v3 fare-shock latch |
| interval clamp of `evaluateMarket` | `period_seconds` | |
| `lagLoadPpm`, `shareBasisPoints` | `lag_load_ppm`, `share_basis_points` | |
| local `signedAdd` | `settlement::signed_add` | |

### `economy_allocation.lua` → `economy::allocation`

| Lua | Rust | notes |
|---|---|---|
| `proportional` | `proportional` | generic over the id type; its `Ord` decides ties |
| `capacityConstrained` | `capacity_constrained` | v9 queue and the legacy re-split |

### `economy_revenue.lua` → `economy::revenue`

| Lua | Rust |
|---|---|
| constants | `PASSENGER_COHORT_SCALE`, `CARGO_CENTS_PER_UNIT_KM`, … |
| `saturatingMultiply` | `saturating_multiply` |
| `defaultFareCents` | `default_fare_cents` |
| `passengerDeliveryCents` | `passenger_delivery_cents` |
| `modelDeliveryCents` | `model_delivery_cents` |

### `economy_costs.lua` → `economy::costs`

| Lua | Rust |
|---|---|
| constants | `HOURS_PER_YEAR`, `FINANCIAL_YEAR_SECONDS`, … |
| `vehicleAnnualUpkeepCents` | `vehicle_annual_upkeep_cents` |
| `infrastructureAnnualUpkeepCents` | `infrastructure_annual_upkeep_cents` |
| `hourlyCharge`, `periodCharge`, `charge` | `hourly_charge`, `period_charge`, `charge` |
| `allocateCapital` | `allocate_capital` |

### `economy_difficulty.lua` → `economy::difficulty`

| Lua | Rust |
|---|---|
| `PRESETS`, `ORDER`, `DEFAULT_KEY` | `Difficulty` and its `ORDER`, `DEFAULT`, `key`, `label`, `revenue_multiplier_ppm` |
| `normaliseKey`, `preset`, `multiplier` | `Difficulty::from_key` |
| `apply` (and `preview`) | `apply` |

### `economy_town_demand.lua` → `economy::town_demand`

| Lua | Rust | notes |
|---|---|---|
| constants | `NOMINAL_CAPACITY_PER_BUILDING`, … | |
| `marketSizeFromBuildings` | `market_size_from_buildings` | |
| `gravityDemand` | `gravity_demand` | |
| local `upsertTown` | `observe_town` | the record rules; the table update is the caller's |
| local `carriedByTown` | `carried_by_town` | |
| growth step of `advance` | `grow_town` | |
| per-market body of `refreshMarkets` | `refresh_market_demand` | |

### `economy_feeder_access.lua` → `economy::feeder_access`

| Lua | Rust |
|---|---|
| `CENTS_PER_ENDPOINT` | `CENTS_PER_ENDPOINT` |
| `buildIndex` | `build_index` |
| `cents` | `cents` |

### `economy.lua` → `economy::settlement`

| Lua | Rust |
|---|---|
| local `saturatingAdd`, `saturatingMultiply`, `signedAdd` | `saturating_add`, `saturating_multiply`, `signed_add` |
| `walletDeltaDollars` | `wallet_delta_dollars` |
| `modelValueCents` in `scoreboard` | `model_value_cents` |

### `util.lua` and Lua itself → `tpf3mp_canon::lua`

| Lua | Rust |
|---|---|
| `util.clamp` | `lua::clamp` |
| `+ - *`, `math.floor(a / b)`, `a % b` on integers | `lua::add`, `sub`, `mul`, `floor_div`, `modulo` |

## What is not ported, and why

- **`evaluateMarket`, `evaluateAll`, `recordSettlement`, `scoreboard`,
  `acceptAuthoritativeResults`.** These read and write TPF2MP's state layout
  and ledgers. TPF3-MP's canonical state model does not exist yet, and porting
  them would fix one. The port supplies the building blocks. The recipe below
  composes them, and the tests prove that the recipe reproduces
  `evaluateMarket` field for field.
- **`delivery` and `delivery_snapshot.lua`.** These are completed-trip ledgers
  fed by the game's presentation layer, with monotonic cursors. The recipe
  covers the model-delivery path, the one the parity vectors exercise. How
  TPF3 reports completed trips is a release-day question.
- **`migrate`, `upsertMarket`, `upsertService`, `setFare` and the other state
  mutators.** These are schema migration and TPF2MP's input clamps. The clamps
  define the ranges the port is designed and tested for (see below).
- **`multihop_*.lua`, `transport_network_graph.lua`, `freight_path_pin.lua`.**
  These do multi-hop route search, a separate subsystem that adds network
  demand to markets from model version 7. They are loaded in the tests only
  so that the generator runs unmodified.
- **`hash.lua`, `json.lua`.** TPF3-MP has its own digests and wire format.
- **`util.integer` and `tonumber` coercion.** The port takes typed integers.
  Coercing strings, fractions, NaN or infinities has no counterpart. On
  integers, `util.integer` is the identity, which a test checks. TPF2MP floors
  distances to whole metres before they reach the economy (`routeMeters`), so
  no fraction is lost.
- **The runtime modules** (`economy_clock_runtime`, `economy_action_runtime`,
  `economy_line_registration`, `economy_public_view`, …). These are engine
  glue.

### `nil` and `Option`

An `Option` parameter marks a value that TPF2MP state can legitimately lack.
The port applies Lua's default:

| value | missing means |
|---|---|
| market `waitWeightPm`, `transferSeconds` (before model version 4) | 2000, the ruleset's transfer time |
| feeder access cents (before model version 8) | no access model; factors omit the split |
| service `sharePpm` | adopt the equilibrium |
| service `lastFareCents` | a fare shock |
| cargo `distanceMeters` | 1000 m |
| passenger distance for the default fare | 0 m |
| a building count, town sizes, corridor length | 50 buildings, 200, 1000 m |
| market `networkDemand`, `directDemand` | 0; demand minus network demand |
| settlement interval | 300 s |

Other values are plain `i64`. TPF2MP's upserts always set them, and the
parity replay fails if one is ever missing. Metadata such as scopes, carriers
and towns is `Option<String>`.

## Exactness: matching Lua's doubles with integers

Lua 5.1 stores every number as an IEEE-754 double. A double holds every
integer of magnitude up to 2^53 exactly. On such integers:

- **`+`, `-`, `*`** are exact while the result stays in range;
- **`math.floor(a / b)`** is exact for `|a| ≤ 2^53`. The quotient may round,
  but it could only round across an integer if it lay within `1/|b|` of it,
  which requires `|a| > 2^53`;
- **`a % b`** is Lua 5.1's `a - math.floor(a / b) * b`. It is exact when
  `|a| + |b| ≤ 2^53`, because the product then lies within `|b|` of `a`.

`tpf3mp_canon::lua` computes in `i64` and returns `None` as soon as an
operand or result leaves `±(2^53 − 1)`, TPF2MP's own `MAX_EXACT_INTEGER`.
Outside that range Lua's results are not just rounded but platform-dependent:
a compiler may contract `a - floor(a / b) * b` into a fused multiply-subtract.
So refusing is the only answer every platform can agree on.

Some functions are exact for **any** operand. They either clamp before any
arithmetic that could round, or round only where the result no longer
depends on it: a rounded sum is already beyond the clamp, and a distance long
enough to round gives a zero gravity quotient. They are:

- `revenue::saturating_multiply`, `passenger_delivery_cents`;
- the whole `settlement` module;
- `difficulty::apply`, `costs::infrastructure_annual_upkeep_cents`;
- `town_demand::market_size_from_buildings`, `gravity_demand`.

The tests check them with every whole double up to 2^62. TPF2MP relies on
this: its scoreboard adds `net * 10`, up to 10^16.

### Where TPF2MP's own clamps admit inexact arithmetic

| function | refused when | reachable within TPF2MP's clamps? |
|---|---|---|
| `allocation::proportional` in capacity admission | riders per interval × share ppm > 2^53 − 1 | yes: about 9 × 10^9 riders per interval, e.g. 3.75 × 10^8 per hour with day-long intervals |
| `flow::lag_load_ppm` | riders chosen × 10^6 > 2^53 − 1 | yes, the same regime |
| `costs::vehicle_annual_upkeep_cents` | price > 90,071,992,547,409 dollars | yes: TPF2MP clamps the price at 10^15 and then multiplies by 100 |
| `revenue::default_fare_cents` | distance > 60,047,995,031,603 m | yes: the distance is not clamped |
| `revenue::model_delivery_cents` (cargo) | distance > 2^53 − 1 m | yes: the distance is not clamped |
| `scaled_rate`, `glide`, `logit_weight`, `generalized_cost`, `period_charge`, `hourly_charge` | only for inputs outside TPF2MP's clamps, e.g. an interval over 27 hours | no |
| `generalized_cost` | crowd threshold of 10^6 ppm (Lua divides 0 by 0 and prices the service at 1 cent) | only through ruleset parameters |
| `logit_weight` | `theta_cents == 0` (Lua divides by zero) | no: theta ≥ 50 |

The tests show that Lua really does go wrong in the first and third rows.
Past 2^54 a double holds only multiples of four, so `total * 999998` rounds.
The largest-remainder rider then goes to the wrong option
(`allocation::lua_misallocates_once_products_pass_two_to_the_53`). Vehicle
upkeep is off by a cent once the quotient passes 2^52
(`costs::vehicle_upkeep_past_the_exact_range_rounds_in_lua`).

TPF2MP's input clamps (`upsertMarket`, `upsertService`, the scheduler) define
the designed ranges:

| value | range |
|---|---|
| hourly demand, capacity | 0 to 10^9 |
| value of time | 30 to 10^5 cents per hour |
| outside cost | 1 to 10^8 cents |
| theta | 50 to 10^6 cents |
| wait weight | 0 to 10^4 per mille |
| transfer time | 0 to 14,400 s |
| headway | 30 to 86,400 s |
| journey | 30 to 604,800 s |
| fare | 0 to 10^8 cents |
| quality | 0 to 1000 |
| transfers | 0 to 8 |
| annual upkeep | 0 to 10^15 cents |
| settlement interval | 60 to 86,400 s |

## Semantic traps

1. **Floor, not truncation.** Lua floors every quotient. Negative quotients
   occur in two places: `glide` on a falling share (−2500 / 1000 steps by −3),
   and the logit interpolation (`(right − left) * fraction` is negative, since
   the table falls). Rust's `/` rounds toward zero.
2. **`%` takes the divisor's sign** in Lua (`-7 % 3 == 2`) and the dividend's
   in Rust (`-7 % 3 == -1`). `lua::modulo` mirrors Lua.
3. **`walletDeltaDollars` deliberately truncates.** A one-cent loss stays a
   carried cent, not a one-dollar debit; the residual keeps the sign. Here
   Rust's `/` and `%` are right, and `floor_div` would be wrong.
4. **`util.clamp` tolerates crossed bounds.** A value below `low` gives `low`,
   anything else `high`. Rust's `i64::clamp` panics instead. With a crowd
   threshold above the scale, `generalizedCost` clamps against a negative span.
5. **`a and b or c` and truthiness.** In Lua, `0` is true, so `tonumber(d) or
   1000` keeps a distance of 0. `enabled` has two readings:
   - feeder access tests `enabled ~= false`, so a missing flag counts as
     enabled;
   - `evaluateMarket` and `scoreboard` test `service.enabled`, so a missing
     flag counts as disabled.
6. **String order.** Lua 5.1 compares strings with `strcoll`, which is
   bytewise only in the C locale. Ids decide largest-remainder ties and every
   sorted iteration. Rust's `str` order is bytewise, and a test pins the
   interpreter to it. The outside option `"~outside"` sorts after `line:` ids,
   which settles its ties. `string.lower` folds ASCII only, and so does
   `Difficulty::from_key`.
7. **Negative zero.** `signedAdd(gross, -charge)` passes −0 when the charge
   is zero. Lua treats it as 0, and TPF2MP's JSON encoder writes it as `0`
   because Windows Lua would print `-0`. The tests read −0 as 0; the port has
   no negative zero.
8. **Table aliasing.** When a market's two towns are the same town,
   `observeMarket` upserts one record twice. Both `townSizeA` and `townSizeB`
   then hold the record's final size.
9. **Duplicate ids** in `proportional` overwrite the base allocation but keep
   both entries' leftover units, as a Lua table does. The port mirrors this.
10. **Legacy admission can drop demand.** Before model version 9, a market
    whose shares and outside option all weigh zero allocates nothing.
11. **Model versions.** The port keeps every version gate the parity vectors
    exercise:
    - v3: zero cutoff weight and the fare-shock latch;
    - v4: kind-specific weights;
    - v6: intervals, scaled rates and interval charges;
    - v7: difficulty and town growth;
    - v8: feeder access;
    - v9: queued capacity overflow;
    - v10 changes only migration.
12. **Delivered, then allocated.** Town growth counts `delivered or allocated`
    per service row.

## Recipe: `evaluateMarket` from the building blocks

For one market, without a delivery ledger. The market replay test
(`tests/differential/market.rs`) is this recipe in code:

1. `version` is the state's model version (1 if absent), and
   `period = flow::period_seconds(interval, version)`.
2. From v6: `(demand, market.demandResid) = scaled_rate(market.demand,
   market.demandResid, period)`. Before: `demand = market.demand`.
3. From v8: `index = feeder_access::build_index(all markets, all services)`.
4. For each service in line-id order that is enabled (a missing flag counts
   as disabled here) and belongs to the market:
   - from v8, `access = feeder_access::cents(market, service, index)`;
   - `cost = generalized_cost(params, market, service, access cents)`;
   - from v6, `(available, service.capacityResid) = scaled_rate(service.capacity,
     service.capacityResid, period)`. Before: `available = service.capacity`.
5. `gc_min` is the minimum of the outside cost and every service's cost.
6. `weights`: the outside option `"~outside"`, then each service, all with
   `logit_weight(gc, gc_min, theta, logit_cutoff_weight(version))`.
7. `equilibria = allocation::proportional(SHARE_SCALE, weights)`.
8. For each service, in order: `move_share(stock, equilibria[id] or 0, fare,
   params, version)`.
9. `outside = outside_share_ppm(shares)`.
10. `admitted = capacity_constrained(demand, (id, share, available)…,
    outside, version)`.
11. For each service, with `allocated = admitted[id] or 0`:
    - `requested` is `admitted.requested[id] or 0` from v9, and `allocated`
      before;
    - `lagLoadPpm = lag_load_ppm(requested, available)`;
    - `raw = model_delivery_cents(kind, distance, fare, allocated)`;
    - from v7: `(gross, service.revenueMultiplierResid) = difficulty::apply(raw,
      params.revenueMultiplierPpm, residual)`;
    - `managed` is whether any listed vehicle has a cost record. If so, the
      service charges nothing and keeps its upkeep residual;
    - otherwise `(charge, service.upkeepResid) = costs::charge(annual, residual,
      period, version)`;
    - `net = signed_add(gross, -charge)`,
      `shareBasisPoints = share_basis_points(allocated, demand)`, and from v9
      `capacityOverflow = max(0, requested − allocated)`.

Town growth (`advance`) works the same way:

1. Compute `carried_by_town` over passenger markets with two known towns.
2. For each town in id order, apply `grow_town` (a town without a record
   starts from `observe_town(None, None)`).
3. For each market whose two towns have records, apply
   `refresh_market_demand` with the grown sizes.

## The differential tests

### How they work

- **Fixtures.** `tests/tpf2mp_lua/` holds byte-identical copies of the 17
  modules `economy.lua` needs and of the parity-vector generator. Its README
  gives provenance, and its `.gitattributes` stops line-ending conversion.
- **Interpreter.** `mlua` with vendored Lua 5.1, a dev-dependency only. The
  version matters: TPF2MP's rules assume all numbers are doubles, which Lua
  5.3 and later no longer guarantee.
- **Hooks.** mlua refuses Lua's `debug` library outside unsafe mode, so the
  harness reaches local functions differently. When loading a module, it
  inserts a block before the final `return M` that reassigns the named locals
  to hooked versions, after all definitions. For example:
  `logitWeight = TPF3MP_HARNESS.hookLocal("economy_flow", "logitWeight", logitWeight)`.
  Module functions are wrapped after loading. A hook records copies of the
  arguments and results, and for mutating functions the arguments after the
  call, only while tracing is on.
- **One checker per function.** Each test module has a `CHECKS` table that
  compares one recorded call with the port. The result is `Matched`, or
  `Refused` when the port returns `None`; any difference fails with the
  inputs. Property tests and parity replay share these checkers.
  `every_hooked_function_has_a_check` keeps the tables complete.
- **Parity vectors.** The generator runs unmodified, with `arg`, `io.open` and
  `print` stubbed. Its vectors are captured from its own `json.encode` call.
  Every recorded call must be `Matched`, and every scoreboard row is checked.
  The set of functions the scenarios reach is pinned. The scenarios use model
  versions 2 and 10 only, so the gates between them rest on the property
  tests, which draw the version from 1 to 10.
- **Property tests** cover three kinds of input:
  - TPF2MP's designed ranges;
  - wider ranges, including negatives and crossed clamps;
  - whole doubles up to 2^62 for the clamping functions.

  They also cover random states:
  - whole markets through `evaluateMarket`, including 20 successive
    settlements carrying Lua's own residuals;
  - town worlds through `observeMarket`, `refreshMarkets` and `advance`;
  - feeder worlds.

  Seeds are random. A failure prints the shrunk input.
- **Edge cases** cover:
  - the pinned `EXP_TABLE` against its definition;
  - division by zero;
  - crossed clamp bounds;
  - negative zero;
  - the C-locale string order;
  - `util.integer` on integers;
  - the live misallocation past 2^53.

### Running them

```
cargo test -p tpf3mp-canon --test differential
cargo test -p tpf3mp-canon --test differential -- parity_vectors --nocapture
```

The second command prints the parity replay's call count per function. The
suite takes a few seconds.

## Adding vectors and functions

- **A TPF2MP scenario.** Add it to `tests/run_economy_parity_vectors.lua` in
  `tf2mod`, then copy the generator (and any changed modules) into
  `tests/tpf2mp_lua/`. Record the new commit in the fixture README, in
  `src/economy/mod.rs` and here. If the scenario reaches a function that no
  scenario reached before, add it to `REACHED` in `parity_vectors.rs`.
- **A TPF3-MP case.** In the test module for the Lua module, call
  `Tpf2mp::run(label, args)` to run the original and record the call. Pass
  the record to the module's `check_*` function. `Tpf2mp::record` builds Lua
  tables from optional integers.
- **A newly ported function.**
  1. Write the Rust function.
  2. Hook the Lua function: add it to `MODULE_HOOKS` in `harness.lua`, or, for
     a local function, to `LOCALS` in `tpf2mp.rs`.
  3. Add a checker to the module's `CHECKS`.
  4. Add property tests.

  The completeness test fails until the hook and the checker exist.
- **A new `tf2mod` commit.** Copy all fixtures again and run the suite. Any
  rule change shows up as a failing call with its inputs, and the module hook
  fails loudly if a local function was renamed.

## Open questions

- **Clamp policy.** In the regimes of the table above, TPF2MP's clamps admit
  arithmetic that its own Lua gets wrong, and the port refuses. For TPF3
  rulesets, the options are:
  - tighten the clamps: hourly demand at most 10^8 keeps every product exact
    at any interval, and distances and prices could be capped;
  - treat `None` as a hard settlement fault.
- **Settlement and state.** `evaluateAll`, the ledgers and the scoreboard
  should be ported as state-machine code once the canonical state model
  exists. The recipe and the market replay test are ready to anchor them.
- **Legacy model versions.** New rooms could pin model version 10 and drop the
  v2 to v9 paths. They are ported because the parity vectors exercise them.
