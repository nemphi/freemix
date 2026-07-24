# Platform portability

[Specification index](README.md)

## 1. Support tiers

| Tier | Targets | Promise |
|---|---|---|
| Production engine Tier 1 | Windows x86-64, macOS arm64, Linux x86-64 | Full portable core plus tested platform capabilities |
| Production engine Tier 2 | Windows arm64, Linux arm64 | Core builds; certified hardware matrix grows by test coverage |
| Native studio Tier 1 | Windows, macOS, Linux | Full operator UI and local preview |
| Remote control | Current desktop/mobile browsers | Switcher, titles, tally, audio subset, replay subset, administration by role |
| Capture node | Windows, macOS, Linux | Screen/camera/audio contribution subject to OS permissions |
| Mobile | iOS/iPadOS/Android PWA or native shell | Control/guest/capture profiles, not desktop hardware parity |

“Cross-platform” means the same project, state model, protocol, compositor
semantics, and portable feature set. It does not imply that an SDI card with a
Windows-only driver works on Linux.

## 2. Platform matrix

| Area | Windows | macOS | Linux |
|---|---|---|---|
| GPU | D3D12 baseline, Vulkan optional | Metal baseline | Vulkan baseline |
| Camera/capture | Media Foundation; vendor SDKs; UVC | AVFoundation/CoreMediaIO; vendor SDKs | V4L2/PipeWire; vendor SDKs |
| Audio | WASAPI; optional ASIO adapter | CoreAudio | PipeWire/ALSA; optional JACK |
| Screen/window | Windows Graphics Capture | ScreenCaptureKit | PipeWire portal; X11 fallback |
| Hardware codecs | Media Foundation plus NVENC/QSV/AMF | VideoToolbox | VA-API plus NVENC/vendor |
| Display | Native window/DXGI | Core Animation/Metal | Wayland preferred; X11 fallback |
| Virtual video/audio | Signed driver/plugin | System extension/device plugin | PipeWire/V4L2 loopback where available |
| Service | Windows Service | launchd | systemd |
| Packaging | MSIX/signed installer | Signed/notarized app bundle | Flatpak/AppImage and distro packages as support permits |

When the headless engine runs outside the logged-in desktop session,
`freemix-capture-node` runs inside that user session to obtain Windows session,
macOS TCC, or Linux portal permissions. It advertises only user-approved
resources and publishes timed media through authenticated local IPC. Service
mode is not certified for screen/window/application-audio capture until this
broker passes clock, reconnect, logout, fast-user-switch, and permission tests.

## 3. Adapter contracts

Platform adapters implement discovery before open. They report:

- stable and session device IDs;
- display name and provider;
- supported formats/rates/channels/color;
- clock/timestamp quality;
- memory domains and zero-copy paths;
- exclusivity and hot-plug behavior;
- permission state and remediation;
- driver/SDK version and known limitations.

Project selectors prefer stable vendor/device identifiers but include fallback
matching rules. Silent substitution is forbidden for on-air devices.

## 4. Vendor and proprietary integrations

Each vendor adapter is a separate optional crate and distributable component.
It owns:

- SDK/header discovery;
- version compatibility;
- native library loading;
- license notice and installer feature;
- capability translation;
- unsafe FFI and callback lifecycle;
- simulator or recorded fixture for CI.

NDI, Zoom, VST3, AJA, Blackmagic, Bluefish444, and similar integrations ship
only after legal and redistribution approval. OMT remains an adapter even when
its implementation is open source, preserving the same dependency direction.

Legacy vMix surfaces such as SWF/Flash, WPF/XAML titles, DirectShow-only devices,
Windows Media output, and VB.NET scripts are compatibility/import plugins, not
portable core requirements. Their parity row can be satisfied by an isolated
Windows compatibility component plus a documented migration path.

## 5. Codec distribution profiles

Define explicit product builds:

- `community`: redistributable codecs and protocols only;
- `creator`: platform-native codecs plus approved FFmpeg components;
- `broadcast`: approved professional/vendor adapters;
- `developer`: simulated adapters and full diagnostics.

The binary reports build-time and runtime codec capabilities. A project never
assumes that a format is available merely because the source code contains an
adapter.

## 6. Permissions

Camera, microphone, screen recording, local network, accessibility/global-key,
and virtual-device permissions vary by OS. The engine:

1. detects `unknown`, `denied`, `restricted`, or `granted`;
2. requests only in an interactive app context;
3. exposes remediation instructions to remote clients;
4. continues headless operation for already authorized services; and
5. never loops permission prompts.

## 7. Build and packaging

- CI cross-checks workspace metadata on all targets; release builds run on their
  native OS.
- SDK-backed crates build only in licensed runners.
- Installers bundle exact redistributable native libraries and generate a
  third-party notice/SBOM.
- Artifacts are code-signed; macOS is notarized; update manifests are signed.
- GPU shaders and protocol schemas are reproducible build inputs.
- Crash symbols are uploaded separately and keyed by build ID.
- Updates stage next to the active version and support rollback of binaries;
  project migrations are never silently rolled back.

## 8. Capability certification

Support claims are generated from a hardware lab database:

```text
OS + version
GPU + driver
capture/output device + driver/firmware
codec and resolution/frame rate
source count and output mix
test duration
latency, drops, sync, thermal and recovery result
```

The UI links each active adapter to its certification state: certified,
community-tested, detected-unverified, or unsupported.
