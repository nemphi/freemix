//! Bounded parsers for the `AVFoundation` helper's binary protocols.

use std::{fmt, io::Read};

use fm_frame::{
    AlphaMode, AudioBlock, ChromaLocation, ClockDomainId, ColorMetadata, ColorPrimaries,
    CpuVideoFrame, CpuVideoPayload, CpuVideoPlane, MatrixCoefficients, MediaTimestamp, MediaTiming,
    NormalizedDuration, OriginalTimestamp, PixelFormat, SequenceNumber, SignalRange, TimeBase,
    TransferFunction, VideoDimensions, VideoFrameMetadata,
};
use fm_types::{Channel, ChannelLayout, FrameRate, SampleRate};

pub const MAX_DISCOVERY_BYTES: usize = 256 * 1024;
pub const MAX_DEVICES: usize = 64;
pub const MAX_FORMATS_PER_DEVICE: usize = 256;
pub const MAX_STRING_BYTES: usize = 4096;
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_FRAME_WIDTH: u32 = 3840;
pub const MAX_FRAME_HEIGHT: u32 = 2160;
pub const MAX_FRAMES_PER_SECOND: u32 = 60;
pub const MAX_AUDIO_SAMPLE_RATE: u32 = 192_000;
pub const MAX_AUDIO_CHANNELS: u8 = 2;
pub const MAX_AUDIO_SAMPLES_PER_BLOCK: usize = 16_384;
pub const MAX_AUDIO_BLOCK_BYTES: usize =
    MAX_AUDIO_SAMPLES_PER_BLOCK * MAX_AUDIO_CHANNELS as usize * size_of::<f32>();

const DISCOVERY_MAGIC: &[u8; 8] = b"FMCAMD2\0";
const CAPTURE_MAGIC: &[u8; 8] = b"FMCAMF3\0";
const FRAME_METADATA_BYTES: usize = 58;
const AUDIO_DISCOVERY_MAGIC: &[u8; 8] = b"FMAUDD1\0";
const AUDIO_CAPTURE_MAGIC: &[u8; 8] = b"FMAUDF1\0";
const AUDIO_BLOCK_METADATA_BYTES: usize = 41;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtocolError {
    detail: String,
    malformed: bool,
}

impl ProtocolError {
    fn malformed(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            malformed: true,
        }
    }

    pub(crate) const fn is_malformed(&self) -> bool {
        self.malformed
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for ProtocolError {}

impl From<std::io::Error> for ProtocolError {
    fn from(error: std::io::Error) -> Self {
        Self {
            detail: format!("camera helper I/O failed: {error}"),
            malformed: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HelperPermission {
    Granted,
    PromptRequired,
    Denied,
    Restricted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HelperFormat {
    pub width: u32,
    pub height: u32,
    pub frame_rate: FrameRate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelperDevice {
    pub id: String,
    pub name: String,
    pub formats: Vec<HelperFormat>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelperDiscovery {
    pub permission: HelperPermission,
    pub devices: Vec<HelperDevice>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HelperAudioFormat {
    pub sample_rate: SampleRate,
    pub channels: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelperAudioDevice {
    pub id: String,
    pub name: String,
    pub formats: Vec<HelperAudioFormat>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelperAudioDiscovery {
    pub permission: HelperPermission,
    pub devices: Vec<HelperAudioDevice>,
}

/// Parses one complete discovery response without retaining oversized input.
///
/// # Errors
///
/// Returns an error for an invalid magic, unknown permission, malformed UTF-8,
/// trailing bytes, or any count, string, dimension, or total-size bound.
pub fn parse_discovery(bytes: &[u8]) -> Result<HelperDiscovery, ProtocolError> {
    if bytes.len() > MAX_DISCOVERY_BYTES {
        return Err(ProtocolError::malformed(format!(
            "discovery output exceeds {MAX_DISCOVERY_BYTES} bytes"
        )));
    }
    let mut cursor = Cursor::new(bytes);
    if cursor.take(8)? != DISCOVERY_MAGIC {
        return Err(ProtocolError::malformed("invalid discovery magic"));
    }
    let permission = match cursor.u8()? {
        0 => HelperPermission::Granted,
        1 => HelperPermission::PromptRequired,
        2 => HelperPermission::Denied,
        3 => HelperPermission::Restricted,
        value => {
            return Err(ProtocolError::malformed(format!(
                "unknown camera permission state {value}"
            )));
        }
    };
    let device_count = cursor.count("device", MAX_DEVICES)?;
    let mut devices = Vec::with_capacity(device_count);
    for _ in 0..device_count {
        let id = cursor.string("device id")?;
        if id.is_empty() {
            return Err(ProtocolError::malformed("camera device id is empty"));
        }
        let name = cursor.string("device name")?;
        let format_count = cursor.count("format", MAX_FORMATS_PER_DEVICE)?;
        let mut formats = Vec::with_capacity(format_count);
        for _ in 0..format_count {
            let width = cursor.u32()?;
            let height = cursor.u32()?;
            let rate_numerator = cursor.u32()?;
            let rate_denominator = cursor.u32()?;
            if width == 0 || height == 0 || width > MAX_FRAME_WIDTH || height > MAX_FRAME_HEIGHT {
                return Err(ProtocolError::malformed(format!(
                    "unsupported camera dimensions {width}x{height}"
                )));
            }
            let frame_rate = FrameRate::new(rate_numerator, rate_denominator).map_err(|_| {
                ProtocolError::malformed(format!(
                    "unsupported camera frame rate {rate_numerator}/{rate_denominator}"
                ))
            })?;
            if frame_rate.numerator() != rate_numerator
                || frame_rate.denominator() != rate_denominator
            {
                return Err(ProtocolError::malformed(format!(
                    "camera frame rate {rate_numerator}/{rate_denominator} is not normalized"
                )));
            }
            if rate_numerator > i32::MAX.cast_unsigned()
                || u64::from(rate_numerator)
                    > u64::from(MAX_FRAMES_PER_SECOND) * u64::from(rate_denominator)
            {
                return Err(ProtocolError::malformed(format!(
                    "unsupported camera frame rate {rate_numerator}/{rate_denominator}"
                )));
            }
            formats.push(HelperFormat {
                width,
                height,
                frame_rate,
            });
        }
        devices.push(HelperDevice { id, name, formats });
    }
    if cursor.remaining() != 0 {
        return Err(ProtocolError::malformed("trailing discovery bytes"));
    }
    Ok(HelperDiscovery {
        permission,
        devices,
    })
}

/// Parses one complete microphone discovery response without retaining
/// oversized input.
///
/// # Errors
///
/// Returns an error for invalid magic, permission, UTF-8, trailing bytes, or
/// unsupported device, format, sample-rate, and channel-count values.
pub fn parse_audio_discovery(bytes: &[u8]) -> Result<HelperAudioDiscovery, ProtocolError> {
    if bytes.len() > MAX_DISCOVERY_BYTES {
        return Err(ProtocolError::malformed(format!(
            "audio discovery output exceeds {MAX_DISCOVERY_BYTES} bytes"
        )));
    }
    let mut cursor = Cursor::new(bytes);
    if cursor.take(8)? != AUDIO_DISCOVERY_MAGIC {
        return Err(ProtocolError::malformed("invalid audio discovery magic"));
    }
    let permission = match cursor.u8()? {
        0 => HelperPermission::Granted,
        1 => HelperPermission::PromptRequired,
        2 => HelperPermission::Denied,
        3 => HelperPermission::Restricted,
        value => {
            return Err(ProtocolError::malformed(format!(
                "unknown microphone permission state {value}"
            )));
        }
    };
    let device_count = cursor.count("audio device", MAX_DEVICES)?;
    let mut devices = Vec::with_capacity(device_count);
    for _ in 0..device_count {
        let id = cursor.string("audio device id")?;
        if id.is_empty() {
            return Err(ProtocolError::malformed("audio device id is empty"));
        }
        let name = cursor.string("audio device name")?;
        let format_count = cursor.count("audio format", MAX_FORMATS_PER_DEVICE)?;
        let mut formats = Vec::with_capacity(format_count);
        for _ in 0..format_count {
            let sample_rate_hz = cursor.u32()?;
            let sample_rate = SampleRate::new(sample_rate_hz)
                .ok_or_else(|| ProtocolError::malformed("audio sample rate must be positive"))?;
            if sample_rate_hz > MAX_AUDIO_SAMPLE_RATE {
                return Err(ProtocolError::malformed(format!(
                    "audio sample rate {sample_rate_hz} exceeds {MAX_AUDIO_SAMPLE_RATE}"
                )));
            }
            let channels = cursor.u8()?;
            if !(1..=MAX_AUDIO_CHANNELS).contains(&channels) {
                return Err(ProtocolError::malformed(format!(
                    "unsupported audio channel count {channels}"
                )));
            }
            formats.push(HelperAudioFormat {
                sample_rate,
                channels,
            });
        }
        devices.push(HelperAudioDevice { id, name, formats });
    }
    if cursor.remaining() != 0 {
        return Err(ProtocolError::malformed("trailing audio discovery bytes"));
    }
    Ok(HelperAudioDiscovery {
        permission,
        devices,
    })
}

/// Streaming capture parser. It allocates payload storage only after validating
/// the fixed-size record metadata and all arithmetic.
pub struct FrameReader<R> {
    reader: R,
    clock_domain: ClockDomainId,
    previous_sequence: Option<u64>,
    previous_native_dropped_total: Option<u64>,
    previous_pts_nanos: Option<i64>,
    expected_dimensions: Option<(u32, u32)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedVideoFrame {
    pub frame: CpuVideoFrame,
    pub native_dropped_total: u64,
}

impl<R: Read> FrameReader<R> {
    /// Validates the stream magic and starts a framed reader.
    ///
    /// # Errors
    ///
    /// Returns an error when the stream is truncated or has the wrong magic.
    pub fn new(reader: R, clock_domain: ClockDomainId) -> Result<Self, ProtocolError> {
        Self::new_inner(reader, clock_domain, None)
    }

    /// Validates stream magic and requires every frame to match one negotiated
    /// width and height.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid expected dimensions, truncation, or magic.
    pub fn new_with_dimensions(
        reader: R,
        clock_domain: ClockDomainId,
        width: u32,
        height: u32,
    ) -> Result<Self, ProtocolError> {
        if width == 0 || height == 0 || width > MAX_FRAME_WIDTH || height > MAX_FRAME_HEIGHT {
            return Err(ProtocolError::malformed(format!(
                "unsupported expected frame dimensions {width}x{height}"
            )));
        }
        Self::new_inner(reader, clock_domain, Some((width, height)))
    }

    fn new_inner(
        mut reader: R,
        clock_domain: ClockDomainId,
        expected_dimensions: Option<(u32, u32)>,
    ) -> Result<Self, ProtocolError> {
        let magic = read_capture_magic(&mut reader)?;
        if &magic != CAPTURE_MAGIC {
            return Err(ProtocolError::malformed("invalid capture magic"));
        }
        Ok(Self {
            reader,
            clock_domain,
            previous_sequence: None,
            previous_native_dropped_total: None,
            previous_pts_nanos: None,
            expected_dimensions,
        })
    }

    /// Reads and validates the next record. Clean EOF between records returns
    /// `None`; partial records are errors.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed lengths, timestamps, sequence numbers,
    /// native drop totals, dimensions, source color metadata, opaque `BGRA`
    /// layout, payload data, or underlying I/O.
    #[allow(clippy::too_many_lines)]
    pub fn read_captured_frame(&mut self) -> Result<Option<CapturedVideoFrame>, ProtocolError> {
        let Some(record_len) = read_optional_u32(&mut self.reader)? else {
            return Ok(None);
        };
        let record_len = usize::try_from(record_len)
            .map_err(|_| ProtocolError::malformed("record length does not fit usize"))?;
        if record_len > MAX_FRAME_BYTES {
            return Err(ProtocolError::malformed(format!(
                "capture record exceeds {MAX_FRAME_BYTES} bytes"
            )));
        }
        if record_len < FRAME_METADATA_BYTES {
            return Err(ProtocolError::malformed(
                "capture record is shorter than metadata",
            ));
        }
        let payload_from_record = record_len - FRAME_METADATA_BYTES;
        if payload_from_record > MAX_FRAME_BYTES {
            return Err(ProtocolError::malformed(format!(
                "frame payload exceeds {MAX_FRAME_BYTES} bytes"
            )));
        }

        let mut metadata = [0; FRAME_METADATA_BYTES];
        self.reader.read_exact(&mut metadata).map_err(|error| {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                ProtocolError::malformed("truncated frame metadata")
            } else {
                error.into()
            }
        })?;
        let mut cursor = Cursor::new(&metadata);
        let sequence = cursor.u64()?;
        let native_dropped_total = cursor.u64()?;
        let pts_value = cursor.i64()?;
        let pts_timescale = cursor.i32()?;
        let duration_value = cursor.i64()?;
        let duration_timescale = cursor.i32()?;
        let width = cursor.u32()?;
        let height = cursor.u32()?;
        let stride = cursor.u32()?;
        let payload_len = cursor.u32()?;
        let primaries = match cursor.u8()? {
            1 => ColorPrimaries::Bt709,
            2 => ColorPrimaries::DisplayP3,
            3 => ColorPrimaries::Bt2020,
            value => {
                return Err(ProtocolError::malformed(format!(
                    "unsupported camera color primaries code {value}"
                )));
            }
        };
        let transfer = match cursor.u8()? {
            1 => TransferFunction::Srgb,
            2 => TransferFunction::Bt709,
            value => {
                return Err(ProtocolError::malformed(format!(
                    "unsupported camera transfer function code {value}"
                )));
            }
        };

        if let Some(previous) = self.previous_sequence {
            let expected = previous
                .checked_add(1)
                .ok_or_else(|| ProtocolError::malformed("capture sequence overflow"))?;
            if sequence != expected {
                return Err(ProtocolError::malformed(format!(
                    "capture sequence {sequence} does not follow {previous}"
                )));
            }
        }
        if let Some(previous) = self.previous_native_dropped_total
            && native_dropped_total < previous
        {
            return Err(ProtocolError::malformed(format!(
                "native dropped total {native_dropped_total} is less than {previous}"
            )));
        }
        if pts_timescale <= 0 {
            return Err(ProtocolError::malformed(
                "capture PTS timescale must be positive",
            ));
        }
        if duration_timescale <= 0 {
            return Err(ProtocolError::malformed(
                "capture duration timescale must be positive",
            ));
        }
        if duration_value <= 0 {
            return Err(ProtocolError::malformed(
                "capture duration must be positive",
            ));
        }
        if width == 0 || height == 0 || width > MAX_FRAME_WIDTH || height > MAX_FRAME_HEIGHT {
            return Err(ProtocolError::malformed(format!(
                "unsupported frame dimensions {width}x{height}"
            )));
        }
        if let Some((expected_width, expected_height)) = self.expected_dimensions
            && (expected_width, expected_height) != (width, height)
        {
            return Err(ProtocolError::malformed(format!(
                "captured frame dimensions {width}x{height} do not match negotiated {expected_width}x{expected_height}"
            )));
        }

        let stride = usize::try_from(stride)
            .map_err(|_| ProtocolError::malformed("frame stride does not fit usize"))?;
        let minimum_stride = usize::try_from(width)
            .map_err(|_| ProtocolError::malformed("frame width does not fit usize"))?
            .checked_mul(4)
            .ok_or_else(|| ProtocolError::malformed("frame stride overflow"))?;
        if stride < minimum_stride {
            return Err(ProtocolError::malformed("BGRA frame stride is too small"));
        }
        let expected_payload = stride
            .checked_mul(
                usize::try_from(height)
                    .map_err(|_| ProtocolError::malformed("frame height does not fit usize"))?,
            )
            .ok_or_else(|| ProtocolError::malformed("frame payload size overflow"))?;
        let payload_len = usize::try_from(payload_len)
            .map_err(|_| ProtocolError::malformed("payload length does not fit usize"))?;
        if expected_payload != payload_len || payload_from_record != payload_len {
            return Err(ProtocolError::malformed(
                "capture record and BGRA payload lengths disagree",
            ));
        }
        if payload_len > MAX_FRAME_BYTES {
            return Err(ProtocolError::malformed(format!(
                "frame payload exceeds {MAX_FRAME_BYTES} bytes"
            )));
        }

        let pts_scale = u32::try_from(pts_timescale)
            .map_err(|_| ProtocolError::malformed("invalid PTS timescale"))?;
        let duration_scale = u32::try_from(duration_timescale)
            .map_err(|_| ProtocolError::malformed("invalid duration timescale"))?;
        let pts_time_base = TimeBase::new(1, pts_scale)
            .map_err(|error| ProtocolError::malformed(format!("invalid PTS timebase: {error}")))?;
        let original = OriginalTimestamp::new(MediaTimestamp::new(pts_value), pts_time_base);
        let normalized = original
            .normalize()
            .map_err(|error| ProtocolError::malformed(format!("invalid PTS: {error}")))?;
        if let Some(previous) = self.previous_pts_nanos
            && normalized.as_nanos() <= previous
        {
            return Err(ProtocolError::malformed(format!(
                "capture PTS {} does not follow {previous}",
                normalized.as_nanos()
            )));
        }
        let duration_nanos = i128::from(duration_value)
            .checked_mul(1_000_000_000)
            .ok_or_else(|| ProtocolError::malformed("duration normalization overflow"))?
            / i128::from(duration_scale);
        let duration_nanos = u64::try_from(duration_nanos)
            .map_err(|_| ProtocolError::malformed("duration normalization overflow"))?;
        let duration = NormalizedDuration::from_nanos(duration_nanos)
            .map_err(|error| ProtocolError::malformed(format!("invalid duration: {error}")))?;

        let mut payload = vec![0; payload_len];
        self.reader.read_exact(&mut payload).map_err(|error| {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                ProtocolError::malformed("truncated frame payload")
            } else {
                error.into()
            }
        })?;
        if payload.chunks_exact(stride).any(|row| {
            row[..minimum_stride]
                .chunks_exact(4)
                .any(|pixel| pixel[3] != 255)
        }) {
            return Err(ProtocolError::malformed(
                "camera BGRA frame contains nonopaque active pixels",
            ));
        }
        let dimensions = VideoDimensions::new(width, height)
            .ok_or_else(|| ProtocolError::malformed("zero frame dimensions"))?;
        let plane = CpuVideoPlane::new(stride, payload)
            .map_err(|error| ProtocolError::malformed(format!("invalid frame plane: {error}")))?;
        let payload = CpuVideoPayload::new(PixelFormat::Bgra8, dimensions, vec![plane])
            .map_err(|error| ProtocolError::malformed(format!("invalid frame payload: {error}")))?;
        let timing = MediaTiming::new(
            original,
            normalized,
            duration,
            self.clock_domain,
            SequenceNumber::new(sequence),
        )
        .map_err(|error| ProtocolError::malformed(format!("invalid frame timing: {error}")))?;

        let metadata = VideoFrameMetadata::new(
            ColorMetadata {
                primaries,
                transfer,
                matrix: MatrixCoefficients::Identity,
                range: SignalRange::Full,
                chroma_location: ChromaLocation::Center,
            },
            Some(AlphaMode::Straight),
        );
        let frame = CpuVideoFrame::new(timing, payload)
            .with_metadata(metadata)
            .map_err(|error| {
                ProtocolError::malformed(format!("invalid frame metadata: {error}"))
            })?;
        self.previous_sequence = Some(sequence);
        self.previous_native_dropped_total = Some(native_dropped_total);
        self.previous_pts_nanos = Some(normalized.as_nanos());
        Ok(Some(CapturedVideoFrame {
            frame,
            native_dropped_total,
        }))
    }

    /// Reads the next frame while discarding the native cumulative drop count.
    ///
    /// # Errors
    ///
    /// Returns the same protocol and I/O errors as [`Self::read_captured_frame`].
    pub fn read_frame(&mut self) -> Result<Option<CpuVideoFrame>, ProtocolError> {
        self.read_captured_frame()
            .map(|captured| captured.map(|captured| captured.frame))
    }
}

fn read_capture_magic(reader: &mut impl Read) -> Result<[u8; 8], ProtocolError> {
    let mut magic = [0; 8];
    let mut offset = 0;
    while offset < magic.len() {
        match reader.read(&mut magic[offset..]) {
            Ok(0) if offset == 0 => {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
            }
            Ok(0) => return Err(ProtocolError::malformed("truncated capture magic")),
            Ok(read) => offset += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof && offset > 0 => {
                return Err(ProtocolError::malformed("truncated capture magic"));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(magic)
}

/// Streaming parser for bounded interleaved F32 microphone blocks.
pub struct AudioBlockReader<R> {
    reader: R,
    clock_domain: ClockDomainId,
    previous_sequence: Option<u64>,
    previous_native_dropped_total: Option<u64>,
    previous_pts_nanos: Option<i64>,
    expected_format: Option<(SampleRate, u8)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CapturedAudioBlock {
    pub block: AudioBlock,
    pub native_dropped_total: u64,
}

impl<R: Read> AudioBlockReader<R> {
    /// Validates stream magic and starts a framed audio reader.
    ///
    /// # Errors
    ///
    /// Returns an error when the stream is truncated or has the wrong magic.
    pub fn new(reader: R, clock_domain: ClockDomainId) -> Result<Self, ProtocolError> {
        Self::new_inner(reader, clock_domain, None)
    }

    /// Validates stream magic and requires every block to match the negotiated
    /// sample rate and channel count.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsupported expected format, truncation, or
    /// invalid stream magic.
    pub fn new_with_format(
        reader: R,
        clock_domain: ClockDomainId,
        sample_rate: SampleRate,
        channels: u8,
    ) -> Result<Self, ProtocolError> {
        if sample_rate.hertz() > MAX_AUDIO_SAMPLE_RATE
            || !(1..=MAX_AUDIO_CHANNELS).contains(&channels)
        {
            return Err(ProtocolError::malformed(
                "unsupported expected audio format",
            ));
        }
        Self::new_inner(reader, clock_domain, Some((sample_rate, channels)))
    }

    fn new_inner(
        mut reader: R,
        clock_domain: ClockDomainId,
        expected_format: Option<(SampleRate, u8)>,
    ) -> Result<Self, ProtocolError> {
        let mut magic = [0; 8];
        reader.read_exact(&mut magic)?;
        if &magic != AUDIO_CAPTURE_MAGIC {
            return Err(ProtocolError::malformed("invalid audio capture magic"));
        }
        Ok(Self {
            reader,
            clock_domain,
            previous_sequence: None,
            previous_native_dropped_total: None,
            previous_pts_nanos: None,
            expected_format,
        })
    }

    /// Reads and validates one audio record. Clean EOF between records returns
    /// `None`; a partial or malformed record is an error.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid bounds, format, samples, sequence, timing,
    /// native-drop telemetry, payload, or underlying I/O.
    #[allow(clippy::too_many_lines)]
    pub fn read_captured_block(&mut self) -> Result<Option<CapturedAudioBlock>, ProtocolError> {
        let Some(record_len) = read_optional_u32(&mut self.reader)? else {
            return Ok(None);
        };
        let record_len = usize::try_from(record_len)
            .map_err(|_| ProtocolError::malformed("audio record length does not fit usize"))?;
        let maximum_record = AUDIO_BLOCK_METADATA_BYTES + MAX_AUDIO_BLOCK_BYTES;
        if !(AUDIO_BLOCK_METADATA_BYTES..=maximum_record).contains(&record_len) {
            return Err(ProtocolError::malformed(format!(
                "audio record length {record_len} is outside {AUDIO_BLOCK_METADATA_BYTES}..={maximum_record}"
            )));
        }
        let payload_from_record = record_len - AUDIO_BLOCK_METADATA_BYTES;
        let mut metadata = [0; AUDIO_BLOCK_METADATA_BYTES];
        self.reader.read_exact(&mut metadata)?;
        let mut cursor = Cursor::new(&metadata);
        let sequence = cursor.u64()?;
        let native_dropped_total = cursor.u64()?;
        let pts_value = cursor.i64()?;
        let pts_timescale = cursor.i32()?;
        let sample_rate_hz = cursor.u32()?;
        let channels = cursor.u8()?;
        let sample_count = usize::try_from(cursor.u32()?)
            .map_err(|_| ProtocolError::malformed("audio sample count does not fit usize"))?;
        let payload_len = usize::try_from(cursor.u32()?)
            .map_err(|_| ProtocolError::malformed("audio payload length does not fit usize"))?;

        if let Some(previous) = self.previous_sequence {
            let expected = previous
                .checked_add(1)
                .ok_or_else(|| ProtocolError::malformed("audio capture sequence overflow"))?;
            if sequence != expected {
                return Err(ProtocolError::malformed(format!(
                    "audio capture sequence {sequence} does not follow {previous}"
                )));
            }
        }
        if let Some(previous) = self.previous_native_dropped_total
            && native_dropped_total < previous
        {
            return Err(ProtocolError::malformed(format!(
                "audio native dropped total {native_dropped_total} is less than {previous}"
            )));
        }
        if pts_timescale <= 0 {
            return Err(ProtocolError::malformed(
                "audio PTS timescale must be positive",
            ));
        }
        let sample_rate = SampleRate::new(sample_rate_hz)
            .ok_or_else(|| ProtocolError::malformed("audio sample rate must be positive"))?;
        if sample_rate_hz > MAX_AUDIO_SAMPLE_RATE {
            return Err(ProtocolError::malformed(format!(
                "audio sample rate {sample_rate_hz} exceeds {MAX_AUDIO_SAMPLE_RATE}"
            )));
        }
        if !(1..=MAX_AUDIO_CHANNELS).contains(&channels) {
            return Err(ProtocolError::malformed(format!(
                "unsupported audio channel count {channels}"
            )));
        }
        if let Some(expected) = self.expected_format
            && expected != (sample_rate, channels)
        {
            return Err(ProtocolError::malformed(format!(
                "captured audio format {sample_rate_hz} Hz/{channels} channels does not match negotiated {} Hz/{} channels",
                expected.0.hertz(),
                expected.1
            )));
        }
        if !(1..=MAX_AUDIO_SAMPLES_PER_BLOCK).contains(&sample_count) {
            return Err(ProtocolError::malformed(format!(
                "audio sample count {sample_count} is outside 1..={MAX_AUDIO_SAMPLES_PER_BLOCK}"
            )));
        }
        let expected_payload = sample_count
            .checked_mul(usize::from(channels))
            .and_then(|samples| samples.checked_mul(size_of::<f32>()))
            .ok_or_else(|| ProtocolError::malformed("audio payload size overflow"))?;
        if expected_payload != payload_len || payload_from_record != payload_len {
            return Err(ProtocolError::malformed(
                "audio record metadata and payload lengths disagree",
            ));
        }

        let pts_scale = u32::try_from(pts_timescale)
            .map_err(|_| ProtocolError::malformed("invalid audio PTS timescale"))?;
        let time_base = TimeBase::new(1, pts_scale).map_err(|error| {
            ProtocolError::malformed(format!("invalid audio PTS timebase: {error}"))
        })?;
        let original = OriginalTimestamp::new(MediaTimestamp::new(pts_value), time_base);
        let normalized = original
            .normalize()
            .map_err(|error| ProtocolError::malformed(format!("invalid audio PTS: {error}")))?;
        if let Some(previous) = self.previous_pts_nanos
            && normalized.as_nanos() <= previous
        {
            return Err(ProtocolError::malformed(format!(
                "audio capture PTS {} does not follow {previous}",
                normalized.as_nanos()
            )));
        }
        let duration_nanos = u128::try_from(sample_count)
            .ok()
            .and_then(|samples| samples.checked_mul(1_000_000_000))
            .map(|nanos| nanos / u128::from(sample_rate_hz))
            .and_then(|nanos| u64::try_from(nanos).ok())
            .ok_or_else(|| ProtocolError::malformed("audio duration normalization overflow"))?;
        let duration = NormalizedDuration::from_nanos(duration_nanos).map_err(|error| {
            ProtocolError::malformed(format!("invalid audio duration: {error}"))
        })?;

        let mut payload = vec![0; payload_len];
        self.reader.read_exact(&mut payload)?;
        let mut planes = vec![Vec::with_capacity(sample_count); usize::from(channels)];
        for (index, bytes) in payload.chunks_exact(4).enumerate() {
            let sample =
                f32::from_le_bytes(bytes.try_into().map_err(|_| {
                    ProtocolError::malformed("invalid audio sample representation")
                })?);
            if !sample.is_finite() {
                return Err(ProtocolError::malformed(
                    "audio payload contains a non-finite sample",
                ));
            }
            planes[index % usize::from(channels)].push(sample);
        }
        let channel_layout = match channels {
            1 => ChannelLayout::new(vec![Channel::Mono]),
            2 => Some(ChannelLayout::stereo()),
            _ => unreachable!("validated audio channel count"),
        }
        .ok_or_else(|| ProtocolError::malformed("invalid audio channel layout"))?;
        let timing = MediaTiming::new(
            original,
            normalized,
            duration,
            self.clock_domain,
            SequenceNumber::new(sequence),
        )
        .map_err(|error| ProtocolError::malformed(format!("invalid audio timing: {error}")))?;
        let block = AudioBlock::new(timing, sample_rate, channel_layout, planes)
            .map_err(|error| ProtocolError::malformed(format!("invalid audio block: {error}")))?;
        self.previous_sequence = Some(sequence);
        self.previous_native_dropped_total = Some(native_dropped_total);
        self.previous_pts_nanos = Some(normalized.as_nanos());
        Ok(Some(CapturedAudioBlock {
            block,
            native_dropped_total,
        }))
    }

    /// Reads the next block while discarding native cumulative drop telemetry.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::read_captured_block`].
    pub fn read_block(&mut self) -> Result<Option<AudioBlock>, ProtocolError> {
        self.read_captured_block()
            .map(|captured| captured.map(|captured| captured.block))
    }
}

fn read_optional_u32(reader: &mut impl Read) -> Result<Option<u32>, ProtocolError> {
    let mut bytes = [0; 4];
    loop {
        match reader.read(&mut bytes[..1]) {
            Ok(0) => return Ok(None),
            Ok(1) => break,
            Ok(_) => unreachable!("one-byte read returned more than one byte"),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    reader.read_exact(&mut bytes[1..]).map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            ProtocolError::malformed("truncated frame record length")
        } else {
            error.into()
        }
    })?;
    Ok(Some(u32::from_le_bytes(bytes)))
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    const fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], ProtocolError> {
        let end = self
            .position
            .checked_add(count)
            .ok_or_else(|| ProtocolError::malformed("protocol cursor overflow"))?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| ProtocolError::malformed("truncated helper output"))?;
        self.position = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, ProtocolError> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| ProtocolError::malformed("invalid u32"))?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn i32(&mut self) -> Result<i32, ProtocolError> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| ProtocolError::malformed("invalid i32"))?;
        Ok(i32::from_le_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, ProtocolError> {
        let bytes: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| ProtocolError::malformed("invalid u64"))?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn i64(&mut self) -> Result<i64, ProtocolError> {
        let bytes: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| ProtocolError::malformed("invalid i64"))?;
        Ok(i64::from_le_bytes(bytes))
    }

    fn count(&mut self, name: &str, maximum: usize) -> Result<usize, ProtocolError> {
        let count = usize::try_from(self.u32()?)
            .map_err(|_| ProtocolError::malformed(format!("{name} count does not fit usize")))?;
        if count > maximum {
            return Err(ProtocolError::malformed(format!(
                "{name} count {count} exceeds {maximum}"
            )));
        }
        Ok(count)
    }

    fn string(&mut self, name: &str) -> Result<String, ProtocolError> {
        let length = self.count(name, MAX_STRING_BYTES)?;
        let bytes = self.take(length)?;
        let value = std::str::from_utf8(bytes)
            .map_err(|_| ProtocolError::malformed(format!("{name} is not UTF-8")))?;
        Ok(value.to_owned())
    }
}
