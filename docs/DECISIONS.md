# Decision log

Each entry records a decision, why it was made, and what it rejected. A later
entry may supersede an earlier one; entries are never rewritten.

## D1 (2026-09-18): Rust for all project code

The server, agent, protocol, canonical rules and in-game hook are written in
Rust.

- **Performance.** On par with C++, with no garbage collector. That matters
  for the hook, which runs on the game's own threads.
- **Memory safety.** The server faces the internet, and the hook must not
  crash the game.
- **One language across both ends.** Client and server share the protocol and
  rules crates, so the two sides cannot drift apart.
- **Platforms.** One codebase builds for Windows x64, Linux x64 and macOS
  arm64. quinn (QUIC) runs on all three.

Rejected:

- **C++.** No memory safety, and the team prefers to avoid it.
- **C#.** A GC runtime inside the game process is a poor fit for detours on
  the simulation thread. Also, .NET's `System.Net.Quic` requires Windows 11 or
  Server 2022 (TPF3's minimum is Windows 10) and supports macOS only
  "partially, through a non-standard Homebrew package". See
  <https://learn.microsoft.com/en-us/dotnet/fundamentals/networking/quic/quic-overview>.
  The hook would still need a second, native language.
- **Python.** Both TPF2 projects use it. It has a performance ceiling for the
  server, and shipping it to players means PyInstaller executables.

## D2 (2026-09-18): canonical server authority, native worlds as replicas

The server advances a deterministic canonical state machine: companies,
money, ownership, identities, topology, lines, vehicles, economy, calendar.
Native TPF3 worlds apply the same ordered events at the same step, report
postconditions, and are rebased when they drift. See [ARCHITECTURE.md](ARCHITECTURE.md).

- Mixed platforms (Windows, Linux, macOS arm64) in one room are a hard
  requirement.
- Different binaries from different compilers, and arm64 FMA contraction,
  make bit-identical native simulation across platforms very unlikely.

This supersedes the first plan of 2026-09-18, "server-sequenced pure
lockstep", which relied on native determinism. That plan's turn-seal
sequencing is kept as the ordering and pacing layer.

Rejected:

- **Pure native lockstep.** It only works within one binary.
- **Trusting one designated replica's native economy.** That replica's machine
  becomes the economic truth. It is kept as an open co-op question, not as
  the foundation.

## D3 (2026-09-18): QUIC first, WebSocket over TLS as fallback

- QUIC gives TLS 1.3, independent streams (a snapshot transfer never delays a
  sealed turn), datagrams for advisory traffic, and connection migration.
- WebSocket over TLS on TCP 443 covers networks that block UDP.
- Both carry the same messages.

Rejected: TCP-only, which has head-of-line blocking between snapshots and
turns, and raw UDP with custom reliability and cryptography, which is what
`tpf2-multiplayer` had to build by hand.

Update (2026-09-19): the fallback carries QUIC itself, not the messages. A
tunnel is a WebSocket whose binary messages are QUIC datagrams, and the
server merges tunnels into its one QUIC endpoint. A second transport for the
same messages would have needed its own framing, multiplexing, flow control
and authentication, and every feature would have had to work on both. QUIC
inside TCP pays twice for congestion control and suffers TCP's head-of-line
blocking, which is acceptable for networks that leave no other way.

## D4 (2026-09-18): operated servers, trusted by clients

Servers are run by the project: first on the existing German server, then on
regional VPS nodes.

- Clients trust the server.
- Players authenticate to it with per-install keys and room invites.
- Community-run servers are out of scope. Supporting them later would require
  player-signed intents, so that a server cannot forge actions.

## D5 (2026-09-18): one team, both TPF2 codebases as input

Julian Cooper (TPF2MP, `tf2mp-relay`) and silver2127 (`tpf2-multiplayer`)
work on this repository together. Both TPF2 codebases are MIT licensed. Code
or test vectors taken from them are credited in the file that uses them.
