use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use clap::{Args as ClapArgs, Parser, Subcommand};
use tpf3mp_agent::{Client, ClientEvent, ConnectOptions, Events, connect};
use tpf3mp_net::{CertificateDer, Identity, ServerTrust};
use tpf3mp_proto::{CreateRoom, Invite, JoinRoom, RoomSettings, RoomView, Text};
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
        /// Room name shown to players.
        #[arg(long, default_value = "TPF3-MP room")]
        room_name: String,
        /// Require this password in addition to the invite.
        #[arg(long)]
        password: Option<String>,
        #[arg(long, default_value_t = 8)]
        max_players: u8,
    },
    /// Join a room with an invite and follow it until Ctrl-C.
    Join {
        #[command(flatten)]
        server: Server,
        invite: String,
        #[arg(long)]
        password: Option<String>,
    },
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
            room_name,
            password,
            max_players,
        } => {
            let (client, events) = open(&server).await?;
            let (invite, room) = client
                .create_room(CreateRoom {
                    name: Text::new(room_name).context("room name")?,
                    max_players,
                    password: password.map(Text::new).transpose().context("password")?,
                    settings: RoomSettings::DEFAULT,
                })
                .await?;
            println!("invite: {invite}");
            print_room(&room);
            follow(client, events).await;
        }
        Command::Join {
            server,
            invite,
            password,
        } => {
            let invite: Invite = invite.parse()?;
            let (client, events) = open(&server).await?;
            let room = client
                .join_room(JoinRoom {
                    invite,
                    password: password.map(Text::new).transpose().context("password")?,
                    resume_after_turn: None,
                })
                .await?;
            print_room(&room);
            follow(client, events).await;
        }
    }
    Ok(())
}

async fn open(server: &Server) -> Result<(Client, Events)> {
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
    Ok(connect(ConnectOptions::new(address, host, trust, identity, name)).await?)
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
