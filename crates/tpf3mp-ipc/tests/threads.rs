//! Two threads in one process driving both rings of a link at once.

#![allow(clippy::unwrap_used)]

use std::thread;

use tpf3mp_ipc::{Config, Link, Role};

fn unique_name(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("threads.{tag}.{}.{nanos}", std::process::id())
}

#[test]
fn two_threads_exchange_both_directions() {
    const COUNT: u32 = 2000;
    let name = unique_name("pingpong");
    let agent = Link::create(&Config::new(name.clone()), Role::Agent).unwrap();
    let hook = Link::open(&name, Role::Hook).unwrap();

    // The hook thread echoes each agent->hook message back on hook->agent.
    let hook_thread = thread::spawn(move || {
        let mut buf = vec![0u8; hook.max_message() as usize];
        let mut echoed = 0u32;
        while echoed < COUNT {
            match hook.recv_into(&mut buf).unwrap() {
                Some(len) => {
                    while hook.send(&buf[..len]).is_err() {
                        std::hint::spin_loop();
                    }
                    echoed += 1;
                }
                None => std::hint::spin_loop(),
            }
        }
    });

    let mut buf = vec![0u8; agent.max_message() as usize];
    for value in 0..COUNT {
        let payload = value.to_le_bytes();
        while agent.send(&payload).is_err() {
            std::hint::spin_loop();
        }
        loop {
            match agent.recv_into(&mut buf).unwrap() {
                Some(len) => {
                    assert_eq!(&buf[..len], &payload, "echo mismatch at {value}");
                    break;
                }
                None => std::hint::spin_loop(),
            }
        }
    }

    hook_thread.join().unwrap();
}

#[test]
fn wraparound_holds_over_many_large_messages() {
    // Messages sized so the free-running counters pass the buffer end many
    // times, exercising the split copies.
    let name = unique_name("wrap");
    let mut config = Config::new(name.clone());
    config.ring_capacity = 1024;
    config.max_message = 300;
    let agent = Link::create(&config, Role::Agent).unwrap();
    let hook = Link::open(&name, Role::Hook).unwrap();

    let mut out = vec![0u8; 512];
    for round in 0..1000u32 {
        let payload = vec![(round & 0xff) as u8; 300];
        while agent.send(&payload).is_err() {
            // Drain so the producer can proceed.
            if let Some(len) = hook.recv_into(&mut out).unwrap() {
                assert_eq!(len, 300);
            }
        }
        // Pull it through.
        loop {
            if let Some(len) = hook.recv_into(&mut out).unwrap() {
                assert_eq!(&out[..len], payload.as_slice());
                break;
            }
        }
    }
}
