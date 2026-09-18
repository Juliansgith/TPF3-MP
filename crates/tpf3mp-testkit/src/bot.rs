//! A bot plays the toy game through the real client and `TurnFollower`, the
//! way a player's game will: it applies events, executes steps, reports
//! progress and checkpoints, and issues commands of its own.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use thiserror::Error;
use tokio::sync::mpsc;
use tpf3mp_agent::{Action, Client, ClientError, ClientEvent, FollowError, Playout, TurnFollower};
use tpf3mp_proto::{EventBody, LaneDigest, PlayerId};

use crate::{
    rng::SplitMix64,
    toy::{Ledger, ToyCommand, ToyWorld},
};

/// A paced bot plays each step this long after the latest recent arrival.
const PLAYOUT_MARGIN: Duration = Duration::from_millis(20);
/// How long a late arrival keeps a paced bot's buffer grown.
const PLAYOUT_MEMORY: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct BotConfig {
    pub name: String,
    /// Seed of this bot's own choices.
    pub seed: u64,
    /// Seed of the shared world; the same for every bot in a room.
    pub world_seed: u64,
    /// The bot stops after executing this step, so every bot in a room
    /// compares the same state.
    pub target_step: u64,
    /// Steps between this bot's commands; `0` never sends any.
    pub act_every: u64,
    /// A step at which this replica's simulation drifts.
    pub drift_at: Option<u64>,
    /// Executes steps at the room's wall-clock pace, as a game does, rather
    /// than as soon as they are sealed.
    pub paced: bool,
}

#[derive(Debug, Clone)]
pub struct BotReport {
    pub name: String,
    pub player: PlayerId,
    pub executed: u64,
    /// Lanes of the world at `executed`.
    pub lanes: Vec<LaneDigest>,
    pub events: usize,
    pub sent: usize,
    pub rejected: usize,
    pub diverged: Vec<(u64, Vec<u16>)>,
    /// From sending an intent to applying it as an event.
    pub latencies: Vec<Duration>,
    pub money: Option<i64>,
}

#[derive(Debug, Error)]
pub enum BotError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("the server broke a turn invariant: {0}")]
    Follow(#[from] FollowError),
    #[error("the connection closed: {0}")]
    Closed(String),
    #[error("a turn arrived before its stream started")]
    NoStream,
    #[error("did not reach step {target} in time (at step {executed})")]
    Timeout { target: u64, executed: u64 },
}

pub struct Bot {
    client: Client,
    events: mpsc::Receiver<ClientEvent>,
    config: BotConfig,
    world: ToyWorld,
    follower: Option<TurnFollower>,
    /// When a paced bot plays each step.
    playout: Option<Playout>,
    /// When the next step falls due, while a paced bot waits for it.
    wait_until: Option<Instant>,
    checkpoint_interval: u64,
    rng: SplitMix64,
    next_seq: u64,
    in_flight: HashMap<u64, Instant>,
    /// The progress last reported.
    reported: Option<u64>,
    report: BotReport,
}

impl Bot {
    pub fn new(client: Client, events: mpsc::Receiver<ClientEvent>, config: BotConfig) -> Self {
        let mut world = ToyWorld::new(config.world_seed);
        if let Some(step) = config.drift_at {
            world = world.with_drift(step);
        }
        let report = BotReport {
            name: config.name.clone(),
            player: client.player(),
            executed: 0,
            lanes: Vec::new(),
            events: 0,
            sent: 0,
            rejected: 0,
            diverged: Vec::new(),
            latencies: Vec::new(),
            money: None,
        };
        Self {
            client,
            events,
            rng: SplitMix64::new(config.seed),
            config,
            world,
            follower: None,
            playout: None,
            wait_until: None,
            checkpoint_interval: u64::MAX,
            next_seq: 0,
            in_flight: HashMap::new(),
            reported: None,
            report,
        }
    }

    /// Plays until the target step and returns the client, still connected,
    /// with the report.
    pub async fn play(mut self, deadline: Duration) -> Result<(Client, BotReport), BotError> {
        let target = self.config.target_step;
        match tokio::time::timeout(deadline, self.play_to_target()).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(BotError::Timeout {
                    target,
                    executed: self.executed(),
                });
            }
        }
        self.report.executed = self.executed();
        self.report.lanes = self.world.lanes();
        self.report.money = self.world.ledger.money(&self.report.player);
        Ok((self.client, self.report))
    }

    fn executed(&self) -> u64 {
        self.follower.as_ref().map_or(0, TurnFollower::executed)
    }

    async fn play_to_target(&mut self) -> Result<(), BotError> {
        while self.executed() < self.config.target_step {
            let next = match self.wait_until {
                Some(at) => tokio::select! {
                    event = self.events.recv() => Some(event),
                    () = tokio::time::sleep_until(at.into()) => None,
                },
                None => Some(self.events.recv().await),
            };
            let Some(event) = next else {
                // The next step fell due.
                self.advance().await?;
                continue;
            };
            let Some(event) = event else {
                return Err(BotError::Closed("event channel ended".into()));
            };
            match event {
                ClientEvent::TurnStream(start) => {
                    match &mut self.follower {
                        Some(follower) => follower.restart(&start)?,
                        None => self.follower = Some(TurnFollower::new(&start)),
                    }
                    self.checkpoint_interval = u64::from(start.checkpoint_interval);
                    if self.config.paced {
                        self.playout = Some(Playout::new(
                            start.steps_per_second,
                            PLAYOUT_MARGIN,
                            PLAYOUT_MEMORY,
                        ));
                    }
                    // A new stream always hears where this replica stands.
                    let executed = self.executed();
                    self.reported = Some(executed);
                    self.client.report_progress(executed).await?;
                }
                ClientEvent::Turn(turn) => {
                    let follower = self.follower.as_mut().ok_or(BotError::NoStream)?;
                    follower.accept(turn)?;
                    if let Some(playout) = &mut self.playout {
                        playout.on_turn(
                            follower.sealed_through(),
                            follower.speed(),
                            Instant::now(),
                        );
                    }
                    self.advance().await?;
                }
                ClientEvent::IntentRejected { client_seq, .. } => {
                    self.in_flight.remove(&client_seq);
                    self.report.rejected += 1;
                }
                ClientEvent::Diverged { step, lanes } => self.report.diverged.push((step, lanes)),
                ClientEvent::RoomUpdate(_) => {}
                ClientEvent::Closed(reason) => return Err(BotError::Closed(reason.to_string())),
            }
        }
        Ok(())
    }

    /// Does everything the received turns allow, stopping exactly at the
    /// target step, then reports. A paced bot also stops at the first step
    /// that is not due yet.
    async fn advance(&mut self) -> Result<(), BotError> {
        let me = self.report.player;
        let now = Instant::now();
        let mut checkpoints = Vec::new();
        let mut commands = Vec::new();
        self.wait_until = None;
        let follower = self.follower.as_mut().ok_or(BotError::NoStream)?;
        while follower.executed() < self.config.target_step {
            if let (Some(playout), Some(step)) = (&mut self.playout, follower.next_step()) {
                let due = playout.due(step, now).unwrap_or(now);
                if due > now {
                    self.wait_until = Some(due);
                    break;
                }
                playout.played(step, due);
            }
            let Some(action) = follower.next_action() else {
                break;
            };
            match action {
                Action::Apply(event) => {
                    if let EventBody::Command {
                        player, client_seq, ..
                    } = &event.body
                        && *player == me
                        && let Some(sent) = self.in_flight.remove(client_seq)
                    {
                        self.report.latencies.push(sent.elapsed());
                    }
                    self.world.apply(&event);
                    self.report.events += 1;
                }
                Action::Execute(step) => {
                    self.world.step(step);
                    if step.is_multiple_of(self.checkpoint_interval) {
                        checkpoints.push((step, self.world.lanes()));
                    }
                    if self.config.act_every > 0 && step.is_multiple_of(self.config.act_every) {
                        commands.push(choose(&mut self.rng, &self.world.ledger, &me));
                    }
                }
            }
        }
        let executed = follower.executed();
        for (step, lanes) in checkpoints {
            self.client.report_checkpoint(step, lanes).await?;
        }
        for command in commands {
            let seq = self.next_seq;
            self.next_seq += 1;
            self.in_flight.insert(seq, Instant::now());
            self.report.sent += 1;
            self.client.send_intent(seq, command.encode()).await?;
        }
        if self.reported != Some(executed) {
            self.reported = Some(executed);
            self.client.report_progress(executed).await?;
        }
        Ok(())
    }
}

/// Picks a plausible command from what this replica believes it owns. The
/// replica can lag the server's canonical ledger, so some commands will be
/// refused, which is part of what the bots exercise.
fn choose(rng: &mut SplitMix64, ledger: &Ledger, me: &PlayerId) -> ToyCommand {
    let tracks = ledger.tracks_of(me);
    let trains = ledger.trains_of(me);
    let pick = |rng: &mut SplitMix64, items: &[u32]| {
        let index = usize::try_from(rng.below(items.len() as u64)).unwrap_or(0);
        items[index]
    };
    let build = |rng: &mut SplitMix64| ToyCommand::BuildTrack {
        length: 20 + u32::try_from(rng.below(200)).unwrap_or(0),
    };
    if tracks.is_empty() {
        return build(rng);
    }
    match rng.below(10) {
        0..=4 => ToyCommand::BuyTrain {
            track: pick(rng, &tracks),
        },
        5..=7 => build(rng),
        _ if trains.is_empty() => build(rng),
        _ => ToyCommand::SellTrain {
            train: pick(rng, &trains),
        },
    }
}
