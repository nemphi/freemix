use fm_types::{InputId, OutputId};

use crate::{
    MissingMediaFallback, OverlayChannelId, StingerPreloadState, StingerSlotId, TBarPosition,
    TransitionKind,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SwitcherCommand {
    SelectPreview(InputId),
    Cut,
    Transition {
        kind: TransitionKind,
        duration_frames: u32,
    },
    StartTBar {
        kind: TransitionKind,
    },
    SetTBarPosition(TBarPosition),
    CommitTBar,
    CancelTBar,
    SetFadeToBlack(bool),
    TakeOverlay {
        channel: OverlayChannelId,
        source: InputId,
    },
    UpdateOverlay {
        channel: OverlayChannelId,
        source: InputId,
    },
    OverlayOff(OverlayChannelId),
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
    FadeToBlackChanged {
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
    ZeroDuration,
}

impl core::fmt::Display for SwitcherError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownInput(id) => write!(formatter, "input {id} is not part of this mix"),
            Self::TransitionInProgress => {
                formatter.write_str("a transition is already in progress")
            }
            Self::ZeroDuration => formatter.write_str("transition duration must be nonzero"),
        }
    }
}

impl std::error::Error for SwitcherError {}
