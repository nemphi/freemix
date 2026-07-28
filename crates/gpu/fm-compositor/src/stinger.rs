use core::fmt;

use fm_video::{CompositeError, ImageFrame, Layer, Rgba8, compose_layers};

/// The Program/Preview source drawn below a stinger media frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StingerBase {
    Program,
    Preview,
}

/// Validated zero-based playback position for one stinger media frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StingerFramePlan {
    frame_index: u32,
    frame_count: u32,
    cut_point_frame: u32,
}

impl StingerFramePlan {
    /// Compiles an exact media-frame position and cut point.
    ///
    /// The cut point is the first stinger frame drawn over Preview. A cut point
    /// equal to `frame_count` keeps Program below every media frame and switches
    /// to Preview only when playback completes.
    ///
    /// # Errors
    ///
    /// Returns an error for empty media, an out-of-range frame index, or a cut
    /// point beyond the end of the media.
    pub const fn compile(
        frame_index: u32,
        frame_count: u32,
        cut_point_frame: u32,
    ) -> Result<Self, StingerPlanError> {
        if frame_count == 0 {
            return Err(StingerPlanError::EmptyMedia);
        }
        if frame_index >= frame_count {
            return Err(StingerPlanError::FrameOutOfRange {
                frame_index,
                frame_count,
            });
        }
        if cut_point_frame > frame_count {
            return Err(StingerPlanError::CutPointOutOfRange {
                cut_point_frame,
                frame_count,
            });
        }
        Ok(Self {
            frame_index,
            frame_count,
            cut_point_frame,
        })
    }

    #[must_use]
    pub const fn frame_index(self) -> u32 {
        self.frame_index
    }

    #[must_use]
    pub const fn frame_count(self) -> u32 {
        self.frame_count
    }

    #[must_use]
    pub const fn cut_point_frame(self) -> u32 {
        self.cut_point_frame
    }

    #[must_use]
    pub const fn base(self) -> StingerBase {
        if self.frame_index < self.cut_point_frame {
            StingerBase::Program
        } else {
            StingerBase::Preview
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StingerPlanError {
    EmptyMedia,
    FrameOutOfRange {
        frame_index: u32,
        frame_count: u32,
    },
    CutPointOutOfRange {
        cut_point_frame: u32,
        frame_count: u32,
    },
}

impl fmt::Display for StingerPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyMedia => {
                formatter.write_str("stinger media must contain at least one frame")
            }
            Self::FrameOutOfRange {
                frame_index,
                frame_count,
            } => write!(
                formatter,
                "stinger frame {frame_index} is outside media with {frame_count} frames"
            ),
            Self::CutPointOutOfRange {
                cut_point_frame,
                frame_count,
            } => write!(
                formatter,
                "stinger cut point {cut_point_frame} exceeds media length {frame_count}"
            ),
        }
    }
}

impl std::error::Error for StingerPlanError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StingerFrameRole {
    Preview,
    Media,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StingerRenderError {
    DimensionMismatch {
        role: StingerFrameRole,
        expected_width: u32,
        expected_height: u32,
        actual_width: u32,
        actual_height: u32,
    },
    Composite(CompositeError),
}

impl fmt::Display for StingerRenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DimensionMismatch {
                role,
                expected_width,
                expected_height,
                actual_width,
                actual_height,
            } => write!(
                formatter,
                "stinger {role:?} frame is {actual_width}x{actual_height}, expected \
                 {expected_width}x{expected_height}"
            ),
            Self::Composite(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for StingerRenderError {}

impl From<CompositeError> for StingerRenderError {
    fn from(value: CompositeError) -> Self {
        Self::Composite(value)
    }
}

/// Composes one premultiplied RGBA stinger media frame over Program or Preview.
///
/// Program remains the base before the configured cut point. Preview becomes
/// the base at the cut point and remains selected through the final media
/// frame. All three frames must have identical dimensions.
///
/// # Errors
///
/// Returns a typed dimension or premultiplied-alpha composition error.
pub fn execute_stinger_frame(
    plan: StingerFramePlan,
    program: &ImageFrame,
    preview: &ImageFrame,
    media: &ImageFrame,
) -> Result<ImageFrame, StingerRenderError> {
    validate_dimensions(program, preview, StingerFrameRole::Preview)?;
    validate_dimensions(program, media, StingerFrameRole::Media)?;
    let base = match plan.base() {
        StingerBase::Program => program,
        StingerBase::Preview => preview,
    };
    Ok(compose_layers(
        program.width(),
        program.height(),
        Rgba8::new(0, 0, 0, 0),
        &[
            Layer::new(base, 0, 0, 0, u8::MAX),
            Layer::new(media, 0, 0, 1, u8::MAX),
        ],
    )?)
}

fn validate_dimensions(
    program: &ImageFrame,
    frame: &ImageFrame,
    role: StingerFrameRole,
) -> Result<(), StingerRenderError> {
    if frame.width() == program.width() && frame.height() == program.height() {
        return Ok(());
    }
    Err(StingerRenderError::DimensionMismatch {
        role,
        expected_width: program.width(),
        expected_height: program.height(),
        actual_width: frame.width(),
        actual_height: frame.height(),
    })
}
