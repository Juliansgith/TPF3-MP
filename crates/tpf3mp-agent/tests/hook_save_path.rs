//! The path a hook reports in `ToAgent::Saved`. The bridge takes a save in
//! and then deletes the file, so it takes only files inside the saves
//! directory it gave the hook (`ToHook::Begin`): a peer on the link must not
//! be able to name any other file the player can delete. Found by the
//! client-side security review.

#![allow(clippy::unwrap_used)]

use std::{collections::VecDeque, fs, path::PathBuf, sync::Arc, time::Duration};

use tpf3mp_agent::{
    ConnectOptions, Worlds,
    bridge::{Bridge, BridgeFault, BridgeOptions, HookLink},
    connect,
};
use tpf3mp_bridge::{BRIDGE_VERSION, ToAgent, encode};
use tpf3mp_net::{
    Identity, ServerIdentity, ServerTrust, read_message, read_preamble, server_config,
    write_message, write_preamble,
};
use tpf3mp_proto::{
    CONTROL_MAX_FRAME, ClientMessage, GameMessage, PROTOCOL_VERSION, ServerMessage, SessionId,
    Text, Welcome,
};

/// A hook that says what it is given to say, and takes everything.
struct ScriptedHook {
    said: VecDeque<Vec<u8>>,
}

impl HookLink for ScriptedHook {
    fn send(&mut self, _message: &[u8]) -> Result<bool, BridgeFault> {
        Ok(true)
    }

    fn recv(&mut self, buf: &mut Vec<u8>) -> Result<bool, BridgeFault> {
        match self.said.pop_front() {
            Some(message) => {
                *buf = message;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn heartbeat(&mut self) {}

    fn peer_heartbeat(&self) -> u64 {
        0
    }
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tpf3mp-review-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_save_outside_the_saves_directory_is_refused() {
    let dir = scratch("save-path");
    let worlds = Worlds::open(&dir.join("worlds"), 1 << 30).unwrap();
    // A file of the player's that has nothing to do with the game.
    let unrelated = dir.join("elsewhere").join("notes.txt");
    fs::create_dir_all(unrelated.parent().unwrap()).unwrap();
    fs::write(&unrelated, b"the player's own file").unwrap();
    assert!(!unrelated.starts_with(worlds.saves()));

    // A server that completes the handshake and reports what the client
    // says about saves.
    let identity = ServerIdentity::self_signed(&["localhost"]).unwrap();
    let leaf = identity.leaf().clone();
    let endpoint = quinn::Endpoint::server(
        server_config(identity).unwrap(),
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap();
    let address = endpoint.local_addr().unwrap();
    let (reported_tx, reported) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let connection = endpoint.accept().await.unwrap().await.unwrap();
        let (mut send, mut recv) = connection.accept_bi().await.unwrap();
        read_preamble(&mut recv).await.unwrap();
        write_preamble(&mut send, PROTOCOL_VERSION).await.unwrap();
        let _hello: ClientMessage = read_message(&mut recv, CONTROL_MAX_FRAME).await.unwrap();
        let welcome = ServerMessage::Welcome(Welcome {
            server_version: Text::new("test").unwrap(),
            session_id: SessionId([0; 16]),
        });
        write_message(&mut send, &welcome, CONTROL_MAX_FRAME)
            .await
            .unwrap();
        let mut reported_tx = Some(reported_tx);
        while let Ok(message) = read_message::<ClientMessage>(&mut recv, CONTROL_MAX_FRAME).await {
            if let ClientMessage::Game(GameMessage::Saved { world, .. }) = message
                && let Some(reported_tx) = reported_tx.take()
            {
                let _ = reported_tx.send(world);
            }
        }
        drop((endpoint, connection, send));
    });

    let player = Arc::new(Identity::generate().unwrap().0);
    let (client, mut events) = connect(ConnectOptions::new(
        address,
        "localhost",
        ServerTrust::Pinned(leaf),
        player,
        Text::new("player").unwrap(),
    ))
    .await
    .unwrap();
    let hook = ScriptedHook {
        said: VecDeque::from([
            encode(&ToAgent::Hello {
                version: BRIDGE_VERSION,
                build: Text::new("test").unwrap(),
            })
            .unwrap(),
            encode(&ToAgent::Saved {
                event: 1,
                lanes: Vec::new(),
                file: Some(Text::new(unrelated.to_string_lossy().into_owned()).unwrap()),
            })
            .unwrap(),
        ]),
    };
    let options = BridgeOptions {
        worlds: Some(worlds.clone()),
        ..BridgeOptions::default()
    };
    let bridge = tokio::spawn(async move {
        let mut bridge = Bridge::new(hook, options);
        let _ = bridge.run(&client, &mut events).await;
    });
    let reported = tokio::time::timeout(Duration::from_secs(20), reported)
        .await
        .expect("the agent reports the save, kept or not")
        .unwrap();
    bridge.abort();
    server.abort();
    let still_there = unrelated.exists();
    let _ = fs::remove_dir_all(&dir);
    assert!(
        still_there && reported.is_none(),
        "the agent took a file outside its saves directory as the game's save: deleted it \
         ({}), kept it in the world store and reported it to the server as {reported:?}",
        !still_there
    );
}
