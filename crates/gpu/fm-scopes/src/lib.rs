//! Deterministic, bounded CPU reference implementations of video scopes.

use core::fmt;

pub use fm_video::{ColorMatrix, ColorRange};
use fm_video::{ImageFrame, Rgba8, Yuv8, rgb_to_yuv};

/// Maximum number of horizontal buckets in a waveform or parade.
pub const MAX_HORIZONTAL_BINS: usize = 4_096;
/// Maximum number of signal-level buckets in a waveform or parade.
pub const MAX_LEVEL_BINS: usize = 256;
/// Maximum width and height of the square vectorscope grid.
pub const MAX_VECTOR_BINS: usize = 1_024;
/// Maximum number of buckets in each histogram channel.
pub const MAX_HISTOGRAM_BINS: usize = 256;
/// Maximum number of counters allocated by one scope result.
pub const MAX_SCOPE_CELLS: usize = 3 * MAX_HORIZONTAL_BINS * MAX_LEVEL_BINS;

/// The color interpretation used while measuring a frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColorMetadata {
    pub matrix: ColorMatrix,
    pub range: ColorRange,
}

impl ColorMetadata {
    #[must_use]
    pub const fn new(matrix: ColorMatrix, range: ColorRange) -> Self {
        Self { matrix, range }
    }
}

/// Source and color metadata carried by every scope result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScopeMetadata {
    pub source_width: u32,
    pub source_height: u32,
    pub color: ColorMetadata,
}

/// Exact signal statistics. Alpha is not included in scope measurements.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignalStatistics {
    pub minimum: u8,
    pub maximum: u8,
    pub sum: u64,
    pub sample_count: u64,
}

impl SignalStatistics {
    /// Returns the arithmetic mean, or `0.0` if there are no samples.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn mean(self) -> f64 {
        if self.sample_count == 0 {
            0.0
        } else {
            self.sum as f64 / self.sample_count as f64
        }
    }
}

/// Occupancy statistics for a set of bins.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BinStatistics {
    pub occupied_bins: usize,
    pub peak_count: u32,
    pub sample_count: u64,
}

/// Identifies a configurable scope dimension in validation errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BinDimension {
    Horizontal,
    Level,
    Vector,
    Histogram,
}

/// Errors returned by scope configuration and pixel probing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScopeError {
    ZeroBins {
        dimension: BinDimension,
    },
    TooManyBins {
        dimension: BinDimension,
        requested: usize,
        maximum: usize,
    },
    OutputTooLarge {
        requested: usize,
        maximum: usize,
    },
    PixelOutOfBounds {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
}

impl fmt::Display for ScopeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroBins { dimension } => {
                write!(formatter, "{dimension:?} bin count must be nonzero")
            }
            Self::TooManyBins {
                dimension,
                requested,
                maximum,
            } => write!(
                formatter,
                "{dimension:?} bin count {requested} exceeds limit {maximum}"
            ),
            Self::OutputTooLarge { requested, maximum } => write!(
                formatter,
                "scope output requires {requested} counters, exceeding limit {maximum}"
            ),
            Self::PixelOutOfBounds {
                x,
                y,
                width,
                height,
            } => write!(
                formatter,
                "pixel ({x}, {y}) is outside frame dimensions {width}x{height}"
            ),
        }
    }
}

impl std::error::Error for ScopeError {}

/// Configuration for a luma waveform.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WaveformConfig {
    pub horizontal_bins: usize,
    pub level_bins: usize,
    pub color: ColorMetadata,
}

/// A luma waveform in row-major `[level][horizontal]` order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LumaWaveform {
    metadata: ScopeMetadata,
    horizontal_bins: usize,
    level_bins: usize,
    bins: Vec<u32>,
    signal: SignalStatistics,
    distribution: BinStatistics,
}

impl LumaWaveform {
    #[must_use]
    pub const fn metadata(&self) -> ScopeMetadata {
        self.metadata
    }

    #[must_use]
    pub const fn horizontal_bins(&self) -> usize {
        self.horizontal_bins
    }

    #[must_use]
    pub const fn level_bins(&self) -> usize {
        self.level_bins
    }

    #[must_use]
    pub fn bins(&self) -> &[u32] {
        &self.bins
    }

    #[must_use]
    pub fn bin(&self, horizontal: usize, level: usize) -> Option<u32> {
        if horizontal >= self.horizontal_bins || level >= self.level_bins {
            return None;
        }
        Some(self.bins[level * self.horizontal_bins + horizontal])
    }

    #[must_use]
    pub const fn signal_statistics(&self) -> SignalStatistics {
        self.signal
    }

    #[must_use]
    pub const fn bin_statistics(&self) -> BinStatistics {
        self.distribution
    }
}

/// Computes a deterministic luma waveform. Horizontal source pixels are scaled
/// into the requested width; signal codes are scaled into the requested levels.
///
/// # Errors
///
/// Returns [`ScopeError`] when a bin count is zero, exceeds its axis limit, or
/// would exceed the total output allocation limit.
pub fn luma_waveform(
    frame: &ImageFrame,
    config: WaveformConfig,
) -> Result<LumaWaveform, ScopeError> {
    validate_bins(
        config.horizontal_bins,
        MAX_HORIZONTAL_BINS,
        BinDimension::Horizontal,
    )?;
    validate_bins(config.level_bins, MAX_LEVEL_BINS, BinDimension::Level)?;
    let cell_count = validate_cells(config.horizontal_bins, config.level_bins, 1)?;
    let mut bins = vec![0_u32; cell_count];
    let mut signal = StatisticsAccumulator::new();

    for (source_x, pixel) in frame_pixels(frame) {
        let luma = rgb_to_yuv(pixel, config.color.matrix, config.color.range).y;
        let horizontal = scale_coordinate(source_x, frame.width(), config.horizontal_bins);
        let level = scale_code(luma, config.level_bins);
        bins[level * config.horizontal_bins + horizontal] += 1;
        signal.add(luma);
    }

    Ok(LumaWaveform {
        metadata: metadata(frame, config.color),
        horizontal_bins: config.horizontal_bins,
        level_bins: config.level_bins,
        distribution: summarize_bins(&bins),
        bins,
        signal: signal.finish(),
    })
}

/// RGB channel selection for parade results.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RgbChannel {
    Red,
    Green,
    Blue,
}

impl RgbChannel {
    const fn index(self) -> usize {
        match self {
            Self::Red => 0,
            Self::Green => 1,
            Self::Blue => 2,
        }
    }
}

/// Configuration for an RGB parade.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParadeConfig {
    pub horizontal_bins: usize,
    pub level_bins: usize,
    pub color: ColorMetadata,
}

/// Three RGB waveforms stored as contiguous red, green, and blue planes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RgbParade {
    metadata: ScopeMetadata,
    horizontal_bins: usize,
    level_bins: usize,
    bins: Vec<u32>,
    signals: [SignalStatistics; 3],
    distributions: [BinStatistics; 3],
}

impl RgbParade {
    #[must_use]
    pub const fn metadata(&self) -> ScopeMetadata {
        self.metadata
    }

    #[must_use]
    pub const fn horizontal_bins(&self) -> usize {
        self.horizontal_bins
    }

    #[must_use]
    pub const fn level_bins(&self) -> usize {
        self.level_bins
    }

    #[must_use]
    pub fn bins(&self, channel: RgbChannel) -> &[u32] {
        let plane_len = self.horizontal_bins * self.level_bins;
        let start = channel.index() * plane_len;
        &self.bins[start..start + plane_len]
    }

    #[must_use]
    pub fn bin(&self, channel: RgbChannel, horizontal: usize, level: usize) -> Option<u32> {
        if horizontal >= self.horizontal_bins || level >= self.level_bins {
            return None;
        }
        Some(self.bins(channel)[level * self.horizontal_bins + horizontal])
    }

    #[must_use]
    pub const fn signal_statistics(&self, channel: RgbChannel) -> SignalStatistics {
        self.signals[channel.index()]
    }

    #[must_use]
    pub const fn bin_statistics(&self, channel: RgbChannel) -> BinStatistics {
        self.distributions[channel.index()]
    }
}

/// Computes deterministic red, green, and blue waveform planes.
///
/// RGB values are measured directly. `color` is retained explicitly as source
/// interpretation metadata so this result can be compared with YUV-based scopes.
///
/// # Errors
///
/// Returns [`ScopeError`] for invalid dimensions or an oversized output.
pub fn rgb_parade(frame: &ImageFrame, config: ParadeConfig) -> Result<RgbParade, ScopeError> {
    validate_bins(
        config.horizontal_bins,
        MAX_HORIZONTAL_BINS,
        BinDimension::Horizontal,
    )?;
    validate_bins(config.level_bins, MAX_LEVEL_BINS, BinDimension::Level)?;
    let plane_len = validate_cells(config.horizontal_bins, config.level_bins, 1)?;
    let cell_count = validate_cells(config.horizontal_bins, config.level_bins, 3)?;
    let mut bins = vec![0_u32; cell_count];
    let mut signals = [StatisticsAccumulator::new(); 3];

    for (source_x, pixel) in frame_pixels(frame) {
        let horizontal = scale_coordinate(source_x, frame.width(), config.horizontal_bins);
        for (channel, code) in [pixel.r, pixel.g, pixel.b].into_iter().enumerate() {
            let level = scale_code(code, config.level_bins);
            bins[channel * plane_len + level * config.horizontal_bins + horizontal] += 1;
            signals[channel].add(code);
        }
    }

    let distributions = core::array::from_fn(|channel| {
        summarize_bins(&bins[channel * plane_len..(channel + 1) * plane_len])
    });
    Ok(RgbParade {
        metadata: metadata(frame, config.color),
        horizontal_bins: config.horizontal_bins,
        level_bins: config.level_bins,
        bins,
        signals: signals.map(StatisticsAccumulator::finish),
        distributions,
    })
}

/// Configuration for a square vectorscope grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VectorscopeConfig {
    pub bins: usize,
    pub color: ColorMetadata,
}

/// Chroma occupancy in row-major `[V][U]` order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Vectorscope {
    metadata: ScopeMetadata,
    dimension: usize,
    bins: Vec<u32>,
    u_signal: SignalStatistics,
    v_signal: SignalStatistics,
    distribution: BinStatistics,
}

impl Vectorscope {
    #[must_use]
    pub const fn metadata(&self) -> ScopeMetadata {
        self.metadata
    }

    #[must_use]
    pub const fn dimension(&self) -> usize {
        self.dimension
    }

    #[must_use]
    pub fn bins(&self) -> &[u32] {
        &self.bins
    }

    #[must_use]
    pub fn bin(&self, u: usize, v: usize) -> Option<u32> {
        if u >= self.dimension || v >= self.dimension {
            return None;
        }
        Some(self.bins[v * self.dimension + u])
    }

    #[must_use]
    pub const fn u_statistics(&self) -> SignalStatistics {
        self.u_signal
    }

    #[must_use]
    pub const fn v_statistics(&self) -> SignalStatistics {
        self.v_signal
    }

    #[must_use]
    pub const fn bin_statistics(&self) -> BinStatistics {
        self.distribution
    }
}

/// Computes deterministic U/V vectorscope occupancy using the configured color
/// matrix and range.
///
/// # Errors
///
/// Returns [`ScopeError`] for a zero or excessive grid dimension, or an
/// oversized output.
pub fn vectorscope(
    frame: &ImageFrame,
    config: VectorscopeConfig,
) -> Result<Vectorscope, ScopeError> {
    validate_bins(config.bins, MAX_VECTOR_BINS, BinDimension::Vector)?;
    let cell_count = validate_cells(config.bins, config.bins, 1)?;
    let mut bins = vec![0_u32; cell_count];
    let mut u_signal = StatisticsAccumulator::new();
    let mut v_signal = StatisticsAccumulator::new();

    for (_, pixel) in frame_pixels(frame) {
        let yuv = rgb_to_yuv(pixel, config.color.matrix, config.color.range);
        let u = scale_code(yuv.u, config.bins);
        let v = scale_code(yuv.v, config.bins);
        bins[v * config.bins + u] += 1;
        u_signal.add(yuv.u);
        v_signal.add(yuv.v);
    }

    Ok(Vectorscope {
        metadata: metadata(frame, config.color),
        dimension: config.bins,
        distribution: summarize_bins(&bins),
        bins,
        u_signal: u_signal.finish(),
        v_signal: v_signal.finish(),
    })
}

/// Histogram channel selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistogramChannel {
    Luma,
    Red,
    Green,
    Blue,
}

impl HistogramChannel {
    const fn index(self) -> usize {
        match self {
            Self::Luma => 0,
            Self::Red => 1,
            Self::Green => 2,
            Self::Blue => 3,
        }
    }
}

/// Configuration for luma and RGB histograms.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistogramConfig {
    pub bins: usize,
    pub color: ColorMetadata,
}

/// Luma, red, green, and blue histogram planes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Histogram {
    metadata: ScopeMetadata,
    bin_count: usize,
    bins: Vec<u32>,
    signals: [SignalStatistics; 4],
    distributions: [BinStatistics; 4],
}

impl Histogram {
    #[must_use]
    pub const fn metadata(&self) -> ScopeMetadata {
        self.metadata
    }

    #[must_use]
    pub const fn bin_count(&self) -> usize {
        self.bin_count
    }

    #[must_use]
    pub fn bins(&self, channel: HistogramChannel) -> &[u32] {
        let start = channel.index() * self.bin_count;
        &self.bins[start..start + self.bin_count]
    }

    #[must_use]
    pub fn bin(&self, channel: HistogramChannel, bin: usize) -> Option<u32> {
        self.bins(channel).get(bin).copied()
    }

    #[must_use]
    pub const fn signal_statistics(&self, channel: HistogramChannel) -> SignalStatistics {
        self.signals[channel.index()]
    }

    #[must_use]
    pub const fn bin_statistics(&self, channel: HistogramChannel) -> BinStatistics {
        self.distributions[channel.index()]
    }
}

/// Computes deterministic luma and RGB histograms.
///
/// # Errors
///
/// Returns [`ScopeError`] when the bin count is zero or exceeds the histogram
/// limit.
pub fn histogram(frame: &ImageFrame, config: HistogramConfig) -> Result<Histogram, ScopeError> {
    validate_bins(config.bins, MAX_HISTOGRAM_BINS, BinDimension::Histogram)?;
    let cell_count = validate_cells(config.bins, 4, 1)?;
    let mut bins = vec![0_u32; cell_count];
    let mut signals = [StatisticsAccumulator::new(); 4];

    for (_, pixel) in frame_pixels(frame) {
        let luma = rgb_to_yuv(pixel, config.color.matrix, config.color.range).y;
        for (channel, code) in [luma, pixel.r, pixel.g, pixel.b].into_iter().enumerate() {
            bins[channel * config.bins + scale_code(code, config.bins)] += 1;
            signals[channel].add(code);
        }
    }

    let distributions = core::array::from_fn(|channel| {
        summarize_bins(&bins[channel * config.bins..(channel + 1) * config.bins])
    });
    Ok(Histogram {
        metadata: metadata(frame, config.color),
        bin_count: config.bins,
        bins,
        signals: signals.map(StatisticsAccumulator::finish),
        distributions,
    })
}

/// An exact pixel sample in both source RGB and interpreted YUV.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixelProbe {
    pub metadata: ScopeMetadata,
    pub x: u32,
    pub y: u32,
    pub rgba: Rgba8,
    pub yuv: Yuv8,
}

/// Samples one source pixel and attaches explicit matrix/range metadata.
///
/// # Errors
///
/// Returns [`ScopeError::PixelOutOfBounds`] if the coordinate is outside the
/// frame.
pub fn pixel_probe(
    frame: &ImageFrame,
    x: u32,
    y: u32,
    color: ColorMetadata,
) -> Result<PixelProbe, ScopeError> {
    let rgba = frame.pixel(x, y).ok_or(ScopeError::PixelOutOfBounds {
        x,
        y,
        width: frame.width(),
        height: frame.height(),
    })?;
    Ok(PixelProbe {
        metadata: metadata(frame, color),
        x,
        y,
        rgba,
        yuv: rgb_to_yuv(rgba, color.matrix, color.range),
    })
}

#[derive(Clone, Copy)]
struct StatisticsAccumulator {
    minimum: u8,
    maximum: u8,
    sum: u64,
    sample_count: u64,
}

impl StatisticsAccumulator {
    const fn new() -> Self {
        Self {
            minimum: u8::MAX,
            maximum: 0,
            sum: 0,
            sample_count: 0,
        }
    }

    fn add(&mut self, code: u8) {
        self.minimum = self.minimum.min(code);
        self.maximum = self.maximum.max(code);
        self.sum += u64::from(code);
        self.sample_count += 1;
    }

    const fn finish(self) -> SignalStatistics {
        if self.sample_count == 0 {
            SignalStatistics {
                minimum: 0,
                maximum: 0,
                sum: 0,
                sample_count: 0,
            }
        } else {
            SignalStatistics {
                minimum: self.minimum,
                maximum: self.maximum,
                sum: self.sum,
                sample_count: self.sample_count,
            }
        }
    }
}

fn metadata(frame: &ImageFrame, color: ColorMetadata) -> ScopeMetadata {
    ScopeMetadata {
        source_width: frame.width(),
        source_height: frame.height(),
        color,
    }
}

fn validate_bins(
    requested: usize,
    maximum: usize,
    dimension: BinDimension,
) -> Result<(), ScopeError> {
    if requested == 0 {
        return Err(ScopeError::ZeroBins { dimension });
    }
    if requested > maximum {
        return Err(ScopeError::TooManyBins {
            dimension,
            requested,
            maximum,
        });
    }
    Ok(())
}

fn validate_cells(width: usize, height: usize, planes: usize) -> Result<usize, ScopeError> {
    let requested = width
        .checked_mul(height)
        .and_then(|cells| cells.checked_mul(planes))
        .unwrap_or(usize::MAX);
    if requested > MAX_SCOPE_CELLS {
        return Err(ScopeError::OutputTooLarge {
            requested,
            maximum: MAX_SCOPE_CELLS,
        });
    }
    Ok(requested)
}

fn scale_code(code: u8, bins: usize) -> usize {
    usize::from(code) * bins / 256
}

fn scale_coordinate(coordinate: usize, source_width: u32, bins: usize) -> usize {
    let coordinate = u64::try_from(coordinate).expect("source coordinate fits u64");
    let bins = u64::try_from(bins).expect("bounded bin count fits u64");
    usize::try_from(coordinate * bins / u64::from(source_width))
        .expect("scaled coordinate fits usize")
}

fn summarize_bins(bins: &[u32]) -> BinStatistics {
    let mut occupied_bins = 0;
    let mut peak_count = 0;
    let mut sample_count = 0_u64;
    for &count in bins {
        if count != 0 {
            occupied_bins += 1;
        }
        peak_count = peak_count.max(count);
        sample_count += u64::from(count);
    }
    BinStatistics {
        occupied_bins,
        peak_count,
        sample_count,
    }
}

fn frame_pixels(frame: &ImageFrame) -> impl Iterator<Item = (usize, Rgba8)> + '_ {
    let width = usize::try_from(frame.width()).expect("valid frame width fits usize");
    let packed_width = width * 4;
    frame
        .pixels()
        .chunks_exact(frame.stride())
        .flat_map(move |row| {
            row[..packed_width]
                .chunks_exact(4)
                .enumerate()
                .map(|(x, bytes)| (x, Rgba8::new(bytes[0], bytes[1], bytes[2], bytes[3])))
        })
}
