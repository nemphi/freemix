use fm_color::{
    AlphaMode, ColorError, ColorPipeline, LinearFrame, LinearRgba, Lut1D, Lut3D, LutError,
    MatrixError, Rgb, ToneMapPolicy, TransferError, Yuv, bt709_from_linear, bt709_to_linear,
    convert_primaries, decode_transfer, encode_transfer, hlg_from_linear, hlg_to_linear,
    pq_from_linear, pq_to_linear, rgb_to_yuv, srgb_from_linear, srgb_to_linear, tone_map_rgb,
    working_color_metadata, working_video_frame_metadata, yuv_to_rgb,
};
use fm_frame::{CpuVideoPayload, CpuVideoPlane};
use fm_types::{
    AlphaMode as CanonicalAlphaMode, ChromaLocation, ColorMetadata, ColorPrimaries,
    MatrixCoefficients, PixelFormat, SignalRange, TransferFunction, VideoDimensions,
};
use fm_video::{ColorMatrix, ColorRange, ImageFrame, Rgba8, rgb_to_yuv as fixed_rgb_to_yuv};

const EPSILON: f32 = 2.0e-5;

fn close(actual: f32, expected: f32, tolerance: f32) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "actual {actual}, expected {expected}, tolerance {tolerance}"
    );
}

fn close_rgb(actual: Rgb, expected: Rgb, tolerance: f32) {
    close(actual.r, expected.r, tolerance);
    close(actual.g, expected.g, tolerance);
    close(actual.b, expected.b, tolerance);
}

fn metadata(
    primaries: ColorPrimaries,
    transfer: TransferFunction,
    matrix: MatrixCoefficients,
    range: SignalRange,
) -> ColorMetadata {
    ColorMetadata {
        primaries,
        transfer,
        matrix,
        range,
        chroma_location: ChromaLocation::Center,
    }
}

fn image(pixels: &[[u8; 4]]) -> ImageFrame {
    let bytes = pixels.iter().flatten().copied().collect();
    ImageFrame::new(
        u32::try_from(pixels.len()).unwrap(),
        1,
        pixels.len() * 4,
        bytes,
    )
    .unwrap()
}

#[test]
fn sdr_transfer_standard_vectors_and_ramps() {
    close(srgb_to_linear(0.04045), 0.003_130_8, 1.0e-7);
    close(srgb_to_linear(0.5), 0.214_041_14, 1.0e-7);
    close(srgb_from_linear(0.214_041_14), 0.5, 1.0e-7);
    close(bt709_to_linear(0.081), 0.018, 2.0e-5);
    close(bt709_from_linear(0.18), 0.409_007_73, 1.0e-7);

    for transfer in [
        TransferFunction::Linear,
        TransferFunction::Srgb,
        TransferFunction::Bt709,
        TransferFunction::Bt1886,
    ] {
        for step in 0_u8..=100 {
            let linear = f32::from(step) / 100.0;
            let encoded = encode_transfer(transfer, linear).unwrap();
            close(decode_transfer(transfer, encoded).unwrap(), linear, 3.0e-6);
        }
    }
}

#[test]
fn hdr_transfer_representation_is_explicit_and_round_trips() {
    // ST 2084 code 0.508078 is approximately 100 cd/m2, or 0.01 of 10,000.
    close(pq_to_linear(0.508_078_4), 0.01, 2.0e-6);
    close(pq_from_linear(0.01), 0.508_078_4, 2.0e-6);
    close(hlg_to_linear(0.5), 1.0 / 12.0, 1.0e-7);
    close(hlg_from_linear(1.0 / 12.0), 0.5, 1.0e-7);

    for transfer in [TransferFunction::Pq, TransferFunction::Hlg] {
        for sample in [0.0, 0.01, 0.1, 0.25, 0.5, 0.75, 1.0] {
            let linear = decode_transfer(transfer, sample).unwrap();
            close(encode_transfer(transfer, linear).unwrap(), sample, 8.0e-6);
        }
    }
    assert_eq!(
        decode_transfer(TransferFunction::Pq, f32::NAN),
        Err(TransferError::NonFiniteSample)
    );
}

#[test]
fn matrix_black_white_and_primary_vectors_cover_all_standards_and_ranges() {
    for matrix in [
        MatrixCoefficients::Bt601,
        MatrixCoefficients::Bt709,
        MatrixCoefficients::Bt2020NonConstant,
    ] {
        close_rgb(
            yuv_to_rgb(Yuv::new(0.0, 0.5, 0.5), matrix, SignalRange::Full).unwrap(),
            Rgb::new(0.0, 0.0, 0.0),
            EPSILON,
        );
        close_rgb(
            yuv_to_rgb(Yuv::new(1.0, 0.5, 0.5), matrix, SignalRange::Full).unwrap(),
            Rgb::new(1.0, 1.0, 1.0),
            EPSILON,
        );
        close_rgb(
            yuv_to_rgb(
                Yuv::new(16.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0),
                matrix,
                SignalRange::Limited,
            )
            .unwrap(),
            Rgb::new(0.0, 0.0, 0.0),
            EPSILON,
        );
        close_rgb(
            yuv_to_rgb(
                Yuv::new(235.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0),
                matrix,
                SignalRange::Limited,
            )
            .unwrap(),
            Rgb::new(1.0, 1.0, 1.0),
            EPSILON,
        );

        for range in [SignalRange::Full, SignalRange::Limited] {
            for primary in [
                Rgb::new(1.0, 0.0, 0.0),
                Rgb::new(0.0, 1.0, 0.0),
                Rgb::new(0.0, 0.0, 1.0),
            ] {
                let signal = rgb_to_yuv(primary, matrix, range).unwrap();
                close_rgb(yuv_to_rgb(signal, matrix, range).unwrap(), primary, EPSILON);
            }
        }
    }

    let red_601 = rgb_to_yuv(
        Rgb::new(1.0, 0.0, 0.0),
        MatrixCoefficients::Bt601,
        SignalRange::Full,
    )
    .unwrap();
    close(red_601.y, 0.299, EPSILON);
    close(red_601.u, 0.331_264_1, EPSILON);
    close(red_601.v, 1.0, EPSILON);
}

#[test]
fn float_matrix_matches_fm_video_fixed_point_with_byte_tolerance() {
    let colors = [
        Rgba8::new(0, 0, 0, 255),
        Rgba8::new(255, 255, 255, 255),
        Rgba8::new(255, 0, 0, 255),
        Rgba8::new(0, 255, 0, 255),
        Rgba8::new(0, 0, 255, 255),
        Rgba8::new(17, 91, 203, 255),
    ];
    for (fixed_matrix, float_matrix) in [
        (ColorMatrix::Bt601, MatrixCoefficients::Bt601),
        (ColorMatrix::Bt709, MatrixCoefficients::Bt709),
    ] {
        for (fixed_range, float_range) in [
            (ColorRange::Full, SignalRange::Full),
            (ColorRange::Limited, SignalRange::Limited),
        ] {
            for color in colors {
                let fixed = fixed_rgb_to_yuv(color, fixed_matrix, fixed_range);
                let float = rgb_to_yuv(
                    Rgb::new(
                        f32::from(color.r) / 255.0,
                        f32::from(color.g) / 255.0,
                        f32::from(color.b) / 255.0,
                    ),
                    float_matrix,
                    float_range,
                )
                .unwrap();
                close(float.y * 255.0, f32::from(fixed.y), 1.1);
                close(float.u * 255.0, f32::from(fixed.u), 1.1);
                close(float.v * 255.0, f32::from(fixed.v), 1.1);
            }
        }
    }
}

#[test]
fn primary_conversion_round_trips_neutral_and_ramp_vectors() {
    for source in [
        ColorPrimaries::Bt601,
        ColorPrimaries::Bt709,
        ColorPrimaries::Bt2020,
        ColorPrimaries::DisplayP3,
    ] {
        for sample in [
            Rgb::new(0.0, 0.0, 0.0),
            Rgb::new(1.0, 1.0, 1.0),
            Rgb::new(1.0, 0.0, 0.0),
            Rgb::new(0.0, 1.0, 0.0),
            Rgb::new(0.0, 0.0, 1.0),
            Rgb::new(0.1, 0.5, 0.9),
        ] {
            let working = convert_primaries(sample, source, ColorPrimaries::Bt2020).unwrap();
            let restored = convert_primaries(working, ColorPrimaries::Bt2020, source).unwrap();
            close_rgb(restored, sample, 4.0e-5);
        }
    }
}

#[test]
fn pipeline_uses_linear_light_before_premultiplication_and_traps_zero_alpha() {
    let color = metadata(
        ColorPrimaries::Bt2020,
        TransferFunction::Srgb,
        MatrixCoefficients::Identity,
        SignalRange::Full,
    );
    let mut pipeline = ColorPipeline::new(color, color);
    let straight = image(&[[188, 0, 0, 128], [255, 127, 63, 0]]);
    let decoded = pipeline.decode_image(&straight).unwrap();
    let first = decoded.frame.pixel(0, 0).unwrap();
    let expected = srgb_to_linear(188.0 / 255.0) * (128.0 / 255.0);
    close(first.r, expected, 1.0e-6);
    close(first.a, 128.0 / 255.0, 1.0e-7);
    assert_eq!(decoded.frame.pixel(1, 0).unwrap(), LinearRgba::default());

    pipeline.source_alpha = AlphaMode::Premultiplied;
    let hidden_color = pipeline
        .decode_image(&image(&[[255, 255, 255, 0]]))
        .unwrap();
    assert_eq!(
        hidden_color.frame.pixel(0, 0).unwrap(),
        LinearRgba::default()
    );

    let premultiplied = pipeline.decode_image(&image(&[[94, 0, 0, 128]])).unwrap();
    close(premultiplied.frame.pixel(0, 0).unwrap().r, expected, 0.004);
}

#[test]
fn pipeline_round_trips_vectors_and_propagates_metadata() {
    let source = metadata(
        ColorPrimaries::Bt709,
        TransferFunction::Srgb,
        MatrixCoefficients::Identity,
        SignalRange::Full,
    );
    let output = metadata(
        ColorPrimaries::Bt2020,
        TransferFunction::Bt1886,
        MatrixCoefficients::Identity,
        SignalRange::Limited,
    );
    let input = image(&[
        [0, 0, 0, 255],
        [255, 255, 255, 255],
        [255, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [64, 128, 192, 255],
    ]);
    let pipeline = ColorPipeline::new(source, output);
    let decoded = pipeline.decode_image(&input).unwrap();
    assert_eq!(decoded.metadata, working_color_metadata());
    assert_eq!(decoded.frame.width(), 6);
    let converted = pipeline.convert_image(&input).unwrap();
    assert_eq!(converted.metadata, output);
    assert_eq!(
        converted.image.pixel(0, 0),
        Some(Rgba8::new(16, 16, 16, 255))
    );
    assert_eq!(
        converted.image.pixel(1, 0),
        Some(Rgba8::new(235, 235, 235, 255))
    );
}

#[test]
fn cpu_rgba_and_limited_nv12_frames_decode_with_metadata() {
    let dimensions = VideoDimensions::new(2, 2).unwrap();
    let rgba = CpuVideoPayload::new(
        PixelFormat::Rgba8,
        dimensions,
        vec![
            CpuVideoPlane::new(
                8,
                vec![
                    255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
                ],
            )
            .unwrap(),
        ],
    )
    .unwrap();
    let rgb_metadata = metadata(
        ColorPrimaries::Bt2020,
        TransferFunction::Linear,
        MatrixCoefficients::Identity,
        SignalRange::Full,
    );
    let decoded = ColorPipeline::new(rgb_metadata, rgb_metadata)
        .decode_cpu_payload(&rgba)
        .unwrap();
    assert_eq!(
        decoded.frame.pixel(0, 0),
        Some(LinearRgba::new(1.0, 0.0, 0.0, 1.0))
    );

    let nv12 = CpuVideoPayload::new(
        PixelFormat::Nv12,
        dimensions,
        vec![
            CpuVideoPlane::new(2, vec![16; 4]).unwrap(),
            CpuVideoPlane::new(2, vec![128, 128]).unwrap(),
        ],
    )
    .unwrap();
    let yuv_metadata = metadata(
        ColorPrimaries::Bt709,
        TransferFunction::Linear,
        MatrixCoefficients::Bt709,
        SignalRange::Limited,
    );
    let decoded = ColorPipeline::new(yuv_metadata, rgb_metadata)
        .decode_cpu_payload(&nv12)
        .unwrap();
    for pixel in decoded.frame.pixels() {
        close(pixel.r, 0.0, EPSILON);
        close(pixel.g, 0.0, EPSILON);
        close(pixel.b, 0.0, EPSILON);
        close(pixel.a, 1.0, EPSILON);
    }
}

#[test]
fn lut_validation_and_sampling_are_deterministic() {
    let identity = Lut1D::new(
        vec![Rgb::new(0.0, 0.0, 0.0), Rgb::new(1.0, 1.0, 1.0)],
        Rgb::new(0.0, 0.0, 0.0),
        Rgb::new(1.0, 1.0, 1.0),
    )
    .unwrap();
    close_rgb(
        identity.sample(Rgb::new(0.25, 0.5, 0.75)),
        Rgb::new(0.25, 0.5, 0.75),
        EPSILON,
    );
    assert_eq!(
        Lut1D::new(
            vec![Rgb::default()],
            Rgb::default(),
            Rgb::new(1.0, 1.0, 1.0)
        ),
        Err(LutError::TooFewEntries)
    );
    assert_eq!(
        Lut1D::new(
            vec![Rgb::default(), Rgb::new(f32::NAN, 1.0, 1.0)],
            Rgb::default(),
            Rgb::new(1.0, 1.0, 1.0)
        ),
        Err(LutError::NonFiniteValue)
    );

    let mut cube = Vec::new();
    for blue in 0_u8..2 {
        for green in 0_u8..2 {
            for red in 0_u8..2 {
                cube.push(Rgb::new(f32::from(red), f32::from(green), f32::from(blue)));
            }
        }
    }
    let identity_cube = Lut3D::new(2, cube, Rgb::default(), Rgb::new(1.0, 1.0, 1.0)).unwrap();
    close_rgb(
        identity_cube.sample(Rgb::new(0.2, 0.4, 0.8)),
        Rgb::new(0.2, 0.4, 0.8),
        EPSILON,
    );
    close_rgb(
        identity_cube.sample(Rgb::new(-1.0, 0.5, 2.0)),
        Rgb::new(0.0, 0.5, 1.0),
        EPSILON,
    );
    assert_eq!(
        Lut3D::new(
            2,
            vec![Rgb::default(); 7],
            Rgb::default(),
            Rgb::new(1.0, 1.0, 1.0)
        ),
        Err(LutError::SizeMismatch {
            expected: 8,
            actual: 7
        })
    );
}

#[test]
fn tone_map_policy_is_luminance_preserving_and_validated() {
    let mapped = tone_map_rgb(
        Rgb::new(10.0, 10.0, 10.0),
        ToneMapPolicy::Reinhard {
            source_peak_nits: 1000.0,
            target_peak_nits: 100.0,
        },
    )
    .unwrap();
    close_rgb(mapped, Rgb::new(1.0, 1.0, 1.0), EPSILON);

    let colored = tone_map_rgb(
        Rgb::new(8.0, 4.0, 2.0),
        ToneMapPolicy::Reinhard {
            source_peak_nits: 1000.0,
            target_peak_nits: 100.0,
        },
    )
    .unwrap();
    close(colored.r / colored.g, 2.0, EPSILON);
    close(colored.g / colored.b, 2.0, EPSILON);
    assert!(
        tone_map_rgb(
            Rgb::new(1.0, 1.0, 1.0),
            ToneMapPolicy::Reinhard {
                source_peak_nits: 0.0,
                target_peak_nits: 100.0,
            }
        )
        .is_err()
    );
}

#[test]
fn unsupported_yuv_identity_metadata_is_an_explicit_error() {
    assert_eq!(
        yuv_to_rgb(
            Yuv::new(0.5, 0.5, 0.5),
            MatrixCoefficients::Identity,
            SignalRange::Full
        ),
        Err(MatrixError::UnsupportedMatrix(MatrixCoefficients::Identity))
    );

    let dimensions = VideoDimensions::new(2, 2).unwrap();
    let nv12 = CpuVideoPayload::new(
        PixelFormat::Nv12,
        dimensions,
        vec![
            CpuVideoPlane::new(2, vec![0; 4]).unwrap(),
            CpuVideoPlane::new(2, vec![128; 2]).unwrap(),
        ],
    )
    .unwrap();
    let unsupported = metadata(
        ColorPrimaries::Bt709,
        TransferFunction::Srgb,
        MatrixCoefficients::Identity,
        SignalRange::Full,
    );
    assert!(matches!(
        ColorPipeline::new(unsupported, unsupported).decode_cpu_payload(&nv12),
        Err(ColorError::Matrix(MatrixError::UnsupportedMatrix(
            MatrixCoefficients::Identity
        )))
    ));
}

#[test]
fn invalid_working_alpha_representation_is_rejected() {
    assert!(matches!(
        LinearFrame::new(1, 1, vec![LinearRgba::new(0.1, 0.0, 0.0, 0.0)]),
        Err(ColorError::InvalidPremultipliedPixel)
    ));
}

#[test]
fn alpha_mode_is_canonical_and_working_metadata_is_exact() {
    let canonical: CanonicalAlphaMode = AlphaMode::Premultiplied;
    assert_eq!(canonical, CanonicalAlphaMode::Premultiplied);

    let metadata = working_video_frame_metadata();
    assert_eq!(metadata.color(), working_color_metadata());
    assert_eq!(metadata.color().primaries, ColorPrimaries::Bt2020);
    assert_eq!(metadata.color().transfer, TransferFunction::Linear);
    assert_eq!(metadata.color().matrix, MatrixCoefficients::Identity);
    assert_eq!(metadata.color().range, SignalRange::Full);
    assert_eq!(metadata.color().chroma_location, ChromaLocation::Center);
    assert_eq!(metadata.alpha_mode(), Some(AlphaMode::Premultiplied));
    assert_eq!(metadata.validate_for(PixelFormat::Rgba16Float), Ok(()));
}
