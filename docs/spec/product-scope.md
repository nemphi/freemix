# Product scope

[Specification index](README.md)

## 1. Users

| Persona | Primary needs |
|---|---|
| Technical director | Fast preview/program switching, transitions, overlays, confidence monitoring, undo, hardware controls |
| Audio operator | Low-latency buses, PFL/solo, metering, plugins, channel mapping, talkback, loudness control |
| Replay operator | Continuous ISO capture, marks, angle selection, jog/shuttle, playlists, highlights |
| Graphics operator | Titles, scoreboards, tickers, data binding, safe areas, keyed outputs |
| Remote producer | Secure control, multiview previews, chat, guest management, health telemetry |
| Solo creator | One-app setup, sensible defaults, scene/layer composition, streaming and recording |
| Integrator | Stable API, tally/activators, MIDI/OSC/GPIO, plugins, project templates, observability |
| Administrator | Headless operation, updates, certificates, user roles, audit trail, recovery |

## 2. Supported production classes

- Single-camera presenter, podcast, lecture, worship, and webinar.
- Multi-camera studio or outside-broadcast production.
- Sports production with ISO recording and slow-motion replay.
- Esports production with screen capture, data graphics, remote casters, and
  multiple delivery outputs.
- Remote and hybrid contribution with guest return video, mix-minus audio,
  talkback, and tally.
- Venue presentation with independent clean/program/confidence feeds.
- Vertical, square, portrait, and conventional broadcast canvases.

## 3. Deployment requirements

### 3.1 Native all-in-one

The studio app starts a supervised engine child process, pairs automatically
through an ephemeral local credential, and presents a unified operator
experience. The project remains on air if the studio UI restarts.

### 3.2 Headless engine

The engine runs as a console process or OS service. It exposes health/readiness,
the control API, WebRTC previews, metrics, and optional browser control. It must
not require a logged-in graphical session except for OS APIs that explicitly
need one, such as desktop capture.

### 3.3 Remote client

Native and web clients discover or connect to an engine, authenticate, download
a capability-filtered snapshot, subscribe to events, and request only the
preview feeds currently visible.

### 3.4 Distributed contribution

Companion capture nodes may publish camera, screen, or audio sources to an
engine. The authoritative switch/composite graph remains on one engine during
the initial parity program. Node clock offset, jitter, and link health are
visible to the operator.

## 4. Compatibility baseline

The reference target is the public vMix 29 Pro/Max feature surface, including
companion workflows such as vMix Call, Zoom integration, Desktop Capture, and
vMix Social. FreeMix does not reproduce vMix edition limits: supported features
are enabled by installed capabilities and product policy, not arbitrary input
counts.

Where vMix documentation disagrees, the current version-specific user guide,
release notes, and product comparison take precedence over older marketing
copy. The baseline currently expects five independent RTMP destinations, eight
overlay channels, eight stinger slots, and up to eight-camera replay.

## 5. Platform contract

Engine and studio targets:

- Windows 11 and supported Windows Server editions;
- current and previous two major macOS releases on Apple silicon;
- Tier-1 Linux distribution families using Wayland or X11 where necessary.

Control targets:

- the same desktop platforms;
- current Chromium, Firefox, and Safari browsers;
- installable PWA layouts for iPadOS and Android tablets;
- phone layouts for tally, shortcuts, title editing, and guest join.

Hardware and protocol support is capability-based. A project may declare
requirements such as `hardware.decklink.output.key_fill` or
`codec.h265.encoder.10bit`; loading reports missing capabilities before going
on air.

## 6. Functional requirements

The complete catalog is in [feature-parity.md](feature-parity.md). The system
must provide these cross-cutting behaviors:

1. Any visible input can be addressed by stable UUID, human name, or current
   ordinal.
2. Any operator action has a typed command and a corresponding state event.
3. State-changing commands carry an idempotency key and optional expected
   revision.
4. Live values such as faders can be coalesced without starving discrete
   commands such as Cut or Stop Recording.
5. Project mutations autosave to a journal without blocking the media graph.
6. Inputs and outputs expose health, format, latency, dropped frames, queue
   depth, and last error.
7. Resources are hot-pluggable where the underlying API allows it.
8. The application distinguishes configuration state, desired runtime state,
   and observed runtime state.
9. On-air operations that can interrupt all outputs require an explicit
   confirmation policy; automation clients can use preauthorized scopes.
10. A failed source displays a configured fallback while retaining its routing,
    title, and recovery settings.

## 7. Non-functional requirements

### Real time

- The compositor uses the output clock, not UI cadence.
- Audio callbacks must not allocate, lock a contended mutex, perform filesystem
  I/O, log synchronously, or wait on the async runtime.
- Queues are bounded and have an explicit late-frame/drop policy.
- The engine reports measured glass-to-glass components; it never promises a
  universal latency independent of capture and display hardware.

### Availability

- UI loss does not interrupt media.
- A source, encoder, plugin, or client failure is isolated when practical.
- Recorders use fragmented/fault-tolerant containers or periodic finalization.
- The project journal can restore the last acknowledged command sequence.

### Portability

- Core domain, protocol, project, graph planning, and most DSP contain no OS
  types.
- Platform SDK code lives behind capability-oriented adapter traits.
- OS/vendor features compile only in their owning crates.

### Operability

- Structured logs, metrics, traces, crash reports, and support bundles redact
  secrets.
- A statistics view exposes render time, GPU memory pressure, sync error,
  encode delay, network loss, disk latency, and thermal throttling where
  available.
- The engine exposes liveness and readiness separately.

### Accessibility and localization

- All critical commands are keyboard reachable.
- Colors are supplemented with labels/icons; tally colors are configurable.
- The browser client uses semantic DOM controls.
- User-facing text is localized through message catalogs, not embedded in
  domain code.

## 8. Parity policy

Each feature has:

- a stable ID;
- priority: `P0` foundation, `P1` professional core, `P2` full parity, or `P3`
  ecosystem enhancement;
- applicable platforms and required capability;
- an observable acceptance test; and
- a status in the compatibility ledger.

Equivalent behavior is acceptable when the original integration is proprietary.
For example, a general standards-based remote guest workflow does not by itself
satisfy a named Zoom integration; that row remains incomplete until an
authorized Zoom adapter ships. The same rule applies to NDI, OMT, VST3, capture
card SDKs, and virtual camera drivers.

## 9. Legal and licensing constraints

- FFmpeg configuration and redistribution must be reviewed per shipped codec,
  platform, and distribution model.
- NDI, Zoom, VST3, AJA, Blackmagic Design, Bluefish444, and similar integrations
  require separate SDK/license review and cannot leak vendor headers or runtime
  dependencies into portable crates.
- H.264, HEVC, AAC, MPEG-2, and other patent-encumbered formats require a
  distribution and territory assessment.
- Product branding, templates, media, and UI must be original.
- GPL components must not be accidentally linked into a distribution intended
  to use a permissive or commercial license.

## 10. Terminology

| Term | Meaning |
|---|---|
| Input | A source instance with runtime, video, audio, transform, and routing state |
| Source | An adapter that produces timed video, audio, or data |
| Mix | A preview/program switcher; the main mix produces Program |
| Layer | A composited visual node inside an input or scene |
| Overlay | An independently controlled keyed layer above a mix output |
| Bus | An independently mixed audio destination |
| Output | A timed video/audio rendition routed to display, hardware, network, or storage |
| Project | Declarative production configuration and durable editor state |
| Session | Runtime realization of a project, including transient health and clocks |
| Capability | A discovered feature with version, limits, and constraints |
