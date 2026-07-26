use core::fmt;

use fm_frame::{CpuVideoPayload, VideoPayloadError};
use fm_types::{
    ChromaLocation, ColorMetadata, ColorPrimaries, MatrixCoefficients, PixelFormat, SignalRange,
    TransferFunction, VideoFrameMetadata,
};
use fm_video::{FrameError, ImageFrame};

use crate::{
    AlphaMode, Lut1D, Lut3D, MatrixError, Rgb, ToneMapError, ToneMapPolicy, TransferError,
    convert_primaries, decode_rgb_range, decode_transfer, encode_rgb_range, encode_transfer,
    tone_map_rgb, yuv_to_rgb,
};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LinearRgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl LinearRgba {
    #[must_use]
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    #[must_use]
    pub const fn rgb(self) -> Rgb {
        Rgb::new(self.r, self.g, self.b)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LinearFrame {
    width: u32,
    height: u32,
    pixels: Vec<LinearRgba>,
}

impl LinearFrame {
    /// Creates a tightly packed premultiplied linear BT.2020 frame.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid dimensions, a pixel-count mismatch, or a
    /// non-finite/negative pixel. Transparent pixels must have zero RGB.
    pub fn new(width: u32, height: u32, pixels: Vec<LinearRgba>) -> Result<Self, ColorError> {
        let expected = pixel_count(width, height)?;
        if pixels.len() != expected {
            return Err(ColorError::PixelCountMismatch {
                expected,
                actual: pixels.len(),
            });
        }
        if pixels.iter().any(|pixel| {
            !pixel.r.is_finite()
                || !pixel.g.is_finite()
                || !pixel.b.is_finite()
                || !pixel.a.is_finite()
                || !(0.0..=1.0).contains(&pixel.a)
                || pixel.r < 0.0
                || pixel.g < 0.0
                || pixel.b < 0.0
                || (pixel.a == 0.0 && (pixel.r != 0.0 || pixel.g != 0.0 || pixel.b != 0.0))
        }) {
            return Err(ColorError::InvalidPremultipliedPixel);
        }
        Ok(Self {
            width,
            height,
            pixels,
        })
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    #[must_use]
    pub fn pixels(&self) -> &[LinearRgba] {
        &self.pixels
    }

    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<LinearRgba> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let index = usize::try_from(y).ok()? * usize::try_from(self.width).ok()?
            + usize::try_from(x).ok()?;
        self.pixels.get(index).copied()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecodedFrame {
    pub frame: LinearFrame,
    pub metadata: ColorMetadata,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConvertedImage {
    pub image: ImageFrame,
    pub metadata: ColorMetadata,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorPipeline<'a> {
    pub source: ColorMetadata,
    pub output: ColorMetadata,
    pub source_alpha: AlphaMode,
    pub output_alpha: AlphaMode,
    pub tone_map: ToneMapPolicy,
    pub lut_1d: Option<&'a Lut1D>,
    pub lut_3d: Option<&'a Lut3D>,
}

impl ColorPipeline<'_> {
    #[must_use]
    pub const fn new(source: ColorMetadata, output: ColorMetadata) -> Self {
        Self {
            source,
            output,
            source_alpha: AlphaMode::Straight,
            output_alpha: AlphaMode::Straight,
            tone_map: ToneMapPolicy::None,
            lut_1d: None,
            lut_3d: None,
        }
    }

    /// Decodes an RGBA8 image into the canonical working representation.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed layout, non-finite color math, or an
    /// invalid working pixel.
    pub fn decode_image(&self, source: &ImageFrame) -> Result<DecodedFrame, ColorError> {
        let mut pixels = Vec::with_capacity(pixel_count(source.width(), source.height())?);
        for y in 0..source.height() {
            for x in 0..source.width() {
                let pixel = source.pixel(x, y).ok_or(ColorError::MalformedPayload)?;
                pixels.push(self.decode_rgba(
                    Rgb::new(
                        f32::from(pixel.r) / 255.0,
                        f32::from(pixel.g) / 255.0,
                        f32::from(pixel.b) / 255.0,
                    ),
                    f32::from(pixel.a) / 255.0,
                )?);
            }
        }
        Ok(DecodedFrame {
            frame: LinearFrame::new(source.width(), source.height(), pixels)?,
            metadata: working_color_metadata(),
        })
    }

    /// Decodes every currently defined CPU payload format.
    ///
    /// NV12 uses interleaved UV, P010 uses little-endian 10-bit values in the
    /// high bits, and YUV422 uses packed YUYV byte order.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed payload layout, unsupported YUV matrix
    /// metadata, non-finite color math, or an invalid working pixel.
    pub fn decode_cpu_payload(&self, source: &CpuVideoPayload) -> Result<DecodedFrame, ColorError> {
        let dimensions = source.dimensions();
        let mut pixels = Vec::with_capacity(pixel_count(dimensions.width(), dimensions.height())?);
        for y in 0..dimensions.height() {
            for x in 0..dimensions.width() {
                let (rgb, alpha) = read_payload_pixel(source, x, y, self.source)?;
                pixels.push(self.decode_rgba(rgb, alpha)?);
            }
        }
        Ok(DecodedFrame {
            frame: LinearFrame::new(dimensions.width(), dimensions.height(), pixels)?,
            metadata: working_color_metadata(),
        })
    }

    /// Encodes a canonical frame as RGBA8 and propagates requested output metadata.
    ///
    /// # Errors
    ///
    /// Returns an error for output layout overflow or non-finite color math.
    pub fn encode_image(&self, source: &LinearFrame) -> Result<ConvertedImage, ColorError> {
        let width = source.width();
        let height = source.height();
        let stride = usize::try_from(width)
            .ok()
            .and_then(|value| value.checked_mul(4))
            .ok_or(ColorError::LayoutOverflow)?;
        let mut bytes = Vec::with_capacity(
            stride
                .checked_mul(usize::try_from(height).map_err(|_| ColorError::LayoutOverflow)?)
                .ok_or(ColorError::LayoutOverflow)?,
        );
        for pixel in source.pixels() {
            let (rgb, alpha) = self.encode_rgba(*pixel)?;
            bytes.extend_from_slice(&[
                quantize(rgb.r),
                quantize(rgb.g),
                quantize(rgb.b),
                quantize(alpha),
            ]);
        }
        Ok(ConvertedImage {
            image: ImageFrame::new(width, height, stride, bytes)?,
            metadata: self.output,
        })
    }

    /// Runs the complete reference transform for an RGBA8 image.
    ///
    /// # Errors
    ///
    /// Returns any decode, linear processing, or encode error.
    pub fn convert_image(&self, source: &ImageFrame) -> Result<ConvertedImage, ColorError> {
        let mut decoded = self.decode_image(source)?;
        self.process(&mut decoded.frame)?;
        self.encode_image(&decoded.frame)
    }

    /// Runs the complete reference transform for a format-aware CPU payload.
    ///
    /// # Errors
    ///
    /// Returns any payload decode, linear processing, or encode error.
    pub fn convert_cpu_payload(
        &self,
        source: &CpuVideoPayload,
    ) -> Result<ConvertedImage, ColorError> {
        let mut decoded = self.decode_cpu_payload(source)?;
        self.process(&mut decoded.frame)?;
        self.encode_image(&decoded.frame)
    }

    fn process(&self, frame: &mut LinearFrame) -> Result<(), ColorError> {
        for pixel in &mut frame.pixels {
            if pixel.a == 0.0 {
                *pixel = LinearRgba::default();
                continue;
            }
            let inverse_alpha = 1.0 / pixel.a;
            let mut rgb = pixel.rgb().map(|value| value * inverse_alpha);
            rgb = tone_map_rgb(rgb, self.tone_map)?;
            if let Some(lut) = self.lut_1d {
                rgb = lut.sample(rgb);
            }
            if let Some(lut) = self.lut_3d {
                rgb = lut.sample(rgb);
            }
            rgb = rgb.map(|value| value.max(0.0) * pixel.a);
            pixel.r = rgb.r;
            pixel.g = rgb.g;
            pixel.b = rgb.b;
        }
        Ok(())
    }

    fn decode_rgba(&self, encoded: Rgb, alpha: f32) -> Result<LinearRgba, ColorError> {
        let alpha = alpha.clamp(0.0, 1.0);
        let mut encoded = decode_rgb_range(encoded, self.source.range)?;
        if self.source_alpha == AlphaMode::Premultiplied {
            if alpha == 0.0 {
                encoded = Rgb::default();
            } else {
                encoded = encoded.map(|value| value / alpha);
            }
        }
        let mut linear = Rgb::new(
            decode_component(self.source.transfer, encoded.r)?,
            decode_component(self.source.transfer, encoded.g)?,
            decode_component(self.source.transfer, encoded.b)?,
        );
        linear = convert_primaries(linear, self.source.primaries, ColorPrimaries::Bt2020)?;
        linear = linear.map(|value| value.max(0.0) * alpha);
        Ok(LinearRgba::new(linear.r, linear.g, linear.b, alpha))
    }

    fn encode_rgba(&self, pixel: LinearRgba) -> Result<(Rgb, f32), ColorError> {
        let alpha = pixel.a.clamp(0.0, 1.0);
        let straight = if alpha == 0.0 {
            Rgb::default()
        } else {
            pixel.rgb().map(|value| value / alpha)
        };
        let linear = convert_primaries(straight, ColorPrimaries::Bt2020, self.output.primaries)?;
        let mut encoded = Rgb::new(
            encode_component(self.output.transfer, linear.r)?,
            encode_component(self.output.transfer, linear.g)?,
            encode_component(self.output.transfer, linear.b)?,
        );
        if self.output_alpha == AlphaMode::Premultiplied {
            encoded = encoded.map(|value| value * alpha);
        }
        encoded = encode_rgb_range(encoded, self.output.range)?;
        Ok((encoded, alpha))
    }
}

#[derive(Debug)]
pub enum ColorError {
    Transfer(TransferError),
    Matrix(MatrixError),
    ToneMap(ToneMapError),
    Frame(FrameError),
    Payload(VideoPayloadError),
    PixelCountMismatch { expected: usize, actual: usize },
    InvalidPremultipliedPixel,
    MalformedPayload,
    LayoutOverflow,
}

impl fmt::Display for ColorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transfer(error) => error.fmt(formatter),
            Self::Matrix(error) => error.fmt(formatter),
            Self::ToneMap(error) => error.fmt(formatter),
            Self::Frame(error) => error.fmt(formatter),
            Self::Payload(error) => error.fmt(formatter),
            Self::PixelCountMismatch { expected, actual } => {
                write!(formatter, "frame has {actual} pixels, expected {expected}")
            }
            Self::InvalidPremultipliedPixel => {
                formatter.write_str("working pixels must be finite premultiplied linear RGBA")
            }
            Self::MalformedPayload => formatter.write_str("validated CPU payload is malformed"),
            Self::LayoutOverflow => formatter.write_str("color frame layout overflow"),
        }
    }
}

impl std::error::Error for ColorError {}

impl From<TransferError> for ColorError {
    fn from(value: TransferError) -> Self {
        Self::Transfer(value)
    }
}

impl From<MatrixError> for ColorError {
    fn from(value: MatrixError) -> Self {
        Self::Matrix(value)
    }
}

impl From<ToneMapError> for ColorError {
    fn from(value: ToneMapError) -> Self {
        Self::ToneMap(value)
    }
}

impl From<FrameError> for ColorError {
    fn from(value: FrameError) -> Self {
        Self::Frame(value)
    }
}

impl From<VideoPayloadError> for ColorError {
    fn from(value: VideoPayloadError) -> Self {
        Self::Payload(value)
    }
}

#[must_use]
pub const fn working_color_metadata() -> ColorMetadata {
    ColorMetadata {
        primaries: ColorPrimaries::Bt2020,
        transfer: TransferFunction::Linear,
        matrix: MatrixCoefficients::Identity,
        range: SignalRange::Full,
        chroma_location: ChromaLocation::Center,
    }
}

/// Returns the canonical premultiplied linear-light BT.2020 RGB metadata.
#[must_use]
pub const fn working_video_frame_metadata() -> VideoFrameMetadata {
    VideoFrameMetadata::new(working_color_metadata(), Some(AlphaMode::Premultiplied))
}

fn pixel_count(width: u32, height: u32) -> Result<usize, ColorError> {
    if width == 0 || height == 0 {
        return Err(ColorError::LayoutOverflow);
    }
    usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or(ColorError::LayoutOverflow)
}

fn decode_component(transfer: TransferFunction, encoded: f32) -> Result<f32, TransferError> {
    let decoded = decode_transfer(transfer, encoded)?;
    Ok(if transfer == TransferFunction::Pq {
        decoded * 100.0
    } else {
        decoded
    })
}

fn encode_component(transfer: TransferFunction, linear: f32) -> Result<f32, TransferError> {
    encode_transfer(
        transfer,
        if transfer == TransferFunction::Pq {
            linear / 100.0
        } else {
            linear
        },
    )
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn quantize(value: f32) -> u8 {
    // Clamping after rounding proves this conversion is within u8 range.
    value.mul_add(255.0, 0.5).floor().clamp(0.0, 255.0) as u8
}

#[allow(clippy::too_many_lines)]
fn read_payload_pixel(
    payload: &CpuVideoPayload,
    x: u32,
    y: u32,
    metadata: ColorMetadata,
) -> Result<(Rgb, f32), ColorError> {
    match payload.format() {
        PixelFormat::Rgba8 | PixelFormat::Bgra8 => {
            let plane = payload.plane(0).ok_or(ColorError::MalformedPayload)?;
            let offset = row_offset(plane.stride(), x, y, 4)?;
            let bytes = plane
                .bytes()
                .get(offset..offset + 4)
                .ok_or(ColorError::MalformedPayload)?;
            let (red, blue) = if payload.format() == PixelFormat::Rgba8 {
                (bytes[0], bytes[2])
            } else {
                (bytes[2], bytes[0])
            };
            Ok((
                Rgb::new(
                    f32::from(red) / 255.0,
                    f32::from(bytes[1]) / 255.0,
                    f32::from(blue) / 255.0,
                ),
                f32::from(bytes[3]) / 255.0,
            ))
        }
        PixelFormat::Rgba16Float => {
            let plane = payload.plane(0).ok_or(ColorError::MalformedPayload)?;
            let offset = row_offset(plane.stride(), x, y, 8)?;
            let bytes = plane
                .bytes()
                .get(offset..offset + 8)
                .ok_or(ColorError::MalformedPayload)?;
            Ok((
                Rgb::new(
                    half_to_f32(u16::from_le_bytes([bytes[0], bytes[1]])),
                    half_to_f32(u16::from_le_bytes([bytes[2], bytes[3]])),
                    half_to_f32(u16::from_le_bytes([bytes[4], bytes[5]])),
                ),
                half_to_f32(u16::from_le_bytes([bytes[6], bytes[7]])),
            ))
        }
        PixelFormat::Nv12 => {
            let luma = payload.plane(0).ok_or(ColorError::MalformedPayload)?;
            let chroma = payload.plane(1).ok_or(ColorError::MalformedPayload)?;
            let y_offset = row_offset(luma.stride(), x, y, 1)?;
            let uv_offset = row_offset(chroma.stride(), x / 2, y / 2, 2)?;
            let y_value = *luma
                .bytes()
                .get(y_offset)
                .ok_or(ColorError::MalformedPayload)?;
            let uv = chroma
                .bytes()
                .get(uv_offset..uv_offset + 2)
                .ok_or(ColorError::MalformedPayload)?;
            let rgb = yuv_to_rgb(
                crate::Yuv::new(
                    f32::from(y_value) / 255.0,
                    f32::from(uv[0]) / 255.0,
                    f32::from(uv[1]) / 255.0,
                ),
                metadata.matrix,
                metadata.range,
            )?;
            Ok((rgb, 1.0))
        }
        PixelFormat::P010 => {
            let luma = payload.plane(0).ok_or(ColorError::MalformedPayload)?;
            let chroma = payload.plane(1).ok_or(ColorError::MalformedPayload)?;
            let y_offset = row_offset(luma.stride(), x, y, 2)?;
            let uv_offset = row_offset(chroma.stride(), x / 2, y / 2, 4)?;
            let y_bytes = luma
                .bytes()
                .get(y_offset..y_offset + 2)
                .ok_or(ColorError::MalformedPayload)?;
            let uv = chroma
                .bytes()
                .get(uv_offset..uv_offset + 4)
                .ok_or(ColorError::MalformedPayload)?;
            let component =
                |low: u8, high: u8| f32::from(u16::from_le_bytes([low, high]) >> 6) / 1023.0;
            let rgb = yuv_to_rgb(
                crate::Yuv::new(
                    component(y_bytes[0], y_bytes[1]),
                    component(uv[0], uv[1]),
                    component(uv[2], uv[3]),
                ),
                metadata.matrix,
                metadata.range,
            )?;
            Ok((rgb, 1.0))
        }
        PixelFormat::Yuv422 => {
            let plane = payload.plane(0).ok_or(ColorError::MalformedPayload)?;
            let offset = row_offset(plane.stride(), x / 2, y, 4)?;
            let bytes = plane
                .bytes()
                .get(offset..offset + 4)
                .ok_or(ColorError::MalformedPayload)?;
            let y_value = if x.is_multiple_of(2) {
                bytes[0]
            } else {
                bytes[2]
            };
            let rgb = yuv_to_rgb(
                crate::Yuv::new(
                    f32::from(y_value) / 255.0,
                    f32::from(bytes[1]) / 255.0,
                    f32::from(bytes[3]) / 255.0,
                ),
                metadata.matrix,
                metadata.range,
            )?;
            Ok((rgb, 1.0))
        }
    }
}

fn row_offset(stride: usize, x: u32, y: u32, bytes_per_pixel: usize) -> Result<usize, ColorError> {
    usize::try_from(y)
        .ok()
        .and_then(|y| y.checked_mul(stride))
        .and_then(|row| {
            usize::try_from(x)
                .ok()
                .and_then(|x| x.checked_mul(bytes_per_pixel))
                .and_then(|column| row.checked_add(column))
        })
        .ok_or(ColorError::LayoutOverflow)
}

fn half_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits & 0x8000) << 16;
    let exponent = (bits >> 10) & 0x1f;
    let fraction = u32::from(bits & 0x03ff);
    let result = match exponent {
        0 if fraction == 0 => sign,
        0 => {
            let leading = fraction.leading_zeros() - 22;
            let normalized = (fraction << (leading + 1)) & 0x03ff;
            let result_exponent = 127_u32 - 15 - leading;
            sign | (result_exponent << 23) | (normalized << 13)
        }
        0x1f => sign | 0x7f80_0000 | (fraction << 13),
        _ => sign | ((u32::from(exponent) + 112) << 23) | (fraction << 13),
    };
    f32::from_bits(result)
}
