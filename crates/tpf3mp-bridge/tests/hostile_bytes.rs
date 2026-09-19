//! Whatever bytes cross the game link, decoding gives an error or a valid
//! message, never a panic: a broken agent cannot crash the game, and a
//! broken hook cannot crash the agent. Most cases corrupt a valid message
//! of each kind, since random bytes rarely get past the first tag.

#![allow(clippy::unwrap_used)]

use std::fmt::Debug;

use proptest::{collection::vec, prelude::*, sample::Index};
use serde::{Serialize, de::DeserializeOwned};
use tpf3mp_bridge::{ToAgent, ToHook, decode, encode};
use tpf3mp_proto::{
    Event, EventBody, FixedBytes, IntentRejection, LaneDigest, Payload, PlayerId, Speed, Text,
};

fn check<T>(bytes: &[u8])
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    if let Ok(message) = decode::<T>(bytes) {
        let encoded = encode(&message).unwrap();
        assert_eq!(decode::<T>(&encoded).unwrap(), message);
    }
}

type Check = fn(&[u8]);

fn lanes() -> Vec<LaneDigest> {
    (0..4)
        .map(|lane| LaneDigest {
            lane,
            digest: FixedBytes([lane as u8; 32]),
        })
        .collect()
}

fn samples() -> Vec<(Check, Vec<u8>)> {
    let to_hook = [
        ToHook::Hello { version: 2 },
        ToHook::Begin {
            steps_per_second: 10,
            checkpoint_interval: 50,
            saves: Text::new("C:/Users/player/TPF3-MP/worlds/saves").unwrap(),
        },
        ToHook::Apply(Event {
            seq: 12,
            step: 401,
            body: EventBody::Command {
                player: PlayerId(FixedBytes([1; 32])),
                client_seq: 3,
                payload: Payload::new(vec![9; 40]).unwrap(),
            },
        }),
        ToHook::Release { through: 410 },
        ToHook::Speed(Speed::NORMAL),
        ToHook::Diverged {
            step: 500,
            lanes: vec![1, 3],
        },
        ToHook::Refused {
            command: 3,
            reason: IntentRejection::Refused { code: 17 },
        },
        ToHook::End {
            reason: Text::new("the room closed").unwrap(),
        },
        ToHook::Load {
            file: Some(Text::new("/home/player/.local/share/TPF3-MP/received/x.sav").unwrap()),
            next_step: 401,
        },
        ToHook::Chat {
            from: Text::new("Ann").unwrap(),
            text: Text::new("gg").unwrap(),
        },
    ];
    let to_agent = [
        ToAgent::Hello {
            version: 2,
            build: Text::new("tpf3 1.0.0 (build 1234)").unwrap(),
        },
        ToAgent::Loaded { next_step: 401 },
        ToAgent::Command {
            payload: Payload::new(vec![7; 64]).unwrap(),
        },
        ToAgent::Ran { step: 402 },
        ToAgent::Checkpoint {
            step: 500,
            lanes: lanes(),
        },
        ToAgent::Log {
            message: Text::new("loaded the world in 2.3 s").unwrap(),
        },
        ToAgent::Saved {
            event: 12,
            lanes: lanes(),
            file: Some(Text::new("saves/12.sav").unwrap()),
        },
        ToAgent::Chat {
            text: Text::new("brb").unwrap(),
        },
    ];
    let mut samples: Vec<(Check, Vec<u8>)> = Vec::new();
    samples.extend(
        to_hook
            .iter()
            .map(|m| (check::<ToHook> as Check, encode(m).unwrap())),
    );
    samples.extend(
        to_agent
            .iter()
            .map(|m| (check::<ToAgent> as Check, encode(m).unwrap())),
    );
    samples
}

#[test]
fn the_samples_are_valid() {
    for (check, bytes) in samples() {
        check(&bytes);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4096))]

    #[test]
    fn corrupted_messages_decode_or_fail_cleanly(
        pick in any::<Index>(),
        edits in vec((any::<Index>(), any::<u8>(), 0u8..4), 1..8),
    ) {
        let samples = samples();
        let (check, sample) = &samples[pick.index(samples.len())];
        let mut bytes = sample.clone();
        for (at, value, kind) in edits {
            match kind {
                0 | 1 if !bytes.is_empty() => {
                    let index = at.index(bytes.len());
                    bytes[index] = value;
                }
                2 => bytes.insert(at.index(bytes.len() + 1), value),
                3 if !bytes.is_empty() => bytes.truncate(at.index(bytes.len())),
                _ => {}
            }
        }
        check(&bytes);
    }

    #[test]
    fn arbitrary_bytes_decode_or_fail_cleanly(bytes in vec(any::<u8>(), 0..300)) {
        check::<ToHook>(&bytes);
        check::<ToAgent>(&bytes);
    }
}
