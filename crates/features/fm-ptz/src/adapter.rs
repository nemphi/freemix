use core::fmt;

use crate::{
    Axis, CameraDescriptor, CameraId, MovementState, Preset, PresetId, PtzIntent, Telemetry,
};

/// Portable errors shared by PTZ adapters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdapterError {
    CameraNotFound(CameraId),
    Disconnected(CameraId),
    RecoveryNotStarted(CameraId),
    UnsupportedAxis(Axis),
    UnsupportedIntent(&'static str),
    PresetOutOfRange(PresetId),
    PresetNotFound(PresetId),
    MovementCounterExhausted,
}

impl fmt::Display for AdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CameraNotFound(id) => write!(formatter, "camera {id} was not found"),
            Self::Disconnected(id) => write!(formatter, "camera {id} is disconnected"),
            Self::RecoveryNotStarted(id) => {
                write!(formatter, "camera {id} is not recovering")
            }
            Self::UnsupportedAxis(axis) => write!(formatter, "unsupported PTZ axis: {axis:?}"),
            Self::UnsupportedIntent(intent) => {
                write!(formatter, "unsupported PTZ intent: {intent}")
            }
            Self::PresetOutOfRange(id) => write!(formatter, "preset {id} is out of range"),
            Self::PresetNotFound(id) => write!(formatter, "preset {id} does not exist"),
            Self::MovementCounterExhausted => formatter.write_str("movement counter exhausted"),
        }
    }
}

impl std::error::Error for AdapterError {}

/// Deterministic result of applying one intent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOutcome {
    /// Intent after adapter capability and position clamping.
    pub applied: PtzIntent,
    pub movement: MovementState,
    pub telemetry: Telemetry,
}

/// Synchronous, transport-neutral camera adapter contract.
///
/// Async runtimes can call implementations at their own I/O boundary. Recovery
/// is split into two operations so callers can observe and report the
/// `Recovering` state while reconnecting.
pub trait PtzAdapter {
    fn cameras(&self) -> Vec<CameraDescriptor>;

    /// Returns the camera's last-known state.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::CameraNotFound`] for an unknown camera.
    fn telemetry(&self, camera_id: &CameraId) -> Result<Telemetry, AdapterError>;

    /// Returns presets in ascending slot order.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::CameraNotFound`] for an unknown camera.
    fn presets(&self, camera_id: &CameraId) -> Result<Vec<Preset>, AdapterError>;

    /// Applies an intent to one camera.
    ///
    /// # Errors
    ///
    /// Returns an [`AdapterError`] when the camera is unavailable, the intent
    /// is unsupported, or a referenced preset does not exist.
    fn execute(
        &mut self,
        camera_id: &CameraId,
        intent: PtzIntent,
    ) -> Result<CommandOutcome, AdapterError>;

    /// Enters the observable recovery state.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::CameraNotFound`] for an unknown camera.
    fn begin_recovery(&mut self, camera_id: &CameraId) -> Result<Telemetry, AdapterError>;

    /// Marks an in-progress recovery as successful.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::CameraNotFound`] for an unknown camera or
    /// [`AdapterError::RecoveryNotStarted`] unless recovery was begun first.
    fn complete_recovery(&mut self, camera_id: &CameraId) -> Result<Telemetry, AdapterError>;
}
