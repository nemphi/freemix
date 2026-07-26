//! Deterministic preview/program switching primitives.

mod command;
mod overlay;
mod state;
mod stinger;
mod transition;

pub use command::{SwitcherCommand, SwitcherError, SwitcherEvent};
pub use overlay::{OVERLAY_CHANNEL_COUNT, OverlayChannelId, OverlayChannelState};
pub use state::{ProgramFrame, SwitcherState};
pub use stinger::{
    MissingMediaFallback, STINGER_SLOT_COUNT, StingerAudioPolicy, StingerDescriptor,
    StingerPlaybackDecision, StingerPreloadState, StingerSlotId, StingerSlotState,
};
pub use transition::{TBarPosition, TBarState, TransitionKind, TransitionState};
