# UI and operator workflows

[Specification index](README.md)

## 1. UI technology boundary

Use immediate mode where the screen is dense, live, and operator-driven:

- input tiles and categories;
- preview/program monitors and tally;
- transition controls and T-bar;
- audio mixer, meters, bus matrix;
- replay transport/timeline;
- multiview layout;
- scopes, statistics, shortcuts, and diagnostics.

Use retained/semantic form UI where accessibility and text interaction dominate:

- first-run setup;
- device/output configuration;
- users, roles, pairing, secrets;
- project/media management;
- title/data-source editors;
- browser and phone control.

The native app uses `egui` behind `fm-ui-egui`. The browser client is
Rust/Wasm with semantic DOM controls. Both consume `fm-ui-model`; neither owns
production state.

## 2. Main studio workspace

```text
┌──────────────── project / status / alerts / clocks ────────────────┐
│                         │                                          │
│       PREVIEW           │                 PROGRAM                  │
│                         │                                          │
├─────────────────────────┴──────────────────────────────────────────┤
│ Cut | 1 Fade | 2 Stinger | 3 Wipe | 4 Merge | T-bar | FTB         │
├────────────────────────────────────────────────────────────────────┤
│ Input categories/search                                            │
│ [Cam 1] [Cam 2] [Guest] [Scene] [Clip] [Title] [Browser] ...      │
├──────────────────────────────────────┬─────────────────────────────┤
│ Context panel: inputs/layers/replay  │ Audio mixer / data / stats  │
└──────────────────────────────────────┴─────────────────────────────┘
```

Panels are dockable and saved per client, not in the shared project. Critical
live status—Program, Preview, recording, streaming, replay record, FTB,
device/output failure—is always available even when panels are hidden.

Input tiles show:

- name, color/category, ordinal, thumbnail;
- program/preview/overlay tally;
- source format and signal state;
- audio meter/mute/follow;
- playhead and remaining time for media;
- loop/pause/GO state;
- warnings without covering essential picture.

## 3. Immediate-mode data flow

Each UI frame:

1. Drain ordered state events into the replicated read model.
2. Apply latest coalesced telemetry.
3. Reconcile or reject optimistic drafts by command ID.
4. Draw from an immutable client snapshot.
5. Translate interactions into typed intents.
6. Coalesce continuous values; send discrete commands immediately.
7. Never wait synchronously for engine response on the render thread.

Fader and T-bar movement appears locally at UI cadence, sends rate-limited
updates, and commits a final value on release. A rejected update snaps to
authoritative state with an explanation.

## 4. Core workflows

### 4.1 First launch

1. Detect GPU, displays, audio, capture, codec, and vendor capabilities.
2. Run a short render/encode/disk probe or skip with visible “unverified”
   status.
3. Choose a default production format.
4. Choose local-only or remote-enabled security posture.
5. Create a starter project with Program, Preview, Master, and one auxiliary
   bus.
6. Open the studio with a capability report link.

### 4.2 Add and configure an input

1. Press Add Input or drag media into the input area.
2. Choose a source category; show only discovered capabilities first.
3. Select source and format, with Auto as the default.
4. Preview signal and audio before committing.
5. Name/category the input and select audio-follow default.
6. Add transactionally; if activation fails, retain a configured offline input
   with actionable error.
7. Open context settings for color, key, layers, audio, triggers, tally, PTZ,
   and advanced timing.

Replacing the source uses the same flow but preserves input ID and downstream
settings.

### 4.3 Build a scene

1. Add a Scene input.
2. Select background and add up to ten foreground layers.
3. Drag/resize/rotate in the canvas or enter exact values.
4. Crop/mask/key each layer and set z-order.
5. Save layer presets and responsive variants for target aspect ratios.
6. Preview at full quality and inspect safe areas.
7. Take the scene through the normal switcher.

### 4.4 Go live

1. Preflight validates sources, outputs, disk, GPU budget, network targets,
   secrets, guest return routes, and missing media.
2. Operator resolves, explicitly waives, or assigns fallback for each blocker.
3. Start ISO/replay record if configured.
4. Start main recorder(s), then streaming destinations independently or as a
   group.
5. Show persistent destination health and elapsed time.
6. Require confirmation only for policy-defined dangerous stops or FTB—not for
   routine switching.

### 4.5 Switch and overlay

1. Select an input into Preview by tile, shortcut, controller, or API.
2. Inspect preview, scopes, and audio-follow effect.
3. Cut, transition, move T-bar, or invoke programmed GO.
4. Engine schedules the action on a frame boundary and emits accepted/realized
   status.
5. Overlay buttons take/update/off independently; output routing determines
   clean feeds.
6. Undo reverses eligible editing mistakes, not irreversible output history.

### 4.6 Remote guest

1. Create a guest slot and one-time invitation.
2. Guest grants devices, tests camera/mic/speaker, and enters lobby.
3. Producer verifies bandwidth, echo, name, framing, and permissions.
4. Assign return video and audio bus; mix-minus is default.
5. Admit guest; their input retains identity across reconnect.
6. Use private talkback/chat; remove or expire invitation after show.

### 4.7 Replay

1. Preflight cameras, common timing, audio source, storage roots, throughput,
   and estimated retention.
2. Start continuous recording and confirm each camera’s health.
3. Mark In/Out or create last-N-second event.
4. Add angles, tags, notes, and list/folder placement.
5. Cue event on Replay A or B; jog/shuttle and select angle/speed.
6. Take replay with configured transition, then auto-return to live.
7. Build/export highlights while record continues.

### 4.8 Recover a show

1. On restart, detect an unclean session and intact autosave journal.
2. Offer recovery summary with project revision and affected recordings.
3. Restore desired configuration with outputs stopped by default unless service
   policy explicitly authorizes automatic resume.
4. Repair/finalize recording segments without blocking project opening.
5. Re-resolve devices by stable selector and show substitutions before air.

## 5. Shortcuts and controllers

The shortcut editor uses intent names, not raw protocol strings. Learn mode
captures keyboard, MIDI, OSC, HID, or supported controller input. It shows
conflicts, local/global scope, value transforms, dynamic target, and activator
feedback.

Programmed GO is an ordered, cancellable command sequence attached to an input.
It may set Preview, transition, start playback, fire overlays, update data, and
schedule follow-up commands. The UI displays the resulting steps before
enabling it.

## 6. Safety and accessibility

- Red Program tally and green Preview tally are accompanied by text/border
  semantics.
- Stop record/stream actions have configurable confirmations and hardware
  double-press support.
- FTB remains visually distinct and keyboard focus cannot activate it
  accidentally.
- Keyboard focus order is deterministic; critical commands have screen-reader
  labels in semantic clients.
- Operators can enlarge hit targets, meters, labels, and use high-contrast and
  color-vision-safe themes.
- Touch layouts prevent scrolling gestures from changing live faders
  accidentally.
