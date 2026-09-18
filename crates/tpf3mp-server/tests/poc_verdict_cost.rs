//! CPU proof of concept from the security review. `verdict` is private to
//! the server crate, so this benchmarks a verbatim copy of
//! `verdict::decide` (crates/tpf3mp-server/src/verdict.rs at da546d9) on the
//! largest round a room allows: 64 members, 32 lanes each, every member
//! using its own lane numbers. Run in release mode:
//!
//! ```sh
//! cargo test --release -p tpf3mp-server --test poc_verdict_cost -- --ignored --nocapture
//! ```

#![allow(clippy::unwrap_used)]

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

use tpf3mp_proto::{
    Arch, FixedBytes, LaneDigest, MAX_CHECKPOINT_LANES, MAX_ROOM_MEMBERS, Os, Platform, PlayerId,
};

// ---- verbatim copy of verdict.rs (types made local) ----
#[derive(Debug, Clone)]
struct Report {
    player: PlayerId,
    platform: Platform,
    order: usize,
    lanes: Vec<LaneDigest>,
}

type Verdict = BTreeMap<u16, FixedBytes<32>>;

fn decide(reports: &[Report]) -> (Verdict, Vec<(PlayerId, Vec<u16>)>) {
    let Some(anchor) = anchor(reports) else {
        return (Verdict::new(), Vec::new());
    };
    let lanes: BTreeSet<u16> = reports
        .iter()
        .flat_map(|report| report.lanes.iter().map(|lane| lane.lane))
        .collect();
    let mut verdict = Verdict::new();
    for lane in lanes {
        let values: Vec<Option<FixedBytes<32>>> = reports
            .iter()
            .map(|report| digest_of(&report.lanes, lane))
            .collect();
        let winner = strict_majority(&values).unwrap_or(values[anchor]);
        if let Some(digest) = winner {
            verdict.insert(lane, digest);
        }
    }
    let diverged = reports
        .iter()
        .filter_map(|report| {
            let lanes = diverging_lanes(&report.lanes, &verdict);
            (!lanes.is_empty()).then_some((report.player, lanes))
        })
        .collect();
    (verdict, diverged)
}

fn diverging_lanes(lanes: &[LaneDigest], verdict: &Verdict) -> Vec<u16> {
    let mut ids: BTreeSet<u16> = verdict.keys().copied().collect();
    ids.extend(lanes.iter().map(|lane| lane.lane));
    ids.into_iter()
        .filter(|id| digest_of(lanes, *id) != verdict.get(id).copied())
        .collect()
}

fn digest_of(lanes: &[LaneDigest], lane: u16) -> Option<FixedBytes<32>> {
    lanes
        .iter()
        .find(|entry| entry.lane == lane)
        .map(|entry| entry.digest)
}

fn strict_majority<T: PartialEq + Copy>(values: &[T]) -> Option<T> {
    values.iter().copied().find(|candidate| {
        values.iter().filter(|value| *value == candidate).count() * 2 > values.len()
    })
}

fn anchor(reports: &[Report]) -> Option<usize> {
    let share = |platform: Platform| {
        reports
            .iter()
            .filter(|report| report.platform == platform)
            .count()
    };
    (0..reports.len()).min_by_key(|&index| {
        let report = &reports[index];
        (std::cmp::Reverse(share(report.platform)), report.order)
    })
}
// ---- end of copy ----

/// FINDING: deciding a checkpoint round costs about
/// `lanes x reports x (lanes per report + reports)` comparisons of 32-byte
/// digests, and lane numbers are arbitrary `u16`s. A room of 64 throwaway
/// identities, each reporting 32 lanes of its own (about 70 KB of input per
/// round), makes the room task spend this long per round, synchronously on
/// a runtime worker (the deployment has 2), once per checkpoint step, and a
/// room may check every step (`checkpoint_interval` 1).
#[test]
#[ignore = "security PoC (demonstration, passes while the finding exists)"]
fn poc_one_round_of_hostile_reports_costs_milliseconds() {
    let members = usize::from(MAX_ROOM_MEMBERS);
    let reports: Vec<Report> = (0..members)
        .map(|member| Report {
            player: PlayerId(FixedBytes([u8::try_from(member).unwrap(); 32])),
            platform: Platform {
                os: Os::Windows,
                arch: Arch::X86_64,
            },
            order: member,
            lanes: (0..MAX_CHECKPOINT_LANES)
                .map(|k| LaneDigest {
                    lane: u16::try_from(member * MAX_CHECKPOINT_LANES + k).unwrap(),
                    digest: FixedBytes([u8::try_from(k).unwrap(); 32]),
                })
                .collect(),
        })
        .collect();
    let rounds = 20;
    let started = Instant::now();
    let mut notices = 0;
    for _ in 0..rounds {
        notices += decide(std::hint::black_box(&reports)).1.len();
    }
    let per_round = started.elapsed() / rounds;
    println!(
        "one round of {members} reports x {MAX_CHECKPOINT_LANES} disjoint lanes: {per_round:?} \
         of room-task CPU ({notices} divergence notices over {rounds} rounds)"
    );
    assert!(per_round.as_micros() > 1_000);
}
