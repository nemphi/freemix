use fm_ptz::{
    AbsoluteMove, AdapterError, AxisLimits, CameraId, ConnectionState, ContinuousMove,
    ContinuousSource, DisconnectReason, FakeViscaAdapter, FakeViscaCamera, MovementKind,
    MovementState, PresetId, PresetVirtualInputId, PtzAdapter, PtzIntent, RelativeMove,
};

fn camera_id() -> CameraId {
    CameraId::new("camera-1").unwrap()
}

fn adapter() -> FakeViscaAdapter {
    FakeViscaAdapter::new([FakeViscaCamera::new(camera_id(), "Studio camera")])
}

#[test]
fn absolute_relative_and_continuous_moves_clamp_to_limits() {
    let mut adapter = adapter();
    let id = camera_id();

    let outcome = adapter
        .execute(
            &id,
            PtzIntent::MoveAbsolute(AbsoluteMove {
                pan: i32::MAX,
                tilt: i32::MIN,
                zoom: Some(20_000),
                focus: Some(-1),
            }),
        )
        .unwrap();
    assert_eq!(outcome.telemetry.position.pan, 2_448);
    assert_eq!(outcome.telemetry.position.tilt, -432);
    assert_eq!(outcome.telemetry.position.zoom, Some(16_384));
    assert_eq!(outcome.telemetry.position.focus, Some(0));

    let outcome = adapter
        .execute(
            &id,
            PtzIntent::MoveRelative(RelativeMove {
                pan: i32::MIN,
                tilt: i32::MAX,
                zoom: Some(i32::MIN),
                focus: Some(i32::MAX),
            }),
        )
        .unwrap();
    assert_eq!(outcome.telemetry.position.pan, -2_448);
    assert_eq!(outcome.telemetry.position.tilt, 1_296);
    assert_eq!(outcome.telemetry.position.zoom, Some(0));
    assert_eq!(outcome.telemetry.position.focus, Some(4_095));

    let outcome = adapter
        .execute(
            &id,
            PtzIntent::MoveContinuous(ContinuousMove {
                source: ContinuousSource::Joystick,
                pan: 100,
                tilt: -100,
                zoom: 100,
                focus: -100,
            }),
        )
        .unwrap();
    assert_eq!(
        outcome.applied,
        PtzIntent::MoveContinuous(ContinuousMove {
            source: ContinuousSource::Joystick,
            pan: 24,
            tilt: -20,
            zoom: 7,
            focus: -7,
        })
    );
}

#[test]
fn axis_limits_normalize_an_out_of_range_current_position() {
    let limits = AxisLimits::new(-10, 10).unwrap();
    assert_eq!(limits.clamp_delta(i32::MIN, 5), 5);
    assert_eq!(limits.clamp_delta(i32::MAX, -5), -5);
}

#[test]
fn presets_round_trip_and_have_stable_virtual_input_ids() {
    let mut adapter = adapter();
    let id = camera_id();
    let preset_id = PresetId::new(7);
    adapter
        .execute(
            &id,
            PtzIntent::MoveAbsolute(AbsoluteMove {
                pan: 100,
                tilt: 200,
                zoom: Some(300),
                focus: Some(400),
            }),
        )
        .unwrap();
    adapter
        .execute(
            &id,
            PtzIntent::SavePreset {
                id: preset_id,
                name: "Desk".into(),
            },
        )
        .unwrap();
    adapter.execute(&id, PtzIntent::Home).unwrap();

    let outcome = adapter
        .execute(&id, PtzIntent::RecallPreset(preset_id))
        .unwrap();
    assert_eq!(outcome.telemetry.position.pan, 100);
    assert!(matches!(
        outcome.movement,
        MovementState::Completed {
            kind: MovementKind::Preset,
            ..
        }
    ));
    let preset = adapter.presets(&id).unwrap().pop().unwrap();
    assert_eq!(
        preset.virtual_input_id(id.clone()),
        PresetVirtualInputId::new(id, preset_id)
    );
    assert_eq!(
        preset.virtual_input_id(camera_id()).to_string(),
        "ptz-preset:camera-1:7"
    );
}

#[test]
fn movement_stop_and_home_have_observable_lifecycles() {
    let mut adapter = adapter();
    let id = camera_id();
    let moving = adapter
        .execute(
            &id,
            PtzIntent::MoveContinuous(ContinuousMove {
                source: ContinuousSource::Mouse,
                pan: 1,
                tilt: 0,
                zoom: 0,
                focus: 0,
            }),
        )
        .unwrap();
    let movement_id = match moving.movement {
        MovementState::Moving { id, .. } => id,
        state => panic!("expected moving state, got {state:?}"),
    };

    let stopped = adapter.execute(&id, PtzIntent::Stop).unwrap();
    assert_eq!(
        stopped.movement,
        MovementState::Stopped {
            id: movement_id,
            kind: MovementKind::Continuous,
        }
    );
    let home = adapter.execute(&id, PtzIntent::Home).unwrap();
    assert_eq!(home.telemetry.position.pan, 0);
    assert_eq!(home.telemetry.position.tilt, 0);
    assert!(matches!(
        home.movement,
        MovementState::Completed {
            kind: MovementKind::Home,
            ..
        }
    ));
}

#[test]
fn disconnect_blocks_commands_and_recovery_is_observable() {
    let mut adapter = adapter();
    let id = camera_id();
    let telemetry = adapter
        .disconnect(&id, DisconnectReason::TransportLost)
        .unwrap();
    assert_eq!(
        telemetry.connection,
        ConnectionState::Disconnected {
            reason: DisconnectReason::TransportLost
        }
    );
    assert_eq!(
        adapter.execute(&id, PtzIntent::Stop),
        Err(AdapterError::Disconnected(id.clone()))
    );

    let recovering = adapter.begin_recovery(&id).unwrap();
    assert_eq!(
        recovering.connection,
        ConnectionState::Recovering { attempt: 1 }
    );
    let connected = adapter.complete_recovery(&id).unwrap();
    assert_eq!(
        connected.connection,
        ConnectionState::Connected { generation: 2 }
    );
    assert_eq!(connected.recovery.attempts, 1);
    assert_eq!(connected.recovery.successes, 1);
    adapter.execute(&id, PtzIntent::Stop).unwrap();
}

#[test]
fn fake_adapter_conforms_to_discovery_and_error_contracts() {
    fn assert_adapter<T: PtzAdapter>(adapter: &mut T, id: &CameraId) {
        assert_eq!(adapter.cameras().len(), 1);
        assert_eq!(adapter.telemetry(id).unwrap().commands_accepted, 0);
        adapter.execute(id, PtzIntent::Home).unwrap();
        assert_eq!(adapter.telemetry(id).unwrap().commands_accepted, 1);
    }

    let mut adapter = adapter();
    assert_adapter(&mut adapter, &camera_id());
    let missing = CameraId::new("missing").unwrap();
    assert_eq!(
        adapter.telemetry(&missing),
        Err(AdapterError::CameraNotFound(missing))
    );
    assert_eq!(
        adapter.execute(&camera_id(), PtzIntent::RecallPreset(PresetId::new(254))),
        Err(AdapterError::PresetNotFound(PresetId::new(254)))
    );
    assert_eq!(
        adapter.execute(&camera_id(), PtzIntent::RecallPreset(PresetId::new(255))),
        Err(AdapterError::PresetOutOfRange(PresetId::new(255)))
    );
}
