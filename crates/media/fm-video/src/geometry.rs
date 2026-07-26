use crate::{FrameError, ImageFrame};
use core::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CropRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl CropRect {
    #[must_use]
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CropError {
    ZeroWidth,
    ZeroHeight,
    BoundsOverflow,
    OutOfBounds { frame_width: u32, frame_height: u32 },
    Frame(FrameError),
}

impl fmt::Display for CropError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroWidth => formatter.write_str("crop width must be nonzero"),
            Self::ZeroHeight => formatter.write_str("crop height must be nonzero"),
            Self::BoundsOverflow => formatter.write_str("crop bounds overflow"),
            Self::OutOfBounds {
                frame_width,
                frame_height,
            } => write!(
                formatter,
                "crop exceeds frame bounds {frame_width}x{frame_height}"
            ),
            Self::Frame(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CropError {}

impl From<FrameError> for CropError {
    fn from(value: FrameError) -> Self {
        Self::Frame(value)
    }
}

/// Copies a validated rectangular region into a packed frame.
///
/// # Errors
///
/// Returns a typed error for an empty, overflowing, or out-of-bounds crop, or
/// when the bounded output frame cannot be allocated.
pub fn crop(source: &ImageFrame, rect: CropRect) -> Result<ImageFrame, CropError> {
    if rect.width == 0 {
        return Err(CropError::ZeroWidth);
    }
    if rect.height == 0 {
        return Err(CropError::ZeroHeight);
    }
    let right = rect
        .x
        .checked_add(rect.width)
        .ok_or(CropError::BoundsOverflow)?;
    let bottom = rect
        .y
        .checked_add(rect.height)
        .ok_or(CropError::BoundsOverflow)?;
    if right > source.width() || bottom > source.height() {
        return Err(CropError::OutOfBounds {
            frame_width: source.width(),
            frame_height: source.height(),
        });
    }

    let mut output = ImageFrame::packed(rect.width, rect.height)?;
    for y in 0..rect.height {
        for x in 0..rect.width {
            let pixel = source
                .pixel(rect.x + x, rect.y + y)
                .ok_or(CropError::Frame(FrameError::LayoutOverflow))?;
            output.set_pixel(x, y, pixel);
        }
    }
    Ok(output)
}

/// Scales an RGBA8 frame with deterministic nearest-neighbor sampling.
///
/// # Errors
///
/// Returns a [`FrameError`] when the requested output dimensions cannot form a
/// bounded frame.
pub fn scale_nearest(
    source: &ImageFrame,
    width: u32,
    height: u32,
) -> Result<ImageFrame, FrameError> {
    let mut output = ImageFrame::packed(width, height)?;
    for y in 0..height {
        let source_y = map_nearest(y, source.height(), height);
        for x in 0..width {
            let source_x = map_nearest(x, source.width(), width);
            let pixel = source
                .pixel(source_x, source_y)
                .ok_or(FrameError::LayoutOverflow)?;
            output.set_pixel(x, y, pixel);
        }
    }
    Ok(output)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Rotation {
    #[default]
    Deg0,
    Deg90,
    Deg180,
    Deg270,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Transform {
    pub translation_x: i32,
    pub translation_y: i32,
    pub scale_width: u32,
    pub scale_height: u32,
    pub rotation: Rotation,
}

impl Transform {
    #[must_use]
    pub const fn new(
        translation_x: i32,
        translation_y: i32,
        scale_width: u32,
        scale_height: u32,
        rotation: Rotation,
    ) -> Self {
        Self {
            translation_x,
            translation_y,
            scale_width,
            scale_height,
            rotation,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransformError {
    ZeroScaleWidth,
    ZeroScaleHeight,
    Frame(FrameError),
}

impl fmt::Display for TransformError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroScaleWidth => formatter.write_str("transform scale width must be nonzero"),
            Self::ZeroScaleHeight => formatter.write_str("transform scale height must be nonzero"),
            Self::Frame(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for TransformError {}

impl From<FrameError> for TransformError {
    fn from(value: FrameError) -> Self {
        Self::Frame(value)
    }
}

/// Renders a translated, scaled, and clockwise quarter-turned source.
///
/// Scaling is applied before rotation. Translation positions the top-left of
/// the rotated bounds on a transparent output canvas. Pixels outside the canvas
/// are clipped.
///
/// # Errors
///
/// Returns a typed error for zero scale dimensions or an invalid output layout.
pub fn transform_nearest(
    source: &ImageFrame,
    output_width: u32,
    output_height: u32,
    transform: Transform,
) -> Result<ImageFrame, TransformError> {
    if transform.scale_width == 0 {
        return Err(TransformError::ZeroScaleWidth);
    }
    if transform.scale_height == 0 {
        return Err(TransformError::ZeroScaleHeight);
    }

    let mut output = ImageFrame::packed(output_width, output_height)?;
    let (rotated_width, rotated_height) = match transform.rotation {
        Rotation::Deg0 | Rotation::Deg180 => (transform.scale_width, transform.scale_height),
        Rotation::Deg90 | Rotation::Deg270 => (transform.scale_height, transform.scale_width),
    };

    let columns = clipped_start(transform.translation_x, rotated_width)
        ..clipped_end(transform.translation_x, rotated_width, output_width);
    let rows = clipped_start(transform.translation_y, rotated_height)
        ..clipped_end(transform.translation_y, rotated_height, output_height);
    for rotated_y in rows {
        for rotated_x in columns.clone() {
            let destination_x = i64::from(transform.translation_x) + i64::from(rotated_x);
            let destination_y = i64::from(transform.translation_y) + i64::from(rotated_y);
            let (scaled_x, scaled_y) = inverse_rotate(
                rotated_x,
                rotated_y,
                transform.scale_width,
                transform.scale_height,
                transform.rotation,
            );
            let source_x = map_nearest(scaled_x, source.width(), transform.scale_width);
            let source_y = map_nearest(scaled_y, source.height(), transform.scale_height);
            let pixel = source
                .pixel(source_x, source_y)
                .ok_or(TransformError::Frame(FrameError::LayoutOverflow))?;
            output.set_pixel(
                u32::try_from(destination_x)
                    .map_err(|_| TransformError::Frame(FrameError::LayoutOverflow))?,
                u32::try_from(destination_y)
                    .map_err(|_| TransformError::Frame(FrameError::LayoutOverflow))?,
                pixel,
            );
        }
    }
    Ok(output)
}

fn clipped_start(position: i32, transformed_size: u32) -> u32 {
    let start = (-i64::from(position)).clamp(0, i64::from(transformed_size));
    u32::try_from(start).unwrap_or(transformed_size)
}

fn clipped_end(position: i32, transformed_size: u32, output_size: u32) -> u32 {
    let end = (i64::from(output_size) - i64::from(position)).clamp(0, i64::from(transformed_size));
    u32::try_from(end).unwrap_or(transformed_size)
}

fn map_nearest(destination: u32, source_size: u32, destination_size: u32) -> u32 {
    let mapped = u64::from(destination) * u64::from(source_size) / u64::from(destination_size);
    u32::try_from(mapped).unwrap_or(source_size - 1)
}

fn inverse_rotate(x: u32, y: u32, width: u32, height: u32, rotation: Rotation) -> (u32, u32) {
    match rotation {
        Rotation::Deg0 => (x, y),
        Rotation::Deg90 => (y, height - 1 - x),
        Rotation::Deg180 => (width - 1 - x, height - 1 - y),
        Rotation::Deg270 => (width - 1 - y, x),
    }
}
