//! Portable PTZ intents, presets, command queues, and adapter contracts.
//!
//! This crate deliberately contains no transport or device SDK integration.
//! Adapters translate these domain contracts to a protocol at the boundary.

mod adapter;
mod domain;
mod fake;
mod queue;

pub use adapter::{AdapterError, CommandOutcome, PtzAdapter};
pub use domain::{
    AbsoluteMove, Axis, AxisCapabilities, AxisLimits, CameraCapabilities, CameraDescriptor,
    CameraId, CameraIdError, CameraPosition, ConnectionState, ContinuousMove, ContinuousSource,
    DisconnectReason, MovementId, MovementKind, MovementState, Preset, PresetId,
    PresetVirtualInputId, PtzIntent, RecoveryTelemetry, RelativeMove, Telemetry,
};
pub use fake::{FakeViscaAdapter, FakeViscaCamera};
pub use queue::{CoalescingQueue, PushOutcome, QueueError, QueuedIntent};
