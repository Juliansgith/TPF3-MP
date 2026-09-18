//! The shared-memory link between the in-game hook and the agent process.
//!
//! One mapping carries a fixed [`header`] and two single-producer,
//! single-consumer rings ([`ring`]): hook->agent and agent->hook. Neither side
//! polls a file or touches the network; a message is a length-prefixed byte
//! frame copied straight into the ring, with no allocation on the hot path.
//!
//! # Startup and roles
//!
//! One side is the owner: it [`Link::create`]s the mapping, which zeroes the
//! header, writes the ABI version, ring sizes and a fresh non-zero `session`,
//! and finally publishes the magic. The other side [`Link::open`]s it, reads
//! the magic (returning [`IpcError::NotReady`] until it appears) and validates
//! the ABI and sizes. In TPF3-MP the agent is the owner and the hook attaches
//! when the game launches; the hook fails closed if no mapping is present.
//!
//! # Heartbeat and restart
//!
//! Each side bumps its own heartbeat counter ([`Link::heartbeat`]) and reads
//! the peer's ([`Link::peer_heartbeat`]); a counter that stops advancing means
//! the peer is gone. `session` distinguishes link generations: after a restart
//! the owner creates again with a new `session`, so a peer that sees the value
//! change knows the rings were reset and drops anything in flight. See
//! `docs/HOOKS.md` for the byte-exact ABI.

mod header;
mod ring;
mod shm;

use std::fmt;

use thiserror::Error;

pub use header::{ABI_VERSION, HEADER_SIZE};
pub use ring::{LENGTH_PREFIX, PopError, PushError};
pub use shm::ShmError;

use header::Layout;
use shm::SharedRegion;

/// Default size of each ring's data area (1 MiB).
pub const DEFAULT_RING_CAPACITY: u32 = 1 << 20;
/// Default maximum payload per message (60 KiB), leaving headroom in the ring.
pub const DEFAULT_MAX_MESSAGE: u32 = 60 * 1024;

/// Which end of the link a [`Link`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// In-game hook: produces on hook->agent, consumes agent->hook.
    Hook,
    /// Agent process: produces on agent->hook, consumes hook->agent.
    Agent,
}

/// How to size a new link.
#[derive(Debug, Clone)]
pub struct Config {
    /// A logical name shared by both ends; mapped to a per-user OS object name.
    pub name: String,
    /// Bytes in each ring's data area. Must be a power of two, and large enough
    /// for `max_message` plus the length prefix.
    pub ring_capacity: u32,
    /// Largest payload a single message may carry.
    pub max_message: u32,
}

impl Config {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ring_capacity: DEFAULT_RING_CAPACITY,
            max_message: DEFAULT_MAX_MESSAGE,
        }
    }
}

#[derive(Debug, Error)]
pub enum IpcError {
    #[error(transparent)]
    Shm(#[from] ShmError),
    #[error("invalid configuration: {0}")]
    InvalidConfig(&'static str),
    #[error("the link is not initialised yet (no magic)")]
    NotReady,
    #[error("ABI mismatch: peer speaks {found}, this build speaks {expected}")]
    AbiMismatch { found: u32, expected: u32 },
    #[error("header size mismatch: peer says {found}, this build uses {expected}")]
    HeaderMismatch { found: u32, expected: u32 },
    #[error("the ring capacity {0} is not a usable power of two")]
    BadRingCapacity(u32),
}

/// Failure to send a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SendError {
    #[error("the outbound ring is full")]
    Full,
    #[error("message of {len} bytes exceeds the {max}-byte limit")]
    TooLarge { len: usize, max: usize },
}

impl From<PushError> for SendError {
    fn from(error: PushError) -> Self {
        match error {
            PushError::Full => SendError::Full,
            PushError::TooLarge { len, max } => SendError::TooLarge { len, max },
        }
    }
}

/// Failure to receive a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RecvError {
    #[error("the inbound ring is corrupt")]
    Corrupt,
    #[error("the buffer of {have} bytes is too small for a {needed}-byte message")]
    BufferTooSmall { have: usize, needed: usize },
}

impl From<PopError> for RecvError {
    fn from(error: PopError) -> Self {
        match error {
            PopError::Corrupt => RecvError::Corrupt,
            PopError::BufferTooSmall { have, needed } => RecvError::BufferTooSmall { have, needed },
        }
    }
}

/// One end of the shared-memory link.
pub struct Link {
    region: SharedRegion,
    layout: Layout,
    role: Role,
    ring_capacity: u32,
    max_message: u32,
}

impl Link {
    /// Creates the link as the owner (resetting it if it already exists).
    pub fn create(config: &Config, role: Role) -> Result<Self, IpcError> {
        if !config.ring_capacity.is_power_of_two() {
            return Err(IpcError::BadRingCapacity(config.ring_capacity));
        }
        if config.ring_capacity > (1 << 31) {
            return Err(IpcError::InvalidConfig("ring capacity exceeds 2^31"));
        }
        if config.max_message as usize + LENGTH_PREFIX > config.ring_capacity as usize {
            return Err(IpcError::InvalidConfig(
                "max_message plus the length prefix does not fit the ring",
            ));
        }
        let total = HEADER_SIZE + 2 * config.ring_capacity as usize;
        let region = SharedRegion::create(&config.name, total)?;
        let layout = Layout::over(&region, config.ring_capacity, config.max_message);
        layout.zero_header();
        layout.write_fields(new_session());
        match role {
            Role::Hook => {
                layout.set_hook_pid(std::process::id());
                layout.set_hook_heartbeat(1);
            }
            Role::Agent => {
                layout.set_agent_pid(std::process::id());
                layout.set_agent_heartbeat(1);
            }
        }
        layout.publish_magic();
        Ok(Self {
            region,
            layout,
            role,
            ring_capacity: config.ring_capacity,
            max_message: config.max_message,
        })
    }

    /// Opens an existing link created by the other side.
    pub fn open(name: &str, role: Role) -> Result<Self, IpcError> {
        let region = SharedRegion::open(name, HEADER_SIZE)?;
        // Only header fields are read through `probe`; the rings are accessed
        // through `layout` below, after the sizes are validated.
        let probe = Layout::over(&region, 0, 0);
        if probe.magic() != header::MAGIC {
            return Err(IpcError::NotReady);
        }
        if probe.abi() != ABI_VERSION {
            return Err(IpcError::AbiMismatch {
                found: probe.abi(),
                expected: ABI_VERSION,
            });
        }
        if probe.header_size() as usize != HEADER_SIZE {
            return Err(IpcError::HeaderMismatch {
                found: probe.header_size(),
                expected: HEADER_SIZE as u32,
            });
        }
        let ring_capacity = probe.ring_capacity_field();
        if !ring_capacity.is_power_of_two() || ring_capacity > (1 << 31) {
            return Err(IpcError::BadRingCapacity(ring_capacity));
        }
        let max_message = probe.max_message_field();
        let total = HEADER_SIZE + 2 * ring_capacity as usize;
        if region.len() < total {
            return Err(IpcError::Shm(ShmError::TooSmall {
                found: region.len(),
                expected: total,
            }));
        }
        let layout = Layout::over(&region, ring_capacity, max_message);
        match role {
            Role::Hook => {
                layout.set_hook_pid(std::process::id());
                layout.set_hook_heartbeat(1);
            }
            Role::Agent => {
                layout.set_agent_pid(std::process::id());
                layout.set_agent_heartbeat(1);
            }
        }
        Ok(Self {
            region,
            layout,
            role,
            ring_capacity,
            max_message,
        })
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn ring_capacity(&self) -> u32 {
        self.ring_capacity
    }

    pub fn max_message(&self) -> u32 {
        self.max_message
    }

    pub fn abi_version(&self) -> u32 {
        self.layout.abi()
    }

    /// The current link generation; changes when the owner re-creates the link.
    pub fn session(&self) -> u32 {
        self.layout.session()
    }

    /// Sends one message on this end's producer ring.
    pub fn send(&self, payload: &[u8]) -> Result<(), SendError> {
        let ring = match self.role {
            Role::Hook => self.layout.h2a(),
            Role::Agent => self.layout.a2h(),
        };
        ring.push(payload).map_err(SendError::from)
    }

    /// Receives one message into `buf`, or `Ok(None)` when nothing is waiting.
    pub fn recv_into(&self, buf: &mut [u8]) -> Result<Option<usize>, RecvError> {
        let ring = match self.role {
            Role::Hook => self.layout.a2h(),
            Role::Agent => self.layout.h2a(),
        };
        ring.pop_into(buf).map_err(RecvError::from)
    }

    /// The length of the next waiting message, for sizing a receive buffer.
    pub fn next_len(&self) -> Option<u32> {
        let ring = match self.role {
            Role::Hook => self.layout.a2h(),
            Role::Agent => self.layout.h2a(),
        };
        ring.peek_len()
    }

    /// Advances this end's heartbeat, so the peer can tell it is alive.
    pub fn heartbeat(&self) {
        match self.role {
            Role::Hook => self.layout.bump_hook_heartbeat(),
            Role::Agent => self.layout.bump_agent_heartbeat(),
        }
    }

    /// The peer's heartbeat counter.
    pub fn peer_heartbeat(&self) -> u64 {
        match self.role {
            Role::Hook => self.layout.agent_heartbeat(),
            Role::Agent => self.layout.hook_heartbeat(),
        }
    }

    /// This end's process id, as recorded in the header.
    pub fn own_pid(&self) -> u32 {
        match self.role {
            Role::Hook => self.layout.hook_pid(),
            Role::Agent => self.layout.agent_pid(),
        }
    }

    /// The peer's process id, or 0 if it has not attached yet.
    pub fn peer_pid(&self) -> u32 {
        match self.role {
            Role::Hook => self.layout.agent_pid(),
            Role::Agent => self.layout.hook_pid(),
        }
    }

    /// Bytes in the mapped region (informational).
    pub fn region_len(&self) -> usize {
        self.region.len()
    }
}

impl fmt::Debug for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Link")
            .field("role", &self.role)
            .field("session", &self.session())
            .field("ring_capacity", &self.ring_capacity)
            .field("max_message", &self.max_message)
            .finish_non_exhaustive()
    }
}

fn new_session() -> u32 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos ^ std::process::id()).wrapping_mul(2_654_435_761) | 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_name(tag: &str) -> String {
        format!("test.{}.{}.{}", tag, std::process::id(), new_session())
    }

    #[test]
    fn create_open_exchange_in_one_process() {
        let name = unique_name("exchange");
        let agent = Link::create(&Config::new(name.clone()), Role::Agent).unwrap();
        let hook = Link::open(&name, Role::Hook).unwrap();

        assert_eq!(hook.abi_version(), ABI_VERSION);
        assert_eq!(hook.session(), agent.session());
        assert_eq!(agent.peer_pid(), std::process::id());

        agent.send(b"to-hook").unwrap();
        let mut buf = [0u8; 64];
        assert_eq!(hook.recv_into(&mut buf).unwrap(), Some(7));
        assert_eq!(&buf[..7], b"to-hook");

        hook.send(b"to-agent").unwrap();
        assert_eq!(agent.recv_into(&mut buf).unwrap(), Some(8));
        assert_eq!(&buf[..8], b"to-agent");
    }

    #[test]
    fn heartbeats_advance() {
        let name = unique_name("heartbeat");
        let agent = Link::create(&Config::new(name.clone()), Role::Agent).unwrap();
        let hook = Link::open(&name, Role::Hook).unwrap();
        let before = agent.peer_heartbeat();
        hook.heartbeat();
        hook.heartbeat();
        assert_eq!(agent.peer_heartbeat(), before + 2);
    }

    #[test]
    fn open_without_create_fails() {
        let error = Link::open("no.such.link", Role::Hook).unwrap_err();
        assert!(matches!(error, IpcError::Shm(_)), "{error}");
    }

    #[test]
    fn rejects_non_power_of_two_capacity() {
        let mut config = Config::new(unique_name("badcap"));
        config.ring_capacity = 1000;
        assert!(matches!(
            Link::create(&config, Role::Agent),
            Err(IpcError::BadRingCapacity(1000))
        ));
    }

    #[test]
    fn full_ring_reports_send_error() {
        let name = unique_name("full");
        let mut config = Config::new(name);
        config.ring_capacity = 64;
        config.max_message = 32;
        let agent = Link::create(&config, Role::Agent).unwrap();
        // 4-byte prefix + 32 payload = 36; a second does not fit in 64.
        agent.send(&[7u8; 32]).unwrap();
        assert_eq!(agent.send(&[7u8; 32]), Err(SendError::Full));
    }
}
