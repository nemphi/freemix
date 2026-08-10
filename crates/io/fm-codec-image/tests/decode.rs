use std::num::NonZeroU128;

use fm_codec_image::{
    DecodedStill, StillDecodeError, StillDecodeLimits, StillFormat, decode_still,
    sniff_still_format,
};
use fm_frame::{
    AlphaMode, ChromaLocation, ClockDomainId, ColorMetadata, ColorPrimaries, MatrixCoefficients,
    MediaTimestamp, MediaTiming, NormalizedDuration, NormalizedTimestamp, OriginalTimestamp,
    PixelFormat, SequenceNumber, SignalRange, TimeBase, TransferFunction, VideoFrameMetadata,
};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::webp::WebPEncoder;
use image::{ExtendedColorType, ImageEncoder};

fn timing() -> MediaTiming {
    let time_base = TimeBase::new(1, 90_000).unwrap();
    MediaTiming::new(
        OriginalTimestamp::new(MediaTimestamp::new(12_345), time_base),
        NormalizedTimestamp::from_nanos(987_654_321),
        NormalizedDuration::from_nanos(33_366_700).unwrap(),
        ClockDomainId::new(NonZeroU128::new(17).unwrap()),
        SequenceNumber::new(42),
    )
    .unwrap()
}

fn png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::new();
    PngEncoder::new(&mut encoded)
        .write_image(rgba, width, height, ExtendedColorType::Rgba8)
        .unwrap();
    encoded
}

fn jpeg_rgb(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::new();
    JpegEncoder::new_with_quality(&mut encoded, 100)
        .write_image(rgb, width, height, ExtendedColorType::Rgb8)
        .unwrap();
    encoded
}

fn webp_rgba(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::new();
    WebPEncoder::new_lossless(&mut encoded)
        .write_image(rgba, width, height, ExtendedColorType::Rgba8)
        .unwrap();
    encoded
}

fn animated_webp(rgba: &[u8]) -> Vec<u8> {
    let still = webp_rgba(1, 1, rgba);
    let frame_chunk = &still[12..];
    let mut encoded = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
    encoded.extend_from_slice(&10_u32.to_le_bytes());
    encoded.extend_from_slice(&[0x12, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    encoded.extend_from_slice(b"ANIM");
    encoded.extend_from_slice(&6_u32.to_le_bytes());
    encoded.extend_from_slice(&[0; 6]);
    encoded.extend_from_slice(b"ANMF");
    encoded.extend_from_slice(&(16_u32 + u32::try_from(frame_chunk.len()).unwrap()).to_le_bytes());
    encoded.extend_from_slice(&[0; 16]);
    encoded.extend_from_slice(frame_chunk);
    let riff_size = u32::try_from(encoded.len() - 8).unwrap();
    encoded[4..8].copy_from_slice(&riff_size.to_le_bytes());
    encoded
}

fn jpeg_luma_with_exif(width: u32, height: u32, luma: &[u8], orientation: u16) -> Vec<u8> {
    let mut output = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut output, 100);
    encoder.set_exif_metadata(exif(orientation)).unwrap();
    encoder
        .write_image(luma, width, height, ExtendedColorType::L8)
        .unwrap();
    output
}

fn png_with_icc(profile: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut encoder = PngEncoder::new(&mut output);
    encoder.set_icc_profile(profile.to_vec()).unwrap();
    encoder
        .write_image(&[1, 2, 3, 255], 1, 1, ExtendedColorType::Rgba8)
        .unwrap();
    output
}

fn jpeg_with_icc(profile: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut output, 100);
    encoder.set_icc_profile(profile.to_vec()).unwrap();
    encoder
        .write_image(&[12, 34, 56], 1, 1, ExtendedColorType::Rgb8)
        .unwrap();
    output
}

fn exif(orientation: u16) -> Vec<u8> {
    let mut exif = Vec::new();
    exif.extend_from_slice(b"II");
    exif.extend_from_slice(&42_u16.to_le_bytes());
    exif.extend_from_slice(&8_u32.to_le_bytes());
    exif.extend_from_slice(&1_u16.to_le_bytes());
    exif.extend_from_slice(&0x0112_u16.to_le_bytes());
    exif.extend_from_slice(&3_u16.to_le_bytes());
    exif.extend_from_slice(&1_u32.to_le_bytes());
    exif.extend_from_slice(&orientation.to_le_bytes());
    exif.extend_from_slice(&0_u16.to_le_bytes());
    exif.extend_from_slice(&0_u32.to_le_bytes());
    exif
}

fn bytes(decoded: &DecodedStill) -> &[u8] {
    decoded.frame.payload().plane(0).unwrap().bytes()
}

fn assert_channel_near(actual: u8, expected: u8, tolerance: u8) {
    assert!(
        actual.abs_diff(expected) <= tolerance,
        "channel {actual} is not within {tolerance} of {expected}"
    );
}

fn expected_metadata() -> VideoFrameMetadata {
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

#[test]
fn sniffs_png_from_signature_without_decoding() {
    let mut encoded_prefix = b"\x89PNG\r\n\x1a\n".to_vec();
    encoded_prefix.extend_from_slice(b"not a valid PNG");

    assert_eq!(sniff_still_format(&encoded_prefix), Ok(StillFormat::Png));
}

#[test]
fn sniffs_jpeg_from_signature_without_decoding() {
    assert_eq!(
        sniff_still_format(b"\xff\xd8not a valid JPEG"),
        Ok(StillFormat::Jpeg)
    );
}

#[test]
fn webp_is_exact_straight_rgba_with_metadata_and_timing() {
    let pixels = [11, 22, 33, 0, 44, 55, 66, 127];
    let encoded = webp_rgba(2, 1, &pixels);

    assert_eq!(sniff_still_format(&encoded), Ok(StillFormat::WebP));
    assert_eq!(
        sniff_still_format(&encoded[..11]),
        Err(StillDecodeError::UnsupportedFormat)
    );
    assert_eq!(
        sniff_still_format(b"RIFF\0\0\0\0NOPE"),
        Err(StillDecodeError::UnsupportedFormat)
    );

    let decoded = decode_still(&encoded, timing(), StillDecodeLimits::default()).unwrap();
    assert_eq!(decoded.format, StillFormat::WebP);
    assert_eq!(
        (
            decoded.source_dimensions.width(),
            decoded.source_dimensions.height()
        ),
        (2, 1)
    );
    assert!(decoded.source_has_alpha);
    assert!(!decoded.orientation_applied);
    assert_eq!(decoded.frame.timing(), timing());
    assert_eq!(decoded.frame.payload().format(), PixelFormat::Rgba8);
    assert_eq!(decoded.frame.payload().plane(0).unwrap().stride(), 8);
    assert_eq!(bytes(&decoded), pixels);
    assert_eq!(decoded.frame.metadata(), Some(expected_metadata()));
}

#[test]
fn rejects_animated_webp_before_static_decode() {
    assert_eq!(
        decode_still(
            &animated_webp(&[1, 2, 3, 127]),
            timing(),
            StillDecodeLimits::default()
        ),
        Err(StillDecodeError::AnimatedWebpUnsupported)
    );
}

#[test]
fn sniff_rejects_short_and_unknown_prefixes() {
    for encoded_prefix in [&b""[..], b"\x89PNG\r\n\x1a", b"\xff", b"GIF89a"] {
        assert_eq!(
            sniff_still_format(encoded_prefix),
            Err(StillDecodeError::UnsupportedFormat)
        );
    }
}

#[test]
fn png_is_exact_straight_rgba_and_preserves_hidden_rgb_and_timing() {
    let pixels = [11, 22, 33, 0, 44, 55, 66, 127];
    let decoded =
        decode_still(&png(2, 1, &pixels), timing(), StillDecodeLimits::default()).unwrap();

    assert_eq!(decoded.format, StillFormat::Png);
    assert_eq!(decoded.source_dimensions.width(), 2);
    assert_eq!(decoded.source_dimensions.height(), 1);
    assert!(decoded.source_has_alpha);
    assert!(!decoded.orientation_applied);
    assert_eq!(decoded.frame.timing(), timing());
    assert_eq!(decoded.frame.payload().format(), PixelFormat::Rgba8);
    assert_eq!(decoded.frame.payload().plane(0).unwrap().stride(), 8);
    assert_eq!(bytes(&decoded), pixels);
    assert_eq!(decoded.frame.metadata(), Some(expected_metadata()));
}

#[test]
fn jpeg_has_expected_dimensions_pixels_and_opaque_alpha() {
    let encoded = jpeg_rgb(2, 2, &[30, 80, 160, 30, 80, 160, 30, 80, 160, 30, 80, 160]);
    let decoded = decode_still(&encoded, timing(), StillDecodeLimits::default()).unwrap();

    assert_eq!(decoded.format, StillFormat::Jpeg);
    assert_eq!(decoded.source_dimensions.width(), 2);
    assert_eq!(decoded.source_dimensions.height(), 2);
    assert!(!decoded.source_has_alpha);
    assert_eq!(decoded.frame.metadata(), Some(expected_metadata()));
    for pixel in bytes(&decoded).chunks_exact(4) {
        assert_channel_near(pixel[0], 30, 8);
        assert_channel_near(pixel[1], 80, 8);
        assert_channel_near(pixel[2], 160, 8);
        assert_eq!(pixel[3], 255);
    }
}

#[test]
fn applies_rotate_90_exif_and_swaps_output_dimensions() {
    let encoded = jpeg_luma_with_exif(2, 1, &[20, 230], 6);
    let decoded = decode_still(&encoded, timing(), StillDecodeLimits::default()).unwrap();

    assert!(decoded.orientation_applied);
    assert_eq!(decoded.source_dimensions.width(), 2);
    assert_eq!(decoded.source_dimensions.height(), 1);
    assert_eq!(decoded.frame.payload().dimensions().width(), 1);
    assert_eq!(decoded.frame.payload().dimensions().height(), 2);
    assert_eq!(decoded.frame.metadata(), Some(expected_metadata()));
    assert_channel_near(bytes(&decoded)[0], 20, 8);
    assert_channel_near(bytes(&decoded)[4], 230, 8);
}

#[test]
fn applies_mirrored_exif_orientation() {
    let encoded = jpeg_luma_with_exif(2, 1, &[20, 230], 2);
    let decoded = decode_still(&encoded, timing(), StillDecodeLimits::default()).unwrap();

    assert!(decoded.orientation_applied);
    assert_eq!(decoded.frame.payload().dimensions().width(), 2);
    assert_eq!(decoded.frame.metadata(), Some(expected_metadata()));
    assert_channel_near(bytes(&decoded)[0], 230, 8);
    assert_channel_near(bytes(&decoded)[4], 20, 8);
}

#[test]
fn rejects_png_and_jpeg_icc_profiles_and_distinguishes_profile_bounds() {
    for encoded in [png_with_icc(b"profile"), jpeg_with_icc(b"profile")] {
        assert_eq!(
            decode_still(&encoded, timing(), StillDecodeLimits::default()),
            Err(StillDecodeError::EmbeddedIccUnsupported)
        );

        let limits = StillDecodeLimits {
            max_icc_bytes: 3,
            ..StillDecodeLimits::default()
        };
        assert_eq!(
            decode_still(&encoded, timing(), limits),
            Err(StillDecodeError::IccProfileTooLarge {
                required: 7,
                maximum: 3,
            })
        );
    }
}

#[test]
fn rejects_apng_control_marker() {
    let mut encoded = png(1, 1, &[1, 2, 3, 255]);
    // The still encoder has no APNG API. Insert a bounded acTL chunk after IHDR;
    // marker rejection intentionally precedes CRC validation and pixel decoding.
    let mut actl = Vec::new();
    actl.extend_from_slice(&8_u32.to_be_bytes());
    actl.extend_from_slice(b"acTL");
    actl.extend_from_slice(&1_u32.to_be_bytes());
    actl.extend_from_slice(&0_u32.to_be_bytes());
    actl.extend_from_slice(&0_u32.to_be_bytes());
    encoded.splice(33..33, actl);

    assert_eq!(
        decode_still(&encoded, timing(), StillDecodeLimits::default()),
        Err(StillDecodeError::AnimatedPngUnsupported)
    );
}

#[test]
fn rejects_empty_unsupported_truncated_and_corrupt_input() {
    assert_eq!(
        decode_still(&[], timing(), StillDecodeLimits::default()),
        Err(StillDecodeError::EmptyInput)
    );
    assert_eq!(
        decode_still(b"GIF89a", timing(), StillDecodeLimits::default()),
        Err(StillDecodeError::UnsupportedFormat)
    );

    let png = png(1, 1, &[1, 2, 3, 4]);
    assert_eq!(
        decode_still(&png[..20], timing(), StillDecodeLimits::default()),
        Err(StillDecodeError::CorruptInput {
            format: StillFormat::Png,
        })
    );
    assert_eq!(
        decode_still(
            b"\xff\xd8\xff\xe0\x00",
            timing(),
            StillDecodeLimits::default()
        ),
        Err(StillDecodeError::CorruptInput {
            format: StillFormat::Jpeg,
        })
    );

    let mut corrupt = png;
    corrupt[12] = b'X';
    assert_eq!(
        decode_still(&corrupt, timing(), StillDecodeLimits::default()),
        Err(StillDecodeError::CorruptInput {
            format: StillFormat::Png,
        })
    );
}

#[test]
fn enforces_encoded_dimension_rgba_and_image_allocation_bounds() {
    let two_by_one = png(2, 1, &[1, 2, 3, 4, 5, 6, 7, 8]);
    let one_by_two = png(1, 2, &[1, 2, 3, 4, 5, 6, 7, 8]);

    let limits = StillDecodeLimits {
        max_encoded_bytes: two_by_one.len() - 1,
        ..StillDecodeLimits::default()
    };
    assert_eq!(
        decode_still(&two_by_one, timing(), limits),
        Err(StillDecodeError::EncodedBytesTooLarge {
            actual: two_by_one.len(),
            maximum: two_by_one.len() - 1,
        })
    );

    let limits = StillDecodeLimits {
        max_width: 1,
        ..StillDecodeLimits::default()
    };
    assert_eq!(
        decode_still(&two_by_one, timing(), limits),
        Err(StillDecodeError::WidthTooLarge {
            actual: 2,
            maximum: 1,
        })
    );

    let limits = StillDecodeLimits {
        max_height: 1,
        ..StillDecodeLimits::default()
    };
    assert_eq!(
        decode_still(&one_by_two, timing(), limits),
        Err(StillDecodeError::HeightTooLarge {
            actual: 2,
            maximum: 1,
        })
    );

    let limits = StillDecodeLimits {
        max_decoded_rgba_bytes: 7,
        ..StillDecodeLimits::default()
    };
    assert_eq!(
        decode_still(&two_by_one, timing(), limits),
        Err(StillDecodeError::DecodedRgbaTooLarge {
            required: 8,
            maximum: 7,
        })
    );

    let limits = StillDecodeLimits {
        max_image_alloc_bytes: 7,
        ..StillDecodeLimits::default()
    };
    assert_eq!(
        decode_still(&two_by_one, timing(), limits),
        Err(StillDecodeError::ImageAllocationLimitExceeded {
            required: None,
            maximum: 7,
        })
    );
}
