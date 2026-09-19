//! The game's side of a session, which the hook runs on the game thread.
//!
//! Everything here is game-agnostic. The game-specific part of the hook
//! implements [`Game`] and calls the session from its detours:
//! [`Session::poll_step`] or [`Session::before_step`] before each simulation
//! step, [`Session::after_step`] after it, and [`Session::command`] when the
//! player acts. When the gate says [`StepGate::Load`], the game loads that
//! world and calls [`Session::loaded`].

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use thiserror::Error;
use tpf3mp_ipc::{IpcError, Link, Role, SendError};
use tpf3mp_proto::{
    ChatText, Event, EventBody, IntentRejection, LaneDigest, Payload, RulesName, Speed, Text,
};

use crate::{
    BRIDGE_VERSION, BridgeError, Gate, GateError, Gated, MAX_MESSAGE, ToAgent, ToHook,
    check_version, decode, encode,
};

/// How long to sleep between polls while waiting for the agent.
const POLL: Duration = Duration::from_micros(200);

/// What the session needs from the game.
pub trait Game {
    /// Applies an event the room ordered. Called only between steps. Save
    /// events never get here: the session calls [`Game::save`] for them.
    fn apply(&mut self, event: &Event);
    /// The world's lane digests, taken at a checkpoint step or a save.
    fn lanes(&mut self) -> Vec<LaneDigest>;
    /// Saves the world as it stands, between two steps, to `file`. A save
    /// the room agrees on is what players who join later load, so it must
    /// hold everything needed to continue from here.
    fn save(&mut self, file: &Path) -> Result<(), String>;
    /// Something to show the player.
    fn notice(&mut self, notice: Notice) {
        let _ = notice;
    }
}

/// Something the game should tell the player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Speed(Speed),
    Diverged {
        step: u64,
        lanes: Vec<u16>,
    },
    /// The room refused a command; `command` is what [`Session::command`]
    /// returned for it.
    Refused {
        command: u64,
        reason: IntentRejection,
    },
    /// The session is over.
    Ended(Text<128>),
    /// A member of the room said something.
    Chat {
        from: Text<32>,
        text: ChatText,
    },
}

/// What the game does about its next step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepGate {
    /// Run it.
    Run,
    /// Not yet: the room has not released it.
    Wait,
    /// The session is over; stop following the room.
    Ended,
    /// Replace the world with this one, then call [`Session::loaded`].
    Load(Load),
}

/// A world to load: see [`ToHook::Load`](crate::ToHook::Load).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Load {
    /// The save to load, or `None` for the world every player starts from.
    pub file: Option<PathBuf>,
    /// The first step the loaded world runs: pass it to
    /// [`Session::loaded`].
    pub next_step: u64,
}

/// A game the room began.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Begin {
    /// The rules the room is played by (see `ToHook::Begin`).
    pub rules: RulesName,
    pub steps_per_second: u16,
    pub checkpoint_interval: u32,
    /// Where the game's saves go.
    pub saves: PathBuf,
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error(transparent)]
    Link(#[from] IpcError),
    #[error("the link failed: {0}")]
    Transfer(String),
    #[error(transparent)]
    Message(#[from] BridgeError),
    #[error(transparent)]
    Gate(#[from] GateError),
    #[error("the agent sent {0} out of place")]
    Unexpected(&'static str),
    #[error("the agent stopped responding")]
    AgentGone,
    #[error("the world was loaded to run step {got} next, but step {expected} was ordered")]
    LoadedElsewhere { expected: u64, got: u64 },
    #[error("the game loaded a world nobody ordered")]
    NotLoading,
}

/// The hook's end of one session with the agent.
pub struct Session {
    link: Link,
    gate: Gate,
    /// A load the agent ordered that the game has not finished.
    pending_load: Option<Load>,
    saves: PathBuf,
    checkpoint_interval: u64,
    /// How long the agent's heartbeat may stand still.
    patience: Duration,
    agent_beat: (u64, Instant),
    buf: Vec<u8>,
    commands: u64,
}

impl Session {
    /// Attaches to the agent's link, waiting up to `patience` for the agent
    /// to create it, and exchanges hellos. `build` names the game build in
    /// the agent's log.
    pub fn attach(name: &str, build: &str, patience: Duration) -> Result<Self, SessionError> {
        let deadline = Instant::now() + patience;
        let link = loop {
            match Link::open(name, Role::Hook) {
                Ok(link) => break link,
                Err(error) if Instant::now() >= deadline => return Err(error.into()),
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        };
        let mut session = Self {
            agent_beat: (link.peer_heartbeat(), Instant::now()),
            link,
            gate: Gate::new(1),
            pending_load: None,
            saves: PathBuf::new(),
            checkpoint_interval: u64::MAX,
            patience,
            buf: vec![0; MAX_MESSAGE],
            commands: 0,
        };
        session.send(&ToAgent::Hello {
            version: BRIDGE_VERSION,
            build: Text::lossy(build),
        })?;
        match session.recv_blocking()? {
            ToHook::Hello { version } => check_version(version)?,
            _ => return Err(SessionError::Unexpected("something before its hello")),
        }
        Ok(session)
    }

    /// Waits for the room to begin a game. The first thing the gate then
    /// says is which world to load ([`StepGate::Load`]).
    pub fn wait_for_begin(&mut self) -> Result<Begin, SessionError> {
        loop {
            match self.recv_blocking()? {
                ToHook::Begin {
                    rules,
                    steps_per_second,
                    checkpoint_interval,
                    saves,
                } => {
                    self.checkpoint_interval = u64::from(checkpoint_interval).max(1);
                    self.saves = PathBuf::from(saves.as_str());
                    return Ok(Begin {
                        rules,
                        steps_per_second,
                        checkpoint_interval,
                        saves: self.saves.clone(),
                    });
                }
                // Talk in the lobby is for the front end.
                ToHook::Chat { .. } => {}
                _ => return Err(SessionError::Unexpected("something before the game began")),
            }
        }
    }

    /// The world the gate ordered ([`StepGate::Load`]) is loaded, and
    /// `next_step` is the first step it will run. Beat
    /// [`Session::heartbeat`] while loading.
    pub fn loaded(&mut self, next_step: u64) -> Result<(), SessionError> {
        let Some(load) = &self.pending_load else {
            return Err(SessionError::NotLoading);
        };
        if load.next_step != next_step {
            return Err(SessionError::LoadedElsewhere {
                expected: load.next_step,
                got: next_step,
            });
        }
        self.pending_load = None;
        self.gate.loaded(next_step);
        self.send(&ToAgent::Loaded { next_step })
    }

    /// Without blocking: handles what the agent sent, applying events, and
    /// says whether the game may run its next step. For a game whose
    /// simulation shares a thread with its rendering, which must not block.
    pub fn poll_step(&mut self, game: &mut impl Game) -> Result<StepGate, SessionError> {
        self.link.heartbeat();
        loop {
            if self.gate.ended() {
                return Ok(StepGate::Ended);
            }
            if let Some(load) = &self.pending_load {
                return Ok(StepGate::Load(load.clone()));
            }
            if self.gate.may_run() {
                return Ok(StepGate::Run);
            }
            let Some(message) = self.try_recv()? else {
                self.check_agent()?;
                return Ok(StepGate::Wait);
            };
            self.handle(message, game)?;
        }
    }

    /// Blocks until the game may run its next step, applying events
    /// meanwhile, or must load a world. A pause can hold the game here for
    /// as long as it lasts; only an agent that stops beating ends the wait.
    pub fn before_step(&mut self, game: &mut impl Game) -> Result<StepGate, SessionError> {
        loop {
            match self.poll_step(game)? {
                StepGate::Wait => std::thread::sleep(POLL),
                other => return Ok(other),
            }
        }
    }

    /// Records that the game ran its next step and reports it, with the
    /// world's lanes at checkpoint steps. Returns the step.
    pub fn after_step(&mut self, game: &mut impl Game) -> Result<u64, SessionError> {
        let step = self.gate.ran()?;
        self.send(&ToAgent::Ran { step })?;
        if step % self.checkpoint_interval == 0 {
            let lanes = game.lanes();
            self.send(&ToAgent::Checkpoint { step, lanes })?;
        }
        Ok(step)
    }

    /// Hands a player's action to the room. Returns its number, which a
    /// refusal names.
    pub fn command(&mut self, payload: Payload) -> Result<u64, SessionError> {
        let number = self.commands;
        self.send(&ToAgent::Command { payload })?;
        self.commands += 1;
        Ok(number)
    }

    /// Says something to the room for the player.
    pub fn chat(&mut self, text: ChatText) -> Result<(), SessionError> {
        self.send(&ToAgent::Chat { text })
    }

    /// The step the game runs next.
    pub fn next_step(&self) -> u64 {
        self.gate.next_step()
    }

    /// Tells the agent this side is alive. Call it while loading.
    pub fn heartbeat(&self) {
        self.link.heartbeat();
    }

    /// Puts a line in the agent's log.
    pub fn log(&mut self, message: &str) -> Result<(), SessionError> {
        self.send(&ToAgent::Log {
            message: Text::lossy(message),
        })
    }

    fn handle(&mut self, message: ToHook, game: &mut impl Game) -> Result<(), SessionError> {
        match self.gate.on_message(message)? {
            Gated::Apply(event) if matches!(event.body, EventBody::Save) => {
                self.save(event.seq, game)?;
            }
            Gated::Apply(event) => game.apply(&event),
            Gated::Load { file, next_step } => {
                self.pending_load = Some(Load {
                    file: file.map(|file| PathBuf::from(file.as_str())),
                    next_step,
                });
            }
            Gated::Speed(speed) => game.notice(Notice::Speed(speed)),
            Gated::Diverged { step, lanes } => game.notice(Notice::Diverged { step, lanes }),
            Gated::Refused { command, reason } => {
                game.notice(Notice::Refused { command, reason });
            }
            Gated::Ended(reason) => game.notice(Notice::Ended(reason)),
            Gated::Chat { from, text } => game.notice(Notice::Chat { from, text }),
            Gated::Nothing => {}
        }
        Ok(())
    }

    /// Saves the world at the save event `event` and reports it, with the
    /// world's lanes there. A failed save is reported too: the room decides
    /// without it.
    fn save(&mut self, event: u64, game: &mut impl Game) -> Result<(), SessionError> {
        let file = self.saves.join(format!("save-{event}.sav"));
        let saved = game.save(&file);
        let lanes = game.lanes();
        let file = match saved {
            Ok(()) => match Text::new(file.to_string_lossy().into_owned()) {
                Ok(file) => Some(file),
                Err(error) => {
                    self.log(&format!(
                        "cannot report the save {}: {error}",
                        file.display()
                    ))?;
                    None
                }
            },
            Err(reason) => {
                self.log(&format!("saving the world failed: {reason}"))?;
                None
            }
        };
        self.send(&ToAgent::Saved { event, lanes, file })
    }

    fn send(&mut self, message: &ToAgent) -> Result<(), SessionError> {
        let bytes = encode(message)?;
        loop {
            self.link.heartbeat();
            match self.link.send(&bytes) {
                Ok(()) => return Ok(()),
                Err(SendError::Full) => {
                    self.check_agent()?;
                    std::thread::sleep(POLL);
                }
                Err(error) => return Err(SessionError::Transfer(error.to_string())),
            }
        }
    }

    fn try_recv(&mut self) -> Result<Option<ToHook>, SessionError> {
        match self.link.recv_into(&mut self.buf) {
            Ok(Some(len)) => Ok(Some(decode(&self.buf[..len])?)),
            Ok(None) => Ok(None),
            Err(error) => Err(SessionError::Transfer(error.to_string())),
        }
    }

    fn recv_blocking(&mut self) -> Result<ToHook, SessionError> {
        loop {
            self.link.heartbeat();
            if let Some(message) = self.try_recv()? {
                return Ok(message);
            }
            self.check_agent()?;
            std::thread::sleep(POLL);
        }
    }

    fn check_agent(&mut self) -> Result<(), SessionError> {
        let beat = self.link.peer_heartbeat();
        let now = Instant::now();
        if beat != self.agent_beat.0 {
            self.agent_beat = (beat, now);
        } else if now.saturating_duration_since(self.agent_beat.1) > self.patience {
            return Err(SessionError::AgentGone);
        }
        Ok(())
    }
}
