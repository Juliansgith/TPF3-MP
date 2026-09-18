//! The server's rooms: creation, recovery, lookup and removal.

use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
};

use ring::hmac;
use tokio::sync::mpsc;
use tpf3mp_proto::{
    CreateRoom, FixedBytes, Invite, MAX_ROOM_MEMBERS, RequestError, RoomId, RoomView,
};
use tracing::{info, warn};

use crate::{
    metrics,
    room::{NewMember, ROOM_QUEUE, Room, RoomEnv, RoomHandle, RoomSecrets, RoomSpec},
    ruleset::RulesetFactory,
};

pub(crate) struct Directory {
    rooms: Mutex<HashMap<RoomId, RoomHandle>>,
    max_rooms: usize,
    key: hmac::Key,
    ruleset: RulesetFactory,
    env: RoomEnv,
}

pub(crate) struct DirectoryConfig {
    pub(crate) secret: [u8; 32],
    pub(crate) max_rooms: usize,
    pub(crate) ruleset: RulesetFactory,
    pub(crate) env: RoomEnv,
}

impl Directory {
    pub(crate) fn new(config: DirectoryConfig) -> Self {
        Self {
            rooms: Mutex::default(),
            max_rooms: config.max_rooms,
            key: hmac::Key::new(hmac::HMAC_SHA256, &config.secret),
            ruleset: config.ruleset,
            env: config.env,
        }
    }

    /// Restores every running room logged in the data directory. A log that
    /// cannot be recovered is renamed to `*.broken` and kept for diagnosis,
    /// never deleted. Returns how many rooms were restored.
    pub(crate) fn recover(self: &Arc<Self>) -> usize {
        let Some(dir) = &self.env.data_dir else {
            return 0;
        };
        let Ok(entries) = fs::read_dir(dir) else {
            return 0;
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "log"))
            .collect();
        paths.sort();
        let mut restored = 0;
        for path in paths {
            let recovered =
                Room::recover(&path, self.key.clone(), (self.ruleset)(), self.env.clone());
            match recovered {
                Ok(Some(room)) => {
                    info!(room = %room.id(), "restored a running room from its log");
                    self.register(room);
                    restored += 1;
                }
                Ok(None) => {}
                Err(error) => {
                    warn!(path = %path.display(), %error, "cannot restore a room; keeping its log aside");
                    let _ = fs::rename(&path, path.with_extension("broken"));
                }
            }
        }
        restored
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
            invite_tag: hmac::sign(&self.key, &RoomSecrets::invite_input(&id, &token))
                .as_ref()
                .to_vec(),
            password_tag: request.password.as_ref().map(|password| {
                hmac::sign(&self.key, &RoomSecrets::password_input(&id, password))
                    .as_ref()
                    .to_vec()
            }),
        };
        let room = Room::new(
            RoomSpec {
                id,
                name: request.name,
                max_players: request.max_players,
                settings: request.settings,
                secrets,
                ruleset: (self.ruleset)(),
                env: self.env.clone(),
            },
            owner,
        );
        let view = room.view();
        let (commands, receiver) = mpsc::channel(ROOM_QUEUE);
        let handle = RoomHandle::new(commands);
        rooms.insert(id, handle.clone());
        drop(rooms);
        metrics::increment(&self.env.metrics.rooms_created);
        tokio::spawn(room.run(receiver, Arc::clone(self)));
        Ok((handle, Invite { room: id, token }, view))
    }

    fn register(self: &Arc<Self>, room: Room) {
        let (commands, receiver) = mpsc::channel(ROOM_QUEUE);
        self.rooms
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(room.id(), RoomHandle::new(commands));
        tokio::spawn(room.run(receiver, Arc::clone(self)));
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
