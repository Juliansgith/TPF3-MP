//! The launcher: a page in the player's browser from which they connect,
//! create or join a room, get ready, chat and play, with this agent doing
//! the work. It is the launcher backend of `docs/ARCHITECTURE.md`; an
//! in-game interface can drive the same actions later.
//!
//! The page is served on the loopback interface only. Every API request
//! carries a secret token that only the launched page knows, and requests
//! for any host but the loopback address are refused, so neither other web
//! pages nor DNS rebinding can drive it.

mod api;
mod http;

use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use serde::{Deserialize, Serialize};

use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tpf3mp_net::{Identity, ServerTrust};
use tpf3mp_proto::{
    ContentFingerprint, CreateRoom, Invite, JoinRoom, RoomPhase, RoomSettings, Text,
};
use tracing::{info, warn};

pub use self::api::Action;
use self::api::View;
use crate::{
    Client, ClientEvent, ConnectOptions, Events, TunnelChoice, Worlds,
    bridge::{self, Bridge, BridgeEnd, BridgeOptions, Control, Rejoin, SharedStatus, Status},
    connect,
};

/// How long the launcher keeps trying to rejoin a room after losing the
/// server.
const REJOIN_PATIENCE: Duration = Duration::from_secs(300);
/// Actions queued from the page before it waits.
const ACTION_QUEUE: usize = 32;

/// What a launcher needs.
#[derive(Debug, Clone)]
pub struct LauncherConfig {
    /// Where the page is served: a loopback address.
    pub listen: SocketAddr,
    /// Which tunnel connections take when UDP does not get through.
    pub tunnel: TunnelChoice,
    /// Where the server and name of each connection are remembered for the
    /// next run (see [`Remembered`]).
    pub remember: Option<PathBuf>,
    /// The server the page offers first, as `host:port`.
    pub server: Option<String>,
    /// How to trust servers.
    pub trust: ServerTrust,
    pub identity: Arc<Identity>,
    /// The name the page offers first.
    pub name: String,
    /// What this player's game runs.
    pub content: ContentFingerprint,
    /// The shared-memory link the game's hook opens.
    pub link: String,
    pub worlds: Worlds,
    /// The settings of rooms this player creates.
    pub room_settings: RoomSettings,
}

/// A running launcher.
pub struct Launcher {
    url: String,
    task: JoinHandle<()>,
}

impl Launcher {
    /// Starts serving the page and returns once it is reachable.
    pub async fn start(config: LauncherConfig) -> std::io::Result<Self> {
        if !config.listen.ip().is_loopback() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the launcher serves the loopback interface only",
            ));
        }
        let listener = TcpListener::bind(config.listen).await?;
        let address = listener.local_addr()?;
        let token = random_token();
        let (actions, actions_rx) = mpsc::channel(ACTION_QUEUE);
        let shared = Arc::new(Shared {
            token: token.clone(),
            address,
            view: Mutex::new(View {
                server: config.server.clone(),
                name: config.name.clone(),
                player: Some(config.identity.player()),
                ..View::default()
            }),
            status: SharedStatus::default(),
            actions,
        });
        let control = tokio::spawn(control(Arc::clone(&shared), config, actions_rx));
        let serve = tokio::spawn(http::serve(listener, Arc::clone(&shared)));
        let task = tokio::spawn(async move {
            let _ = tokio::join!(control, serve);
        });
        Ok(Self {
            url: format!("http://{address}/#{token}"),
            task,
        })
    }

    /// The page's address, with the token it needs.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Runs until the task ends, which it does only if both halves stop.
    pub async fn wait(mut self) {
        let _ = (&mut self.task).await;
    }
}

impl Drop for Launcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// What the page's requests and the controller share.
pub(crate) struct Shared {
    token: String,
    address: SocketAddr,
    view: Mutex<View>,
    status: SharedStatus,
    actions: mpsc::Sender<(Action, oneshot::Sender<Result<(), String>>)>,
}

impl Shared {
    fn view(&self) -> std::sync::MutexGuard<'_, View> {
        self.view.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn status(&self) -> std::sync::MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A connection not in any room.
struct Connected {
    client: Client,
    events: Events,
    options: ConnectOptions,
}

/// A room session, run by a bridge.
struct Session {
    controls: mpsc::Sender<Control>,
    task: JoinHandle<Result<BridgeEnd, bridge::BridgeFault>>,
    options: ConnectOptions,
}

/// Carries out the page's actions, one at a time, and keeps the view.
async fn control(
    shared: Arc<Shared>,
    config: LauncherConfig,
    mut actions: mpsc::Receiver<(Action, oneshot::Sender<Result<(), String>>)>,
) {
    let mut connected: Option<Connected> = None;
    let mut session: Option<Session> = None;
    loop {
        tokio::select! {
            action = actions.recv() => {
                let Some((action, reply)) = action else {
                    return;
                };
                let result = act(&shared, &config, action, &mut connected, &mut session).await;
                if let Err(error) = &result {
                    shared.view().error = Some(error.clone());
                }
                let _ = reply.send(result);
            }
            ended = session_end(&mut session) => {
                let finished = session.take();
                let message = match ended {
                    Ok(end) => format!("the game session ended: {}", describe(&end)),
                    Err(fault) => format!("the game session failed: {fault}"),
                };
                info!(%message);
                shared.status().notice(message);
                {
                    let mut view = shared.view();
                    view.invite = None;
                    view.in_room = false;
                }
                // Back to the server, ready for the next room.
                if let Some(finished) = finished {
                    connected = reconnect(&shared, finished.options).await;
                }
            }
            event = next_event(&mut connected) => match event {
                Some(ClientEvent::Closed(reason)) => {
                    connected = None;
                    let mut view = shared.view();
                    view.connected = false;
                    view.error = Some(format!("disconnected: {reason}"));
                }
                // Outside a room there is nothing else to hear.
                Some(_) => {}
                None => {
                    connected = None;
                    shared.view().connected = false;
                }
            },
        }
    }
}

/// One action from the page.
async fn act(
    shared: &Arc<Shared>,
    config: &LauncherConfig,
    action: Action,
    connected: &mut Option<Connected>,
    session: &mut Option<Session>,
) -> Result<(), String> {
    match action {
        Action::Connect { server, name } => {
            if session.is_some() {
                return Err("leave the room first".into());
            }
            let name = Text::new(name.trim()).map_err(|_| "that name is too long".to_owned())?;
            if name.as_str().is_empty() {
                return Err("choose a name".into());
            }
            // A whole invite, as "Copy invite" gives it, connects and joins.
            let passed = passed_invite(&server);
            let server = match &passed {
                Some(Passed {
                    server: Some(server),
                    ..
                }) => server.clone(),
                Some(Passed { server: None, .. }) => {
                    return Err("that is an invite: put the server's address before it".into());
                }
                None => server.trim().to_owned(),
            };
            connect_to(shared, config, connected, &server, name).await?;
            match passed {
                Some(passed) => join(shared, config, connected, session, passed.invite, None).await,
                None => Ok(()),
            }
        }
        Action::Disconnect => {
            if let Some(session) = session.take() {
                let _ = session.controls.send(Control::Leave).await;
                let _ = session.task.await;
            }
            if let Some(connected) = connected.take() {
                connected.client.close().await;
            }
            let mut view = shared.view();
            view.connected = false;
            view.in_room = false;
            view.invite = None;
            Ok(())
        }
        Action::Create {
            room,
            max_players,
            password,
            rules,
        } => {
            let current = connected.as_ref().ok_or("connect to a server first")?;
            let rules = match rules.as_deref().map(str::trim) {
                None | Some("") => None,
                Some(name) => Some(Text::new(name).map_err(|_| "no such rules".to_owned())?),
            };
            let create = CreateRoom {
                name: Text::new(room.trim())
                    .map_err(|_| "that room name is too long".to_owned())?,
                max_players,
                password: password_text(password)?,
                settings: config.room_settings,
                rules,
            };
            let (invite, _room) = current
                .client
                .create_room(create.clone())
                .await
                .map_err(|error| error.to_string())?;
            current
                .client
                .declare_content(config.content)
                .await
                .map_err(|error| error.to_string())?;
            begin_session(shared, config, connected, session, invite, create.password)
        }
        Action::Join { invite, password } => {
            let current = connected.as_ref().ok_or("connect to a server first")?;
            let passed = passed_invite(&invite).ok_or("that is not an invite")?;
            let password = password_text(password)?;
            // An invite to another server takes the player there first.
            let here = shared.view().server.clone();
            if let Some(server) = passed.server
                && !here.is_some_and(|here| here.eq_ignore_ascii_case(&server))
            {
                let name = current.options.name.clone();
                connect_to(shared, config, connected, &server, name).await?;
            }
            join(shared, config, connected, session, passed.invite, password).await
        }
        Action::Ready { ready } => forward(session, Control::Ready(ready)).await,
        Action::Start => forward(session, Control::Start).await,
        Action::Speed { percent } => {
            if percent > tpf3mp_proto::Speed::MAX.0 {
                return Err("that speed is too fast".into());
            }
            forward(session, Control::Speed(tpf3mp_proto::Speed(percent))).await
        }
        Action::Kick { player } => {
            let player = api::parse_player(&player).ok_or("that is not a player")?;
            forward(session, Control::Kick(player)).await
        }
        Action::Chat { text } => {
            let text = Text::new(text.trim()).map_err(|_| "that message is too long".to_owned())?;
            if text.as_str().is_empty() {
                return Ok(());
            }
            forward(session, Control::Chat(text)).await
        }
        Action::Leave => forward(session, Control::Leave).await,
    }
}

/// Hands the connection to a bridge, which runs the room from its lobby to
/// the end of its game, and plays it through the game's hook.
fn begin_session(
    shared: &Arc<Shared>,
    config: &LauncherConfig,
    connected: &mut Option<Connected>,
    session: &mut Option<Session>,
    invite: Invite,
    password: Option<Text<64>>,
) -> Result<(), String> {
    let Connected {
        client,
        events,
        options,
    } = connected.take().ok_or("not connected")?;
    let link = tpf3mp_ipc::Link::create(
        &tpf3mp_ipc::Config::new(&config.link),
        tpf3mp_ipc::Role::Agent,
    )
    .map_err(|error| format!("cannot open the link to the game: {error}"))?;
    let (controls, controls_rx) = mpsc::channel(ACTION_QUEUE);
    // A fresh status for the new session, with the room already known.
    {
        let mut status = shared.status();
        let room = status.room.take();
        *status = Status {
            room,
            ..Status::default()
        };
    }
    let bridge_options = BridgeOptions {
        worlds: Some(config.worlds.clone()),
        status: Some(Arc::clone(&shared.status)),
        ..BridgeOptions::default()
    };
    let rejoin = Rejoin {
        options: options.clone(),
        invite: invite.clone(),
        password,
        content: Some(config.content),
        give_up_after: REJOIN_PATIENCE,
    };
    let task = tokio::spawn(async move {
        let mut bridge = Bridge::new(link, bridge_options).with_controls(controls_rx);
        bridge::play(&mut bridge, client, events, &rejoin).await
    });
    {
        let mut view = shared.view();
        // Friends need the server too: "Copy invite" gives both.
        view.invite = Some(match &view.server {
            Some(server) => format!("{server} {invite}"),
            None => invite.to_string(),
        });
        view.in_room = true;
        view.error = None;
    }
    *session = Some(Session {
        controls,
        task,
        options,
    });
    Ok(())
}

async fn forward(session: &Option<Session>, control: Control) -> Result<(), String> {
    let session = session.as_ref().ok_or("join a room first")?;
    session
        .controls
        .send(control)
        .await
        .map_err(|_| "the room session has ended".to_owned())
}

/// Connects again after a room session, which took the old connection.
async fn reconnect(shared: &Arc<Shared>, options: ConnectOptions) -> Option<Connected> {
    match connect(options.clone()).await {
        Ok((client, events)) => {
            let mut view = shared.view();
            view.connected = true;
            view.tunneled = client.tunneled();
            drop(view);
            Some(Connected {
                client,
                events,
                options,
            })
        }
        Err(error) => {
            warn!(%error, "cannot reconnect after the session");
            let mut view = shared.view();
            view.connected = false;
            view.error = Some(error.to_string());
            None
        }
    }
}

/// Connects to `server` as `name`, replacing any connection.
async fn connect_to(
    shared: &Arc<Shared>,
    config: &LauncherConfig,
    connected: &mut Option<Connected>,
    server: &str,
    name: Text<32>,
) -> Result<(), String> {
    let options = connect_options(config, server, name).await?;
    *connected = None;
    {
        let mut view = shared.view();
        view.connecting = true;
        view.server = Some(server.to_owned());
    }
    let result = connect(options.clone()).await;
    let mut view = shared.view();
    view.connecting = false;
    let (client, events) = result.map_err(|error| error.to_string())?;
    view.connected = true;
    view.tunneled = client.tunneled();
    view.error = None;
    view.name = options.name.as_str().to_owned();
    view.server_version = Some(client.welcome().server_version.as_str().to_owned());
    view.rules = client.welcome().rules.clone();
    drop(view);
    if let Some(file) = &config.remember {
        let remembered = Remembered {
            server: Some(server.to_owned()),
            name: Some(options.name.as_str().to_owned()),
        };
        if let Err(error) = remembered.save(file) {
            warn!(%error, "cannot remember the server and name for next time");
        }
    }
    *connected = Some(Connected {
        options: options.again_after(&client),
        client,
        events,
    });
    Ok(())
}

/// What the launcher remembers between runs: the server and the name the
/// player last connected with, which the page then offers first.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remembered {
    pub server: Option<String>,
    pub name: Option<String>,
}

impl Remembered {
    /// What `file` holds, or nothing if it is missing or not ours.
    pub fn load(file: &Path) -> Self {
        let small = fs::metadata(file).is_ok_and(|metadata| metadata.len() <= 4096);
        small
            .then(|| fs::read(file).ok())
            .flatten()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self, file: &Path) -> std::io::Result<()> {
        if let Some(dir) = file.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(file, serde_json::to_vec_pretty(self)?)
    }
}

/// Joins the room of `invite` on the current connection.
async fn join(
    shared: &Arc<Shared>,
    config: &LauncherConfig,
    connected: &mut Option<Connected>,
    session: &mut Option<Session>,
    invite: Invite,
    password: Option<Text<64>>,
) -> Result<(), String> {
    let current = connected.as_ref().ok_or("connect to a server first")?;
    let room = current
        .client
        .join_room(JoinRoom {
            invite: invite.clone(),
            password: password.clone(),
            resume: None,
            content: Some(config.content),
        })
        .await
        .map_err(|error| error.to_string())?;
    if room.phase == RoomPhase::Lobby {
        current
            .client
            .declare_content(config.content)
            .await
            .map_err(|error| error.to_string())?;
    }
    shared.status().room = Some(room);
    begin_session(shared, config, connected, session, invite, password)
}

/// An invite as players pass it on: the room's invite, perhaps with the
/// server's address before it, as "Copy invite" gives it, inside whatever
/// message it came in.
#[derive(Debug, PartialEq, Eq)]
struct Passed {
    server: Option<String>,
    invite: Invite,
}

fn passed_invite(text: &str) -> Option<Passed> {
    let tokens: Vec<&str> = text
        .split_whitespace()
        .map(|token| {
            token.trim_matches(|c: char| {
                matches!(
                    c,
                    '"' | '\'' | '`' | '<' | '>' | '(' | ')' | ',' | ';' | '*'
                )
            })
        })
        .collect();
    let (at, invite) = tokens
        .iter()
        .enumerate()
        .find_map(|(at, token)| token.parse::<Invite>().ok().map(|invite| (at, invite)))?;
    let server = tokens[..at]
        .iter()
        .rev()
        .find(|token| names_a_server(token))
        .map(|token| (*token).to_owned());
    Some(Passed { server, invite })
}

/// Whether `token` reads as `host:port`, the host a name or an address, as
/// in `tpf3mp.example.org:29470` or `[2001:db8::1]:29470`: not a time of
/// day like `12:30`.
fn names_a_server(token: &str) -> bool {
    token.rsplit_once(':').is_some_and(|(host, port)| {
        port.parse::<u16>().is_ok_and(|port| port > 0)
            && host.contains(|c: char| c == '.' || c == '[' || c.is_ascii_alphabetic())
    })
}

async fn connect_options(
    config: &LauncherConfig,
    server: &str,
    name: Text<32>,
) -> Result<ConnectOptions, String> {
    let server = server.trim();
    let (host, _port) = server
        .rsplit_once(':')
        .ok_or("the server address must be host:port")?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let address = crate::resolve(server)
        .await
        .map_err(|error| format!("cannot find {server}: {error}"))?;
    let mut options = ConnectOptions::new(
        address,
        host,
        config.trust.clone(),
        Arc::clone(&config.identity),
        name,
    );
    options.route = config
        .tunnel
        .route(host)
        .map_err(|error| error.to_string())?;
    Ok(options)
}

fn password_text(password: Option<String>) -> Result<Option<Text<64>>, String> {
    password
        .filter(|password| !password.is_empty())
        .map(|password| Text::new(password).map_err(|_| "that password is too long".to_owned()))
        .transpose()
}

/// The end of the current session, or never without one.
async fn session_end(session: &mut Option<Session>) -> Result<BridgeEnd, bridge::BridgeFault> {
    match session {
        Some(session) => match (&mut session.task).await {
            Ok(ended) => ended,
            Err(error) => Err(bridge::BridgeFault::Rejoin(error.to_string())),
        },
        None => std::future::pending().await,
    }
}

/// The next event of a connection not in a room, or never without one.
async fn next_event(connected: &mut Option<Connected>) -> Option<ClientEvent> {
    match connected {
        Some(connected) => connected.events.recv().await,
        None => std::future::pending().await,
    }
}

fn describe(end: &BridgeEnd) -> String {
    match end {
        BridgeEnd::Closed(reason) => format!("the connection closed ({reason})"),
        BridgeEnd::Kicked => "the owner removed you from the room".into(),
        BridgeEnd::EventsEnded => "the connection ended".into(),
        BridgeEnd::WorldUnavailable => "the world could not be fetched".into(),
        BridgeEnd::Left => "you left the room".into(),
    }
}

fn random_token() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the operating system's random source is available");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use tpf3mp_proto::{FixedBytes, RoomId};

    use super::*;

    fn invite() -> Invite {
        Invite {
            room: RoomId(FixedBytes([5; 16])),
            token: FixedBytes([6; 32]),
        }
    }

    #[test]
    fn the_server_and_name_are_remembered_for_next_time() {
        let dir = std::env::temp_dir().join(format!("tpf3mp-remember-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let file = dir.join("launcher.json");
        assert_eq!(Remembered::load(&file), Remembered::default());
        let remembered = Remembered {
            server: Some("tpf3mp.example.org:29470".into()),
            name: Some("Ann".into()),
        };
        remembered.save(&file).unwrap();
        assert_eq!(Remembered::load(&file), remembered);
        fs::write(&file, b"not json").unwrap();
        assert_eq!(Remembered::load(&file), Remembered::default());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_invite_is_found_in_whatever_message_it_came_in() {
        let code = invite().to_string();
        let passed = |text: String| passed_invite(&text);
        let at = |server: &str| {
            Some(Passed {
                server: Some(server.to_owned()),
                invite: invite(),
            })
        };
        // As "Copy invite" gives it.
        assert_eq!(
            passed(format!("tpf3mp.example.org:29470 {code}")),
            at("tpf3mp.example.org:29470")
        );
        // Pasted from a chat, formatted.
        assert_eq!(
            passed(format!(
                "join us at `play.example.net:29470` with \"{code}\", at 12:30!"
            )),
            at("play.example.net:29470")
        );
        assert_eq!(
            passed(format!("[2001:db8::1]:29470 {code}")),
            at("[2001:db8::1]:29470")
        );
        assert_eq!(
            passed(format!("localhost:29470\n{code}")),
            at("localhost:29470")
        );
        // A time of day is no server.
        assert_eq!(
            passed(format!("at 12:30 {code}")),
            Some(Passed {
                server: None,
                invite: invite(),
            })
        );
        // The bare invite, and no invite at all.
        assert_eq!(passed(code.clone()).map(|p| p.server), Some(None));
        assert_eq!(passed("tpf3mp.example.org:29470".into()), None);
        // A cut-off invite is none.
        assert_eq!(passed(code[..code.len() - 4].to_owned()), None);
    }
}
