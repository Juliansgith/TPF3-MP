//! What the page reads (the state, as JSON) and what it asks (actions).

use serde::{Deserialize, Serialize};
use tpf3mp_proto::{Arch, FixedBytes, Os, Platform, PlayerId, RoomPhase};

use crate::bridge::{Status, WorldStatus};

/// What the launcher itself knows, next to the session's [`Status`].
#[derive(Debug, Default)]
pub(crate) struct View {
    pub(crate) server: Option<String>,
    pub(crate) name: String,
    pub(crate) player: Option<PlayerId>,
    pub(crate) connecting: bool,
    pub(crate) connected: bool,
    /// The connection runs through a tunnel, not over UDP.
    pub(crate) tunneled: bool,
    pub(crate) server_version: Option<String>,
    pub(crate) in_room: bool,
    pub(crate) invite: Option<String>,
    pub(crate) error: Option<String>,
}

/// Something the page asks for.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Connect {
        server: String,
        name: String,
    },
    Disconnect,
    Create {
        room: String,
        max_players: u8,
        password: Option<String>,
    },
    Join {
        invite: String,
        password: Option<String>,
    },
    Ready {
        ready: bool,
    },
    Start,
    Speed {
        percent: u16,
    },
    Kick {
        player: String,
    },
    Chat {
        text: String,
    },
    Leave,
}

#[derive(Serialize)]
struct State<'a> {
    name: &'a str,
    player: Option<String>,
    server: Option<&'a str>,
    server_version: Option<&'a str>,
    connection: &'static str,
    tunneled: bool,
    error: Option<&'a str>,
    room: Option<Room>,
    game: Game,
    chat: Vec<Chat>,
    notices: Vec<&'a str>,
}

#[derive(Serialize)]
struct Room {
    name: String,
    phase: &'static str,
    invite: Option<String>,
    you_own: bool,
    max_players: u8,
    has_password: bool,
    members: Vec<Member>,
}

#[derive(Serialize)]
struct Member {
    id: String,
    name: String,
    platform: String,
    ready: bool,
    connected: bool,
    owner: bool,
    you: bool,
}

#[derive(Serialize)]
struct Game {
    attached: Option<String>,
    world: &'static str,
    bytes: u64,
    total: u64,
    step: Option<u64>,
    speed: u16,
}

#[derive(Serialize)]
struct Chat {
    from: String,
    text: String,
    you: bool,
}

/// The state the page shows, as JSON.
pub(crate) fn render(view: &View, status: &Status) -> String {
    let you = view.player;
    let connection = if view.connected {
        "connected"
    } else if view.connecting {
        "connecting"
    } else {
        "disconnected"
    };
    let room = status
        .room
        .as_ref()
        .filter(|_| view.in_room)
        .map(|room| Room {
            name: room.name.as_str().to_owned(),
            phase: match room.phase {
                RoomPhase::Lobby => "lobby",
                RoomPhase::Running => "running",
            },
            invite: view.invite.clone(),
            you_own: Some(room.owner) == you,
            max_players: room.max_players,
            has_password: room.has_password,
            members: room
                .members
                .iter()
                .map(|member| Member {
                    id: player_hex(&member.player),
                    name: member.name.as_str().to_owned(),
                    platform: platform_name(member.platform),
                    ready: member.ready,
                    connected: member.connected,
                    owner: member.player == room.owner,
                    you: Some(member.player) == you,
                })
                .collect(),
        });
    let (world, bytes, total) = match status.world {
        WorldStatus::None => ("none", 0, 0),
        WorldStatus::Fetching { bytes, total } => ("fetching", bytes, total),
        WorldStatus::Loading => ("loading", 0, 0),
        WorldStatus::Playing => ("playing", 0, 0),
    };
    let name_of = |player: &PlayerId| {
        status
            .room
            .as_ref()
            .and_then(|room| room.members.iter().find(|member| member.player == *player))
            .map_or_else(
                || player.to_string(),
                |member| member.name.as_str().to_owned(),
            )
    };
    let state = State {
        name: &view.name,
        player: you.map(|player| player.to_string()),
        server: view.server.as_deref(),
        server_version: view.server_version.as_deref(),
        connection,
        tunneled: view.connected && view.tunneled,
        error: view.error.as_deref(),
        room,
        game: Game {
            attached: status.game.clone(),
            world,
            bytes,
            total,
            step: status.step,
            speed: status.speed.0,
        },
        chat: status
            .chat
            .iter()
            .map(|(from, text)| Chat {
                from: name_of(from),
                text: text.as_str().to_owned(),
                you: Some(*from) == you,
            })
            .collect(),
        notices: status.notices.iter().map(String::as_str).collect(),
    };
    serde_json::to_string(&state).unwrap_or_else(|_| "{}".to_owned())
}

/// A player's full key as 64 hex digits, as the page names players.
pub(crate) fn player_hex(player: &PlayerId) -> String {
    player
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A player named by [`player_hex`].
pub(crate) fn parse_player(text: &str) -> Option<PlayerId> {
    let text = text.trim().trim_start_matches("p-");
    if text.len() != 64 || !text.is_ascii() {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(PlayerId(FixedBytes(bytes)))
}

fn platform_name(platform: Platform) -> String {
    let os = match platform.os {
        Os::Windows => "Windows",
        Os::Linux => "Linux",
        Os::MacOs => "macOS",
        Os::Other => "other",
    };
    let arch = match platform.arch {
        Arch::X86_64 => "x86-64",
        Arch::Aarch64 => "arm64",
        Arch::Other => "other",
    };
    format!("{os} {arch}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_parse_from_the_pages_json() {
        let action: Action =
            serde_json::from_str(r#"{"action":"join","invite":"TPF3MP1.x","password":null}"#)
                .unwrap();
        assert_eq!(
            action,
            Action::Join {
                invite: "TPF3MP1.x".into(),
                password: None
            }
        );
        let action: Action = serde_json::from_str(r#"{"action":"start"}"#).unwrap();
        assert_eq!(action, Action::Start);
        assert!(serde_json::from_str::<Action>(r#"{"action":"format_disk"}"#).is_err());
    }

    #[test]
    fn players_round_trip_through_their_page_names() {
        let player = PlayerId(FixedBytes([0xab; 32]));
        let hex = player_hex(&player);
        assert_eq!(hex.len(), 64);
        assert_eq!(parse_player(&hex), Some(player));
        assert_eq!(parse_player(&format!("p-{hex}")), Some(player));
        assert_eq!(parse_player("abc"), None);
        assert_eq!(parse_player(&"zz".repeat(32)), None);
    }

    #[test]
    fn the_state_names_the_connection_and_the_room() {
        let view = View {
            name: "Ann".into(),
            connected: true,
            ..View::default()
        };
        let json: serde_json::Value =
            serde_json::from_str(&render(&view, &Status::default())).unwrap();
        assert_eq!(json["connection"], "connected");
        assert_eq!(json["tunneled"], false);
        assert_eq!(json["name"], "Ann");
        assert!(json["room"].is_null());
        assert_eq!(json["game"]["world"], "none");
    }
}
