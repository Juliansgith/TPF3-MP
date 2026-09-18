//! Counters exposed by the admin endpoint in the Prometheus text format.

use std::{
    fmt::Write,
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Debug, Default)]
pub(crate) struct Metrics {
    pub(crate) sessions_opened: AtomicU64,
    pub(crate) handshakes_refused: AtomicU64,
    pub(crate) protocol_violations: AtomicU64,
    pub(crate) rooms_created: AtomicU64,
    pub(crate) games_started: AtomicU64,
    pub(crate) turns_sealed: AtomicU64,
    pub(crate) events_ordered: AtomicU64,
    pub(crate) intents_refused: AtomicU64,
    pub(crate) divergences: AtomicU64,
    pub(crate) slow_consumers: AtomicU64,
    pub(crate) stalls: AtomicU64,
}

/// Values measured at scrape time rather than counted.
pub(crate) struct Gauges {
    pub(crate) sessions: usize,
    pub(crate) rooms: usize,
}

pub(crate) fn increment(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn add(counter: &AtomicU64, amount: u64) {
    counter.fetch_add(amount, Ordering::Relaxed);
}

impl Metrics {
    pub(crate) fn render(&self, gauges: &Gauges) -> String {
        let counters: [(&str, &str, &AtomicU64); 11] = [
            (
                "sessions_opened",
                "Sessions that completed the handshake.",
                &self.sessions_opened,
            ),
            (
                "handshakes_refused",
                "Connections refused or timed out during the handshake.",
                &self.handshakes_refused,
            ),
            (
                "protocol_violations",
                "Sessions closed for breaking the protocol.",
                &self.protocol_violations,
            ),
            ("rooms_created", "Rooms created.", &self.rooms_created),
            ("games_started", "Games started.", &self.games_started),
            ("turns_sealed", "Turns sealed and sent.", &self.turns_sealed),
            (
                "events_ordered",
                "Events ordered into room logs.",
                &self.events_ordered,
            ),
            (
                "intents_refused",
                "Intents refused by rate limits or rules.",
                &self.intents_refused,
            ),
            (
                "divergences",
                "Replicas found to differ from a checkpoint verdict.",
                &self.divergences,
            ),
            (
                "slow_consumers",
                "Sessions disconnected for not reading fast enough.",
                &self.slow_consumers,
            ),
            (
                "stalls",
                "Members that stopped advancing and no longer hold their room.",
                &self.stalls,
            ),
        ];
        let mut out = String::new();
        for (name, help, counter) in counters {
            let _ = writeln!(out, "# HELP tpf3mp_{name}_total {help}");
            let _ = writeln!(out, "# TYPE tpf3mp_{name}_total counter");
            let _ = writeln!(
                out,
                "tpf3mp_{name}_total {}",
                counter.load(Ordering::Relaxed)
            );
        }
        for (name, help, value) in [
            ("sessions", "Sessions open now.", gauges.sessions),
            ("rooms", "Rooms hosted now.", gauges.rooms),
        ] {
            let _ = writeln!(out, "# HELP tpf3mp_{name} {help}");
            let _ = writeln!(out, "# TYPE tpf3mp_{name} gauge");
            let _ = writeln!(out, "tpf3mp_{name} {value}");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_the_prometheus_text_format() {
        let metrics = Metrics::default();
        increment(&metrics.turns_sealed);
        add(&metrics.events_ordered, 5);
        let text = metrics.render(&Gauges {
            sessions: 3,
            rooms: 1,
        });
        assert!(
            text.contains(
                "# TYPE tpf3mp_turns_sealed_total counter\ntpf3mp_turns_sealed_total 1\n"
            )
        );
        assert!(text.contains("tpf3mp_events_ordered_total 5\n"));
        assert!(text.contains("# TYPE tpf3mp_sessions gauge\ntpf3mp_sessions 3\n"));
    }
}
