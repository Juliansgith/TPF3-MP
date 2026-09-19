//! Runs a room of bots against a server: seat everyone, start the game,
//! play to the target step, and collect every bot's report.

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tpf3mp_agent::{
    Client, ConnectOptions, Events, Route, Worlds,
    bridge::{self, Bridge, BridgeEnd, BridgeFault, BridgeOptions, Rejoin},
    connect,
};
use tpf3mp_ipc::{Config as LinkConfig, Link, Role};
use tpf3mp_net::{Identity, ServerTrust, tunnel::TunnelUrl};
use tpf3mp_proto::{
    ContentFingerprint, CreateRoom, FixedBytes, Invite, JoinRoom, RoomSettings, Speed, Text,
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
    let seated: Vec<&Client> = clients.iter().map(|(client, _)| client).collect();
    seat_and_start(&seated, seated.len(), plan.settings).await?;
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
        let options = player_options(server, server_name, trust, name)?;
        clients.push(connect(options).await.context("connecting a player")?);
    }
    Ok(clients)
}

/// How a player with a fresh identity connects.
fn player_options(
    server: SocketAddr,
    server_name: &str,
    trust: &ServerTrust,
    name: &str,
) -> Result<ConnectOptions> {
    Ok(ConnectOptions::new(
        server,
        server_name,
        trust.clone(),
        Arc::new(Identity::generate()?.0),
        Text::new(name).context("player name")?,
    ))
}

/// Seats every client in one room of `max_players` seats, the first as its
/// owner, and starts the game. Returns the room's invite.
async fn seat_and_start(
    clients: &[&Client],
    max_players: usize,
    settings: RoomSettings,
) -> Result<Invite> {
    let Some((owner, others)) = clients.split_first() else {
        bail!("a room needs at least one player");
    };
    let (invite, _) = owner
        .create_room(CreateRoom {
            name: Text::new("testkit").context("room name")?,
            max_players: u8::try_from(max_players).context("too many players")?,
            password: None,
            settings,
        })
        .await?;
    for client in others {
        client
            .join_room(JoinRoom {
                invite: invite.clone(),
                password: None,
                resume: None,
                content: None,
            })
            .await?;
    }
    for client in clients {
        client.declare_content(TOY_CONTENT).await?;
        client.set_ready(true).await?;
    }
    owner.start_game().await?;
    Ok(invite)
}

pub struct BridgedPlan {
    pub server: SocketAddr,
    pub server_name: String,
    pub trust: ServerTrust,
    pub settings: RoomSettings,
    pub players: Vec<BridgedPlayer>,
    /// How long the games may take to reach their target.
    pub deadline: Duration,
    /// Where each player keeps its worlds, in a directory of its own name.
    /// Without it, players can neither save for the room nor join late.
    pub worlds: Option<PathBuf>,
}

pub struct BridgedPlayer {
    pub name: String,
    pub seed: u64,
    pub world_seed: u64,
    pub act_every: u64,
    pub target_step: u64,
    /// This player's world deviates once at this step.
    pub drift_at: Option<u64>,
    /// Join the game this long after it started, instead of from the lobby.
    pub join_after: Option<Duration>,
    /// Reach the server only through this tunnel, as behind a network that
    /// blocks UDP.
    pub tunnel: Option<TunnelUrl>,
}

impl BridgedPlayer {
    /// How this player connects.
    fn options(&self, plan: &BridgedPlan) -> Result<ConnectOptions> {
        let mut options = player_options(plan.server, &plan.server_name, &plan.trust, &self.name)?;
        if let Some(url) = &self.tunnel {
            options.route = Route::Tunnel(url.clone());
        }
        Ok(options)
    }
}

static NEXT_LINK: AtomicU64 = AtomicU64::new(0);

/// Plays one room through the whole stack a game uses: each player is a
/// fake hook (the toy game behind the step gate) on a shared-memory link to
/// its agent's bridge. An agent that loses the server rejoins the room and
/// resumes, as the real one does; a player who joins late receives the
/// world from the room. Returns the hooks' reports in plan order.
pub async fn play_bridged_room(plan: BridgedPlan) -> Result<Vec<HookReport>> {
    let mut starting = Vec::new();
    for player in plan.players.iter().filter(|p| p.join_after.is_none()) {
        let options = player.options(&plan)?;
        let (client, events) = connect(options.clone())
            .await
            .context("connecting a player")?;
        starting.push((options, client, events));
    }
    let seated: Vec<&Client> = starting.iter().map(|(_, client, _)| client).collect();
    let invite = seat_and_start(&seated, plan.players.len(), plan.settings).await?;
    let started = tokio::time::Instant::now();

    let mut games: Vec<Option<(Hook, BridgeTask)>> = plan.players.iter().map(|_| None).collect();
    let starters = plan
        .players
        .iter()
        .enumerate()
        .filter(|(_, player)| player.join_after.is_none());
    for ((index, player), (options, client, events)) in starters.zip(starting) {
        games[index] = Some(play_through_hook(
            &plan, player, &invite, options, client, events,
        )?);
    }
    let mut late: Vec<(usize, &BridgedPlayer, Duration)> = plan
        .players
        .iter()
        .enumerate()
        .filter_map(|(index, player)| player.join_after.map(|after| (index, player, after)))
        .collect();
    late.sort_by_key(|(_, _, after)| *after);
    for (index, player, after) in late {
        tokio::time::sleep_until(started + after).await;
        let options = player.options(&plan)?;
        let (client, events) = connect(options.clone())
            .await
            .context("connecting a late player")?;
        client
            .join_room(JoinRoom {
                invite: invite.clone(),
                password: None,
                resume: None,
                content: Some(TOY_CONTENT),
            })
            .await
            .context("joining the running game")?;
        games[index] = Some(play_through_hook(
            &plan, player, &invite, options, client, events,
        )?);
    }

    let mut reports = Vec::with_capacity(games.len());
    let mut bridges = Vec::with_capacity(games.len());
    for (hook, bridge) in games.into_iter().flatten() {
        let report = tokio::task::spawn_blocking(move || hook.join())
            .await
            .context("waiting for a game")?
            .map_err(|_| anyhow::anyhow!("a game panicked"))?;
        reports.push(report.context("a game failed")?);
        bridges.push(bridge);
    }
    for bridge in bridges {
        if bridge.is_finished() {
            let ended = bridge.await.context("a bridge panicked")?;
            bail!("a bridge ended before its game: {ended:?}");
        }
        bridge.abort();
    }
    Ok(reports)
}

type Hook = std::thread::JoinHandle<Result<HookReport, fake_hook::HookError>>;
type BridgeTask = tokio::task::JoinHandle<Result<BridgeEnd, BridgeFault>>;

/// Starts a player's fake game and the bridge between it and its client.
fn play_through_hook(
    plan: &BridgedPlan,
    player: &BridgedPlayer,
    invite: &Invite,
    options: ConnectOptions,
    client: Client,
    events: Events,
) -> Result<(Hook, BridgeTask)> {
    let rejoin = Rejoin {
        options,
        invite: invite.clone(),
        password: None,
        content: Some(TOY_CONTENT),
        give_up_after: plan.deadline,
    };
    let worlds = match &plan.worlds {
        Some(dir) => Some(
            Worlds::open(&dir.join(&player.name), 1 << 30).context("opening a player's worlds")?,
        ),
        None => None,
    };
    let link_name = format!(
        "tpf3mp-bridged-{}-{}",
        std::process::id(),
        NEXT_LINK.fetch_add(1, Ordering::Relaxed)
    );
    let link = Link::create(&LinkConfig::new(&link_name), Role::Agent)?;
    let hook = fake_hook::spawn(FakeHookConfig {
        link_name,
        player: client.player(),
        seed: player.seed,
        world_seed: player.world_seed,
        act_every: player.act_every,
        target_step: player.target_step,
        drift_at: player.drift_at,
        patience: plan.deadline,
    });
    let bridge = tokio::spawn(async move {
        let options = BridgeOptions {
            worlds,
            ..BridgeOptions::default()
        };
        let mut bridge = Bridge::new(link, options);
        bridge::play(&mut bridge, client, events, &rejoin).await
    });
    Ok((hook, bridge))
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
