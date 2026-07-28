use crate::{PipelineConfigError, RegistryError, RenderError, SimulatedSource, SourcePattern};
use fm_switcher::{ProgramFrame, TransitionKind};
use fm_types::InputId;
use fm_video::{BlendError, FrameError, ImageFrame, crossfade, solid_color, vertical_color_bars};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct SimulatedPipeline {
    width: u32,
    height: u32,
    sources: BTreeMap<InputId, SimulatedSource>,
}

impl SimulatedPipeline {
    pub const MAX_WIDTH: u32 = 7_680;
    pub const MAX_HEIGHT: u32 = 4_320;

    /// Creates an empty simulated pipeline with bounded output dimensions.
    ///
    /// # Errors
    ///
    /// Returns a typed error for zero or out-of-range dimensions.
    pub fn new(width: u32, height: u32) -> Result<Self, PipelineConfigError> {
        if width == 0 {
            return Err(PipelineConfigError::ZeroWidth);
        }
        if height == 0 {
            return Err(PipelineConfigError::ZeroHeight);
        }
        if width > Self::MAX_WIDTH || height > Self::MAX_HEIGHT {
            return Err(PipelineConfigError::DimensionsExceedLimit {
                width,
                height,
                maximum_width: Self::MAX_WIDTH,
                maximum_height: Self::MAX_HEIGHT,
            });
        }
        Ok(Self {
            width,
            height,
            sources: BTreeMap::new(),
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

    /// Registers a source without replacing an existing identity.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::DuplicateSource`] if the input is registered.
    pub fn register(&mut self, source: SimulatedSource) -> Result<(), RegistryError> {
        if self.sources.contains_key(&source.input()) {
            return Err(RegistryError::DuplicateSource(source.input()));
        }
        self.sources.insert(source.input(), source);
        Ok(())
    }

    pub fn remove(&mut self, input: InputId) -> Option<SimulatedSource> {
        self.sources.remove(&input)
    }

    #[must_use]
    pub fn source(&self, input: InputId) -> Option<&SimulatedSource> {
        self.sources.get(&input)
    }

    #[must_use]
    pub fn inputs(&self) -> impl ExactSizeIterator<Item = InputId> + '_ {
        self.sources.keys().copied()
    }

    /// Renders the switcher's desired program frame.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::MissingSource`] for either unavailable input, or
    /// a structured video error when generation or blending fails.
    pub fn render(
        &self,
        frame_number: u64,
        program: ProgramFrame,
    ) -> Result<ImageFrame, RenderError> {
        let primary = self.render_input(program.primary, frame_number)?;
        let Some(secondary) = program
            .secondary
            .filter(|secondary| *secondary != program.primary)
        else {
            return Ok(primary);
        };
        let secondary = self.render_input(secondary, frame_number)?;
        match program.transition_kind {
            Some(TransitionKind::Fade | TransitionKind::AlphaFade) => Ok(crossfade(
                &primary,
                &secondary,
                program.mix_numerator,
                program.mix_denominator,
            )?),
            Some(TransitionKind::Wipe) => Ok(horizontal_wipe(
                &primary,
                &secondary,
                program.mix_numerator,
                program.mix_denominator,
            )?),
            Some(TransitionKind::Slide) => Ok(horizontal_slide(
                &primary,
                &secondary,
                program.mix_numerator,
                program.mix_denominator,
            )?),
            Some(TransitionKind::Zoom) => Ok(centered_zoom(
                &primary,
                &secondary,
                program.mix_numerator,
                program.mix_denominator,
            )?),
            Some(kind) => Err(RenderError::UnsupportedTransition(kind)),
            None => Err(RenderError::MissingTransitionKind),
        }
    }

    fn render_input(&self, input: InputId, frame_number: u64) -> Result<ImageFrame, RenderError> {
        let source = self
            .sources
            .get(&input)
            .ok_or(RenderError::MissingSource { input })?;
        Ok(match source.pattern() {
            SourcePattern::Bars => vertical_color_bars(self.width, self.height, frame_number)?,
            SourcePattern::Solid(color) => solid_color(self.width, self.height, color)?,
        })
    }
}

fn horizontal_wipe(
    from: &ImageFrame,
    to: &ImageFrame,
    numerator: u32,
    denominator: u32,
) -> Result<ImageFrame, BlendError> {
    if denominator == 0 {
        return Err(BlendError::ZeroDenominator);
    }
    if numerator > denominator {
        return Err(BlendError::NumeratorExceedsDenominator {
            numerator,
            denominator,
        });
    }
    if numerator == 0 {
        return Ok(from.clone());
    }
    if numerator == denominator {
        return Ok(to.clone());
    }

    let boundary = u64::from(from.width()) * u64::from(numerator) / u64::from(denominator);
    let boundary_bytes = usize::try_from(boundary)
        .map_err(|_| BlendError::Frame(FrameError::LayoutOverflow))?
        .checked_mul(4)
        .ok_or(BlendError::Frame(FrameError::LayoutOverflow))?;
    let mut pixels = from.pixels().to_vec();
    for (output_row, to_row) in pixels
        .chunks_exact_mut(from.stride())
        .zip(to.pixels().chunks_exact(to.stride()))
    {
        output_row[..boundary_bytes].copy_from_slice(&to_row[..boundary_bytes]);
    }
    ImageFrame::new(from.width(), from.height(), from.stride(), pixels).map_err(BlendError::Frame)
}

fn horizontal_slide(
    from: &ImageFrame,
    to: &ImageFrame,
    numerator: u32,
    denominator: u32,
) -> Result<ImageFrame, BlendError> {
    if denominator == 0 {
        return Err(BlendError::ZeroDenominator);
    }
    if numerator > denominator {
        return Err(BlendError::NumeratorExceedsDenominator {
            numerator,
            denominator,
        });
    }
    if numerator == 0 {
        return Ok(from.clone());
    }
    if numerator == denominator {
        return Ok(to.clone());
    }

    let offset = u64::from(from.width()) * u64::from(numerator) / u64::from(denominator);
    let offset_bytes = usize::try_from(offset)
        .map_err(|_| BlendError::Frame(FrameError::LayoutOverflow))?
        .checked_mul(4)
        .ok_or(BlendError::Frame(FrameError::LayoutOverflow))?;
    let row_bytes = usize::try_from(from.width())
        .map_err(|_| BlendError::Frame(FrameError::LayoutOverflow))?
        .checked_mul(4)
        .ok_or(BlendError::Frame(FrameError::LayoutOverflow))?;
    let remaining_bytes = row_bytes - offset_bytes;
    let mut pixels = from.pixels().to_vec();
    for ((output_row, from_row), to_row) in pixels
        .chunks_exact_mut(from.stride())
        .zip(from.pixels().chunks_exact(from.stride()))
        .zip(to.pixels().chunks_exact(to.stride()))
    {
        output_row[..remaining_bytes].copy_from_slice(&from_row[offset_bytes..row_bytes]);
        output_row[remaining_bytes..row_bytes].copy_from_slice(&to_row[..offset_bytes]);
    }
    ImageFrame::new(from.width(), from.height(), from.stride(), pixels).map_err(BlendError::Frame)
}

fn centered_zoom(
    from: &ImageFrame,
    to: &ImageFrame,
    numerator: u32,
    denominator: u32,
) -> Result<ImageFrame, BlendError> {
    if denominator == 0 {
        return Err(BlendError::ZeroDenominator);
    }
    if numerator > denominator {
        return Err(BlendError::NumeratorExceedsDenominator {
            numerator,
            denominator,
        });
    }
    if numerator == 0 {
        return Ok(from.clone());
    }
    if numerator == denominator {
        return Ok(to.clone());
    }

    let width = scaled_extent(from.width(), numerator, denominator);
    let height = scaled_extent(from.height(), numerator, denominator);
    if width == 0 || height == 0 {
        return Ok(from.clone());
    }
    let left = (from.width() - width) / 2;
    let top = (from.height() - height) / 2;
    let mut pixels = from.pixels().to_vec();
    for output_y in 0..height {
        let source_y =
            usize::try_from(u64::from(output_y) * u64::from(to.height()) / u64::from(height))
                .map_err(|_| BlendError::Frame(FrameError::LayoutOverflow))?;
        let output_y = usize::try_from(top + output_y)
            .map_err(|_| BlendError::Frame(FrameError::LayoutOverflow))?;
        let output_row = output_y
            .checked_mul(from.stride())
            .ok_or(BlendError::Frame(FrameError::LayoutOverflow))?;
        let source_row = source_y
            .checked_mul(to.stride())
            .ok_or(BlendError::Frame(FrameError::LayoutOverflow))?;
        for output_x in 0..width {
            let source_x =
                usize::try_from(u64::from(output_x) * u64::from(to.width()) / u64::from(width))
                    .map_err(|_| BlendError::Frame(FrameError::LayoutOverflow))?
                    .checked_mul(4)
                    .ok_or(BlendError::Frame(FrameError::LayoutOverflow))?;
            let output_x = usize::try_from(left + output_x)
                .map_err(|_| BlendError::Frame(FrameError::LayoutOverflow))?
                .checked_mul(4)
                .ok_or(BlendError::Frame(FrameError::LayoutOverflow))?;
            let output = output_row
                .checked_add(output_x)
                .ok_or(BlendError::Frame(FrameError::LayoutOverflow))?;
            let source = source_row
                .checked_add(source_x)
                .ok_or(BlendError::Frame(FrameError::LayoutOverflow))?;
            pixels[output..output + 4].copy_from_slice(&to.pixels()[source..source + 4]);
        }
    }
    ImageFrame::new(from.width(), from.height(), from.stride(), pixels).map_err(BlendError::Frame)
}

fn scaled_extent(size: u32, numerator: u32, denominator: u32) -> u32 {
    let extent = u64::from(size) * u64::from(numerator) / u64::from(denominator);
    u32::try_from(extent).expect("scaled extent cannot exceed its source dimension")
}
