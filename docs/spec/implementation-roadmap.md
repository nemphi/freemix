# Step-by-step implementation roadmap

[Specification index](README.md)

## 1. Program expectations

Full vMix Pro/Max-class parity is a multi-year broadcast product program, not a
single MVP. A realistic broad-parity effort is roughly 12–18 experienced
engineers over 30–42 months, plus product/design, QA/hardware lab, release,
security, legal/licensing, and vendor relationships. A smaller team should keep
the same phase order but narrow certified devices and overlap less.

Phases ship vertical slices. Do not build every input before one end-to-end
camera-to-display-to-record path is reliable.

## 2. Phase 0 — feasibility and constraints (6–10 weeks)

Build disposable prototypes, not production abstractions.

1. Prove one 1080p60 camera/file frame can enter D3D12, Metal, and Vulkan wgpu
   paths, composite, display, and export to a hardware encoder.
2. Measure copy count, latency, GPU/CPU use, and device-loss behavior.
3. Prototype 48 kHz capture/mix/output on all three OSes.
4. Record and stream one hardware-encoded rendition with recoverable container.
5. Prototype engine/studio process separation and local shared preview.
6. Validate FFmpeg/GStreamer/vendor SDK redistribution and codec patent posture.
7. Acquire representative GPU, capture, audio, controller, and storage hardware.
8. Freeze the vMix 29 compatibility ledger and tag every row with acceptance
   owner and applicable platform.

Exit:

- architecture decision records for media stack, GPU interop, wire schema, UI,
  and plugin ABI;
- measured prototype report on all Tier-1 OSes;
- approved licensing plan;
- no unresolved blocker to a two-frame-class HD path.

## 3. Phase 1 — foundation and simulated engine (8–10 weeks)

1. Create workspace, dependency policy, CI, `xtask`, and binary skeletons.
2. Implement `fm-types`, IDs, formats, time/rates, color/audio metadata.
3. Implement project schema, migration harness, journal, atomic save.
4. Implement commands, events, revisions, idempotency, transactions.
5. Implement capability registry and compatibility report.
6. Materialize `parity.toml` from the ledger with phase, owning workstream,
   dependencies, platform/capability profile, and acceptance-test IDs; CI rejects
   missing or duplicate mappings.
7. Implement editable graph, validation, plan representation, bounded budgets.
8. Implement deterministic clock, fake video/audio sources/sinks, and scheduler.
9. Implement server handshake, snapshot/event resume, auth development mode.
10. Implement UI replicated model and a diagnostic client.
11. Add golden protocol/project fixtures and simulated end-to-end tests.

Exit: a headless simulated production can be controlled by CLI and client,
saved, restarted, resumed, and tested deterministically.

## 4. Phase 2 — GPU playback switcher (10–14 weeks)

1. Build wgpu context, resource pools, shader validation, render graph.
2. Add image, color/bars, and file playback through the codec adapter.
3. Implement preview/program, Cut/Fade, frame-boundary command realization.
4. Implement transforms, crop, opacity, layers, and basic color conversion.
5. Build native studio shell, input tiles, Program/Preview, transition controls.
6. Present local previews without CPU readback where possible.
7. Add planar float audio playback, Master bus, meters, gain/mute/follow-video.
8. Add fullscreen output and one fault-tolerant program recorder.
9. Instrument latency, queue depth, frame drops, and GPU time.
10. Pass the first one-hour Basic Show scenario on each Tier-1 OS.

Current implementation boundary for item 4: `fm-compositor` executes bounded
native `CompositionPlan` layers with crop, nearest scaling, quarter-turn
rotation, translation, opacity, stable z-order, premultiplied source-over, and
canonical RGBA16F inputs. Persisted daemon scene realization remains pending:
schema v3 `fm-model::Layer` has no background, geometry, opacity, or z-order,
and switcher `InputId` routing is not yet mapped to `SceneId`/`OutputId`.
Connecting those contracts requires an explicit schema migration and routing
decision rather than an implicit compatibility interpretation.

Current implementation boundary for item 5: `freemix-studio` opens a native
`eframe`/wgpu shell by default with responsive Program/Preview monitor wells,
stable-ID input tiles, realized/desired tally, permission-gated Cut/Fade
controls, bounded worker channels, optimistic Preview intent, and negotiated
client-state replication. Blocking TCP and daemon supervision remain off the
render thread. Project input names and video frames are not present in the
replicated client contract, so tiles use ordinal/ID labels and monitor wells
state that preview delivery is pending. Automatic worker reconnect and real
preview presentation remain later increments.

Current implementation boundary for item 6: `fm-frame` defines a bounded,
portable local-preview contract for shared-image versus encoded fallback,
stream/engine/adapter identity, image metadata, opaque handle references,
ready/release synchronization, monotonic frame and resize generations, exact
release acknowledgement, timeout reclamation, and client-death reclamation.
Outstanding resources and handle references cannot be reissued before release.
This is lifecycle groundwork, not preview delivery. The daemon still owns
non-shareable opaque wgpu 30 textures in a separate process, Studio currently
renders through eframe's wgpu 29 device, no authenticated native-handle/fence
IPC exists, and the engine produces no separate Preview image tap. A platform
bridge plus output color transform, bounded shared ring, sidecar IPC, and
encoded fallback are required before item 6 can claim presentation without CPU
readback.

Current implementation boundary for item 7: `fm-codec-ffmpeg` provides a
transactional sequential local-audio cursor with global block sequence,
sample-contiguous timing, sticky EOS, and explicit per-page operation
block/sample/byte limits. Cursor progress no longer spends prior pages against
those limits. `fm-audio` provides a deterministic reference Master
with planar F32 identity mapping, gain/mute/follow-video, meters, timed canonical
blocks, sample-count timing validation, and transactional gain ramps.
`fm-clock::ClockMapping` now also exposes checked signed source-to-Master and
inverse Master-to-source nanosecond mapping with explicit floor rounding before
anchors and overflow rejection. This is arithmetic groundwork for live audio
synchronization; no estimator filtering, sample interpolation, FIFO, or drift
resampler is connected yet. Opt-in
native daemon mode maintains bounded CPU audio rings on a decode worker,
allocates Master intervals directly from absolute engine frame numbers, follows
the authoritative `ProgramFrame.primary`, and writes a bounded fake sink. During
a Fade it intentionally keeps the old primary until completion, then hard
switches. Local audio must exactly match the project sample rate/layout and its
first timestamp must align with the selected video's first timestamp; no
resampling or implicit mapping is performed. Missing audio, stills, and
configured simulated silence produce silence, while unsupported simulated sine
audio is rejected. This remains a diagnostic/reference path: it allocates while
mixing, waits for all preflighted sources, and has no OS audio device,
bus/output routing, persisted strip controls, transition crossfade, drift
correction, or externally delivered audio. Later FFmpeg pages still rescan and
trim from the beginning, so deep playback becomes progressively more expensive
and can fail transactionally at fixed metadata-output or subprocess-timeout
bounds. Item 7 and the related parity rows therefore remain incomplete.

Current implementation boundary for item 8: `fm-gpu` provides a portable,
bounded latest-frame presentation policy plus opaque, context-bound native
surface configuration, acquisition, submission, presentation dispatch, and
same-context recreation. Configure/acquire/present validation is captured
without exposing native handles; a failed configure poisons that surface instead
of retaining stale state. `fm-color::NativeSdrOutputTransform` writes canonical
premultiplied linear BT.2020 RGBA16F directly into non-sRGB BGRA8/RGBA8 surface
targets as opaque, hard-clipped sRGB Rec.709 with black aspect-fit bars and no
CPU readback. CPU oracles and explicitly invoked ignored Metal tests cover
Rec.709, Display-P3, and BT.2020 source primaries across sRGB, BT.709, and
BT.1886 transfer functions, over-range and fractional-alpha pixels, and
two-dimensional fitting. With the
opt-in `macos-program-surface` feature and `--fullscreen-program`, `freemixd`
owns one borderless macOS Program window on its main-thread winit loop while one
worker retains all media and GPU submission. `--fullscreen-display` selects a
zero-based deterministic monitor entry. The selected winit monitor identity is
retained across inventory changes; removal suspends and hides output, reconnect
retargets it, resize updates coalesce, and surface loss has one
generation-correlated same-context recreation path. An ignored generated-bars
subprocess test requires at least one validated queue-presentation dispatch.
This remains a single macOS/Metal diagnostic output, not display certification:
the display choice is not persisted, physical scanout and visible pixels are not
observed, real unplug/replug and induced surface loss are policy-tested rather
than hardware-induced, and no second output, HDR, exclusive mode, or
cross-platform surface adapter exists. `fm-record` provides a private
CRC-protected packet spool with bounded queues and records, durable action
receipts and directory publication, segmentation, corruption isolation,
validate-before-repair torn-tail recovery, and a bounded subprocess-kill test.
It is not the playable recorder. `fm-codec-ffmpeg` owns a separate bounded
startup-only recorder that pairs tight SDR RGBA8 Program readback with the exact
planar F32 Master block, sends both over authenticated loopback HTTP inputs to
FFmpeg, and writes fragmented MP4 with H.264/libx264 and AAC into a caller-owned
exclusive empty file. Queue admission, retained bytes, connection/no-progress,
stop/kill, child reaping, output flush/sync, and sticky failures are bounded. A
paced pre-readiness barrier requires both a completed raw pair and observed mux
output; `FREEMIXD_RECORDER` reports frozen pair, output-byte, finalization,
cleanup, and failure state. `--record-program` enables this path only with
`--native-media`; Unix signals and Windows Ctrl-C stop acceptance, settle any
remaining Fade control state without rendering shutdown-only frames, checkpoint,
and finalize without publishing false runtime-realized events. Separate
required-tool macOS integrations start recording from an unaligned restored
frame and decode the resulting H.264/AAC file, while a generator integration
covers signal shutdown during a 3,600-frame Fade. This remains a diagnostic
recorder: Program capture uses
synchronous GPU-to-CPU readback, recording cannot be started or stopped through
the protocol, encoding is software-only and fixed-format, parent path traversal
is not file-capability based, and there is no zero-copy bridge, hardware encoder,
codec/segment policy, disk-space status, second recorder, or cross-platform
native evidence. Multi-file `fm-record` repair is necessarily sequential after
validation and assumes exclusive ownership of each recorder directory. Item 8
and its parity rows therefore remain incomplete.

Current implementation boundary for item 9: `fm-gpu` presentation telemetry
now reports current and peak occupancy for its one-slot latest-frame queue in
addition to saturating accepted, presented, replaced, dropped, acquisition, and
recovery counters. Native contexts request `TIMESTAMP_QUERY` only when the
adapter advertises it and use a four-slot nonblocking readback ring to measure
real fullscreen render-pass duration. Unsupported adapters continue without
profiling; ring pressure or unavailable timestamp results increment saturating
counters instead of blocking or failing rendering. `fm-observability` exposes a
distinct bounded `gpu_time_ms` histogram rather than mislabeling CPU wall time or
GPU utilization. Opt-in native daemon mode aggregates up to 256 host-deadline
lateness and GPU-pass samples plus sampled current and observed-peak audio
retention, exact fake-sink and presentation high watermarks/drops, and sampled
recorder outstanding-pair/retained-byte pressure. Cooperative daemon teardown
emits one idempotent final `FREEMIXD_TELEMETRY` record with lifetime and retained
sample populations, p50/p95/p99 values, capability state, pending/lost samples,
and metric errors; native Metal and daemon tests require actual GPU samples where
supported. A forced fullscreen worker timeout cannot guarantee that final worker
diagnostic before process exit. Host lateness means time past the
native frame deadline immediately before realization, not capture or
glass-to-glass latency. GPU duration excludes queue wait, presentation,
compositor feedback, and physical scanout. There is no live telemetry protocol,
component-dimensioned metric registry, alerting/UI, CPU/GPU utilization, disk or
network instrumentation, cross-platform native evidence, or QR/audio-loop
latency fixture. Item 9 therefore remains incomplete and the broader RC-015
parity row remains planned.

Current implementation boundary for item 10: headless native mode accepts a
bounded `--diagnostic-stop-after` duration after readiness and exits through the
normal cooperative checkpoint and telemetry path. The ignored
`phase2_native_diagnostic_soak` integration creates two small generated video
inputs with silent audio, alternates Cut and four-frame Fade commands, validates
accepted command targets plus durable and runtime events, and then checks
bounded telemetry, persisted receipts/routing/clock progress, and continued
rendering after restart. Its default duration is 60 seconds; environment flags
can require native startup and GPU timestamps, and its report is explicitly
classified `diagnostic-not-certification` with `basic_show_complete=false`. A
required local 60-second macOS/Metal run rendered all 1,500 requested frames,
completed 59 commands, reported 1,333 lifetime GPU timing samples, resumed to
1,575 total frames after a separate three-second restart, and exposed 1,492
bounded fake audio-sink drops rather than hiding them. This is useful lifecycle
and pressure evidence, not the universal Basic Show scenario. That scenario
requires four cameras, two clips, a title, a browser source, a two-box scene,
streaming, and recording for one hour on every Tier-1 OS. Cameras and persisted
scenes are scheduled for Phase 3, streaming for Phase 4, and title/browser inputs
for Phase 5; no Windows/DX12 or Linux/Vulkan one-hour evidence exists. Item 10 is
therefore incomplete and is structurally blocked by capabilities scheduled
after Phase 2.

Exit: a useful offline/file-based switcher, with no engine dependency on UI
lifecycle.

## 5. Phase 3 — live capture and professional core (12–16 weeks)

1. Add OS camera, audio, and screen/window capture adapters.
2. Implement source hot-plug, signal loss/fallback, timestamp validation.
3. Implement audio device clocks, drift correction, channel mapping, delay,
   native EQ/gate/compressor/limiter.
4. Add scenes with background plus ten layers, keys, masks, and effect stack.
5. Complete standard transitions, AlphaFade, T-bar, FTB.
6. Add eight overlay channels, stinger preload/cut point, per-output inclusion.
7. Add two multiviews, labels, tally, audio meters, safe areas.
8. Add scopes and professional color correction.
9. Add shortcuts and initial MIDI/OSC/controller support.
10. Certify Profile A without network contribution.

Current implementation boundary for item 1: `fm-io-macos` is the first native
platform leaf. An isolated Swift helper uses `AVFoundation` to enumerate camera
video and microphone devices without prompting, report their independent
authorization states, and expose exact
normalized rational-rate formats, and capture selected CPU-backed BGRA frames
with original `CoreMedia` timestamps. Rust communicates through bounded binary discovery and
stream protocols, derives collision-checked stable device/source IDs, implements
the `fm-io-api` discovery and source lifecycle contracts, and never substitutes
a different device. Discovery is capped at 64 devices, 256 formats per device,
256 KiB total metadata, 3840x2160 at 60 fps, and 4 KiB identifier/name fields.
Capture records are capped at 64 MiB and feed a negotiated queue of at most
eight frames with drop-oldest accounting; native `AVFoundation` drops are
reported separately. The v3 capture protocol requires source-derived color
primaries and transfer attachments on every delivered CoreVideo buffer. It
accepts Rec.709, Display-P3 D65, or BT.2020 primaries paired with sRGB or BT.709
transfer, derives identity matrix and full-range RGB semantics from the
negotiated BGRA representation, verifies opaque active pixels, and rejects
missing, malformed, unknown, or unsupported metadata instead of substituting
defaults. Startup waits for a running-session
marker, stop/reap and worker cleanup have explicit deadlines and retained ownership, and helper
interruptions, disconnects, malformed frames, no-frame deadlines, Hold/Stop
fallback, and recovery are observable. Capture magic is emitted only after the
session is running and before the delegate is attached; Rust revalidates the
negotiated dimensions, positive CoreMedia timescales, and the 60 fps bound.
Swift compilation targets the Cargo
architecture with a macOS 13 deployment minimum. Applications can supply a
packaged helper path; the embedded Cargo `OUT_DIR` path is developer-only.
`freemix-capture-node cameras` now selects this adapter and emits bounded,
versioned, one-line records with stable IDs, adapter-qualified project
`stable_key` values, permission state, format counts, and percent-escaped names.
The v2 key is bounded, derived from the collision-checked source ID, and supports
exact adapter lookup without display-name or discovery-order substitution. The
command never prompts unless `--request-permission` is
present when using the built-in helper; `--helper` trusts an
application-packaged executable. `camera-smoke` selects one exact snapshot source
and advertised format, acquires 1 to 300 frames under a 1 to 60 second acquisition
deadline, reports source timestamps plus native/queue drops, and always attempts
bounded stop/close cleanup. It never requests permission. A hermetic helper test
acquires two framed samples, verifies telemetry and helper-process disappearance,
and proves `prompt-required` preflight never invokes capture. The v2 discovery
protocol carries normalized frame-rate numerators and denominators. The helper
advertises supported candidate rates through 60 fps: every integer rate plus
24000/1001, 30000/1001, and 60000/1001, with common rates prioritized within
the existing 256-format bound. Selection and `CoreMedia` frame duration use the
exact rational value rather than floating-point or nearest-rate substitution.

The same helper now has separate `discover-audio`, `request-audio-permission`,
and `capture-audio` commands, preserving the camera D2/F3 protocol. `FMAUDD1`
discovery reports independently identified microphone endpoints and exact native
formats up to 192 kHz, accepting mono or a two-channel format only when Core
Audio explicitly labels it stereo. `FMAUDF1` carries bounded interleaved
little-endian F32 records with original CoreMedia PTS, sample rate, channel
count, sample count, and independent sequence telemetry. Rust validates all
length arithmetic before allocating, limits each block to 16,384 samples and
128 KiB, rejects non-finite samples, derives duration from the exact sample
span, and deinterleaves into planar `AudioBlock`s. Each microphone has a
collision-checked `macos.avfoundation.audio.v1` stable key and source-specific
clock domain; no equivalence to a camera or Master clock is claimed.
`MacosAudioSource` implements the standard lifecycle with a queue of at most 32
blocks. Queue overflow is sticky failure rather than silent sample loss, and
stop, recovery, drop, worker, and helper ownership are bounded. The capture node
adds `audio-inputs` and `audio-smoke`; discovery prompts only with explicit
`--request-permission`, while smoke requires the reported stable key plus exact
sample rate/channel count and never substitutes a newly indexed or default
device. Its v1 smoke diagnostic reports timing, samples, signal peak, queue
pressure, and explicitly marks native drop measurement unavailable. Hermetic
protocol and process tests prove PCM layout, finite-sample rejection, exact
arguments, no permission invocation, and helper reaping. Non-prompting local
discovery found two audio inputs with four and one bounded formats respectively
and `prompt-required` microphone permission. The developer helper embeds camera
and microphone usage descriptions, but remains an unsigned Cargo `OUT_DIR`
artifact rather than application packaging.

`freemixd --native-media` now realizes macOS `Device` inputs by exact persisted
`stable_key`; `--camera-helper` supplies an application-packaged helper path. It
discovers once without prompting, requires already-granted permission, rejects
duplicate or unknown keys, and opens only an advertised BGRA mode matching the
project dimensions and exact rational frame rate. Camera helpers start
concurrently, all sources share one three-second initial-frame deadline, and each adapter queue
is bounded to one frame with Stop fallback. The native runtime seeds and updates
a dedicated live-video lane that retains exactly one GPU frame, preserves the
original `CoreMedia` timing and clock, accepts monotonic sequence gaps caused by
bounded drops, rejects clock/PTS regressions, and never schedules FFmpeg refill.
Normal and failed startup paths retain camera ownership through bounded stop,
close, worker, and child cleanup. Hermetic tests prove exact helper arguments,
initial timing, unknown-key non-substitution, no permission invocation, and child
reaping. A required hermetic daemon process run binds a fake camera by persisted
key at 30000/1001, starts with Rec.709/sRGB and then cycles the six supported
camera primary/transfer combinations, continuously ingests BGRA frames through
Metal, renders and checkpoints for one second, and confirms helper-process
disappearance. A readiness barrier ensures the startup frame is ingested before
updates. Protocol tests decode all six combinations, capture-node process tests
exercise Display-P3/sRGB and BT.2020/BT.709 records, current schema-v3
persistence round-trips Display-P3/BT.709, and native Metal readback tests
compare all three primaries across sRGB, BT.709, and BT.1886 against CPU oracles.
The daemon run also exercises the validated BGRA-to-RGBA swizzle while preserving
source timing. `FREEMIXD_TELEMETRY` v3
adds bounded daemon-wide camera aggregates for configured sources,
adapter-received frames, successful live GPU ingests, native `AVFoundation`
drops, and current/peak/dropped Rust queue frames.
The required helper run reports one configured source, all 12 received frames,
two synthetic native drops, zero final queue depth, a one-frame queue peak, and
accounts for every received frame as either ingested or queue-dropped.
`FREEMIXD_CAMERA_SOURCE` v1 emits one fixed-shape local diagnostic per camera in
ascending project input-ID order before cleanup, with sampled lifecycle, health,
received/ingested frames, native drops, and queue depth/peak/drops. The aggregate
is folded from the same snapshots once native runtime telemetry exists; camera
startup failures before runtime construction still emit source diagnostics with
zero successful GPU ingests. These diagnostics omit hardware identifiers, names,
stable keys, paths, health details, clocks, and media, and remain
diagnostic evidence rather than a network telemetry subscription.

Portable protocol/lifecycle and capture-node process tests pass; required local
D2 discovery enumerated two real macOS camera endpoints with 252 and 126 bounded
candidate modes and `prompt-required` permission, and the real smoke command
stopped at permission preflight without prompting. No real camera
frame-acquisition or daemon hardware evidence therefore
exists; the passing daemon run is hermetic rather than device certification. The
slice also omits paired/synchronized camera audio, signed helper packaging, source-clock scheduling
and drift correction, live per-source telemetry subscription, HDR transfers,
ICC-only profiles,
gamma-tag fallback, configurable unknown-metadata policy, certified
hot-plug/recovery loops, Windows/Linux adapters, audio-device daemon/Master
realization, and screen/window/application-audio capture. Item 1 and `IN-001`, `IN-005`, and
`IN-011` therefore remain incomplete and planned.

Exit: `P0` switcher, composition, audio, display, record, and control rows pass.

## 6. Phase 4 — output, stream, and headless reliability (10–14 weeks)

1. Add RTMP/RTMPS and SRT outputs with independent state.
2. Add five simultaneous destinations and shared-rendition planning.
3. Add multi-bitrate output and local HLS/LiveLAN equivalent.
4. Add second program recorder, segment rules, WAV/multichannel audio.
5. Add ISO/MultiCorder graph taps and timecode manifest.
6. Implement the per-user `freemix-capture-node` and certify service-to-session
   screen/camera/application-audio permission, clock, reconnect, and logout
   behavior.
7. Implement engine OS service mode, health/readiness, support bundles.
8. Implement remote browser control, WebRTC multiview preview, roles/pairing.
9. Add disk/network/GPU preflight and alert policy.
10. Run process-kill, disk-full, network impairment, and 24-hour soak.
11. Publish the first supported hardware/capability matrix.

Exit: a remote-controlled headless production can stream and record
independently with tested recovery.

## 7. Phase 5 — graphics, data, automation, and ecosystem controls (12–16 weeks)

1. Implement title scene model, text shaping, images/shapes, animation/tickers.
2. Build title designer and live field editor.
3. Add isolated browser input with audio, interaction, custom CSS, and network
   policy.
4. Add data sources and typed field mapping.
5. Implement PlayList scheduling and per-input programmed GO.
6. Implement the complete trigger catalog, macro transactions, timers,
   conditions, cancellation.
7. Add Wasm automation/data plugins with capabilities and limits.
8. Complete controller learn/templates, dynamic values, activators, tally.
9. Add PTZ protocols, presets as virtual inputs, mouse/joystick control.
10. Add telestrator and web monitor.
11. Add project append, bundle, relink, import/export, templates, undo/lock.

Exit: graphics/data/automation/operator ecosystem `P1` rows pass.

## 8. Phase 6 — guests and collaboration (10–14 weeks)

1. Build guest signaling, lobby, invitations, device preflight, reconnect.
2. Implement up to eight WebRTC guest inputs.
3. Implement return video selection, mix-minus, talkback, and latency health.
4. Add chat and guest manager roles.
5. Add remote desktop capture node with tally and optional audio.
6. Harden NAT/TURN, bandwidth adaptation, echo, and permission UX.
7. Add optional authorized Zoom adapter as a separate licensed workstream.
8. Run eight-guest network impairment and long-call tests.

Exit: Remote Show scenario passes; named proprietary rows remain explicitly
pending until licensed adapters pass.

## 9. Phase 7 — replay and sports (14–20 weeks)

1. Implement rolling segment storage and per-camera storage roots.
2. Synchronize and record up to eight sources with four-channel audio.
3. Add event database, marks, last-N presets, tags, notes, lists/folders.
4. Add Replay A/B, live/recorded modes, angle selection, linked channels.
5. Add jog/shuttle, variable speed, transitions, auto-return, audio policy.
6. Add replay multiview and four-angle quad view.
7. Add highlights, background music, export during record.
8. Add replay shortcuts, activators, and dual-controller profiles.
9. Validate storage prediction, retention, low-space behavior, and recovery.
10. Certify Profile C.

Exit: Sports Show scenario passes on certified storage/hardware.

## 10. Phase 8 — professional hardware and advanced formats (ongoing, 16–24 weeks)

Parallel adapters, each with its own certification:

1. Blackmagic capture/output, embedded audio, key/fill, reference.
2. AJA capture/output, embedded audio, key/fill, reference.
3. Bluefish444 and prioritized capture families.
4. NDI and OMT send/receive, alpha, tally/metadata, direct record where possible.
5. Virtual camera/audio devices with signing/install/uninstall.
6. Genlock/PTP and timecode workflows.
7. Interlaced output and advanced deinterlace.
8. 10-bit HDR ingest, compose, scopes, record, stream, display.
9. VST3 host and optional AU/LV2 compatibility.
10. UHD Profile B certification and multi-GPU policy if benchmarks justify it.

Exit: each adapter ships only with a platform-specific conformance report.

## 11. Phase 9 — full parity and compatibility (12–20 weeks)

1. Close remaining `P2` rows: presentation/DVD/import/legacy surfaces according
   to legal platform scope.
2. Add vMix HTTP/TCP/tally compatibility adapter.
3. Complete virtual sets, advanced title import, social moderation adapters.
4. Complete localization and accessibility audit.
5. Exercise every universal acceptance scenario on each applicable Tier-1 OS.
6. Run 72-hour release soak and disaster-recovery drills.
7. External security, broadcast-operator, and hardware compatibility review.
8. Freeze 1.0 project/plugin/protocol compatibility policy.

Exit: no unclassified vMix 29 public feature and no incomplete P0/P1/P2 row
without a documented legal/platform exception.

## 12. Workstream ownership

Suggested durable teams:

- media core and clocks;
- GPU/compositor/color;
- audio/DSP;
- codecs/network/recording;
- platform and professional hardware;
- engine/control/persistence/security;
- studio/web UX;
- graphics/data/automation/plugins;
- replay/guests;
- QA automation and hardware lab.

Crate ownership follows these boundaries. Cross-team APIs are reviewed through
small executable conformance tests, not a giant shared facade.

## 13. First backlog, in order

1. Create capability ledger machine-readable schema.
2. Write ADTs for frame rate, timebase, color, channel layout, memory domain.
3. Write command/event/revision state-machine tests.
4. Write simulated clock and frame generator.
5. Write graph validation and bounded resource planner.
6. Build one platform camera/file adapter behind `fm-io-api`.
7. Build one wgpu composite-to-display path.
8. Build one audio capture-to-output path.
9. Connect Cut/Fade command through server to a scheduled frame.
10. Save/restart/recover the project.
11. Add one program recorder and process-kill recovery test.
12. Port the vertical slice to the other two Tier-1 OSes before adding breadth.

## 14. Scope control

At each phase:

- update the parity ledger;
- publish measured capability support;
- carry no “temporary” unbounded queues or UI-owned engine state;
- retire prototypes before stabilizing public APIs;
- require a fallback/failure policy for every new source and sink; and
- prioritize a reliable vertical path over a long list of half-integrated
  adapters.

## 15. Machine-checkable traceability

`feature-parity.md` is the reader-facing contract. Before Phase 1 closes,
`parity.toml` becomes the executable ledger. Each `IN-*`, `SW-*`, `GX-*`,
`AU-*`, `OR-*`, and `RC-*` ID must define:

```toml
id = "IN-013"
priority = "P1"
phase = 5
owner = "graphics-data-automation"
dependencies = ["gpu.browser-surface", "security.browser-sandbox"]
capability_profiles = ["windows-tier1", "macos-tier1", "linux-tier1"]
tests = ["browser-input-html5-audio", "browser-input-css", "browser-input-crash"]
```

`xtask parity check` fails when a Markdown ID lacks a record, a test ID is
unknown, a phase exit claims an incomplete dependency, or a release includes an
unwaived applicable row. Named proprietary integrations may be legally blocked
but cannot be marked parity-complete through a generic substitute.
