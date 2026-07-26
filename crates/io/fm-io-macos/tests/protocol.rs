use std::{io::Cursor, num::NonZeroU128};

use fm_frame::{AlphaMode, ClockDomainId, ColorPrimaries, TransferFunction};
use fm_io_macos::protocol::{
    AudioBlockReader, FrameReader, MAX_AUDIO_BLOCK_BYTES, MAX_AUDIO_SAMPLES_PER_BLOCK, MAX_DEVICES,
    MAX_DISCOVERY_BYTES, MAX_FRAME_BYTES, MAX_FRAMES_PER_SECOND, parse_audio_discovery,
    parse_discovery,
};
use fm_types::SampleRate;

fn discovery_header(permission: u8, devices: u32) -> Vec<u8> {
    let mut bytes = b"FMCAMD2\0".to_vec();
    bytes.push(permission);
    bytes.extend_from_slice(&devices.to_le_bytes());
    bytes
}

fn one_format_discovery(rate_numerator: u32, rate_denominator: u32) -> Vec<u8> {
    let mut bytes = discovery_header(0, 1);
    for value in ["camera", "Camera"] {
        bytes.extend_from_slice(&u32::try_from(value.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(value.as_bytes());
    }
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    bytes.extend_from_slice(&1_920_u32.to_le_bytes());
    bytes.extend_from_slice(&1_080_u32.to_le_bytes());
    bytes.extend_from_slice(&rate_numerator.to_le_bytes());
    bytes.extend_from_slice(&rate_denominator.to_le_bytes());
    bytes
}

fn record(sequence: u64, native_dropped_total: u64, pts: i64) -> Vec<u8> {
    let payload = [1, 2, 3, 255];
    let mut bytes = u32::try_from(58 + payload.len())
        .unwrap()
        .to_le_bytes()
        .to_vec();
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes.extend_from_slice(&native_dropped_total.to_le_bytes());
    bytes.extend_from_slice(&pts.to_le_bytes());
    bytes.extend_from_slice(&1_000_i32.to_le_bytes());
    bytes.extend_from_slice(&33_i64.to_le_bytes());
    bytes.extend_from_slice(&1_000_i32.to_le_bytes());
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    bytes.extend_from_slice(&4_u32.to_le_bytes());
    bytes.extend_from_slice(&4_u32.to_le_bytes());
    bytes.push(1);
    bytes.push(1);
    bytes.extend_from_slice(&payload);
    bytes
}

fn clock() -> ClockDomainId {
    ClockDomainId::new(NonZeroU128::new(7).unwrap())
}

fn audio_discovery(sample_rate: u32, channels: u8) -> Vec<u8> {
    let mut bytes = b"FMAUDD1\0".to_vec();
    bytes.push(0);
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    for value in ["microphone", "Microphone"] {
        bytes.extend_from_slice(&u32::try_from(value.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(value.as_bytes());
    }
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.push(channels);
    bytes
}

fn audio_record(sequence: u64, native_dropped_total: u64, pts: i64, samples: &[f32]) -> Vec<u8> {
    let payload_len = size_of_val(samples);
    let mut bytes = u32::try_from(41 + payload_len)
        .unwrap()
        .to_le_bytes()
        .to_vec();
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes.extend_from_slice(&native_dropped_total.to_le_bytes());
    bytes.extend_from_slice(&pts.to_le_bytes());
    bytes.extend_from_slice(&48_000_i32.to_le_bytes());
    bytes.extend_from_slice(&48_000_u32.to_le_bytes());
    bytes.push(2);
    bytes.extend_from_slice(&u32::try_from(samples.len() / 2).unwrap().to_le_bytes());
    bytes.extend_from_slice(&u32::try_from(payload_len).unwrap().to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

#[test]
fn discovery_rejects_malformed_and_bounded_inputs() {
    assert!(parse_discovery(b"not a protocol").is_err());
    assert!(parse_discovery(b"FMCAMD1\0\0\0\0\0\0").is_err());
    assert!(
        parse_discovery(&discovery_header(
            0,
            u32::try_from(MAX_DEVICES + 1).unwrap()
        ))
        .is_err()
    );
    assert!(parse_discovery(&vec![0; MAX_DISCOVERY_BYTES + 1]).is_err());

    let mut oversized_string = discovery_header(0, 1);
    oversized_string.extend_from_slice(&4097_u32.to_le_bytes());
    assert!(parse_discovery(&oversized_string).is_err());

    let mut trailing = discovery_header(0, 0);
    trailing.push(0);
    assert!(parse_discovery(&trailing).is_err());

    assert!(parse_discovery(&one_format_discovery(MAX_FRAMES_PER_SECOND + 1, 1)).is_err());
    assert!(parse_discovery(&one_format_discovery(30_000, 0)).is_err());
    assert!(parse_discovery(&one_format_discovery(60_000, 2_002)).is_err());
    assert!(parse_discovery(&one_format_discovery(u32::MAX, u32::MAX - 1)).is_err());
}

#[test]
fn discovery_preserves_exact_normalized_fractional_rates() {
    for (numerator, denominator) in [(24_000, 1_001), (30_000, 1_001), (60_000, 1_001)] {
        let discovery = parse_discovery(&one_format_discovery(numerator, denominator)).unwrap();
        let rate = discovery.devices[0].formats[0].frame_rate;
        assert_eq!(rate.numerator(), numerator);
        assert_eq!(rate.denominator(), denominator);
    }
    let integer = parse_discovery(&one_format_discovery(30, 1)).unwrap();
    assert_ne!(
        integer.devices[0].formats[0].frame_rate,
        parse_discovery(&one_format_discovery(30_000, 1_001))
            .unwrap()
            .devices[0]
            .formats[0]
            .frame_rate
    );
}

#[test]
fn frame_reader_preserves_coremedia_time_and_builds_opaque_bgra() {
    let mut stream = b"FMCAMF3\0".to_vec();
    stream.extend_from_slice(&record(10, 4, 1_500));
    let mut reader = FrameReader::new(Cursor::new(stream), clock()).unwrap();
    let captured = reader.read_captured_frame().unwrap().unwrap();
    assert_eq!(captured.native_dropped_total, 4);
    let frame = captured.frame;
    let timing = frame.timing();
    assert_eq!(timing.original_timestamp().timestamp().ticks(), 1_500);
    assert_eq!(timing.original_timestamp().time_base().denominator(), 1_000);
    assert_eq!(timing.presentation_timestamp().as_nanos(), 1_500_000_000);
    assert_eq!(timing.duration().as_nanos(), 33_000_000);
    assert_eq!(timing.sequence().get(), 10);
    assert_eq!(frame.payload().plane(0).unwrap().bytes(), &[1, 2, 3, 255]);
    assert_eq!(
        frame.metadata().unwrap().alpha_mode(),
        Some(AlphaMode::Straight)
    );
    assert_eq!(
        frame.metadata().unwrap().color().primaries,
        ColorPrimaries::Bt709
    );
    assert_eq!(
        frame.metadata().unwrap().color().transfer,
        TransferFunction::Srgb
    );
    assert!(reader.read_frame().unwrap().is_none());
}

#[test]
fn frame_reader_rejects_lengths_layout_and_nonsequential_records() {
    let mut short = b"FMCAMF3\0".to_vec();
    short.extend_from_slice(&57_u32.to_le_bytes());
    let mut reader = FrameReader::new(Cursor::new(short), clock()).unwrap();
    assert!(reader.read_frame().is_err());

    let mut oversized = b"FMCAMF3\0".to_vec();
    oversized.extend_from_slice(&u32::try_from(MAX_FRAME_BYTES + 1).unwrap().to_le_bytes());
    let mut reader = FrameReader::new(Cursor::new(oversized), clock()).unwrap();
    assert!(reader.read_frame().is_err());

    let mut bad_layout = b"FMCAMF3\0".to_vec();
    let mut bad_record = record(0, 0, 0);
    bad_record[52..56].copy_from_slice(&3_u32.to_le_bytes());
    bad_layout.extend_from_slice(&bad_record);
    let mut reader = FrameReader::new(Cursor::new(bad_layout), clock()).unwrap();
    assert!(reader.read_frame().is_err());

    let mut sequence = b"FMCAMF3\0".to_vec();
    sequence.extend_from_slice(&record(1, 0, 0));
    sequence.extend_from_slice(&record(3, 0, 33));
    let mut reader = FrameReader::new(Cursor::new(sequence), clock()).unwrap();
    reader.read_frame().unwrap().unwrap();
    assert!(reader.read_frame().is_err());

    let mut drops = b"FMCAMF3\0".to_vec();
    drops.extend_from_slice(&record(1, 2, 0));
    drops.extend_from_slice(&record(2, 1, 33));
    let mut reader = FrameReader::new(Cursor::new(drops), clock()).unwrap();
    reader.read_frame().unwrap().unwrap();
    assert!(reader.read_frame().is_err());

    let mut zero_timescale = b"FMCAMF3\0".to_vec();
    let mut zero_timescale_record = record(0, 0, 0);
    zero_timescale_record[28..32].copy_from_slice(&0_i32.to_le_bytes());
    zero_timescale.extend_from_slice(&zero_timescale_record);
    let mut reader = FrameReader::new(Cursor::new(zero_timescale), clock()).unwrap();
    assert!(reader.read_frame().is_err());

    let mut wrong_dimensions = b"FMCAMF3\0".to_vec();
    wrong_dimensions.extend_from_slice(&record(0, 0, 0));
    let mut reader =
        FrameReader::new_with_dimensions(Cursor::new(wrong_dimensions), clock(), 2, 1).unwrap();
    assert!(reader.read_frame().is_err());
}

#[test]
fn frame_reader_accepts_positive_coremedia_timescales_below_one_thousand() {
    let mut stream = b"FMCAMF3\0".to_vec();
    let mut frame = record(0, 0, 600);
    frame[28..32].copy_from_slice(&600_i32.to_le_bytes());
    stream.extend_from_slice(&frame);
    let mut reader = FrameReader::new_with_dimensions(Cursor::new(stream), clock(), 1, 1).unwrap();
    let timing = reader.read_frame().unwrap().unwrap().timing();
    assert_eq!(timing.original_timestamp().time_base().denominator(), 600);
    assert_eq!(timing.presentation_timestamp().as_nanos(), 1_000_000_000);
}

#[test]
fn frame_reader_requires_supported_color_codes_and_opaque_bgra() {
    assert!(FrameReader::new(Cursor::new(b"FMCAMF2\0"), clock()).is_err());

    let mut unknown_primaries = b"FMCAMF3\0".to_vec();
    let mut unknown_primaries_record = record(0, 0, 0);
    unknown_primaries_record[60] = 0;
    unknown_primaries.extend_from_slice(&unknown_primaries_record);
    assert!(
        FrameReader::new(Cursor::new(unknown_primaries), clock())
            .unwrap()
            .read_frame()
            .is_err()
    );
    let mut unknown_primaries = b"FMCAMF3\0".to_vec();
    let mut unknown_primaries_record = record(0, 0, 0);
    unknown_primaries_record[60] = 4;
    unknown_primaries.extend_from_slice(&unknown_primaries_record);
    assert!(
        FrameReader::new(Cursor::new(unknown_primaries), clock())
            .unwrap()
            .read_frame()
            .is_err()
    );

    let mut unknown_transfer = b"FMCAMF3\0".to_vec();
    let mut unknown_transfer_record = record(0, 0, 0);
    unknown_transfer_record[61] = 3;
    unknown_transfer.extend_from_slice(&unknown_transfer_record);
    assert!(
        FrameReader::new(Cursor::new(unknown_transfer), clock())
            .unwrap()
            .read_frame()
            .is_err()
    );
    let mut unknown_transfer = b"FMCAMF3\0".to_vec();
    let mut unknown_transfer_record = record(0, 0, 0);
    unknown_transfer_record[61] = 0;
    unknown_transfer.extend_from_slice(&unknown_transfer_record);
    assert!(
        FrameReader::new(Cursor::new(unknown_transfer), clock())
            .unwrap()
            .read_frame()
            .is_err()
    );

    let mut nonopaque = b"FMCAMF3\0".to_vec();
    let mut nonopaque_record = record(0, 0, 0);
    nonopaque_record[65] = 254;
    nonopaque.extend_from_slice(&nonopaque_record);
    assert!(
        FrameReader::new(Cursor::new(nonopaque), clock())
            .unwrap()
            .read_frame()
            .is_err()
    );

    let mut padded = b"FMCAMF3\0".to_vec();
    let mut padded_record = record(0, 0, 0);
    padded_record[0..4].copy_from_slice(&66_u32.to_le_bytes());
    padded_record[52..56].copy_from_slice(&8_u32.to_le_bytes());
    padded_record[56..60].copy_from_slice(&8_u32.to_le_bytes());
    padded_record.extend_from_slice(&[9, 9, 9, 0]);
    padded.extend_from_slice(&padded_record);
    let frame = FrameReader::new(Cursor::new(padded), clock())
        .unwrap()
        .read_frame()
        .unwrap()
        .unwrap();
    assert_eq!(
        frame.payload().plane(0).unwrap().bytes(),
        &[1, 2, 3, 255, 9, 9, 9, 0]
    );
}

#[test]
fn frame_reader_maps_supported_sdr_camera_color_codes() {
    for (primary_code, primaries) in [
        (1, ColorPrimaries::Bt709),
        (2, ColorPrimaries::DisplayP3),
        (3, ColorPrimaries::Bt2020),
    ] {
        for (transfer_code, transfer) in [(1, TransferFunction::Srgb), (2, TransferFunction::Bt709)]
        {
            let mut stream = b"FMCAMF3\0".to_vec();
            let mut color_record = record(0, 0, 0);
            color_record[60] = primary_code;
            color_record[61] = transfer_code;
            stream.extend_from_slice(&color_record);
            let frame = FrameReader::new(Cursor::new(stream), clock())
                .unwrap()
                .read_frame()
                .unwrap()
                .unwrap();
            assert_eq!(frame.metadata().unwrap().color().primaries, primaries);
            assert_eq!(frame.metadata().unwrap().color().transfer, transfer);
        }
    }
}

#[test]
fn audio_discovery_is_exact_bounded_and_permission_separate() {
    let discovery = parse_audio_discovery(&audio_discovery(48_000, 2)).unwrap();
    assert_eq!(discovery.devices.len(), 1);
    assert_eq!(discovery.devices[0].id, "microphone");
    assert_eq!(discovery.devices[0].formats[0].sample_rate.hertz(), 48_000);
    assert_eq!(discovery.devices[0].formats[0].channels, 2);

    let mut prompt = audio_discovery(48_000, 1);
    prompt[8] = 1;
    assert_eq!(
        parse_audio_discovery(&prompt).unwrap().permission,
        fm_io_macos::protocol::HelperPermission::PromptRequired
    );
    assert!(parse_audio_discovery(&audio_discovery(0, 2)).is_err());
    assert!(parse_audio_discovery(&audio_discovery(192_001, 2)).is_err());
    assert!(parse_audio_discovery(&audio_discovery(48_000, 0)).is_err());
    assert!(parse_audio_discovery(&audio_discovery(48_000, 3)).is_err());
    assert!(parse_audio_discovery(b"FMCAMD2\0\0\0\0\0\0").is_err());
}

#[test]
fn audio_reader_deinterleaves_finite_f32_and_preserves_coremedia_time() {
    let mut stream = b"FMAUDF1\0".to_vec();
    stream.extend_from_slice(&audio_record(7, 2, 48_000, &[0.25, -0.5, 1.0, -1.0]));
    let mut reader = AudioBlockReader::new_with_format(
        Cursor::new(stream),
        clock(),
        SampleRate::new(48_000).unwrap(),
        2,
    )
    .unwrap();
    let captured = reader.read_captured_block().unwrap().unwrap();
    assert_eq!(captured.native_dropped_total, 2);
    assert_eq!(captured.block.sample_rate().hertz(), 48_000);
    assert_eq!(captured.block.sample_count(), 2);
    assert_eq!(captured.block.plane(0).unwrap(), &[0.25, 1.0]);
    assert_eq!(captured.block.plane(1).unwrap(), &[-0.5, -1.0]);
    assert_eq!(
        captured
            .block
            .timing()
            .original_timestamp()
            .timestamp()
            .ticks(),
        48_000
    );
    assert_eq!(
        captured
            .block
            .timing()
            .original_timestamp()
            .time_base()
            .denominator(),
        48_000
    );
    assert_eq!(
        captured.block.timing().presentation_timestamp().as_nanos(),
        1_000_000_000
    );
    assert_eq!(captured.block.timing().duration().as_nanos(), 41_666);
    assert!(reader.read_block().unwrap().is_none());
}

#[test]
fn audio_reader_rejects_malformed_bounds_format_sequence_and_samples() {
    let mut oversized = b"FMAUDF1\0".to_vec();
    oversized.extend_from_slice(
        &u32::try_from(41 + MAX_AUDIO_BLOCK_BYTES + 1)
            .unwrap()
            .to_le_bytes(),
    );
    assert!(
        AudioBlockReader::new(Cursor::new(oversized), clock())
            .unwrap()
            .read_block()
            .is_err()
    );

    for (offset, bytes) in [
        (28, 0_i32.to_le_bytes().to_vec()),
        (32, 0_u32.to_le_bytes().to_vec()),
        (36, vec![3]),
        (37, 0_u32.to_le_bytes().to_vec()),
        (
            37,
            u32::try_from(MAX_AUDIO_SAMPLES_PER_BLOCK + 1)
                .unwrap()
                .to_le_bytes()
                .to_vec(),
        ),
        (41, 3_u32.to_le_bytes().to_vec()),
    ] {
        let mut stream = b"FMAUDF1\0".to_vec();
        let mut record = audio_record(0, 0, 0, &[0.0, 0.0]);
        record[offset..offset + bytes.len()].copy_from_slice(&bytes);
        stream.extend_from_slice(&record);
        assert!(
            AudioBlockReader::new(Cursor::new(stream), clock())
                .unwrap()
                .read_block()
                .is_err()
        );
    }

    let mut stream = b"FMAUDF1\0".to_vec();
    stream.extend_from_slice(&audio_record(0, 1, 0, &[0.0, 0.0]));
    stream.extend_from_slice(&audio_record(2, 0, 1, &[f32::NAN, 0.0]));
    let mut reader = AudioBlockReader::new(Cursor::new(stream), clock()).unwrap();
    assert!(reader.read_block().unwrap().is_some());
    assert!(reader.read_block().is_err());

    let mut stream = b"FMAUDF1\0".to_vec();
    stream.extend_from_slice(&audio_record(0, 0, 0, &[f32::NAN, 0.0]));
    assert!(
        AudioBlockReader::new(Cursor::new(stream), clock())
            .unwrap()
            .read_block()
            .is_err()
    );

    let mut stream = b"FMAUDF1\0".to_vec();
    stream.extend_from_slice(&audio_record(0, 1, 0, &[0.0, 0.0]));
    stream.extend_from_slice(&audio_record(1, 0, 1, &[0.0, 0.0]));
    let mut reader = AudioBlockReader::new(Cursor::new(stream), clock()).unwrap();
    assert!(reader.read_block().unwrap().is_some());
    assert!(reader.read_block().is_err());
}
