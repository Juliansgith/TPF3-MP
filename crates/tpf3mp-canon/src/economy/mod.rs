//! TPF2MP's deterministic economy core, ported from Lua.
//!
//! Ported from `tf2mod` (TPF2MP by Julian Cooper, MIT licence) at commit
//! `58da402ba144b2b5ad9f5615f7d686aadd34ffb2`, files
//! `tpf2_mp_1/res/scripts/tpf2_mp/economy_*.lua` and the pure arithmetic of
//! `economy.lua`. Each submodule names the Lua module it ports.
//! `docs/ECONOMY.md` maps every function, lists what is not ported, and
//! describes the differential tests that run the original Lua next to this
//! port and require identical results.
//!
//! Conventions:
//!
//! - Money is in cents and shares are parts per million (ppm), all `i64`.
//! - A function either returns exactly what the Lua original returns, or
//!   `None` where Lua would compute with rounded doubles, infinities or NaN
//!   (see [`crate::lua`]).
//! - An `Option` argument is a value TPF2MP state can legitimately lack (Lua
//!   `nil`); the port applies the same default. Other arguments are typed
//!   integers, so Lua's coercion of strings, fractions and NaN does not arise.
//! - Collections that Lua iterates in sorted key order are `BTreeMap`s keyed
//!   by string. Rust orders strings bytewise, as Lua 5.1 does in the C locale.

pub mod allocation;
pub mod costs;
pub mod difficulty;
pub mod feeder_access;
pub mod flow;
pub mod revenue;
pub mod settlement;
pub mod town_demand;

/// Ceiling of every authored money aggregate and of costed capital: 10^15
/// cents. TPF2MP chose it well below 2^53 so aggregates stay exact doubles.
pub const ACCUMULATOR_LIMIT: i64 = 1_000_000_000_000_000;

/// One whole market's demand, in parts per million.
pub const SHARE_SCALE: i64 = 1_000_000;

/// What a market carries. TPF2MP treats every kind other than `"cargo"` as
/// passengers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MarketKind {
    Passenger,
    Cargo,
}
