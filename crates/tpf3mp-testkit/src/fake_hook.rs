//! A stand-in for the in-game hook: the toy game as a [`Game`] behind the
//! real [`Session`], on the real shared-memory link, in a thread of its own
//! as a game would be. With it a whole session runs end to end without the
//! game: server, client, bridge, link, session, gate and world, including
//! saving the world and loading one the room sends. The real hook differs
//! only in what implements [`Game`].

use std::{path::Path, thread::JoinHandle, time::Duration};

use thiserror::Error;
use tpf3mp_bridge::{Game, Load, Notice, Session, SessionError, StepGate};
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
    /// This replica's simulation deviates once at this step, as a
    /// platform-specific float difference would.
    pub drift_at: Option<u64>,
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
    /// Worlds loaded from a save the room sent: a late join or a rebase.
    pub received: usize,
    /// Saves the room asked for.
    pub saves: usize,
    /// The session ended before the target step.
    pub ended: bool,
}

#[derive(Debug, Error)]
pub enum HookError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("cannot load the world {0}")]
    Load(String),
}

/// The toy game, as the session sees a game.
struct ToyGame {
    world: ToyWorld,
    applied: usize,
    refused: usize,
    saves: usize,
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

    fn save(&mut self, file: &Path) -> Result<(), String> {
        self.saves += 1;
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
        }
        std::fs::write(file, self.world.save()).map_err(|error| error.to_string())
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
    let mut game = ToyGame {
        world: ToyWorld::new(config.world_seed),
        applied: 0,
        refused: 0,
        saves: 0,
        diverged: Vec::new(),
    };
    let mut rng = SplitMix64::new(config.seed);
    let mut ran = 0;
    let mut commands = 0;
    let mut received = 0;
    let mut ended = false;
    while ran < config.target_step {
        match session.before_step(&mut game)? {
            StepGate::Run => {}
            StepGate::Load(load) => {
                received += usize::from(load.file.is_some());
                game.world = load_world(config, &load)?;
                session.loaded(load.next_step)?;
                ran = load.next_step - 1;
                continue;
            }
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
        received,
        saves: game.saves,
        ended,
    })
}

/// The world a load names: a save the room sent, or the world every player
/// starts from. A drift still ahead of the loaded world stays ahead.
fn load_world(config: &FakeHookConfig, load: &Load) -> Result<ToyWorld, HookError> {
    let world = match &load.file {
        Some(file) => {
            let bytes = std::fs::read(file)
                .map_err(|error| HookError::Load(format!("{}: {error}", file.display())))?;
            ToyWorld::load(&bytes)
                .ok_or_else(|| HookError::Load(format!("{}: not a toy save", file.display())))?
        }
        None => ToyWorld::new(config.world_seed),
    };
    Ok(match config.drift_at {
        Some(step) if step >= load.next_step => world.with_drift(step),
        _ => world,
    })
}
