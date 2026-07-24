# Feature parity ledger

[Specification index](README.md)

This file is the functional contract. `P0` forms the first usable vertical
slice, `P1` is a professional release, `P2` completes the public vMix-class
surface, and `P3` extends beyond parity. “Done” always includes API control,
project persistence, capability reporting, telemetry, and documented failure
behavior.

## 1. Inputs and source management

| ID | Pri | Capability | Acceptance |
|---|---:|---|---|
| IN-001 | P0 | Local camera/capture inputs at SD, HD, UHD; common progressive and interlaced frame rates | Enumerate, hot-plug, configure format, acquire A/V, detect signal loss, and recover without changing input ID |
| IN-002 | P1 | Vendor capture adapters: Blackmagic, AJA, Magewell-class UVC, Bluefish444 where SDK permits | Adapter capability tests cover embedded audio, timecode, genlock status, and supported pixel formats |
| IN-003 | P0 | Video files and growing files | Seek, pause, resume, loop, trim, speed change, end triggers, audio track selection, and frame-accurate position |
| IN-004 | P1 | Common containers/codecs: MP4, MOV, MKV, MXF, MPEG-TS, AVI and licensed H.264/H.265/AAC/ProRes/DNx/MPEG variants | Golden files decode with timestamp, color, channel, rotation, and interlace metadata preserved |
| IN-005 | P1 | Audio files and multichannel audio-only devices | WAV/FLAC/MP3/AAC-class playback plus device channel selection and mapping |
| IN-006 | P1 | Media List input | Mixed audio/video/M3U list supports order, search, shuffle, Auto Next/Auto First, next/previous, loop, per-item marks, and active-item events |
| IN-007 | P2 | DVD/menu input where platform and licensing allow | Load title/menu, navigate, select audio/subtitles, and preserve aspect ratio |
| IN-008 | P0 | Still image and solid color/bars | Load common image formats, respect alpha/orientation/color profile, generate colors/bars without per-frame CPU upload |
| IN-009 | P1 | Photo/slideshow input | Folder/list, duration, transition, pan/zoom, random/ordered playback, live list edits, optional thumbnail suppression, and filename fallback for very large sets |
| IN-010 | P1 | Image sequence and stinger input | Decode alpha sequence/video, configure cut point, preload, and play deterministically |
| IN-011 | P1 | Local display/window/application capture with optional audio | Enumerate targets, crop, cursor toggle, and survive resize/target closure |
| IN-012 | P1 | Remote desktop capture agent | Pair, choose screen/window, receive A/V, report latency/loss, and provide tally |
| IN-013 | P1 | Browser input | Navigate, interact, zoom, transparent background, custom CSS, browser audio, HTML5 media, cookies/profile isolation, and refresh-on-activate |
| IN-014 | P2 | Presentation input | Import/render PowerPoint-compatible slides or use a documented PDF/image fallback; next/previous and notes metadata |
| IN-015 | P0 | IP stream input | RTSP/RTP/UDP/TCP/HTTP MPEG-TS and common HLS/HTTP inputs reconnect with bounded jitter buffering |
| IN-016 | P1 | RTMP input | Receive supported RTMP A/V and reconnect with status |
| IN-017 | P1 | SRT caller/listener/rendezvous | Both input and output modes support caller/listener/rendezvous, passphrases up to 256-bit, stream ID, latency, statistics, reconnect, H.264/HEVC up to UHD when capable, MPEG-TS/AAC-LC, and up to eight audio channels/split stereo pairs |
| IN-018 | P1 | NDI send/receive and discovery, including alpha where licensed | Discover, connect, low-bandwidth preview, metadata/tally, alpha, and isolated loss recovery |
| IN-019 | P1 | OMT send/receive | Open OMT adapter supports discovery, preview plus low/medium/high quality, alpha/audio, UHD, 120fps+ when capable, corrupt-data fault tolerance, send/receive, and direct replay/ISO record without re-encode when compatible |
| IN-020 | P0 | Scene/layer input | One background plus at least ten positioned foreground layers, each crop/transform/key/effect capable |
| IN-021 | P1 | Virtual set | Camera source, keyed talent, animated/zoomable set layers, reflection/shadow hooks, and presets |
| IN-022 | P1 | Video delay | Configurable live delay, loop/freeze, save clip, audio sync, and capacity estimate |
| IN-023 | P1 | Secondary Mix input | At least 15 addressable preview/program mini-mixes with cut/transitions, output-only mode, downstream routing with defined cycle timing, and Overlay/Stinger targeting to one or several Mixes |
| IN-024 | P1 | Program/preview-as-input | Safe graph-cycle detection, one-frame feedback behavior defined, and clean routing |
| IN-025 | P1 | Virtual inputs/aliases | Reuse a source with independent transform, effects, audio, name, shortcuts, and layer state |
| IN-026 | P0 | Input lifecycle | Add, duplicate, reorder, categorize, color-label, rename, pause, close, offline, replace source, and batch import |
| IN-027 | P1 | Per-input triggers | Ordered delayed actions on Transition In/Out, Overlay In/Out, Completion, Countdown Completed/Time/Remaining, Playback Time/Remaining, Call Connected/Disconnected, Zoom active-speaker/while-in-output/self, and Replay Events Completed |
| IN-028 | P1 | PTZ cameras | Network-controlled/UVC PTZ supports pan/tilt/zoom/focus/speed, presets as virtual inputs, joystick/mouse/shortcut operation; serial-only and additional ONVIF breadth are FreeMix extensions |
| IN-029 | P2 | Named Zoom meeting integration | Authorized SDK adapter joins Meetings/Events/Sessions, searches and assigns multiple participants, exposes camera/share/audio, switches camera/share, provides chat/manager/return feed, and emits active-speaker triggers |
| IN-030 | P2 | Legacy compatibility inputs | Windows-only plugin may import documented Flash/SWF/FLV and WPF/XAML-era assets; unsupported active content receives a safe migration report |

## 2. Switching, composition, and video processing

| ID | Pri | Capability | Acceptance |
|---|---:|---|---|
| SW-001 | P0 | Main preview/program switcher | Cut and timed transition are frame-boundary atomic; UI, API, keyboard, and controller stay consistent |
| SW-002 | P0 | Transition family | Cut, fade, zoom, wipes, slides, fly, fly-rotate, cube, cube-zoom, cross-zoom, vertical variants, merge, and AlphaFade; at least four favorite buttons are operator-assignable |
| SW-003 | P1 | Stingers | At least eight configurable stingers with alpha, cut point, audio, preload, and missing-media fallback |
| SW-004 | P0 | Fade to black and T-bar | FTB affects configured outputs; T-bar provides reversible manual transition progress |
| SW-005 | P0 | Eight overlay channels | Independent source, transition, duration, position/border preset, queue, on/off, and per-output inclusion |
| SW-006 | P0 | Layer designer | Direct manipulation plus numeric pan, zoom, crop, rotation, anchor, opacity, z-order, masks, and safe-area snapping |
| SW-007 | P0 | Video effects | Color adjustments, lift/gamma/gain, hue/saturation, white/black levels, sharpen, deinterlace, flip, crop, pan, zoom, rotate |
| SW-008 | P0 | Keying | Chroma/blue/green auto key, manual chroma controls, luma key, alpha, premultiplication, and key/fill input |
| SW-009 | P1 | 4:4:4 32-bit-class processing parity; canonical float FreeMix pipeline | Test patterns demonstrate no unintended range, transfer, alpha, or chroma conversion; floating-point working semantics are a FreeMix quality decision, not attributed to vMix |
| SW-010 | P1 | Color monitoring | RGB/Y waveform, RGB parade, vectorscope, split view, pixel sampler, and input/program selection |
| SW-011 | P0 | Two independent multiviews | Each selects Mix, layout/order, Program/Preview/inputs, labels, tally, clocks, chosen audio meters, inputs-only or single-input confidence mode, and click/touch-to-Preview |
| SW-012 | P1 | Four independent logical outputs and clean feeds | Outputs 1–4 independently select Program/Preview/input/Mix/MultiView and include chosen overlays/audio maps |
| SW-013 | P1 | Production clocks | Time-of-day, count up/down, remaining media time, stream/record/replay timers, timezone, and API exposure |
| SW-014 | P1 | Safe areas and aspect guides | SMPTE/action/title and custom guides appear only in operator views unless routed explicitly |
| SW-015 | P1 | Vertical and arbitrary canvas production | Parity test covers the vMix-style 1080x1920 vertical crop/safe-area/rotation workflow; independent arbitrary canvases and responsive scenes are FreeMix extensions |
| SW-016 | P1 | Snapshot and freeze/live-pause | Capture lossless still, freeze at frame boundary, resume without timestamp regression |
| SW-017 | P1 | Undo/redo and padlock | Editing commands are undoable; live transport actions follow explicit safety rules; locked inputs reject accidental edits |
| SW-018 | P1 | Flatten layers | Precompose a layered input at a deliberate graph boundary while preserving output timing and showing memory/quality cost |

## 3. Titles, graphics, data, and telestration

| ID | Pri | Capability | Acceptance |
|---|---:|---|---|
| GX-001 | P0 | Text/image/shape title templates | Editable fields, alpha, fonts, alignment, outline/shadow, auto-fit, per-field visibility, and reusable templates |
| GX-002 | P1 | Animated title designer | Keyframes, easing, groups, masks, tickers, transitions, clocks, and GPU-rendered playback |
| GX-003 | P1 | Scoreboards and tickers | Team/player fields, scores, clocks, increment/decrement shortcuts, crawl/roll, and external data mapping |
| GX-004 | P1 | Data sources | CSV, JSON, XML/XPath, RSS, text/file, HTTP/HTML tables, spreadsheet/Google Sheets-class adapters, Zoom chat, database adapter, polling/push, row selection, and transforms |
| GX-005 | P1 | Data-to-title mapping | Bind text, image URI, visibility, color, number/date format, and list row with preview/test state |
| GX-006 | P1 | Browser title/editor control | Authorized client can edit exposed fields, preview changes, take/update/off-air, and use optimistic concurrency |
| GX-007 | P1 | Telestrator | Low-latency program/input preview, simultaneous multi-device drawing, pen/line/arrow/shape/text, custom images, laser pointer, colors, undo/clear, transparent routed layer, and ten production shortcut buttons |
| GX-008 | P2 | Import pipeline | Layered PSD and documented graphic formats map to editable layers where legal/technically possible |
| GX-009 | P1 | Key/fill graphics output | Synchronized key and fill through supported hardware/network outputs |
| GX-010 | P2 | Social moderation companion | Authorized Facebook page comments, Twitch, YouTube Live chat, Bluesky, IRC, and Zoom chat adapters aggregate into a browser approval queue with title mapping, profile media/photos, rotation, and auto-update |

## 4. Audio

| ID | Pri | Capability | Acceptance |
|---|---:|---|---|
| AU-001 | P0 | Per-input mixer strip | Gain, fader, mute, follow-video, solo/PFL, pan/balance, delay, meter, bus sends, labels, and groups |
| AU-002 | P0 | Master plus seven auxiliary buses | Independent mixes, mute/fader/processing, named routing, device/network/record destinations, and mix-minus |
| AU-003 | P0 | Multichannel audio | Preserve channel count/layout, map matrix, stereo pairs/mono, downmix, embedded/de-embedded audio |
| AU-004 | P0 | Sample-rate conversion and sync | Stable 48 kHz engine clock by default, drift compensation, discontinuity handling, and per-source sync telemetry |
| AU-005 | P0 | Native DSP | Parametric EQ, high-pass, compressor/limiter, gate/expander, polarity, delay, and loudness/peak metering |
| AU-006 | P1 | VST3 plugins where licensed | Scan, validate, order, bypass, save state, compensate latency, expose editor, and quarantine crashes |
| AU-007 | P1 | Audio follow/auto-mixing | Configurable AFV and microphone automix with weight/priority and gain-sharing telemetry |
| AU-008 | P1 | Monitoring and talkback | PFL/solo bus, dim, selectable device, guest/camera talkback routes, and no accidental program injection |
| AU-009 | P1 | Fader/controller feedback | High-rate coalesced control and activator feedback without zipper noise |
| AU-010 | P1 | Recording audio flexibility | Select buses/channels per recorder, split WAV, multichannel track mapping, and metadata |
| AU-011 | P1 | Application/system audio capture and virtual audio device | Capture supported targets; publish program/buses to third-party apps through signed OS drivers where required |

## 5. Recording, replay, streaming, and outputs

| ID | Pri | Capability | Acceptance |
|---|---:|---|---|
| OR-001 | P0 | Program recording | MP4 plus a high-quality fault-tolerant 4:2:2 editing format equivalent to vMix AVI; optional Windows Media compatibility and FFmpeg MPEG-2/MP4/VC-3 profiles; hardware/software encode, segment, safe stop, duration/space status |
| OR-002 | P1 | Two independent program recorders | Different source, overlays, audio map, codec, resolution, and destination can run concurrently |
| OR-003 | P1 | Fault-tolerant recording | Crash/power-interrupted fixture yields a playable finalized or repairable file with bounded loss |
| OR-004 | P1 | MultiCorder/ISO | Simultaneously record selected raw camera/input feeds with embedded/mapped audio and shared or source timecode |
| OR-005 | P1 | Streaming destinations | RTMP/RTMPS and SRT output, service profiles, custom server/key, reconnect, backup URL, and per-destination health |
| OR-006 | P1 | Five simultaneous destinations and independent control | Start/stop/retry each destination independently or as a group without affecting recording |
| OR-007 | P1 | Multi-bitrate/ABR | Single-key and multi-key ladders, per-rendition output/audio/quality, and explicit compatibility rule with five-destination mode |
| OR-008 | P1 | Four physical external outputs | Outputs 1–4 route independently to certified AJA/Blackmagic/Bluefish-class devices with embedded audio, reference/genlock, and key/fill where supported |
| OR-009 | P1 | NDI/OMT/SRT outputs | Four independently routed outputs with discovery, alpha where available, and audio mapping; NDI/OMT can also publish every Camera/Call and audio input plus audio-only buses |
| OR-010 | P1 | Four virtual camera outputs and virtual audio | Four independently routed virtual video outputs plus audio-device publishing through signed Windows/macOS components and Linux PipeWire/V4L2-compatible paths |
| OR-011 | P0 | Two fullscreen/display outputs | Two independent borderless outputs on chosen displays with source, mode, scaling, HDR state, cursor suppression, and hot-plug recovery |
| OR-012 | P1 | Instant replay, up to eight cameras | Continuous synchronized capture up to 4K/240fps when capable, four-channel audio, per-camera storage roots, last 5/10/20 and In/Out marks, independent/linked A/B channels, angle switching, speed/jog/shuttle |
| OR-013 | P1 | Replay playlists/highlights | At least 20 named lists with unlimited events, multiple tags/folders, per-event angle/marks/speed, transitions/music, configured export audio, export during recording, and auto-return to live |
| OR-014 | P1 | Replay multiview/quad view and controllers | Synchronized high-quality four-angle Quad View, Replay MultiView, live/recorded modes, shortcut/activator surface, dual-controller profiles, and disk capacity/health |
| OR-015 | P1 | Playlist automation | Scheduled/relative start, duration, transition, loop, pause, next, active-state persistence, and API control |
| OR-016 | P2 | Direct network-source recording | Record an OMT/compatible compressed source into replay or ISO storage without re-encode when formats are safe and supported |
| OR-017 | P1 | LiveLAN | Access-controlled Web Controller page exposes a LAN HLS URL, reports per-viewer/capacity bandwidth, and validates the expected roughly ten-second latency class without affecting Program |

## 6. Remote guests, control, automation, and operations

| ID | Pri | Capability | Acceptance |
|---|---:|---|---|
| RC-001 | P1 | Up to eight encrypted browser guests | Host-a-call/connect-to-call, invite/password, direct/low-latency modes, camera/mic selection, dynamic bandwidth, return up to 1080p/4 Mbps, reconnect, lobby, remove, stats, and automatic NDI exposure |
| RC-002 | P1 | Guest audio/return/talkback | Full-duplex audio, automatic mix-minus, independently selectable Outputs 1–4 and Master/Headphones/A–G return audio, talkback, mute, gain, latency, and echo status |
| RC-003 | P1 | Guest manager and chat | Producer lobby, display names, device health, private/group messages, and permissioned controls |
| RC-004 | P0 | Keyboard and mouse shortcuts | Global/local scopes, chords, dynamic input/value targets, templates, conflict detection, and import/export |
| RC-005 | P1 | MIDI, OSC, HID/controller and jog-shuttle input | Buttons, faders, knobs, velocity/range mapping, learn mode, reconnect, and device profiles |
| RC-006 | P1 | Activators/tally feedback | Program/preview/overlay/record/stream/audio state drives lights, motor faders, OSC, network, and GPIO adapters |
| RC-007 | P1 | Web controller | Responsive shortcuts, switcher, tally, title editor, audio subset, replay subset, and role-based layouts |
| RC-008 | P0 | HTTP/WebSocket API | Version discovery, commands, snapshots, event subscriptions, idempotency, authentication, and rate limits |
| RC-009 | P2 | vMix API compatibility adapter | Full documented vMix 29 HTTP function/state XML plus persistent TCP command, tally, and activator subscription semantics map to stable FreeMix behavior; extensions are separately namespaced |
| RC-010 | P1 | Macros and scripts | Typed command sequences, conditions, timers, retries, variables, cancellation, permissions, and execution log |
| RC-011 | P1 | Sandboxed scripting | Wasm component or similarly isolated runtime; no ambient filesystem/network; declared capabilities |
| RC-012 | P1 | Project presets and bundles | Save/open/append, autosave, media relink, portable bundle, schema migration, templates, and recent projects |
| RC-013 | P1 | Import/export settings | Merge strategy, preview changes, conflict report, secrets excluded or separately encrypted |
| RC-014 | P1 | Tally and web monitor | Phone tally, low-latency preview, labels, full-screen mode, reconnect, and role-limited shortcuts |
| RC-015 | P1 | Statistics and alerts | FPS, drops, sync, queues, GPU/CPU/disk/network, input/output health, hardware encoder session inventory/capacity, thresholds, notifications, and support bundle |
| RC-016 | P2 | Localized UI | Extracted catalogs, runtime language switch where practical, locale-safe numbers/dates, and RTL-readiness |
| RC-017 | P1 | Per-input programmable GO | An input button runs a previewable ordered action list such as preview/take, play, overlay, data update, delay, and follow-up |

## 7. Universal acceptance scenarios

1. **Basic show:** add four cameras, two clips, a title, and a browser source;
   create a two-box scene; cut and transition; stream and record for one hour.
2. **Failure recovery:** disconnect an on-air camera, encoder network, display,
   and control client independently; configured fallback and remaining outputs
   continue, and each component recovers.
3. **Remote show:** eight guests join, receive mix-minus and return video, are
   switched and overlaid, while a remote producer controls through a browser.
4. **Sports show:** eight cameras record continuously; the replay operator marks,
   edits, angles, slows, and plays a highlight while live ISO capture continues.
5. **Portable project:** bundle a project, move it to another OS, resolve only
   explicitly platform-specific devices, and reproduce layout, routing, data,
   shortcuts, and media.
6. **Automation:** a controller changes preview, runs a stinger, updates a title,
   starts recording/streaming, receives tally, reconnects, and resumes from the
   last event revision without duplicate actions.
