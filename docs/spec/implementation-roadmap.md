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

During current development, the repository carries one project schema, one wire
protocol, and one exact plugin ABI/state-snapshot version only. Breaking changes
replace those contracts in place: do not add schema or snapshot migrations,
protocol downgrade projections, plugin ABI ranges, legacy API facades,
compatibility fixtures, or backward-regression gates. Delete superseded paths
and update current-contract coverage. Version compatibility policy is outside
this development roadmap.

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
8. Freeze the vMix 29 feature-parity ledger and tag every row with acceptance
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
3. Implement the current project schema, journal, and atomic save.
4. Implement commands, events, revisions, idempotency, transactions.
5. Implement capability registry and compatibility report.
6. Materialize `parity.toml` from the ledger with phase, owning workstream,
   dependencies, platform/capability profile, and acceptance-test IDs; CI rejects
   missing or duplicate mappings.
7. Implement editable graph, validation, plan representation, bounded budgets.
8. Implement deterministic clock, fake video/audio sources/sinks, and scheduler.
9. Implement exact-current server handshake, snapshot/event resume, auth
   development mode.
10. Implement UI replicated model and a diagnostic client.
11. Add current-contract protocol/project integration and simulated end-to-end
    tests.

Exit: a headless simulated production can be controlled by CLI and client,
saved, restarted, resumed, and tested deterministically.

Current Phase 1 control boundary: the daemon, protocol, and client exercise a
versioned raw TCP session with handshake, snapshot/resume, commands, events,
development authentication, bounded bidirectional heartbeat acknowledgement,
and deterministic model tests. Protocol 2.13 added one bounded, read-only
control-plane diagnostics query, authorized by `ViewStatus`; it reports the
point-in-time control diagnostics and does not mutate engine or durable state.
It is not telemetry, health, or readiness, and RC-015 remains planned.
Protocol 2.14 adds the ordered configured output ID/name roster required for
exact overlay-output inclusion controls; it does not prove output delivery.
Protocol 2.15 defines bounded fixed-point Master and per-input meter records;
`fm-client` validates identity and monotonic sequence without changing the
durable cursor. The daemon does not publish these records yet.
Protocol 2.12 retains the protocol 2.10 rule
that the daemon acknowledges only a validated heartbeat with the session server identity, the exact client
heartbeat sequence, and the server receive time. Studio accepts one expected
sequence and uses its bounded peer wait before the existing reconnect backoff.
Incomplete pre-handshake peers expire on the same absolute heartbeat deadline.
The one-shot CLI has finite connect/read/write deadlines and accepts only exact
newline-terminated records up to 64 KiB.
While the CLI waits for a command, it applies valid peer events but still
requires the accepted revision's durable event and realized runtime event.
Studio diagnose checks one bounded raw-TCP heartbeat acknowledgement and keeps
waiting when validated durable or runtime state arrives first, then reports
validated point-in-time control diagnostics. RC-015 remains planned.
The default simulated daemon cooperatively stops after Unix SIGINT/SIGTERM or Windows Ctrl-C through bounded listener polling.
Simulated CLI PPM output uses same-directory write/sync/replace, not broader media or disk certification.
Local CLI loads reject unapplied journal batches without recovering or mutating journal state; torn-final-only state remains readable.
This is control-plane peer liveness. It is not service readiness, production
authentication, media health, HTTP resources, WebSocket event subscriptions,
or transport-level production rate limits. Native and non-native raw TCP now
share the bounded single-thread scheduler: an idle Viewer can receive live
events while an Operator controls the show, and native mode remains limited to
one peer. Native-media acceptance is not proven on this Mac. This is
prerequisite transport infrastructure only; AU-001 daemon and Studio meter
transport remain planned. It is not HTTP/WebSocket/service/media readiness or
RC-008 completion. Expired raw TCP sessions are reclaimed.

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
an adapter. The current schema persists each scene background and each layer's explicit
input/scene source, geometry, crop, optional hard-edged rectangular mask,
opacity, and z-order. Mask
bounds are validated against the effective
post-crop source dimensions before native planning; inversion does not change
planner capacity or transient-resource accounting.
`InputKind::Scene` explicitly routes a `SceneId` plus an optional audio
`InputId`, while every persisted output explicitly routes a video `SceneId` and
audio `BusId`; outputs are preserved as declared and are not inferred. Native
`freemixd` now consumes this visual model,
including the persisted rectangular mask, through the bounded scene planner
described under Phase 3 item 4 instead of rejecting scene inputs. Feathered or
non-rectangular masks, keys, effects, per-output realization, live
edits/replanning, and cross-platform or hardware certification remain outside
this item, so its parity rows remain planned.

Current implementation boundary for item 5: `freemix-studio` opens a native
`eframe`/wgpu shell by default with responsive Program/Preview monitor wells,
stable-ID input tiles, realized/desired tally, and permission-gated Cut/Fade/Wipe
controls. Manual Fade/Wipe/AlphaFade/Slide T-bar controls require transition permission,
synchronized state, and Ready lifecycle. The manual panel displays
replicated desired and realized kind, routing, and exact integer basis-point
position. Bounded worker channels preserve order across retained non-coalesced action
and input boundaries while recovering, and later retained intents cannot overtake
unresolved commands. Native Studio locally retains bounded unsent intents and uses
latest-value coalescing for adjacent manual-position values and same-input audio-strip
field edits; this can suppress intermediate unsent manual or audio effects.
`fm-client` retains a
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
process-group/job and descendant cleanup. The supervised stdout readiness record
is byte-bounded; this does not add service/media readiness or RC-008 completion.
Studio UI state is latest-state and
cannot block control I/O; this adds no preview delivery, service readiness, or
transport support, and RC-008 remains planned. Studio Program/Preview wells display
replicated desired and realized routing input names; this control state is not
output-video proof, and preview delivery remains pending.
An owned supervised daemon exit detected while Studio is idle enters the existing
bounded reconnect/restart path; protocol mismatch, restart-limit, and fatal supervisor
errors stay terminal, and this does not complete item 5 or RC-008 or prove service, media, or cross-platform readiness.
`freemix-web` likewise declares only the current protocol in its client configuration.
Its transport-free semantic presentation model preserves the existing
permission- and protocol-gated Cut/Fade/Wipe controls and adds manual Fade/Wipe/AlphaFade/Slide
Start, exact basis-point Position, Commit, and Cancel controls. Manual controls
derive availability from Ready state, transition permission, and separate authoritative desired and realized
manual projections. It also models protocol-gated FTB actions and exact state.
Web still has no browser renderer or network runtime, and the remaining
transition families plus cross-platform output acceptance keep item 5 and
RC-007 planned.

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
gain/stereo-balance/mute/solo/follow-video, meters, timed canonical blocks,
sample-count timing validation, and transactional gain and balance ramps.
Allocating reference outputs include deterministic per-input post-strip peak
and RMS readings in `InputId` order after delay, channel mapping, operator
gates, gain, balance, and transition gain. Silent configured strips remain
visible with zero readings. An opt-in allocation-free planar render API writes
Master and per-input readings into exact-size caller-owned flat buffers without
changing the existing native caller. Protocol 2.15 and `fm-client` can carry a
lossy meter sample, but the daemon does not publish it and Studio does not show it.
Its bounded
`ClockMappedAudioSynchronizer` is now connected to native `freemixd` local-file
audio. The daemon accepts source rates such as 44.1 kHz and linearly resamples
them to the 48 kHz project Master while preserving absolute source and Master
cadence origins. Master intervals derive directly from absolute engine frame
numbers, including deep restored cursors and fractional video rates. Initial
positioning compares audio and video on their relative media timeline: early
audio is trimmed and delayed audio produces bounded leading silence. Every
decoded source, including an inactive one, advances on every Master interval so
later switching does not replay stale audio.

The current schema also persists one exact per-input audio strip as bounded
integer milli-dB gain (`-96000..=24000`), stereo balance
(`-10000..=10000` basis points), mute, solo, follow-video, and a bounded
0–48,000-sample delay. Native daemon preflight transactionally maps those
records to `fm-audio::Gain` and `fm-audio::Balance` and constructs both Master
mixer copies with the target gain, balance, and delay applied immediately.
Protocol 2.12 carries authoritative input IDs and canonical names plus full-strip
status in snapshots. It adds an EditProject-authorized input rename command and
a small durable rename event without changing the snapshot shape. Other durable
events carry full-strip state plus one permission-gated atomic live strip command. The engine schedules
accepted gain, balance, mute, solo, follow-video, and delay changes together at a frame boundary, and
native realization updates active and pending Master/Stinger mixers and the
checkpoint project atomically. Live gain and balance changes then ramp linearly
over 240 samples (5 ms at the 48 kHz project Master rate) while mute,
solo, follow-video, and delay take effect at the next rendered sample; restored strips
remain immediate. Balance is linear after channel mapping: negative values
attenuate the right destination, positive values attenuate the left destination,
and non-stereo destination labels remain at unity.
Solo is solo-in-place across the logical Master strips: if any strip is soloed,
every non-soloed strip is gated. Mute and follow-video remain independent gates,
so a muted or video-inactive soloed strip can intentionally produce silence.
Local and remote CLI commands, Studio controls,
and the Web semantic control model expose the same bounded mutation. Restart
restores engine desired state from the canonical strips and checkpoints it back
to the project. Studio resolves coalesced adjacent audio field edits against the latest
confirmed strip before dispatch. Current-contract persistence, generated-audio, AFV
selected/inactive, mute, solo, immediate startup gain/balance/delay, live full-strip realization,
and failed-preflight no-partial-state tests cover this slice.

Studio input tiles and mixer strips use exact persisted input names carried by
the snapshot rather than generated ordinal labels. The client reducer validates
each rename against the current input set, the 128-byte limit, and exact name
uniqueness, then changes only the matching name at the durable revision. It also
carries an authoritative full input reorder through the EditProject command and
durable input-order event; the daemon persists that vector order and restores
the order after restart. Studio now has a compact permission-gated rename editor.
The local `input-add`, `scene-input-add`, `input-duplicate`, `input-remove`, and
`input-replace-simulated` commands persist input state for the next project open
only. `scene-input-add` creates an empty opaque-black scene with no layers and no
audio source. `scene-input-audio-source` and `scene-input-audio-source-clear`
persist the scene input audio route. `scene-layer-add`, `scene-layer-remove`, `scene-layer-appearance`,
`scene-layer-geometry`, `scene-layer-crop`, `scene-layer-crop-clear`,
`scene-layer-mask`, `scene-layer-mask-clear`, `scene-layer-z-order`, and
`scene-background` (premultiplied RGBA), `scene-layer-source-input`, and
`scene-layer-source-scene` are local next-open layer-editing
footholds; local scene rendering, live editing, feathered and non-rectangular
masks, effects, and IN-020 remain planned.
The rename and reorder commands carry input identity; audio and routing state
remain separate.
Neither command applies an optimistic change or changes current switcher state.
Studio exposes compact Up/Down controls
for one-step reorder, gated by the current Ready session and EditProject
permission; the native worker resolves each move against the latest confirmed
order and the daemon remains final authority. Live add/duplicate, input removal,
categories, colors, pause, close, offline, relink, import, and the rest of
IN-026 remain planned.
Studio now has a compact permission-gated gain fader with exact milli-dB entry.
There are still no audio meters in Studio, PFL, strip-name editor, strip groups,
bus
sends, microphone automix, device-audio path,
device-clock correction, native EQ/gate/compressor/limiter, or acceptance
evidence. Phase 2 item 7 is partial, Phase 3 item 3 is partial, and
`AU-001`/`AU-007` remain planned.

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
physical leaf or explicit silence. Cut keeps one source at unity; Fade, Wipe, AlphaFade, Slide, and Zoom
crossfade two sources with sample-linear gains from each interval's explicit
start/end mix endpoints. Physical terminals render once per interval, but
distinct logical strips that share one terminal remain independent mixer
submissions with their own gain/balance/mute/solo/follow-video state and transition
coefficients. Truly identical logical Program IDs submit once at unity.
Automatic Fade and held or reversed Fade, Wipe, AlphaFade, and Slide T-bar movement propagate exact
endpoints. The manual-transition core is exposed through `EngineCommand` and
the current protocol, and the current schema preserves exact desired and realized
manual state across replay-safe daemon restart. The CLI exposes local and remote manual Start, exact integer
position, Commit, and Cancel commands; its local path restores and saves the
current engine state. Studio exposes permission-gated manual Fade/Wipe/AlphaFade/Slide
controls while Ready. Web now has only a current-contract transport-free semantic
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
movement. A protocol-driven macOS/Metal recording acceptance now covers manual
video progression, reversal, cancel, and commit, but does not distinguish
T-bar audio sources or certify other platforms and outputs. The Web runtime
operator path also remains incomplete. Item 7 and the related parity rows
therefore remain incomplete and planned.

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
output; the first capture, backend, readback, frame, or enqueue failure emits a
sanitized immediate `FREEMIXD_RECORDER_FAILURE` record before cancellation,
while the final `FREEMIXD_RECORDER` record remains the shutdown result with no
live telemetry, alerting, restart, or fault-tolerant recording.
`--record-program` enables this path only with
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
latency fixture. The raw TCP heartbeat acknowledgement is not service readiness,
telemetry, or media health. Item 9 therefore remains incomplete and the broader
RC-015 parity row remains planned.

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

Current implementation boundary for item 9: native Studio maps unmodified keys
1–8 to Preview selection for the first eight replicated inputs, C/F to Cut/Fade
with the current transition duration, B to toggle FTB with its dedicated duration,
and Escape to cancel a desired active manual T-bar, while its current Ready, view,
permission, focus, non-repeat, modifier, and manual-control gates allow the action. This adds no binding editor,
chords, scopes, persistence, MIDI/controller runtime, conflict handling, or
shortcut acceptance evidence. When configured, native Studio also accepts the
bounded loopback OSC action contract, including zero-argument manual commit and
cancel and automatic AlphaFade, Slide, Zoom, and Wipe actions, with the same
current view, permission, and manual-transition gates;
malformed, rejected, and overflow counts are visible to
the operator. RC-004 remains planned.

Current implementation boundary for item 6: the switcher and engine now own
eight independently addressed overlay channels with a retained source, exact
on/off state, source update, a deduplicated set of included output IDs, and an
independent Cut/Fade transition with a bounded 1–3,600-frame duration.
Overlay commands are frame-boundary mutations independent of an active Program
transition, and desired/realized overlay arrays are included in engine frames,
durable switcher events, snapshots, client projections, and idle checkpoint
validation. Fade realization advances channel opacity deterministically, keeps
fade-out channels active until their zero-opacity endpoint, and rejects idle
snapshots while any channel is moving. Every channel also owns a bounded
64-source FIFO queue and deterministic full-frame, top-left, top-right,
bottom-left, and bottom-right position presets plus none/thin-white/thick-white
inset-border presets. Schema 17 persists the complete desired and realized
arrays, appearance, queue state, and exact per-input audio strips, and accepts schema
17 only. Protocol 2.12 carries canonical input names, durable input renames, opacity, transition kind,
duration, Take, Update,
Off, output inclusion, transition/appearance configuration, Queue, Take Next,
and atomic per-input audio-strip commands and state, and accepts protocol 2.12 only.

Control authorization treats overlay mutations as transition operations. The
client/UI reducer validates exactly eight unique channels, active-source
references, and output inclusion uniqueness. CLI local and remote commands
cover Take, Update, Off, inclusion, Queue, Take Next, and Cut/Fade/appearance
configuration; status prints both desired and realized arrays with opacity,
transition, appearance, and queue state. Web exposes transport-free semantic
controls, and Studio exposes Take Preview, Update Preview on active channels,
Queue Preview, Take Next, Off, and per-channel Cut/Fade/position/border
controls plus one persisted-name output inclusion toggle for every configured
output. Studio displays desired and last-confirmed realized source, activity,
opacity, queue, and appearance summaries for all eight channels; its output
toggles reflect the latest confirmed desired inclusion state. This is not
output-video evidence. Queued
Studio overlay toggle and cycle actions resolve from the latest confirmed desired
channel, so repeated clicks remain cumulative before dispatching full protocol commands.
The native daemon derives a stable realization for every configured project output,
unions their active overlay sources into one scene-execution closure, renders the
authoritative Program transition or Stinger base once, and then produces and
retains one independent GPU texture per output. Each output composites only its
included channels source-over in channel order before Fade-to-Black, using the
exact per-frame opacity, deterministic one-third-frame PiP geometry, and
scale-aware inset white border presets implemented by both the CPU oracle and
native WGSL compositor. Resource planning charges the shared base, per-output
composition and final targets, and the prior retained output set. The existing
fullscreen and recorder consumers select the first configured output
deterministically; an output-less headless project retains its unbound Program
path.

This completes the independent source, on/off, per-output-inclusion,
Cut/Fade-duration, position/border, bounded FIFO queue, and simultaneous native
output-realization slices. Profile-wide hardware acceptance evidence remains,
so `SW-005` stays planned.

Current implementation boundary for item 3: `fm-audio` provides a bounded,
deterministic planar-F32 sample-delay primitive with immutable channel count and
exact nonnegative sample delay, transactional block validation, caller-owned
output, allocation-free steady-state processing, and explicit reset. The
reference `MasterMixer` now gives every logical strip independent raw-planar
delay history before channel mapping and gains, advances that history with
submitted PCM or silence on every successful Master interval, and bounds total
retained history per mixer. Schema 17 gives every persisted input strip exact
bounded gain, stereo balance, mute, solo, follow-video, and 0–48,000-sample delay
values and rejects missing, wrong-typed, or out-of-range fields.
Native project compilation carries that value into physical and scene-alias
strips, and native Master/Stinger preflight applies it transactionally to both
active and pending mixers before gain, balance, mute, solo, follow-video, and source envelopes.
The engine owns the live desired full-strip map and emits frame-boundary
realization updates. Protocol 2.12 snapshots pair every input ID with its
canonical persisted name and replicate the complete strip map; its
`SetInputAudioStrip` command atomically carries gain, balance, mute, solo,
follow-video, and delay and is authorized by the dedicated audio-control
permission. The daemon applies each update transactionally to every active and
pending ordinary/Stinger mixer, with 240-sample linear live gain and balance
ramps and next-sample mute/solo/follow-video/delay changes, before checkpointing the
canonical project. The local and remote CLI expose the command, exact status,
and input labels; Studio renders persisted names with Ready- and
permission-gated per-input controls; Web exposes the equivalent
transport-free semantic controls. Device audio and clocks, drift correction,
channel mapping, native EQ/gate/compressor/limiter, meters, and hardware
acceptance remain. This is still a partial item 3 slice and does not
complete or change the status of any `AU-*` parity row.

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
protocol and capture-node tests cover metadata boundaries, and the current schema
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
current-schema scene inputs through an immutable `NativeProjectPlan` compiled before
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
coverage remains opt-in and is not claimed without an adapter. Daemon
checkpoint/restart tests preserve masks. Feathered or non-rectangular masks,
keys, effect stacks, a ten-layer
product limit, per-output scene realization/routing, live scene
edits/replanning, and cross-platform or hardware certification remain. Item 4
and its parity rows therefore remain incomplete and planned.

Current implementation boundary for item 5: horizontal Wipe now flows through
local and remote CLI commands, `fm-control`, `EngineCommand`, the switcher,
`fm-sim`, the native compositor, and daemon rendering/checkpointing. The current
protocol carries `CommandPayload::Wipe`. Exact
rational progress selects `floor(width * numerator / denominator)` replacement
columns and preserves identical start/end frames, with exact CPU and Metal
coverage of endpoints and pixel boundaries. `freemix-studio` enables its Wipe
button when transition permission allows it; Fade and Wipe share one bounded
duration. Recovery preserves strict intent FIFO, so an unresolved Wipe is not
bypassed by later commands. Bounded
terminal history, collision-triggered authoritative resync, and Studio's sticky
terminal-uncertainty ledger cover ambiguous replay receipts. `freemix-web`
declares only the current protocol in its client configuration. Its transport-free
semantic presentation model preserves permission- and protocol-gated
Cut/Fade/Wipe and adds manual Fade/Wipe/AlphaFade/Slide Start, exact basis-point Position,
Commit, and Cancel controls derived from separate authoritative desired and
realized projections. No browser renderer or network runtime exists.

The T-bar control core supports Fade, Wipe, AlphaFade, and Slide through `fm-switcher`,
`EngineCommand`, `fm-control`, and the current protocol with exact held,
reversed, committed, and cancelled progress. The current schema persists
distinct desired and realized manual state, and daemon process tests cover
replay-safe restart through commit and cancel. The CLI exposes local and
remote Start, integer `0..=10_000` basis-point position, Commit, and Cancel
commands; local command processes restore/mutate/save the engine checkpoint,
and remote commands use the current contract. Studio presents replicated desired and realized kind, routing, and
exact position without treating widget state as engine truth. Its manual
controls require Ready state and transition permission. While either replicated
manual state is active, Studio disables automatic Program/Preview transitions and
input-bank Preview selection, but leaves Fade-to-Black and overlay controls
independently gated. Reconnect handling
preserves strict worker FIFO. Web's transport-free semantic model
retains the manual controls and adds FTB live/black actions, a bounded duration,
exact separate desired and realized state, reversal through the opposite target,
and protocol/permission/readiness/completeness gates. It still has no browser
renderer or network runtime. A hardware-gated macOS recording acceptance covers
protocol-driven native manual Fade progression, reversal, cancel, and commit on
one configured output.

AlphaFade groundwork preserves the switcher's existing distinct transition kind
through the simulated pipeline, compositor CPU plan, native wgpu plan, daemon
video planner, and sample-linear Master-audio transition plan. The selected
contract independently interpolates every premultiplied RGBA channel, including
alpha. Exact CPU tests cover transparent intermediate pixels, daemon unit tests
cover video mapping and continuous Master-audio endpoints, and an opt-in Metal
test compares AlphaFade output with the CPU linear-frame oracle on a real
adapter. The current standard Fade uses the same channel-wise reference math, so
these primitives do not yet prove a visible product distinction between Fade and
AlphaFade.

The automatic path now continues through a bounded `EngineCommand`,
target-free transition authorization, runtime lifecycle tracking, and a
current-contract `AlphaFade` command. The client preserves the exact duration.
Daemon durable execution
projects all requested frames before saving, then a focused checkpoint test
restores the settled Program/Preview routing and engine counters exactly. The
CLI now exposes matching local and remote `alpha-fade ... <frames>` commands.
Local execution settles and saves the bundle with replay-safe idempotency;
remote execution preserves the exact duration. Web exposes a semantic
AlphaFade action with the shared bounded duration when Ready and authorized.
It remains a transport-free model with no browser network runtime. Studio presents an
AlphaFade action alongside Fade/Wipe using the same bounded duration. Its
availability requires Ready state, replicated view, and transition permission.
The worker maps the typed intent to the exact command, and a loopback worker test observes the duration, envelope, durable
event, and runtime realization ordering. A hardware-gated macOS/Metal process
acceptance now sends protocol AlphaFade to a real configured Program recorder
using static opaque-white and transparent-black generators, observes an ordered
opaque/intermediate/transparent recording, decodes the result, and verifies the
settled persisted routing.

The authoritative manual T-bar core now accepts AlphaFade alongside Fade and
Wipe, preserving exact held and reversed basis-point intervals through engine
snapshots and feeding the existing AlphaFade native video plus sample-linear
Master-audio plans. The current protocol carries the new manual kind. The
current schema persists the kind, and a
daemon restart acceptance verifies replay-safe commit and cancel from the
restored held state. CLI manual AlphaFade start is available locally and
remotely with local restart-safe idempotent replay. The Web semantic control
surface exposes a manual AlphaFade start control, and
preserves authoritative AlphaFade projections. Studio gates its manual AlphaFade
start button independently from the existing manual
controls, and carries AlphaFade intents through the worker into authoritative
desired and realized presentation. A hardware-gated macOS/Metal process
acceptance drives manual AlphaFade to 75%, reverses to 25%, cancels, then fully
progresses and commits on a configured Program recorder. It requires the
corresponding ordered decoded-luma sequence: initial white, forward gray,
reversed bright, cancelled white, and committed black. It also verifies the
inactive committed checkpoint.

Horizontal Slide rendering groundwork now moves Program left while Preview
enters from the right at the exact integer offset
`floor(width * numerator / denominator)`. CPU and simulated paths cover exact
endpoints and odd-width pixels; a required Metal readback matches the CPU
oracle. Native planning preserves the Slide kind, and Master audio uses the
existing sample-linear two-source crossfade. Automatic Slide is now
command-reachable through the engine and target-free transition authorization.
The current protocol carries its exact duration. Daemon durable
execution settles every requested frame before checkpointing; focused unit and
process restart acceptances restore exact Program/Preview routing, counters,
receipt history, and resume position. CLI now exposes matching local and remote
`slide ... <frames>` commands. Local execution settles and saves the bundle
with replay-safe idempotency; remote execution preserves the exact duration.
Web exposes an accessible semantic Slide action with the shared bounded
duration when Ready and authorized. It remains a
transport-free model with no browser network runtime. Studio presents a Slide
action alongside the existing automatic transitions using the shared bounded
duration. Its availability requires Ready state, a replicated view, and
transition permission. The worker maps the typed intent to the exact command,
and a loopback worker test observes the duration,
envelope, durable event, and runtime realization ordering. A hardware-gated
macOS/Metal process acceptance now sends
protocol Slide through a real configured Program recorder, decodes stable
white, a Slide-specific white-left/black-right intermediate frame, then stable
black, and verifies the settled persisted routing.

Manual Slide now uses the existing T-bar contract and the existing Slide
compositor. Exact held, forward, reversed, cancelled, committed, and restored
basis-point intervals flow through the switcher, engine, persistence, protocol,
durable control/client projections, local and remote CLI, Studio, and the Web
semantic model. The daemon maps those intervals to the existing Slide video
plan and sample-linear two-source audio crossfade. Focused non-native unit and
process tests cover authority, projection, checkpoint restart, and terminal
actions. This is control/runtime evidence only. It does not add a browser
transport or renderer, and it does not certify fullscreen, hardware, or
cross-platform output. Phase 3 item 5 and `SW-002`/`SW-004` remain planned.

Centered Zoom rendering groundwork now overlays Preview on Program with
independently floored extents
`floor(width * numerator / denominator)` and
`floor(height * numerator / denominator)`, a left/top bias for an odd centering
remainder, and deterministic nearest-neighbor sampling from the full Preview
frame. CPU and simulated paths cover byte-exact endpoints plus odd 5×3
intermediates. The native wgpu path carries the same explicit geometry in its
uniform, and a required Metal readback matches the CPU oracle at five progress
points. Native daemon video planning now preserves Zoom, while Master audio
uses the existing sample-linear two-source crossfade. The source-only
audio planner rejects Stinger without project configuration; the project-aware
Master path applies the configured policies described below. Automatic Zoom is
now command-reachable through the engine
and target-free transition authorization. The current protocol carries its
exact duration. Daemon durable execution
settles every requested frame before checkpointing. Focused unit and process
restart acceptances restore exact Program/Preview routing, counters, receipt
history, and resume position. CLI now exposes matching local and remote
`zoom ... <frames>` commands. Local execution settles and saves the bundle with
replay-safe idempotency; remote execution preserves the exact duration. Web
exposes an accessible semantic Zoom action with the shared bounded duration
when Ready and authorized. It remains a transport-free model with no browser
network runtime. Studio presents a Zoom action
alongside the existing automatic transitions using the shared bounded duration.
Its availability requires Ready state, a replicated view, and transition
permission. The worker maps the typed intent to the exact command, and a
loopback worker test observes the duration,
envelope, durable event, and runtime realization ordering. A hardware-gated
macOS/Metal process acceptance now sends protocol Zoom through a real configured
Program recorder, decodes stable white, a Zoom-specific white perimeter with a
black center, then stable black on a 3×3 luma grid, and verifies the settled
persisted routing.

Stinger groundwork now defines a validated zero-based media-frame plan
independent of the generic two-source transition plan. The configured cut point
is the first media frame drawn over Preview; earlier frames are drawn over
Program, and a cut point equal to the media length defers the routing swap until
playback completes. The CPU oracle source-over composites same-size
premultiplied RGBA, while a dedicated native wgpu renderer applies the same
equation without CPU readback or a third shader binding. Required Metal oracles
match the CPU core and the compiled daemon project path before and at the cut.
The current schema durably stores up to eight unique slots with media input,
preload intent, cut point, audio policy, and missing-media fallback. Daemon
restore configures identical
desired and realized slot state, and idle restore rejects divergence.

The engine now rejects unconfigured slots and cut points beyond the requested
duration, applies ready Stingers on exact frame boundaries, and settles
Cut/Fade/KeepProgram missing-media fallbacks without stale busy state. The
current protocol carries a one-through-eight slot plus exact duration. The
native compiled project realizes retained
single-frame Stinger media through exact video base selection and three explicit
Master policies: base-only `Muted`, media-only `StingerOnly`, and unity
media-plus-base `MixWithProgram`. Three independent scene roots are included in
the preflight GPU bound. Restore now honors the durable preload intent:
unrequested slots remain `NotRequested` and take their configured fallback
instead of being promoted to Ready.

The FFmpeg adapter now admits an explicit bounded set of straight-alpha YUVA,
GBRAP, RGBA, and luma-alpha pixel formats into its existing RGBA decode
contract. Native startup gives every requested local-video Stinger a second
video playback instance: its decoder worker, GPU ring, and clip-local deadline
are independent from the same input's ordinary show timeline. The ring retains
at most eight frames, evicts only before the
clip-local floor anchor, refills in bounded pages, holds the final frame only
after confirmed EOS, and restarts its FFmpeg cursor at ordinal zero when a new
trigger requests a deadline before the retained window. Startup partitions the
existing 512 MiB RGBA16F source budget between ordinary and unique requested
Stinger rings, rejecting configurations whose full bounded rings would exceed
that one aggregate limit instead of silently doubling the budget. Replacement
pages reserve only their additional charge, and both registries must match the
project output dimensions before readiness. The daemon caches one read-only
authority projection per upcoming frame so preparation does not repeatedly
clone authority state while polling, and asserts that projection matches the
realized frame.

A hardware-gated Metal/FFmpeg oracle decodes a tagged twelve-frame alpha clip, pages
beyond the initial eight-frame GPU prefix without exceeding it, observes
Program/media/Preview composition across the configured cut, proves the
ordinary input ring was not changed, and verifies a byte-identical retrigger.
A separate native-daemon process acceptance persists the same twelve-frame
asset, sends two immediately consecutive current-protocol Stingers through durable and runtime
realization, records the ordered white/media/black/media/white result, verifies
the restored routing and revision, then starts a second recording daemon from
that checkpoint, fires a third Stinger, decodes its recording, and verifies
revision three plus the opposite settled routing. Live sources and local video
remain rejected path-free until deterministic live capture exists.

An additional required native-daemon process acceptance configures three
preload-disabled slots and sends all three missing-readiness policies over
the current protocol. It verifies two `KeepProgram` commands leave white and black
Program routing unchanged, the `Fade` fallback records an intermediate frame
before settling on black, the `Cut` fallback returns directly to white, and all
four accepted revisions checkpoint the final routing without requiring the
deferred media source.

The local and remote CLI now expose an exact `stinger <slot> <frames>` action,
restore persisted slot state before local mutation, and use the current contract
for remote writes. The offline CLI can also atomically configure, replace,
or remove any of the eight slots with full-width media input, preload intent,
cut point, audio policy, and fallback. It validates the canonical project before
save, preserves routing, manual-transition, Fade-to-Black, revision, frame,
runtime-generation, and receipt state, and projects every persisted slot field
in status output. Web exposes typed one-through-eight slot controls and preserves
the exact requested duration. Studio exposes eight accessible numbered controls with a
Ready replicated transition-capable session, displays authoritative
Unconfigured/Not Requested/Ready/Missing state for every slot, carries typed
slots into the wire payload, and keeps unresolved work ordered in its reconnect FIFO. Loopback
worker evidence observes exact slot, duration,
pending-command state, durable routing, and runtime realization ordering.
The current protocol snapshots project every configured slot field plus the
realized `NotRequested`, `Ready`, or `Missing` preload state. Clients reject an
omitted or invalid projection, and the replicated model validates unique bounded slots and media
input references. Web and Studio require the selected slot's authoritative
`Ready` state before enabling its fire action; Studio loopback evidence observes
the exact projected slot before dispatch. The FFmpeg video and audio cursor
adapters can restart at clip-local ordinal/sample zero while retaining their
fixed source identity and bounded audio metadata index; byte/sample oracles
verify each replay against the original leading decode. The independent native
Stinger video ring now consumes the video restart primitive for bounded paging
and retriggering. The current protocol carries live
configure-or-replace and remove mutations for any slot, including the complete
canonical configuration. The
transition-authorized authority validates the media input, rejects automatic
or manual transition conflicts, settles desired and realized readiness at an
idle frame boundary, emits the projected slot event, and checkpoints the exact
replacement or removal. A non-native daemon process acceptance configures then
immediately fires a slot, verifies the settled routing, restarts on the
replacement descriptor, removes it, and verifies a second empty restart.
Native-media sessions now perform the same mutations without rebuilding the
ordinary show runtime. They classify and abort an initial prepared authority
submission, preflight the complete candidate Stinger project/video/audio pools
on a background worker while native frames continue ticking, then prepare and
revalidate again before any save or commit. Failed resource preflight returns a
path-free `unavailable` result without a receipt, revision, file, projection, or
runtime change. After the authoritative idle frame realizes successfully, the
daemon atomically swaps only the Stinger plan and independent pools, re-limits
future ordinary GPU refills inside the existing aggregate bound, and transfers
old decoder/GPU/audio ownership to a bounded two-worker/two-queued-pool
retirement owner. Retirement never joins on the render thread or daemon
shutdown. A native generator-process acceptance covers Ready configure, fire,
Ready replacement, second fire, removal, exact routing, persistence, and
restart; a separate aggregate-limit rejection acceptance covers
manifest/receipt/revision rollback and continued show scheduling. An opt-in
Metal/FFmpeg acceptance hot replaces an audible local-media lane, fires it,
removes it, and checks aggregate audio telemetry. Cross-platform/fullscreen
evidence and complete `SW-003`/`SW-004` acceptance remain pending; parity
therefore stays planned.

Clip-local Stinger audio is now realized by a second, independently clocked
Master lane for each unique requested audible media input. Native startup
reserves half of the existing aggregate retained-audio block, sample, and byte
caps for ordinary show playback and half for current or future hot Stinger
lanes, then divides the latter across its per-media runtimes. This fixed
reservation prevents adding the first audible slot from rebuilding or
relimiting ordinary synchronizers, cursors, or strip-delay history. Requested
video-only slots use explicit clip silence inside the reserved Stinger side.
Only the selected media runtime is serviced or rendered, so another
slot's decoder, EOS, or stall cannot delay the active clip. Decoder requests
remain single-flight and nonblocking, and a retrigger invalidates stale
generations, restores bounded pre-video positioning, clears synchronizer and
strip-delay history, and restarts the retained FFmpeg decoder at sample zero.
Aggregate retention, reservations, stalls, padding, positioning, and peak
telemetry include every Stinger lane.

The realized policy contract is explicit:
`Muted` suppresses only the Stinger clip and keeps the selected base,
`StingerOnly` suppresses the base, and `MixWithProgram` sums both at unity
before the existing Master output stages. Program is the base before the
configured video cut and Preview is the base at and after it; that base change
does not restart or fade the clip. The media input's persisted strip state
applies to its Stinger bus. Audio before video frame zero is trimmed, delayed
audio produces clip-local leading silence, clip EOS produces silence or
base-only output for the remainder of the transition, and a shorter transition
truncates the clip. Every trigger reanchors sample zero independently while the
ordinary input cursor, strip-delay history, and inactive-source advancement
continue on the show timeline. The clip cadence is reanchored to the
authoritative global trigger frame, so fractional-rate frames consume the exact
global output sample count without losing clip-local sample zero. Unit oracles
cover all policies, the exact video cut, 59.94 fps cadence phase, selected-media
isolation, aggregate retention, and FTB after the Stinger mix. A required
FFmpeg PCM oracle proves byte-identical first-frame replay for early, delayed,
and negative-origin audio, including bounded pre-video repositioning and
sample-boundary phase preservation. A separate required
Metal/FFmpeg daemon acceptance records frequency-separated Program, Preview,
leading-clip, and trailing-clip signals through all three policies, two
replays, checkpoint restore, process restart, and a restored replay; it decodes
both recordings and verifies revisions and settled routing.

Item 5, item 6, and RC-007 remain planned.

FTB groundwork now includes a switcher-owned bounded automatic control core
alongside the compositor plan. The controller moves from its current exact
fixed-rational position to live or black over 1–3,600 frames, supports
no-jump reversal and idempotent repeated targets, and exposes each interval plus
exact trajectory progress without cumulative floating-point drift. It advances
orthogonally to automatic and manual Program/Preview transitions. The engine accepts the same
bounded in-memory intent, commits the desired endpoint immediately, advances a
separate realized trajectory on frame boundaries, exposes that exact interval
in each `FrameResult`, and permits Program transitions concurrently. Idle
snapshots reject partial FTB motion and validate identical settled desired and
realized endpoints. The compositor separately admits a bounded exact-rational
start, end, and progress plan, including hold and reverse trajectories. Its CPU
oracle and native wgpu path apply the plan after canonical RGBA16F Program
composition by mixing premultiplied linear RGBA toward opaque black, without
color conversion, audio work, or production readback.

The current protocol carries a bounded `FadeToBlack` command plus exact desired and
realized target/position state in snapshots, durable switcher events, and
runtime confirmations.
`fm-control` authorizes FTB as a transition, tracks it independently from
Program transitions, preserves monotonic runtime generation/sequence ordering
through overlap and reversal, and emits deterministic supersession. The client
and UI reducer require complete state on the exact current protocol and retain
exact desired and realized FTB projections by durable revision.

The current schema persists only settled live or black checkpoints and rejects
partial or desired/realized-divergent FTB state. Daemon and local CLI
checkpoint/restore paths preserve the exact
endpoint; daemon tests cover live-to-black and black-to-live commands across
engine reconstruction. The production native realizer now applies each engine
frame after Program scene/transition composition: video renders the exact FTB
interval endpoint into a final canonical RGBA16F target, while Master audio
ramps the inverse fixed-rational position across the same sample interval after
the complete Program mix. Native project planning accounts for that additional
in-flight target. Unit coverage exercises forward, reverse, held-black, and
post-Master behavior, and focused Metal tests validate both the compositor
oracle and the scene/transition/FTB ordering on a real adapter. The CLI now
exposes explicit local and remote `ftb ... <live|black> <frames>` commands,
settles and persists local moves, and prints separate exact desired and realized
target and position state. Web has the corresponding transport-free semantic
model, but no renderer or network runtime. Studio presents a dedicated native
panel with exact separate desired and realized
target/position labels, a separately bounded duration, and live/black actions
gated by Ready state, transition permission, and replicated
state. The desired target action is disabled while the opposite action remains
available for reversal. Its worker maps the typed intent to the current command,
preserves FIFO behavior, and a loopback protocol test
observes exact black then live desired and realized state. A hardware-gated
macOS daemon acceptance now sends current-protocol live-to-black and black-to-live
commands while Program recording is configured, decodes the H.264/AAC result,
requires ordered live/black/live video plus a sustained Master-audio silence
interval, and verifies the final live checkpoint. This proves the native FTB
path reaches one configured output on the exercised Metal/FFmpeg host. A
companion acceptance records a protocol-driven manual Fade at 75% forward,
25% reversed, cancelled, then fully progressed and committed; decoded video
must contain the corresponding ordered Program/Preview blend sequence, and the
checkpoint must be inactive with committed routing. Together these exercise
both halves of `SW-004` on one macOS recording output. Fullscreen presentation,
Windows/Linux adapters, and complete profile acceptance remain absent, so Phase
3 item 5 and `SW-002`/`SW-004` remain planned.

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
   The non-native scheduler gives established peers a bounded retryable shutdown
   notice before it returns an internal command error.
8. Implement remote browser control, WebRTC multiview preview, roles/pairing.
9. Add disk/network/GPU preflight and alert policy.
10. Run process-kill, disk-full, network impairment, and 24-hour soak. (needs to be in a VM or something else, don't kill my pc)
11. Publish the first supported hardware/capability matrix.

Current implementation boundary for item 1: the transport-independent
`fm-io-network` output state machine starts each configured destination on its
primary endpoint. A retryable primary connection or write failure retains the
bounded packet queue, keeps the existing retry budget and delay, and selects
the configured backup endpoint for subsequent attempts. After a reconnect, it
removes queued interframes before the first random-access packet and waits for
one when the queue has none. The first incoming random-access packet has queue
priority when that recovery queue is full only with interframes. This recovery
is transport-neutral queue policy; it does not request a keyframe. Backup
failures do not alternate back to the primary, while a manual stop and start
selects the primary again.
Non-retryable failures remain terminal. No socket adapter, RTMP/RTMPS or SRT
transport, runtime wiring, persistence, output-health UI, live decoder proof,
or live acceptance exists, so item 1 and `OR-005` remain planned.

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
The current persistence slice only audits stored media asset references; all item 11 mutation workflows remain planned.

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

## 11. Phase 9 — full parity and release certification (12–20 weeks)

1. Close remaining `P2` rows: presentation/DVD/import surfaces according
   to legal platform scope.
2. Add vMix HTTP/TCP/tally compatibility adapter.
3. Complete virtual sets, advanced title import, social moderation adapters.
4. Complete localization and accessibility audit.
5. Exercise every universal acceptance scenario on each applicable Tier-1 OS.
6. Run 72-hour release soak and disaster-recovery drills.
7. External security, broadcast-operator, and hardware conformance review.

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
