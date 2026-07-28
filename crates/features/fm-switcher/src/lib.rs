//! Deterministic preview/program switching primitives.

mod command;
mod fade_to_black;
mod overlay;
mod state;
mod stinger;
mod transition;

pub use command::{SwitcherCommand, SwitcherError, SwitcherEvent};
pub use fade_to_black::{
    FADE_TO_BLACK_POSITION_DENOMINATOR, FadeToBlackAdvance, FadeToBlackController,
    FadeToBlackError, FadeToBlackFrame, FadeToBlackPosition, FadeToBlackRequest,
    FadeToBlackStarted, FadeToBlackTarget, MAX_FADE_TO_BLACK_DURATION_FRAMES,
};
pub use overlay::{OVERLAY_CHANNEL_COUNT, OverlayChannelId, OverlayChannelState};
pub use state::{ProgramFrame, SwitcherState};
pub use stinger::{
    MissingMediaFallback, STINGER_SLOT_COUNT, StingerAudioPolicy, StingerDescriptor,
    StingerPlaybackDecision, StingerPreloadState, StingerSlotId, StingerSlotState,
};
pub use transition::{TBarPosition, TBarState, TransitionKind, TransitionState};
