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

Current Phase 1 control boundary: the daemon, protocol, and client exercise a
versioned raw TCP session with handshake, snapshot/resume, commands, events,
development authentication, and deterministic model tests. This is useful
partial evidence for the control plane, but it does not implement or verify the
RC-008 HTTP/WebSocket API contract, including HTTP resources, WebSocket event
subscriptions, or transport-level production authentication and rate limits.
RC-008 therefore remains planned.

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
canonical RGBA16F inputs. It also executes hard-edged rectangular masks in
half-open post-crop source space, with exact CPU-oracle coverage and ignored
native Metal readback coverage that remains opt-in and is not claimed without
an adapter. Schema v7 persists each scene background and each layer's explicit
input/scene source, geometry, crop, optional hard-edged rectangular mask,
opacity, and z-order; rectangular masks were introduced in schema v6. Mask
bounds are validated against the effective
post-crop source dimensions before native planning; inversion does not change
planner capacity or transient-resource accounting. The explicit v5-to-v6
migration defaults existing layers to no mask while preserving exact desired
and realized manual-transition state.
`InputKind::Scene` explicitly routes a `SceneId` plus an optional audio
`InputId`, while every persisted output explicitly routes a video `SceneId` and
audio `BusId`. The explicit v3-to-v4 migration supplies opaque-black,
canvas-identity, no-crop, full-opacity, and zero-z-order defaults while
preserving legacy sources and layer counts; outputs are preserved as declared
and are not inferred. Native `freemixd` now consumes this visual model,
including the persisted rectangular mask, through the bounded scene planner
described under Phase 3 item 4 instead of rejecting scene inputs. Feathered or
non-rectangular masks, keys, effects, per-output realization, live
edits/replanning, and cross-platform or hardware certification remain outside
this item, so its parity rows remain planned.

Current implementation boundary for item 5: `freemix-studio` opens a native
`eframe`/wgpu shell by default with responsive Program/Preview monitor wells,
stable-ID input tiles, realized/desired tally, and permission-gated Cut/Fade/Wipe
controls. Studio advertises protocol 1.4; Wipe additionally requires protocol
1.3, while manual Fade/Wipe T-bar controls require protocol 1.4, transition
permission, synchronized state, and Ready lifecycle. The manual panel displays
replicated desired and realized kind, routing, and exact integer basis-point
position. Bounded worker channels preserve strict operator FIFO while
recovering: unresolved or deferred versioned commands cannot cross a protocol
downgrade, and later supported intents cannot overtake them. `fm-client` retains a
bounded terminal command history (256 by default, configurable through 65,536),
while replay-receipt collisions mark affected sent commands terminal-uncertain,
force authoritative snapshot resynchronization, and remain visibly sticky in
Studio's bounded eight-entry ledger until Studio is restarted.

Blocking TCP and daemon supervision remain off the render thread. The worker
reconnects after bounded backoff, negotiates durable resume or an authoritative
snapshot, resumes unresolved command sequences, and requests a snapshot when
runtime realization becomes uncertain. TCP establishment is finite, using one
attempt of up to the configured timeout, but cancellation is checked only
before and after that attempt. Supervisor readiness and connected protocol
read, write, and flush waits are polled and cancellable; deferred intents are
capped, and supervised daemon shutdown or restart performs bounded
process-group/job and descendant cleanup. Project input names and video frames
are not present in the replicated client contract, so tiles use ordinal/ID
labels and monitor wells state that real preview delivery remains pending.
`freemix-web` likewise declares protocol 1.4 support in its client configuration.
Its transport-free semantic presentation model preserves the existing
permission- and protocol-gated Cut/Fade/Wipe controls and adds manual Fade/Wipe
Start, exact basis-point Position, Commit, and Cancel controls. Manual controls
are hidden below protocol 1.4 and otherwise derive availability from Ready
state, transition permission, and separate authoritative desired and realized
manual projections. Web still has no browser renderer or network runtime, and
the remaining transition families, FTB, and protocol-driven
native-media/hardware T-bar evidence keep item 5 and RC-007 planned.

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
those limits, and bounded metadata-only positioning can skip complete blocks to
a restored sample without decoding their PCM prefix. The sequential cursor now
keeps a source-fingerprint- and stream-bound metadata suffix with cumulative
sample offsets plus periodic exact checkpoints. Each extension asks ffprobe to
resume at an absolute checkpoint PTS, accepts it only after the checkpoint and
retained overlap reproduce exactly, and uses a packet budget bounded by the
checkpoint interval, page size, and fixed preroll slack. Retained records,
estimated bytes, checkpoints, and resume attempts have explicit limits and
expose probe, packet-budget, reuse, recomputation, eviction, and invalidation
telemetry. This makes metadata discovery amortized O(total blocks) on demuxers
that honor a reproducible interval seek. FFprobe does not guarantee that for
every container: when none of the bounded retained checkpoints can be
reproduced, the operation returns incomplete metadata transactionally rather
than restarting an ordinal-sized prefix scan. PCM decoding separately uses a
bounded from-start correction or a fixed validated seek-anchor stack; source
changes and process failures leave both cursor and index content coherent.
Native restore still allows at most 4,096 skipped blocks, and fixed
metadata-output/decode bounds plus subprocess timeouts can reject deep playback
or restore transactionally.

`fm-audio` provides a deterministic reference Master with planar F32 mapping,
gain/mute/follow-video, meters, timed canonical blocks, sample-count timing
validation, and transactional gain ramps. Its bounded
`ClockMappedAudioSynchronizer` is now connected to native `freemixd` local-file
audio. The daemon accepts source rates such as 44.1 kHz and linearly resamples
them to the 48 kHz project Master while preserving absolute source and Master
cadence origins. Master intervals derive directly from absolute engine frame
numbers, including deep restored cursors and fractional video rates. Initial
positioning compares audio and video on their relative media timeline: early
audio is trimmed and delayed audio produces bounded leading silence. Every
decoded source, including an inactive one, advances on every Master interval so
later switching does not replay stale audio.

Schema v7 also persists one exact per-input audio strip as bounded integer
milli-dB gain (`-96000..=24000`), mute, and follow-video. The explicit v6-to-v7
migration adds unity gain, unmuted, follow-video-enabled records for every
input kind while preserving v6 masks and exact v5 desired/realized manual
transition state. Native daemon preflight transactionally maps those records
to `fm-audio::Gain` and constructs both Master mixer copies with the target
gain applied immediately rather than as a restart ramp. Checkpoint/restart
keeps the strips because engine routing projection clones the canonical
project. Persistence, generated-audio, AFV selected/inactive, mute, immediate
startup gain, and failed-preflight no-partial-state tests cover this slice.
There are still no live strip commands or operator controls, meters in the
studio, pan/solo/PFL, realized strip delay, labels/groups, bus sends,
microphone automix, device-audio path, or acceptance evidence. Phase 2 item 7
is partial, Phase 3 item 3 is partial, and `AU-001`/`AU-007` remain planned.

The worker and synchronizer retain bounded blocks, samples, and bytes. Refills
reserve capacity before nonblocking dispatch, prioritize uncovered sources, and
commit a completed batch only after every returned page validates. EOS silence
is staged and preflighted across all affected sources before any cursor changes;
render preflights every source commit and sink admission before advancing the
absolute frame cursor. Synchronizer/source PCM buffers, per-terminal render
planes, and EOS staging scratch are preallocated. The canonical returned block
and bounded fake-sink clone still allocate per frame; event-bearing
`FrameResult`s and other control-path allocations can also remain. Exact source
channel layout is enforced across pages and must map by matching labels to
Master; there is no channel conversion.

Scene inputs recursively route audio through explicit `audio_source` links to a
physical leaf or explicit silence. Cut keeps one source at unity; Fade and Wipe
crossfade two sources with sample-linear gains from each interval's explicit
start/end mix endpoints. Physical terminals render once per interval, but
distinct logical strips that share one terminal remain independent mixer
submissions with their own gain/mute/follow-video state and transition
coefficients. Truly identical logical Program IDs submit once at unity.
Automatic Fade and held or reversed Fade and Wipe T-bar movement propagate exact
endpoints. The manual-transition core is exposed through `EngineCommand` and
protocol 1.4, and schema v7 preserves exact desired and realized manual state
across replay-safe daemon restart, including through the explicit v5-to-v6
no-mask migration. The CLI exposes local and remote manual Start, exact integer
position, Commit, and Cancel commands; its local path restores and saves the
schema-v7 engine state and its remote path gates the commands at protocol 1.4.
Studio negotiates protocol 1.4 and exposes permission-gated manual Fade/Wipe
controls while Ready. Web now has only a protocol-1.4 transport-free semantic
manual-control model; it still has no browser renderer or network runtime.
Missing local audio, stills, scene silence, and
configured simulated silence produce silence; unsupported simulated sine audio
is rejected. At decoded EOS, the daemon synthesizes continuing silence for
subsequent Master intervals. It exposes no media-completion/end trigger and has
no stop, loop, or operator-selectable EOS policy.

`FREEMIXD_TELEMETRY` v4 reports current and observed-peak retained
blocks/samples/bytes; reservation requests and current/peak reserved
blocks/samples/bytes; source stalls; positioned blocks/samples; leading-silence
samples; EOS-padding blocks/samples; and fake-sink current depth, peak depth, and
drops. Evidence is split by media type. The simulated-silence
`phase2_native_diagnostic_soak` exercises Master cadence, fake-sink pressure,
checkpoint/restart, and telemetry only; it does not exercise decoded-audio
resampling, refill, positioning, or EOS. Dedicated ignored integrations
separately cover daemon refill/checkpoint
(`native_media_daemon_refills_beyond_startup_prefix_and_checkpoints_once`),
44.1-to-48 kHz recording
(`native_44_1k_local_audio_resamples_to_48k_master_recording`), and deep restored
FFmpeg positioning/resampling
(`deep_restored_44_1k_nonzero_pts_positions_without_pcm_prefix_and_preserves_phase`);
deterministic native-runtime tests cover EOS padding. The path remains a
fixed-clock linear diagnostic resampler feeding a fake sink: there is no drift
estimator, device clock or OS audio device, channel conversion, persisted
buses/output routing or strip controls, DSP, or externally delivered audio.
Dedicated Wipe audio-policy tests cover exact interval endpoints, sample-linear
rendering, unity Cut completion, and held, reversed, and committed manual T-bar
movement. Protocol-driven native-media end-to-end and hardware evidence for the
manual path is absent, and the Web operator path remains incomplete. Item 7 and
the related parity rows therefore remain incomplete and planned.

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
rendering after restart. Because its audio is simulated silence, it exercises
Master cadence, the fake sink, checkpoint/restart, and telemetry, not decoded
resampling, refill, positioning, or EOS. Its default duration is 60 seconds;
environment flags can require native startup and GPU timestamps, and its report
is explicitly classified `diagnostic-not-certification` with
`basic_show_complete=false`. A required local 60-second macOS/Metal run rendered
all 1,500 requested frames, completed 59 commands, reported 1,333 lifetime GPU
timing samples, resumed to 1,575 total frames after a separate three-second
restart, and exposed 1,492 bounded fake audio-sink drops rather than hiding them.
This is useful lifecycle and pressure evidence, not the universal Basic Show
scenario. That scenario requires four cameras, two clips, a title, a browser
source, a two-box scene, streaming, and recording for one hour on every Tier-1
OS. Cameras and persisted scenes are scheduled for Phase 3, streaming for Phase
4, and title/browser inputs for Phase 5; no Windows/DX12 or Linux/Vulkan one-hour
evidence exists. Item 10 is therefore incomplete and is structurally blocked by
capabilities scheduled after Phase 2.

Exit: a useful offline/file-based switcher, with no engine dependency on UI
lifecycle.

## 5. Phase 3 — live capture and professional core (12–16 weeks)

1. Add OS camera, audio, and screen/window capture adapters.
2. Implement source hot-plug, signal loss/fallback, timestamp validation.
3. Implement audio device clocks, drift correction, channel mapping, delay,
   native EQ/gate/compressor/limiter.
4. Add scenes with background plus ten layers, keys, masks, and effect stack.
5. Complete standard transition families, AlphaFade, FTB, and T-bar operator
   controls and acceptance evidence.
6. Add eight overlay channels, stinger preload/cut point, per-output inclusion.
7. Add two multiviews, labels, tally, audio meters, safe areas.
8. Add scopes and professional color correction.
9. Add shortcuts and initial MIDI/OSC/controller support.
10. Certify Profile A without network contribution.

Current implementation boundary for item 3: `fm-audio` provides a bounded,
deterministic planar-F32 sample-delay primitive with immutable channel count and
exact nonnegative sample delay, transactional block validation, caller-owned
output, allocation-free steady-state processing, and explicit reset. The
reference `MasterMixer` now gives every logical strip independent raw-planar
delay history before channel mapping and gains, advances that history with
submitted PCM or silence on every successful Master interval, and bounds total
retained history per mixer. Delay configuration is an in-memory mixer API with
transactional allocation and leading-silence reset; it is not connected to
device audio, project persistence, daemon/protocol commands, native DSP, or
operator UI. This remains a partial item 3 slice and does not complete or change
the status of any `AU-*` parity row.

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

At this adapter boundary, camera recovery is adapter-local, caller-driven, and
bounded. Hold continues returning the last delivered frame through signal loss
and rejected recovery attempts. A restarted helper retains source identity and
must produce the same clock with a strictly newer PTS before recovery completes;
the first accepted frame is marked discontinuous. Sequence remapping creates one
globally monotonic stream while preserving native sequence gaps, including gaps
from bounded queue drops before the first recovered delivery. Received,
queue-drop, native-drop, continuity-rejection, and timeout-discard counters plus
peak depth are cumulative across attempts; current queue depth is instantaneous.
Recovery startup, first-frame waits, timeout discard, and late-producer
exclusion are bounded. Foreground helper cleanup attempts return by a bounded
deadline. If child or worker completion misses it, ownership is retained and
handed to a background reaper on final teardown rather than detached; blocking
reap/join completion itself is not guaranteed within a bounded time. Hermetic
helper-process evidence covers malformed, regressed, recovered, timed-out, and
truncated attempts.

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

Current implementation boundary for item 2: `freemixd --native-media` realizes
macOS `Device` inputs by exact persisted `stable_key`; `--camera-helper` supplies
an application-packaged helper path. Startup discovery does not prompt, requires
already-granted permission, rejects duplicate or unknown keys, and opens only an
advertised BGRA mode matching the project dimensions and exact rational frame
rate. Camera helpers start concurrently under one initial-frame deadline with a
one-frame adapter queue and Hold fallback.

After preflight, each camera has a persistent supervisor worker that owns its
adapter lifecycle independently of the render loop. Recoverable signal loss and
runtime helper exits enter bounded retry with capped backoff; exhausted attempts
enter a bounded rearm wait and retry again. Every attempt retains the exact
source identity, clock, and mode. A one-frame latest slot reports replacement
pressure and carries a pending discontinuity onto its replacement, so a dropped
first recovery frame cannot hide the handoff. Malformed framing and other media
contract failures are fatal rather than retried. During recovery, continuity
timestamp regressions are instead rejected, counted, and skipped while Hold
continues; if no valid frame arrives before the first-frame deadline, timeout
returns to daemon retry. Daemon shutdown requests cancellation for all workers
before waiting against one aggregate deadline.

`FREEMIXD_TELEMETRY` v4 and `FREEMIXD_CAMERA_SOURCE` v1 include recovery
attempt/success/exhaustion/failure state, adapter queue and ready-delivery state,
terminal/cancellation discards, latest-slot replacement/depth, preflight state,
ingest failures, and successful live GPU ingests. Per-source tests assert exact
conservation of every adapter-received frame across ingestion, explicit discard
or replacement classes, and instantaneous outstanding slots; daemon aggregates
are folded from the same snapshots. Telemetry v4 additionally exposes native
audio current/peak retention and reservation blocks, samples, and bytes;
reservation requests, stalls, positioned blocks/samples, leading-silence and
EOS-padding samples, and fake-sink depth/peak/drop counters. Diagnostics remain local
`diagnostic-not-certification` records and omit hardware identifiers, names,
stable keys, paths, health details, clocks, and media.

Hermetic daemon-level evidence covers exact helper arguments and mode, recovery
while rendering/checkpointing continues, retry exhaustion and rearm, generic
runtime exit recovery, fatal malformed contracts without restart, one recovering
camera alongside one uninterrupted camera, aggregate multi-camera startup
cleanup and shutdown cancellation, frame conservation, and helper reaping. The
hermetic daemon process exercises generated camera metadata plus selected GPU
ingest, frame conservation, source timing, checkpointing, and cleanup. Separate
protocol and capture-node tests cover metadata boundaries, and schema-v7
persistence round-trips Display-P3/BT.709. Separate native Metal readback tests
compare color conversions across supported primary/transfer combinations
against CPU oracles; this does not claim that every combination traverses and
is pixel-validated through the full daemon path.

Portable protocol/lifecycle and capture-node process tests pass; required local
D2 discovery enumerated two real macOS camera endpoints with 252 and 126 bounded
candidate modes and `prompt-required` permission, and the real smoke command
stopped at permission preflight without prompting. No real camera
frame-acquisition or daemon hardware evidence therefore
exists; the passing daemon run is hermetic rather than device certification. The
slice also omits paired/synchronized camera audio, signed helper packaging,
source-clock scheduling and drift correction, live per-source telemetry
subscription, HDR transfers, ICC-only profiles, gamma-tag fallback, configurable
unknown-metadata policy, autonomous inventory hot-plug, real hardware recovery
certification, Windows/Linux adapters, audio-device daemon/Master realization,
and screen/window/application-audio capture. Items 1 and 2, plus `IN-001`,
`IN-005`, and `IN-011`, therefore remain incomplete and planned.

Current implementation boundary for item 4: native `freemixd` now realizes
schema-v7 scene inputs through an immutable `NativeProjectPlan` compiled before
opening media or GPU resources. The planner rejects missing references and
video/audio cycles, bounds reachable scenes and total enabled layers at 64 each,
and enforces a default 512 MiB peak transient RGBA16F budget. It maps full-width
128-bit input/scene identities to generated compositor tokens without narrowing,
orders nested and shareable scenes dependency-first, and preserves background,
enabled state, translation/size, crop, opacity, signed z-order, and 0/90/180/270
degree rotation. Per frame it derives the selected Program transition roots,
renders only their dependency closure once, releases non-root scene textures
after their final consumer, and composites both scene endpoints before applying
Cut, Fade, or Wipe.

Scene audio recursively follows each explicit `audio_source` to one physical
leaf or explicit silence; two scene routes that resolve to the same terminal are
mixed once at unity. One GPU completion-fenced project-frame slot prevents the
next frame from reclaiming transient scene textures before prior submissions
complete, and the planner charges the worst selected closure plus transition and
previous-Program targets against its peak budget. CPU planner tests cover bounds,
cycles, full-width-ID collisions, visual mapping, selected closure, recursive
audio, and sharing. Native Metal tests cover nested shared scenes before Fade and
Wipe plus 96 completion-fenced frames without production readback; a daemon test
checkpoints and restarts from scene Program/Preview routes. These are macOS/Metal
and hermetic diagnostics, not cross-platform certification. Independently,
`fm-compositor` executes hard-edged rectangular masks in half-open post-crop
source space. Schema v6 persists an optional mask per layer and native
`freemixd` maps it into the immutable plan after strict post-crop bounds
validation. CPU-oracle tests cover crop, half-open edges, inversion, rotation,
translation, and stable resource accounting; ignored native Metal readback
coverage remains opt-in and is not claimed without an adapter. The explicit
v5-to-v6 migration supplies a no-mask default without changing desired or
realized T-bar runtime state, and daemon checkpoint/restart tests preserve
masks. Feathered or non-rectangular masks, keys, effect stacks, a ten-layer
product limit, per-output scene realization/routing, live scene
edits/replanning, and cross-platform or hardware certification remain. Item 4
and its parity rows therefore remain incomplete and planned.

Current implementation boundary for item 5: horizontal Wipe now flows through
local and remote CLI commands, `fm-control`, `EngineCommand`, the switcher,
`fm-sim`, the native compositor, and daemon rendering/checkpointing. Protocol
1.3 gates `CommandPayload::Wipe`; the server rejects Wipe from an older
negotiated peer before durable acceptance or control/engine mutation. Exact
rational progress selects `floor(width * numerator / denominator)` replacement
columns and preserves identical start/end frames, with exact CPU and Metal
coverage of endpoints and pixel boundaries. `freemix-studio` now negotiates
protocol 1.4 and exposes a Wipe button only when both transition permission and
the negotiated protocol allow it; Fade and Wipe share one bounded duration.
Recovery preserves strict intent FIFO across a tested protocol downgrade, so an
unresolved Wipe is neither sent to 1.2 nor bypassed by later commands. Bounded
terminal history, collision-triggered authoritative resync, and Studio's sticky
terminal-uncertainty ledger cover ambiguous replay receipts. `freemix-web`
declares protocol 1.4 support in its client configuration. Its transport-free
semantic presentation model preserves permission- and protocol-gated
Cut/Fade/Wipe and adds manual Fade/Wipe Start, exact basis-point Position,
Commit, and Cancel controls derived from separate authoritative desired and
realized projections. No browser renderer or network runtime exists.

The T-bar control core supports Fade and Wipe through `fm-switcher`,
`EngineCommand`, `fm-control`, and protocol 1.4 with exact held, reversed,
committed, and cancelled progress. Schema v6 persists distinct desired and
realized manual state, including through v5 migration, and daemon process tests
cover replay-safe restart through commit and cancel. The CLI exposes local and
remote Start, integer `0..=10_000` basis-point position, Commit, and Cancel
commands; local command processes restore/mutate/save the engine checkpoint,
and remote commands cannot cross a protocol 1.3 session. Studio advertises
protocol 1.4 and presents replicated desired and realized kind, routing, and
exact position without treating widget state as engine truth. Its manual
controls require Ready state, transition permission, and negotiated 1.4.
Reconnect tests preserve strict worker FIFO: an unresolved manual head and
later commands remain blocked through a 1.3 downgrade and resume unchanged only
on 1.4. Web now has only a protocol-1.4 transport-free semantic manual-control
model; it still has no browser renderer or network runtime. There is no
protocol-driven native-media end-to-end or hardware evidence for the manual
path. FTB, AlphaFade, stinger, Slide/Zoom, the remaining transition families,
and `SW-004` acceptance remain pending; parity therefore stays planned. Item 5
and RC-007 remain planned.

Compositor-only FTB groundwork now admits a bounded exact-rational start, end,
and progress plan, including hold and reverse trajectories. Its CPU oracle and
native wgpu path apply the plan after canonical RGBA16F Program composition by
mixing premultiplied linear RGBA toward opaque black, without color conversion,
audio work, or production readback. No switcher, engine, protocol, persistence,
operator control, output-routing integration, or acceptance evidence is exposed
by this slice, so Phase 3 item 5 and `SW-004` remain planned.

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
10. Run process-kill, disk-full, network impairment, and 24-hour soak. (needs to be in a VM or something else, don't kill my pc)
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

Acceptance-test evidence progresses from `planned` to `present` to `verified`.
`present` records local test material and resolvable command targets without
claiming the feature contract is complete. Every feature must reference its
owned `accept-<feature-id>` record. A feature may be `verified` only when every
referenced acceptance test is also `verified`, has both local test paths and
resolvable test commands, and declares the test file resolved by each command.
