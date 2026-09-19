//! Whatever bytes a peer sends, decoding gives an error or a message, never
//! a panic, and a decoded message is valid: it encodes back to bytes that
//! decode to the same message, so every bound and check held.
//!
//! Random bytes rarely get past the first tag, so most cases start from a
//! valid message of each kind and corrupt it: bytes overwritten, inserted
//! and cut off.

#![allow(clippy::unwrap_used)]

use std::fmt::Debug;

use proptest::{collection::vec, prelude::*, sample::Index};
use serde::{Serialize, de::DeserializeOwned};
use tpf3mp_proto::{
    Arch, BulkOpen, BulkRequest, BulkResponse, ChunkHash, ClientMessage, ContentFingerprint,
    CreateRoom, Event, EventBody, FixedBytes, GameMessage, Hello, IntentRejection, Invite,
    JoinRoom, LaneDigest, MemberView, Os, Payload, Platform, PlayerId, Reject, RejectReason,
    Request, RequestError, Response, Resume, RoomId, RoomPhase, RoomSettings, RoomView, SavedWorld,
    ServerMessage, SessionId, Signature, SnapshotId, Speed, Text, Turn, TurnMessage, TurnStart,
    Welcome, WorldOffer, decode_frame,
};

/// Decodes `bytes` as a `T`: an error, or a message that survives a round
/// trip.
fn check<T>(bytes: &[u8])
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    if let Ok(message) = decode_frame::<T>(bytes) {
        let encoded = postcard::to_stdvec(&message).unwrap();
        assert_eq!(decode_frame::<T>(&encoded).unwrap(), message);
    }
}

type Check = fn(&[u8]);

fn encoded<T: Serialize>(message: &T) -> Vec<u8> {
    postcard::to_stdvec(message).unwrap()
}

fn player(n: u8) -> PlayerId {
    PlayerId(FixedBytes([n; 32]))
}

fn platform() -> Platform {
    Platform {
        os: Os::Linux,
        arch: Arch::Aarch64,
    }
}

fn room_view() -> RoomView {
    RoomView {
        id: RoomId(FixedBytes([3; 16])),
        name: Text::new("A room with a name").unwrap(),
        owner: player(1),
        max_players: 8,
        has_password: true,
        phase: RoomPhase::Running,
        settings: RoomSettings::DEFAULT,
        members: vec![
            MemberView {
                player: player(1),
                name: Text::new("Ann").unwrap(),
                platform: platform(),
                ready: true,
                content: Some(ContentFingerprint(FixedBytes([9; 32]))),
                connected: true,
            },
            MemberView {
                player: player(2),
                name: Text::new("Bob").unwrap(),
                platform: platform(),
                ready: false,
                content: None,
                connected: false,
            },
        ],
    }
}

fn invite() -> Invite {
    Invite {
        room: RoomId(FixedBytes([3; 16])),
        token: FixedBytes([4; 32]),
    }
}

fn lanes() -> Vec<LaneDigest> {
    (0..3)
        .map(|lane| LaneDigest {
            lane,
            digest: FixedBytes([lane as u8; 32]),
        })
        .collect()
}

/// A valid message of every kind a peer sends, with the check for its type.
fn samples() -> Vec<(Check, Vec<u8>)> {
    let snapshot = SnapshotId(FixedBytes([7; 32]));
    let client = [
        ClientMessage::Hello(Hello {
            client_version: Text::new("0.1.0").unwrap(),
            platform: platform(),
            name: Text::new("Ann").unwrap(),
            identity: player(1),
            proof: Signature(FixedBytes([5; 64])),
        }),
        ClientMessage::Request {
            id: 7,
            request: Request::CreateRoom(CreateRoom {
                name: Text::new("Table").unwrap(),
                max_players: 4,
                password: Some(Text::new("secret").unwrap()),
                settings: RoomSettings::DEFAULT,
            }),
        },
        ClientMessage::Request {
            id: 8,
            request: Request::JoinRoom(JoinRoom {
                invite: invite(),
                password: None,
                resume: Some(Resume {
                    after_turn: 40,
                    history: 99,
                }),
                content: Some(ContentFingerprint(FixedBytes([9; 32]))),
            }),
        },
        ClientMessage::Request {
            id: 9,
            request: Request::Chat(Text::new("Hello, everyone 👋").unwrap()),
        },
        ClientMessage::Request {
            id: 10,
            request: Request::Kick(player(2)),
        },
        ClientMessage::Game(GameMessage::Intent {
            client_seq: 3,
            payload: Payload::new(vec![1, 2, 3, 4, 5, 6, 7, 8]).unwrap(),
        }),
        ClientMessage::Game(GameMessage::Checkpoint {
            step: 500,
            lanes: lanes(),
        }),
        ClientMessage::Game(GameMessage::Saved {
            event: 12,
            lanes: lanes(),
            world: Some(SavedWorld {
                snapshot,
                size: 120 << 20,
            }),
        }),
    ];
    let server = [
        ServerMessage::Welcome(Welcome {
            server_version: Text::new("0.1.0").unwrap(),
            session_id: SessionId([6; 16]),
        }),
        ServerMessage::Reject(Reject {
            reason: RejectReason::TooManyConnections,
        }),
        ServerMessage::Response {
            id: 7,
            result: Ok(Response::RoomCreated {
                invite: invite(),
                room: room_view(),
            }),
        },
        ServerMessage::Response {
            id: 8,
            result: Err(RequestError::ResumeUnavailable),
        },
        ServerMessage::RoomUpdate(room_view()),
        ServerMessage::IntentRejected {
            client_seq: 3,
            reason: IntentRejection::Refused { code: 17 },
        },
        ServerMessage::Diverged {
            step: 500,
            lanes: vec![0, 2],
        },
        ServerMessage::Upload {
            event: 12,
            snapshot,
        },
        ServerMessage::Chat {
            from: player(1),
            text: Text::new("gg").unwrap(),
        },
    ];
    let turns = [
        TurnMessage::Start(TurnStart {
            room: RoomId(FixedBytes([3; 16])),
            next_turn: 41,
            next_event: 12,
            sealed_through: 400,
            steps_per_second: 10,
            checkpoint_interval: 50,
            history: 99,
            world: Some(WorldOffer {
                snapshot,
                size: 120 << 20,
            }),
        }),
        TurnMessage::Turn(Turn {
            number: 41,
            sealed_through: 410,
            speed: Speed::NORMAL,
            events: vec![
                Event {
                    seq: 12,
                    step: 401,
                    body: EventBody::Command {
                        player: player(1),
                        client_seq: 3,
                        payload: Payload::new(vec![9; 40]).unwrap(),
                    },
                },
                Event {
                    seq: 13,
                    step: 401,
                    body: EventBody::PlayerJoined {
                        player: player(3),
                        name: Text::new("Cid").unwrap(),
                        platform: platform(),
                    },
                },
                Event {
                    seq: 14,
                    step: 402,
                    body: EventBody::PlayerLeft {
                        player: player(2),
                        kicked: true,
                    },
                },
                Event {
                    seq: 15,
                    step: 402,
                    body: EventBody::Save,
                },
            ],
        }),
    ];
    let bulk_opens = [BulkOpen::Fetch { snapshot }, BulkOpen::Serve { snapshot }];
    let bulk_requests = [
        BulkRequest::Manifest,
        BulkRequest::Chunks {
            ids: vec![ChunkHash(FixedBytes([8; 32])); 3],
        },
    ];
    let bulk_responses = [
        BulkResponse::Manifest {
            bytes: vec![1; 100],
        },
        BulkResponse::Chunk {
            id: ChunkHash(FixedBytes([8; 32])),
            frame: vec![2; 100],
        },
        BulkResponse::Unavailable,
    ];

    let mut samples: Vec<(Check, Vec<u8>)> = Vec::new();
    samples.extend(
        client
            .iter()
            .map(|m| (check::<ClientMessage> as Check, encoded(m))),
    );
    samples.extend(
        server
            .iter()
            .map(|m| (check::<ServerMessage> as Check, encoded(m))),
    );
    samples.extend(
        turns
            .iter()
            .map(|m| (check::<TurnMessage> as Check, encoded(m))),
    );
    samples.extend(
        bulk_opens
            .iter()
            .map(|m| (check::<BulkOpen> as Check, encoded(m))),
    );
    samples.extend(
        bulk_requests
            .iter()
            .map(|m| (check::<BulkRequest> as Check, encoded(m))),
    );
    samples.extend(
        bulk_responses
            .iter()
            .map(|m| (check::<BulkResponse> as Check, encoded(m))),
    );
    samples
}

/// Every type a peer's bytes are decoded as.
const DECODERS: [Check; 6] = [
    check::<ClientMessage>,
    check::<ServerMessage>,
    check::<TurnMessage>,
    check::<BulkOpen>,
    check::<BulkRequest>,
    check::<BulkResponse>,
];

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
                // Overwrite a byte, most often.
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
        for check in DECODERS {
            check(&bytes);
        }
    }
}
