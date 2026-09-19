//! Rooms of bots playing the toy game against a real server, over loopback
//! or through the network emulator. These are the netcode's acceptance
//! tests until the real game exists.

#![allow(clippy::unwrap_used)]

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use tokio::{sync::oneshot, task::JoinHandle};
use tpf3mp_net::tunnel::TunnelUrl;
use tpf3mp_net::{ServerIdentity, ServerTrust};
use tpf3mp_proto::{RoomSettings, Speed};
use tpf3mp_server::{Server, ServerConfig, ServerStats, SnapshotConfig, TunnelConfig};
use tpf3mp_testkit::{
    bot::{BotConfig, BotReport},
    netem::{Impairment, Netem},
    scenario::{
        BridgedPlan, BridgedPlayer, RoomPlan, latency_summary, play_bridged_room, play_room,
    },
    toy::{ToyRules, lane},
};

struct TestServer {
    address: SocketAddr,
    trust: ServerTrust,
    stats: ServerStats,
    /// The server's tunnel, for players behind networks that block UDP.
    tunnel: TunnelUrl,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl TestServer {
    /// A server whose rooms validate intents with the toy game's canonical
    /// ledger, as TPF3 rooms will with the canonical rules.
    async fn start() -> Self {
        let identity = ServerIdentity::self_signed(&["localhost"]).unwrap();
        Self::start_with("127.0.0.1:0".parse().unwrap(), identity, None).await
    }

    /// A server at `listen` with this identity, logging its rooms in `data`
    /// under the given secret, so a restart restores them.
    async fn start_with(
        listen: SocketAddr,
        identity: ServerIdentity,
        data: Option<(PathBuf, [u8; 32])>,
    ) -> Self {
        Self::start_configured(listen, identity, data, None).await
    }

    /// A server that keeps world snapshots in `snapshots`, so players can
    /// join running games and diverged replicas are rebased. Games save
    /// only when someone needs a world, and at most every second.
    async fn start_saving(
        listen: SocketAddr,
        identity: ServerIdentity,
        data: Option<(PathBuf, [u8; 32])>,
        snapshots: PathBuf,
    ) -> Self {
        let mut config = SnapshotConfig::new(snapshots);
        config.every = Duration::from_secs(3600);
        config.min_gap = Duration::from_secs(1);
        Self::start_configured(listen, identity, data, Some(config)).await
    }

    async fn start_configured(
        listen: SocketAddr,
        identity: ServerIdentity,
        data: Option<(PathBuf, [u8; 32])>,
        snapshots: Option<SnapshotConfig>,
    ) -> Self {
        let trust = ServerTrust::Pinned(identity.leaf().clone());
        let mut config = ServerConfig::new(listen, identity);
        if let Some((dir, secret)) = data {
            config.data_dir = Some(dir);
            config.secret = secret;
        }
        config.snapshots = snapshots;
        config.ruleset = Arc::new(|| Box::new(ToyRules::default()));
        // Compact logs every second or two of play, so restarts restore the
        // canonical ledger from a compacted log's base.
        config.compact_log_at = 1 << 10;
        // Every scenario's UDP players share the endpoint with tunnels.
        config.tunnel = Some(TunnelConfig::new("127.0.0.1:0".parse().unwrap()));
        config.tick = Duration::from_millis(25);
        // Every bot connects from loopback, one address.
        config.max_sessions_per_address = 1000;
        config.max_handshakes_per_address = 1000;
        config.max_rooms_per_address = 1000;
        let server = Server::bind(config).unwrap();
        let address = server.local_addr().unwrap();
        let stats = server.stats();
        let tunnel = format!(
            "wss://localhost:{}/tpf3mp",
            server.tunnel_addr().unwrap().port()
        )
        .parse()
        .unwrap();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(server.run(async {
            let _ = stopped.await;
        }));
        Self {
            address,
            trust,
            stats,
            tunnel,
            stop: Some(stop),
            task,
        }
    }

    fn plan(&self, via: SocketAddr, settings: RoomSettings, bots: Vec<BotConfig>) -> RoomPlan {
        RoomPlan {
            server: via,
            server_name: "localhost".into(),
            trust: self.trust.clone(),
            settings,
            speed: Speed::NORMAL,
            bots,
            deadline: Duration::from_secs(90),
        }
    }

    async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = tokio::time::timeout(Duration::from_secs(10), &mut self.task).await;
    }
}

fn bots(count: u64, target_step: u64, act_every: u64) -> Vec<BotConfig> {
    (0..count)
        .map(|index| BotConfig {
            name: format!("bot{index}"),
            seed: index,
            world_seed: 42,
            target_step,
            act_every: act_every + index,
            drift_at: None,
            paced: false,
        })
        .collect()
}

fn assert_all_agree(reports: &[BotReport]) {
    let reference = &reports[0];
    for report in &reports[1..] {
        assert_eq!(
            report.lanes, reference.lanes,
            "{} and {} disagree at step {}",
            report.name, reference.name, reference.executed
        );
        assert_eq!(report.events, reference.events);
        assert_eq!(report.executed, reference.executed);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eight_bots_agree_over_a_lossy_high_latency_link() {
    let server = TestServer::start().await;
    // 150 ms round trips with jitter and 2% loss in each direction.
    let netem = Netem::start(
        server.address,
        Impairment {
            latency: Duration::from_millis(75),
            jitter: Duration::from_millis(15),
            loss_per_million: 20_000,
        },
        7,
    )
    .await
    .unwrap();
    let settings = RoomSettings {
        steps_per_second: 100,
        input_delay_ms: 250,
        checkpoint_interval: 50,
    };
    let reports = play_room(server.plan(netem.address(), settings, bots(8, 800, 9)))
        .await
        .unwrap();

    assert_all_agree(&reports);
    assert!(reports.iter().all(|report| report.diverged.is_empty()));
    let sent: usize = reports.iter().map(|report| report.sent).sum();
    assert!(sent > 300, "the bots were busy: {sent} commands");
    let [p50, p95, p99, max] = latency_summary(&reports).unwrap();
    eprintln!(
        "intent-to-apply latency over 150 ms RTT, 2% loss: p50 {p50} ms, p95 {p95} ms, p99 {p99} ms, max {max} ms"
    );
    // The input delay (250 ms) plus one-way latency and the tick; loss adds
    // retransmissions to the tail.
    assert!(p50 < 600, "median latency {p50} ms");
    drop(netem);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paced_players_feel_their_round_trip_plus_a_small_buffer() {
    let server = TestServer::start().await;
    // 150 ms round trips with jitter.
    let netem = Netem::start(
        server.address,
        Impairment {
            latency: Duration::from_millis(75),
            jitter: Duration::from_millis(15),
            loss_per_million: 0,
        },
        11,
    )
    .await
    .unwrap();
    let settings = RoomSettings {
        steps_per_second: 20,
        input_delay_ms: 60,
        checkpoint_interval: 20,
    };
    let mut plan_bots = bots(4, 200, 3);
    for bot in &mut plan_bots {
        bot.paced = true;
    }
    let mut plan = server.plan(netem.address(), settings, plan_bots);
    // At 2x the bots also rebase their playout when the speed changes.
    plan.speed = Speed(200);
    let reports = play_room(plan).await.unwrap();

    assert_all_agree(&reports);
    assert!(reports.iter().all(|report| report.diverged.is_empty()));
    let [p50, p95, p99, max] = latency_summary(&reports).unwrap();
    eprintln!(
        "paced intent-to-apply latency over 150 ms RTT: p50 {p50} ms, p95 {p95} ms, p99 {p99} ms, max {max} ms"
    );
    // A player feels the round trip plus their own jitter buffer, not the
    // room's input delay.
    assert!(p50 < 350, "median latency {p50} ms");
    drop(netem);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn games_behind_the_bridge_and_gate_agree() {
    let server = TestServer::start().await;
    let settings = RoomSettings {
        steps_per_second: 100,
        input_delay_ms: 60,
        checkpoint_interval: 20,
    };
    let players = (0..3)
        .map(|index| BridgedPlayer {
            name: format!("game{index}"),
            seed: index,
            world_seed: 42,
            act_every: 7 + index,
            target_step: 300,
            drift_at: None,
            join_after: None,
            tunnel: None,
        })
        .collect();
    let reports = play_bridged_room(BridgedPlan {
        server: server.address,
        server_name: "localhost".into(),
        trust: server.trust.clone(),
        settings,
        players,
        deadline: Duration::from_secs(60),
        worlds: None,
    })
    .await
    .unwrap();

    let reference = &reports[0];
    for report in &reports[1..] {
        assert_eq!(report.lanes, reference.lanes, "the worlds agree");
        assert_eq!(report.applied, reference.applied);
    }
    for report in &reports {
        assert_eq!(report.ran, 300);
        assert!(!report.ended);
        assert!(report.diverged.is_empty(), "{:?}", report.diverged);
    }
    let commands: u64 = reports.iter().map(|report| report.commands).sum();
    assert!(commands > 60, "the players were busy: {commands} commands");
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn games_ride_out_a_server_restart() {
    let dir = std::env::temp_dir().join(format!("tpf3mp-ride-out-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let secret = [7; 32];
    let identity = ServerIdentity::self_signed(&["localhost"]).unwrap();
    let first = TestServer::start_with(
        "127.0.0.1:0".parse().unwrap(),
        identity.clone(),
        Some((dir.clone(), secret)),
    )
    .await;
    let address = first.address;
    let settings = RoomSettings {
        steps_per_second: 50,
        input_delay_ms: 60,
        checkpoint_interval: 25,
    };
    let players = (0..2)
        .map(|index| BridgedPlayer {
            name: format!("game{index}"),
            seed: index,
            world_seed: 7,
            act_every: 9 + index,
            target_step: 400,
            drift_at: None,
            join_after: None,
            tunnel: None,
        })
        .collect();
    let game = tokio::spawn(play_bridged_room(BridgedPlan {
        server: address,
        server_name: "localhost".into(),
        trust: first.trust.clone(),
        settings,
        players,
        deadline: Duration::from_secs(60),
        worlds: None,
    }));

    // Mid-game, the server is upgraded: it stops, and a new process takes
    // over the same address and data directory.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let metrics = first.stats.render_metrics();
    assert!(
        !metrics.contains("tpf3mp_logs_compacted_total 0\n"),
        "the restart restores from a compacted log:\n{metrics}"
    );
    first.stop().await;
    let second = TestServer::start_with(address, identity, Some((dir.clone(), secret))).await;

    let reports = game.await.unwrap().unwrap();
    assert_eq!(reports[0].lanes, reports[1].lanes, "the worlds agree");
    for report in &reports {
        assert_eq!(report.ran, 400, "played on after the restart");
        assert!(!report.ended);
        assert!(report.diverged.is_empty(), "{:?}", report.diverged);
    }
    second.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_drifting_replica_is_singled_out() {
    let server = TestServer::start().await;
    let settings = RoomSettings {
        steps_per_second: 100,
        input_delay_ms: 60,
        checkpoint_interval: 50,
    };
    let mut plan_bots = bots(3, 400, 5);
    plan_bots[2].drift_at = Some(120);
    let reports = play_room(server.plan(server.address, settings, plan_bots))
        .await
        .unwrap();

    let context = format!(
        "diverged: {:?}",
        reports
            .iter()
            .map(|report| (&report.name, &report.diverged))
            .collect::<Vec<_>>()
    );
    assert!(reports[0].diverged.is_empty(), "{context}");
    assert!(reports[1].diverged.is_empty(), "{context}");
    let (step, lanes) = reports[2]
        .diverged
        .first()
        .expect("the drifting replica is told");
    assert_eq!(
        *step, 150,
        "the first checkpoint after the drift; {context}"
    );
    // Which simulation lanes differ at one checkpoint depends on the
    // commands' timing: the generator's state counts draws, so a draw the
    // drift added can be offset by one a slower train has not made yet.
    // Some simulation lane always differs; the canonical ledger never does.
    assert!(
        lanes.contains(&lane::TRAINS) || lanes.contains(&lane::DELIVERIES),
        "{context}"
    );
    assert!(
        reports[2]
            .diverged
            .iter()
            .all(|(_, lanes)| !lanes.contains(&lane::LEDGER)),
        "{context}"
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_server_refuses_what_a_company_cannot_afford() {
    let server = TestServer::start().await;
    let settings = RoomSettings {
        steps_per_second: 50,
        input_delay_ms: 60,
        checkpoint_interval: 25,
    };
    // Commands every few steps outrun the money quickly.
    let reports = play_room(server.plan(server.address, settings, bots(2, 600, 4)))
        .await
        .unwrap();
    assert_all_agree(&reports);
    let rejected: usize = reports.iter().map(|report| report.rejected).sum();
    assert!(rejected > 0, "some purchases were refused");
    for report in &reports {
        let money = report.money.unwrap();
        assert!(money >= 0, "{} went into debt: {money}", report.name);
    }
    server.stop().await;
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tpf3mp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn saving_players(count: u64, target_step: u64) -> Vec<BridgedPlayer> {
    (0..count)
        .map(|index| BridgedPlayer {
            name: format!("game{index}"),
            seed: index,
            world_seed: 42,
            act_every: 7 + index,
            target_step,
            drift_at: None,
            join_after: None,
            tunnel: None,
        })
        .collect()
}

fn assert_worlds_agree(reports: &[tpf3mp_testkit::fake_hook::HookReport]) {
    let reference = &reports[0];
    for (index, report) in reports.iter().enumerate() {
        assert_eq!(
            report.lanes, reference.lanes,
            "game {index} ended in another world"
        );
        assert_eq!(report.ran, reference.ran, "game {index} stopped elsewhere");
        assert!(!report.ended, "game {index}'s session ended early");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_player_joins_a_running_game_from_the_rooms_world() {
    let root = temp_dir("late-join");
    let identity = ServerIdentity::self_signed(&["localhost"]).unwrap();
    let server = TestServer::start_saving(
        "127.0.0.1:0".parse().unwrap(),
        identity,
        None,
        root.join("server"),
    )
    .await;
    let settings = RoomSettings {
        steps_per_second: 100,
        input_delay_ms: 60,
        checkpoint_interval: 20,
    };
    let mut players = saving_players(3, 700);
    // The third player arrives two seconds in, after about 200 steps.
    players[2].join_after = Some(Duration::from_secs(2));
    let reports = play_bridged_room(BridgedPlan {
        server: server.address,
        server_name: "localhost".into(),
        trust: server.trust.clone(),
        settings,
        players,
        deadline: Duration::from_secs(60),
        worlds: Some(root.join("players")),
    })
    .await
    .unwrap();

    assert_worlds_agree(&reports);
    assert_eq!(
        reports[2].received, 1,
        "the newcomer loaded the room's world"
    );
    // The owner's world, saved at the start, serves the newcomer too: it
    // loads that save and plays the turns since.
    assert!(reports[0].saves >= 1, "the owner saved its world");
    assert_eq!(reports[1].received, 1, "the other starter loaded it too");
    for report in &reports {
        assert!(report.diverged.is_empty(), "{:?}", report.diverged);
    }
    server.stop().await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_player_behind_a_udp_block_joins_late_through_the_tunnel() {
    let root = temp_dir("tunnel-join");
    let identity = ServerIdentity::self_signed(&["localhost"]).unwrap();
    let server = TestServer::start_saving(
        "127.0.0.1:0".parse().unwrap(),
        identity,
        None,
        root.join("server"),
    )
    .await;
    let settings = RoomSettings {
        steps_per_second: 100,
        input_delay_ms: 60,
        checkpoint_interval: 20,
    };
    let mut players = saving_players(3, 600);
    // The newcomer's network lets nothing but HTTPS out: it joins, downloads
    // the world and plays, all through the tunnel.
    players[2].join_after = Some(Duration::from_secs(2));
    players[2].tunnel = Some(server.tunnel.clone());
    let reports = play_bridged_room(BridgedPlan {
        server: server.address,
        server_name: "localhost".into(),
        trust: server.trust.clone(),
        settings,
        players,
        deadline: Duration::from_secs(60),
        worlds: Some(root.join("players")),
    })
    .await
    .unwrap();

    assert_worlds_agree(&reports);
    assert_eq!(
        reports[2].received, 1,
        "the newcomer loaded the room's world"
    );
    for report in &reports {
        assert!(report.diverged.is_empty(), "{:?}", report.diverged);
    }
    let metrics = server.stats.render_metrics();
    assert!(
        !metrics.contains("tpf3mp_tunnels_opened_total 0\n"),
        "the newcomer came through the tunnel:\n{metrics}"
    );
    server.stop().await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_player_starts_from_the_owners_world() {
    let root = temp_dir("owners-world");
    let identity = ServerIdentity::self_signed(&["localhost"]).unwrap();
    let server = TestServer::start_saving(
        "127.0.0.1:0".parse().unwrap(),
        identity,
        None,
        root.join("server"),
    )
    .await;
    let settings = RoomSettings {
        steps_per_second: 100,
        input_delay_ms: 60,
        checkpoint_interval: 20,
    };
    // Each player's own starting world differs, as maps generated on
    // different machines might.
    let mut players = saving_players(3, 400);
    for (index, player) in players.iter_mut().enumerate() {
        player.world_seed = 40 + index as u64;
    }
    let reports = play_bridged_room(BridgedPlan {
        server: server.address,
        server_name: "localhost".into(),
        trust: server.trust.clone(),
        settings,
        players,
        deadline: Duration::from_secs(60),
        worlds: Some(root.join("players")),
    })
    .await
    .unwrap();

    assert_worlds_agree(&reports);
    let received: Vec<usize> = reports.iter().map(|report| report.received).collect();
    assert_eq!(
        received,
        [0, 1, 1],
        "everyone but the owner loaded its world"
    );
    for report in &reports {
        assert!(report.diverged.is_empty(), "{:?}", report.diverged);
    }
    server.stop().await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_diverged_replica_is_rebased_onto_the_agreed_world() {
    let root = temp_dir("rebase");
    let identity = ServerIdentity::self_signed(&["localhost"]).unwrap();
    let server = TestServer::start_saving(
        "127.0.0.1:0".parse().unwrap(),
        identity,
        None,
        root.join("server"),
    )
    .await;
    let settings = RoomSettings {
        steps_per_second: 100,
        input_delay_ms: 60,
        checkpoint_interval: 50,
    };
    let mut players = saving_players(3, 800);
    players[2].drift_at = Some(120);
    let reports = play_bridged_room(BridgedPlan {
        server: server.address,
        server_name: "localhost".into(),
        trust: server.trust.clone(),
        settings,
        players,
        deadline: Duration::from_secs(60),
        worlds: Some(root.join("players")),
    })
    .await
    .unwrap();

    // The drifting replica was told, given the agreed world, and ended in
    // the same world as everyone else. Every player but the owner also
    // loaded the owner's world at the start.
    assert!(!reports[2].diverged.is_empty(), "the drift was noticed");
    assert_eq!(reports[2].received, 2, "the start, then one rebase");
    assert_worlds_agree(&reports);
    for (report, received) in reports[..2].iter().zip([0, 1]) {
        assert!(report.diverged.is_empty(), "{:?}", report.diverged);
        assert_eq!(report.received, received);
    }
    server.stop().await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restored_room_still_hands_on_its_world() {
    let root = temp_dir("restored-world");
    let secret = [9; 32];
    let identity = ServerIdentity::self_signed(&["localhost"]).unwrap();
    let data = Some((root.join("rooms"), secret));
    let first = TestServer::start_saving(
        "127.0.0.1:0".parse().unwrap(),
        identity.clone(),
        data.clone(),
        root.join("snapshots"),
    )
    .await;
    let address = first.address;
    let settings = RoomSettings {
        steps_per_second: 50,
        input_delay_ms: 60,
        checkpoint_interval: 25,
    };
    let mut players = saving_players(3, 500);
    // One player joins early, so the room saves before the restart; the
    // last joins after it, from the world the restored room kept.
    players[1].join_after = Some(Duration::from_secs(1));
    players[2].join_after = Some(Duration::from_secs(6));
    let game = tokio::spawn(play_bridged_room(BridgedPlan {
        server: address,
        server_name: "localhost".into(),
        trust: first.trust.clone(),
        settings,
        players,
        deadline: Duration::from_secs(90),
        worlds: Some(root.join("players")),
    }));

    tokio::time::sleep(Duration::from_secs(4)).await;
    first.stop().await;
    let second = TestServer::start_saving(address, identity, data, root.join("snapshots")).await;

    let reports = game.await.unwrap().unwrap();
    assert_worlds_agree(&reports);
    assert_eq!(reports[1].received, 1);
    assert_eq!(reports[2].received, 1);
    second.stop().await;
    let _ = std::fs::remove_dir_all(&root);
}
