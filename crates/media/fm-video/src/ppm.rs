use crate::ImageFrame;
use std::io::{self, Write};

/// Writes an image as a binary RGB PPM (`P6`) stream.
///
/// Alpha and row padding are omitted.
///
/// # Errors
///
/// Returns the first error reported by the destination writer.
pub fn write_ppm(frame: &ImageFrame, mut writer: impl Write) -> io::Result<()> {
    write!(writer, "P6\n{} {}\n255\n", frame.width(), frame.height())?;
    for y in 0..frame.height() {
        for x in 0..frame.width() {
            let Some(pixel) = frame.pixel(x, y) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "validated frame has an invalid pixel layout",
                ));
            };
            writer.write_all(&[pixel.r, pixel.g, pixel.b])?;
        }
    }
    Ok(())
}
