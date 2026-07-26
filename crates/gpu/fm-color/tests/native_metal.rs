#![cfg(all(feature = "native-wgpu", target_os = "macos"))]

use std::{
    future::Future,
    num::NonZeroU128,
    pin::pin,
    sync::Arc,
    task::{Context, Poll, Wake, Waker},
    thread,
};

use fm_color::{
    ColorPipeline, LinearRgba, NativeImportNormalizer, NativeSdrOutputTransform, convert_primaries,
    srgb_from_linear, working_color_metadata, working_video_frame_metadata,
};
use fm_frame::{
    AlphaMode, ChromaLocation, ClockDomainId, ColorMetadata, ColorPrimaries, CpuVideoFrame,
    CpuVideoPayload, CpuVideoPlane, MatrixCoefficients, MediaTimestamp, MediaTiming,
    NormalizedDuration, NormalizedTimestamp, OriginalTimestamp, PixelFormat, SequenceNumber,
    SignalRange, TimeBase, TransferFunction, VideoDimensions, VideoFrameMetadata,
};
use fm_gpu::{
    NativeBackend, NativeContext, NativeTexture, ShaderDescriptor, ShaderLanguage, ShaderSource,
    ShaderStage, TextureFormat,
};
use half::f16;

const SYNTHETIC_WORKING_SHADER: &str = r"
@fragment
fn synthetic_working(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let pixel = vec2<u32>(position.xy);
    if pixel.y == 0u {
        if pixel.x == 0u {
            return vec4<f32>(1.0, 0.0, 0.0, 1.0);
        }
        if pixel.x == 1u {
            return vec4<f32>(2.0, 2.0, 2.0, 1.0);
        }
        return vec4<f32>(0.5, 0.0, 0.0, 0.5);
    }
    if pixel.x == 0u {
        return vec4<f32>(0.0, 1.0, 0.0, 1.0);
    }
    if pixel.x == 1u {
        return vec4<f32>(0.0, 0.0, 1.0, 1.0);
    }
    return vec4<f32>(0.1, 0.3, 0.2, 1.0);
}
";

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

fn timing() -> MediaTiming {
    MediaTiming::new(
        OriginalTimestamp::new(MediaTimestamp::new(42), TimeBase::new(1, 1_000).unwrap()),
        NormalizedTimestamp::from_nanos(42_000_000),
        NormalizedDuration::from_nanos(33_000_000).unwrap(),
        ClockDomainId::new(NonZeroU128::new(7).unwrap()),
        SequenceNumber::new(9),
    )
    .unwrap()
}

fn source_metadata(primaries: ColorPrimaries, transfer: TransferFunction) -> VideoFrameMetadata {
    VideoFrameMetadata::new(
        ColorMetadata {
            primaries,
            transfer,
            matrix: MatrixCoefficients::Identity,
            range: SignalRange::Full,
            chroma_location: ChromaLocation::Center,
        },
        Some(AlphaMode::Straight),
    )
}

fn frame(primaries: ColorPrimaries, transfer: TransferFunction, bytes: Vec<u8>) -> CpuVideoFrame {
    frame_with_layout(
        primaries,
        transfer,
        PixelFormat::Rgba8,
        VideoDimensions::new(4, 1).unwrap(),
        16,
        bytes,
    )
}

fn frame_with_layout(
    primaries: ColorPrimaries,
    transfer: TransferFunction,
    format: PixelFormat,
    dimensions: VideoDimensions,
    stride: usize,
    bytes: Vec<u8>,
) -> CpuVideoFrame {
    let payload = CpuVideoPayload::new(
        format,
        dimensions,
        vec![CpuVideoPlane::new(stride, bytes).unwrap()],
    )
    .unwrap();
    CpuVideoFrame::new(timing(), payload)
        .with_metadata(source_metadata(primaries, transfer))
        .unwrap()
}

async fn assert_import_matches_cpu(
    context: &NativeContext,
    normalizer: &NativeImportNormalizer,
    input: &CpuVideoFrame,
    label: &str,
) {
    let expected = ColorPipeline::new(input.metadata().unwrap().color(), working_color_metadata())
        .decode_cpu_payload(input.payload())
        .unwrap();
    let working = normalizer.normalize(context, input).await.unwrap();

    assert_eq!(working.timing(), input.timing());
    assert_eq!(working.metadata(), working_video_frame_metadata());
    assert_eq!(working.texture().format(), TextureFormat::Rgba16Float);
    let actual = context.readback(working.texture()).await.unwrap();
    assert_eq!(actual.width, input.payload().dimensions().width());
    assert_eq!(actual.height, input.payload().dimensions().height());
    assert_eq!(actual.stride, actual.width * 8);
    assert_eq!(actual.format, TextureFormat::Rgba16Float);

    for (pixel_index, (actual, expected)) in actual
        .bytes
        .chunks_exact(8)
        .zip(expected.frame.pixels())
        .enumerate()
    {
        let components = [expected.r, expected.g, expected.b, expected.a];
        for (component_index, (bytes, expected)) in
            actual.chunks_exact(2).zip(components).enumerate()
        {
            let value = f16::from_bits(u16::from_le_bytes([bytes[0], bytes[1]])).to_f32();
            let half_expected = f16::from_f32(expected).to_f32();
            assert!(
                (value - half_expected).abs() <= 0.001,
                "{label}, pixel {pixel_index} component {component_index}: GPU {value}, CPU half {half_expected}"
            );
        }
    }
}

fn fitted_source_pixel(
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
    target_x: u32,
    target_y: u32,
) -> Option<(u32, u32)> {
    let target_is_narrower = u64::from(target_width) * u64::from(source_height)
        <= u64::from(target_height) * u64::from(source_width);
    let (width, height) = if target_is_narrower {
        (
            target_width,
            u32::try_from(
                ((u64::from(target_width) * u64::from(source_height)) / u64::from(source_width))
                    .max(1),
            )
            .unwrap(),
        )
    } else {
        (
            u32::try_from(
                ((u64::from(target_height) * u64::from(source_width)) / u64::from(source_height))
                    .max(1),
            )
            .unwrap(),
            target_height,
        )
    };
    let offset_x = (target_width - width) / 2;
    let offset_y = (target_height - height) / 2;
    let end_x = offset_x.checked_add(width).unwrap();
    let end_y = offset_y.checked_add(height).unwrap();
    if target_x < offset_x || target_y < offset_y || target_x >= end_x || target_y >= end_y {
        return None;
    }
    Some((
        center_nearest(target_x - offset_x, source_width, width),
        center_nearest(target_y - offset_y, source_height, height),
    ))
}

fn center_nearest(coordinate: u32, source_size: u32, destination_size: u32) -> u32 {
    assert!(source_size > 0);
    assert!(destination_size > 0);
    assert!(coordinate < destination_size);

    let numerator = u128::from(coordinate)
        .checked_mul(2)
        .and_then(|value| value.checked_add(1))
        .and_then(|value| value.checked_mul(u128::from(source_size)))
        .unwrap();
    let denominator = u128::from(destination_size).checked_mul(2).unwrap();
    let bounded = (numerator / denominator).min(u128::from(source_size - 1));
    u32::try_from(bounded).unwrap()
}

fn half_pixel(pixel: LinearRgba) -> LinearRgba {
    LinearRgba::new(
        f16::from_f32(pixel.r).to_f32(),
        f16::from_f32(pixel.g).to_f32(),
        f16::from_f32(pixel.b).to_f32(),
        f16::from_f32(pixel.a).to_f32(),
    )
}

fn sdr_output_oracle(pixel: LinearRgba) -> [f32; 4] {
    let pixel = half_pixel(pixel);
    let rec709 = convert_primaries(pixel.rgb(), ColorPrimaries::Bt2020, ColorPrimaries::Bt709)
        .unwrap()
        .map(|linear| srgb_from_linear(linear.clamp(0.0, 1.0)) * 255.0);
    [rec709.r, rec709.g, rec709.b, 255.0]
}

async fn synthetic_working_texture(context: &NativeContext) -> NativeTexture {
    let pipeline = context
        .create_fullscreen_pipeline_for_format(
            ShaderDescriptor::new(
                "fm-color synthetic canonical working fixture",
                ShaderStage::Fragment,
                ShaderLanguage::Wgsl,
                "synthetic_working",
                ShaderSource::Text(SYNTHETIC_WORKING_SHADER.to_owned()),
            ),
            TextureFormat::Rgba16Float,
        )
        .await
        .unwrap();
    let unused = context
        .create_rgba16_float_render_target(3, 2)
        .await
        .unwrap();
    let working = context
        .create_rgba16_float_render_target(3, 2)
        .await
        .unwrap();
    context
        .submit_fullscreen(&pipeline, &unused, &unused, &working, &[0; 16])
        .await
        .unwrap();
    working
}

fn assert_pixel_close(actual: &[u8], expected: [f32; 4], location: &str) {
    for (component, (actual, expected)) in actual.iter().copied().zip(expected).enumerate() {
        assert!(
            (f32::from(actual) - expected).abs() <= 1.5,
            "{location} component {component}: GPU {actual}, CPU {expected}"
        );
    }
    assert_eq!(actual[3], 255, "{location} alpha");
}

#[test]
#[ignore = "requires a native macOS Metal adapter"]
fn native_metal_import_matches_cpu_reference_in_half_float() {
    block_on(async {
        let context = NativeContext::new([NativeBackend::Metal]).await.unwrap();
        let normalizer = NativeImportNormalizer::new(&context).await.unwrap();
        for primaries in [
            ColorPrimaries::Bt709,
            ColorPrimaries::DisplayP3,
            ColorPrimaries::Bt2020,
        ] {
            for transfer in [
                TransferFunction::Srgb,
                TransferFunction::Bt709,
                TransferFunction::Bt1886,
            ] {
                let input = frame(
                    primaries,
                    transfer,
                    vec![
                        255, 0, 0, 255, 64, 128, 192, 200, 5, 250, 90, 128, 255, 127, 63, 0,
                    ],
                );
                assert_import_matches_cpu(
                    &context,
                    &normalizer,
                    &input,
                    &format!("{primaries:?}/{transfer:?}"),
                )
                .await;
            }
        }

        let bgra = frame_with_layout(
            ColorPrimaries::DisplayP3,
            TransferFunction::Bt709,
            PixelFormat::Bgra8,
            VideoDimensions::new(2, 2).unwrap(),
            12,
            vec![
                3, 19, 241, 255, 229, 127, 11, 192, 91, 92, 93, 94, 207, 43, 17, 128, 31, 223, 101,
                64, 81, 82, 83, 84,
            ],
        );
        assert_import_matches_cpu(
            &context,
            &normalizer,
            &bgra,
            "padded BGRA Display-P3/BT.709",
        )
        .await;
    });
}

#[test]
#[ignore = "requires a native macOS Metal adapter"]
fn native_metal_sdr_output_matches_cpu_oracle_for_primaries_alpha_and_bars() {
    block_on(async {
        let context = NativeContext::new([NativeBackend::Metal]).await.unwrap();
        let normalizer = NativeImportNormalizer::new(&context).await.unwrap();
        let output_transform = NativeSdrOutputTransform::new(&context).await.unwrap();
        let input = frame(
            ColorPrimaries::Bt709,
            TransferFunction::Srgb,
            vec![
                255, 0, 0, 128, 0, 255, 0, 192, 0, 0, 255, 64, 255, 127, 63, 0,
            ],
        );
        let expected_working = ColorPipeline::new(
            source_metadata(ColorPrimaries::Bt709, TransferFunction::Srgb).color(),
            working_color_metadata(),
        )
        .decode_cpu_payload(input.payload())
        .unwrap();
        let half_alpha_red = sdr_output_oracle(expected_working.frame.pixel(0, 0).unwrap());
        assert!((half_alpha_red[0] - srgb_from_linear(128.0 / 255.0) * 255.0).abs() <= 0.1);
        assert!(half_alpha_red[1] <= 0.1);
        assert!(half_alpha_red[2] <= 0.1);
        let transparent = sdr_output_oracle(expected_working.frame.pixel(3, 0).unwrap());
        for (actual, expected) in transparent.into_iter().zip([0.0, 0.0, 0.0, 255.0]) {
            assert!((actual - expected).abs() <= f32::EPSILON);
        }
        let working = normalizer.normalize(&context, &input).await.unwrap();

        for (target_width, target_height) in [(6, 5), (10, 1), (2, 1)] {
            let target = context
                .create_rgba8_render_target(target_width, target_height)
                .await
                .unwrap();
            output_transform
                .transform(&context, working.texture(), &target)
                .await
                .unwrap();
            let actual = context.readback_rgba8(&target).await.unwrap();
            assert_eq!((actual.width, actual.height), (target_width, target_height));

            for target_y in 0..target_height {
                for target_x in 0..target_width {
                    let expected =
                        fitted_source_pixel(4, 1, target_width, target_height, target_x, target_y)
                            .map_or([0.0, 0.0, 0.0, 255.0], |(source_x, source_y)| {
                                sdr_output_oracle(
                                    expected_working.frame.pixel(source_x, source_y).unwrap(),
                                )
                            });
                    let offset = usize::try_from(target_y * actual.stride + target_x * 4).unwrap();
                    let pixel = &actual.rgba[offset..offset + 4];
                    for (component, (actual, expected)) in
                        pixel.iter().copied().zip(expected).enumerate()
                    {
                        assert!(
                            (f32::from(actual) - expected).abs() <= 1.5,
                            "{target_width}x{target_height} pixel ({target_x}, {target_y}) component {component}: GPU {actual}, CPU {expected}"
                        );
                    }
                    assert_eq!(pixel[3], 255);
                }
            }
        }
    });
}

#[test]
#[ignore = "requires a native macOS Metal adapter"]
fn native_metal_sdr_output_covers_canonical_wide_gamut_and_two_dimensional_fit() {
    block_on(async {
        let context = NativeContext::new([NativeBackend::Metal]).await.unwrap();
        let output_transform = NativeSdrOutputTransform::new(&context).await.unwrap();
        let working = synthetic_working_texture(&context).await;
        let canonical = [
            LinearRgba::new(1.0, 0.0, 0.0, 1.0),
            LinearRgba::new(2.0, 2.0, 2.0, 1.0),
            LinearRgba::new(0.5, 0.0, 0.0, 0.5),
            LinearRgba::new(0.0, 1.0, 0.0, 1.0),
            LinearRgba::new(0.0, 0.0, 1.0, 1.0),
            LinearRgba::new(0.1, 0.3, 0.2, 1.0),
        ];
        let expected = canonical.map(sdr_output_oracle);

        for (actual, expected) in expected[0].into_iter().zip([255.0, 0.0, 0.0, 255.0]) {
            assert!((actual - expected).abs() <= 0.001);
        }
        for (actual, expected) in expected[1].into_iter().zip([255.0, 255.0, 255.0, 255.0]) {
            assert!((actual - expected).abs() <= 0.001);
        }
        assert!(expected[2][0] > 230.0 && expected[2][0] < 240.0);
        for (actual, expected) in expected[2][1..].iter().zip([0.0, 0.0, 255.0]) {
            assert!((*actual - expected).abs() <= 0.001);
        }

        let native_size = context.create_rgba8_render_target(3, 2).await.unwrap();
        output_transform
            .transform(&context, &working, &native_size)
            .await
            .unwrap();
        let actual = context.readback_rgba8(&native_size).await.unwrap();
        for (index, expected) in expected.iter().copied().enumerate() {
            let offset = index * 4;
            assert_pixel_close(
                &actual.rgba[offset..offset + 4],
                expected,
                &format!("native-size pixel {index}"),
            );
        }

        let fitted = context.create_rgba8_render_target(7, 5).await.unwrap();
        output_transform
            .transform(&context, &working, &fitted)
            .await
            .unwrap();
        let actual = context.readback_rgba8(&fitted).await.unwrap();
        let expected_source_indices = [
            Some(0),
            Some(0),
            Some(1),
            Some(1),
            Some(1),
            Some(2),
            Some(2),
            Some(0),
            Some(0),
            Some(1),
            Some(1),
            Some(1),
            Some(2),
            Some(2),
            Some(3),
            Some(3),
            Some(4),
            Some(4),
            Some(4),
            Some(5),
            Some(5),
            Some(3),
            Some(3),
            Some(4),
            Some(4),
            Some(4),
            Some(5),
            Some(5),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        ];
        for (index, source_index) in expected_source_indices.into_iter().enumerate() {
            let expected = source_index.map_or([0.0, 0.0, 0.0, 255.0], |index| expected[index]);
            let offset = index * 4;
            assert_pixel_close(
                &actual.rgba[offset..offset + 4],
                expected,
                &format!("7x5 fitted pixel {index}"),
            );
        }
    });
}
