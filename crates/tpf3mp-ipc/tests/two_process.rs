//! Two real processes over the link: this test is the agent; the spawned
//! `ipc-echo` helper is the hook.

#![allow(clippy::unwrap_used)]

use std::{
    process::Command,
    thread,
    time::{Duration, Instant},
};

use tpf3mp_ipc::{Config, Link, Role};

fn unique_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("twoproc.{}.{nanos}", std::process::id())
}

#[test]
fn agent_and_hook_processes_exchange_messages() {
    let name = unique_name();
    let agent = Link::create(&Config::new(name.clone()), Role::Agent).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_ipc-echo"))
        .arg(&name)
        .spawn()
        .expect("spawn ipc-echo helper");

    let deadline = Instant::now() + Duration::from_secs(15);

    // Wait for the hook process to attach.
    while agent.peer_pid() == 0 {
        assert!(Instant::now() < deadline, "the hook process did not attach");
        thread::sleep(Duration::from_millis(5));
    }
    assert_ne!(
        agent.peer_pid(),
        std::process::id(),
        "peer is a separate process"
    );
    let heartbeat_before = agent.peer_heartbeat();

    let mut buf = vec![0u8; agent.max_message() as usize];
    for index in 0..200u32 {
        let message = format!("message-{index}");
        while agent.send(message.as_bytes()).is_err() {
            assert!(Instant::now() < deadline, "send stalled");
            thread::sleep(Duration::from_millis(1));
        }
        let echoed = loop {
            match agent.recv_into(&mut buf).unwrap() {
                Some(len) => break len,
                None => {
                    assert!(Instant::now() < deadline, "no echo received");
                    thread::sleep(Duration::from_millis(1));
                }
            }
        };
        assert_eq!(
            &buf[..echoed],
            message.as_bytes(),
            "echo mismatch at {index}"
        );
    }

    assert!(
        agent.peer_heartbeat() > heartbeat_before,
        "the hook's heartbeat should advance"
    );

    while agent.send(b"quit").is_err() {
        thread::sleep(Duration::from_millis(1));
    }
    let status = child.wait().expect("wait for ipc-echo");
    assert!(status.success(), "ipc-echo exited with {status}");
}
