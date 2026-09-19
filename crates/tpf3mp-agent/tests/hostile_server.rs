//! From the security review: turn streams a hostile or broken server can
//! send, which `TurnFollower` must refuse rather than follow into a wedged
//! or crashed state.

#![allow(clippy::unwrap_used)]

use tpf3mp_agent::{Action, TurnFollower};
use tpf3mp_proto::{Event, EventBody, FixedBytes, PlayerId, RoomId, Speed, Turn, TurnStart};

fn start(next_turn: u64, next_event: u64) -> TurnStart {
    TurnStart {
        room: RoomId(FixedBytes([0; 16])),
        next_turn,
        next_event,
        sealed_through: 0,
        steps_per_second: 5,
        checkpoint_interval: 10,
        history: 0,
        world: None,
    }
}

fn event(seq: u64, step: u64) -> Event {
    Event {
        seq,
        step,
        body: EventBody::PlayerLeft {
            player: PlayerId(FixedBytes([1; 32])),
            kicked: false,
        },
    }
}

fn turn(number: u64, sealed_through: u64, events: Vec<Event>) -> Turn {
    Turn {
        number,
        sealed_through,
        speed: Speed::NORMAL,
        events,
    }
}

fn drain(follower: &mut TurnFollower) -> (Vec<u64>, u64) {
    let mut applied = Vec::new();
    let mut executed = 0;
    while let Some(action) = follower.next_action() {
        match action {
            Action::Apply(event) => applied.push(event.seq),
            Action::Execute(step) => executed = step,
        }
    }
    (applied, executed)
}

/// FINDING: `TurnFollower::accept` only checks `event.step > sealed_through`.
/// It accepts steps that go backwards, while `next_action` only ever looks at
/// the front of its queue. An event whose step has already run sits at the
/// front forever and silently blocks every later event, while steps keep
/// executing: the replica forks without any error.
#[test]
fn events_whose_steps_go_backwards_are_refused() {
    let mut follower = TurnFollower::new(&start(1, 1));
    let accepted = follower.accept(turn(1, 200, vec![event(1, 100), event(2, 50)]));
    let (applied, executed) = drain(&mut follower);
    let later = follower.accept(turn(2, 300, vec![event(3, 201)]));
    let (applied_later, executed_later) = drain(&mut follower);
    assert!(
        accepted.is_err(),
        "accepted events for steps 100 then 50: executed through step {executed} applying only \
         {applied:?}; the next turn ({later:?}) executed through {executed_later} applying \
         {applied_later:?}"
    );
}

/// FINDING: the protocol gives every event the step `sealed_through + 1`, but
/// the follower accepts any later step. A server that holds the frontier and
/// sends events for a step beyond the next one makes the follower keep every
/// event, with no bound, and apply none of them.
#[test]
fn events_beyond_the_next_step_are_refused() {
    let mut follower = TurnFollower::new(&start(1, 1));
    let mut held = 0;
    for number in 1..=10_000 {
        let events = vec![event(number, 2)];
        if follower.accept(turn(number, 0, events)).is_err() {
            break;
        }
        held += 1;
    }
    let (applied, _) = drain(&mut follower);
    assert_eq!(
        held,
        0,
        "the follower holds {held} events it cannot apply (applied: {})",
        applied.len()
    );
}

/// FINDING (debug builds): `accept` increments `next_turn` and the event
/// counter with `+=`, so a stream starting at `u64::MAX` panics with an
/// arithmetic overflow; release builds wrap and then report no last turn.
#[test]
fn counters_at_the_end_of_the_range_do_not_panic() {
    let turn_number = std::panic::catch_unwind(|| {
        let mut follower = TurnFollower::new(&start(u64::MAX, 1));
        let _ = follower.accept(turn(u64::MAX, 1, vec![]));
    });
    let event_number = std::panic::catch_unwind(|| {
        let mut follower = TurnFollower::new(&start(1, u64::MAX));
        let _ = follower.accept(turn(1, 1, vec![event(u64::MAX, 1)]));
    });
    assert!(
        turn_number.is_ok() && event_number.is_ok(),
        "a server-chosen turn or event number panics the follower (turn: {}, event: {})",
        turn_number.is_err(),
        event_number.is_err(),
    );
}
