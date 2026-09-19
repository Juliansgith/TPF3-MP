//! A stand-in for the in-game hook: the toy game behind the real step gate,
//! on the real shared-memory link, in a thread of its own as a game would
//! be. With it a whole session runs end to end without the game: server,
//! client, bridge, link, gate and world.

use std::{
    thread::JoinHandle,
    time::{Duration, Instant},
};

use thiserror::Error;
use tpf3mp_bridge::{
    BRIDGE_VERSION, BridgeError, Gate, GateError, Gated, ToAgent, ToHook, check_version, decode,
    encode,
};
use tpf3mp_ipc::{IpcError, Link, Role};
use tpf3mp_proto::{LaneDigest, PlayerId, Text};

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
    /// Longest wait for anything from the agent.
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
    Link(#[from] IpcError),
    #[error("the link failed: {0}")]
    Io(String),
    #[error(transparent)]
    Message(#[from] BridgeError),
    #[error(transparent)]
    Gate(#[from] GateError),
    #[error("the agent sent {0} out of place")]
    Unexpected(&'static str),
    #[error("waited too long for the agent")]
    Timeout,
}

/// Starts the game in a thread of its own.
pub fn spawn(config: FakeHookConfig) -> JoinHandle<Result<HookReport, HookError>> {
    std::thread::spawn(move || run(&config))
}

fn run(config: &FakeHookConfig) -> Result<HookReport, HookError> {
    let deadline = || Instant::now() + config.patience;
    let link = open(&config.link_name, deadline())?;
    let mut buf = vec![0; tpf3mp_bridge::MAX_MESSAGE];
    send(
        &link,
        &ToAgent::Hello {
            version: BRIDGE_VERSION,
            build: Text::lossy("fake hook"),
        },
    )?;
    match recv(&link, &mut buf, deadline())? {
        ToHook::Hello { version } => check_version(version)?,
        _ => return Err(HookError::Unexpected("something before its hello")),
    }
    let interval = match recv(&link, &mut buf, deadline())? {
        ToHook::Begin {
            checkpoint_interval,
            ..
        } => u64::from(checkpoint_interval).max(1),
        _ => return Err(HookError::Unexpected("something before the game began")),
    };
    // Loading the world takes a moment in the real game.
    let mut world = ToyWorld::new(config.world_seed);
    send(&link, &ToAgent::Loaded { next_step: 1 })?;

    let mut rng = SplitMix64::new(config.seed);
    let mut gate = Gate::new(1);
    let mut report = HookReport {
        ran: 0,
        lanes: world.lanes(),
        applied: 0,
        commands: 0,
        refused: 0,
        diverged: Vec::new(),
        money: None,
        ended: false,
    };
    while report.ran < config.target_step {
        let wait_until = deadline();
        while !gate.may_run() {
            if gate.ended() {
                report.ended = true;
                return Ok(finish(report, &world, config));
            }
            match gate.on_message(recv(&link, &mut buf, wait_until)?)? {
                Gated::Apply(event) => {
                    world.apply(&event);
                    report.applied += 1;
                }
                Gated::Refused { .. } => report.refused += 1,
                Gated::Diverged { step, lanes } => report.diverged.push((step, lanes)),
                Gated::Ended(_) => {
                    report.ended = true;
                    return Ok(finish(report, &world, config));
                }
                Gated::Speed(_) | Gated::Nothing => {}
            }
        }
        let step = gate.ran()?;
        world.step(step);
        report.ran = step;
        send(&link, &ToAgent::Ran { step })?;
        if step % interval == 0 {
            send(
                &link,
                &ToAgent::Checkpoint {
                    step,
                    lanes: world.lanes(),
                },
            )?;
        }
        if config.act_every > 0 && step % config.act_every == 0 {
            let command = choose(&mut rng, &world.ledger, &config.player);
            send(
                &link,
                &ToAgent::Command {
                    payload: command.encode(),
                },
            )?;
            report.commands += 1;
        }
    }
    Ok(finish(report, &world, config))
}

fn finish(mut report: HookReport, world: &ToyWorld, config: &FakeHookConfig) -> HookReport {
    report.lanes = world.lanes();
    report.money = world.ledger.money(&config.player);
    report
}

/// Opens the agent's link, waiting for the agent to create it.
fn open(name: &str, deadline: Instant) -> Result<Link, HookError> {
    loop {
        match Link::open(name, Role::Hook) {
            Ok(link) => return Ok(link),
            Err(error) if Instant::now() >= deadline => return Err(error.into()),
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

fn send(link: &Link, message: &ToAgent) -> Result<(), HookError> {
    let bytes = encode(message)?;
    loop {
        link.heartbeat();
        match link.send(&bytes) {
            Ok(()) => return Ok(()),
            Err(tpf3mp_ipc::SendError::Full) => std::thread::sleep(Duration::from_micros(200)),
            Err(error) => return Err(HookError::Io(error.to_string())),
        }
    }
}

/// Waits for the agent's next message, beating the heartbeat meanwhile.
fn recv(link: &Link, buf: &mut [u8], deadline: Instant) -> Result<ToHook, HookError> {
    loop {
        link.heartbeat();
        match link.recv_into(buf) {
            Ok(Some(len)) => return Ok(decode(&buf[..len])?),
            Ok(None) if Instant::now() >= deadline => return Err(HookError::Timeout),
            Ok(None) => std::thread::sleep(Duration::from_micros(200)),
            Err(error) => return Err(HookError::Io(error.to_string())),
        }
    }
}
