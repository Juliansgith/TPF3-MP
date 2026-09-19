//! The server's rooms: creation, recovery, lookup and removal.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use ring::hmac;
use tokio::{sync::mpsc, task::JoinHandle};
use tpf3mp_proto::{
    CreateRoom, FixedBytes, Invite, MAX_ROOM_MEMBERS, RequestError, RoomId, RoomView,
};
use tracing::{info, warn};

use crate::{
    admission::RoomShare,
    metrics,
    room::{NewMember, ROOM_QUEUE, Room, RoomEnv, RoomHandle, RoomSecrets, RoomSpec},
    ruleset::RulesetFactory,
};

/// How long a shutdown waits for each room to finish.
const ROOM_SHUTDOWN: Duration = Duration::from_secs(5);

pub(crate) struct Directory {
    rooms: Mutex<HashMap<RoomId, Registered>>,
    max_rooms: usize,
    key: hmac::Key,
    ruleset: RulesetFactory,
    env: RoomEnv,
}

/// A room the directory knows: the way to reach it, and its task.
struct Registered {
    handle: RoomHandle,
    task: JoinHandle<()>,
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
    /// cannot be recovered is renamed to `*.broken`, unmodified, and kept
    /// for diagnosis, never deleted. Returns how many rooms were restored.
    pub(crate) fn recover(self: &Arc<Self>) -> usize {
        let mut held = Vec::new();
        let restored = self.recover_rooms(&mut held);
        // Snapshots of rooms that are gone would otherwise stay forever.
        if let Some(snapshots) = &self.env.snapshots {
            snapshots.release_all_but(&held);
        }
        restored
    }

    /// Restores the logged rooms, noting the snapshots they hold.
    fn recover_rooms(self: &Arc<Self>, held: &mut Vec<tpf3mp_snapshot::ManifestId>) -> usize {
        let Some(dir) = &self.env.data_dir else {
            return 0;
        };
        let Ok(entries) = fs::read_dir(dir) else {
            return 0;
        };
        // Only regular files: a planted link must not lead recovery, which
        // may cut a torn record, to some other file.
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
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
                    held.extend(room.held_snapshots());
                    self.register(room);
                    restored += 1;
                }
                Ok(None) => {}
                Err(error) => {
                    warn!(path = %path.display(), %error, "cannot restore a room; keeping its log aside");
                    set_aside(&path);
                }
            }
        }
        restored
    }

    /// Creates a room with `owner` as its first member and starts its task.
    /// The room holds `share` until it closes.
    pub(crate) fn create(
        self: &Arc<Self>,
        owner: NewMember,
        request: CreateRoom,
        share: RoomShare,
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
                share,
            },
            owner,
        );
        let view = room.view();
        let (commands, receiver) = mpsc::channel(ROOM_QUEUE);
        let handle = RoomHandle::new(commands);
        let task = tokio::spawn(room.run(receiver, Arc::clone(self)));
        rooms.insert(
            id,
            Registered {
                handle: handle.clone(),
                task,
            },
        );
        drop(rooms);
        metrics::increment(&self.env.metrics.rooms_created);
        Ok((handle, Invite { room: id, token }, view))
    }

    fn register(self: &Arc<Self>, room: Room) {
        let (commands, receiver) = mpsc::channel(ROOM_QUEUE);
        let id = room.id();
        let task = tokio::spawn(room.run(receiver, Arc::clone(self)));
        self.rooms
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                id,
                Registered {
                    handle: RoomHandle::new(commands),
                    task,
                },
            );
    }

    pub(crate) fn get(&self, id: &RoomId) -> Option<RoomHandle> {
        self.rooms
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .map(|registered| registered.handle.clone())
    }

    /// Stops every room once its connections are gone: without the
    /// directory's handles, a room's queue closes and its task ends, closing
    /// its log and letting go of the snapshot store. Waits for each a while.
    pub(crate) async fn shut_down(&self) {
        let registered: Vec<Registered> = self
            .rooms
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
            .map(|(_, registered)| registered)
            .collect();
        for Registered { handle, task } in registered {
            drop(handle);
            if tokio::time::timeout(ROOM_SHUTDOWN, task).await.is_err() {
                warn!("a room did not stop in time");
            }
        }
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

/// Renames a log that cannot be restored to `<room>.broken`, or
/// `<room>.<n>.broken` if that exists, so an earlier one is never replaced.
fn set_aside(path: &Path) {
    for attempt in 0..1000 {
        let aside = if attempt == 0 {
            path.with_extension("broken")
        } else {
            path.with_extension(format!("{attempt}.broken"))
        };
        if aside.exists() {
            continue;
        }
        if let Err(error) = fs::rename(path, &aside) {
            warn!(path = %path.display(), %error, "cannot set a broken log aside");
        }
        return;
    }
    warn!(path = %path.display(), "too many broken logs of one room; leaving this one in place");
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).expect("the operating system's random source is available");
    bytes
}
