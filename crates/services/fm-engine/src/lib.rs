//! Deterministic composition root for the simulated `FreeMix` engine.

mod engine;
mod error;
mod state;

pub use engine::{
    Engine, EngineAcceptance, EngineCommand, EngineCommandOutcome, EngineEvent,
    EngineFadeToBlackState, EngineManualTransitionKind, EngineManualTransitionPosition,
    EngineManualTransitionState, EnginePrepareOutcome, EngineRestoreState, EngineSnapshot,
    FrameResult, PreparedEngineExecution,
};
pub use error::{EngineError, ShowError, SnapshotError};
pub use state::ShowState;
