//! What one network address, and everyone together, may hold: handshakes in
//! progress and sessions. Without these limits one host with throwaway keys
//! could take every session slot, or keep the server busy with handshakes.

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr},
    sync::{Arc, Mutex, PoisonError},
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The limits an operator can set.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// Handshakes in progress across the server.
    pub(crate) handshakes: usize,
    /// Handshakes in progress from one address.
    pub(crate) handshakes_per_address: usize,
    /// Sessions held by one address.
    pub(crate) sessions_per_address: usize,
    /// Open rooms created from one address. A room counts until it closes,
    /// even after its creator disconnects.
    pub(crate) rooms_per_address: usize,
}

/// Where a connection comes from, as far as limits go. An IPv6 host usually
/// controls a whole /64, so its addresses count together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Origin {
    V4(Ipv4Addr),
    V6(u64),
}

impl Origin {
    pub(crate) fn of(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(v4) => Self::V4(v4),
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => Self::V4(v4),
                None => Self::V6((v6.to_bits() >> 64) as u64),
            },
        }
    }
}

#[derive(Debug, Default)]
struct Held {
    handshakes: usize,
    sessions: usize,
    rooms: usize,
}

impl Held {
    fn is_empty(&self) -> bool {
        self.handshakes == 0 && self.sessions == 0 && self.rooms == 0
    }
}

/// What to do with a connection attempt.
pub(crate) enum Decision {
    Accept(Handshake),
    /// Ask the peer to prove it owns its address before any work is spent.
    Retry,
    Refuse,
}

pub(crate) struct Admission {
    limits: Limits,
    handshakes: Arc<Semaphore>,
    held: Mutex<HashMap<Origin, Held>>,
}

impl Admission {
    pub(crate) fn new(limits: Limits) -> Arc<Self> {
        Arc::new(Self {
            limits,
            handshakes: Arc::new(Semaphore::new(limits.handshakes)),
            held: Mutex::default(),
        })
    }

    /// Decides on a connection attempt from `origin`. Once half the
    /// handshake capacity is in use, peers must prove their address with a
    /// QUIC retry first, so spoofed packets cost the server nothing.
    pub(crate) fn on_attempt(
        self: &Arc<Self>,
        origin: Origin,
        address_validated: bool,
        may_retry: bool,
    ) -> Decision {
        let in_progress = self.limits.handshakes - self.handshakes.available_permits();
        if !address_validated && may_retry && in_progress * 2 >= self.limits.handshakes {
            return Decision::Retry;
        }
        let Ok(permit) = Arc::clone(&self.handshakes).try_acquire_owned() else {
            return Decision::Refuse;
        };
        let mut held = self.lock();
        let entry = held.entry(origin).or_default();
        if entry.handshakes >= self.limits.handshakes_per_address {
            if entry.is_empty() {
                held.remove(&origin);
            }
            return Decision::Refuse;
        }
        entry.handshakes += 1;
        Decision::Accept(Handshake {
            admission: Arc::clone(self),
            origin,
            _permit: permit,
        })
    }

    /// Counts a new room against `origin`, or `None` when the address
    /// already has its share of open rooms.
    pub(crate) fn room(self: &Arc<Self>, origin: Origin) -> Option<RoomShare> {
        let mut held = self.lock();
        let entry = held.entry(origin).or_default();
        if entry.rooms >= self.limits.rooms_per_address {
            if entry.is_empty() {
                held.remove(&origin);
            }
            return None;
        }
        entry.rooms += 1;
        Some(RoomShare {
            admission: Arc::clone(self),
            origin,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Origin, Held>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn release(&self, origin: Origin, update: impl FnOnce(&mut Held)) {
        let mut held = self.lock();
        if let Some(entry) = held.get_mut(&origin) {
            update(entry);
            if entry.is_empty() {
                held.remove(&origin);
            }
        }
    }
}

/// An open room counted against the address it was created from; it stops
/// counting when the room drops it.
pub(crate) struct RoomShare {
    admission: Arc<Admission>,
    origin: Origin,
}

impl Drop for RoomShare {
    fn drop(&mut self) {
        self.admission.release(self.origin, |held| {
            held.rooms = held.rooms.saturating_sub(1);
        });
    }
}

/// A handshake in progress; it stops counting when dropped.
pub(crate) struct Handshake {
    admission: Arc<Admission>,
    origin: Origin,
    _permit: OwnedSemaphorePermit,
}

impl Handshake {
    /// Turns the handshake into a session, or `None` when the address
    /// already holds its share of sessions.
    pub(crate) fn into_session(self) -> Option<Session> {
        let mut held = self.admission.lock();
        let entry = held.entry(self.origin).or_default();
        if entry.sessions >= self.admission.limits.sessions_per_address {
            return None;
        }
        entry.sessions += 1;
        drop(held);
        Some(Session {
            admission: Arc::clone(&self.admission),
            origin: self.origin,
        })
    }
}

impl Drop for Handshake {
    fn drop(&mut self) {
        self.admission.release(self.origin, |held| {
            held.handshakes = held.handshakes.saturating_sub(1);
        });
    }
}

/// A session counted against its address; it stops counting when dropped.
pub(crate) struct Session {
    admission: Arc<Admission>,
    origin: Origin,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.admission.release(self.origin, |held| {
            held.sessions = held.sessions.saturating_sub(1);
        });
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv6Addr;

    use super::*;

    fn limits() -> Limits {
        Limits {
            handshakes: 8,
            handshakes_per_address: 2,
            sessions_per_address: 1,
            rooms_per_address: 2,
        }
    }

    #[test]
    fn an_address_has_only_so_many_open_rooms() {
        let admission = Admission::new(limits());
        let first = admission.room(HOME).unwrap();
        let _second = admission.room(HOME).unwrap();
        assert!(admission.room(HOME).is_none());
        assert!(admission.room(AWAY).is_some());
        drop(first);
        assert!(
            admission.room(HOME).is_some(),
            "a closed room frees its share"
        );
    }

    fn accept(decision: Decision) -> Handshake {
        match decision {
            Decision::Accept(handshake) => handshake,
            Decision::Retry => panic!("asked for a retry"),
            Decision::Refuse => panic!("refused"),
        }
    }

    const HOME: Origin = Origin::V4(Ipv4Addr::new(192, 0, 2, 1));
    const AWAY: Origin = Origin::V4(Ipv4Addr::new(192, 0, 2, 2));

    #[test]
    fn an_address_gets_its_share_of_handshakes_and_sessions() {
        let admission = Admission::new(limits());
        let first = accept(admission.on_attempt(HOME, true, true));
        let second = accept(admission.on_attempt(HOME, true, true));
        assert!(matches!(
            admission.on_attempt(HOME, true, true),
            Decision::Refuse
        ));
        // Another address is unaffected.
        let _away = accept(admission.on_attempt(AWAY, true, true));
        let session = first.into_session().expect("the first session");
        assert!(
            second.into_session().is_none(),
            "one session per address here"
        );
        drop(session);
        let third = accept(admission.on_attempt(HOME, true, true)).into_session();
        assert!(third.is_some(), "the slot came back");
        assert!(admission.lock().contains_key(&HOME));
    }

    #[test]
    fn released_addresses_are_forgotten() {
        let admission = Admission::new(limits());
        let session = accept(admission.on_attempt(HOME, true, true))
            .into_session()
            .unwrap();
        drop(session);
        assert!(admission.lock().is_empty());
    }

    #[test]
    fn under_load_unvalidated_peers_must_prove_their_address() {
        let admission = Admission::new(limits());
        let held: Vec<_> = (0..4u8)
            .map(|i| {
                let origin = Origin::V4(Ipv4Addr::new(198, 51, 100, i));
                accept(admission.on_attempt(origin, false, true))
            })
            .collect();
        assert!(matches!(
            admission.on_attempt(AWAY, false, true),
            Decision::Retry
        ));
        // A validated peer, or one that already retried, goes through.
        let _validated = accept(admission.on_attempt(AWAY, true, true));
        let _retried = accept(admission.on_attempt(HOME, false, false));
        drop(held);
    }

    #[test]
    fn the_server_wide_handshake_limit_holds() {
        let admission = Admission::new(Limits {
            handshakes: 2,
            handshakes_per_address: 8,
            sessions_per_address: 8,
            rooms_per_address: 8,
        });
        let _a = accept(admission.on_attempt(HOME, true, true));
        let _b = accept(admission.on_attempt(AWAY, true, true));
        assert!(matches!(
            admission.on_attempt(HOME, true, true),
            Decision::Refuse
        ));
    }

    #[test]
    fn an_ipv6_host_counts_as_its_slash_64() {
        let a = Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 0, 0, 0, 1);
        let b = Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 0xffff, 0, 0, 9);
        let c = Ipv6Addr::new(0x2001, 0xdb8, 1, 3, 0, 0, 0, 1);
        assert_eq!(Origin::of(a.into()), Origin::of(b.into()));
        assert_ne!(Origin::of(a.into()), Origin::of(c.into()));
        let mapped = Ipv4Addr::new(192, 0, 2, 1).to_ipv6_mapped();
        assert_eq!(Origin::of(mapped.into()), HOME);
    }
}
