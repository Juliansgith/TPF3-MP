//! A deterministic toy transport game that stands in for TPF3 until the real
//! game exists. It is split the way the architecture splits TPF3:
//!
//! - [`Ledger`] is canonical: companies, money and ownership, changed only by
//!   ordered events. The server runs it as the room's ruleset to validate
//!   intents, and every replica runs the same code on the same events.
//! - [`ToyWorld`] is a replica: the ledger plus a simulation (trains moving
//!   on tracks, driven by a seeded generator) that plays the role of TPF3's
//!   native world. Its state is compared through checkpoint lanes, and
//!   [`ToyWorld::with_drift`] perturbs it the way a platform-specific float
//!   difference would.

use std::collections::BTreeMap;

use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use tpf3mp_proto::{Event, EventBody, FixedBytes, LaneDigest, Payload, PlayerId};
use tpf3mp_server::Ruleset;

use crate::rng::SplitMix64;

pub const START_MONEY: i64 = 1_000_000;
pub const TRACK_COST_PER_UNIT: i64 = 100;
pub const TRAIN_COST: i64 = 25_000;
pub const MAX_TRACK_LENGTH: u32 = 1_000;

/// Refusal codes returned to players.
pub mod refusal {
    pub const MALFORMED: u16 = 1;
    pub const FUNDS: u16 = 2;
    pub const NOT_OWNER: u16 = 3;
    pub const UNKNOWN: u16 = 4;
    pub const OUT_OF_RANGE: u16 = 5;
    pub const NO_COMPANY: u16 = 6;
}

/// Lanes a replica reports at checkpoints.
pub mod lane {
    pub const LEDGER: u16 = 0;
    pub const TRAINS: u16 = 1;
    pub const DELIVERIES: u16 = 2;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToyCommand {
    BuildTrack { length: u32 },
    BuyTrain { track: u32 },
    SellTrain { train: u32 },
}

impl ToyCommand {
    pub fn encode(&self) -> Payload {
        let bytes = postcard::to_stdvec(self).expect("toy commands always encode");
        Payload::new(bytes).expect("toy commands are far below the payload limit")
    }

    pub fn decode(payload: &Payload) -> Option<Self> {
        postcard::from_bytes(payload.as_bytes()).ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct Track {
    owner: PlayerId,
    length: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct Train {
    owner: PlayerId,
    track: u32,
}

/// What a ledger change means for the simulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    TrainBought { train: u32 },
    TrainSold { train: u32 },
}

/// The canonical state: companies, money and ownership.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Ledger {
    money: BTreeMap<PlayerId, i64>,
    tracks: BTreeMap<u32, Track>,
    trains: BTreeMap<u32, Train>,
    next_track: u32,
    next_train: u32,
}

impl Ledger {
    pub fn money(&self, player: &PlayerId) -> Option<i64> {
        self.money.get(player).copied()
    }

    pub fn tracks_of(&self, player: &PlayerId) -> Vec<u32> {
        self.tracks
            .iter()
            .filter(|(_, track)| track.owner == *player)
            .map(|(id, _)| *id)
            .collect()
    }

    pub fn trains_of(&self, player: &PlayerId) -> Vec<u32> {
        self.trains
            .iter()
            .filter(|(_, train)| train.owner == *player)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Whether `player` may do `command` now.
    pub fn check(&self, player: &PlayerId, command: &ToyCommand) -> Result<(), u16> {
        let money = self.money(player).ok_or(refusal::NO_COMPANY)?;
        match command {
            ToyCommand::BuildTrack { length } => {
                if !(1..=MAX_TRACK_LENGTH).contains(length) {
                    return Err(refusal::OUT_OF_RANGE);
                }
                if money < i64::from(*length) * TRACK_COST_PER_UNIT {
                    return Err(refusal::FUNDS);
                }
            }
            ToyCommand::BuyTrain { track } => {
                let track = self.tracks.get(track).ok_or(refusal::UNKNOWN)?;
                if track.owner != *player {
                    return Err(refusal::NOT_OWNER);
                }
                if money < TRAIN_COST {
                    return Err(refusal::FUNDS);
                }
            }
            ToyCommand::SellTrain { train } => {
                let train = self.trains.get(train).ok_or(refusal::UNKNOWN)?;
                if train.owner != *player {
                    return Err(refusal::NOT_OWNER);
                }
            }
        }
        Ok(())
    }

    /// Applies an ordered event. A command the rules refuse changes nothing;
    /// the server never orders one, but every replica checks anyway.
    pub fn apply(&mut self, event: &Event) -> Option<Effect> {
        match &event.body {
            EventBody::PlayerJoined { player, .. } => {
                self.money.entry(*player).or_insert(START_MONEY);
                None
            }
            EventBody::PlayerLeft { .. } => None,
            EventBody::Command {
                player, payload, ..
            } => {
                let command = ToyCommand::decode(payload)?;
                self.check(player, &command).ok()?;
                let money = self.money.get_mut(player)?;
                match command {
                    ToyCommand::BuildTrack { length } => {
                        *money -= i64::from(length) * TRACK_COST_PER_UNIT;
                        self.tracks.insert(
                            self.next_track,
                            Track {
                                owner: *player,
                                length,
                            },
                        );
                        self.next_track += 1;
                        None
                    }
                    ToyCommand::BuyTrain { track } => {
                        *money -= TRAIN_COST;
                        let train = self.next_train;
                        self.trains.insert(
                            train,
                            Train {
                                owner: *player,
                                track,
                            },
                        );
                        self.next_train += 1;
                        Some(Effect::TrainBought { train })
                    }
                    ToyCommand::SellTrain { train } => {
                        *money += TRAIN_COST / 2;
                        self.trains.remove(&train);
                        Some(Effect::TrainSold { train })
                    }
                }
            }
        }
    }
}

/// The ledger as a room's canonical ruleset on the server.
#[derive(Debug, Default)]
pub struct ToyRules {
    ledger: Ledger,
}

impl Ruleset for ToyRules {
    fn validate(&self, player: &PlayerId, payload: &Payload) -> Result<(), u16> {
        let command = ToyCommand::decode(payload).ok_or(refusal::MALFORMED)?;
        self.ledger.check(player, &command)
    }

    fn apply(&mut self, event: &Event) {
        self.ledger.apply(event);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct TrainState {
    position: u32,
    forward: bool,
    load: u32,
}

/// One replica of the toy game.
#[derive(Debug, Clone)]
pub struct ToyWorld {
    pub ledger: Ledger,
    trains: BTreeMap<u32, TrainState>,
    rng: SplitMix64,
    delivered: u64,
    drift_at: Option<u64>,
}

impl ToyWorld {
    pub fn new(seed: u64) -> Self {
        Self {
            ledger: Ledger::default(),
            trains: BTreeMap::new(),
            rng: SplitMix64::new(seed),
            delivered: 0,
            drift_at: None,
        }
    }

    /// Makes this replica's simulation deviate once at `step`, as a
    /// platform-specific rounding difference would.
    pub fn with_drift(mut self, step: u64) -> Self {
        self.drift_at = Some(step);
        self
    }

    pub fn apply(&mut self, event: &Event) {
        match self.ledger.apply(event) {
            Some(Effect::TrainBought { train }) => {
                self.trains.insert(
                    train,
                    TrainState {
                        position: 0,
                        forward: true,
                        load: 0,
                    },
                );
            }
            Some(Effect::TrainSold { train }) => {
                self.trains.remove(&train);
            }
            None => {}
        }
    }

    pub fn step(&mut self, step: u64) {
        for (id, state) in &mut self.trains {
            let Some(train) = self.ledger.trains.get(id) else {
                continue;
            };
            let Some(track) = self.ledger.tracks.get(&train.track) else {
                continue;
            };
            let advance = u32::try_from(1 + self.rng.below(3)).unwrap_or(1);
            if state.forward {
                state.position = state.position.saturating_add(advance);
                if state.position >= track.length {
                    state.position = track.length;
                    state.forward = false;
                    self.delivered += u64::from(state.load);
                    state.load = u32::try_from(self.rng.below(100)).unwrap_or(0);
                }
            } else {
                state.position = state.position.saturating_sub(advance);
                if state.position == 0 {
                    state.forward = true;
                    self.delivered += u64::from(state.load);
                    state.load = u32::try_from(self.rng.below(100)).unwrap_or(0);
                }
            }
        }
        if self.drift_at == Some(step) {
            // One extra draw desynchronizes every later random choice, as a
            // rounding difference that flips one comparison would. Unlike a
            // nudged position, it can never be absorbed by clamping.
            self.rng.next_u64();
        }
    }

    /// The checkpoint lanes: the canonical ledger, the trains, and the
    /// deliveries together with the generator state.
    pub fn lanes(&self) -> Vec<LaneDigest> {
        vec![
            lane_digest(lane::LEDGER, &self.ledger),
            lane_digest(lane::TRAINS, &self.trains),
            lane_digest(lane::DELIVERIES, &(self.delivered, self.rng.state())),
        ]
    }
}

fn lane_digest<T: Serialize>(lane: u16, value: &T) -> LaneDigest {
    let bytes = postcard::to_stdvec(value).expect("toy state always encodes");
    let hash = digest(&SHA256, &bytes);
    let mut out = [0; 32];
    out.copy_from_slice(hash.as_ref());
    LaneDigest {
        lane,
        digest: FixedBytes(out),
    }
}

#[cfg(test)]
mod tests {
    use tpf3mp_proto::Text;

    use super::*;

    fn player(id: u8) -> PlayerId {
        PlayerId(FixedBytes([id; 32]))
    }

    fn joined(seq: u64, id: u8) -> Event {
        Event {
            seq,
            step: 1,
            body: EventBody::PlayerJoined {
                player: player(id),
                name: Text::new("p").unwrap(),
            },
        }
    }

    fn command(seq: u64, id: u8, command: &ToyCommand) -> Event {
        Event {
            seq,
            step: seq,
            body: EventBody::Command {
                player: player(id),
                client_seq: seq,
                payload: command.encode(),
            },
        }
    }

    #[test]
    fn the_ledger_enforces_funds_and_ownership() {
        let mut ledger = Ledger::default();
        ledger.apply(&joined(1, 1));
        ledger.apply(&joined(2, 2));
        let build = ToyCommand::BuildTrack { length: 100 };
        assert_eq!(ledger.check(&player(1), &build), Ok(()));
        ledger.apply(&command(3, 1, &build));
        assert_eq!(ledger.money(&player(1)), Some(START_MONEY - 10_000));
        let buy = ToyCommand::BuyTrain { track: 0 };
        assert_eq!(ledger.check(&player(2), &buy), Err(refusal::NOT_OWNER));
        assert_eq!(ledger.check(&player(9), &buy), Err(refusal::NO_COMPANY));
        let too_long = ToyCommand::BuildTrack {
            length: MAX_TRACK_LENGTH + 1,
        };
        assert_eq!(
            ledger.check(&player(1), &too_long),
            Err(refusal::OUT_OF_RANGE)
        );
    }

    #[test]
    fn replicas_given_the_same_events_agree_and_drift_shows() {
        let events = [
            joined(1, 1),
            command(2, 1, &ToyCommand::BuildTrack { length: 40 }),
            command(3, 1, &ToyCommand::BuyTrain { track: 0 }),
            command(4, 1, &ToyCommand::BuyTrain { track: 0 }),
        ];
        let mut a = ToyWorld::new(7);
        let mut b = ToyWorld::new(7);
        let mut c = ToyWorld::new(7).with_drift(50);
        for world in [&mut a, &mut b, &mut c] {
            for event in &events {
                world.apply(event);
            }
            for step in 1..=200 {
                world.step(step);
            }
        }
        assert_eq!(a.lanes(), b.lanes());
        let (lanes_a, lanes_c) = (a.lanes(), c.lanes());
        assert_eq!(lanes_a[0], lanes_c[0], "drift never touches the ledger");
        assert_ne!(lanes_a[1], lanes_c[1], "the trains move differently");
        assert_ne!(lanes_a[2], lanes_c[2], "the generator state differs");
    }
}
