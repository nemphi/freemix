use fm_types::{InputId, OutputId};

/// Number of independently controlled downstream-key overlay channels.
pub const OVERLAY_CHANNEL_COUNT: usize = 8;
pub const MAX_OVERLAY_TRANSITION_DURATION_FRAMES: u32 = 3_600;
pub const MAX_OVERLAY_QUEUE_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OverlayTransitionKind {
    #[default]
    Cut,
    Fade,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OverlayPositionPreset {
    #[default]
    FullFrame,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OverlayBorderPreset {
    #[default]
    None,
    ThinWhite,
    ThickWhite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AutomaticOverlayTransition {
    start_opacity: u8,
    target_opacity: u8,
    target_active: bool,
    duration_frames: u32,
    elapsed_frames: u32,
}

impl AutomaticOverlayTransition {
    fn opacity_at(self, elapsed_frames: u32) -> u8 {
        let distance = self.start_opacity.abs_diff(self.target_opacity);
        let offset = u32::from(distance) * elapsed_frames / self.duration_frames;
        let offset = u8::try_from(offset).expect("overlay opacity stays within its endpoints");
        if self.target_opacity >= self.start_opacity {
            self.start_opacity + offset
        } else {
            self.start_opacity - offset
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OverlayTransitionAdvance {
    pub opacity_changed: Option<u8>,
    pub completed: Option<bool>,
}

/// An operator-facing overlay channel number in the range one through eight.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OverlayChannelId(u8);

impl OverlayChannelId {
    pub const ALL: [Self; OVERLAY_CHANNEL_COUNT] = [
        Self(1),
        Self(2),
        Self(3),
        Self(4),
        Self(5),
        Self(6),
        Self(7),
        Self(8),
    ];

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
    opacity: u8,
    transition: OverlayTransitionKind,
    duration_frames: u32,
    position: OverlayPositionPreset,
    border: OverlayBorderPreset,
    queue: Vec<InputId>,
    automatic: Option<AutomaticOverlayTransition>,
    included_outputs: Vec<OutputId>,
}

impl OverlayChannelState {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            source: None,
            active: false,
            opacity: 0,
            transition: OverlayTransitionKind::Cut,
            duration_frames: 1,
            position: OverlayPositionPreset::FullFrame,
            border: OverlayBorderPreset::None,
            queue: Vec::new(),
            automatic: None,
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
    pub const fn opacity(&self) -> u8 {
        self.opacity
    }

    #[must_use]
    pub const fn transition(&self) -> OverlayTransitionKind {
        self.transition
    }

    #[must_use]
    pub const fn duration_frames(&self) -> u32 {
        self.duration_frames
    }

    #[must_use]
    pub const fn position(&self) -> OverlayPositionPreset {
        self.position
    }

    #[must_use]
    pub const fn border(&self) -> OverlayBorderPreset {
        self.border
    }

    #[must_use]
    pub fn queued_sources(&self) -> &[InputId] {
        &self.queue
    }

    #[must_use]
    pub const fn is_transitioning(&self) -> bool {
        self.automatic.is_some()
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
        self.opacity = u8::MAX;
        self.automatic = None;
    }

    pub(crate) fn update(&mut self, source: InputId) {
        self.source = Some(source);
    }

    pub(crate) const fn off(&mut self) {
        self.active = false;
        self.opacity = 0;
        self.automatic = None;
    }

    pub(crate) fn configure_transition(
        &mut self,
        transition: OverlayTransitionKind,
        duration_frames: u32,
    ) {
        self.transition = transition;
        self.duration_frames = duration_frames;
    }

    pub(crate) fn configure_appearance(
        &mut self,
        position: OverlayPositionPreset,
        border: OverlayBorderPreset,
    ) {
        self.position = position;
        self.border = border;
    }

    pub(crate) fn enqueue(&mut self, source: InputId) -> bool {
        if self.queue.len() >= MAX_OVERLAY_QUEUE_DEPTH {
            return false;
        }
        self.queue.push(source);
        true
    }

    pub(crate) fn take_next(&mut self) -> Option<InputId> {
        let source = self.queue.first().copied()?;
        self.queue.remove(0);
        self.take(source);
        Some(source)
    }

    pub(crate) fn request_take_next(&mut self) -> Option<InputId> {
        let source = self.queue.first().copied()?;
        self.queue.remove(0);
        self.request_take(source);
        Some(source)
    }

    pub(crate) fn request_take(&mut self, source: InputId) {
        self.source = Some(source);
        self.active = true;
        match self.transition {
            OverlayTransitionKind::Cut => {
                self.opacity = u8::MAX;
                self.automatic = None;
            }
            OverlayTransitionKind::Fade => {
                self.opacity = 0;
                self.automatic = Some(AutomaticOverlayTransition {
                    start_opacity: 0,
                    target_opacity: u8::MAX,
                    target_active: true,
                    duration_frames: self.duration_frames,
                    elapsed_frames: 0,
                });
            }
        }
    }

    pub(crate) fn request_off(&mut self) {
        match self.transition {
            OverlayTransitionKind::Fade if self.active => {
                self.automatic = Some(AutomaticOverlayTransition {
                    start_opacity: self.opacity,
                    target_opacity: 0,
                    target_active: false,
                    duration_frames: self.duration_frames,
                    elapsed_frames: 0,
                });
            }
            OverlayTransitionKind::Cut | OverlayTransitionKind::Fade => self.off(),
        }
    }

    pub(crate) fn advance(&mut self) -> OverlayTransitionAdvance {
        let Some(mut automatic) = self.automatic else {
            return OverlayTransitionAdvance {
                opacity_changed: None,
                completed: None,
            };
        };
        let previous = self.opacity;
        automatic.elapsed_frames += 1;
        self.opacity = automatic.opacity_at(automatic.elapsed_frames);
        let completed = automatic.elapsed_frames >= automatic.duration_frames;
        if completed {
            self.opacity = automatic.target_opacity;
            self.active = automatic.target_active;
            self.automatic = None;
        } else {
            self.automatic = Some(automatic);
        }
        OverlayTransitionAdvance {
            opacity_changed: (self.opacity != previous).then_some(self.opacity),
            completed: completed.then_some(automatic.target_active),
        }
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
