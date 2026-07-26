use crate::{FrameError, ImageFrame, Rgba8};
use core::fmt;

#[derive(Clone, Copy, Debug)]
pub struct Layer<'a> {
    pub frame: &'a ImageFrame,
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub opacity: u8,
}

impl<'a> Layer<'a> {
    #[must_use]
    pub const fn new(frame: &'a ImageFrame, x: i32, y: i32, z: i32, opacity: u8) -> Self {
        Self {
            frame,
            x,
            y,
            z,
            opacity,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompositeError {
    NotPremultiplied {
        layer: Option<usize>,
        x: u32,
        y: u32,
        pixel: Rgba8,
    },
    Frame(FrameError),
}

impl fmt::Display for CompositeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotPremultiplied { layer, x, y, pixel } => {
                if let Some(index) = layer {
                    write!(formatter, "layer {index}")?;
                } else {
                    formatter.write_str("background")?;
                }
                write!(
                    formatter,
                    " pixel ({x}, {y}) ({}, {}, {}, {}) is not premultiplied",
                    pixel.r, pixel.g, pixel.b, pixel.a
                )
            }
            Self::Frame(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CompositeError {}

impl From<FrameError> for CompositeError {
    fn from(value: FrameError) -> Self {
        Self::Frame(value)
    }
}

/// Converts straight-alpha RGBA8 to premultiplied-alpha RGBA8.
///
/// # Errors
///
/// Returns a [`FrameError`] if the bounded output frame cannot be allocated.
pub fn premultiply_alpha(source: &ImageFrame) -> Result<ImageFrame, FrameError> {
    let mut output = ImageFrame::packed(source.width(), source.height())?;
    for y in 0..source.height() {
        for x in 0..source.width() {
            let pixel = source.pixel(x, y).ok_or(FrameError::LayoutOverflow)?;
            output.set_pixel(
                x,
                y,
                Rgba8::new(
                    multiply_u8(pixel.r, pixel.a),
                    multiply_u8(pixel.g, pixel.a),
                    multiply_u8(pixel.b, pixel.a),
                    pixel.a,
                ),
            );
        }
    }
    Ok(output)
}

/// Applies opacity to validated premultiplied-alpha RGBA8.
///
/// All four channels are multiplied by `opacity`, preserving premultiplied
/// representation. `0` is transparent and `255` is unchanged.
///
/// # Errors
///
/// Returns a typed error if an input RGB channel exceeds alpha, or if the
/// bounded output frame cannot be allocated.
pub fn apply_opacity_premultiplied(
    source: &ImageFrame,
    opacity: u8,
) -> Result<ImageFrame, CompositeError> {
    validate_premultiplied(source, None)?;
    let mut output = ImageFrame::packed(source.width(), source.height())?;
    for y in 0..source.height() {
        for x in 0..source.width() {
            let pixel = source
                .pixel(x, y)
                .ok_or(CompositeError::Frame(FrameError::LayoutOverflow))?;
            output.set_pixel(x, y, with_opacity(pixel, opacity));
        }
    }
    Ok(output)
}

/// Composes positioned layers over a premultiplied clear color.
///
/// Layers use premultiplied-alpha source-over blending. Lower `z` values are
/// drawn first; equal `z` values preserve slice order. Layer opacity multiplies
/// all four source channels before blending. Off-canvas pixels are clipped.
///
/// # Errors
///
/// Returns a typed error for non-premultiplied input or an invalid output
/// layout.
pub fn compose_layers(
    width: u32,
    height: u32,
    clear: Rgba8,
    layers: &[Layer<'_>],
) -> Result<ImageFrame, CompositeError> {
    if !is_premultiplied(clear) {
        return Err(CompositeError::NotPremultiplied {
            layer: None,
            x: 0,
            y: 0,
            pixel: clear,
        });
    }
    for (index, layer) in layers.iter().enumerate() {
        validate_premultiplied(layer.frame, Some(index))?;
    }

    let mut output = ImageFrame::packed(width, height)?;
    for y in 0..height {
        for x in 0..width {
            output.set_pixel(x, y, clear);
        }
    }

    let mut previous = None;
    for _ in layers {
        let next = layers
            .iter()
            .enumerate()
            .filter(|(index, layer)| previous.is_none_or(|key| (layer.z, *index) > key))
            .min_by_key(|(index, layer)| (layer.z, *index));
        let Some((index, layer)) = next else {
            return Err(CompositeError::Frame(FrameError::LayoutOverflow));
        };
        composite_layer(&mut output, layer)?;
        previous = Some((layer.z, index));
    }
    Ok(output)
}

fn composite_layer(output: &mut ImageFrame, layer: &Layer<'_>) -> Result<(), CompositeError> {
    let columns = clipped_start(layer.x, layer.frame.width())
        ..clipped_end(layer.x, layer.frame.width(), output.width());
    let rows = clipped_start(layer.y, layer.frame.height())
        ..clipped_end(layer.y, layer.frame.height(), output.height());
    for source_y in rows {
        let destination_y = i64::from(layer.y) + i64::from(source_y);
        for source_x in columns.clone() {
            let destination_x = i64::from(layer.x) + i64::from(source_x);
            let x = u32::try_from(destination_x)
                .map_err(|_| CompositeError::Frame(FrameError::LayoutOverflow))?;
            let y = u32::try_from(destination_y)
                .map_err(|_| CompositeError::Frame(FrameError::LayoutOverflow))?;
            let source = layer
                .frame
                .pixel(source_x, source_y)
                .ok_or(CompositeError::Frame(FrameError::LayoutOverflow))?;
            let destination = output
                .pixel(x, y)
                .ok_or(CompositeError::Frame(FrameError::LayoutOverflow))?;
            output.set_pixel(
                x,
                y,
                source_over(with_opacity(source, layer.opacity), destination),
            );
        }
    }
    Ok(())
}

fn clipped_start(position: i32, source_size: u32) -> u32 {
    let start = (-i64::from(position)).clamp(0, i64::from(source_size));
    u32::try_from(start).unwrap_or(source_size)
}

fn clipped_end(position: i32, source_size: u32, output_size: u32) -> u32 {
    let end = (i64::from(output_size) - i64::from(position)).clamp(0, i64::from(source_size));
    u32::try_from(end).unwrap_or(source_size)
}

fn validate_premultiplied(frame: &ImageFrame, layer: Option<usize>) -> Result<(), CompositeError> {
    for y in 0..frame.height() {
        for x in 0..frame.width() {
            let pixel = frame
                .pixel(x, y)
                .ok_or(CompositeError::Frame(FrameError::LayoutOverflow))?;
            if !is_premultiplied(pixel) {
                return Err(CompositeError::NotPremultiplied { layer, x, y, pixel });
            }
        }
    }
    Ok(())
}

const fn is_premultiplied(pixel: Rgba8) -> bool {
    pixel.r <= pixel.a && pixel.g <= pixel.a && pixel.b <= pixel.a
}

fn with_opacity(pixel: Rgba8, opacity: u8) -> Rgba8 {
    Rgba8::new(
        multiply_u8(pixel.r, opacity),
        multiply_u8(pixel.g, opacity),
        multiply_u8(pixel.b, opacity),
        multiply_u8(pixel.a, opacity),
    )
}

fn source_over(source: Rgba8, destination: Rgba8) -> Rgba8 {
    let inverse_alpha = u8::MAX - source.a;
    Rgba8::new(
        source
            .r
            .saturating_add(multiply_u8(destination.r, inverse_alpha)),
        source
            .g
            .saturating_add(multiply_u8(destination.g, inverse_alpha)),
        source
            .b
            .saturating_add(multiply_u8(destination.b, inverse_alpha)),
        source
            .a
            .saturating_add(multiply_u8(destination.a, inverse_alpha)),
    )
}

fn multiply_u8(left: u8, right: u8) -> u8 {
    let product = u16::from(left) * u16::from(right) + 127;
    u8::try_from(product / 255).unwrap_or(u8::MAX)
}
