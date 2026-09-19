# Operations

How to run `tpf3mp-server` on a Linux host, starting with the German server
that already runs `tf2mp-relay`. The deployment mirrors the relay's
hardened container profile.

## What the server needs

- **UDP port 29470** open to the internet: players connect with QUIC.
  Nothing else needs to be public.
- **A TLS certificate** for a hostname that points at the server, such as
  `tpf3mp.<ip>.sslip.io`. Agents verify it against public certificate
  authorities, exactly as a browser would.
- **A data volume** holding:
  - `/data/invite.key`: losing it invalidates every invite, including those
    of restored games.
  - `/data/rooms/`: one log per running game, so games survive restarts,
    and a pointer to each game's current snapshot.
  - `/data/rooms/snapshots/`: the world snapshots, deduplicated chunks of
    the games' saves. At most 64 GiB by default (`--snapshot-gib`).

  Back up the key and the logs. Snapshots are rebuilt by the next save, so
  losing them only makes late joiners wait for one.

## First deployment

1. Point a hostname at the server, and open the port:
   `ufw allow 29470/udp`.
2. Get a certificate for that hostname (see [Certificates](#certificates)).
   Place `fullchain.pem` and `privkey.pem` in `deploy/certs/`, readable by
   UID 65532, the image's non-root user:
   ```sh
   sudo chown 65532:65532 deploy/certs/*.pem
   sudo chmod 0440 deploy/certs/*.pem
   ```
3. Build and start the server:
   ```sh
   cd deploy && docker compose up -d --build
   ```
4. Check it from any machine:
   ```sh
   tpf3mp-agent connect tpf3mp.example.org:29470
   ```
   This prints the server version, a session ID and the round trip.

## Certificates

The server reads its certificate at start. After a renewal, restart it:
`docker compose restart`. Two options:

- **certbot.** Use `certbot certonly --standalone -d <host>` while port 80
  is free, or `--webroot` behind the existing reverse proxy. Add a deploy
  hook that copies the renewed files into `deploy/certs/`, fixes their owner
  and restarts the container.
- **Reuse the existing Caddy.** Add the hostname to the Caddyfile so Caddy
  obtains the certificate, then copy it from Caddy's storage
  (`certificates/acme-v02.api.letsencrypt.org-directory/<host>/`) with a
  small scheduled job. Caddy's files are root-only, so a copy with the right
  owner is required; do not mount them directly.

For local development, `--dev-self-signed <file>` writes a throwaway
certificate that agents pin with `--pin-cert <file>`.

## Monitoring

- **Metrics.** `http://127.0.0.1:9470/metrics` serves Prometheus text on the
  host: sessions and rooms now, plus counters for handshakes refused,
  protocol violations, turns sealed, events ordered, intents refused,
  divergences and slow consumers, and for snapshots: saves, snapshots
  agreed, failed uploads, late joins, rebases and bytes served.
- **Health.** `/healthz` returns `ok`.
- **Logs.** Logs go to stdout (`docker compose logs -f`) and never contain
  IP addresses or invite tokens. `RUST_LOG=debug` adds per-connection
  refusals; `RUST_LOG=tpf3mp_server=debug,quinn=warn` narrows it.

A rise in `divergences_total` means replicas disagree with verdicts: look
at the platforms involved. A rise in `slow_consumers_total` means clients
cannot keep up with their turn streams. `uploads_failed_total` rising
while `snapshots_agreed_total` stands still means players' saves do not
reach the server: late joiners then wait.

## Upgrades

Every player must run the server's protocol version. The handshake tells
players on another version which side to update. To upgrade:

```sh
git pull && cd deploy && docker compose up -d --build
```

What happens during the restart:

1. The old container gets SIGTERM and closes every session with
   `SHUTTING_DOWN`.
2. With `--data-dir` (the image's default), every running game has been
   logged turn by turn. The new server restores those games at start.
3. Players reconnect with the same identity and resume after the last turn
   they applied. The event log continues without a gap. Lobbies that had not
   started are not kept.
4. A restored game that nobody reconnects to within 10 minutes closes and
   its log is deleted, like any running game whose players all disconnected.

Persistence details:

- **Crash safety.** Each turn is written to the operating system as it is
  sealed, so a process crash loses nothing. A power loss or kernel crash can
  lose the last few turns; a client that saw them is told
  `ResumeUnavailable`, even once the room has sealed new turns with the same
  numbers (see "Histories" in PROTOCOL.md). Logs from before format
  version 2 are set aside, not restored.
- **Damaged logs.** Recovery reads a log without changing it. A damaged final
  record is what a crash leaves behind, so it is cut off once the room is
  rebuilt. Any other damage leaves the log exactly as it was, renamed to
  `*.broken` (or `*.1.broken` and so on, never replacing an earlier one) and
  kept for diagnosis. Symbolic links are ignored.
- **Size limits.** A room's log stops growing at 1 GiB; the game continues
  but would not survive a restart. Each player may send 32 KiB of commands
  per second, with a 256 KiB burst, so an honest game takes days to get
  there. Recovery streams a log and holds at most the last 64 MiB of turns
  per room in memory.
- **Permissions.** On Linux, logs are readable by the server's user only:
  they hold invite and password tags and every command.
- **Invite key.** Restored games are rejoined with their original invites,
  which only verify with the same `invite.key`.

## Snapshots

With `--data-dir`, the server keeps world snapshots in `snapshots/` inside
it (`--snapshot-dir` puts them elsewhere, `--no-snapshots` turns them off).
They let players join a game that has started, rejoin one they can no
longer resume, and repair replicas that diverged. "Snapshots" in
PROTOCOL.md describes the flow.

- **When games save.** Every 10 minutes of play (`--save-every-secs`),
  sooner when a player waits for a world, never twice within a minute
  (`--save-gap-secs`). Every player's game saves at the same step, which
  the players see as a short pause, like an autosave.
- **Traffic.** One player uploads each save; successive saves share most of
  their chunks, so only what changed moves. A player who joins downloads
  the whole world once, then only changes. Up to 32 transfers run at once.
- **Disk.** Each game keeps its current snapshot and the one before; chunks
  both share are stored once. Closed games release theirs, and at start the
  server releases snapshots of games that are gone.

## Capacity

Measured with `tpf3mp-loadtest` on one Windows desktop, with the server and
400 bot clients in the same process:

- 50 rooms of 8 players;
- 1,000 steps at 50 steps per second;
- 229,000 events applied across replicas;
- no divergence;
- p99 command latency 112 ms on loopback.

Repeat against the real host after deploying. Every bot connects from the
machine running the load test, so first raise that address's limits on the
server, for example with `--max-sessions-per-address 1000
--max-handshakes-per-address 1000`, and restore them afterwards:

```sh
cargo run --release -p tpf3mp-testkit --bin tpf3mp-loadtest -- \
    --server tpf3mp.example.org:29470 --rooms 20 --bots 8 --paced
```

`--paced` makes the bots play at the room's pace behind a jitter buffer, as
games do, so the latencies it reports are the ones players would feel.

## Security notes

- The container runs as a non-root user with a read-only root filesystem,
  every Linux capability dropped, `no-new-privileges`, and limits on PIDs,
  memory and CPU. It mounts only its certificates (read-only) and its own
  data volume.
- The admin endpoint has no authentication. The compose file publishes it
  on the host's loopback only.
- One network address holds at most 8 sessions and 4 handshakes in progress
  (`--max-sessions-per-address`, `--max-handshakes-per-address`). An IPv6
  /64 counts as one address. A household or LAN party with more players
  behind one address needs a higher limit.
- Once half of the 256 handshake slots are busy, new clients must prove
  their address with a QUIC retry, so spoofed packets cost nothing. The
  `retries_sent` and `connections_refused` counters show when this happens.
- A session that stays outside any room for 10 minutes is closed
  (`idle_sessions_closed`).
- One address has at most 8 open rooms (`--max-rooms-per-address`). A room
  counts until it closes, and a running game with nobody connected closes
  after 10 minutes (`rooms_abandoned`). Throwaway identities therefore
  cannot fill the server's rooms.
- Rotate the invite key only deliberately: every existing invite stops
  working.
