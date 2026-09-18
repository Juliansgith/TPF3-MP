//! Differential tests: TPF2MP's original Lua economy against the Rust port.
//!
//! Every test runs the original Lua under Lua 5.1 and the port on the same
//! inputs and requires identical results (see `docs/ECONOMY.md`). Inputs come
//! from three sources:
//!
//! - TPF2MP's own parity-vector generator, run unmodified: every call it makes
//!   to a ported function is recorded and replayed (`parity_vectors`);
//! - property tests over the ranges the rules are designed for, and wider;
//! - explicit edge cases.
//!
//! One checker per ported function compares a recorded call with the port,
//! so recorded and generated calls are held to the same comparison.

#![allow(clippy::unwrap_used)]

mod tpf2mp;

mod allocation;
mod costs;
mod difficulty;
mod feeder_access;
mod flow;
mod lua_semantics;
mod market;
mod parity_vectors;
mod revenue;
mod settlement;
mod town_demand;
