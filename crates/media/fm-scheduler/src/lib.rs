//! Deterministic, runtime-independent media scheduling primitives.

mod action;
mod pacing;
mod plan;
mod queue;
mod scheduler;
mod telemetry;
mod types;

pub use action::{ActionError, ActionQueue, ScheduledAction};
pub use pacing::{FramePacer, PacingError};
pub use plan::{PendingPlan, PlanActivation, PlanError, PlanManager};
pub use queue::{
    BoundedQueue, InputQueueError, InputQueues, QueueConfigError, QueuePolicy, QueuePush,
};
pub use scheduler::{FrameScheduler, FrameTick, TickError};
pub use telemetry::SchedulerTelemetry;
pub use types::{ActionId, FrameDeadline, FrameNumber, PlanGeneration};
