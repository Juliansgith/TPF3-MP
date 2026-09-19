//! Rooms and the lobby: invites, passwords, limits, ownership and cleanup.

#![allow(clippy::unwrap_used)]

mod common;

use common::{FAST, RunningServer, content, join, room};
use tpf3mp_agent::{ClientError, ClientEvent};
use tpf3mp_proto::{
    CreateRoom, FixedBytes, Invite, JoinRoom, RequestError, RoomId, RoomPhase, RoomSettings, Text,
};

#[tokio::test]
async fn a_room_is_joined_with_its_invite() {
    let server = RunningServer::start(|_| {}).await;
    let mut ann = server.client("ann").await;
    let mut bob = server.client("bob").await;
    let (invite, created) = ann.client.create_room(room("table", FAST)).await.unwrap();
    assert_eq!(created.owner, ann.client.player());
    assert_eq!(created.phase, RoomPhase::Lobby);
    // The invite survives a round trip through its text form, as players
    // paste it into chat.
    let invite: Invite = invite.to_string().parse().unwrap();
    let joined = bob.client.join_room(join(&invite)).await.unwrap();
    assert_eq!(joined.members.len(), 2);
    // Both see the full table.
    ann.room_where(|room| room.members.len() == 2).await;
    bob.room_where(|room| room.members.len() == 2).await;
    server.shut_down().await;
}

#[tokio::test]
async fn every_bad_invite_fails_the_same_way() {
    let server = RunningServer::start(|_| {}).await;
    let ann = server.client("ann").await;
    let bob = server.client("bob").await;
    let mut create = room("secret", FAST);
    create.password = Some(Text::new("hunter2").unwrap());
    let (invite, created) = ann.client.create_room(create).await.unwrap();
    assert!(created.has_password);

    let wrong_token = Invite {
        room: invite.room,
        token: FixedBytes([0; 32]),
    };
    let unknown_room = Invite {
        room: RoomId(FixedBytes([9; 16])),
        token: invite.token,
    };
    let attempts = [
        (wrong_token, Some("hunter2")),
        (unknown_room, Some("hunter2")),
        (invite.clone(), Some("hunter3")),
        (invite.clone(), None),
    ];
    for (invite, password) in attempts {
        let error = bob
            .client
            .join_room(JoinRoom {
                invite,
                password: password.map(|p| Text::new(p).unwrap()),
                resume: None,
                content: None,
            })
            .await
            .unwrap_err();
        assert_eq!(error, ClientError::Refused(RequestError::BadInvite));
    }
    // The right invite and password still work.
    bob.client
        .join_room(JoinRoom {
            invite,
            password: Some(Text::new("hunter2").unwrap()),
            resume: None,
            content: None,
        })
        .await
        .unwrap();
    server.shut_down().await;
}

#[tokio::test]
async fn a_full_room_refuses_more_players() {
    let server = RunningServer::start(|_| {}).await;
    let ann = server.client("ann").await;
    let bob = server.client("bob").await;
    let cat = server.client("cat").await;
    let mut create = room("pair", FAST);
    create.max_players = 2;
    let (invite, _) = ann.client.create_room(create).await.unwrap();
    bob.client.join_room(join(&invite)).await.unwrap();
    assert_eq!(
        cat.client.join_room(join(&invite)).await.unwrap_err(),
        ClientError::Refused(RequestError::RoomFull)
    );
    server.shut_down().await;
}

#[tokio::test]
async fn ownership_passes_on_and_an_empty_room_closes() {
    let server = RunningServer::start(|_| {}).await;
    let ann = server.client("ann").await;
    let mut bob = server.client("bob").await;
    let (invite, _) = ann.client.create_room(room("table", FAST)).await.unwrap();
    bob.client.join_room(join(&invite)).await.unwrap();
    assert_eq!(server.stats.rooms(), 1);

    ann.client.leave_room().await.unwrap();
    let room = bob.room_where(|room| room.members.len() == 1).await;
    assert_eq!(room.owner, bob.client.player());

    bob.client.leave_room().await.unwrap();
    server.wait_for_rooms(0).await;
    // The invite of a closed room is just a bad invite.
    assert_eq!(
        ann.client.join_room(join(&invite)).await.unwrap_err(),
        ClientError::Refused(RequestError::BadInvite)
    );
    server.shut_down().await;
}

#[tokio::test]
async fn a_lobby_seat_is_freed_when_its_player_disconnects() {
    let server = RunningServer::start(|_| {}).await;
    let mut ann = server.client("ann").await;
    let bob = server.client("bob").await;
    let (invite, _) = ann.client.create_room(room("table", FAST)).await.unwrap();
    bob.client.join_room(join(&invite)).await.unwrap();
    ann.room_where(|room| room.members.len() == 2).await;
    bob.client.close().await;
    ann.room_where(|room| room.members.len() == 1).await;
    server.shut_down().await;
}

#[tokio::test]
async fn a_connection_is_in_one_room_at_a_time() {
    let server = RunningServer::start(|_| {}).await;
    let ann = server.client("ann").await;
    ann.client.create_room(room("first", FAST)).await.unwrap();
    assert_eq!(
        ann.client
            .create_room(room("second", FAST))
            .await
            .unwrap_err(),
        ClientError::Refused(RequestError::AlreadyInRoom)
    );
    server.shut_down().await;
}

#[tokio::test]
async fn settings_out_of_range_are_refused() {
    let server = RunningServer::start(|_| {}).await;
    let ann = server.client("ann").await;
    let bad = [
        RoomSettings {
            steps_per_second: 0,
            ..FAST
        },
        RoomSettings {
            input_delay_ms: 5,
            ..FAST
        },
        RoomSettings {
            checkpoint_interval: 0,
            ..FAST
        },
    ];
    for settings in bad {
        assert_eq!(
            ann.client
                .create_room(room("bad", settings))
                .await
                .unwrap_err(),
            ClientError::Refused(RequestError::InvalidSettings)
        );
    }
    let zero_players = CreateRoom {
        max_players: 0,
        ..room("bad", FAST)
    };
    assert_eq!(
        ann.client.create_room(zero_players).await.unwrap_err(),
        ClientError::Refused(RequestError::InvalidSettings)
    );
    server.shut_down().await;
}

#[tokio::test]
async fn the_server_caps_its_rooms() {
    let server = RunningServer::start(|config| config.max_rooms = 1).await;
    let ann = server.client("ann").await;
    let bob = server.client("bob").await;
    ann.client.create_room(room("one", FAST)).await.unwrap();
    assert_eq!(
        bob.client.create_room(room("two", FAST)).await.unwrap_err(),
        ClientError::Refused(RequestError::TooManyRooms)
    );
    server.shut_down().await;
}

#[tokio::test]
async fn an_address_has_only_so_many_open_rooms() {
    let server = RunningServer::start(|config| config.max_rooms_per_address = 1).await;
    let ann = server.client("ann").await;
    let bob = server.client("bob").await;
    ann.client.create_room(room("one", FAST)).await.unwrap();
    // Bob connects from the same address as Ann.
    assert_eq!(
        bob.client.create_room(room("two", FAST)).await.unwrap_err(),
        ClientError::Refused(RequestError::TooManyRooms)
    );
    ann.client.leave_room().await.unwrap();
    server.wait_for_rooms(0).await;
    bob.client.create_room(room("two", FAST)).await.unwrap();
    server.shut_down().await;
}

#[tokio::test]
async fn the_owner_can_kick_a_player_for_good() {
    let server = RunningServer::start(|_| {}).await;
    let ann = server.client("ann").await;
    let mut bob = server.client("bob").await;
    let cat = server.client("cat").await;
    let (invite, _) = ann.client.create_room(room("table", FAST)).await.unwrap();
    bob.client.join_room(join(&invite)).await.unwrap();
    cat.client.join_room(join(&invite)).await.unwrap();
    let bob_id = bob.client.player();

    assert_eq!(
        cat.client.kick(bob_id).await.unwrap_err(),
        ClientError::Refused(RequestError::NotOwner)
    );
    assert_eq!(
        ann.client.kick(ann.client.player()).await.unwrap_err(),
        ClientError::Refused(RequestError::CannotKickSelf)
    );
    ann.client.kick(bob_id).await.unwrap();
    bob.wait_for(|event| matches!(event, ClientEvent::Kicked).then_some(()))
        .await;
    assert_eq!(
        ann.client.kick(bob_id).await.unwrap_err(),
        ClientError::Refused(RequestError::NoSuchPlayer)
    );
    // Bob cannot come back, even with the invite.
    assert_eq!(
        bob.client.join_room(join(&invite)).await.unwrap_err(),
        ClientError::Refused(RequestError::BadInvite)
    );
    // He is free to make a room of his own.
    bob.client.create_room(room("mine", FAST)).await.unwrap();
    server.shut_down().await;
}

#[tokio::test]
async fn starting_requires_the_owner_readiness_and_matching_content() {
    let server = RunningServer::start(|_| {}).await;
    let ann = server.client("ann").await;
    let bob = server.client("bob").await;
    let (invite, _) = ann.client.create_room(room("table", FAST)).await.unwrap();
    bob.client.join_room(join(&invite)).await.unwrap();

    let refused = |error| Err(ClientError::Refused(error));
    assert_eq!(
        ann.client.start_game().await,
        refused(RequestError::NotAllReady)
    );
    ann.client.set_ready(true).await.unwrap();
    bob.client.set_ready(true).await.unwrap();
    assert_eq!(
        ann.client.start_game().await,
        refused(RequestError::ContentMismatch)
    );
    ann.client.declare_content(content(1)).await.unwrap();
    bob.client.declare_content(content(2)).await.unwrap();
    assert_eq!(
        ann.client.start_game().await,
        refused(RequestError::ContentMismatch)
    );
    bob.client.declare_content(content(1)).await.unwrap();
    assert_eq!(
        bob.client.start_game().await,
        refused(RequestError::NotOwner)
    );
    ann.client.start_game().await.unwrap();
    assert_eq!(
        ann.client.start_game().await,
        refused(RequestError::GameRunning)
    );
    // The lobby is closed to changes once the game runs.
    assert_eq!(
        bob.client.set_ready(false).await,
        refused(RequestError::GameRunning)
    );
    server.shut_down().await;
}
