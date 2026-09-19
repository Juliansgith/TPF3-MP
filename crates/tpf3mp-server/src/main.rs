use std::{
    fs,
    io::{self, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use tpf3mp_net::ServerIdentity;
use tpf3mp_server::{Server, ServerConfig, SnapshotConfig, TunnelConfig, serve_admin};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// The TPF3-MP dedicated server.
#[derive(Debug, Parser)]
#[command(version)]
struct Args {
    /// UDP address to listen on.
    #[arg(long, default_value = "0.0.0.0:29470")]
    listen: SocketAddr,

    /// PEM certificate chain, leaf first (for example from Let's Encrypt).
    #[arg(long, requires = "key")]
    cert: Option<PathBuf>,

    /// PEM private key belonging to --cert.
    #[arg(long, requires = "cert")]
    key: Option<PathBuf>,

    /// Generate a throwaway self-signed certificate for local development and
    /// write it (DER) to this file, so an agent can pin it with --pin-cert.
    #[arg(long, value_name = "CERT_OUT", conflicts_with_all = ["cert", "key"])]
    dev_self_signed: Option<PathBuf>,

    /// Extra name for the development certificate. Always valid for
    /// localhost, 127.0.0.1 and ::1.
    #[arg(long, requires = "dev_self_signed")]
    dev_name: Vec<String>,

    /// File holding the 32-byte key that signs invites, created on first
    /// start. Without it, a fresh key per process invalidates every invite
    /// when the server restarts.
    #[arg(long)]
    secret_file: Option<PathBuf>,

    /// Directory where running games are logged, so they survive a restart.
    /// Restored rooms are rejoined with their old invites, which needs the
    /// same --secret-file.
    #[arg(long, requires = "secret_file")]
    data_dir: Option<PathBuf>,

    /// TCP address of the admin endpoint (`/metrics`, `/healthz`). It has no
    /// authentication: keep it on loopback or a private network.
    #[arg(long)]
    admin_listen: Option<SocketAddr>,

    /// Sessions served at once.
    #[arg(long, default_value_t = 4096)]
    max_sessions: usize,

    /// Sessions one network address may hold; an IPv6 /64 counts as one.
    /// Raise it, with the next flag, to load-test from a single machine.
    #[arg(long, default_value_t = 8)]
    max_sessions_per_address: usize,

    /// Handshakes one network address may have in progress at once.
    #[arg(long, default_value_t = 4)]
    max_handshakes_per_address: usize,

    /// Rooms hosted at once.
    #[arg(long, default_value_t = 10_000)]
    max_rooms: usize,

    /// Open rooms created from one network address.
    #[arg(long, default_value_t = 8)]
    max_rooms_per_address: usize,

    /// Directory for world snapshots, which let players join running games
    /// and repair diverged ones. Defaults to `snapshots` inside --data-dir;
    /// without either, the server keeps none and nobody can join a game
    /// that has started.
    #[arg(long)]
    snapshot_dir: Option<PathBuf>,

    /// Keep no world snapshots, even with --data-dir.
    #[arg(long, conflicts_with = "snapshot_dir")]
    no_snapshots: bool,

    /// Disk the snapshots may take, in GiB.
    #[arg(long, default_value_t = 64)]
    snapshot_gib: u64,

    /// How often a running game saves, in seconds.
    #[arg(long, default_value_t = 600)]
    save_every_secs: u64,

    /// The least time between two saves of one game, however many players
    /// wait for a world, in seconds.
    #[arg(long, default_value_t = 60)]
    save_gap_secs: u64,

    /// Size in MiB past which a running game's log is compacted to start
    /// from the game's current state.
    #[arg(long, default_value_t = 64)]
    compact_log_mib: u64,

    /// TCP address that also takes players through a WebSocket tunnel, for
    /// networks that block UDP. Serves TLS with --cert unless
    /// --tunnel-behind-proxy. Players look for wss://<host>/tpf3mp on 443.
    #[arg(long)]
    tunnel_listen: Option<SocketAddr>,

    /// Serve tunnels as plain WebSocket to a TLS-terminating proxy in front,
    /// such as Caddy or nginx, and take each player's address from the
    /// X-Forwarded-For it sets. Only the proxy may reach the listener.
    #[arg(long, requires = "tunnel_listen")]
    tunnel_behind_proxy: bool,

    /// The URL path tunnels open.
    #[arg(long, default_value = "/tpf3mp")]
    tunnel_path: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();

    let identity = match (&args.cert, &args.key, &args.dev_self_signed) {
        (Some(cert), Some(key), None) => {
            ServerIdentity::from_pem_files(cert, key).context("loading the TLS certificate")?
        }
        (None, None, Some(cert_out)) => {
            let mut names = vec!["localhost", "127.0.0.1", "::1"];
            names.extend(args.dev_name.iter().map(String::as_str));
            let identity = ServerIdentity::self_signed(&names)?;
            create_parent(cert_out)?;
            fs::write(cert_out, identity.leaf())
                .with_context(|| format!("writing {}", cert_out.display()))?;
            warn!(
                "using a self-signed development certificate; clients must pin {}",
                cert_out.display()
            );
            identity
        }
        _ => bail!("pass --cert and --key, or --dev-self-signed <CERT_OUT>"),
    };

    let mut config = ServerConfig::new(args.listen, identity);
    config.max_sessions = args.max_sessions;
    config.max_sessions_per_address = args.max_sessions_per_address;
    config.max_handshakes_per_address = args.max_handshakes_per_address;
    config.max_rooms = args.max_rooms;
    config.max_rooms_per_address = args.max_rooms_per_address;
    config.data_dir = args.data_dir.clone();
    config.compact_log_at = args.compact_log_mib.max(1).saturating_mul(1 << 20);
    if !args.tunnel_path.starts_with('/') {
        bail!("--tunnel-path must start with /");
    }
    config.tunnel = args.tunnel_listen.map(|listen| {
        let mut tunnel = if args.tunnel_behind_proxy {
            TunnelConfig::behind_proxy(listen)
        } else {
            TunnelConfig::new(listen)
        };
        tunnel.path = args.tunnel_path.clone();
        tunnel
    });
    let snapshot_dir = match (&args.snapshot_dir, &args.data_dir) {
        _ if args.no_snapshots => None,
        (Some(dir), _) => Some(dir.clone()),
        (None, Some(data)) => Some(data.join("snapshots")),
        (None, None) => None,
    };
    config.snapshots = snapshot_dir.map(|dir| {
        let mut snapshots = SnapshotConfig::new(dir);
        snapshots.max_bytes = args.snapshot_gib.saturating_mul(1 << 30);
        snapshots.every = Duration::from_secs(args.save_every_secs.max(1));
        snapshots.min_gap = Duration::from_secs(args.save_gap_secs);
        snapshots
    });
    if config.snapshots.is_none() {
        warn!("no snapshots: players cannot join games that have started");
    }
    match &args.secret_file {
        Some(path) => config.secret = load_or_create_secret(path)?,
        None => warn!("no --secret-file: invites will not survive a restart"),
    }
    let server = Server::bind(config)?;
    info!(
        address = %server.local_addr()?,
        version = env!("CARGO_PKG_VERSION"),
        "listening"
    );
    if let Some(tunnel) = server.tunnel_addr() {
        info!(address = %tunnel, path = %args.tunnel_path, "accepting tunnels");
    }
    if let Some(address) = args.admin_listen {
        if !address.ip().is_loopback() {
            warn!(%address, "the admin endpoint is not on loopback; keep it off the internet");
        }
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .with_context(|| format!("binding the admin endpoint to {address}"))?;
        tokio::spawn(serve_admin(listener, server.stats()));
        info!(%address, "admin endpoint listening");
    }
    server.run(shutdown_signal()).await;
    info!("stopped");
    Ok(())
}

fn create_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    Ok(())
}

/// Reads the invite key, or creates one readable only by its owner.
fn load_or_create_secret(path: &Path) -> Result<[u8; 32]> {
    match fs::read(path) {
        Ok(bytes) => bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("{} must hold exactly 32 bytes", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut secret = [0; 32];
            getrandom::fill(&mut secret).map_err(|e| anyhow::anyhow!("random source: {e}"))?;
            create_parent(path)?;
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(path)
                .with_context(|| format!("creating {}", path.display()))?;
            file.write_all(&secret)?;
            file.sync_all()?;
            info!(path = %path.display(), "created a new invite key");
            Ok(secret)
        }
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Completes on Ctrl-C, and on SIGTERM where it exists (`docker stop` sends it).
async fn shutdown_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            // Without a working handler, wait for the other signal instead of
            // shutting down immediately.
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    () = ctrl_c => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => ctrl_c.await,
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await;
}
