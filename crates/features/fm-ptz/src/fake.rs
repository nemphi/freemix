use std::collections::BTreeMap;

use crate::{
    AbsoluteMove, AdapterError, Axis, AxisCapabilities, AxisLimits, CameraCapabilities,
    CameraDescriptor, CameraId, CameraPosition, CommandOutcome, ConnectionState, DisconnectReason,
    MovementId, MovementKind, MovementState, Preset, PresetId, PtzAdapter, PtzIntent,
    RecoveryTelemetry, RelativeMove, Telemetry,
};

/// Configuration used to create one fake VISCA-like camera.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FakeViscaCamera {
    pub id: CameraId,
    pub name: String,
    pub capabilities: CameraCapabilities,
    pub initial_position: CameraPosition,
}

impl FakeViscaCamera {
    #[must_use]
    pub fn new(id: CameraId, name: impl Into<String>) -> Self {
        let capabilities = visca_capabilities();
        Self {
            id,
            name: name.into(),
            initial_position: CameraPosition {
                pan: 0,
                tilt: 0,
                zoom: Some(0),
                focus: Some(0),
            },
            capabilities,
        }
    }
}

fn visca_capabilities() -> CameraCapabilities {
    CameraCapabilities {
        pan: axis(-2_448, 2_448, 24),
        tilt: axis(-432, 1_296, 20),
        zoom: Some(axis(0, 16_384, 7)),
        focus: Some(axis(0, 4_095, 7)),
        absolute_movement: true,
        relative_movement: true,
        home: true,
        preset_slots: 255,
    }
}

fn axis(min: i32, max: i32, max_speed: i32) -> AxisCapabilities {
    AxisCapabilities::new(
        AxisLimits::new(min, max).expect("fake VISCA limits are ordered"),
        max_speed,
    )
    .expect("fake VISCA speed is positive")
}

#[derive(Clone, Debug)]
struct CameraState {
    descriptor: CameraDescriptor,
    telemetry: Telemetry,
    presets: BTreeMap<PresetId, Preset>,
}

/// Deterministic in-memory adapter with common VISCA coordinate ranges.
#[derive(Clone, Debug, Default)]
pub struct FakeViscaAdapter {
    cameras: BTreeMap<CameraId, CameraState>,
    next_movement_id: u64,
}

impl FakeViscaAdapter {
    #[must_use]
    pub fn new(cameras: impl IntoIterator<Item = FakeViscaCamera>) -> Self {
        let cameras = cameras
            .into_iter()
            .map(|camera| {
                let id = camera.id.clone();
                let position = camera.initial_position.clamped(&camera.capabilities);
                let state = CameraState {
                    descriptor: CameraDescriptor {
                        id: id.clone(),
                        name: camera.name,
                        capabilities: camera.capabilities,
                    },
                    telemetry: Telemetry {
                        connection: ConnectionState::Connected { generation: 1 },
                        position,
                        movement: MovementState::Idle,
                        commands_accepted: 0,
                        recovery: RecoveryTelemetry::default(),
                        last_error: None,
                    },
                    presets: BTreeMap::new(),
                };
                (id, state)
            })
            .collect();
        Self {
            cameras,
            next_movement_id: 1,
        }
    }

    /// Simulates a transport failure without depending on a network stack.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::CameraNotFound`] for an unknown camera.
    pub fn disconnect(
        &mut self,
        camera_id: &CameraId,
        reason: DisconnectReason,
    ) -> Result<Telemetry, AdapterError> {
        let camera = self.camera_mut(camera_id)?;
        camera.telemetry.connection = ConnectionState::Disconnected { reason };
        if let MovementState::Moving { id, kind } = camera.telemetry.movement {
            camera.telemetry.movement = MovementState::Stopped { id, kind };
        }
        camera.telemetry.last_error = Some(format!("disconnected: {reason:?}"));
        Ok(camera.telemetry.clone())
    }

    fn camera(&self, camera_id: &CameraId) -> Result<&CameraState, AdapterError> {
        self.cameras
            .get(camera_id)
            .ok_or_else(|| AdapterError::CameraNotFound(camera_id.clone()))
    }

    fn camera_mut(&mut self, camera_id: &CameraId) -> Result<&mut CameraState, AdapterError> {
        self.cameras
            .get_mut(camera_id)
            .ok_or_else(|| AdapterError::CameraNotFound(camera_id.clone()))
    }

    fn movement_id(&mut self) -> Result<MovementId, AdapterError> {
        let id = self.next_movement_id;
        self.next_movement_id = id
            .checked_add(1)
            .ok_or(AdapterError::MovementCounterExhausted)?;
        Ok(MovementId::new(id))
    }

    fn validate_intent(
        capabilities: &CameraCapabilities,
        intent: &PtzIntent,
    ) -> Result<(), AdapterError> {
        match intent {
            PtzIntent::MoveAbsolute(movement) => {
                if !capabilities.absolute_movement {
                    return Err(AdapterError::UnsupportedIntent("absolute movement"));
                }
                validate_optional_axes(capabilities, movement.zoom, movement.focus)
            }
            PtzIntent::MoveRelative(movement) => {
                if !capabilities.relative_movement {
                    return Err(AdapterError::UnsupportedIntent("relative movement"));
                }
                validate_optional_axes(capabilities, movement.zoom, movement.focus)
            }
            PtzIntent::MoveContinuous(movement) => validate_optional_axes(
                capabilities,
                (movement.zoom != 0).then_some(movement.zoom),
                (movement.focus != 0).then_some(movement.focus),
            ),
            PtzIntent::Home if !capabilities.home => Err(AdapterError::UnsupportedIntent("home")),
            PtzIntent::SavePreset { id, .. }
            | PtzIntent::RecallPreset(id)
            | PtzIntent::DeletePreset(id)
                if !capabilities.supports_preset(*id) =>
            {
                Err(AdapterError::PresetOutOfRange(*id))
            }
            _ => Ok(()),
        }
    }

    fn apply_movement(
        camera: &mut CameraState,
        movement_id: MovementId,
        intent: &PtzIntent,
    ) -> Result<MovementState, AdapterError> {
        let movement = match intent {
            PtzIntent::MoveAbsolute(movement) => {
                camera.telemetry.position = absolute_position(*movement);
                MovementState::Completed {
                    id: movement_id,
                    kind: MovementKind::Absolute,
                }
            }
            PtzIntent::MoveRelative(movement) => {
                apply_relative(&mut camera.telemetry.position, *movement);
                MovementState::Completed {
                    id: movement_id,
                    kind: MovementKind::Relative,
                }
            }
            PtzIntent::MoveContinuous(_) => MovementState::Moving {
                id: movement_id,
                kind: MovementKind::Continuous,
            },
            PtzIntent::Home => {
                camera.telemetry.position = CameraPosition {
                    pan: 0,
                    tilt: 0,
                    zoom: camera
                        .descriptor
                        .capabilities
                        .zoom
                        .map(|axis| axis.position.min),
                    focus: camera
                        .descriptor
                        .capabilities
                        .focus
                        .map(|axis| axis.position.min),
                };
                MovementState::Completed {
                    id: movement_id,
                    kind: MovementKind::Home,
                }
            }
            PtzIntent::RecallPreset(id) => {
                let preset = camera
                    .presets
                    .get(id)
                    .ok_or(AdapterError::PresetNotFound(*id))?;
                camera.telemetry.position = preset.position;
                MovementState::Completed {
                    id: movement_id,
                    kind: MovementKind::Preset,
                }
            }
            _ => return Ok(camera.telemetry.movement),
        };
        Ok(movement)
    }
}

fn validate_optional_axes(
    capabilities: &CameraCapabilities,
    zoom: Option<i32>,
    focus: Option<i32>,
) -> Result<(), AdapterError> {
    if zoom.is_some() && capabilities.zoom.is_none() {
        return Err(AdapterError::UnsupportedAxis(Axis::Zoom));
    }
    if focus.is_some() && capabilities.focus.is_none() {
        return Err(AdapterError::UnsupportedAxis(Axis::Focus));
    }
    Ok(())
}

fn absolute_position(movement: AbsoluteMove) -> CameraPosition {
    CameraPosition {
        pan: movement.pan,
        tilt: movement.tilt,
        zoom: movement.zoom,
        focus: movement.focus,
    }
}

fn apply_relative(position: &mut CameraPosition, movement: RelativeMove) {
    position.pan += movement.pan;
    position.tilt += movement.tilt;
    if let (Some(current), Some(delta)) = (&mut position.zoom, movement.zoom) {
        *current += delta;
    }
    if let (Some(current), Some(delta)) = (&mut position.focus, movement.focus) {
        *current += delta;
    }
}

impl PtzAdapter for FakeViscaAdapter {
    fn cameras(&self) -> Vec<CameraDescriptor> {
        self.cameras
            .values()
            .map(|camera| camera.descriptor.clone())
            .collect()
    }

    fn telemetry(&self, camera_id: &CameraId) -> Result<Telemetry, AdapterError> {
        Ok(self.camera(camera_id)?.telemetry.clone())
    }

    fn presets(&self, camera_id: &CameraId) -> Result<Vec<Preset>, AdapterError> {
        Ok(self.camera(camera_id)?.presets.values().cloned().collect())
    }

    fn execute(
        &mut self,
        camera_id: &CameraId,
        intent: PtzIntent,
    ) -> Result<CommandOutcome, AdapterError> {
        let camera = self.camera(camera_id)?;
        if !matches!(
            camera.telemetry.connection,
            ConnectionState::Connected { .. }
        ) {
            return Err(AdapterError::Disconnected(camera_id.clone()));
        }
        Self::validate_intent(&camera.descriptor.capabilities, &intent)?;
        let applied = intent.clamped(&camera.descriptor.capabilities, camera.telemetry.position);

        let needs_movement_id = matches!(
            applied,
            PtzIntent::MoveAbsolute(_)
                | PtzIntent::MoveRelative(_)
                | PtzIntent::MoveContinuous(_)
                | PtzIntent::Home
                | PtzIntent::RecallPreset(_)
        );
        let movement_id = needs_movement_id.then(|| self.movement_id()).transpose()?;
        let camera = self.camera_mut(camera_id)?;

        let movement = match &applied {
            PtzIntent::Stop => match camera.telemetry.movement {
                MovementState::Moving { id, kind } => MovementState::Stopped { id, kind },
                existing => existing,
            },
            PtzIntent::SavePreset { id, name } => {
                camera.presets.insert(
                    *id,
                    Preset {
                        id: *id,
                        name: name.clone(),
                        position: camera.telemetry.position,
                    },
                );
                camera.telemetry.movement
            }
            PtzIntent::DeletePreset(id) => {
                if camera.presets.remove(id).is_none() {
                    return Err(AdapterError::PresetNotFound(*id));
                }
                camera.telemetry.movement
            }
            _ => Self::apply_movement(
                camera,
                movement_id.expect("movement intent has an allocated ID"),
                &applied,
            )?,
        };

        camera.telemetry.movement = movement;
        camera.telemetry.commands_accepted = camera.telemetry.commands_accepted.saturating_add(1);
        camera.telemetry.last_error = None;
        Ok(CommandOutcome {
            applied,
            movement,
            telemetry: camera.telemetry.clone(),
        })
    }

    fn begin_recovery(&mut self, camera_id: &CameraId) -> Result<Telemetry, AdapterError> {
        let camera = self.camera_mut(camera_id)?;
        camera.telemetry.recovery.attempts = camera.telemetry.recovery.attempts.saturating_add(1);
        camera.telemetry.connection = ConnectionState::Recovering {
            attempt: camera.telemetry.recovery.attempts,
        };
        Ok(camera.telemetry.clone())
    }

    fn complete_recovery(&mut self, camera_id: &CameraId) -> Result<Telemetry, AdapterError> {
        let camera = self.camera_mut(camera_id)?;
        if !matches!(
            camera.telemetry.connection,
            ConnectionState::Recovering { .. }
        ) {
            return Err(AdapterError::RecoveryNotStarted(camera_id.clone()));
        }
        camera.telemetry.recovery.successes = camera.telemetry.recovery.successes.saturating_add(1);
        camera.telemetry.connection = ConnectionState::Connected {
            generation: u64::from(camera.telemetry.recovery.successes) + 1,
        };
        camera.telemetry.last_error = None;
        Ok(camera.telemetry.clone())
    }
}
