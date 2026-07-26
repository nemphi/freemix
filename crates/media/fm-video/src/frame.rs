use core::fmt;

const CHANNELS: usize = 4;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Rgba8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba8 {
    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    #[must_use]
    pub const fn to_bytes(self) -> [u8; CHANNELS] {
        [self.r, self.g, self.b, self.a]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameError {
    ZeroWidth,
    ZeroHeight,
    LayoutOverflow,
    StrideTooSmall { minimum: usize, actual: usize },
    BufferTooLarge { required: usize, maximum: usize },
    BufferLengthMismatch { expected: usize, actual: usize },
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroWidth => formatter.write_str("image width must be nonzero"),
            Self::ZeroHeight => formatter.write_str("image height must be nonzero"),
            Self::LayoutOverflow => {
                formatter.write_str("image layout overflows addressable memory")
            }
            Self::StrideTooSmall { minimum, actual } => {
                write!(formatter, "stride {actual} is smaller than {minimum}")
            }
            Self::BufferTooLarge { required, maximum } => {
                write!(formatter, "buffer size {required} exceeds limit {maximum}")
            }
            Self::BufferLengthMismatch { expected, actual } => {
                write!(
                    formatter,
                    "buffer length {actual} does not match {expected}"
                )
            }
        }
    }
}

impl std::error::Error for FrameError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageFrame {
    width: u32,
    height: u32,
    stride: usize,
    pixels: Vec<u8>,
}

impl ImageFrame {
    pub const MAX_BUFFER_BYTES: usize = 512 * 1024 * 1024;

    /// Creates an owned RGBA8 frame from a complete strided buffer.
    ///
    /// # Errors
    ///
    /// Returns a typed layout error for zero dimensions, insufficient stride,
    /// arithmetic overflow, an oversized buffer, or a length mismatch.
    pub fn new(
        width: u32,
        height: u32,
        stride: usize,
        pixels: Vec<u8>,
    ) -> Result<Self, FrameError> {
        let expected = validate_layout(width, height, stride)?;
        if pixels.len() != expected {
            return Err(FrameError::BufferLengthMismatch {
                expected,
                actual: pixels.len(),
            });
        }
        Ok(Self {
            width,
            height,
            stride,
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
    pub const fn stride(&self) -> usize {
        self.stride
    }

    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<Rgba8> {
        let offset = self.pixel_offset(x, y)?;
        Some(Rgba8::new(
            self.pixels[offset],
            self.pixels[offset + 1],
            self.pixels[offset + 2],
            self.pixels[offset + 3],
        ))
    }

    pub(crate) fn packed(width: u32, height: u32) -> Result<Self, FrameError> {
        let width_usize = usize::try_from(width).map_err(|_| FrameError::LayoutOverflow)?;
        let stride = width_usize
            .checked_mul(CHANNELS)
            .ok_or(FrameError::LayoutOverflow)?;
        let length = validate_layout(width, height, stride)?;
        Self::new(width, height, stride, vec![0; length])
    }

    pub(crate) fn set_pixel(&mut self, x: u32, y: u32, pixel: Rgba8) {
        if let Some(offset) = self.pixel_offset(x, y) {
            self.pixels[offset..offset + CHANNELS].copy_from_slice(&pixel.to_bytes());
        }
    }

    pub(crate) fn pixels_mut(&mut self) -> &mut [u8] {
        &mut self.pixels
    }

    fn pixel_offset(&self, x: u32, y: u32) -> Option<usize> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let x = usize::try_from(x).ok()?;
        let y = usize::try_from(y).ok()?;
        y.checked_mul(self.stride)?.checked_add(x * CHANNELS)
    }
}

pub(crate) fn validate_layout(width: u32, height: u32, stride: usize) -> Result<usize, FrameError> {
    if width == 0 {
        return Err(FrameError::ZeroWidth);
    }
    if height == 0 {
        return Err(FrameError::ZeroHeight);
    }
    let width = usize::try_from(width).map_err(|_| FrameError::LayoutOverflow)?;
    let minimum = width
        .checked_mul(CHANNELS)
        .ok_or(FrameError::LayoutOverflow)?;
    if stride < minimum {
        return Err(FrameError::StrideTooSmall {
            minimum,
            actual: stride,
        });
    }
    let height = usize::try_from(height).map_err(|_| FrameError::LayoutOverflow)?;
    let required = stride
        .checked_mul(height)
        .ok_or(FrameError::LayoutOverflow)?;
    if required > ImageFrame::MAX_BUFFER_BYTES {
        return Err(FrameError::BufferTooLarge {
            required,
            maximum: ImageFrame::MAX_BUFFER_BYTES,
        });
    }
    Ok(required)
}
