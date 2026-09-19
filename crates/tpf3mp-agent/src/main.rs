use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use clap::{Args as ClapArgs, Parser, Subcommand};
use tpf3mp_agent::{
    Client, ClientEvent, ConnectOptions, Events, Worlds,
    bridge::{self, Bridge, BridgeOptions, Rejoin},
    connect,
};
use tpf3mp_net::{CertificateDer, Identity, ServerTrust};
use tpf3mp_proto::{
    ContentFingerprint, CreateRoom, FixedBytes, Invite, JoinRoom, RoomPhase, RoomSettings,
    RoomView, Text,
};
use tracing_subscriber::EnvFilter;

/// The TPF3-MP agent, which connects the game to a TPF3-MP server.
#[derive(Debug, Parser)]
#[command(version)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Connect to a server, complete the handshake and report the session.
    Connect(Server),
    /// Create a room, print its invite, and follow it until Ctrl-C.
    Host {
        #[command(flatten)]
        server: Server,
        #[command(flatten)]
        game: Game,
        /// Room name shown to players.
        #[arg(long, default_value = "TPF3-MP room")]
        room_name: String,
        /// Require this password in addition to the invite.
        #[arg(long)]
        password: Option<String>,
        #[arg(long, default_value_t = 8)]
        max_players: u8,
        /// Start the game once this many players are in the room and ready.
        #[arg(long)]
        start_with: Option<usize>,
    },
    /// Join a room with an invite and follow it until Ctrl-C.
    Join {
        #[command(flatten)]
        server: Server,
        #[command(flatten)]
        game: Game,
        invite: String,
        #[arg(long)]
        password: Option<String>,
    },
}

#[derive(Debug, ClapArgs)]
struct Game {
    /// Play the room through the game: create the shared-memory link of
    /// this name, which the game's hook opens, and bridge the two. Without
    /// a name, the link the hook opens by default.
    #[arg(long, num_args = 0..=1, default_missing_value = tpf3mp_bridge::DEFAULT_LINK)]
    game_link: Option<String>,

    /// What this player's game runs (build and mods). Every player in a
    /// room must declare the same.
    #[arg(long, default_value = "tpf3")]
    content: String,

    /// Where worlds are kept: saves the room agreed on, and worlds received
    /// to join running games. Defaults to a directory per game link in the
    /// per-user data directory; two agents cannot share one.
    #[arg(long)]
    worlds: Option<PathBuf>,

    /// Space worlds may take, in GiB.
    #[arg(long, default_value_t = 8)]
    worlds_gib: u64,
}

impl Game {
    /// The digest of what this player's game runs.
    fn fingerprint(&self) -> Result<ContentFingerprint> {
        let digest = ring::digest::digest(&ring::digest::SHA256, self.content.as_bytes());
        let bytes: [u8; 32] = digest
            .as_ref()
            .try_into()
            .context("a SHA-256 digest is 32 bytes")?;
        Ok(ContentFingerprint(FixedBytes(bytes)))
    }

    fn open_worlds(&self, link: &str) -> Result<Worlds> {
        let dir = match &self.worlds {
            Some(dir) => dir.clone(),
            None => dirs::data_local_dir()
                .context("no per-user data directory; pass --worlds")?
                .join("TPF3-MP")
                .join("worlds")
                .join(link),
        };
        Worlds::open(&dir, self.worlds_gib << 30)
            .with_context(|| format!("opening the worlds in {}", dir.display()))
    }
}

#[derive(Debug, ClapArgs)]
struct Server {
    /// Server address as host:port.
    server: String,

    /// Trust exactly this DER certificate instead of public certificate
    /// authorities (for development servers started with --dev-self-signed).
    #[arg(long)]
    pin_cert: Option<PathBuf>,

    /// Player name shown to others.
    #[arg(long, default_value = "player")]
    name: String,

    /// Identity key file. Created on first use; keep it private, it is what
    /// makes you the same player next time.
    #[arg(long)]
    identity: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();
    match Args::parse().command {
        Command::Connect(server) => {
            let (client, _events) = open(&server).await?;
            let welcome = client.welcome();
            println!(
                "connected as {}: server {}, session {}, round trip {} ms",
                client.player(),
                welcome.server_version,
                welcome.session_id,
                client.rtt().as_millis()
            );
            client.close().await;
        }
        Command::Host {
            server,
            game,
            room_name,
            password,
            max_players,
            start_with,
        } => {
            let options = options(&server).await?;
            let (client, mut events) = connect(options.clone()).await?;
            let password = password.map(Text::new).transpose().context("password")?;
            let (invite, room) = client
                .create_room(CreateRoom {
                    name: Text::new(room_name).context("room name")?,
                    max_players,
                    password: password.clone(),
                    settings: RoomSettings::DEFAULT,
                })
                .await?;
            println!("invite: {invite}");
            print_room(&room);
            get_ready(&client, &game).await?;
            if let Some(players) = start_with {
                start_when_ready(&client, &mut events, players).await?;
            }
            let rejoin = Rejoin {
                options,
                invite,
                password,
                content: Some(game.fingerprint()?),
                give_up_after: REJOIN_PATIENCE,
            };
            play(client, events, &game, rejoin).await?;
        }
        Command::Join {
            server,
            game,
            invite,
            password,
        } => {
            let invite: Invite = invite.parse()?;
            let options = options(&server).await?;
            let (client, events) = connect(options.clone()).await?;
            let password = password.map(Text::new).transpose().context("password")?;
            let content = game.fingerprint()?;
            let room = client
                .join_room(JoinRoom {
                    invite: invite.clone(),
                    password: password.clone(),
                    resume: None,
                    content: Some(content),
                })
                .await?;
            print_room(&room);
            // A running game was joined as it is; a lobby wants readiness.
            if room.phase == RoomPhase::Lobby {
                get_ready(&client, &game).await?;
            }
            let rejoin = Rejoin {
                options,
                invite,
                password,
                content: Some(content),
                give_up_after: REJOIN_PATIENCE,
            };
            play(client, events, &game, rejoin).await?;
        }
    }
    Ok(())
}

/// How long the agent keeps trying to rejoin a room after losing the
/// server, for example while it restarts.
const REJOIN_PATIENCE: Duration = Duration::from_secs(300);

/// Declares this player's content and readiness.
async fn get_ready(client: &Client, game: &Game) -> Result<()> {
    client.declare_content(game.fingerprint()?).await?;
    client.set_ready(true).await?;
    Ok(())
}

/// Waits until `players` members are in the room and ready, then starts.
async fn start_when_ready(client: &Client, events: &mut Events, players: usize) -> Result<()> {
    println!("starting once {players} players are ready");
    loop {
        match events.recv().await {
            Some(ClientEvent::RoomUpdate(room)) => {
                print_room(&room);
                if room.members.len() >= players && room.members.iter().all(|m| m.ready) {
                    client.start_game().await?;
                    println!("game started");
                    return Ok(());
                }
            }
            Some(ClientEvent::Closed(reason)) => anyhow::bail!("disconnected: {reason}"),
            Some(_) => {}
            None => anyhow::bail!("the connection ended"),
        }
    }
}

/// Follows the room, through the game when a link name is given. Through
/// the game, a lost server is rejoined and the room resumed.
async fn play(client: Client, events: Events, game: &Game, rejoin: Rejoin) -> Result<()> {
    let Some(name) = &game.game_link else {
        follow(client, events).await;
        return Ok(());
    };
    let link = tpf3mp_ipc::Link::create(&tpf3mp_ipc::Config::new(name), tpf3mp_ipc::Role::Agent)
        .with_context(|| format!("creating the game link {name}"))?;
    println!("waiting for the game on link {name}");
    let options = BridgeOptions {
        worlds: Some(game.open_worlds(name)?),
        ..BridgeOptions::default()
    };
    let mut bridge = Bridge::new(link, options);
    tokio::select! {
        ended = bridge::play(&mut bridge, client, events, &rejoin) => match ended {
            Ok(end) => println!("the session ended: {end:?}"),
            Err(fault) => eprintln!("the bridge to the game failed: {fault}"),
        },
        _ = tokio::signal::ctrl_c() => bridge.end("the agent was stopped"),
    }
    Ok(())
}

async fn open(server: &Server) -> Result<(Client, Events)> {
    Ok(connect(options(server).await?).await?)
}

async fn options(server: &Server) -> Result<ConnectOptions> {
    let (host, _port) = server
        .server
        .rsplit_once(':')
        .context("the server address must be host:port")?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let address: SocketAddr = tokio::net::lookup_host(&server.server)
        .await
        .with_context(|| format!("resolving {}", server.server))?
        .next()
        .with_context(|| format!("{} has no address", server.server))?;
    let trust = match &server.pin_cert {
        Some(path) => ServerTrust::Pinned(CertificateDer::from(
            std::fs::read(path).with_context(|| format!("reading {}", path.display()))?,
        )),
        None => ServerTrust::WebPki,
    };
    let identity_path = match &server.identity {
        Some(path) => path.clone(),
        None => dirs::data_local_dir()
            .context("no per-user data directory; pass --identity")?
            .join("TPF3-MP")
            .join("identity.key"),
    };
    let identity = Arc::new(Identity::load_or_create(&identity_path)?);
    let name = Text::new(server.name.clone()).context("player name")?;
    Ok(ConnectOptions::new(address, host, trust, identity, name))
}

fn print_room(room: &RoomView) {
    println!(
        "room {} ({:?}), {} of {} players:",
        room.name,
        room.phase,
        room.members.len(),
        room.max_players
    );
    for member in &room.members {
        println!(
            "  {} {} {:?}{}{}",
            member.player,
            member.name,
            member.platform.os,
            if member.ready { " ready" } else { "" },
            if member.connected { "" } else { " (away)" },
        );
    }
}

async fn follow(client: Client, mut events: Events) {
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(ClientEvent::RoomUpdate(room)) => print_room(&room),
                Some(ClientEvent::Closed(reason)) => {
                    println!("disconnected: {reason}");
                    return;
                }
                Some(other) => println!("{other:?}"),
                None => return,
            },
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    client.close().await;
}
