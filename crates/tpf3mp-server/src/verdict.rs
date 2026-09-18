//! Checkpoint verdicts: which lane digests are right when members disagree.
//! See "Quorum" in `docs/ARCHITECTURE.md`.

use std::collections::{BTreeMap, BTreeSet};

use tpf3mp_proto::{FixedBytes, LaneDigest, Platform, PlayerId};

/// One member's digests at a checkpoint.
#[derive(Debug, Clone)]
pub(crate) struct Report {
    pub(crate) player: PlayerId,
    pub(crate) platform: Platform,
    /// Position in the room's join order, for breaking ties.
    pub(crate) order: usize,
    /// Sorted by lane, one entry per lane.
    pub(crate) lanes: Vec<LaneDigest>,
}

/// The agreed digest of every lane. A lane the verdict lacks is one the
/// deciding reports did not include.
pub(crate) type Verdict = BTreeMap<u16, FixedBytes<32>>;

/// Decides a round. For every lane, a strict majority of reports wins; without
/// one, the anchor's digest wins. The anchor is the reporter on the most
/// common platform among the reports, earliest in join order: on a mixed
/// room, the platform most players share is the likeliest to agree with the
/// rest of the room later. Returns the verdict and the diverging lanes of
/// every player who differs from it.
pub(crate) fn decide(reports: &[Report]) -> (Verdict, Vec<(PlayerId, Vec<u16>)>) {
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

/// The lanes where `lanes` differs from `verdict`, including lanes present on
/// only one side.
pub(crate) fn diverging_lanes(lanes: &[LaneDigest], verdict: &Verdict) -> Vec<u16> {
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

#[cfg(test)]
mod tests {
    use tpf3mp_proto::{Arch, Os};

    use super::*;

    const WINDOWS: Platform = Platform {
        os: Os::Windows,
        arch: Arch::X86_64,
    };
    const MAC: Platform = Platform {
        os: Os::MacOs,
        arch: Arch::Aarch64,
    };

    fn digest(value: u8) -> FixedBytes<32> {
        FixedBytes([value; 32])
    }

    fn report(player: u8, platform: Platform, lanes: &[(u16, u8)]) -> Report {
        Report {
            player: PlayerId(FixedBytes([player; 32])),
            platform,
            order: usize::from(player),
            lanes: lanes
                .iter()
                .map(|&(lane, value)| LaneDigest {
                    lane,
                    digest: digest(value),
                })
                .collect(),
        }
    }

    fn player(id: u8) -> PlayerId {
        PlayerId(FixedBytes([id; 32]))
    }

    #[test]
    fn unanimous_reports_diverge_nowhere() {
        let reports = [
            report(1, WINDOWS, &[(0, 7), (1, 8)]),
            report(2, MAC, &[(0, 7), (1, 8)]),
        ];
        let (verdict, diverged) = decide(&reports);
        assert_eq!(verdict.len(), 2);
        assert!(diverged.is_empty());
    }

    #[test]
    fn the_majority_wins_each_lane() {
        let reports = [
            report(1, WINDOWS, &[(0, 7), (1, 8)]),
            report(2, WINDOWS, &[(0, 7), (1, 9)]),
            report(3, MAC, &[(0, 6), (1, 9)]),
        ];
        let (verdict, diverged) = decide(&reports);
        assert_eq!(verdict[&0], digest(7));
        assert_eq!(verdict[&1], digest(9));
        assert_eq!(diverged, vec![(player(1), vec![1]), (player(3), vec![0])]);
    }

    #[test]
    fn a_tie_goes_to_the_most_common_platform() {
        // Two players per platform and no majority digest: the platforms tie,
        // so join order decides and player 1 is the anchor.
        let reports = [
            report(1, MAC, &[(0, 1)]),
            report(2, WINDOWS, &[(0, 2)]),
            report(3, WINDOWS, &[(0, 2)]),
            report(4, MAC, &[(0, 1)]),
        ];
        let (verdict, _) = decide(&reports);
        assert_eq!(verdict[&0], digest(1));

        // Three Windows players, one Mac: the anchor is on Windows even
        // though the Mac player joined first.
        let reports = [
            report(1, MAC, &[(0, 1)]),
            report(2, WINDOWS, &[(0, 2)]),
            report(3, WINDOWS, &[(0, 3)]),
            report(4, WINDOWS, &[(0, 4)]),
        ];
        let (verdict, diverged) = decide(&reports);
        assert_eq!(verdict[&0], digest(2));
        assert_eq!(diverged.len(), 3);
    }

    #[test]
    fn two_players_disagreeing_resolve_to_the_anchor() {
        let reports = [report(1, WINDOWS, &[(0, 1)]), report(2, WINDOWS, &[(0, 2)])];
        let (verdict, diverged) = decide(&reports);
        assert_eq!(verdict[&0], digest(1));
        assert_eq!(diverged, vec![(player(2), vec![0])]);
    }

    #[test]
    fn a_missing_lane_is_a_divergence() {
        let reports = [
            report(1, WINDOWS, &[(0, 1), (5, 1)]),
            report(2, WINDOWS, &[(0, 1), (5, 1)]),
            report(3, WINDOWS, &[(0, 1)]),
        ];
        let (_, diverged) = decide(&reports);
        assert_eq!(diverged, vec![(player(3), vec![5])]);
    }

    #[test]
    fn a_late_report_is_judged_against_the_verdict() {
        let (verdict, _) = decide(&[
            report(1, WINDOWS, &[(0, 1), (1, 1)]),
            report(2, WINDOWS, &[(0, 1), (1, 1)]),
        ]);
        let late = report(3, MAC, &[(0, 1), (1, 2), (2, 2)]).lanes;
        assert_eq!(diverging_lanes(&late, &verdict), vec![1, 2]);
    }

    #[test]
    fn no_reports_decide_nothing() {
        let (verdict, diverged) = decide(&[]);
        assert!(verdict.is_empty());
        assert!(diverged.is_empty());
    }
}
