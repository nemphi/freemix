#![forbid(unsafe_code)]

//! Bounded decoding for static PNG, JPEG, and WebP still images.
//!
//! Images without an ICC profile are decoded without color conversion and
//! tagged as full-range, straight-alpha sRGB BT.709 RGB. Embedded ICC profiles
//! are rejected because frames support only enumerated color metadata.

use std::fmt;
use std::io::Cursor;

use fm_frame::{
    AlphaMode, ChromaLocation, ColorMetadata, ColorPrimaries, CpuVideoFrame, CpuVideoPayload,
    CpuVideoPlane, MatrixCoefficients, MediaTiming, PixelFormat, SignalRange, TransferFunction,
    VideoDimensions, VideoFrameMetadata, VideoFrameMetadataError, VideoPayloadError,
};
use image::codecs::webp::WebPDecoder;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageError, ImageFormat, ImageReader, Limits};

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const JPEG_SIGNATURE: &[u8; 2] = b"\xff\xd8";
const ICC_JPEG_SIGNATURE: &[u8; 12] = b"ICC_PROFILE\0";
const WEBP_SIGNATURE_LENGTH: usize = 12;

/// Encoded still-image formats accepted by [`decode_still`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StillFormat {
    Png,
    Jpeg,
    WebP,
}

/// Caller-controlled resource bounds for one still-image decode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StillDecodeLimits {
    pub max_encoded_bytes: usize,
    pub max_width: u32,
    pub max_height: u32,
    pub max_decoded_rgba_bytes: usize,
    pub max_icc_bytes: usize,
    /// Budget passed to `image` for its decoder allocations.
    pub max_image_alloc_bytes: u64,
}

impl Default for StillDecodeLimits {
    fn default() -> Self {
        Self {
            max_encoded_bytes: 64 * 1024 * 1024,
            max_width: CpuVideoPayload::MAX_WIDTH,
            max_height: CpuVideoPayload::MAX_HEIGHT,
            max_decoded_rgba_bytes: CpuVideoPayload::MAX_TOTAL_BYTES,
            max_icc_bytes: 4 * 1024 * 1024,
            max_image_alloc_bytes: 512 * 1024 * 1024,
        }
    }
}

/// A decoded, tightly packed, straight-alpha RGBA8 still frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedStill {
    pub frame: CpuVideoFrame,
    pub format: StillFormat,
    /// Dimensions encoded in the source, before EXIF orientation.
    pub source_dimensions: VideoDimensions,
    pub orientation_applied: bool,
    pub source_has_alpha: bool,
}

/// Failure from bounded still-image decoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StillDecodeError {
    EmptyInput,
    EncodedBytesTooLarge { actual: usize, maximum: usize },
    UnsupportedFormat,
    AnimatedPngUnsupported,
    AnimatedWebpUnsupported,
    CorruptInput { format: StillFormat },
    WidthTooLarge { actual: u32, maximum: u32 },
    HeightTooLarge { actual: u32, maximum: u32 },
    DecodedRgbaTooLarge { required: u64, maximum: usize },
    IccProfileTooLarge { required: usize, maximum: usize },
    EmbeddedIccUnsupported,
    ImageAllocationLimitExceeded { required: Option<u64>, maximum: u64 },
    FramePayload(VideoPayloadError),
    FrameMetadata(VideoFrameMetadataError),
}

impl fmt::Display for StillDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyInput => formatter.write_str("still image input is empty"),
            Self::EncodedBytesTooLarge { actual, maximum } => write!(
                formatter,
                "encoded image is {actual} bytes, exceeding the {maximum}-byte limit"
            ),
            Self::UnsupportedFormat => {
                formatter.write_str("input is not a supported PNG, JPEG, or WebP image")
            }
            Self::AnimatedPngUnsupported => formatter.write_str("animated PNG is not supported"),
            Self::AnimatedWebpUnsupported => {
                formatter.write_str("animated WebP is not supported")
            }
            Self::CorruptInput { format } => write!(formatter, "corrupt {format} image"),
            Self::WidthTooLarge { actual, maximum } => {
                write!(formatter, "image width {actual} exceeds {maximum}")
            }
            Self::HeightTooLarge { actual, maximum } => {
                write!(formatter, "image height {actual} exceeds {maximum}")
            }
            Self::DecodedRgbaTooLarge { required, maximum } => write!(
                formatter,
                "decoded RGBA image requires {required} bytes, exceeding {maximum}"
            ),
            Self::IccProfileTooLarge { required, maximum } => write!(
                formatter,
                "embedded ICC profile is {required} bytes, exceeding {maximum}"
            ),
            Self::EmbeddedIccUnsupported => formatter.write_str(
                "embedded ICC profiles are unsupported; only enumerated frame metadata is supported",
            ),
            Self::ImageAllocationLimitExceeded { required, maximum } => {
                if let Some(required) = required {
                    write!(
                        formatter,
                        "image decoder requires {required} bytes, exceeding its {maximum}-byte allocation limit"
                    )
                } else {
                    write!(
                        formatter,
                        "image decoder exceeded its {maximum}-byte allocation limit"
                    )
                }
            }
            Self::FramePayload(error) => {
                write!(formatter, "invalid decoded frame payload: {error}")
            }
            Self::FrameMetadata(error) => {
                write!(formatter, "invalid decoded frame metadata: {error}")
            }
        }
    }
}

impl std::error::Error for StillDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::FramePayload(error) => Some(error),
            Self::FrameMetadata(error) => Some(error),
            _ => None,
        }
    }
}

impl fmt::Display for StillFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::WebP => "WebP",
        })
    }
}

/// Decodes static PNG, JPEG, or WebP bytes into straight-alpha RGBA8.
///
/// Format detection uses signatures rather than file names. Every nonempty ICC
/// profile is rejected because the destination supports only enumerated color
/// metadata.
///
/// # Errors
///
/// Returns a typed format, corruption, metadata, resource-limit, or frame-layout
/// error. APNG and animated WebP input are always rejected.
pub fn decode_still(
    encoded: &[u8],
    timing: MediaTiming,
    limits: StillDecodeLimits,
) -> Result<DecodedStill, StillDecodeError> {
    if encoded.is_empty() {
        return Err(StillDecodeError::EmptyInput);
    }
    if encoded.len() > limits.max_encoded_bytes {
        return Err(StillDecodeError::EncodedBytesTooLarge {
            actual: encoded.len(),
            maximum: limits.max_encoded_bytes,
        });
    }

    let format = sniff_still_format(encoded)?;
    match format {
        StillFormat::Png => inspect_png(encoded)?,
        StillFormat::Jpeg if inspect_jpeg_icc(encoded, limits.max_icc_bytes)? => {
            return Err(StillDecodeError::EmbeddedIccUnsupported);
        }
        StillFormat::WebP => {
            let decoder = WebPDecoder::new(Cursor::new(encoded)).map_err(|error| {
                map_image_error(&error, format, limits.max_image_alloc_bytes, None)
            })?;
            if decoder.has_animation() {
                return Err(StillDecodeError::AnimatedWebpUnsupported);
            }
            return decode_with_decoder(decoder, timing, limits, format);
        }
        StillFormat::Jpeg => {}
    }

    let image_format = match format {
        StillFormat::Png => ImageFormat::Png,
        StillFormat::Jpeg => ImageFormat::Jpeg,
        StillFormat::WebP => unreachable!("WebP uses its concrete decoder"),
    };
    let mut reader = ImageReader::with_format(Cursor::new(encoded), image_format);
    let mut image_limits = Limits::default();
    image_limits.max_image_width = None;
    image_limits.max_image_height = None;
    image_limits.max_alloc = Some(limits.max_image_alloc_bytes);
    reader.limits(image_limits);
    let decoder = reader
        .into_decoder()
        .map_err(|error| map_image_error(&error, format, limits.max_image_alloc_bytes, None))?;

    decode_with_decoder(decoder, timing, limits, format)
}

fn decode_with_decoder(
    mut decoder: impl ImageDecoder,
    timing: MediaTiming,
    limits: StillDecodeLimits,
    format: StillFormat,
) -> Result<DecodedStill, StillDecodeError> {
    let (width, height) = decoder.dimensions();
    let layout = validate_layout(width, height, limits)?;

    let native_bytes = decoder.total_bytes();
    if native_bytes > limits.max_image_alloc_bytes {
        return Err(StillDecodeError::ImageAllocationLimitExceeded {
            required: Some(native_bytes),
            maximum: limits.max_image_alloc_bytes,
        });
    }

    if let Some(profile) = decoder
        .icc_profile()
        .map_err(|error| map_image_error(&error, format, limits.max_image_alloc_bytes, None))?
        .filter(|profile| !profile.is_empty())
    {
        if profile.len() > limits.max_icc_bytes {
            return Err(StillDecodeError::IccProfileTooLarge {
                required: profile.len(),
                maximum: limits.max_icc_bytes,
            });
        }
        return Err(StillDecodeError::EmbeddedIccUnsupported);
    }

    let orientation = decoder
        .orientation()
        .map_err(|error| map_image_error(&error, format, limits.max_image_alloc_bytes, None))?;
    let source_has_alpha = decoder.color_type().has_alpha();
    let source_dimensions =
        VideoDimensions::new(width, height).ok_or(StillDecodeError::CorruptInput { format })?;
    let (output_width, output_height) = if matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    ) {
        (height, width)
    } else {
        (width, height)
    };
    if output_width > layout.max_width {
        return Err(StillDecodeError::WidthTooLarge {
            actual: output_width,
            maximum: layout.max_width,
        });
    }
    if output_height > layout.max_height {
        return Err(StillDecodeError::HeightTooLarge {
            actual: output_height,
            maximum: layout.max_height,
        });
    }

    let mut decoder_limits = Limits::default();
    decoder_limits.max_image_width = Some(layout.max_width);
    decoder_limits.max_image_height = Some(layout.max_height);
    decoder_limits.max_alloc = Some(limits.max_image_alloc_bytes - native_bytes);
    decoder.set_limits(decoder_limits).map_err(|error| {
        map_image_error(
            &error,
            format,
            limits.max_image_alloc_bytes,
            Some(native_bytes),
        )
    })?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(|error| {
        map_image_error(
            &error,
            format,
            limits.max_image_alloc_bytes,
            Some(native_bytes),
        )
    })?;
    image.apply_orientation(orientation);
    let rgba = image.into_rgba8();
    let output_dimensions = VideoDimensions::new(rgba.width(), rgba.height())
        .ok_or(StillDecodeError::CorruptInput { format })?;
    let stride = usize::try_from(rgba.width())
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or(StillDecodeError::DecodedRgbaTooLarge {
            required: layout.rgba_bytes,
            maximum: layout.max_rgba_bytes,
        })?;
    let plane =
        CpuVideoPlane::new(stride, rgba.into_raw()).map_err(StillDecodeError::FramePayload)?;
    let payload = CpuVideoPayload::new(PixelFormat::Rgba8, output_dimensions, vec![plane])
        .map_err(StillDecodeError::FramePayload)?;

    let frame = CpuVideoFrame::new(timing, payload)
        .with_metadata(still_frame_metadata())
        .map_err(StillDecodeError::FrameMetadata)?;

    Ok(DecodedStill {
        frame,
        format,
        source_dimensions,
        orientation_applied: orientation != Orientation::NoTransforms,
        source_has_alpha,
    })
}

const fn still_frame_metadata() -> VideoFrameMetadata {
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

#[derive(Clone, Copy)]
struct ValidatedLayout {
    max_width: u32,
    max_height: u32,
    max_rgba_bytes: usize,
    rgba_bytes: u64,
}

fn validate_layout(
    width: u32,
    height: u32,
    limits: StillDecodeLimits,
) -> Result<ValidatedLayout, StillDecodeError> {
    let max_width = limits.max_width.min(CpuVideoPayload::MAX_WIDTH);
    let max_height = limits.max_height.min(CpuVideoPayload::MAX_HEIGHT);
    let max_rgba_bytes = limits
        .max_decoded_rgba_bytes
        .min(CpuVideoPayload::MAX_TOTAL_BYTES);
    if width > max_width {
        return Err(StillDecodeError::WidthTooLarge {
            actual: width,
            maximum: max_width,
        });
    }
    if height > max_height {
        return Err(StillDecodeError::HeightTooLarge {
            actual: height,
            maximum: max_height,
        });
    }

    let rgba_bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(StillDecodeError::DecodedRgbaTooLarge {
            required: u64::MAX,
            maximum: max_rgba_bytes,
        })?;
    if rgba_bytes > u64::try_from(max_rgba_bytes).unwrap_or(u64::MAX) {
        return Err(StillDecodeError::DecodedRgbaTooLarge {
            required: rgba_bytes,
            maximum: max_rgba_bytes,
        });
    }

    Ok(ValidatedLayout {
        max_width,
        max_height,
        max_rgba_bytes,
        rgba_bytes,
    })
}

/// Classifies an encoded prefix as PNG, JPEG, or WebP from its signature bytes.
///
/// Only signature bytes are inspected; no image decoding or validation occurs.
/// File names and extensions are not considered.
///
/// # Errors
///
/// Returns [`StillDecodeError::UnsupportedFormat`] when the prefix does not
/// contain a complete supported signature.
pub fn sniff_still_format(encoded_prefix: &[u8]) -> Result<StillFormat, StillDecodeError> {
    if encoded_prefix.starts_with(PNG_SIGNATURE) {
        Ok(StillFormat::Png)
    } else if encoded_prefix.starts_with(JPEG_SIGNATURE) {
        Ok(StillFormat::Jpeg)
    } else if encoded_prefix.len() >= WEBP_SIGNATURE_LENGTH
        && &encoded_prefix[..4] == b"RIFF"
        && &encoded_prefix[8..WEBP_SIGNATURE_LENGTH] == b"WEBP"
    {
        Ok(StillFormat::WebP)
    } else {
        Err(StillDecodeError::UnsupportedFormat)
    }
}

fn inspect_png(encoded: &[u8]) -> Result<(), StillDecodeError> {
    let mut offset = PNG_SIGNATURE.len();
    while offset < encoded.len() {
        let header_end = offset
            .checked_add(8)
            .ok_or(StillDecodeError::CorruptInput {
                format: StillFormat::Png,
            })?;
        let header = encoded
            .get(offset..header_end)
            .ok_or(StillDecodeError::CorruptInput {
                format: StillFormat::Png,
            })?;
        let length = usize::try_from(u32::from_be_bytes(header[..4].try_into().map_err(
            |_| StillDecodeError::CorruptInput {
                format: StillFormat::Png,
            },
        )?))
        .map_err(|_| StillDecodeError::CorruptInput {
            format: StillFormat::Png,
        })?;
        if &header[4..] == b"acTL" {
            return Err(StillDecodeError::AnimatedPngUnsupported);
        }
        let chunk_end = header_end
            .checked_add(length)
            .and_then(|end| end.checked_add(4))
            .ok_or(StillDecodeError::CorruptInput {
                format: StillFormat::Png,
            })?;
        encoded
            .get(header_end..chunk_end)
            .ok_or(StillDecodeError::CorruptInput {
                format: StillFormat::Png,
            })?;
        offset = chunk_end;
        if &header[4..] == b"IEND" {
            return Ok(());
        }
    }
    Err(StillDecodeError::CorruptInput {
        format: StillFormat::Png,
    })
}

fn inspect_jpeg_icc(encoded: &[u8], maximum: usize) -> Result<bool, StillDecodeError> {
    let mut offset = JPEG_SIGNATURE.len();
    let mut icc_bytes = 0usize;
    while offset < encoded.len() {
        while encoded.get(offset) == Some(&0xff) {
            offset += 1;
        }
        let marker = *encoded.get(offset).ok_or(StillDecodeError::CorruptInput {
            format: StillFormat::Jpeg,
        })?;
        offset += 1;
        if marker == 0xda || marker == 0xd9 {
            return Ok(icc_bytes != 0);
        }
        if matches!(marker, 0x01 | 0xd0..=0xd8) {
            continue;
        }
        let length_end = offset
            .checked_add(2)
            .ok_or(StillDecodeError::CorruptInput {
                format: StillFormat::Jpeg,
            })?;
        let length_bytes =
            encoded
                .get(offset..length_end)
                .ok_or(StillDecodeError::CorruptInput {
                    format: StillFormat::Jpeg,
                })?;
        let length = usize::from(u16::from_be_bytes(length_bytes.try_into().map_err(
            |_| StillDecodeError::CorruptInput {
                format: StillFormat::Jpeg,
            },
        )?));
        if length < 2 {
            return Err(StillDecodeError::CorruptInput {
                format: StillFormat::Jpeg,
            });
        }
        let segment_end = offset
            .checked_add(length)
            .ok_or(StillDecodeError::CorruptInput {
                format: StillFormat::Jpeg,
            })?;
        let payload =
            encoded
                .get(length_end..segment_end)
                .ok_or(StillDecodeError::CorruptInput {
                    format: StillFormat::Jpeg,
                })?;
        if marker == 0xe2 && payload.starts_with(ICC_JPEG_SIGNATURE) {
            let profile_part = payload.get(ICC_JPEG_SIGNATURE.len() + 2..).ok_or(
                StillDecodeError::CorruptInput {
                    format: StillFormat::Jpeg,
                },
            )?;
            icc_bytes = icc_bytes.checked_add(profile_part.len()).ok_or(
                StillDecodeError::IccProfileTooLarge {
                    required: usize::MAX,
                    maximum,
                },
            )?;
            if icc_bytes > maximum {
                return Err(StillDecodeError::IccProfileTooLarge {
                    required: icc_bytes,
                    maximum,
                });
            }
        }
        offset = segment_end;
    }
    Err(StillDecodeError::CorruptInput {
        format: StillFormat::Jpeg,
    })
}

fn map_image_error(
    error: &ImageError,
    format: StillFormat,
    maximum: u64,
    required: Option<u64>,
) -> StillDecodeError {
    if matches!(error, ImageError::Limits(_)) {
        StillDecodeError::ImageAllocationLimitExceeded { required, maximum }
    } else {
        StillDecodeError::CorruptInput { format }
    }
}
