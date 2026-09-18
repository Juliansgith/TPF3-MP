//! Test helper: attaches to a link as the hook and echoes every message back to
//! the agent, until it receives `quit`. The two-process integration test spawns
//! it. Not shipped.

use std::{env, thread, time::Duration};

use tpf3mp_ipc::{Link, Role};

fn main() {
    let name = env::args().nth(1).expect("usage: ipc-echo <link-name>");

    // The agent creates the link before spawning us, but retry briefly so the
    // helper is robust to scheduling.
    let mut attempts = 0;
    let link = loop {
        match Link::open(&name, Role::Hook) {
            Ok(link) => break link,
            Err(error) => {
                attempts += 1;
                assert!(attempts < 500, "could not open link {name:?}: {error}");
                thread::sleep(Duration::from_millis(2));
            }
        }
    };

    let mut buf = vec![0u8; link.max_message() as usize];
    loop {
        link.heartbeat();
        match link.recv_into(&mut buf) {
            Ok(Some(len)) => {
                if &buf[..len] == b"quit" {
                    break;
                }
                // Echo it back, waiting for room if the return ring is full.
                while link.send(&buf[..len]).is_err() {
                    link.heartbeat();
                    thread::sleep(Duration::from_millis(1));
                }
            }
            Ok(None) => thread::sleep(Duration::from_millis(1)),
            Err(error) => {
                eprintln!("ipc-echo: recv error: {error}");
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
}
