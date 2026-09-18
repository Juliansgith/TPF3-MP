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

**Milestone M0: foundations.**

- **Done:** workspace and CI on Windows, Linux and macOS; the wire protocol's
  version preamble and framing; a QUIC/TLS 1.3 server and client that
  complete a handshake; determinism guardrails for the canonical rules.
- **Next, M1:** rooms, the sequencer and turn seals, the canonical economy
  port, and a test kit with a toy game, bots and a network emulator.

## Layout

| path | contents |
|---|---|
| `crates/tpf3mp-proto` | Wire messages, framing and limits. |
| `crates/tpf3mp-canon` | Canonical rules. Integer arithmetic only, enforced by lints. |
| `crates/tpf3mp-net` | QUIC endpoints, TLS configuration, framed stream I/O. |
| `crates/tpf3mp-server` | The dedicated server. |
| `crates/tpf3mp-agent` | The client daemon that runs next to the game. |
| `docs/` | Architecture, decisions, release-day plan. |

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
agents verify it against the public certificate authorities.

## License

MIT; see [LICENSE](LICENSE). This project is not affiliated with or endorsed
by Urban Games or Paradox Interactive, and it does not redistribute any part
of Transport Fever 3.
