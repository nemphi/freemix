# Testing and performance

[Specification index](README.md)

## 1. Quality model

Testing is layered:

1. Pure domain tests for commands, revisions, routing, and graph
   validation.
2. Deterministic simulated-clock tests for scheduling, transitions, playback,
   drift, and replay.
3. Signal tests with generated video/audio/timecode patterns.
4. Backend contract tests shared by every source, sink, codec, and GPU adapter.
5. Golden image/audio tests for composition, color, effects, and DSP.
6. Hardware-in-the-loop certification.
7. End-to-end operator scenarios.
8. Long-running soak, impairment, and recovery tests.

Every parity item links to at least one test ID and applicable capability
profile.

## 2. Reference performance profiles

These are acceptance workloads, not minimum hardware claims.

### Profile A: HD studio

- 1920x1080p60 Rec.709 output;
- eight live 1080p60 sources, four with embedded audio;
- two 4-layer scenes, eight available overlays, one animated title;
- Program, Preview, two multiviews;
- one high-quality program recording;
- one 1080p60 stream encode to two destinations sharing the rendition;
- four ISO recordings;
- native studio UI and one remote multiview.

Pass: zero Program audio discontinuities, no sustained A/V sync error above
20 ms, no output frame drops caused by FreeMix after warm-up, and bounded memory
over a 4-hour run on certified reference hardware.

### Profile B: UHD production

- 3840x2160p60 10-bit capable path;
- four UHD inputs plus four HD inputs;
- four-layer Program composition and animated keyed graphics;
- UHD Program record, UHD external output, HD stream rendition;
- scopes at reduced cadence.

Pass thresholds are certified per GPU/codec/device combination. The UI must
report if the chosen machine cannot meet the profiled budget before air.

### Profile C: replay

- eight synchronized 1080p60 camera records;
- two replay playback channels;
- four-angle quad view;
- simultaneous event creation, variable-speed playback, highlight export;
- separate storage groups.

Pass: recording remains continuous during playback/export; event marks remain
within one source frame of intended timestamps; storage backpressure is visible
before loss.

## 3. Latency budgets

For a directly captured 1080p60 camera to display on certified hardware, the
engine target is approximately two output frames excluding source and display
latency. Budget:

| Stage | Target |
|---|---:|
| Capture handoff and sync selection | <= 0.5 frame typical |
| GPU effects/composition | <= 0.5 frame GPU execution |
| Output queue/presentation | <= 1 frame typical |

Remote, browser, decode, deinterlace, frame-rate conversion, plugin, and encode
paths add explicit measured latency. Test results report median, p95, p99, and
max rather than one average.

Audio round-trip and video-to-audio sync are measured separately. Audio target
block size is adapter/capability dependent; the engine must remain stable under
the smallest certified buffer.

## 4. Deterministic media fixtures

Fixture generators produce:

- frame number, timestamp, cadence, color bars, ramps, gamut/range patches;
- alpha/key edges and premultiplication traps;
- interlaced moving wedges and field-order markers;
- HDR metadata and transfer ramps;
- per-channel audio impulses, sweeps, phase, silence, loudness, and drift;
- corrupt timestamps, missing frames, clock jumps, and discontinuities;
- replay camera slates with common event flash/audio pop.

Tests compare frame hashes only for bit-stable paths; GPU/color tests use
well-defined numerical tolerances and structural metrics.

## 5. Backend conformance suite

Every source adapter must pass enumerate/open/start/stop/reopen, format
negotiation, timestamps, signal loss/recovery, hot-plug, queue overflow, and
resource-leak tests.

Every sink must pass start, backpressure, drain, stop, restart, format rejection,
device loss, timestamp continuity, and error-reporting tests.

Every codec must pass capability truthfulness, flush, seek/discontinuity,
color/audio metadata, hardware session exhaustion, and software fallback tests.

## 6. Network impairment

Automated tests inject latency, jitter, reordering, duplication, packet loss,
bandwidth reduction, disconnect, DNS change, NAT/TURN, and certificate expiry.
They assert:

- control commands are idempotent and ordered;
- SRT/WebRTC/RTMP behavior matches configured recovery;
- previews degrade without affecting Program;
- guests preserve identity and mix-minus on reconnect; and
- stream/record sinks fail independently.

## 7. Reliability and soak

- 24-hour baseline show with periodic switching, title/data updates, and
  destination reconnects.
- 72-hour headless idle/on-air cycle for release candidates.
- Repeated 1,000 project loads/graph swaps in simulation.
- Repeated UI and plugin-host crashes while engine stays on air.
- GPU device-loss injection where backend permits.
- Crash injection before/after journal append, command acknowledgement,
  preparation, scheduling, and per-domain realization; irreversible actions
  never repeat.
- Multi-clock graph activation where one domain realizes late or fails.
- Local shared-handle preview resize, client death, daemon restart, lease
  timeout, and GPU-adapter mismatch.
- External-image import/export fence correctness under producer and consumer
  cancellation.
- Real-time allocation, blocking lock, syscall, and deadline detectors on audio
  and render paths.
- DSP/native plugin missed-deadline and crash tests with click-free bypass and
  correct delay compensation.
- Long-run reconciliation where desired revision and realized generations
  intentionally diverge, supersede, recover, and converge.
- Disk slow/full/unplug and process-kill during every recording container.
- Hot-plug loops for displays, audio, cameras, and controllers.
- Memory, handle, thread, GPU allocation, and journal growth remain bounded.

## 8. Performance tooling

Release builds include low-overhead counters for:

- per-node CPU time and queue depth;
- GPU pass time and memory pools;
- capture/decode/render/encode/sink timestamps;
- audio callback max time, underruns, drift correction;
- recorder throughput/fsync and projected capacity;
- network queue/loss/retransmit/RTT;
- UI command-to-accept and command-to-realize time.

Support traces are ring-buffered and can be frozen on an alert without doing
blocking work on media threads.

## 9. CI and release gates

- Formatting, lint, docs, dependency-policy, unsafe-audit, and license jobs.
- Foundation tests on every PR.
- Simulated engine and GPU reference tests on every PR.
- Native GPU/backend matrix nightly.
- Vendor hardware lab before release.
- Feature-powerset and minimal-build checks weekly.
- Exact-current project/protocol contract coverage.
- Installer, update, rollback, signing, and SBOM checks.
- No known P0/P1 parity loss, unexplained performance degradation over 5%,
  or untriaged soak failure at release.
