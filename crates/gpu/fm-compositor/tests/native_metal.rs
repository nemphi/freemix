#![cfg(all(feature = "native-wgpu", target_os = "macos"))]

use std::{
    future::Future,
    num::NonZeroU128,
    pin::pin,
    sync::Arc,
    task::{Context, Poll, Wake, Waker},
    thread,
};

use fm_color::{ColorPipeline, NativeImportNormalizer, working_color_metadata};
use fm_compositor::{
    CpuSourceFrame, CropRect, NativeCompositionRenderer, NativeSourceFrame,
    NativeTransitionRenderer, OutputTarget, RectMask, Rgba8, Rotation, Scene, SourceId,
    SourceLayer, Transform, TransitionKind, TransitionPlan, compile_scene, execute_cpu,
    execute_transition, image_from_cpu_frame,
};
use fm_frame::{
    AlphaMode, ChromaLocation, ClockDomainId, ColorMetadata, ColorPrimaries, CpuVideoFrame,
    CpuVideoPayload, CpuVideoPlane, MatrixCoefficients, MediaTimestamp, MediaTiming,
    NormalizedDuration, NormalizedTimestamp, OriginalTimestamp, PixelFormat, SequenceNumber,
    SignalRange, TimeBase, TransferFunction, VideoDimensions, VideoFrameMetadata,
};
use fm_gpu::{NativeBackend, NativeContext, NativeTexture, TextureFormat};
use fm_video::ImageFrame;
use half::f16;

struct ThreadWaker(thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::park(),
        }
    }
}

fn timing(sequence: u64) -> MediaTiming {
    MediaTiming::new(
        OriginalTimestamp::new(
            MediaTimestamp::new(i64::try_from(sequence).unwrap()),
            TimeBase::new(1, 1_000).unwrap(),
        ),
        NormalizedTimestamp::from_nanos(i64::try_from(sequence).unwrap() * 1_000_000),
        NormalizedDuration::from_nanos(1_000_000).unwrap(),
        ClockDomainId::new(NonZeroU128::new(11).unwrap()),
        SequenceNumber::new(sequence),
    )
    .unwrap()
}

fn source_metadata() -> VideoFrameMetadata {
    VideoFrameMetadata::new(
        ColorMetadata {
            primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Srgb,
            matrix: MatrixCoefficients::Identity,
            range: SignalRange::Full,
            chroma_location: ChromaLocation::Center,
        },
        Some(AlphaMode::Straight),
    )
}

fn frame(sequence: u64, pixels: &[u8; 16]) -> CpuVideoFrame {
    sized_frame(sequence, 2, 2, pixels)
}

fn sized_frame(sequence: u64, width: u32, height: u32, pixels: &[u8]) -> CpuVideoFrame {
    let payload = CpuVideoPayload::new(
        PixelFormat::Rgba8,
        VideoDimensions::new(width, height).unwrap(),
        vec![CpuVideoPlane::new(width as usize * 4, pixels.to_vec()).unwrap()],
    )
    .unwrap();
    CpuVideoFrame::new(timing(sequence), payload)
        .with_metadata(source_metadata())
        .unwrap()
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn canonical_cpu_image(source: &CpuVideoFrame) -> ImageFrame {
    let pipeline = ColorPipeline::new(source_metadata().color(), working_color_metadata());
    let decoded = pipeline.decode_cpu_payload(source.payload()).unwrap().frame;
    let bytes = decoded
        .pixels()
        .iter()
        .flat_map(|pixel| {
            [pixel.r, pixel.g, pixel.b, pixel.a]
                .map(|component| (component.clamp(0.0, 1.0) * 255.0).round() as u8)
        })
        .collect::<Vec<_>>();
    ImageFrame::new(
        decoded.width(),
        decoded.height(),
        decoded.width() as usize * 4,
        bytes,
    )
    .unwrap()
}

fn assert_rgba16f_matches_cpu(output: &fm_gpu::NativeTextureReadback, expected: &ImageFrame) {
    assert_eq!(output.format, TextureFormat::Rgba16Float);
    assert_eq!(
        (output.width, output.height),
        (expected.width(), expected.height())
    );
    assert_eq!(output.stride, expected.width() * 8);
    for (pixel_index, (actual, expected)) in output
        .bytes
        .chunks_exact(8)
        .zip(expected.pixels().chunks_exact(4))
        .enumerate()
    {
        for (component_index, (actual, expected)) in actual
            .chunks_exact(2)
            .zip(expected.iter().copied())
            .enumerate()
        {
            let actual = f16::from_bits(u16::from_le_bytes([actual[0], actual[1]])).to_f32();
            let expected = f32::from(expected) / 255.0;
            // CPU composition rounds each u8 multiply while native blending uses
            // float/half arithmetic, so parity is intentionally tolerance-based.
            assert!(
                (actual - expected).abs() <= 0.006,
                "pixel {pixel_index}, component {component_index}: GPU {actual}, CPU {expected}"
            );
        }
    }
}

async fn assert_composition_case(
    context: &NativeContext,
    normalizer: &NativeImportNormalizer,
    renderer: &NativeCompositionRenderer,
    scene: &Scene,
    inputs: &[(SourceId, &CpuVideoFrame)],
    cpu_images: &[(SourceId, &ImageFrame)],
) {
    let (plan, _) = compile_scene(scene, OutputTarget::Program).unwrap();
    let mut working = Vec::with_capacity(inputs.len());
    for (_, source) in inputs {
        working.push(normalizer.normalize(context, source).await.unwrap());
    }
    let native_sources = inputs
        .iter()
        .zip(&working)
        .map(|((source, _), frame)| NativeSourceFrame::new(*source, frame.texture()))
        .collect::<Vec<_>>();
    let cpu_sources = cpu_images
        .iter()
        .map(|(source, frame)| CpuSourceFrame::new(*source, frame))
        .collect::<Vec<_>>();
    let expected = execute_cpu(&plan, &cpu_sources).unwrap();
    let output = renderer
        .render(context, &plan, &native_sources)
        .await
        .unwrap();
    assert_eq!(output.format(), TextureFormat::Rgba16Float);
    let readback = context.readback(&output).await.unwrap();
    assert_rgba16f_matches_cpu(&readback, &expected);
}

fn expected_component(
    from: f32,
    to: f32,
    kind: TransitionKind,
    numerator: u32,
    denominator: u32,
) -> f32 {
    if kind == TransitionKind::Cut || numerator == denominator {
        to
    } else if numerator == 0 {
        from
    } else {
        let progress = f32::from(u16::try_from(numerator).unwrap())
            / f32::from(u16::try_from(denominator).unwrap());
        (to - from).mul_add(progress, from)
    }
}

#[test]
#[ignore = "requires a native macOS Metal adapter"]
fn native_metal_cut_and_fade_match_cpu_linear_frames() {
    block_on(async {
        let from_cpu = frame(
            1,
            &[
                0, 0, 0, 255, 255, 255, 255, 255, 96, 96, 96, 128, 255, 1, 127, 0,
            ],
        );
        let to_cpu = frame(
            2,
            &[
                255, 255, 255, 255, 0, 0, 0, 255, 192, 192, 192, 64, 5, 250, 90, 0,
            ],
        );
        let cpu_pipeline = ColorPipeline::new(source_metadata().color(), working_color_metadata());
        let from_linear = cpu_pipeline
            .decode_cpu_payload(from_cpu.payload())
            .unwrap()
            .frame;
        let to_linear = cpu_pipeline
            .decode_cpu_payload(to_cpu.payload())
            .unwrap()
            .frame;

        let context = NativeContext::new([NativeBackend::Metal]).await.unwrap();
        let normalizer = NativeImportNormalizer::new(&context).await.unwrap();
        let from = normalizer.normalize(&context, &from_cpu).await.unwrap();
        let to = normalizer.normalize(&context, &to_cpu).await.unwrap();
        let renderer = NativeTransitionRenderer::new(&context).await.unwrap();
        let from_half = context.readback(from.texture()).await.unwrap();
        let to_half = context.readback(to.texture()).await.unwrap();

        let cases = [
            (TransitionKind::Cut, 0, 1),
            (TransitionKind::Fade, 0, 4),
            (TransitionKind::Fade, 1, 4),
            (TransitionKind::Fade, 4, 4),
        ];
        for (kind, numerator, denominator) in cases {
            let plan = TransitionPlan::compile(kind, numerator, denominator).unwrap();
            let output = renderer
                .render(&context, plan, from.texture(), to.texture())
                .await
                .unwrap();
            assert_eq!(output.format(), TextureFormat::Rgba16Float);
            let actual = context.readback(&output).await.unwrap();
            assert_eq!((actual.width, actual.height, actual.stride), (2, 2, 16));
            for (pixel_index, ((bytes, from), to)) in actual
                .bytes
                .chunks_exact(8)
                .zip(from_linear.pixels())
                .zip(to_linear.pixels())
                .enumerate()
            {
                let from = [from.r, from.g, from.b, from.a];
                let to = [to.r, to.g, to.b, to.a];
                for (component_index, ((bytes, from), to)) in
                    bytes.chunks_exact(2).zip(from).zip(to).enumerate()
                {
                    let actual_bits = u16::from_le_bytes([bytes[0], bytes[1]]);
                    let actual = f16::from_bits(actual_bits).to_f32();
                    let expected = expected_component(from, to, kind, numerator, denominator);
                    let half_expected = f16::from_f32(expected);
                    if kind == TransitionKind::Cut || numerator == 0 || numerator == denominator {
                        let endpoint = if kind == TransitionKind::Cut || numerator == denominator {
                            &to_half.bytes
                        } else {
                            &from_half.bytes
                        };
                        let offset = pixel_index * 8 + component_index * 2;
                        assert_eq!(
                            actual_bits,
                            u16::from_le_bytes([endpoint[offset], endpoint[offset + 1]]),
                            "{kind:?} {numerator}/{denominator}, pixel {pixel_index}, component {component_index} endpoint"
                        );
                    }
                    assert!(
                        (actual - half_expected.to_f32()).abs() <= 0.001,
                        "{kind:?} {numerator}/{denominator}, pixel {pixel_index}, component {component_index}: GPU {actual}, CPU half {}",
                        half_expected.to_f32()
                    );
                }
            }
        }
    });
}

#[test]
#[ignore = "requires a native macOS Metal adapter"]
fn native_metal_wipe_matches_cpu_at_odd_width_boundaries() {
    block_on(async {
        let from_cpu = sized_frame(
            3,
            5,
            1,
            &[
                0, 0, 0, 255, 32, 32, 32, 255, 64, 64, 64, 255, 96, 96, 96, 255, 128, 128, 128, 255,
            ],
        );
        let to_cpu = sized_frame(
            4,
            5,
            1,
            &[
                255, 255, 255, 255, 224, 224, 224, 255, 192, 192, 192, 255, 160, 160, 160, 255,
                144, 144, 144, 255,
            ],
        );
        let from_image = canonical_cpu_image(&from_cpu);
        let to_image = canonical_cpu_image(&to_cpu);

        let context = NativeContext::new([NativeBackend::Metal]).await.unwrap();
        let normalizer = NativeImportNormalizer::new(&context).await.unwrap();
        let from = normalizer.normalize(&context, &from_cpu).await.unwrap();
        let to = normalizer.normalize(&context, &to_cpu).await.unwrap();
        let renderer = NativeTransitionRenderer::new(&context).await.unwrap();
        let from_half = context.readback(from.texture()).await.unwrap();
        let to_half = context.readback(to.texture()).await.unwrap();

        for (numerator, denominator) in [(0, 2), (1, 3), (1, 2), (2, 2)] {
            let plan =
                TransitionPlan::compile(TransitionKind::Wipe, numerator, denominator).unwrap();
            let expected = execute_transition(plan, &from_image, &to_image).unwrap();
            let output = renderer
                .render(&context, plan, from.texture(), to.texture())
                .await
                .unwrap();
            let actual = context.readback(&output).await.unwrap();
            assert_rgba16f_matches_cpu(&actual, &expected);

            let boundary = 5 * numerator / denominator;
            for (pixel, actual) in actual.bytes.chunks_exact(8).enumerate() {
                let endpoint = if u32::try_from(pixel).unwrap() < boundary {
                    &to_half
                } else {
                    &from_half
                };
                assert_eq!(
                    actual,
                    &endpoint.bytes[pixel * 8..pixel * 8 + 8],
                    "Wipe {numerator}/{denominator}, pixel {pixel} endpoint"
                );
            }
        }
    });
}

#[test]
#[ignore = "requires a native macOS Metal adapter"]
#[allow(clippy::too_many_lines)]
fn native_metal_composition_matches_cpu_geometry_and_blending() {
    block_on(async {
        let context = NativeContext::new([NativeBackend::Metal]).await.unwrap();
        let normalizer = NativeImportNormalizer::new(&context).await.unwrap();
        let renderer = NativeCompositionRenderer::new(&context).await.unwrap();
        let black = Rgba8::new(0, 0, 0, 255);

        let geometry_source = sized_frame(
            10,
            3,
            2,
            &[
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 0, 0, 0, 255, 255, 255, 255, 255,
                255, 255, 0, 255,
            ],
        );
        let geometry_cpu = canonical_cpu_image(&geometry_source);
        let mut geometry_scene = Scene::new(4, 4, black).unwrap();
        geometry_scene.push_layer(
            SourceLayer::new(
                SourceId::new(1),
                0,
                Transform::new(-1, 1, 4, 4, Rotation::Deg0),
            )
            .with_crop(CropRect::new(1, 0, 2, 2))
            .with_alpha_mode(AlphaMode::Premultiplied),
        );
        assert_composition_case(
            &context,
            &normalizer,
            &renderer,
            &geometry_scene,
            &[(SourceId::new(1), &geometry_source)],
            &[(SourceId::new(1), &geometry_cpu)],
        )
        .await;

        let rotation_source = sized_frame(
            11,
            2,
            3,
            &[
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 0, 0, 0, 255, 255, 255, 255, 255,
                255, 255, 0, 255,
            ],
        );
        let rotation_cpu = canonical_cpu_image(&rotation_source);
        for rotation in [
            Rotation::Deg0,
            Rotation::Deg90,
            Rotation::Deg180,
            Rotation::Deg270,
        ] {
            let mut scene = Scene::new(3, 3, black).unwrap();
            scene.push_layer(
                SourceLayer::new(SourceId::new(2), 0, Transform::new(0, 0, 2, 3, rotation))
                    .with_alpha_mode(AlphaMode::Premultiplied),
            );
            assert_composition_case(
                &context,
                &normalizer,
                &renderer,
                &scene,
                &[(SourceId::new(2), &rotation_source)],
                &[(SourceId::new(2), &rotation_cpu)],
            )
            .await;
        }

        let straight_source = sized_frame(12, 2, 1, &[255, 255, 255, 128, 0, 0, 0, 64]);
        let straight_cpu = image_from_cpu_frame(&straight_source).unwrap();
        let mut straight_scene = Scene::new(3, 1, black).unwrap();
        straight_scene.push_layer(
            SourceLayer::new(
                SourceId::new(3),
                0,
                Transform::new(1, 0, 2, 1, Rotation::Deg0),
            )
            .with_opacity(128),
        );
        assert_composition_case(
            &context,
            &normalizer,
            &renderer,
            &straight_scene,
            &[(SourceId::new(3), &straight_source)],
            &[(SourceId::new(3), &straight_cpu)],
        )
        .await;

        let red = sized_frame(13, 1, 1, &[255, 0, 0, 255]);
        let green = sized_frame(14, 1, 1, &[0, 255, 0, 255]);
        let blue = sized_frame(15, 1, 1, &[0, 0, 255, 255]);
        let red_cpu = canonical_cpu_image(&red);
        let green_cpu = canonical_cpu_image(&green);
        let blue_cpu = canonical_cpu_image(&blue);
        let transform = Transform::new(0, 0, 1, 1, Rotation::Deg0);
        let mut order_scene = Scene::new(1, 1, black).unwrap();
        order_scene.push_layer(
            SourceLayer::new(SourceId::new(4), 5, transform)
                .with_opacity(128)
                .with_alpha_mode(AlphaMode::Premultiplied),
        );
        order_scene.push_layer(
            SourceLayer::new(SourceId::new(5), 0, transform)
                .with_opacity(128)
                .with_alpha_mode(AlphaMode::Premultiplied),
        );
        order_scene.push_layer(
            SourceLayer::new(SourceId::new(6), 5, transform)
                .with_opacity(128)
                .with_alpha_mode(AlphaMode::Premultiplied),
        );
        assert_composition_case(
            &context,
            &normalizer,
            &renderer,
            &order_scene,
            &[
                (SourceId::new(4), &red),
                (SourceId::new(5), &blue),
                (SourceId::new(6), &green),
            ],
            &[
                (SourceId::new(4), &red_cpu),
                (SourceId::new(5), &blue_cpu),
                (SourceId::new(6), &green_cpu),
            ],
        )
        .await;
    });
}

#[test]
#[ignore = "requires a native macOS Metal adapter"]
#[allow(clippy::too_many_lines)]
fn native_metal_rect_masks_match_cpu_edges_transforms_and_layers() {
    block_on(async {
        let context = NativeContext::new([NativeBackend::Metal]).await.unwrap();
        let normalizer = NativeImportNormalizer::new(&context).await.unwrap();
        let renderer = NativeCompositionRenderer::new(&context).await.unwrap();
        let black = Rgba8::new(0, 0, 0, 255);
        let source_id = SourceId::new(20);
        let source = sized_frame(
            20,
            4,
            2,
            &[
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255, 128, 0, 0, 255,
                0, 128, 0, 255, 0, 0, 128, 255, 128, 128, 128, 255,
            ],
        );
        let cpu = canonical_cpu_image(&source);

        for (mask, transform, crop, opacity) in [
            (
                RectMask::new(0, 0, 4, 2),
                Transform::new(0, 0, 4, 2, Rotation::Deg0),
                None,
                255,
            ),
            (
                RectMask::new(4, 0, 1, 2),
                Transform::new(0, 0, 4, 2, Rotation::Deg0),
                None,
                255,
            ),
        ] {
            let mut scene = Scene::new(5, 6, black).unwrap();
            let mut layer = SourceLayer::new(source_id, 0, transform)
                .with_mask(mask)
                .with_opacity(opacity)
                .with_alpha_mode(AlphaMode::Premultiplied);
            if let Some(crop) = crop {
                layer = layer.with_crop(crop);
            }
            scene.push_layer(layer);
            assert_composition_case(
                &context,
                &normalizer,
                &renderer,
                &scene,
                &[(source_id, &source)],
                &[(source_id, &cpu)],
            )
            .await;
        }

        for rotation in [
            Rotation::Deg0,
            Rotation::Deg90,
            Rotation::Deg180,
            Rotation::Deg270,
        ] {
            let mut scene = Scene::new(7, 7, black).unwrap();
            scene.push_layer(
                SourceLayer::new(source_id, 0, Transform::new(-1, 1, 6, 4, rotation))
                    .with_crop(CropRect::new(1, 0, 3, 2))
                    .with_mask(RectMask::new(1, 0, 2, 2).inverted(true))
                    .with_opacity(128)
                    .with_alpha_mode(AlphaMode::Premultiplied),
            );
            assert_composition_case(
                &context,
                &normalizer,
                &renderer,
                &scene,
                &[(source_id, &source)],
                &[(source_id, &cpu)],
            )
            .await;
        }

        let mut layered = Scene::new(5, 3, black).unwrap();
        layered.push_layer(
            SourceLayer::new(source_id, 0, Transform::new(0, 0, 4, 2, Rotation::Deg0))
                .with_mask(RectMask::new(0, 0, 2, 2))
                .with_opacity(128)
                .with_alpha_mode(AlphaMode::Premultiplied),
        );
        layered.push_layer(
            SourceLayer::new(source_id, 1, Transform::new(1, 1, 4, 2, Rotation::Deg180))
                .with_mask(RectMask::new(2, 0, 2, 2))
                .with_opacity(192)
                .with_alpha_mode(AlphaMode::Premultiplied),
        );
        assert_composition_case(
            &context,
            &normalizer,
            &renderer,
            &layered,
            &[(source_id, &source)],
            &[(source_id, &cpu)],
        )
        .await;
    });
}

#[test]
#[ignore = "requires a native macOS Metal adapter"]
fn native_metal_empty_composition_clears_to_background() {
    block_on(async {
        let context = NativeContext::new([NativeBackend::Metal]).await.unwrap();
        let renderer = NativeCompositionRenderer::new(&context).await.unwrap();
        let scene = Scene::new(2, 2, Rgba8::new(32, 16, 8, 128)).unwrap();
        let (plan, _) = compile_scene(&scene, OutputTarget::Program).unwrap();
        let expected = execute_cpu(&plan, &[]).unwrap();
        let output: NativeTexture = renderer.render(&context, &plan, &[]).await.unwrap();
        let readback = context.readback(&output).await.unwrap();
        assert_rgba16f_matches_cpu(&readback, &expected);
    });
}
