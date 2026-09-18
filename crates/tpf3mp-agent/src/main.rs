use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tpf3mp_agent::{ConnectOptions, connect};
use tpf3mp_net::{CertificateDer, ServerTrust};
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
    Connect {
        /// Server address as host:port.
        server: String,

        /// Trust exactly this DER certificate instead of public certificate
        /// authorities (for development servers started with --dev-self-signed).
        #[arg(long)]
        pin_cert: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();
    match Args::parse().command {
        Command::Connect { server, pin_cert } => connect_command(&server, pin_cert).await,
    }
}

async fn connect_command(server: &str, pin_cert: Option<PathBuf>) -> Result<()> {
    let (host, _port) = server
        .rsplit_once(':')
        .context("the server address must be host:port")?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let address = tokio::net::lookup_host(server)
        .await
        .with_context(|| format!("resolving {server}"))?
        .next()
        .with_context(|| format!("{server} has no address"))?;
    let trust = match pin_cert {
        Some(path) => ServerTrust::Pinned(CertificateDer::from(
            std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
        )),
        None => ServerTrust::WebPki,
    };

    let session = connect(ConnectOptions::new(address, host, trust)).await?;
    let welcome = session.welcome();
    println!(
        "connected to {address}: server {}, session {}, round trip {} ms",
        welcome.server_version,
        welcome.session_id,
        session.rtt().as_millis()
    );
    session.close().await;
    Ok(())
}
