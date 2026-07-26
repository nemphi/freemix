use core::fmt;

/// Stable identifier assigned to a camera by the application.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CameraId(String);

impl CameraId {
    /// Creates an identifier. Empty and whitespace-only identifiers are rejected.
    ///
    /// # Errors
    ///
    /// Returns [`CameraIdError`] when the value is empty or whitespace-only.
    pub fn new(value: impl Into<String>) -> Result<Self, CameraIdError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(CameraIdError);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CameraId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Error returned for an empty camera identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CameraIdError;

impl fmt::Display for CameraIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("camera ID must not be empty")
    }
}

impl std::error::Error for CameraIdError {}

/// Device preset slot. Slot zero is valid because some protocols expose it.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PresetId(u16);

impl PresetId {
    #[must_use]
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl fmt::Display for PresetId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A preset exposed as a selectable virtual input.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PresetVirtualInputId {
    camera_id: CameraId,
    preset_id: PresetId,
}

impl PresetVirtualInputId {
    #[must_use]
    pub const fn new(camera_id: CameraId, preset_id: PresetId) -> Self {
        Self {
            camera_id,
            preset_id,
        }
    }

    #[must_use]
    pub const fn camera_id(&self) -> &CameraId {
        &self.camera_id
    }

    #[must_use]
    pub const fn preset_id(&self) -> PresetId {
        self.preset_id
    }
}

impl fmt::Display for PresetVirtualInputId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "ptz-preset:{}:{}",
            self.camera_id, self.preset_id
        )
    }
}

/// Inclusive units accepted by one camera axis.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AxisLimits {
    pub min: i32,
    pub max: i32,
}

impl AxisLimits {
    /// Creates valid inclusive limits.
    #[must_use]
    pub const fn new(min: i32, max: i32) -> Option<Self> {
        if min <= max {
            Some(Self { min, max })
        } else {
            None
        }
    }

    #[must_use]
    pub fn clamp(self, value: i32) -> i32 {
        value.clamp(self.min, self.max)
    }

    #[must_use]
    pub fn clamp_delta(self, current: i32, delta: i32) -> i32 {
        let current = self.clamp(current);
        let target = i64::from(current) + i64::from(delta);
        let target = target.clamp(i64::from(self.min), i64::from(self.max));
        let delta = target - i64::from(current);
        i32::try_from(delta).unwrap_or(if delta.is_negative() {
            i32::MIN
        } else {
            i32::MAX
        })
    }
}

/// Position and continuous-speed limits for one axis.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AxisCapabilities {
    pub position: AxisLimits,
    pub max_speed: i32,
}

impl AxisCapabilities {
    /// Creates capabilities when maximum speed is positive.
    #[must_use]
    pub const fn new(position: AxisLimits, max_speed: i32) -> Option<Self> {
        if max_speed > 0 {
            Some(Self {
                position,
                max_speed,
            })
        } else {
            None
        }
    }

    #[must_use]
    pub fn clamp_speed(self, speed: i32) -> i32 {
        speed.clamp(-self.max_speed, self.max_speed)
    }
}

/// A controllable camera axis.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Axis {
    Pan,
    Tilt,
    Zoom,
    Focus,
}

/// Features and hardware ranges reported by a camera adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CameraCapabilities {
    pub pan: AxisCapabilities,
    pub tilt: AxisCapabilities,
    pub zoom: Option<AxisCapabilities>,
    pub focus: Option<AxisCapabilities>,
    pub absolute_movement: bool,
    pub relative_movement: bool,
    pub home: bool,
    /// Number of preset slots, starting at slot zero.
    pub preset_slots: u16,
}

impl CameraCapabilities {
    #[must_use]
    pub fn supports_preset(&self, preset_id: PresetId) -> bool {
        preset_id.get() < self.preset_slots
    }

    #[must_use]
    pub const fn axis(&self, axis: Axis) -> Option<AxisCapabilities> {
        match axis {
            Axis::Pan => Some(self.pan),
            Axis::Tilt => Some(self.tilt),
            Axis::Zoom => self.zoom,
            Axis::Focus => self.focus,
        }
    }
}

/// Camera metadata discovered by an adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CameraDescriptor {
    pub id: CameraId,
    pub name: String,
    pub capabilities: CameraCapabilities,
}

/// Last known camera coordinates in protocol-native integer units.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CameraPosition {
    pub pan: i32,
    pub tilt: i32,
    pub zoom: Option<i32>,
    pub focus: Option<i32>,
}

impl CameraPosition {
    #[must_use]
    pub fn clamped(self, capabilities: &CameraCapabilities) -> Self {
        Self {
            pan: capabilities.pan.position.clamp(self.pan),
            tilt: capabilities.tilt.position.clamp(self.tilt),
            zoom: clamp_optional_position(self.zoom, capabilities.zoom),
            focus: clamp_optional_position(self.focus, capabilities.focus),
        }
    }
}

fn clamp_optional_position(
    value: Option<i32>,
    capabilities: Option<AxisCapabilities>,
) -> Option<i32> {
    value
        .zip(capabilities)
        .map(|(value, axis)| axis.position.clamp(value))
}

/// Absolute destination for all supplied axes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AbsoluteMove {
    pub pan: i32,
    pub tilt: i32,
    pub zoom: Option<i32>,
    pub focus: Option<i32>,
}

impl AbsoluteMove {
    #[must_use]
    pub fn clamped(self, capabilities: &CameraCapabilities) -> Self {
        Self {
            pan: capabilities.pan.position.clamp(self.pan),
            tilt: capabilities.tilt.position.clamp(self.tilt),
            zoom: clamp_optional_position(self.zoom, capabilities.zoom),
            focus: clamp_optional_position(self.focus, capabilities.focus),
        }
    }
}

/// Relative displacement for all supplied axes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RelativeMove {
    pub pan: i32,
    pub tilt: i32,
    pub zoom: Option<i32>,
    pub focus: Option<i32>,
}

impl RelativeMove {
    #[must_use]
    pub fn clamped(self, capabilities: &CameraCapabilities, current: CameraPosition) -> Self {
        Self {
            pan: capabilities.pan.position.clamp_delta(current.pan, self.pan),
            tilt: capabilities
                .tilt
                .position
                .clamp_delta(current.tilt, self.tilt),
            zoom: clamp_optional_delta(self.zoom, current.zoom, capabilities.zoom),
            focus: clamp_optional_delta(self.focus, current.focus, capabilities.focus),
        }
    }
}

fn clamp_optional_delta(
    delta: Option<i32>,
    current: Option<i32>,
    capabilities: Option<AxisCapabilities>,
) -> Option<i32> {
    delta
        .zip(current)
        .zip(capabilities)
        .map(|((delta, current), axis)| axis.position.clamp_delta(current, delta))
}

/// Origin of a continuous move, used as its coalescing key.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ContinuousSource {
    Joystick,
    Mouse,
}

/// Signed continuous velocity in protocol-native units.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContinuousMove {
    pub source: ContinuousSource,
    pub pan: i32,
    pub tilt: i32,
    pub zoom: i32,
    pub focus: i32,
}

impl ContinuousMove {
    #[must_use]
    pub fn clamped(self, capabilities: &CameraCapabilities) -> Self {
        Self {
            source: self.source,
            pan: capabilities.pan.clamp_speed(self.pan),
            tilt: capabilities.tilt.clamp_speed(self.tilt),
            zoom: capabilities
                .zoom
                .map_or(0, |axis| axis.clamp_speed(self.zoom)),
            focus: capabilities
                .focus
                .map_or(0, |axis| axis.clamp_speed(self.focus)),
        }
    }
}

/// Transport-neutral command accepted by a PTZ adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PtzIntent {
    MoveAbsolute(AbsoluteMove),
    MoveRelative(RelativeMove),
    MoveContinuous(ContinuousMove),
    Stop,
    Home,
    SavePreset { id: PresetId, name: String },
    RecallPreset(PresetId),
    DeletePreset(PresetId),
}

impl PtzIntent {
    #[must_use]
    pub const fn is_continuous(&self) -> bool {
        matches!(self, Self::MoveContinuous(_))
    }

    #[must_use]
    pub fn clamped(self, capabilities: &CameraCapabilities, current: CameraPosition) -> Self {
        match self {
            Self::MoveAbsolute(movement) => Self::MoveAbsolute(movement.clamped(capabilities)),
            Self::MoveRelative(movement) => {
                Self::MoveRelative(movement.clamped(capabilities, current))
            }
            Self::MoveContinuous(movement) => Self::MoveContinuous(movement.clamped(capabilities)),
            discrete => discrete,
        }
    }
}

/// Stored camera destination.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Preset {
    pub id: PresetId,
    pub name: String,
    pub position: CameraPosition,
}

impl Preset {
    #[must_use]
    pub const fn virtual_input_id(&self, camera_id: CameraId) -> PresetVirtualInputId {
        PresetVirtualInputId::new(camera_id, self.id)
    }
}

/// Monotonic movement identifier scoped to an adapter instance.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MovementId(u64);

impl MovementId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Kind of motion tracked by movement telemetry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MovementKind {
    Absolute,
    Relative,
    Continuous,
    Preset,
    Home,
}

/// Observable lifecycle of the latest movement.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MovementState {
    #[default]
    Idle,
    Moving {
        id: MovementId,
        kind: MovementKind,
    },
    Completed {
        id: MovementId,
        kind: MovementKind,
    },
    Stopped {
        id: MovementId,
        kind: MovementKind,
    },
}

/// Why an adapter considers a camera disconnected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisconnectReason {
    Requested,
    TransportLost,
    Timeout,
    DeviceRejected,
}

/// Connection and recovery lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionState {
    Connected { generation: u64 },
    Disconnected { reason: DisconnectReason },
    Recovering { attempt: u32 },
}

/// Cumulative recovery counters retained across reconnections.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RecoveryTelemetry {
    pub attempts: u32,
    pub successes: u32,
}

/// Last-known state exposed without requiring protocol-specific telemetry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Telemetry {
    pub connection: ConnectionState,
    pub position: CameraPosition,
    pub movement: MovementState,
    pub commands_accepted: u64,
    pub recovery: RecoveryTelemetry,
    pub last_error: Option<String>,
}
