//! Runs a room of bots against a server: seat everyone, start the game,
//! play to the target step, and collect every bot's report.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tpf3mp_agent::{
    Client, ConnectOptions, Events,
    bridge::{Bridge, BridgeOptions},
    connect,
};
use tpf3mp_ipc::{Config as LinkConfig, Link, Role};
use tpf3mp_net::{Identity, ServerTrust};
use tpf3mp_proto::{
    ContentFingerprint, CreateRoom, FixedBytes, JoinRoom, RoomSettings, Speed, Text,
};

use crate::{
    bot::{Bot, BotConfig, BotReport},
    fake_hook::{self, FakeHookConfig, HookReport},
};

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
    let names: Vec<&str> = plan.bots.iter().map(|bot| bot.name.as_str()).collect();
    let clients = connect_all(plan.server, &plan.server_name, &plan.trust, &names).await?;
    seat_and_start(&clients, plan.settings).await?;
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

/// Connects one client per name, each with a fresh identity.
async fn connect_all(
    server: SocketAddr,
    server_name: &str,
    trust: &ServerTrust,
    names: &[&str],
) -> Result<Vec<(Client, Events)>> {
    let mut clients = Vec::with_capacity(names.len());
    for name in names {
        let identity = Arc::new(Identity::generate()?.0);
        let options = ConnectOptions::new(
            server,
            server_name,
            trust.clone(),
            identity,
            Text::new(*name).context("player name")?,
        );
        clients.push(connect(options).await.context("connecting a player")?);
    }
    Ok(clients)
}

/// Seats every client in one room, the first as its owner, and starts the
/// game.
async fn seat_and_start(clients: &[(Client, Events)], settings: RoomSettings) -> Result<()> {
    let Some(((owner, _), others)) = clients.split_first() else {
        bail!("a room needs at least one player");
    };
    let (invite, _) = owner
        .create_room(CreateRoom {
            name: Text::new("testkit").context("room name")?,
            max_players: u8::try_from(clients.len()).context("too many players")?,
            password: None,
            settings,
        })
        .await?;
    for (client, _) in others {
        client
            .join_room(JoinRoom {
                invite: invite.clone(),
                password: None,
                resume: None,
            })
            .await?;
    }
    for (client, _) in clients {
        client.declare_content(TOY_CONTENT).await?;
        client.set_ready(true).await?;
    }
    owner.start_game().await?;
    Ok(())
}

pub struct BridgedPlan {
    pub server: SocketAddr,
    pub server_name: String,
    pub trust: ServerTrust,
    pub settings: RoomSettings,
    pub players: Vec<BridgedPlayer>,
    /// How long the games may take to reach their target.
    pub deadline: Duration,
}

pub struct BridgedPlayer {
    pub name: String,
    pub seed: u64,
    pub world_seed: u64,
    pub act_every: u64,
    pub target_step: u64,
}

static NEXT_LINK: AtomicU64 = AtomicU64::new(0);

/// Plays one room through the whole stack a game uses: each player is a
/// fake hook (the toy game behind the step gate) on a shared-memory link to
/// its agent's bridge. Returns the hooks' reports in seat order.
pub async fn play_bridged_room(plan: BridgedPlan) -> Result<Vec<HookReport>> {
    let names: Vec<&str> = plan.players.iter().map(|p| p.name.as_str()).collect();
    let clients = connect_all(plan.server, &plan.server_name, &plan.trust, &names).await?;
    seat_and_start(&clients, plan.settings).await?;

    let mut hooks = Vec::with_capacity(clients.len());
    let mut bridges = Vec::with_capacity(clients.len());
    for ((client, mut events), player) in clients.into_iter().zip(&plan.players) {
        let link_name = format!(
            "tpf3mp-bridged-{}-{}",
            std::process::id(),
            NEXT_LINK.fetch_add(1, Ordering::Relaxed)
        );
        let link = Link::create(&LinkConfig::new(&link_name), Role::Agent)?;
        hooks.push(fake_hook::spawn(FakeHookConfig {
            link_name,
            player: client.player(),
            seed: player.seed,
            world_seed: player.world_seed,
            act_every: player.act_every,
            target_step: player.target_step,
            patience: plan.deadline,
        }));
        bridges.push(tokio::spawn(async move {
            let ended = Bridge::new(link, BridgeOptions::default())
                .run(&client, &mut events)
                .await;
            (client, ended)
        }));
    }

    let mut reports = Vec::with_capacity(hooks.len());
    for hook in hooks {
        let report = tokio::task::spawn_blocking(move || hook.join())
            .await
            .context("waiting for a game")?
            .map_err(|_| anyhow::anyhow!("a game panicked"))?;
        reports.push(report.context("a game failed")?);
    }
    for bridge in bridges {
        if bridge.is_finished() {
            let (_, ended) = bridge.await.context("a bridge panicked")?;
            bail!("a bridge ended before its game: {ended:?}");
        }
        bridge.abort();
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
