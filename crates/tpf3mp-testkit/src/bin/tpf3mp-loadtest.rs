//! Load test: many rooms of bots playing the toy game against a server,
//! in-process or deployed.

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use clap::Parser;
use tokio::time::Instant;
use tpf3mp_net::{CertificateDer, ServerIdentity, ServerTrust};
use tpf3mp_proto::{RoomSettings, Speed};
use tpf3mp_server::{Server, ServerConfig};
use tpf3mp_testkit::{
    bot::{BotConfig, BotReport},
    netem::{Impairment, Netem},
    scenario::{RoomPlan, latency_summary, play_room},
    toy::ToyRules,
};
use tracing_subscriber::EnvFilter;

/// Runs rooms of bots against a TPF3-MP server and reports latency,
/// throughput and divergence.
#[derive(Debug, Parser)]
#[command(version)]
struct Args {
    /// A deployed server as host:port. Without it, a server runs in-process.
    #[arg(long)]
    server: Option<String>,
    /// Trust exactly this DER certificate (development servers).
    #[arg(long, requires = "server")]
    pin_cert: Option<PathBuf>,
    #[arg(long, default_value_t = 10)]
    rooms: usize,
    /// Bots per room.
    #[arg(long, default_value_t = 8)]
    bots: u8,
    /// Every bot plays until this step.
    #[arg(long, default_value_t = 1000)]
    steps: u64,
    #[arg(long, default_value_t = 50)]
    sps: u16,
    #[arg(long, default_value_t = 250)]
    input_delay_ms: u16,
    #[arg(long, default_value_t = 50)]
    checkpoint_interval: u32,
    /// Steps between one bot's commands.
    #[arg(long, default_value_t = 10)]
    act_every: u64,
    /// Bots play at the room's pace behind a jitter buffer, as games do,
    /// instead of as soon as steps are sealed. Latencies are then the ones
    /// players would feel.
    #[arg(long)]
    paced: bool,
    /// One-way latency added by the network emulator (in-process only).
    #[arg(long, default_value_t = 0, conflicts_with = "server")]
    latency_ms: u64,
    /// Jitter added on top of the latency (in-process only).
    #[arg(long, default_value_t = 0, conflicts_with = "server")]
    jitter_ms: u64,
    /// Packet loss per direction, in percent (in-process only).
    #[arg(long, default_value_t = 0.0, conflicts_with = "server")]
    loss_percent: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();
    let args = Args::parse();

    let (address, server_name, trust, _local) = match &args.server {
        Some(server) => {
            let (host, _) = server
                .rsplit_once(':')
                .context("the server address must be host:port")?;
            let address: SocketAddr = tokio::net::lookup_host(server)
                .await?
                .next()
                .with_context(|| format!("{server} has no address"))?;
            let trust = match &args.pin_cert {
                Some(path) => ServerTrust::Pinned(CertificateDer::from(std::fs::read(path)?)),
                None => ServerTrust::WebPki,
            };
            (address, host.to_owned(), trust, None)
        }
        None => {
            let (address, trust, local) = start_local(&args).await?;
            (address, "localhost".to_owned(), trust, Some(local))
        }
    };

    let settings = RoomSettings {
        steps_per_second: args.sps,
        input_delay_ms: args.input_delay_ms,
        checkpoint_interval: args.checkpoint_interval,
    };
    anyhow::ensure!(settings.is_valid(), "room settings out of range");
    let deadline = Duration::from_secs(args.steps / u64::from(args.sps).max(1) * 4 + 120);

    let started = Instant::now();
    let rooms: Vec<_> = (0..args.rooms)
        .map(|room| {
            let plan = RoomPlan {
                server: address,
                server_name: server_name.clone(),
                trust: trust.clone(),
                settings,
                speed: Speed::NORMAL,
                bots: (0..args.bots)
                    .map(|bot| BotConfig {
                        name: format!("r{room}b{bot}"),
                        seed: (room as u64) << 8 | u64::from(bot),
                        world_seed: room as u64,
                        target_step: args.steps,
                        act_every: args.act_every + u64::from(bot),
                        drift_at: None,
                        paced: args.paced,
                    })
                    .collect(),
                deadline,
            };
            tokio::spawn(play_room(plan))
        })
        .collect();

    let mut reports: Vec<BotReport> = Vec::new();
    let mut failed = 0;
    for room in rooms {
        match room.await? {
            Ok(room_reports) => reports.extend(room_reports),
            Err(error) => {
                failed += 1;
                eprintln!("room failed: {error:#}");
            }
        }
    }
    let elapsed = started.elapsed();

    let events: usize = reports.iter().map(|r| r.events).sum();
    let sent: usize = reports.iter().map(|r| r.sent).sum();
    let rejected: usize = reports.iter().map(|r| r.rejected).sum();
    let diverged = reports.iter().filter(|r| !r.diverged.is_empty()).count();
    println!(
        "{} rooms ({} failed), {} bots, {} steps each, {:.1} s",
        args.rooms,
        failed,
        reports.len(),
        args.steps,
        elapsed.as_secs_f64()
    );
    println!(
        "events applied {events} ({:.0}/s across all replicas), commands sent {sent}, refused {rejected}",
        events as f64 / elapsed.as_secs_f64()
    );
    println!("replicas that diverged: {diverged}");
    if let Some([p50, p95, p99, max]) = latency_summary(&reports) {
        println!("intent-to-apply latency: p50 {p50} ms, p95 {p95} ms, p99 {p99} ms, max {max} ms");
    }
    anyhow::ensure!(failed == 0 && diverged == 0, "load test failed");
    Ok(())
}

/// Starts an in-process server, behind the network emulator when asked.
async fn start_local(args: &Args) -> Result<(SocketAddr, ServerTrust, LocalServer)> {
    let identity = ServerIdentity::self_signed(&["localhost"])?;
    let trust = ServerTrust::Pinned(identity.leaf().clone());
    let mut config = ServerConfig::new("127.0.0.1:0".parse()?, identity);
    config.ruleset = Arc::new(|| Box::new(ToyRules::default()));
    config.max_sessions = 100_000;
    let server = Server::bind(config)?;
    let mut address = server.local_addr()?;
    let task = tokio::spawn(server.run(std::future::pending()));
    let netem = if args.latency_ms > 0 || args.jitter_ms > 0 || args.loss_percent > 0.0 {
        let netem = Netem::start(
            address,
            Impairment {
                latency: Duration::from_millis(args.latency_ms),
                jitter: Duration::from_millis(args.jitter_ms),
                loss_per_million: (args.loss_percent * 10_000.0).round() as u32,
            },
            1,
        )
        .await?;
        address = netem.address();
        Some(netem)
    } else {
        None
    };
    Ok((
        address,
        trust,
        LocalServer {
            task,
            _netem: netem,
        },
    ))
}

struct LocalServer {
    task: tokio::task::JoinHandle<()>,
    _netem: Option<Netem>,
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
