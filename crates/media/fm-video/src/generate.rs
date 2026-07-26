use crate::{FrameError, ImageFrame, Rgba8};

const COLOR_BARS: [Rgba8; 7] = [
    Rgba8::new(191, 191, 191, 255),
    Rgba8::new(191, 191, 0, 255),
    Rgba8::new(0, 191, 191, 255),
    Rgba8::new(0, 191, 0, 255),
    Rgba8::new(191, 0, 191, 255),
    Rgba8::new(191, 0, 0, 255),
    Rgba8::new(0, 0, 191, 255),
];

/// Generates a packed solid-color frame.
///
/// # Errors
///
/// Returns a [`FrameError`] when the requested dimensions cannot form a
/// bounded frame.
pub fn solid_color(width: u32, height: u32, color: Rgba8) -> Result<ImageFrame, FrameError> {
    let mut frame = ImageFrame::packed(width, height)?;
    for pixel in frame.pixels_mut().chunks_exact_mut(4) {
        pixel.copy_from_slice(&color.to_bytes());
    }
    Ok(frame)
}

/// Generates SMPTE-like vertical bars with a one-pixel moving frame marker.
///
/// # Errors
///
/// Returns a [`FrameError`] when the requested dimensions cannot form a
/// bounded frame.
pub fn vertical_color_bars(
    width: u32,
    height: u32,
    frame_number: u64,
) -> Result<ImageFrame, FrameError> {
    let mut frame = ImageFrame::packed(width, height)?;
    let bar_count = u64::try_from(COLOR_BARS.len()).map_err(|_| FrameError::LayoutOverflow)?;
    for y in 0..height {
        for x in 0..width {
            let bar = u64::from(x) * bar_count / u64::from(width);
            let bar = usize::try_from(bar).map_err(|_| FrameError::LayoutOverflow)?;
            frame.set_pixel(x, y, COLOR_BARS[bar]);
        }
    }
    let marker_x = frame_number % u64::from(width);
    frame.set_pixel(
        u32::try_from(marker_x).map_err(|_| FrameError::LayoutOverflow)?,
        height - 1,
        Rgba8::new(0, 0, 0, 255),
    );
    Ok(frame)
}
