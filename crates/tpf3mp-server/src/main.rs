use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;
use tpf3mp_net::ServerIdentity;
use tpf3mp_server::{Server, ServerConfig};
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

    /// Sessions served at once.
    #[arg(long, default_value_t = 4096)]
    max_sessions: usize,
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
            if let Some(parent) = cert_out.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(cert_out, identity.leaf())
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
    let server = Server::bind(config)?;
    info!(
        address = %server.local_addr()?,
        version = env!("CARGO_PKG_VERSION"),
        "listening"
    );
    server.run(shutdown_signal()).await;
    info!("stopped");
    Ok(())
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
