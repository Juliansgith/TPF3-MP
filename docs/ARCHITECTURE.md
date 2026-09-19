# TPF3-MP architecture

Status: design baseline, 2026-09-18. Transport Fever 3 releases on 2026-09-29;
everything that depends on the game binary is marked **[needs game]** and is
settled by the release-day investigation in [DAY_ONE.md](DAY_ONE.md). Decisions
and the alternatives they rejected are logged in [DECISIONS.md](DECISIONS.md).

## Goals

- Multiplayer for Transport Fever 3 with any number of players, in competitive
  (separate companies) and co-op (shared company) rooms.
- Dedicated servers operated by the project: first node in Germany, more regions
  later. Players never host and never open a port.
- Mixed platforms in one room: Windows x64, Linux x64 and macOS arm64.
- Custom economy rules are a first-class feature, not a patch on top.
- Secure against outsiders and cheating players, many rooms per server node, and
  negligible overhead inside the game process.

## Non-goals

- Consoles. Mod Hub content is data and script only; the native layer this
  design needs cannot run there.
- Running TPF3 on the server. There is no headless build, and every instance
  needs its own licence. The server is authoritative for canonical state, not
  for the native simulation.
- Community-hosted servers. Possible later, but the trust model below assumes
  operated servers.

## The deciding constraint: mixed platforms

Native lockstep replays player commands and trusts the engine to compute
identical results everywhere, which requires a bit-identical simulation. The
Windows, Linux and macOS builds are different binaries from different
compilers, and macOS runs on arm64, where compilers contract `a*b+c` into a
fused multiply-add by default. Their simulations should be expected to drift
apart. Release day measures this, but the design must not depend on the
answer.

**Principle: canonical state is the truth; every native world is a replica.**

The state that decides a match is a canonical state machine that only the
server advances, with integer and fixed-point deterministic code. It covers
companies, money, ownership, entity identities, network topology, lines,
vehicle assignments, the economy model and the calendar. Native TPF3 worlds
apply the same ordered commands at the same simulation step so they stay close
to it, and they are checked against it. Their own economy output never decides
anything.

This is TPF2MP's authority model (`tf2mod`), moved from Player 1's companion
to the dedicated server and generalised to N players. It keeps the lockstep
mechanics of `tpf2-multiplayer`, which keep native replicas close and let
same-binary rooms relax drift control:

- step-exact command application;
- pacing;
- world hashes;
- resync.

## Components

```
TPF3 process ─ Lua mod: capture intents, apply ordered events at step N, report
             │          postconditions and observations, show canonical values
             └ tpf3mp-hook: command capture/cancel, step gate, speed, save/load,
                   │        Lua bindings                          [needs game]
                   │ shared-memory rings (no file polling)
tpf3mp-agent ─ session, turn buffer, snapshot cache, launcher backend
                   │ QUIC (TLS 1.3); WebSocket over TLS on 443 as fallback
tpf3mp-server ─ identity, rooms, lobby, sequencer and turn seals, canonical
                state machine (rules, economy), quorum, event log, snapshot
                store, admin API, metrics
```

| crate | role |
|---|---|
| `tpf3mp-proto` | Wire messages, framing, size limits, versioning. No I/O. |
| `tpf3mp-canon` | Canonical state machine and rules. Deterministic by construction: no float arithmetic, clocks, hash-ordered collections or I/O (enforced by lints). |
| `tpf3mp-net` | QUIC endpoints, TLS configuration, framed stream I/O. |
| `tpf3mp-server` | The dedicated server. |
| `tpf3mp-agent` | The client daemon next to the game. |
| `tpf3mp-hook` | In-game native library per platform. **[needs game]** |
| `tpf3mp-testkit` | Toy deterministic game, bots and network emulator for integration and load tests (milestone M1). |

Everything is Rust. The hook and agent talk through a small shared-memory ABI,
never the network protocol, so the hook stays small and independently
testable.

Players drive the agent from the launcher, a page it serves on the loopback
interface (`tpf3mp-agent launcher`): connecting, rooms, readiness, chat and
the game's progress. An in-game interface can use the same actions through
the hook's Lua bindings once they exist.

## Authority and data flow

1. **Intent.** A player acts in the stock UI. The hook captures the native
   command before it applies and cancels it, so every replica stays on one
   timeline. A command the engine cannot cancel without breaking its widget
   is applied optimistically on the issuing replica and has its residue
   handled fail-closed, as TPF2MP does for line editors. The Lua mod converts
   the capture into a portable intent: positions, resource names and canonical
   IDs, never machine-local entity IDs.
2. **Validate and order.** The agent sends the intent to the server, which
   checks it against canonical state: role, company, ownership, funds, schema,
   size and rate. The server assigns canonical IDs to anything the intent
   creates and appends it to the room's event log at an execution step.
3. **Turn seal.** Roughly every 100 ms the server broadcasts a sealed turn:
   "steps N..M are closed; these are their events, in order". A replica never
   simulates an unsealed step, and the hook applies step N's events
   immediately before step N runs. So:
   - a late command cannot happen;
   - speed and pause belong to the server;
   - the message rate does not depend on game speed.
4. **Apply and observe.** Each replica applies the event to its native world
   and reports back:
   - physical postconditions: canonical topology descriptors and bound outputs;
   - observations: vehicle arrivals, construction completion.

   Geometry is compared within a tolerance and discrete facts exactly, so
   last-bit float differences between platforms never count as disagreement.
5. **Quorum.** The server judges reports against the canonical intent where it
   can, and otherwise by majority. Ties go to the room's anchor replica, which
   the server chooses and which prefers the most common platform. Agreement
   advances canonical state, e.g. binding identities or committing finance.
   Replicas that disagree are faulted and rebased (see Snapshots). Canonical
   state never depends on a single client's report.
6. **Settle.** Economy settlement runs in the canonical state machine on the
   server at canonical boundaries, from canonical facts only. The results are
   broadcast, and the Lua mod shows canonical values in the stock UI.

## Drift control for native replicas

These mechanisms come from TPF2MP, where both worlds were already independent:

- **Vehicles: station rendezvous.** A canonical vehicle may leave a stop only
  after every replica has reported its arrival; the server then orders the
  release. This bounds route-phase drift for any platform mix.
- **Passengers and cargo.** The canonical economy model owns queues, loads and
  revenue. Native agents are presentation.
- **Towns and industries.** Native autonomy is gated. Growth and production are
  canonical decisions issued as ordered commands.
- **Calendar.** Canonical, projected into the native HUD.
- **Detection.** Per-lane digests at checkpoints: topology, constructions,
  vehicle phase, towns.
- **Repair.** A drifted replica is rebased from the latest agreed snapshot
  plus the event log.

A room whose replicas all run the same binary may relax drift control if the
release-day measurements show the native simulation is deterministic. That is
an optimisation, never a correctness requirement.

## Economy and custom rules

Rules live in `tpf3mp-canon` and run on the server. TPF2MP's economy is the
starting point: integer cents, ppm shares, the fixed `EXP_TABLE` logit,
largest-remainder allocation. Its parity vectors (`tests/run_economy_parity_vectors.lua`,
`tests/check_economy_parity.py` in `tf2mod`) become the conformance tests of
the Rust port. Consequences:

- **No client updates for balance changes.** They ship with the server.
- **Clients cannot forge money.** They never compute it.
- **Rulesets are per room:** competitive, co-op or custom.
- **Hidden information works.** Sealed bids or secret contracts stay on the
  server until it reveals them in a sealed turn.
- **TPF3's native economy is presentation in every mode.** The economy reads
  canonical facts, never native agents. (A co-op variant that trusts one
  designated replica's native economy is an open product question below.)

## Transport

**QUIC** (quinn, TLS 1.3 only) on one dedicated UDP port per node. The
connection carries:

| channel | carries |
|---|---|
| control stream | handshake, room membership, lobby |
| turn stream | sealed turns, server to client |
| intent stream | intents and reports, client to server |
| bulk streams | snapshots, so a 100 MB+ transfer never delays turns |
| datagrams | advisory traffic: cursors, build previews, pings |

**Fallback:** for networks that block UDP, the same QUIC connection runs
through a WebSocket over TLS on TCP 443, one datagram per message, usually
behind the reverse proxy. The server merges tunnels into its QUIC endpoint,
so nothing above the transport knows the difference (see "Tunnels" in
PROTOCOL.md).

**Frames:** a little-endian `u32` length plus a postcard-encoded message.
Each channel has a hard frame cap, and every message is validated after
decoding. Canonical payloads carry integers and IEEE-754 bit patterns, never
float text, so digests are exact.

**Version:** the handshake checks the exact protocol version and rejects a
mismatch with an actionable reason before anything else is exchanged.

## Snapshots

There are two kinds:

- **Canonical state:** small, kept by the server at every checkpoint.
- **Native world saves:** large, loadable across platforms (announced for
  TPF3; verified on release day).

Native saves are:

- split with content-defined chunking;
- compressed with zstd and addressed by BLAKE3;
- deduplicated, so a rejoining or rebased player downloads only the chunks
  it does not already have.

The server keeps the latest agreed native save and the event log since it.
Hot-join, reconnect, crash resume, rebasing and persistent worlds all use that
one path.

## Security

- **Transport:** TLS 1.3 only. No homemade cryptography.
- **Identity:**
  - one Ed25519 key per install, so no accounts;
  - session resumption tickets;
  - room invites as 256-bit tokens stored as peppered HMAC (the
    `tf2mp-relay` design);
  - an optional room password.
- **Authorisation:**
  - the server binds each connection to one player, one role and at most
    one company;
  - it validates every intent for schema, size, rate, permission and funds;
  - parsers are bounded and fuzzed.
- **Privacy:** clients never learn each other's IP addresses.
- **Saves are code.** A save's Lua sidecar is validated as data before a client
  loads a server-supplied world, as TPF2MP's `save_metadata.py` does.
- **Server trust:** servers are operated by the project and trusted.
  Community hosting would additionally need player-signed intents.
- **Admin API:** on a separate listener, never exposed publicly.

## Scale and operations

- **Sizing:**
  - one room lives on one node;
  - sequencing, validation and settlement are cheap;
  - node capacity is set by load tests (target: thousands of rooms on a
    4-vCPU node) and by snapshot bandwidth.
- **First deployment:** the existing German server. It runs as a hardened
  container next to `tf2mp-relay`, with the same deployment profile:
  - non-root, read-only root filesystem;
  - dropped capabilities;
  - a dedicated data volume.
- **Later:**
  - a directory service places each new room in the region that minimises
    its players' worst RTT;
  - nodes announce their capacity;
  - rooms stay on one node.
- **Observability:** Prometheus metrics, structured logs with redaction,
  non-secret support IDs.

## Platforms and the native hook [needs game]

| OS | loader | notes |
|---|---|---|
| Windows x64 | proxy DLL next to the executable, chosen from its import table | as in both TPF2 mods |
| Linux x64 | `LD_PRELOAD` through the Steam launch options | straightforward |
| macOS arm64 | proxy of a bundled dylib, or re-signed insertion | code signing and W^X make patching harder; feasibility is a release-day question |

Hooks find their targets by byte signature, with one profile per game build.
On an unknown build they switch multiplayer off instead of patching blindly.
TPF3 will be patched often after launch; TPF2's build 35924 was frozen for
years, which is why exact addresses were workable there.

The Lua mod is the cross-platform part and is kept as large as the script
API allows. Native code does only what script cannot: cancelling commands,
gating steps, controlling speed and save/load, and the fast IPC path.

## Carried over from the TPF2 projects

| from | what |
|---|---|
| `tf2mod` (TPF2MP) | Authority model, fail-closed rules, canonical identities, station rendezvous, economy model and parity vectors, content fingerprinting, recovery discipline, installer/updater. |
| `tf2mp-relay` | Credential design, digest-bound lobby state machine, redacted diagnostics and support IDs, hardened container deployment. |
| `tpf2-multiplayer` | Step-exact application, pacing and catch-up lessons, lane hashing with adaptive cadence, resync flow, companies as engine players, position-based command encoding, deterministic wrapper for third-party scripts, the `__FUNCSIG__`/RTTI/Ghidra RE pipeline. |

Not carried over:

- file-polling IPC;
- stamp-in-the-future scheduling (replaced by turn seals);
- `seal.py` cryptography (replaced by TLS 1.3);
- a player as the authority;
- Python on the hot path;
- exact-address hooks.

## Open questions

- [needs game] Is TPF3 the same engine lineage as TPF2, with the same command
  pipeline, RTTI and assert strings?
- [needs game] What Lua version and sandbox do game and GUI scripts get?
- [needs game] Is there any anti-tamper?
- [needs game] How deterministic is the native simulation per platform pair?
- [needs game] Is the macOS hook feasible, and are saves really cross-platform?
- Product: should co-op offer a mode that trusts one designated replica's
  native economy instead of the canonical model? It would mean more native
  fidelity, but that player's machine becomes the economic truth.
