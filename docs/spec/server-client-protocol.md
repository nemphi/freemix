# Server/client protocol

[Specification index](README.md)

## 1. Transport surface

| Purpose | Transport |
|---|---|
| Discovery | mDNS/DNS-SD locally; explicit URL and optional rendezvous remotely |
| Capabilities, projects, assets, administration | HTTPS REST |
| Commands, results, ordered events, telemetry | Authenticated WebSocket |
| Remote previews and guest media | WebRTC |
| Local native previews | Shared GPU/image handles plus authenticated local IPC when supported |
| Metrics/health | HTTPS; OpenTelemetry/Prometheus adapters where configured |

The semantic protocol is independent of transport. A future QUIC transport may
reuse the same messages. Full-resolution production frames are never serialized
into control messages.

## 2. Connection sequence

1. Client resolves the engine and validates TLS identity.
2. Client sends the current protocol identifier, build, client type, desired role, and cached
   cursor `(engine_id, state_epoch, log_id, revision)`.
3. Server authenticates, requires the exact current protocol, and returns permissions,
   capabilities digest, engine identity, and current revision.
4. Server sends missing ordered events or a fresh snapshot.
5. Client builds its read model and subscribes to selected telemetry.
6. Client requests only visible preview renditions.
7. Heartbeats carry last applied revision and clock sample. After the server
   validates a heartbeat, it returns the session server identity, the exact
   client heartbeat sequence, and the server receive time.
8. Reconnect resumes only when all cursor identity fields match; project
   replacement, restore, log compaction, or engine identity change forces a
   snapshot and a new cursor.

The current Protocol 2.15 implementation uses bounded newline-delimited raw TCP.
Studio keeps one expected heartbeat sequence and waits for its matching
acknowledgement within the bounded peer wait. EOF, timeout, wrong server
identity, or wrong sequence enters the existing reconnect backoff. The server
does not acknowledge an invalid heartbeat. This exchange gives bidirectional
control-plane peer liveness only. It is not service readiness, production
authentication, media health, or the planned HTTP/WebSocket transport.

Snapshots carry bounded project-order input and output catalogs as exact ID/name pairs.
The canonical persisted name labels input tiles and mixer strips; clients do not
invent ordinal display names. The `input_renamed` and `input_order_changed`
events carry durable exact-current name and order edits. Add/remove catalog edits
still require a fresh snapshot.

## 3. Command semantics

Conceptual envelope:

```json
{
  "protocol": 1,
  "id": "01K...",
  "idempotency_key": "operator-7:01K...",
  "expected_revision": 1842,
  "deadline_ms": 500,
  "payload": {
    "type": "transition",
    "mix_id": "019...",
    "effect": "fade",
    "duration_ms": 300
  }
}
```

The result is accepted with a revision and optional scheduled media timestamp,
or rejected with a stable error code, field details, current revision, and
retryability.

Rules:

- Duplicate idempotency keys return the original result.
- `expected_revision` protects editor-style updates; immediate live commands
  may omit it when their semantics are commutative or last-intent-wins.
- Transactions either commit all durable state or none.
- Frame-boundary commands acknowledge state acceptance separately from runtime
  realization. Later lifecycle events report `preparing`, `scheduled`,
  per-domain `realized`, `failed`, or `superseded` with runtime generation.
- High-rate fader/PTZ updates use a coalescible stream with a final committed
  value. Intermediate intents do not advance durable revision; the final
  commit does.
- Command permissions are checked before validation reveals sensitive details.
- The current input-audio mutation replaces one complete Master strip
  atomically: gain in `-96000..=24000` milli-dB, stereo balance in
  `-10000..=10000` basis points, mute, follow-video, and delay in
  `0..=48000` samples. Negative balance attenuates the right destination,
  positive balance attenuates the left destination, and non-stereo destination
  labels remain at unity. Snapshots and durable events always carry one complete
  strip for every show input; partial audio-strip patches are not accepted.

Command results are ordered before any event bearing the same accepted revision
on that connection. State events are globally ordered within one
`(state_epoch, log_id)`. Runtime realization events reference a durable revision
but use their own per-generation sequence because independent clock domains may
complete at different times.

Declarative state mutations and edge-triggered actions have different recovery
semantics. The server persists idempotency results for at least the project
recovery window and never replays an irreversible action after restart. A client
whose result was lost queries the idempotency key and receives the original
receipt and latest realization outcome.

Diagnostics are a read-only query, separate from the durable command envelope. A
`diagnostics_request` carries only `protocol` and non-blank `request_id`; it is not
a command, does not mutate state, and has no idempotency or revision precondition.
The matching `diagnostics_response` carries request ID, engine identity,
`current_revision`, optional retained revision bounds, and subscriber limits and
counts. Exact fields are `oldest_retained_revision`, `newest_retained_revision`,
`subscriber_count`, `retained_events_limit`, `subscriber_limit`, and
`subscriber_queue_limit`. These fields do not claim readiness, health,
capabilities, authentication state, session counters, paths, or error details.

## 4. Events and telemetry

Durable ordered events include project edits, routing, switcher state, overlay
state, output desired state, shortcut changes, and authorization-relevant
operations.

Ephemeral streams include:

- audio meters;
- clocks and playheads;
- preview statistics/thumbnails;
- input/output health;
- GPU/CPU/disk/network metrics;
- pointer/hover previews and telestrator cursors.

Protocol 2.15 defines `audio_meters` as a lossy, non-resumable record with
server identity, an independent sequence, one native Program frame/sample
interval, fixed-point linear Master levels, and fixed-point per-input levels in
strict `InputId` order. Levels use millionths of linear full scale and can
exceed unity. The codec validates only that the sample interval is positive;
the native timeline owns frame/sample coherence. The record does not advance or
carry a durable revision.

Each telemetry class has requested cadence, aggregation, and maximum bandwidth.
Slow clients receive coalesced latest data, never unbounded queues.

## 5. Versioning

- Protocol uses one explicit current-development major/minor identifier.
- Any wire change may replace that identifier and contract in place.
- Client and server accept only the exact current identifier; there is no
  downgrade negotiation or upgrade window during current development.
- Unknown fields, events, and message kinds are rejected.
- Tests generate current records through the current encoder, round-trip them,
  and mutate them to cover malformed input. No historical wire fixtures or
  cross-version matrix is retained.
- Domain models do not derive their wire schema directly.

## 6. Authentication and authorization

Local first run uses a one-time pairing secret exchanged through protected local
IPC. Remote pairing displays a short-lived code on an already trusted client or
engine console. Production deployments may use local accounts, OIDC, or mTLS.

Suggested roles:

- `viewer`: health, tally, permitted previews;
- `graphics`: title/data changes and assigned overlays;
- `audio`: mixer/bus controls;
- `replay`: replay record/events/playback;
- `operator`: switcher and show controls;
- `admin`: projects, devices, users, secrets, plugins, updates.

Tokens are short-lived and scoped. Streaming keys, TURN credentials, and vendor
tokens are returned only as redacted references.

## 7. Preview protocol

Remote previews use WebRTC simulcast/SVC where available:

- multiview-first to limit decoder count;
- optional individual input subscriptions for focused inspection;
- operator-configured maximum latency/quality;
- labels and tally may be composited server-side or sent as metadata;
- preview congestion never backpressures Program.

Local studio uses shared textures only after same-user, engine-generation, GPU
adapter, and handle-duplication validation. Each frame lease transfers a ready
fence, sequence and resize generation and requires release acknowledgement; the
engine reclaims outstanding leases on client death. It falls back to a loopback
low-latency encoded preview on any incompatibility.

## 8. Compatibility API

An optional `fm-vmix-compat` service maps the documented vMix HTTP function and
persistent TCP/tally model to FreeMix commands/events. It:

- resolves input ordinals/names to stable IDs at command time;
- exposes the complete documented vMix 29 XML state and HTTP function surface;
- maps tally/activator subscriptions;
- never weakens authentication by default; and
- passes exact-current contract coverage for the persistent TCP command/response
  and subscription semantics.

This adapter is isolated from the native protocol and is not the basis of new
features. FreeMix-only capabilities use a separate namespace and do not alter
the compatibility surface.

## 9. SDKs

Generate supported clients from the transport schema, then wrap them with
handwritten domain-friendly APIs:

- Rust client for studio, CLI, plugins, and integrators;
- TypeScript client for browser integrations even though the shipped web app is
  Rust/Wasm;
- small C ABI for hardware controllers;
- OpenAPI and message-schema documentation.

All examples connect through the public API. There is no privileged internal
mutation path for the studio UI.
