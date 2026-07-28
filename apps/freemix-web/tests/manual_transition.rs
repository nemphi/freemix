use core::num::NonZeroU128;

use fm_client::{ConnectionState, ReconnectBackoff, Session};
use fm_protocol::{
    ALPHA_FADE_PROTOCOL_VERSION, CommandPayload, FADE_TO_BLACK_PROTOCOL_VERSION,
    MANUAL_ALPHA_FADE_PROTOCOL_VERSION, MANUAL_TRANSITION_PROTOCOL_VERSION, ManualTransitionKind,
    ManualTransitionPosition, ProtocolVersion, Role, SLIDE_PROTOCOL_VERSION, ServerIdentity,
    ZOOM_PROTOCOL_VERSION,
};
use fm_types::InputId;
use fm_ui_model::{ActiveManualTransition, BusSelection, ManualTransitionStatus, SwitcherState};
use freemix_web::{
    ManualTransitionControl, ManualTransitionModel, ManualTransitionMotion,
    ManualTransitionProjection, SUPPORTED_PROTOCOL_VERSIONS, TransitionControlState,
};

fn input(value: u128) -> InputId {
    InputId::new(NonZeroU128::new(value).unwrap())
}

fn session(protocol: ProtocolVersion, permissions: &[&str]) -> Session {
    Session {
        protocol,
        granted_role: Role::Operator,
        permissions: permissions
            .iter()
            .map(|permission| (*permission).to_owned())
            .collect(),
        server: ServerIdentity {
            engine_id: "engine-web-manual-test".to_owned(),
            project_id: "project-web-manual-test".to_owned(),
            state_epoch: 1,
            log_id: "log-web-manual-test".to_owned(),
        },
        capabilities_digest: "sha256:web-manual-test".to_owned(),
    }
}

fn active(
    kind: ManualTransitionKind,
    from: u128,
    to: u128,
    interval_start: u16,
    position: u16,
) -> ManualTransitionStatus {
    ManualTransitionStatus::Active(ActiveManualTransition {
        kind,
        from: input(from),
        to: input(to),
        interval_start: ManualTransitionPosition::new(interval_start).unwrap(),
        position: ManualTransitionPosition::new(position).unwrap(),
    })
}

fn ready_session() -> Session {
    session(MANUAL_ALPHA_FADE_PROTOCOL_VERSION, &["transition"])
}

#[test]
fn web_handshake_advertises_only_protocol_1_9() {
    assert_eq!(SUPPORTED_PROTOCOL_VERSIONS, [ZOOM_PROTOCOL_VERSION]);
    assert_eq!(ZOOM_PROTOCOL_VERSION, ProtocolVersion::new(1, 9));
    assert_eq!(SLIDE_PROTOCOL_VERSION, ProtocolVersion::new(1, 8));
    assert_eq!(
        MANUAL_ALPHA_FADE_PROTOCOL_VERSION,
        ProtocolVersion::new(1, 7)
    );
    assert_eq!(ALPHA_FADE_PROTOCOL_VERSION, ProtocolVersion::new(1, 6));
    assert_eq!(FADE_TO_BLACK_PROTOCOL_VERSION, ProtocolVersion::new(1, 5));
    assert_eq!(
        MANUAL_TRANSITION_PROTOCOL_VERSION,
        ProtocolVersion::new(1, 4)
    );
}

#[test]
fn controls_have_stable_accessible_labels() {
    assert_eq!(
        ManualTransitionControl::StartFade.accessibility_label(),
        "Start manual Fade transition"
    );
    assert_eq!(
        ManualTransitionControl::StartWipe.accessibility_label(),
        "Start manual Wipe transition"
    );
    assert_eq!(
        ManualTransitionControl::StartAlphaFade.accessibility_label(),
        "Start manual AlphaFade transition"
    );
    assert_eq!(
        ManualTransitionControl::Position(ManualTransitionPosition::END).accessibility_label(),
        "Manual transition position in basis points"
    );
    assert_eq!(
        ManualTransitionControl::Commit.accessibility_label(),
        "Commit manual transition"
    );
    assert_eq!(
        ManualTransitionControl::Cancel.accessibility_label(),
        "Cancel manual transition"
    );
}

#[test]
fn idle_model_emits_all_start_kinds_and_no_active_commands() {
    let model = ManualTransitionModel::new(
        Some(ManualTransitionStatus::Inactive),
        Some(ManualTransitionStatus::Inactive),
    );
    let session = ready_session();

    assert_eq!(
        model.command_payload(
            ManualTransitionControl::StartFade,
            &ConnectionState::Ready,
            Some(&session),
        ),
        Some(CommandPayload::StartManualTransition {
            kind: ManualTransitionKind::Fade,
        })
    );
    assert_eq!(
        model.command_payload(
            ManualTransitionControl::StartWipe,
            &ConnectionState::Ready,
            Some(&session),
        ),
        Some(CommandPayload::StartManualTransition {
            kind: ManualTransitionKind::Wipe,
        })
    );
    assert_eq!(
        model.command_payload(
            ManualTransitionControl::StartAlphaFade,
            &ConnectionState::Ready,
            Some(&session),
        ),
        Some(CommandPayload::StartManualTransition {
            kind: ManualTransitionKind::AlphaFade,
        })
    );
    for control in [
        ManualTransitionControl::Position(ManualTransitionPosition::START),
        ManualTransitionControl::Commit,
        ManualTransitionControl::Cancel,
    ] {
        assert_eq!(
            model.control_state(control, &ConnectionState::Ready, Some(&session)),
            TransitionControlState::Disabled
        );
        assert_eq!(
            model.command_payload(control, &ConnectionState::Ready, Some(&session)),
            None
        );
    }
}

#[test]
fn active_model_emits_exact_boundary_positions_commit_and_cancel() {
    let model = ManualTransitionModel::new(
        Some(active(ManualTransitionKind::Wipe, 1, 2, 8_000, 2_500)),
        Some(active(ManualTransitionKind::Wipe, 1, 2, 8_000, 2_500)),
    );
    let session = ready_session();

    for position in [
        ManualTransitionPosition::START,
        ManualTransitionPosition::new(2_500).unwrap(),
        ManualTransitionPosition::END,
    ] {
        assert_eq!(
            model.command_payload(
                ManualTransitionControl::Position(position),
                &ConnectionState::Ready,
                Some(&session),
            ),
            Some(CommandPayload::SetManualTransitionPosition { position })
        );
    }
    assert_eq!(
        model.command_payload(
            ManualTransitionControl::Commit,
            &ConnectionState::Ready,
            Some(&session),
        ),
        Some(CommandPayload::CommitManualTransition)
    );
    assert_eq!(
        model.command_payload(
            ManualTransitionControl::Cancel,
            &ConnectionState::Ready,
            Some(&session),
        ),
        Some(CommandPayload::CancelManualTransition)
    );
    for control in [
        ManualTransitionControl::StartFade,
        ManualTransitionControl::StartWipe,
        ManualTransitionControl::StartAlphaFade,
    ] {
        assert_eq!(
            model.command_payload(control, &ConnectionState::Ready, Some(&session)),
            None
        );
    }
    assert_eq!(ManualTransitionPosition::new(10_001), None);
}

#[test]
fn desired_start_lag_allows_active_commands_without_inventing_realized_state() {
    let model = ManualTransitionModel::new(
        Some(active(ManualTransitionKind::Fade, 1, 2, 0, 6_250)),
        Some(ManualTransitionStatus::Inactive),
    );
    let session = ready_session();

    assert_eq!(
        model.control_state(
            ManualTransitionControl::Position(ManualTransitionPosition::new(4_000).unwrap()),
            &ConnectionState::Ready,
            Some(&session),
        ),
        TransitionControlState::Enabled
    );
    assert_eq!(
        model.control_state(
            ManualTransitionControl::Commit,
            &ConnectionState::Ready,
            Some(&session),
        ),
        TransitionControlState::Enabled
    );
    assert!(matches!(
        model.desired(),
        ManualTransitionProjection::Active(_)
    ));
    assert_eq!(
        model.realized(),
        ManualTransitionProjection::Inactive,
        "realized presentation must not borrow desired or widget state"
    );
}

#[test]
fn terminal_lag_waits_for_realized_inactive_before_enabling_start() {
    let lagging = ManualTransitionModel::new(
        Some(ManualTransitionStatus::Inactive),
        Some(active(ManualTransitionKind::Fade, 1, 2, 6_250, 6_250)),
    );
    let settled = ManualTransitionModel::new(
        Some(ManualTransitionStatus::Inactive),
        Some(ManualTransitionStatus::Inactive),
    );
    let session = ready_session();

    for control in [
        ManualTransitionControl::StartFade,
        ManualTransitionControl::StartWipe,
        ManualTransitionControl::StartAlphaFade,
        ManualTransitionControl::Position(ManualTransitionPosition::START),
        ManualTransitionControl::Commit,
        ManualTransitionControl::Cancel,
    ] {
        assert_eq!(
            lagging.control_state(control, &ConnectionState::Ready, Some(&session)),
            TransitionControlState::Disabled
        );
    }
    assert_eq!(
        settled.control_state(
            ManualTransitionControl::StartFade,
            &ConnectionState::Ready,
            Some(&session),
        ),
        TransitionControlState::Enabled
    );
}

#[test]
fn protocol_1_6_hides_manual_alpha_fade_without_hiding_existing_manual_controls() {
    let model = ManualTransitionModel::new(
        Some(ManualTransitionStatus::Inactive),
        Some(ManualTransitionStatus::Inactive),
    );
    let session = session(ALPHA_FADE_PROTOCOL_VERSION, &["transition"]);

    assert_eq!(
        model.control_state(
            ManualTransitionControl::StartAlphaFade,
            &ConnectionState::Ready,
            Some(&session),
        ),
        TransitionControlState::Hidden
    );
    assert_eq!(
        model.command_payload(
            ManualTransitionControl::StartAlphaFade,
            &ConnectionState::Ready,
            Some(&session),
        ),
        None
    );
    assert_eq!(
        model.control_state(
            ManualTransitionControl::StartFade,
            &ConnectionState::Ready,
            Some(&session),
        ),
        TransitionControlState::Enabled
    );
    assert_eq!(
        model.command_payload(
            ManualTransitionControl::StartWipe,
            &ConnectionState::Ready,
            Some(&session),
        ),
        Some(CommandPayload::StartManualTransition {
            kind: ManualTransitionKind::Wipe,
        })
    );
}

#[test]
fn desired_and_realized_present_routing_intervals_positions_and_motion_separately() {
    let model = ManualTransitionModel::new(
        Some(active(ManualTransitionKind::Wipe, 1, 2, 8_000, 2_500)),
        Some(active(ManualTransitionKind::Wipe, 1, 2, 2_500, 2_500)),
    );

    let ManualTransitionProjection::Active(desired) = model.desired() else {
        panic!("expected desired active presentation");
    };
    assert_eq!(desired.kind, ManualTransitionKind::Wipe);
    assert_eq!((desired.from, desired.to), (input(1), input(2)));
    assert_eq!(desired.interval_start.basis_points(), 8_000);
    assert_eq!(desired.position.basis_points(), 2_500);
    assert_eq!(desired.motion(), ManualTransitionMotion::Reverse);

    let ManualTransitionProjection::Active(realized) = model.realized() else {
        panic!("expected realized active presentation");
    };
    assert_eq!(realized.interval_start.basis_points(), 2_500);
    assert_eq!(realized.position.basis_points(), 2_500);
    assert_eq!(realized.motion(), ManualTransitionMotion::Held);

    let forward = ManualTransitionModel::new(
        Some(active(ManualTransitionKind::Fade, 2, 3, 1_000, 9_000)),
        Some(active(ManualTransitionKind::Fade, 2, 3, 1_000, 9_000)),
    );
    let ManualTransitionProjection::Active(forward) = forward.desired() else {
        panic!("expected active presentation");
    };
    assert_eq!(forward.motion(), ManualTransitionMotion::Forward);
}

#[test]
fn missing_projection_ready_and_permission_gates_disable_without_hiding() {
    let missing_desired = ManualTransitionModel::new(None, Some(ManualTransitionStatus::Inactive));
    let missing_realized = ManualTransitionModel::new(Some(ManualTransitionStatus::Inactive), None);
    let active = ManualTransitionModel::new(
        Some(active(ManualTransitionKind::Fade, 1, 2, 0, 0)),
        Some(active(ManualTransitionKind::Fade, 1, 2, 0, 0)),
    );
    let operator_session = ready_session();
    let no_permission = session(FADE_TO_BLACK_PROTOCOL_VERSION, &["view_status"]);

    for model in [missing_desired, missing_realized] {
        assert_eq!(
            model.control_state(
                ManualTransitionControl::StartFade,
                &ConnectionState::Ready,
                Some(&operator_session),
            ),
            TransitionControlState::Disabled
        );
    }
    for (state, session) in [
        (ConnectionState::Connecting, &operator_session),
        (ConnectionState::Ready, &no_permission),
    ] {
        assert_eq!(
            active.control_state(ManualTransitionControl::Commit, &state, Some(session)),
            TransitionControlState::Disabled
        );
        assert_eq!(
            active.command_payload(ManualTransitionControl::Commit, &state, Some(session)),
            None
        );
    }
}

#[test]
fn reconnect_and_protocol_downgrade_hide_every_manual_control_and_emit_nothing() {
    let model = ManualTransitionModel::new(
        Some(active(ManualTransitionKind::Wipe, 1, 2, 0, 5_000)),
        Some(active(ManualTransitionKind::Wipe, 1, 2, 0, 5_000)),
    );
    let current = ready_session();
    let downgraded = session(ProtocolVersion::new(1, 3), &["transition"]);
    let controls = [
        ManualTransitionControl::StartFade,
        ManualTransitionControl::StartWipe,
        ManualTransitionControl::StartAlphaFade,
        ManualTransitionControl::Position(ManualTransitionPosition::END),
        ManualTransitionControl::Commit,
        ManualTransitionControl::Cancel,
    ];

    for control in controls {
        assert_eq!(
            model.control_state(
                control,
                &ConnectionState::Backoff(ReconnectBackoff {
                    attempt: 1,
                    delay_ms: 250,
                }),
                None,
            ),
            TransitionControlState::Hidden
        );
        assert_eq!(
            model.command_payload(
                control,
                &ConnectionState::Backoff(ReconnectBackoff {
                    attempt: 1,
                    delay_ms: 250,
                }),
                None,
            ),
            None
        );
        assert_eq!(
            model.control_state(control, &ConnectionState::Ready, Some(&downgraded)),
            TransitionControlState::Hidden
        );
        assert_eq!(
            model.command_payload(control, &ConnectionState::Ready, Some(&downgraded)),
            None
        );
    }

    assert_eq!(
        model.control_state(
            ManualTransitionControl::Commit,
            &ConnectionState::Ready,
            Some(&current),
        ),
        TransitionControlState::Enabled
    );
}

#[test]
fn replicated_switcher_conversion_preserves_both_authoritative_projections() {
    let switcher = SwitcherState {
        desired: BusSelection::new(input(1), input(2)),
        realized: BusSelection::new(input(1), input(2)),
        desired_manual_transition: active(ManualTransitionKind::AlphaFade, 1, 2, 8_000, 2_500),
        realized_manual_transition: active(ManualTransitionKind::AlphaFade, 1, 2, 2_500, 2_500),
        desired_fade_to_black: fm_protocol::FadeToBlackState {
            target_active: false,
            position: fm_protocol::FadeToBlackPosition::LIVE,
        },
        realized_fade_to_black: fm_protocol::FadeToBlackState {
            target_active: false,
            position: fm_protocol::FadeToBlackPosition::LIVE,
        },
        runtime_generation: Some(9),
    };

    let model = ManualTransitionModel::from_switcher(Some(&switcher));
    assert!(matches!(
        model.desired(),
        ManualTransitionProjection::Active(state)
            if state.kind == ManualTransitionKind::AlphaFade
                && state.interval_start.basis_points() == 8_000
                && state.position.basis_points() == 2_500
    ));
    assert!(matches!(
        model.realized(),
        ManualTransitionProjection::Active(state)
            if state.interval_start.basis_points() == 2_500
                && state.position.basis_points() == 2_500
    ));
    assert_eq!(
        ManualTransitionModel::from_switcher(None),
        ManualTransitionModel::default()
    );
}
