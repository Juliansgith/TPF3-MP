//! Runs a room of bots against a server: seat everyone, start the game,
//! play to the target step, and collect every bot's report.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use tpf3mp_agent::{ConnectOptions, connect};
use tpf3mp_net::{Identity, ServerTrust};
use tpf3mp_proto::{
    ContentFingerprint, CreateRoom, FixedBytes, JoinRoom, RoomSettings, Speed, Text,
};

use crate::bot::{Bot, BotConfig, BotReport};

/// Every bot plays the same toy content.
const TOY_CONTENT: ContentFingerprint = ContentFingerprint(FixedBytes([0x70; 32]));

pub struct RoomPlan {
    pub server: SocketAddr,
    pub server_name: String,
    pub trust: ServerTrust,
    pub settings: RoomSettings,
    pub speed: Speed,
    pub bots: Vec<BotConfig>,
    /// How long the bots may take to reach their target.
    pub deadline: Duration,
}

/// Plays one room and returns the bots' reports in seat order. The first
/// bot creates and owns the room.
pub async fn play_room(plan: RoomPlan) -> Result<Vec<BotReport>> {
    if plan.bots.is_empty() {
        bail!("a room needs at least one bot");
    }
    let mut clients = Vec::with_capacity(plan.bots.len());
    for config in &plan.bots {
        let identity = Arc::new(Identity::generate()?.0);
        let options = ConnectOptions::new(
            plan.server,
            plan.server_name.clone(),
            plan.trust.clone(),
            identity,
            Text::new(config.name.clone()).context("bot name")?,
        );
        clients.push(connect(options).await.context("connecting a bot")?);
    }

    let owner = &clients[0].0;
    let (invite, _) = owner
        .create_room(CreateRoom {
            name: Text::new("testkit").context("room name")?,
            max_players: u8::try_from(plan.bots.len()).context("too many bots")?,
            password: None,
            settings: plan.settings,
        })
        .await?;
    for (client, _) in &clients[1..] {
        client
            .join_room(JoinRoom {
                invite: invite.clone(),
                password: None,
                resume_after_turn: None,
            })
            .await?;
    }
    for (client, _) in &clients {
        client.declare_content(TOY_CONTENT).await?;
        client.set_ready(true).await?;
    }
    clients[0].0.start_game().await?;
    if plan.speed != Speed::NORMAL {
        clients[0].0.set_speed(plan.speed).await?;
    }

    let tasks: Vec<_> = clients
        .into_iter()
        .zip(plan.bots)
        .map(|((client, events), config)| {
            tokio::spawn(Bot::new(client, events, config).play(plan.deadline))
        })
        .collect();
    let mut finished = Vec::with_capacity(tasks.len());
    for task in tasks {
        finished.push(task.await.context("a bot task panicked")?);
    }
    let mut reports = Vec::with_capacity(finished.len());
    let mut connected = Vec::with_capacity(finished.len());
    for result in finished {
        let (client, report) = result?;
        connected.push(client);
        reports.push(report);
    }
    // Everyone stays connected until every bot is done, so nobody leaves the
    // pacing set early.
    for client in connected {
        client.close().await;
    }
    Ok(reports)
}

/// Latency percentiles over every report, in milliseconds: p50, p95, p99, max.
pub fn latency_summary(reports: &[BotReport]) -> Option<[u128; 4]> {
    let mut all: Vec<Duration> = reports
        .iter()
        .flat_map(|report| report.latencies.iter().copied())
        .collect();
    if all.is_empty() {
        return None;
    }
    all.sort_unstable();
    let at = |per_mille: usize| all[(all.len() - 1) * per_mille / 1000].as_millis();
    Some([at(500), at(950), at(990), at(1000)])
}
