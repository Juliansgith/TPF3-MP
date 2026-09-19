//! A stand-in for the in-game hook: the toy game as a [`Game`] behind the
//! real [`Session`], on the real shared-memory link, in a thread of its own
//! as a game would be. With it a whole session runs end to end without the
//! game: server, client, bridge, link, session, gate and world. The real
//! hook differs only in what implements [`Game`].

use std::{thread::JoinHandle, time::Duration};

use thiserror::Error;
use tpf3mp_bridge::{Game, Notice, Session, SessionError, StepGate};
use tpf3mp_proto::{Event, LaneDigest, PlayerId};

use crate::{bot::choose, rng::SplitMix64, toy::ToyWorld};

#[derive(Debug, Clone)]
pub struct FakeHookConfig {
    /// The link the agent created.
    pub link_name: String,
    /// The player this game belongs to.
    pub player: PlayerId,
    /// Seed of this player's own choices.
    pub seed: u64,
    /// Seed of the shared world; the same for every game in a room.
    pub world_seed: u64,
    /// Steps between this player's commands; `0` never sends any.
    pub act_every: u64,
    /// The game stops after running this step.
    pub target_step: u64,
    /// How long to wait for the agent to appear, or to beat again.
    pub patience: Duration,
}

#[derive(Debug, Clone)]
pub struct HookReport {
    pub ran: u64,
    /// The world's lanes after the last step run.
    pub lanes: Vec<LaneDigest>,
    pub applied: usize,
    pub commands: u64,
    pub refused: usize,
    pub diverged: Vec<(u64, Vec<u16>)>,
    pub money: Option<i64>,
    /// The session ended before the target step.
    pub ended: bool,
}

#[derive(Debug, Error)]
pub enum HookError {
    #[error(transparent)]
    Session(#[from] SessionError),
}

/// The toy game, as the session sees a game.
struct ToyGame {
    world: ToyWorld,
    applied: usize,
    refused: usize,
    diverged: Vec<(u64, Vec<u16>)>,
}

impl Game for ToyGame {
    fn apply(&mut self, event: &Event) {
        self.world.apply(event);
        self.applied += 1;
    }

    fn lanes(&mut self) -> Vec<LaneDigest> {
        self.world.lanes()
    }

    fn notice(&mut self, notice: Notice) {
        match notice {
            Notice::Refused { .. } => self.refused += 1,
            Notice::Diverged { step, lanes } => self.diverged.push((step, lanes)),
            Notice::Speed(_) | Notice::Ended(_) => {}
        }
    }
}

/// Starts the game in a thread of its own.
pub fn spawn(config: FakeHookConfig) -> JoinHandle<Result<HookReport, HookError>> {
    std::thread::spawn(move || run(&config))
}

fn run(config: &FakeHookConfig) -> Result<HookReport, HookError> {
    let mut session = Session::attach(&config.link_name, "fake hook", config.patience)?;
    session.wait_for_begin()?;
    // Loading the world takes a moment in the real game.
    let mut game = ToyGame {
        world: ToyWorld::new(config.world_seed),
        applied: 0,
        refused: 0,
        diverged: Vec::new(),
    };
    session.loaded(1)?;

    let mut rng = SplitMix64::new(config.seed);
    let mut ran = 0;
    let mut commands = 0;
    let mut ended = false;
    while ran < config.target_step {
        match session.before_step(&mut game)? {
            StepGate::Run => {}
            StepGate::Ended | StepGate::Wait => {
                ended = true;
                break;
            }
        }
        game.world.step(session.next_step());
        ran = session.after_step(&mut game)?;
        if config.act_every > 0 && ran % config.act_every == 0 {
            let command = choose(&mut rng, &game.world.ledger, &config.player);
            session.command(command.encode())?;
            commands += 1;
        }
    }
    Ok(HookReport {
        ran,
        lanes: game.world.lanes(),
        applied: game.applied,
        commands,
        refused: game.refused,
        diverged: game.diverged,
        money: game.world.ledger.money(&config.player),
        ended,
    })
}
