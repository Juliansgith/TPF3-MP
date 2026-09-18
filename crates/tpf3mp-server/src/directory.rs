//! The server's rooms: creation, lookup and removal.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use ring::hmac;
use tokio::sync::mpsc;
use tpf3mp_proto::{
    CreateRoom, FixedBytes, Invite, MAX_ROOM_MEMBERS, RequestError, RoomId, RoomView,
};

use crate::{
    room::{NewMember, ROOM_QUEUE, Room, RoomHandle, RoomSecrets, RoomSpec},
    ruleset::RulesetFactory,
};

pub(crate) struct Directory {
    rooms: Mutex<HashMap<RoomId, RoomHandle>>,
    max_rooms: usize,
    key: hmac::Key,
    ruleset: RulesetFactory,
    tick: Duration,
}

impl Directory {
    pub(crate) fn new(
        secret: &[u8; 32],
        max_rooms: usize,
        ruleset: RulesetFactory,
        tick: Duration,
    ) -> Self {
        Self {
            rooms: Mutex::default(),
            max_rooms,
            key: hmac::Key::new(hmac::HMAC_SHA256, secret),
            ruleset,
            tick,
        }
    }

    /// Creates a room with `owner` as its first member and starts its task.
    pub(crate) fn create(
        self: &Arc<Self>,
        owner: NewMember,
        request: CreateRoom,
    ) -> Result<(RoomHandle, Invite, RoomView), RequestError> {
        if !request.settings.is_valid() || !(1..=MAX_ROOM_MEMBERS).contains(&request.max_players) {
            return Err(RequestError::InvalidSettings);
        }
        let mut rooms = self.rooms.lock().unwrap_or_else(PoisonError::into_inner);
        if rooms.len() >= self.max_rooms {
            return Err(RequestError::TooManyRooms);
        }
        let id = loop {
            let id = RoomId(FixedBytes(random()));
            if !rooms.contains_key(&id) {
                break id;
            }
        };
        let token = FixedBytes(random());
        let secrets = RoomSecrets {
            key: self.key.clone(),
            invite_tag: hmac::sign(&self.key, &RoomSecrets::invite_input(&id, &token)),
            password_tag: request
                .password
                .as_ref()
                .map(|password| hmac::sign(&self.key, &RoomSecrets::password_input(&id, password))),
        };
        let room = Room::new(
            RoomSpec {
                id,
                name: request.name,
                max_players: request.max_players,
                settings: request.settings,
                secrets,
                ruleset: (self.ruleset)(),
                tick: self.tick,
            },
            owner,
        );
        let view = room.view();
        let (commands, receiver) = mpsc::channel(ROOM_QUEUE);
        let handle = RoomHandle::new(commands);
        rooms.insert(id, handle.clone());
        drop(rooms);
        tokio::spawn(room.run(receiver, Arc::clone(self)));
        Ok((handle, Invite { room: id, token }, view))
    }

    pub(crate) fn get(&self, id: &RoomId) -> Option<RoomHandle> {
        self.rooms
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    pub(crate) fn remove(&self, id: &RoomId) {
        self.rooms
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
    }

    pub(crate) fn len(&self) -> usize {
        self.rooms
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).expect("the operating system's random source is available");
    bytes
}
