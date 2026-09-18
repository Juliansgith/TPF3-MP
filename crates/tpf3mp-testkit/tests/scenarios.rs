//! Rooms of bots playing the toy game against a real server, over loopback
//! or through the network emulator. These are the netcode's acceptance
//! tests until the real game exists.

#![allow(clippy::unwrap_used)]

use std::{net::SocketAddr, sync::Arc, time::Duration};

use tokio::{sync::oneshot, task::JoinHandle};
use tpf3mp_net::{ServerIdentity, ServerTrust};
use tpf3mp_proto::{RoomSettings, Speed};
use tpf3mp_server::{Server, ServerConfig};
use tpf3mp_testkit::{
    bot::{BotConfig, BotReport},
    netem::{Impairment, Netem},
    scenario::{RoomPlan, latency_summary, play_room},
    toy::{ToyRules, lane},
};

struct TestServer {
    address: SocketAddr,
    trust: ServerTrust,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl TestServer {
    /// A server whose rooms validate intents with the toy game's canonical
    /// ledger, as TPF3 rooms will with the canonical rules.
    async fn start() -> Self {
        let identity = ServerIdentity::self_signed(&["localhost"]).unwrap();
        let trust = ServerTrust::Pinned(identity.leaf().clone());
        let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), identity);
        config.ruleset = Arc::new(|| Box::new(ToyRules::default()));
        config.tick = Duration::from_millis(25);
        let server = Server::bind(config).unwrap();
        let address = server.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(server.run(async {
            let _ = stopped.await;
        }));
        Self {
            address,
            trust,
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
