use fm_scopes::{
    BinDimension, ColorMatrix, ColorMetadata, ColorRange, HistogramChannel, HistogramConfig,
    MAX_HISTOGRAM_BINS, MAX_HORIZONTAL_BINS, MAX_VECTOR_BINS, ParadeConfig, RgbChannel, ScopeError,
    VectorscopeConfig, WaveformConfig, histogram, luma_waveform, pixel_probe, rgb_parade,
    vectorscope,
};
use fm_video::{ImageFrame, Rgba8, vertical_color_bars};

const FULL_601: ColorMetadata = ColorMetadata::new(ColorMatrix::Bt601, ColorRange::Full);
const FULL_709: ColorMetadata = ColorMetadata::new(ColorMatrix::Bt709, ColorRange::Full);
const LIMITED_709: ColorMetadata = ColorMetadata::new(ColorMatrix::Bt709, ColorRange::Limited);

fn frame(width: u32, pixels: &[Rgba8]) -> ImageFrame {
    let bytes = pixels
        .iter()
        .flat_map(|pixel| pixel.to_bytes())
        .collect::<Vec<_>>();
    ImageFrame::new(width, 1, usize::try_from(width).unwrap() * 4, bytes).unwrap()
}

#[test]
fn full_range_gray_ramp_has_one_sample_per_luma_level() {
    let pixels = (0..=u8::MAX)
        .map(|code| Rgba8::new(code, code, code, u8::MAX))
        .collect::<Vec<_>>();
    let ramp = frame(256, &pixels);
    let waveform = luma_waveform(
        &ramp,
        WaveformConfig {
            horizontal_bins: 1,
            level_bins: 256,
            color: FULL_601,
        },
    )
    .unwrap();

    assert!(waveform.bins().iter().all(|count| *count == 1));
    assert_eq!(waveform.signal_statistics().minimum, 0);
    assert_eq!(waveform.signal_statistics().maximum, 255);
    assert_eq!(waveform.signal_statistics().sum, 32_640);
    assert!((waveform.signal_statistics().mean() - 127.5).abs() < f64::EPSILON);
    assert_eq!(waveform.bin_statistics().occupied_bins, 256);
}

#[test]
fn rgb_primary_parade_keeps_channels_separate() {
    let primaries = frame(
        3,
        &[
            Rgba8::new(255, 0, 0, 255),
            Rgba8::new(0, 255, 0, 255),
            Rgba8::new(0, 0, 255, 255),
        ],
    );
    let parade = rgb_parade(
        &primaries,
        ParadeConfig {
            horizontal_bins: 3,
            level_bins: 256,
            color: FULL_709,
        },
    )
    .unwrap();

    assert_eq!(parade.bin(RgbChannel::Red, 0, 255), Some(1));
    assert_eq!(parade.bin(RgbChannel::Red, 1, 0), Some(1));
    assert_eq!(parade.bin(RgbChannel::Green, 1, 255), Some(1));
    assert_eq!(parade.bin(RgbChannel::Blue, 2, 255), Some(1));
    assert_eq!(parade.signal_statistics(RgbChannel::Red).sum, 255);
    assert_eq!(parade.bin_statistics(RgbChannel::Blue).sample_count, 3);
}

#[test]
fn primary_vectors_match_bt709_full_range_codes() {
    let primaries = frame(
        3,
        &[
            Rgba8::new(255, 0, 0, 255),
            Rgba8::new(0, 255, 0, 255),
            Rgba8::new(0, 0, 255, 255),
        ],
    );
    let scope = vectorscope(
        &primaries,
        VectorscopeConfig {
            bins: 256,
            color: FULL_709,
        },
    )
    .unwrap();

    assert_eq!(scope.bin(99, 255), Some(1));
    assert_eq!(scope.bin(29, 12), Some(1));
    assert_eq!(scope.bin(255, 116), Some(1));
    assert_eq!(scope.bin_statistics().occupied_bins, 3);
    assert_eq!(scope.u_statistics().sample_count, 3);
}

#[test]
fn color_bars_produce_known_rgb_histogram() {
    let bars = vertical_color_bars(7, 2, 0).unwrap();
    let histogram = histogram(
        &bars,
        HistogramConfig {
            bins: 256,
            color: FULL_601,
        },
    )
    .unwrap();

    // The second row includes the generator's one black frame marker.
    assert_eq!(histogram.bin(HistogramChannel::Red, 191), Some(7));
    assert_eq!(histogram.bin(HistogramChannel::Green, 191), Some(7));
    assert_eq!(histogram.bin(HistogramChannel::Blue, 191), Some(7));
    assert_eq!(histogram.bin(HistogramChannel::Red, 0), Some(7));
    assert_eq!(
        histogram
            .bin_statistics(HistogramChannel::Luma)
            .sample_count,
        14
    );
}

#[test]
fn limited_range_luma_and_probe_preserve_metadata() {
    let black_and_white = frame(
        2,
        &[Rgba8::new(0, 0, 0, 17), Rgba8::new(255, 255, 255, 255)],
    );
    let waveform = luma_waveform(
        &black_and_white,
        WaveformConfig {
            horizontal_bins: 2,
            level_bins: 256,
            color: LIMITED_709,
        },
    )
    .unwrap();
    let probe = pixel_probe(&black_and_white, 0, 0, LIMITED_709).unwrap();

    assert_eq!(waveform.bin(0, 16), Some(1));
    assert_eq!(waveform.bin(1, 235), Some(1));
    assert_eq!(waveform.metadata().color, LIMITED_709);
    assert_eq!(probe.metadata.color, LIMITED_709);
    assert_eq!(probe.rgba.a, 17);
    assert_eq!((probe.yuv.y, probe.yuv.u, probe.yuv.v), (16, 128, 128));
}

#[test]
fn invalid_configs_are_rejected_before_allocating() {
    let image = frame(1, &[Rgba8::new(0, 0, 0, 255)]);

    assert_eq!(
        luma_waveform(
            &image,
            WaveformConfig {
                horizontal_bins: 0,
                level_bins: 256,
                color: FULL_601,
            }
        ),
        Err(ScopeError::ZeroBins {
            dimension: BinDimension::Horizontal,
        })
    );
    assert!(matches!(
        rgb_parade(
            &image,
            ParadeConfig {
                horizontal_bins: MAX_HORIZONTAL_BINS + 1,
                level_bins: 1,
                color: FULL_601,
            }
        ),
        Err(ScopeError::TooManyBins { .. })
    ));
    assert!(matches!(
        vectorscope(
            &image,
            VectorscopeConfig {
                bins: MAX_VECTOR_BINS + 1,
                color: FULL_601,
            }
        ),
        Err(ScopeError::TooManyBins { .. })
    ));
    assert!(matches!(
        histogram(
            &image,
            HistogramConfig {
                bins: MAX_HISTOGRAM_BINS + 1,
                color: FULL_601,
            }
        ),
        Err(ScopeError::TooManyBins { .. })
    ));
    assert!(matches!(
        pixel_probe(&image, 1, 0, FULL_601),
        Err(ScopeError::PixelOutOfBounds { .. })
    ));
}
