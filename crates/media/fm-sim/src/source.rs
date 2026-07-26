use fm_types::InputId;
use fm_video::Rgba8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourcePattern {
    Bars,
    Solid(Rgba8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SimulatedSource {
    input: InputId,
    pattern: SourcePattern,
}

impl SimulatedSource {
    #[must_use]
    pub const fn new(input: InputId, pattern: SourcePattern) -> Self {
        Self { input, pattern }
    }

    #[must_use]
    pub const fn input(&self) -> InputId {
        self.input
    }

    #[must_use]
    pub const fn pattern(&self) -> SourcePattern {
        self.pattern
    }
}
