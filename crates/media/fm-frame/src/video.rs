use core::fmt;

use fm_types::{PixelFormat, VideoDimensions, VideoFrameMetadata, VideoFrameMetadataError};

use crate::MediaTiming;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CpuVideoPlane {
    stride: usize,
    bytes: Vec<u8>,
}

impl CpuVideoPlane {
    /// Creates a bounded, otherwise format-independent CPU plane.
    ///
    /// # Errors
    ///
    /// Returns a typed error for a zero or excessive stride or plane size.
    pub fn new(stride: usize, bytes: Vec<u8>) -> Result<Self, VideoPayloadError> {
        if stride == 0 {
            return Err(VideoPayloadError::ZeroStride);
        }
        if stride > CpuVideoPayload::MAX_STRIDE_BYTES {
            return Err(VideoPayloadError::StrideTooLarge {
                stride,
                maximum: CpuVideoPayload::MAX_STRIDE_BYTES,
            });
        }
        if bytes.len() > CpuVideoPayload::MAX_TOTAL_BYTES {
            return Err(VideoPayloadError::PayloadTooLarge {
                required: bytes.len(),
                maximum: CpuVideoPayload::MAX_TOTAL_BYTES,
            });
        }
        Ok(Self { stride, bytes })
    }

    #[must_use]
    pub const fn stride(&self) -> usize {
        self.stride
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CpuVideoPayload {
    format: PixelFormat,
    dimensions: VideoDimensions,
    planes: Vec<CpuVideoPlane>,
    byte_len: usize,
}

impl CpuVideoPayload {
    pub const MAX_WIDTH: u32 = 16_384;
    pub const MAX_HEIGHT: u32 = 16_384;
    pub const MAX_PLANES: usize = 2;
    pub const MAX_STRIDE_BYTES: usize = 1024 * 1024;
    pub const MAX_TOTAL_BYTES: usize = 512 * 1024 * 1024;

    /// Validates format-specific plane count, stride, dimensions, and size.
    ///
    /// # Errors
    ///
    /// Returns a typed layout error before retaining the payload.
    pub fn new(
        format: PixelFormat,
        dimensions: VideoDimensions,
        planes: Vec<CpuVideoPlane>,
    ) -> Result<Self, VideoPayloadError> {
        let layouts = plane_layouts(format, dimensions)?;
        if planes.len() != layouts.len() {
            return Err(VideoPayloadError::PlaneCountMismatch {
                expected: layouts.len(),
                actual: planes.len(),
            });
        }

        let mut byte_len = 0usize;
        for (index, (plane, layout)) in planes.iter().zip(&layouts).enumerate() {
            validate_plane(index, plane, *layout)?;
            byte_len = byte_len
                .checked_add(plane.bytes.len())
                .ok_or(VideoPayloadError::LayoutOverflow)?;
        }
        if byte_len > Self::MAX_TOTAL_BYTES {
            return Err(VideoPayloadError::PayloadTooLarge {
                required: byte_len,
                maximum: Self::MAX_TOTAL_BYTES,
            });
        }

        Ok(Self {
            format,
            dimensions,
            planes,
            byte_len,
        })
    }

    /// Allocates zeroed planes with tightly packed strides.
    ///
    /// # Errors
    ///
    /// Returns a layout or allocation-limit error before allocating memory.
    pub fn allocate(
        format: PixelFormat,
        dimensions: VideoDimensions,
    ) -> Result<Self, VideoPayloadError> {
        let layouts = plane_layouts(format, dimensions)?;
        let total = layouts.iter().try_fold(0usize, |total, layout| {
            let size = layout
                .row_bytes
                .checked_mul(layout.rows)
                .ok_or(VideoPayloadError::LayoutOverflow)?;
            total
                .checked_add(size)
                .ok_or(VideoPayloadError::LayoutOverflow)
        })?;
        if total > Self::MAX_TOTAL_BYTES {
            return Err(VideoPayloadError::PayloadTooLarge {
                required: total,
                maximum: Self::MAX_TOTAL_BYTES,
            });
        }
        let planes = layouts
            .iter()
            .map(|layout| CpuVideoPlane {
                stride: layout.row_bytes,
                bytes: vec![0; layout.row_bytes * layout.rows],
            })
            .collect();
        Self::new(format, dimensions, planes)
    }

    #[must_use]
    pub const fn format(&self) -> PixelFormat {
        self.format
    }

    #[must_use]
    pub const fn dimensions(&self) -> VideoDimensions {
        self.dimensions
    }

    #[must_use]
    pub fn planes(&self) -> &[CpuVideoPlane] {
        &self.planes
    }

    #[must_use]
    pub fn plane(&self, index: usize) -> Option<&CpuVideoPlane> {
        self.planes.get(index)
    }

    #[must_use]
    pub const fn byte_len(&self) -> usize {
        self.byte_len
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CpuVideoFrame {
    timing: MediaTiming,
    payload: CpuVideoPayload,
    metadata: Option<VideoFrameMetadata>,
}

impl CpuVideoFrame {
    #[must_use]
    pub const fn new(timing: MediaTiming, payload: CpuVideoPayload) -> Self {
        Self {
            timing,
            payload,
            metadata: None,
        }
    }

    /// Attaches metadata after validating it against the payload format.
    ///
    /// # Errors
    ///
    /// Returns a typed matrix or alpha interpretation error for the format.
    pub fn with_metadata(
        mut self,
        metadata: VideoFrameMetadata,
    ) -> Result<Self, VideoFrameMetadataError> {
        metadata.validate_for(self.payload.format())?;
        self.metadata = Some(metadata);
        Ok(self)
    }

    #[must_use]
    pub const fn timing(&self) -> MediaTiming {
        self.timing
    }

    #[must_use]
    pub const fn payload(&self) -> &CpuVideoPayload {
        &self.payload
    }

    #[must_use]
    pub const fn metadata(&self) -> Option<VideoFrameMetadata> {
        self.metadata
    }

    #[must_use]
    pub fn into_payload(self) -> CpuVideoPayload {
        self.payload
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoPayloadError {
    DimensionsTooLarge {
        width: u32,
        height: u32,
        maximum_width: u32,
        maximum_height: u32,
    },
    SubsampledDimensionsMustBeEven,
    PlaneCountMismatch {
        expected: usize,
        actual: usize,
    },
    ZeroStride,
    StrideTooSmall {
        plane: usize,
        minimum: usize,
        actual: usize,
    },
    StrideTooLarge {
        stride: usize,
        maximum: usize,
    },
    PlaneLengthMismatch {
        plane: usize,
        expected: usize,
        actual: usize,
    },
    PayloadTooLarge {
        required: usize,
        maximum: usize,
    },
    LayoutOverflow,
}

impl fmt::Display for VideoPayloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DimensionsTooLarge {
                width,
                height,
                maximum_width,
                maximum_height,
            } => write!(
                formatter,
                "video dimensions {width}x{height} exceed {maximum_width}x{maximum_height}"
            ),
            Self::SubsampledDimensionsMustBeEven => {
                formatter.write_str("subsampled video dimensions must be even")
            }
            Self::PlaneCountMismatch { expected, actual } => {
                write!(formatter, "plane count {actual} does not match {expected}")
            }
            Self::ZeroStride => formatter.write_str("video stride must be nonzero"),
            Self::StrideTooSmall {
                plane,
                minimum,
                actual,
            } => write!(
                formatter,
                "plane {plane} stride {actual} is smaller than {minimum}"
            ),
            Self::StrideTooLarge { stride, maximum } => {
                write!(formatter, "video stride {stride} exceeds {maximum}")
            }
            Self::PlaneLengthMismatch {
                plane,
                expected,
                actual,
            } => write!(
                formatter,
                "plane {plane} length {actual} does not match {expected}"
            ),
            Self::PayloadTooLarge { required, maximum } => {
                write!(
                    formatter,
                    "video payload {required} bytes exceeds {maximum}"
                )
            }
            Self::LayoutOverflow => formatter.write_str("video layout arithmetic overflow"),
        }
    }
}

impl std::error::Error for VideoPayloadError {}

#[derive(Clone, Copy)]
struct PlaneLayout {
    row_bytes: usize,
    rows: usize,
}

fn plane_layouts(
    format: PixelFormat,
    dimensions: VideoDimensions,
) -> Result<Vec<PlaneLayout>, VideoPayloadError> {
    let width = dimensions.width();
    let height = dimensions.height();
    if width > CpuVideoPayload::MAX_WIDTH || height > CpuVideoPayload::MAX_HEIGHT {
        return Err(VideoPayloadError::DimensionsTooLarge {
            width,
            height,
            maximum_width: CpuVideoPayload::MAX_WIDTH,
            maximum_height: CpuVideoPayload::MAX_HEIGHT,
        });
    }
    let width = usize::try_from(width).map_err(|_| VideoPayloadError::LayoutOverflow)?;
    let height = usize::try_from(height).map_err(|_| VideoPayloadError::LayoutOverflow)?;
    let packed = |bytes_per_pixel: usize| {
        width
            .checked_mul(bytes_per_pixel)
            .map(|row_bytes| {
                vec![PlaneLayout {
                    row_bytes,
                    rows: height,
                }]
            })
            .ok_or(VideoPayloadError::LayoutOverflow)
    };

    match format {
        PixelFormat::Rgba8 | PixelFormat::Bgra8 => packed(4),
        PixelFormat::Rgba16Float => packed(8),
        PixelFormat::Yuv422 => {
            if width % 2 != 0 {
                return Err(VideoPayloadError::SubsampledDimensionsMustBeEven);
            }
            packed(2)
        }
        PixelFormat::Nv12 | PixelFormat::P010 => {
            if width % 2 != 0 || height % 2 != 0 {
                return Err(VideoPayloadError::SubsampledDimensionsMustBeEven);
            }
            let bytes_per_component = usize::from(format == PixelFormat::P010) + 1;
            let row_bytes = width
                .checked_mul(bytes_per_component)
                .ok_or(VideoPayloadError::LayoutOverflow)?;
            Ok(vec![
                PlaneLayout {
                    row_bytes,
                    rows: height,
                },
                PlaneLayout {
                    row_bytes,
                    rows: height / 2,
                },
            ])
        }
    }
}

fn validate_plane(
    index: usize,
    plane: &CpuVideoPlane,
    layout: PlaneLayout,
) -> Result<(), VideoPayloadError> {
    if plane.stride < layout.row_bytes {
        return Err(VideoPayloadError::StrideTooSmall {
            plane: index,
            minimum: layout.row_bytes,
            actual: plane.stride,
        });
    }
    let expected = plane
        .stride
        .checked_mul(layout.rows)
        .ok_or(VideoPayloadError::LayoutOverflow)?;
    if plane.bytes.len() != expected {
        return Err(VideoPayloadError::PlaneLengthMismatch {
            plane: index,
            expected,
            actual: plane.bytes.len(),
        });
    }
    Ok(())
}
