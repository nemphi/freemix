# GPU and rendering

[Specification index](README.md)

## 1. Backend strategy

The portable compositor uses wgpu:

- D3D12 on Windows;
- Metal on macOS/iOS;
- Vulkan on Linux/Android;
- browser WebGPU only for client-side UI/preview helpers, not the authoritative
  production compositor.

OpenGL/GLES is a compatibility tier, not the professional baseline. Adapter
startup publishes feature/limit information and chooses a tested capability
profile rather than assuming backend equivalence.

## 2. GPU ownership

One `GpuContext` owns the production adapter, device, queue, caches, and memory
budget. The render thread records/submits work. Other components request
resources and enqueue declarative work; they do not submit independently.

Resource classes:

- persistent source and output textures;
- in-flight transient render targets from descriptor-keyed pools;
- immutable samplers, bind layouts, and pipelines;
- upload/readback staging rings;
- native imported/exported surfaces with explicit fences.

All allocations are budgeted. Cache eviction is fence-aware. The engine refuses
a graph plan whose guaranteed allocation exceeds the configured safe budget.

## 3. Render graph

For each output tick:

1. Wait only on required source readiness fences up to their deadline.
2. Import or upload selected frames.
3. Normalize sampling and color into the chosen working space.
4. Run source effects, keys, masks, and transforms.
5. Composite reusable scene inputs.
6. Execute preview/program transition.
7. Composite selected overlay channels.
8. Derive unique output sizes/colors/clean feeds.
9. Render multiview and scopes at their requested cadences.
10. Export textures to displays, hardware outputs, and encoders.
11. Signal release fences and emit GPU timing.

Independent outputs share intermediate textures. Scopes and thumbnails run at a
lower configurable cadence and cannot delay Program.

## 4. Shader library

Standard WGSL modules cover:

- scale/filter, crop, transform, perspective, opacity and blend;
- chroma key, luma key, key/fill, premultiply/unpremultiply;
- lift/gamma/gain, hue/saturation, levels, LUT, range and transfer conversion;
- sharpen, blur, deinterlace building blocks, masks, borders and shadows;
- cut, fade, wipe, slide, zoom, fly, cube, merge, AlphaFade, and stingers;
- text/image/shape composition and title animation;
- waveform, parade, vectorscope, histogram, false color, and pixel sampling.

`xtask shaders` validates WGSL, checks binding contracts, generates reflection
metadata, and compiles representative pipelines in CI. Shader packages declare
bounded resources and cannot access arbitrary buffers.

## 5. Color pipeline

Every frame carries explicit color metadata. Unknown metadata is resolved by
configurable format defaults and reported. The pipeline supports:

- limited/full range YUV and RGB;
- common 8/10/12/16-bit source formats;
- Rec.601, Rec.709, Display-P3, Rec.2020-class primaries;
- SDR gamma, sRGB, PQ, and HLG transfer functions;
- straight and premultiplied alpha;
- configurable linear-light or broadcast-compatible transition behavior;
- SDR/HDR tone mapping and gamut mapping per output.

The compositor uses a high-precision 4:4:4 working representation. The exact
canonical representation is linear-light Rec.2020 RGB with premultiplied alpha
in RGBA16F textures. Chroma is reconstructed before the working transform using
the source chroma siting and the configured quality filter. Composition and
transitions blend in linear light; a separately named legacy/broadcast
compatibility mode may reproduce non-linear behavior but is project-visible and
tested identically on every OS.

Output transforms apply gamut mapping, optional HDR-to-SDR/SDR-to-HDR tone
mapping, transfer function, range conversion, chroma subsampling, quantization,
and deterministic dither in that order. Mastering/CLL metadata is preserved or
regenerated according to the output policy. A GPU unable to provide RGBA16F
render/filter/blend semantics does not qualify for the production profile; it
may use a clearly labeled low-resolution compatibility profile, never silently
change compositor semantics.

Golden image tests use tolerance-aware comparisons in linear light plus
metadata assertions. Scopes are calculated from the signal presented at the
selected monitoring point.

## 6. Zero-copy interop

Portable wgpu alone is insufficient for all professional zero-copy paths.
Target-specific crates may use audited low-level wgpu/native API access:

| Platform | Intended paths |
|---|---|
| Windows | D3D12 shared resources/fences with Media Foundation, vendor capture, NVENC/QSV/AMF |
| macOS | IOSurface/CoreVideo pixel buffers with Metal, AVFoundation, VideoToolbox |
| Linux | DMA-BUF and explicit sync with Vulkan, PipeWire/V4L2, VA-API and vendor codecs |

The graph compiler chooses a memory domain end to end. If any node cannot
consume it, the plan inserts one explicit transfer and reports the reason.
Correct synchronization outranks a nominal “zero-copy” label.

## 7. GPU encode/decode

Codec discovery lists hardware implementation, formats, maximum resolution,
bit depth, chroma modes, reference-frame/GOP constraints, session limits,
external-memory compatibility, and measured warm-up. The scheduler may share
encodes but never silently changes a requested output profile to fit a hardware
session limit.

Software fallback is available for functional continuity at supported loads.
The UI predicts capacity from benchmarked cost and active sessions; it does not
hardcode one vendor-wide session count.

## 8. Device loss and fallback

On device loss:

1. Stop new GPU submissions and mark video outputs degraded.
2. Keep audio running if its clock remains valid.
3. Finalize or pause affected encoders safely.
4. Recreate adapter/device, caches, pools, imports, and execution plan once.
5. Preroll sources and resume on a clean boundary.
6. Require operator action after repeated loss.

A CPU compositor is a conformance/reference tool and emergency low-resolution
fallback, not the normal production path.
