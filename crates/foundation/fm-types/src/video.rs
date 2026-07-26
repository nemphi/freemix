use core::fmt;

use crate::{AlphaMode, ColorMetadata, FrameRate, MatrixCoefficients};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct VideoDimensions {
    width: u32,
    height: u32,
}

impl VideoDimensions {
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Option<Self> {
        if width == 0 || height == 0 {
            None
        } else {
            Some(Self { width, height })
        }
    }

    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PixelFormat {
    Rgba8,
    Bgra8,
    Rgba16Float,
    Nv12,
    P010,
    Yuv422,
}

/// Enumerated color and alpha interpretation for a decoded video frame.
///
/// Metadata is validated against the frame's pixel format when attached. RGB
/// formats ignore chroma location but require an identity matrix and an alpha
/// mode; YUV formats require a non-identity matrix and prohibit an alpha mode.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct VideoFrameMetadata {
    color: ColorMetadata,
    alpha_mode: Option<AlphaMode>,
}

impl VideoFrameMetadata {
    #[must_use]
    pub const fn new(color: ColorMetadata, alpha_mode: Option<AlphaMode>) -> Self {
        Self { color, alpha_mode }
    }

    #[must_use]
    pub const fn color(self) -> ColorMetadata {
        self.color
    }

    #[must_use]
    pub const fn alpha_mode(self) -> Option<AlphaMode> {
        self.alpha_mode
    }

    /// Validates this metadata for a concrete pixel format.
    ///
    /// # Errors
    ///
    /// Returns an error when RGB/YUV matrix or alpha semantics do not match the
    /// pixel format.
    pub const fn validate_for(
        self,
        pixel_format: PixelFormat,
    ) -> Result<(), VideoFrameMetadataError> {
        match pixel_format {
            PixelFormat::Rgba8 | PixelFormat::Bgra8 | PixelFormat::Rgba16Float => {
                if !matches!(self.color.matrix, MatrixCoefficients::Identity) {
                    return Err(VideoFrameMetadataError::RgbMatrixMustBeIdentity {
                        pixel_format,
                        matrix: self.color.matrix,
                    });
                }
                if self.alpha_mode.is_none() {
                    return Err(VideoFrameMetadataError::RgbAlphaModeRequired { pixel_format });
                }
            }
            PixelFormat::Nv12 | PixelFormat::P010 | PixelFormat::Yuv422 => {
                if matches!(self.color.matrix, MatrixCoefficients::Identity) {
                    return Err(VideoFrameMetadataError::YuvMatrixMustNotBeIdentity {
                        pixel_format,
                    });
                }
                if let Some(alpha_mode) = self.alpha_mode {
                    return Err(VideoFrameMetadataError::YuvAlphaModeNotAllowed {
                        pixel_format,
                        alpha_mode,
                    });
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoFrameMetadataError {
    RgbMatrixMustBeIdentity {
        pixel_format: PixelFormat,
        matrix: MatrixCoefficients,
    },
    RgbAlphaModeRequired {
        pixel_format: PixelFormat,
    },
    YuvMatrixMustNotBeIdentity {
        pixel_format: PixelFormat,
    },
    YuvAlphaModeNotAllowed {
        pixel_format: PixelFormat,
        alpha_mode: AlphaMode,
    },
}

impl fmt::Display for VideoFrameMetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RgbMatrixMustBeIdentity {
                pixel_format,
                matrix,
            } => write!(
                formatter,
                "RGB format {pixel_format:?} requires an identity matrix, got {matrix:?}"
            ),
            Self::RgbAlphaModeRequired { pixel_format } => {
                write!(
                    formatter,
                    "RGB format {pixel_format:?} requires an alpha mode"
                )
            }
            Self::YuvMatrixMustNotBeIdentity { pixel_format } => write!(
                formatter,
                "YUV format {pixel_format:?} requires a non-identity matrix"
            ),
            Self::YuvAlphaModeNotAllowed {
                pixel_format,
                alpha_mode,
            } => write!(
                formatter,
                "YUV format {pixel_format:?} does not allow alpha mode {alpha_mode:?}"
            ),
        }
    }
}

impl std::error::Error for VideoFrameMetadataError {}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ScanMode {
    Progressive,
    InterlacedTopFieldFirst,
    InterlacedBottomFieldFirst,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoFormat {
    pub dimensions: VideoDimensions,
    pub frame_rate: FrameRate,
    pub pixel_format: PixelFormat,
    pub scan: ScanMode,
    pub color: ColorMetadata,
}
