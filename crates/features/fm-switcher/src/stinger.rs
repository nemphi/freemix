use fm_types::InputId;

/// Number of independently configured stinger slots.
pub const STINGER_SLOT_COUNT: usize = 8;

/// An operator-facing stinger slot number in the range one through eight.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StingerSlotId(u8);

impl StingerSlotId {
    /// Creates a slot from its one-based operator-facing number.
    #[must_use]
    pub const fn new(number: u8) -> Option<Self> {
        if number >= 1 && number <= 8 {
            Some(Self(number))
        } else {
            None
        }
    }

    /// Creates a slot from a zero-based array index.
    #[must_use]
    pub fn from_index(index: usize) -> Option<Self> {
        if index < STINGER_SLOT_COUNT {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StingerAudioPolicy {
    Muted,
    StingerOnly,
    MixWithProgram,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MissingMediaFallback {
    Cut,
    Fade,
    KeepProgram,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StingerPreloadState {
    NotRequested,
    Ready,
    Missing,
}

/// Configuration retained by a stinger slot independently of media readiness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StingerDescriptor {
    pub media_input: InputId,
    pub preload: bool,
    pub cut_point_frames: u32,
    pub audio_policy: StingerAudioPolicy,
    pub missing_media_fallback: MissingMediaFallback,
}

impl StingerDescriptor {
    #[must_use]
    pub const fn new(
        media_input: InputId,
        preload: bool,
        cut_point_frames: u32,
        audio_policy: StingerAudioPolicy,
        missing_media_fallback: MissingMediaFallback,
    ) -> Self {
        Self {
            media_input,
            preload,
            cut_point_frames,
            audio_policy,
            missing_media_fallback,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StingerSlotState {
    descriptor: Option<StingerDescriptor>,
    preload_state: StingerPreloadState,
}

impl StingerSlotState {
    pub(crate) const fn empty() -> Self {
        Self {
            descriptor: None,
            preload_state: StingerPreloadState::NotRequested,
        }
    }

    #[must_use]
    pub const fn descriptor(&self) -> Option<&StingerDescriptor> {
        self.descriptor.as_ref()
    }

    #[must_use]
    pub const fn preload_state(&self) -> StingerPreloadState {
        self.preload_state
    }

    pub(crate) fn configure(&mut self, descriptor: StingerDescriptor) {
        self.descriptor = Some(descriptor);
        self.preload_state = StingerPreloadState::NotRequested;
    }

    pub(crate) const fn set_preload_state(&mut self, preload_state: StingerPreloadState) {
        self.preload_state = preload_state;
    }

    pub(crate) const fn clear(&mut self) {
        self.descriptor = None;
        self.preload_state = StingerPreloadState::NotRequested;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StingerPlaybackDecision {
    Play,
    Fallback(MissingMediaFallback),
    Unconfigured,
}
