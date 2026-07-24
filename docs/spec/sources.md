# Sources and constraints

[Specification index](README.md)

## vMix parity baseline

The parity ledger is based on the public vMix 29 material available on
2026-07-23:

- [vMix live production feature overview](https://www.vmix.com/software/features.aspx)
- [vMix 29 user guide index](https://www.vmix.com/help29/)
- [Introduction and feature list](https://www.vmix.com/help29/Introduction.html)
- [vMix 29 user guide PDF](https://www.vmix.com/help29/vMixUserGuide.pdf)
- [vMix 29 release notes](https://www.vmix.com/Software/Download.aspx)
- [vMix 29 release announcement](https://blog.vmix.com/vmix-29-is-available-now/)
- [vMix edition comparison](https://www.vmix.com/purchase/default.aspx)
- [Supported hardware](https://www.vmix.com/software/supported-hardware.aspx)
- [Developer interfaces](https://www.vmix.com/help29/DeveloperInformation.html)
- [Presets](https://www.vmix.com/help29/PresetsMenu.html)
- [vMix Call](https://www.vmix.com/help29/VideoCall.html)
- [GT Title Designer](https://doc.vmix.com/products/vmix-gt-title-designer.aspx)
- [vMix Social](https://www.vmix.com/products/vmix-social.aspx)
- [Desktop Capture](https://www.vmix.com/software/vmix-desktop-capture.aspx)

The user guide index is the canonical checklist because it includes operational
features that do not appear on the marketing page. The compatibility ledger
must be regenerated against each new major vMix user guide before claiming
ongoing parity.

## Technical references

- [wgpu supported backends](https://docs.rs/wgpu/latest/wgpu/struct.Backends.html)
- [WebRTC specifications](https://www.w3.org/TR/webrtc/)
- [SRT protocol project](https://github.com/Haivision/srt)
- [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- [Semantic Versioning](https://semver.org/)

## Decision constraints

1. A cited library is a candidate, not a commitment to its latest version.
2. Vendor SDK access, redistributability, and platform support must be verified
   before scheduling its adapter.
3. Public protocol types must not expose FFmpeg, wgpu, OS, or vendor SDK types.
4. The project must retain a software fallback for essential format conversion,
   but not necessarily for every high-resolution encode workload.
5. The exact media stack choice is gated by Phase 0 benchmarks and licensing.
