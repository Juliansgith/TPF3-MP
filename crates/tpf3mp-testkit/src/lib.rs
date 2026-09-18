//! A stand-in for Transport Fever 3 until the real game exists: a
//! deterministic toy game, bots that play it through the real client, a
//! network emulator, and a runner for rooms of bots. It drives the whole
//! server and client stack the way the game will.

pub mod bot;
pub mod netem;
pub mod rng;
pub mod scenario;
pub mod toy;
