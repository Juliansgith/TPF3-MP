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
  who are loading or slow, and stops waiting for one that stalls. Pause,
  speed, and exact resume after a reconnect, which survives a server restart.
- **Playout.** Each client plays behind its own jitter buffer, so a player
  feels their own round trip plus a small buffer. Paced bots over 150 ms
  round trips see a median of about 220 ms, and a poor link delays only its
  owner.
- **Protection.** An adversarial security review found no critical issues.
  Every finding it made is fixed and guarded by a test:
  - per-address limits on sessions, handshakes and rooms;
  - QUIC retries under load;
  - budgets for turns, payload bytes and logs;
  - crash recovery that never damages a log or resumes a client onto
    turns it did not see;
  - checkpoint verdicts one member cannot switch off.
- **Economy.** TPF2MP's economy core ported to integer arithmetic. All
  46,048 of TPF2MP's parity vectors replay identically against the original
  Lua.
- **Snapshots.** A deduplicating chunk store for world saves: 100 scattered
  edits to a 120 MiB save transfer 8.9 MiB.
- **Hook engine.** Signature resolution with per-build profiles, an x86-64
  detour engine, a shared-memory link to the agent, and a proxy-DLL
  generator. All five known TPF2 targets resolve uniquely.
- **Test kit.** A toy game whose canonical rules run on the server, bots
  that play it through the real client, a lossy-network emulator and a load
  tester. 8 bots over a 150 ms, 2%-loss link agree on every lane, and 400
  bots in 50 rooms run without a divergence.
- **Operations.** Prometheus metrics, a hardened container image and a
  deployment runbook.

**In progress:** the agent-side game bridge, snapshot transfer for joining
running games, and the release-day RE kit.

## Layout

| path | contents |
|---|---|
| `crates/tpf3mp-proto` | Wire messages, framing and limits. |
| `crates/tpf3mp-canon` | Canonical rules. Integer arithmetic only, enforced by lints. |
| `crates/tpf3mp-net` | QUIC endpoints, TLS configuration, identities, framed stream I/O. |
| `crates/tpf3mp-server` | The dedicated server: rooms, sequencer, verdicts, metrics. |
| `crates/tpf3mp-agent` | The client library and CLI that run next to the game. |
| `crates/tpf3mp-snapshot` | Deduplicated storage and transfer of world saves. |
| `crates/tpf3mp-hookcore` | Signatures, per-build profiles and the detour engine. |
| `crates/tpf3mp-ipc` | The shared-memory link between the hook and the agent. |
| `crates/tpf3mp-hook` | The library injected into the game. |
| `crates/tpf3mp-proxygen` | Generates proxy DLLs that load the hook. |
| `crates/tpf3mp-testkit` | Toy game, bots, network emulator, load tester. |
| `tools/` | Release-day reverse-engineering and determinism probes. |
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

Play a room through the whole stack, with a fake game in place of TPF3.
Each game process attaches to its agent over shared memory and runs the toy
game behind the step gate, as the real hook will:

```sh
cargo run -p tpf3mp-agent -- host 127.0.0.1:29470 --pin-cert runtime/dev-cert.der --name ann --game-link ann --start-with 2
cargo run -p tpf3mp-testkit --bin tpf3mp-fakegame -- ann --steps 100
cargo run -p tpf3mp-agent -- join 127.0.0.1:29470 <invite> --pin-cert runtime/dev-cert.der --name bob --game-link bob
cargo run -p tpf3mp-testkit --bin tpf3mp-fakegame -- bob --steps 100
```

Both games print the same lane digests at the end.

Load-test a server with bots:

```sh
cargo run --release -p tpf3mp-testkit --bin tpf3mp-loadtest -- --rooms 50 --bots 8
```

## License

MIT; see [LICENSE](LICENSE). This project is not affiliated with or endorsed
by Urban Games or Paradox Interactive, and it does not redistribute any part
of Transport Fever 3.
