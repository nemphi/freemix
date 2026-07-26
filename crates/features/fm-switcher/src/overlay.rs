use fm_types::{InputId, OutputId};

/// Number of independently controlled downstream-key overlay channels.
pub const OVERLAY_CHANNEL_COUNT: usize = 8;

/// An operator-facing overlay channel number in the range one through eight.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OverlayChannelId(u8);

impl OverlayChannelId {
    /// Creates a channel from its one-based operator-facing number.
    #[must_use]
    pub const fn new(number: u8) -> Option<Self> {
        if number >= 1 && number <= 8 {
            Some(Self(number))
        } else {
            None
        }
    }

    /// Creates a channel from a zero-based array index.
    #[must_use]
    pub fn from_index(index: usize) -> Option<Self> {
        if index < OVERLAY_CHANNEL_COUNT {
            Some(Self(u8::try_from(index).ok()? + 1))
        } else {
            None
        }
    }

    #[must_use]
    pub const fn number(self) -> u8 {
        self.0
    }

    #[must_use]
    pub fn index(self) -> usize {
        usize::from(self.0 - 1)
    }
}

/// Desired state for one overlay channel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverlayChannelState {
    source: Option<InputId>,
    active: bool,
    included_outputs: Vec<OutputId>,
}

impl OverlayChannelState {
    pub(crate) const fn empty() -> Self {
        Self {
            source: None,
            active: false,
            included_outputs: Vec::new(),
        }
    }

    #[must_use]
    pub const fn source(&self) -> Option<InputId> {
        self.source
    }

    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.active
    }

    #[must_use]
    pub fn included_outputs(&self) -> &[OutputId] {
        &self.included_outputs
    }

    #[must_use]
    pub fn is_included_in(&self, output: OutputId) -> bool {
        self.included_outputs.contains(&output)
    }

    pub(crate) fn take(&mut self, source: InputId) {
        self.source = Some(source);
        self.active = true;
    }

    pub(crate) fn update(&mut self, source: InputId) {
        self.source = Some(source);
    }

    pub(crate) const fn off(&mut self) {
        self.active = false;
    }

    pub(crate) fn set_output_inclusion(&mut self, output: OutputId, included: bool) {
        if included {
            if !self.included_outputs.contains(&output) {
                self.included_outputs.push(output);
            }
        } else {
            self.included_outputs
                .retain(|candidate| *candidate != output);
        }
    }
}
