# Security and reliability

[Specification index](README.md)

## 1. Assets and trust boundaries

Protected assets:

- control authority over on-air outputs;
- stream keys, guest links, vendor tokens, TURN credentials, and certificates;
- camera/microphone/screen content;
- project media, chat, recordings, and replay;
- plugin execution and update channel;
- audit and recovery records.

Trust boundaries exist at every remote client, guest browser, network source,
media parser, native SDK, plugin, project bundle, and update package.

## 2. Secure defaults

- Bind control services to loopback until remote access is enabled.
- Use TLS for all non-loopback control and signaling.
- Pair explicitly; no default password.
- Use short-lived scoped access tokens.
- Store secrets in OS credential storage or an encrypted service vault.
- Redact secrets and private media paths from logs/support bundles.
- Disable browser input navigation to local files/internal networks by default.
- Treat media files, captions, fonts, title packages, and project bundles as
  untrusted input.
- Verify signed updates and plugins before load.

## 3. Roles and dangerous actions

Authorization is command-level and resource-aware. A graphics operator may
change assigned titles but not outputs. A replay operator may control replay
without reading stream keys.

Dangerous actions include:

- stopping all outputs;
- deleting replay/recording media;
- replacing an on-air project;
- installing/enabling native plugins;
- changing network exposure, users, or certificates;
- overwriting an existing portable bundle.

They require both permission and the configured confirmation policy. Automation
can receive narrowly scoped preauthorization; it is auditable and revocable.

## 4. Guest and browser isolation

Guest invitations are random, short-lived, single-slot credentials. Admission,
device permissions, chat, return feeds, and data-channel controls are separate
authorizations. WebRTC uses encrypted media; TURN credentials are ephemeral.

Browser inputs run in isolated profiles/processes. Their network policy can
block loopback, RFC1918, metadata-service, and file access. Downloads, popups,
camera/microphone, clipboard, and arbitrary protocol handlers are denied unless
explicitly allowed.

## 5. Plugin isolation

Plugin classes:

1. Wasm components for automation/data/control with declared filesystem,
   network, clock, and command capabilities; memory, fuel, and deadline limits.
2. Validated WGSL shader packages with bounded resources and no arbitrary native
   GPU API.
3. Native device/codec plugins behind a versioned C ABI, out of process by
   default, with platform shared-surface bridges when zero-copy is required.
4. Real-time DSP plugins in a dedicated shared-memory host with a fixed
   one-audio-block latency, preallocated rings, deadline reporting, crash bypass
   ramp, and plugin-latency compensation. Audited built-in DSP may run in
   process; third-party in-process mode is an explicit unsafe low-latency
   deployment profile, never the default.

Plugins submit commands; they cannot mutate engine state directly. Plugin state
is namespaced and versioned. A crash, timeout, protocol violation, or excessive
resource use quarantines only that plugin and activates bypass/fallback.

## 6. Project and media safety

- Project manifests and journals are checksummed and size-limited.
- Bundle extraction rejects traversal, symlink escape, device files, and zip
  bombs.
- Media probing/decoding runs with resource limits; risky parsers may run out of
  process.
- Fonts and title packages are validated before production use.
- Asset URLs have allowlists, timeouts, response-size limits, and cache policy.
- Database/data-source credentials remain secret references.

## 7. Recording durability

- Warn by projected time-to-full, not only free-space threshold.
- Write recovery metadata to a separate small journal.
- Use fault-tolerant/fragmented containers or short segments where appropriate.
- Flush indexes/metadata at a configurable interval.
- Never delete a user recording automatically.
- Replay rolling deletion follows retention policy and protects referenced
  events.
- A repair tool works without starting the full engine.

## 8. Watchdogs and recovery

Internal heartbeats monitor audio callback progress, render ticks, encoder
progress, recorder write progress, and control responsiveness. A process
supervisor restarts a dead engine only under explicit service policy; it does
not blindly restart-loop into device contention.

Recovery levels:

1. restart failed source/sink;
2. bypass failed effect/plugin;
3. rebuild graph plan;
4. recreate GPU device;
5. controlled engine restart with project journal;
6. manual intervention after a bounded attempt count.

All recovery steps emit a reason, attempt number, downtime, and outcome.

## 9. Audit and privacy

Audit events record actor, command class, target IDs, accepted revision, time,
and result. They do not store secret values or unnecessary guest content.
Retention and export are configurable. The product documents what optional
telemetry leaves the machine and keeps it disabled without consent.

## 10. Security release gate

Before public release:

- threat model updated for the release surface;
- dependency and license audit complete;
- fuzzing covers protocol, project, title/data, and media metadata parsers;
- TLS/auth/pairing and authorization integration tests pass;
- plugins and updates verify signatures;
- support bundle redaction tests pass; and
- external penetration test covers remote control, guest join, browser input,
  project bundles, and plugin installation.
