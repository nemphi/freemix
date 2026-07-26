use crate::{PipelineConfigError, RegistryError, RenderError, SimulatedSource, SourcePattern};
use fm_switcher::ProgramFrame;
use fm_types::InputId;
use fm_video::{ImageFrame, crossfade, solid_color, vertical_color_bars};
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
        let Some(secondary) = program.secondary else {
            return Ok(primary);
        };
        let secondary = self.render_input(secondary, frame_number)?;
        Ok(crossfade(
            &primary,
            &secondary,
            program.mix_numerator,
            program.mix_denominator,
        )?)
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
