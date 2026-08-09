//! Deterministic composition root for the simulated `FreeMix` engine.

mod engine;
mod error;
mod state;

pub use engine::{
    Engine, EngineAcceptance, EngineCommand, EngineCommandOutcome, EngineEvent,
    EngineFadeToBlackState, EngineManualTransitionKind, EngineManualTransitionPosition,
    EngineManualTransitionState, EnginePrepareOutcome, EngineRestoreState, EngineSnapshot,
    FrameResult, MAX_INPUT_AUDIO_DELAY_SAMPLES, MAX_INPUT_AUDIO_GAIN_MILLIDB,
    MIN_INPUT_AUDIO_GAIN_MILLIDB, PreparedEngineExecution,
};
pub use error::{EngineError, ShowError, SnapshotError};
pub use state::{EngineInputAudioStripState, ShowState};
