//! Two players use the launcher as a browser would, over its HTTP API, each
//! with a fake game attached: connect, create and join a room, get ready,
//! start, chat and play to the end in the same world.

#![allow(clippy::unwrap_used)]

use std::{net::SocketAddr, path::Path, sync::Arc, time::Duration};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tpf3mp_agent::{
    TunnelChoice, Worlds,
    launcher::{Launcher, LauncherConfig},
};
use tpf3mp_net::{Identity, ServerIdentity, ServerTrust};
use tpf3mp_proto::{ContentFingerprint, FixedBytes, RoomSettings};
use tpf3mp_server::{Server, ServerConfig, SnapshotConfig};
use tpf3mp_testkit::{
    fake_hook::{self, FakeHookConfig},
    toy::ToyRules,
};

const WAIT: Duration = Duration::from_secs(60);

/// A page's view of one launcher: where it listens and its token.
struct Page {
    address: SocketAddr,
    token: String,
}

impl Page {
    fn of(launcher: &Launcher) -> Self {
        let url = launcher.url().strip_prefix("http://").unwrap();
        let (address, token) = url.split_once("/#").unwrap();
        Self {
            address: address.parse().unwrap(),
            token: token.to_owned(),
        }
    }

    async fn state(&self) -> Value {
        let (status, body) = request(
            self.address,
            &self.address.to_string(),
            "GET",
            "/api/state",
            Some(&self.token),
            None,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        body
    }

    async fn act(&self, action: Value) -> Value {
        let (status, body) = request(
            self.address,
            &self.address.to_string(),
            "POST",
            "/api/action",
            Some(&self.token),
            Some(&action.to_string()),
        )
        .await;
        assert_eq!(status, 200, "{action} was refused: {body}");
        body
    }

    /// Waits until the page's state satisfies `done`.
    async fn wait_for(&self, what: &str, done: impl Fn(&Value) -> bool) -> Value {
        tokio::time::timeout(WAIT, async {
            loop {
                let state = self.state().await;
                if done(&state) {
                    return state;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }
}

/// One HTTP request, as a browser would send it. Returns the status code and
/// the body as JSON, or `Null` for a body that is not JSON.
async fn request(
    address: SocketAddr,
    host: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> (u16, Value) {
    let mut stream = TcpStream::connect(address).await.unwrap();
    let body = body.unwrap_or_default();
    let token = token
        .map(|token| format!("X-Launcher-Token: {token}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\n{token}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8(response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

fn launcher_config(
    root: &Path,
    name: &str,
    trust: &ServerTrust,
    identity: Arc<Identity>,
) -> LauncherConfig {
    LauncherConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        server: None,
        tunnel: TunnelChoice::Off,
        trust: trust.clone(),
        identity,
        name: name.into(),
        content: ContentFingerprint(FixedBytes([0x70; 32])),
        link: format!("tpf3mp-launcher-{}-{name}", std::process::id()),
        worlds: Worlds::open(&root.join(name), 1 << 30).unwrap(),
        room_settings: RoomSettings {
            steps_per_second: 100,
            input_delay_ms: 60,
            checkpoint_interval: 20,
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_players_play_a_room_from_their_launchers() {
    let root = tempfile::tempdir().unwrap();
    let identity = ServerIdentity::self_signed(&["localhost", "127.0.0.1"]).unwrap();
    let trust = ServerTrust::Pinned(identity.leaf().clone());
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), identity);
    config.ruleset = Arc::new(|| Box::new(ToyRules::default()));
    config.tick = Duration::from_millis(25);
    config.max_sessions_per_address = 100;
    config.max_handshakes_per_address = 100;
    config.snapshots = Some(SnapshotConfig::new(root.path().join("server")));
    let server = Server::bind(config).unwrap();
    let server_address = server.local_addr().unwrap().to_string();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server_task = tokio::spawn(server.run(async {
        let _ = stopped.await;
    }));

    let ann_identity = Arc::new(Identity::generate().unwrap().0);
    let bob_identity = Arc::new(Identity::generate().unwrap().0);
    let ann_config = launcher_config(root.path(), "ann", &trust, Arc::clone(&ann_identity));
    let bob_config = launcher_config(root.path(), "bob", &trust, Arc::clone(&bob_identity));
    let hooks = [
        (ann_config.link.clone(), ann_identity.player(), 42),
        // Bob's own world differs; he plays the owner's.
        (bob_config.link.clone(), bob_identity.player(), 7),
    ]
    .map(|(link_name, player, world_seed)| {
        fake_hook::spawn(FakeHookConfig {
            link_name,
            player,
            seed: world_seed,
            world_seed,
            act_every: 9,
            target_step: 300,
            drift_at: None,
            patience: WAIT,
        })
    });
    let ann = Launcher::start(ann_config).await.unwrap();
    let bob = Launcher::start(bob_config).await.unwrap();
    let (ann_page, bob_page) = (Page::of(&ann), Page::of(&bob));

    // Only the page with the token, naming the launcher's own address, gets
    // in.
    let (status, _) = request(
        ann_page.address,
        &ann_page.address.to_string(),
        "GET",
        "/api/state",
        None,
        None,
    )
    .await;
    assert_eq!(status, 401, "no token");
    let (status, _) = request(
        ann_page.address,
        &ann_page.address.to_string(),
        "GET",
        "/api/state",
        Some("0123456789abcdef0123456789abcdef"),
        None,
    )
    .await;
    assert_eq!(status, 401, "a wrong token");
    let (status, _) = request(
        ann_page.address,
        "evil.example:80",
        "GET",
        "/api/state",
        Some(&ann_page.token),
        None,
    )
    .await;
    assert_eq!(status, 403, "a rebound host");
    let (status, _) = request(
        ann_page.address,
        &ann_page.address.to_string(),
        "GET",
        "/",
        None,
        None,
    )
    .await;
    assert_eq!(status, 200, "the page itself holds no secret");

    ann_page
        .act(json!({ "action": "connect", "server": server_address, "name": "Ann" }))
        .await;
    ann_page
        .act(json!({ "action": "create", "room": "table", "max_players": 4, "password": null }))
        .await;
    let state = ann_page
        .wait_for("Ann's room", |state| state["room"]["invite"].is_string())
        .await;
    let invite = state["room"]["invite"].as_str().unwrap().to_owned();

    bob_page
        .act(json!({ "action": "connect", "server": server_address, "name": "Bob" }))
        .await;
    bob_page
        .act(json!({ "action": "join", "invite": invite, "password": null }))
        .await;
    for page in [&ann_page, &bob_page] {
        page.act(json!({ "action": "ready", "ready": true })).await;
    }
    ann_page
        .wait_for("everyone ready", |state| {
            let members = state["room"]["members"].as_array();
            members.is_some_and(|members| {
                members.len() == 2 && members.iter().all(|member| member["ready"] == true)
            })
        })
        .await;
    ann_page.act(json!({ "action": "start" })).await;
    ann_page
        .act(json!({ "action": "chat", "text": "good luck" }))
        .await;
    let state = bob_page
        .wait_for("Ann's message", |state| {
            state["chat"]
                .as_array()
                .is_some_and(|chat| chat.iter().any(|line| line["text"] == "good luck"))
        })
        .await;
    let line = &state["chat"][0];
    assert_eq!(
        (line["from"].as_str(), line["you"].as_bool()),
        (Some("Ann"), Some(false))
    );

    let mut reports = Vec::new();
    for hook in hooks {
        let report = tokio::task::spawn_blocking(move || hook.join())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        reports.push(report);
    }
    assert_eq!(reports[0].lanes, reports[1].lanes, "the worlds agree");
    assert_eq!(reports[0].ran, 300);
    assert_eq!(reports[1].ran, 300);
    assert_eq!(reports[1].received, 1, "Bob played the owner's world");
    let state = bob_page.state().await;
    assert_eq!(state["room"]["phase"], "running");
    assert!(state["game"]["attached"].is_string(), "{state}");

    drop((ann, bob));
    let _ = stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(10), server_task).await;
}
