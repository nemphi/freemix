use fm_types::{InputId, OutputId};

use crate::{
    FadeToBlackPosition, FadeToBlackTarget, MissingMediaFallback, OverlayBorderPreset,
    OverlayChannelId, OverlayPositionPreset, OverlayTransitionKind, StingerPreloadState,
    StingerSlotId, TBarPosition, TransitionKind,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SwitcherCommand {
    SelectPreview(InputId),
    Cut,
    Transition {
        kind: TransitionKind,
        duration_frames: u32,
    },
    Wipe {
        duration_frames: u32,
    },
    StartTBar {
        kind: TransitionKind,
    },
    SetTBarPosition(TBarPosition),
    CommitTBar,
    CancelTBar,
    TakeOverlay {
        channel: OverlayChannelId,
        source: InputId,
    },
    UpdateOverlay {
        channel: OverlayChannelId,
        source: InputId,
    },
    OverlayOff(OverlayChannelId),
    ConfigureOverlayTransition {
        channel: OverlayChannelId,
        transition: OverlayTransitionKind,
        duration_frames: u32,
    },
    ConfigureOverlayAppearance {
        channel: OverlayChannelId,
        position: OverlayPositionPreset,
        border: OverlayBorderPreset,
    },
    QueueOverlay {
        channel: OverlayChannelId,
        source: InputId,
    },
    TakeNextOverlay(OverlayChannelId),
    SetOverlayOutputInclusion {
        channel: OverlayChannelId,
        output: OutputId,
        included: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SwitcherEvent {
    PreviewSelected {
        input: InputId,
    },
    ProgramChanged {
        previous: InputId,
        program: InputId,
    },
    TransitionStarted {
        kind: TransitionKind,
        from: InputId,
        to: InputId,
        duration_frames: u32,
    },
    TransitionCompleted {
        kind: TransitionKind,
        program: InputId,
    },
    TBarStarted {
        kind: TransitionKind,
        from: InputId,
        to: InputId,
    },
    TBarPositionChanged {
        position: TBarPosition,
    },
    TBarCancelled,
    FadeToBlackStarted {
        from: FadeToBlackPosition,
        target: FadeToBlackTarget,
        duration_frames: u32,
    },
    FadeToBlackPositionChanged {
        position: FadeToBlackPosition,
    },
    FadeToBlackCompleted {
        active: bool,
    },
    OverlayTaken {
        channel: OverlayChannelId,
        source: InputId,
    },
    OverlayUpdated {
        channel: OverlayChannelId,
        source: InputId,
    },
    OverlayTurnedOff {
        channel: OverlayChannelId,
    },
    OverlayTransitionConfigured {
        channel: OverlayChannelId,
        transition: OverlayTransitionKind,
        duration_frames: u32,
    },
    OverlayAppearanceConfigured {
        channel: OverlayChannelId,
        position: OverlayPositionPreset,
        border: OverlayBorderPreset,
    },
    OverlayQueued {
        channel: OverlayChannelId,
        source: InputId,
        depth: usize,
    },
    OverlayQueueAdvanced {
        channel: OverlayChannelId,
        source: InputId,
        remaining: usize,
    },
    OverlayOpacityChanged {
        channel: OverlayChannelId,
        opacity: u8,
    },
    OverlayTransitionCompleted {
        channel: OverlayChannelId,
        active: bool,
    },
    OverlayOutputInclusionChanged {
        channel: OverlayChannelId,
        output: OutputId,
        included: bool,
    },
    StingerPreloadChanged {
        slot: StingerSlotId,
        state: StingerPreloadState,
    },
    StingerFallbackApplied {
        slot: StingerSlotId,
        fallback: MissingMediaFallback,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SwitcherError {
    UnknownInput(InputId),
    TransitionInProgress,
    UnsupportedManualTransitionKind,
    InvalidManualTransitionRoute,
    ZeroDuration,
    InvalidOverlayTransitionDuration {
        duration_frames: u32,
        maximum: u32,
    },
    OverlayQueueFull {
        channel: OverlayChannelId,
        maximum: usize,
    },
    OverlayQueueEmpty(OverlayChannelId),
    UnconfiguredStinger(StingerSlotId),
    StingerCutPointOutOfRange {
        slot: StingerSlotId,
        cut_point_frames: u32,
        duration_frames: u32,
    },
}

impl core::fmt::Display for SwitcherError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownInput(id) => write!(formatter, "input {id} is not part of this mix"),
            Self::TransitionInProgress => {
                formatter.write_str("a transition is already in progress")
            }
            Self::UnsupportedManualTransitionKind => formatter
                .write_str("manual transitions support only Fade, Wipe, AlphaFade, and Slide"),
            Self::InvalidManualTransitionRoute => {
                formatter.write_str("manual transition endpoints must match Program and Preview")
            }
            Self::ZeroDuration => formatter.write_str("transition duration must be nonzero"),
            Self::InvalidOverlayTransitionDuration {
                duration_frames,
                maximum,
            } => write!(
                formatter,
                "overlay transition duration {duration_frames} is outside 1..={maximum}"
            ),
            Self::OverlayQueueFull { channel, maximum } => write!(
                formatter,
                "overlay channel {} queue is full (maximum {maximum})",
                channel.number()
            ),
            Self::OverlayQueueEmpty(channel) => {
                write!(
                    formatter,
                    "overlay channel {} queue is empty",
                    channel.number()
                )
            }
            Self::UnconfiguredStinger(slot) => {
                write!(
                    formatter,
                    "stinger slot {} is not configured",
                    slot.number()
                )
            }
            Self::StingerCutPointOutOfRange {
                slot,
                cut_point_frames,
                duration_frames,
            } => write!(
                formatter,
                "stinger slot {} cut point {cut_point_frames} exceeds duration {duration_frames}",
                slot.number()
            ),
        }
    }
}

impl std::error::Error for SwitcherError {}
