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
    pub(crate) connections_refused: AtomicU64,
    pub(crate) retries_sent: AtomicU64,
    pub(crate) idle_sessions_closed: AtomicU64,
    pub(crate) rooms_abandoned: AtomicU64,
    pub(crate) saves: AtomicU64,
    pub(crate) snapshots_agreed: AtomicU64,
    pub(crate) uploads_failed: AtomicU64,
    pub(crate) late_joins: AtomicU64,
    pub(crate) rebases: AtomicU64,
    pub(crate) snapshot_bytes_served: AtomicU64,
    pub(crate) logs_compacted: AtomicU64,
    pub(crate) tunnels_opened: AtomicU64,
    pub(crate) tunnels_refused: AtomicU64,
}

/// Values measured at scrape time rather than counted.
pub(crate) struct Gauges {
    pub(crate) sessions: usize,
    pub(crate) rooms: usize,
    pub(crate) tunnels: usize,
}

pub(crate) fn increment(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn add(counter: &AtomicU64, amount: u64) {
    counter.fetch_add(amount, Ordering::Relaxed);
}

impl Metrics {
    pub(crate) fn render(&self, gauges: &Gauges) -> String {
        let counters: [(&str, &str, &AtomicU64); 24] = [
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
            (
                "connections_refused",
                "Connection attempts refused for too many handshakes in progress.",
                &self.connections_refused,
            ),
            (
                "retries_sent",
                "Connection attempts asked to prove their address first, under load.",
                &self.retries_sent,
            ),
            (
                "idle_sessions_closed",
                "Sessions closed for staying outside any room too long.",
                &self.idle_sessions_closed,
            ),
            (
                "rooms_abandoned",
                "Running games closed because nobody returned to them.",
                &self.rooms_abandoned,
            ),
            (
                "saves",
                "World saves sealed into running games.",
                &self.saves,
            ),
            (
                "snapshots_agreed",
                "Saves the room agreed on and the server received.",
                &self.snapshots_agreed,
            ),
            (
                "uploads_failed",
                "Saves a player was asked for and did not deliver.",
                &self.uploads_failed,
            ),
            (
                "late_joins",
                "Players who joined a game already running.",
                &self.late_joins,
            ),
            (
                "rebases",
                "Diverged replicas given the agreed world again.",
                &self.rebases,
            ),
            (
                "snapshot_bytes_served",
                "Compressed snapshot bytes sent to players.",
                &self.snapshot_bytes_served,
            ),
            (
                "logs_compacted",
                "Room logs rewritten to start from their game's current state.",
                &self.logs_compacted,
            ),
            (
                "tunnels_opened",
                "Tunnels opened: QUIC over WebSocket, for networks that block UDP.",
                &self.tunnels_opened,
            ),
            (
                "tunnels_refused",
                "Tunnel connections refused by the server-wide or per-address limit.",
                &self.tunnels_refused,
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
            ("tunnels", "Tunnels open now.", gauges.tunnels),
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
            tunnels: 0,
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
