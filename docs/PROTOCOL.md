# Protocol

This page defines the semantics and invariants of the TPF3-MP protocol. The
exact fields live in `crates/tpf3mp-proto`, which is the source of truth; this
page explains what they mean and which orderings are guaranteed. Design
background is in [ARCHITECTURE.md](ARCHITECTURE.md).

## Connection

One QUIC connection per client (ALPN `tpf3mp`, TLS 1.3 only). It carries
two kinds of stream:

- **The control stream** is bidirectional and opened by the client first. It
  carries the handshake, requests and responses, room updates, notices, and
  the client's game messages (intents, progress, checkpoints).
- **The turn stream** is unidirectional, server to client. The server opens it
  when the client enters a running game. It carries the room's ordered event
  log and nothing else.

Every stream starts with the version preamble. Frames and limits are described
in the `tpf3mp-proto` crate docs.

## Handshake and identity

1. Both sides exchange the preamble. If the versions differ, both close with
   `VERSION_MISMATCH`, and the client tells the player which side is older.
2. The client sends `Hello` with:
   - its version and platform;
   - a display name;
   - its **identity key**, a per-install Ed25519 public key;
   - a **proof**: an Ed25519 signature over `"tpf3mp-auth-v1" || E`. `E` is
     32 bytes of TLS keying material exported for this connection (label
     `EXPORTER-tpf3mp-auth`, empty context). Only a party inside this TLS
     session can compute `E`, so a proof cannot be replayed on another
     connection.
3. The server verifies the proof and answers `Welcome` or `Reject`.

A player *is* their identity key. There are no accounts or passwords. A
player who reconnects with the same key is the same player.

## Rooms

A room has a name, an owner, a player limit, settings, members, and a phase:
**lobby** or **running**.

- **Creating.** Any player can create a room and becomes its owner.
- **Invites.** The server answers with an **invite**,
  `TPF3MP1.<base64url(room id ‖ 256-bit token)>`. The server stores only an
  HMAC of the token under a server-side pepper and checks it in constant time.
  Invalid invites, unknown rooms and wrong passwords all fail the same way, so
  invites cannot be used to probe which rooms exist.
- **Updates.** Members receive the full room view (`RoomUpdate`) whenever it
  changes. Updates and responses are independent messages: a `RoomUpdate`
  caused by a request can arrive before that request's `Response`.
- **Starting.** In the lobby, members declare their **content fingerprint**
  (game build plus mod set digest) and toggle **ready**. The owner can start
  the game only when every member is ready and all fingerprints are equal.

## Turns: the ordered event log

A running room has a **sequencer**. Every tick (100 ms by default) it seals a
**turn**: a sequence number, the step **frontier** `sealed_through`, the
session speed, and the events appended since the previous turn. Each event has
a room-global sequence number and an execution step.

Invariants every client relies on:

1. **Events apply before their step.** An event with step `N` is applied
   after step `N-1` has executed and before step `N` executes. Events for the
   same step apply in sequence order.
2. **Steps run only when sealed.** A client never executes step `N` unless a
   received turn has `sealed_through >= N`.
3. **A sealed step is closed.** Once a turn announces `sealed_through = S`, no
   later event has a step `<= S`. The sequencer gives every new event the step
   `sealed_through + 1`.
4. **Turns are gap-free.** Turn numbers start at the number in the stream's
   `Start` message and increase by one; event sequence numbers increase by
   one across turns. A client that sees a gap closes the connection with
   `PROTOCOL_VIOLATION` and reconnects.

Together these make every replica apply the same events at the same point in
simulation time, whatever its latency. Invariant 1 also makes building work
while paused: a paused client has executed `sealed_through` and can apply the
events for `sealed_through + 1` at once, without running a step.

**Pacing.**
- **The server owns the clock.** It advances an ideal step count at
  `steps_per_second × speed`.
- **Frontier.** It seals up to that count plus a **lead** that hides network
  latency (the room's input delay).
- **Nobody runs ahead.** The frontier never goes further than a bounded
  window past the slowest active member's reported progress. A slow machine
  therefore slows the room instead of forking from it.
- **Speed.** Speed `0` pauses. Speed only changes pacing, never simulation
  results, so it travels in the turn header, not as an event. A speed change
  always produces a turn, even when nothing else changed, so a pause is
  announced.
- **Loading.** The clock holds until every member has reported progress
  `0`, meaning it has loaded the world.
- **Catching up.** A member who reconnects does not hold the room until its
  progress is back within the pacing window.

## Game messages from the client

These travel on the control stream.

- **`Intent`**: a player action, carrying a client sequence number and an
  opaque, size-capped payload.
  - The server validates it: the room is running, the sender is a member,
    rate and size limits hold, and the ruleset accepts it.
  - Accepted: the intent enters the next turn as a `Command` event. The event
    names the player and the client sequence number, so the sender can match
    it.
  - Refused: only the sender gets `IntentRejected` with a reason.
- **`Progress`**: the last step the client executed. It drives pacing.
- **`Checkpoint`**: per-lane digests at every checkpoint step (a room
  setting). The server compares members' digests, as described in
  [ARCHITECTURE.md](ARCHITECTURE.md) under "Authority and data flow".
  - **Deciding.** A round is decided once every member pacing the room has
    reported, or 30 seconds after the first report.
  - **Verdict.** For each lane, a strict majority wins; otherwise the anchor
    wins (the reporter on the most common platform, earliest in join order).
  - **Divergence.** A member that differs from the verdict receives
    `Diverged` with the step and the lanes. So does a member that reports
    after the decision.

Game messages that arrive when the sender is not in a running room are
ignored; an intent is answered with `IntentRejected(GameNotRunning)`. Such
messages can be in flight when a player leaves, so they are not violations.

## Resuming

A player who reconnects joins the room again with the same identity and
the number of the last turn it applied (`resume_after_turn`). The server
opens a turn stream that starts right after that turn. The new connection
replaces the old one, which is closed with `REPLACED`. A client checks that
the stream continues its log exactly (`TurnFollower::restart`).

## Slow and misbehaving clients

- **Bounded buffers.** Outbound queues are bounded per client. A client that
  cannot keep up with its turn stream is disconnected; it never makes the
  server buffer without limit or stall other players.
- **Protocol violations** close the connection with `PROTOCOL_VIOLATION`:
  - malformed or oversized frames;
  - a first message that is not `Hello`, or a second `Hello`;
  - progress beyond the sealed frontier;
  - a checkpoint with too many lanes.
- **Rate limits.** Intents are limited per player (20 per second with a
  burst of 40). Excess intents are answered with
  `IntentRejected(RateLimited)`.
- **Rejected requests** are answered with a typed error and leave the
  connection open.
