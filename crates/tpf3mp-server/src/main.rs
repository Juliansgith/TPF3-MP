use std::{
    fs,
    io::{self, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use tpf3mp_net::ServerIdentity;
use tpf3mp_server::{Server, ServerConfig, serve_admin};
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

    /// TCP address of the admin endpoint (`/metrics`, `/healthz`). It has no
    /// authentication: keep it on loopback or a private network.
    #[arg(long)]
    admin_listen: Option<SocketAddr>,

    /// Sessions served at once.
    #[arg(long, default_value_t = 4096)]
    max_sessions: usize,

    /// Rooms hosted at once.
    #[arg(long, default_value_t = 10_000)]
    max_rooms: usize,
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
    config.max_rooms = args.max_rooms;
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
