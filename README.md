# TPF3-MP

Multiplayer for Transport Fever 3, with dedicated servers. Players on Windows,
Linux and macOS can share one room.

Transport Fever 3 releases on 2026-09-29 and is single-player only. This
project adds multiplayer from outside the game. Until release, only the
network side can be built and tested; everything that touches the game
waits for the [release-day investigation](docs/DAY_ONE.md).

## How it works

- **The server owns the truth.** A dedicated server orders every player
  action and advances a deterministic canonical state machine: companies,
  money, ownership, lines, vehicles, economy.
- **Each player's game is a replica.** It applies the same ordered events at
  the same simulation step and reports back. The server checks the reports
  and rebases a replica that drifts.
- **Mixed platforms work.** The design never depends on different game builds
  simulating identically.

The full design is in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), and the
reasoning behind it in [docs/DECISIONS.md](docs/DECISIONS.md).

## Status

**Milestone M1: the core netcode, tested without the game.**

- **Protocol.** QUIC with TLS 1.3, per-install Ed25519 identities proven
  against the TLS session, and a version preamble frozen for good.
- **Rooms.** HMAC-tagged invites and optional passwords, a lobby with
  readiness and content fingerprints, owner hand-over.
- **Sequencer.** Hard lockstep turns. A server-owned clock holds for players
  who are loading or slow. Pause, speed, exact resume after a reconnect.
- **Protection.** Checkpoint verdicts that single out a diverged replica,
  rate limits, bounded queues, and eviction of slow consumers.
- **Test kit.** A toy game whose canonical rules run on the server, bots
  that play it through the real client, a lossy-network emulator and a load
  tester. 8 bots over a 150 ms, 2%-loss link agree on every lane, and 400
  bots in 50 rooms run without a divergence.
- **Operations.** Prometheus metrics, a hardened container image and a
  deployment runbook.

**In progress:** porting the TPF2MP economy, a snapshot store for hot-join
and rebasing, the in-game hook and IPC, the release-day RE kit.

## Layout

| path | contents |
|---|---|
| `crates/tpf3mp-proto` | Wire messages, framing and limits. |
| `crates/tpf3mp-canon` | Canonical rules. Integer arithmetic only, enforced by lints. |
| `crates/tpf3mp-net` | QUIC endpoints, TLS configuration, identities, framed stream I/O. |
| `crates/tpf3mp-server` | The dedicated server: rooms, sequencer, verdicts, metrics. |
| `crates/tpf3mp-agent` | The client library and CLI that run next to the game. |
| `crates/tpf3mp-testkit` | Toy game, bots, network emulator, load tester. |
| `deploy/` | Container image and compose file. |
| `docs/` | Architecture, protocol, decisions, operations, release-day plan. |

## Development

Requires Rust. The toolchain is pinned in `rust-toolchain.toml` and installed
automatically by rustup.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Run a local server with a throwaway certificate, then connect to it:

```sh
cargo run -p tpf3mp-server -- --dev-self-signed runtime/dev-cert.der
cargo run -p tpf3mp-agent -- connect 127.0.0.1:29470 --pin-cert runtime/dev-cert.der
```

In production, the server takes a real certificate (`--cert`, `--key`), and
agents verify it against the public certificate authorities. See
[docs/OPERATIONS.md](docs/OPERATIONS.md) for deployment.

Try a room by hand with two agents:

```sh
cargo run -p tpf3mp-agent -- host 127.0.0.1:29470 --pin-cert runtime/dev-cert.der --name ann
cargo run -p tpf3mp-agent -- join 127.0.0.1:29470 <invite> --pin-cert runtime/dev-cert.der --name bob
```

Load-test a server with bots:

```sh
cargo run --release -p tpf3mp-testkit --bin tpf3mp-loadtest -- --rooms 50 --bots 8
```

## License

MIT; see [LICENSE](LICENSE). This project is not affiliated with or endorsed
by Urban Games or Paradox Interactive, and it does not redistribute any part
of Transport Fever 3.
