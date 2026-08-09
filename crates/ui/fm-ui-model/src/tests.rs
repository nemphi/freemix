use core::num::NonZeroU128;

use fm_protocol::{CommandResult, EngineIdentity, FadeToBlackPosition};

use super::*;

fn project(value: u128) -> ProjectId {
    ProjectId::new(NonZeroU128::new(value).unwrap())
}

fn input(value: u128) -> InputId {
    InputId::new(NonZeroU128::new(value).unwrap())
}

fn engine(id: &str, epoch: u64, log: &str) -> EngineIdentity {
    EngineIdentity {
        engine_id: id.into(),
        state_epoch: epoch,
        log_id: log.into(),
    }
}

fn overlays() -> Vec<OverlayStatus> {
    (1..=8)
        .map(|channel| OverlayStatus {
            channel,
            source: None,
            active: false,
            opacity: 0,
            transition: fm_protocol::OverlayTransitionKind::Cut,
            duration_frames: 1,
            position: fm_protocol::OverlayPositionPreset::FullFrame,
            border: fm_protocol::OverlayBorderPreset::None,
            queued_sources: Vec::new(),
            included_outputs: Vec::new(),
        })
        .collect()
}

fn input_audio_strips() -> Vec<InputAudioStripStatus> {
    [input(1), input(2), input(3)]
        .into_iter()
        .map(|input| InputAudioStripStatus {
            input,
            gain_millidb: 0,
            muted: false,
            follow_video: true,
            delay_samples: 0,
        })
        .collect()
}

fn snapshot(project_id: ProjectId, revision: u64) -> ProjectSnapshot {
    ProjectSnapshot {
        cursor: ProjectCursor {
            project_id,
            engine: engine("engine-a", 1, "log-a"),
            revision: Revision::new(revision),
        },
        show_name: "Show".into(),
        inputs: vec![input(1), input(2), input(3)],
        input_audio_strips: input_audio_strips(),
        stingers: Vec::new(),
        desired_overlays: overlays(),
        realized_overlays: overlays(),
        switcher: SwitcherState {
            desired: BusSelection::new(input(1), input(2)),
            realized: BusSelection::new(input(1), input(2)),
            desired_manual_transition: ManualTransitionStatus::Inactive,
            realized_manual_transition: ManualTransitionStatus::Inactive,
            desired_fade_to_black: FadeToBlackState {
                target_active: false,
                position: FadeToBlackPosition::LIVE,
            },
            realized_fade_to_black: FadeToBlackState {
                target_active: false,
                position: FadeToBlackPosition::LIVE,
            },
            runtime_generation: Some(4),
        },
    }
}

fn event(
    project_id: ProjectId,
    engine: EngineIdentity,
    revision: u64,
    change: DurableChange,
) -> DurableProjectEvent {
    DurableProjectEvent {
        cursor: ProjectCursor {
            project_id,
            engine,
            revision: Revision::new(revision),
        },
        change,
    }
}

fn realization(
    project_id: ProjectId,
    engine: EngineIdentity,
    revision: u64,
    generation: u64,
    sequence: u64,
) -> RuntimeRealization {
    RuntimeRealization {
        project_id,
        engine,
        revision: Revision::new(revision),
        generation,
        sequence,
        manual_transition: ManualTransitionStatus::Inactive,
        fade_to_black: fade_to_black(false, 0),
    }
}

fn desired(selection: BusSelection) -> DurableChange {
    DurableChange::DesiredSwitcher {
        selection,
        manual_transition: ManualTransitionStatus::Inactive,
        fade_to_black: FadeToBlackState {
            target_active: false,
            position: FadeToBlackPosition::LIVE,
        },
        overlays: overlays(),
        input_audio_strips: input_audio_strips(),
    }
}

fn fade_to_black(target_active: bool, numerator: u16) -> FadeToBlackState {
    FadeToBlackState {
        target_active,
        position: FadeToBlackPosition::new(numerator),
    }
}

#[test]
fn installs_and_validates_snapshot() {
    let project_id = project(10);
    let mut model = ClientModel::new(project_id);
    model.install_snapshot(snapshot(project_id, 7)).unwrap();

    let state = model.state().unwrap();
    assert_eq!(state.show_name(), "Show");
    assert_eq!(state.switcher().desired.program, input(1));
    assert_eq!(model.sync_status(), &SyncStatus::Current);

    let error = model
        .install_snapshot(snapshot(project(11), 8))
        .unwrap_err();
    assert!(matches!(error, ModelError::ProjectMismatch { .. }));
    assert_eq!(model.reconnect_cursor().unwrap().revision, Revision::new(7));

    let error = model.install_snapshot(snapshot(project_id, 6)).unwrap_err();
    assert_eq!(
        error,
        ModelError::OutOfOrder {
            current: Revision::new(7),
            observed: Revision::new(6),
        }
    );
    assert_eq!(model.reconnect_cursor().unwrap().revision, Revision::new(7));
}

#[test]
fn reduces_exact_desired_and_realized_fade_to_black_state() {
    let project_id = project(10);
    let mut initial = snapshot(project_id, 4);
    initial.switcher.desired_fade_to_black = fade_to_black(true, 40_000);
    initial.switcher.realized_fade_to_black = fade_to_black(true, 20_000);

    let mut model = ClientModel::new(project_id);
    model.install_snapshot(initial).unwrap();
    let identity = model.reconnect_cursor().unwrap().engine.clone();
    assert_eq!(
        model.state().unwrap().switcher().desired_fade_to_black,
        fade_to_black(true, 40_000)
    );
    assert_eq!(
        model.state().unwrap().switcher().realized_fade_to_black,
        fade_to_black(true, 20_000)
    );

    model
        .apply_event(event(
            project_id,
            identity.clone(),
            5,
            DurableChange::DesiredSwitcher {
                selection: BusSelection::new(input(1), input(2)),
                manual_transition: ManualTransitionStatus::Inactive,
                fade_to_black: fade_to_black(false, 12_345),
                overlays: overlays(),
                input_audio_strips: input_audio_strips(),
            },
        ))
        .unwrap();
    assert_eq!(
        model.state().unwrap().switcher().desired_fade_to_black,
        fade_to_black(false, 12_345)
    );
    assert_eq!(
        model.state().unwrap().switcher().realized_fade_to_black,
        fade_to_black(true, 20_000)
    );

    let mut realized = realization(project_id, identity, 5, 9, 1);
    realized.fade_to_black = fade_to_black(false, 12_345);
    model.apply_runtime_realization(realized).unwrap();
    assert_eq!(
        model.state().unwrap().switcher().realized_fade_to_black,
        fade_to_black(false, 12_345)
    );
}

#[test]
fn applies_contiguous_events_and_tracks_desired_separately_from_realized() {
    let project_id = project(10);
    let mut model = ClientModel::new(project_id);
    model.install_snapshot(snapshot(project_id, 7)).unwrap();
    let identity = model.reconnect_cursor().unwrap().engine.clone();

    model
        .apply_event(event(
            project_id,
            identity.clone(),
            8,
            desired(BusSelection::new(input(2), input(3))),
        ))
        .unwrap();
    let after_desired = model.state().unwrap().switcher();
    assert_eq!(after_desired.desired, BusSelection::new(input(2), input(3)));
    assert_eq!(
        after_desired.realized,
        BusSelection::new(input(1), input(2))
    );

    model
        .apply_runtime_realization(realization(project_id, identity, 8, 5, 1))
        .unwrap();
    let realized = model.state().unwrap().switcher();
    assert_eq!(realized.realized, BusSelection::new(input(2), input(3)));
    assert_eq!(realized.runtime_generation, Some(5));
}

#[test]
fn ignores_exact_duplicates_but_rejects_conflicting_ones() {
    let project_id = project(10);
    let mut model = ClientModel::new(project_id);
    model.install_snapshot(snapshot(project_id, 1)).unwrap();
    let identity = model.reconnect_cursor().unwrap().engine.clone();
    let first = event(
        project_id,
        identity.clone(),
        2,
        desired(BusSelection::new(input(2), input(1))),
    );

    assert!(matches!(
        model.apply_event(first.clone()).unwrap(),
        EventApplied::Applied { .. }
    ));
    assert_eq!(model.apply_event(first), Ok(EventApplied::Duplicate));

    let conflict = event(
        project_id,
        identity,
        2,
        desired(BusSelection::new(input(3), input(1))),
    );
    assert!(matches!(
        model.apply_event(conflict),
        Err(ModelError::ConflictingDuplicate { .. })
    ));
    assert_eq!(model.sync_status(), &SyncStatus::RequiresSnapshot);
}

#[test]
fn rejects_gaps_without_advancing_and_recovers_when_missing_event_arrives() {
    let project_id = project(10);
    let mut model = ClientModel::new(project_id);
    model.install_snapshot(snapshot(project_id, 4)).unwrap();
    let identity = model.reconnect_cursor().unwrap().engine.clone();
    let change = desired(BusSelection::new(input(2), input(1)));

    assert_eq!(
        model.apply_event(event(project_id, identity.clone(), 6, change.clone())),
        Err(ModelError::RevisionGap {
            expected: Revision::new(5),
            observed: Revision::new(6),
        })
    );
    assert_eq!(model.reconnect_cursor().unwrap().revision, Revision::new(4));
    assert!(model.is_stale());

    model
        .apply_event(event(project_id, identity, 5, change))
        .unwrap();
    assert_eq!(model.reconnect_cursor().unwrap().revision, Revision::new(5));
    assert!(model.is_stale());
}

#[test]
fn rejects_project_and_engine_identity_changes() {
    let project_id = project(10);
    let mut model = ClientModel::new(project_id);
    model.install_snapshot(snapshot(project_id, 4)).unwrap();
    let change = desired(BusSelection::new(input(2), input(1)));

    assert!(matches!(
        model.apply_event(event(
            project(11),
            engine("engine-a", 1, "log-a"),
            5,
            change.clone(),
        )),
        Err(ModelError::ProjectMismatch { .. })
    ));
    assert!(matches!(
        model.apply_event(event(project_id, engine("engine-a", 2, "log-b"), 5, change,)),
        Err(ModelError::EngineMismatch { .. })
    ));
    assert_eq!(model.sync_status(), &SyncStatus::RequiresSnapshot);
    assert_eq!(model.reconnect_cursor().unwrap().revision, Revision::new(4));
}

#[test]
fn optimistic_accept_remains_until_accepted_revision_is_applied() {
    let project_id = project(10);
    let mut model = ClientModel::new(project_id);
    model.install_snapshot(snapshot(project_id, 1)).unwrap();
    model
        .track_command(
            CommandId::new("select-3"),
            Some(OptimisticChange::DesiredPreview(input(3))),
        )
        .unwrap();
    assert_eq!(model.view().unwrap().switcher.desired.preview, input(3));
    assert_eq!(model.state().unwrap().switcher().desired.preview, input(2));

    let result = CommandResult::Accepted {
        id: "select-3".into(),
        revision: 2,
        scheduled_frame: None,
    };
    assert_eq!(
        model.reconcile_command(&result).unwrap(),
        CommandReconciled::Accepted {
            revision: Revision::new(2),
            awaiting_event: true,
        }
    );
    assert_eq!(model.pending_commands().len(), 1);

    let identity = model.reconnect_cursor().unwrap().engine.clone();
    let applied = model
        .apply_event(event(
            project_id,
            identity,
            2,
            desired(BusSelection::new(input(1), input(3))),
        ))
        .unwrap();
    assert_eq!(
        applied,
        EventApplied::Applied {
            reconciled_commands: vec![CommandId::new("select-3")],
        }
    );
    assert!(model.pending_commands().is_empty());
    assert_eq!(model.view().unwrap().switcher.desired.preview, input(3));
}

#[test]
fn optimistic_rejection_snaps_to_authoritative_state_and_retains_reason() {
    let project_id = project(10);
    let mut model = ClientModel::new(project_id);
    model.install_snapshot(snapshot(project_id, 1)).unwrap();
    model
        .track_command(
            CommandId::new("select-3"),
            Some(OptimisticChange::DesiredPreview(input(3))),
        )
        .unwrap();

    let result = CommandResult::Rejected {
        id: "select-3".into(),
        code: "permission_denied".into(),
        message: "operator role required".into(),
        fields: Vec::new(),
        current_revision: 1,
        retryable: false,
    };
    assert!(matches!(
        model.reconcile_command(&result).unwrap(),
        CommandReconciled::Rejected(_)
    ));
    assert!(model.pending_commands().is_empty());
    assert_eq!(model.view().unwrap().switcher.desired.preview, input(2));
    assert_eq!(model.last_rejection().unwrap().code, "permission_denied");
}

#[test]
fn reconnect_cursor_advances_only_for_contiguous_authoritative_events() {
    let project_id = project(10);
    let mut model = ClientModel::new(project_id);
    model.install_snapshot(snapshot(project_id, 20)).unwrap();
    let initial = model.reconnect_cursor().unwrap().clone();
    assert_eq!(initial.protocol_cursor().revision, 20);

    model
        .track_command(CommandId::new("local-only"), None)
        .unwrap();
    assert_eq!(model.reconnect_cursor(), Some(&initial));

    model
        .apply_event(event(
            project_id,
            initial.engine.clone(),
            21,
            desired(BusSelection::new(input(2), input(3))),
        ))
        .unwrap();
    let reconnect = model.reconnect_cursor().unwrap();
    assert_eq!(reconnect.project_id, project_id);
    assert_eq!(reconnect.revision, Revision::new(21));
    assert_eq!(reconnect.engine, initial.engine);
    let protocol = model.protocol_reconnect_cursor().unwrap();
    assert_eq!(protocol.server.project_id, project_id.to_string());
    assert_eq!(protocol.revision, 21);
}

#[test]
fn runtime_realization_leaves_durable_and_command_tracking_unchanged() {
    let project_id = project(10);
    let mut model = ClientModel::new(project_id);
    model.install_snapshot(snapshot(project_id, 7)).unwrap();
    let identity = model.reconnect_cursor().unwrap().engine.clone();
    model
        .track_command(CommandId::new("pending"), None)
        .unwrap();
    model
        .reconcile_command(&CommandResult::Accepted {
            id: "pending".into(),
            revision: 8,
            scheduled_frame: None,
        })
        .unwrap();
    let cursor = model.reconnect_cursor().unwrap().clone();
    let sync_status = model.sync_status().clone();
    let pending = model.pending_commands().to_vec();

    assert_eq!(
        model
            .reduce(Action::ApplyRuntimeRealization(realization(
                project_id, identity, 7, 5, 1,
            )))
            .unwrap(),
        Reduction::RuntimeRealizationApplied(RuntimeRealizationApplied::Applied)
    );

    assert_eq!(model.reconnect_cursor(), Some(&cursor));
    assert_eq!(model.sync_status(), &sync_status);
    assert_eq!(model.pending_commands(), pending);
}

#[test]
fn runtime_realization_can_use_current_or_retained_desired_revision() {
    let project_id = project(10);
    let mut model = ClientModel::new(project_id);
    model.install_snapshot(snapshot(project_id, 7)).unwrap();
    let identity = model.reconnect_cursor().unwrap().engine.clone();
    let retained = BusSelection::new(input(2), input(3));
    let current = BusSelection::new(input(3), input(1));
    model
        .apply_event(event(project_id, identity.clone(), 8, desired(retained)))
        .unwrap();
    model
        .apply_event(event(project_id, identity.clone(), 9, desired(current)))
        .unwrap();

    model
        .apply_runtime_realization(realization(project_id, identity.clone(), 9, 5, 1))
        .unwrap();
    assert_eq!(model.state().unwrap().switcher().realized, current);

    model
        .apply_runtime_realization(realization(project_id, identity, 8, 6, 2))
        .unwrap();
    let switcher = model.state().unwrap().switcher();
    assert_eq!(switcher.realized, retained);
    assert_eq!(switcher.runtime_generation, Some(6));
    assert_eq!(model.reconnect_cursor().unwrap().revision, Revision::new(9));
}

#[test]
fn runtime_realization_ordering_is_scoped_to_generation() {
    let project_id = project(10);
    let mut model = ClientModel::new(project_id);
    model.install_snapshot(snapshot(project_id, 7)).unwrap();
    let identity = model.reconnect_cursor().unwrap().engine.clone();
    model
        .apply_event(event(
            project_id,
            identity.clone(),
            8,
            desired(BusSelection::new(input(2), input(3))),
        ))
        .unwrap();

    model
        .apply_runtime_realization(realization(project_id, identity.clone(), 7, 1, 1))
        .unwrap();
    model
        .apply_runtime_realization(realization(project_id, identity.clone(), 8, 2, 1))
        .unwrap();
    assert_eq!(
        model.state().unwrap().switcher().runtime_generation,
        Some(2)
    );

    assert_eq!(
        model.apply_runtime_realization(realization(project_id, identity.clone(), 7, 1, 2,)),
        Err(ModelError::RuntimeGenerationOutOfOrder {
            current_generation: 2,
            observed_generation: 1,
        })
    );
    assert_eq!(
        model.apply_runtime_realization(realization(project_id, identity.clone(), 7, 2, 0,)),
        Err(ModelError::RuntimeOutOfOrder {
            current_sequence: 1,
            observed_sequence: 0,
        })
    );
    assert_eq!(
        model.apply_runtime_realization(realization(project_id, identity, 7, 2, 1)),
        Err(ModelError::ConflictingRuntimeSequence { sequence: 1 })
    );
}

#[test]
fn runtime_realization_rejects_unknown_revision_and_identity() {
    let project_id = project(10);
    let mut model = ClientModel::new(project_id);
    model.install_snapshot(snapshot(project_id, 7)).unwrap();
    let identity = model.reconnect_cursor().unwrap().engine.clone();

    assert_eq!(
        model.apply_runtime_realization(realization(project_id, identity.clone(), 8, 5, 1,)),
        Err(ModelError::UnknownDurableRevision {
            revision: Revision::new(8),
        })
    );
    assert!(matches!(
        model.apply_runtime_realization(realization(project(11), identity, 7, 5, 1,)),
        Err(ModelError::ProjectMismatch { .. })
    ));
    assert!(matches!(
        model.apply_runtime_realization(realization(
            project_id,
            engine("engine-b", 1, "log-b"),
            7,
            5,
            1,
        )),
        Err(ModelError::EngineMismatch { .. })
    ));
    assert_eq!(model.sync_status(), &SyncStatus::Current);
    assert_eq!(model.reconnect_cursor().unwrap().revision, Revision::new(7));
}

#[test]
fn runtime_realization_rejects_invalid_manual_endpoints_transactionally() {
    let project_id = project(10);
    let mut model = ClientModel::new(project_id);
    model.install_snapshot(snapshot(project_id, 7)).unwrap();
    let identity = model.reconnect_cursor().unwrap().engine.clone();
    let before = model.state().unwrap().clone();

    let mut unknown = realization(project_id, identity.clone(), 7, 5, 1);
    unknown.manual_transition = ManualTransitionStatus::Active(ActiveManualTransition {
        kind: ManualTransitionKind::Fade,
        from: input(1),
        to: input(99),
        interval_start: ManualTransitionPosition::START,
        position: ManualTransitionPosition::new(2_500).unwrap(),
    });
    assert_eq!(
        model.apply_runtime_realization(unknown),
        Err(ModelError::UnknownInput(input(99)))
    );
    assert_eq!(model.state(), Some(&before));

    let mut mismatched = realization(project_id, identity.clone(), 7, 5, 1);
    mismatched.manual_transition = ManualTransitionStatus::Active(ActiveManualTransition {
        kind: ManualTransitionKind::Fade,
        from: input(1),
        to: input(3),
        interval_start: ManualTransitionPosition::START,
        position: ManualTransitionPosition::new(2_500).unwrap(),
    });
    assert_eq!(
        model.apply_runtime_realization(mismatched),
        Err(ModelError::InvalidManualTransitionRouting)
    );
    assert_eq!(model.state(), Some(&before));

    assert_eq!(
        model.apply_runtime_realization(realization(project_id, identity, 7, 5, 1)),
        Ok(RuntimeRealizationApplied::Applied)
    );
}

#[test]
fn runtime_revision_errors_are_separate_from_durable_gaps() {
    let project_id = project(10);
    let mut model = ClientModel::new(project_id);
    model.install_snapshot(snapshot(project_id, 4)).unwrap();
    let identity = model.reconnect_cursor().unwrap().engine.clone();

    assert!(matches!(
        model.apply_runtime_realization(realization(project_id, identity.clone(), 6, 5, 1,)),
        Err(ModelError::UnknownDurableRevision { .. })
    ));
    assert_eq!(model.sync_status(), &SyncStatus::Current);

    assert!(matches!(
        model.apply_event(event(
            project_id,
            identity,
            6,
            desired(BusSelection::new(input(2), input(1))),
        )),
        Err(ModelError::RevisionGap { .. })
    ));
    assert!(matches!(model.sync_status(), SyncStatus::Behind { .. }));
}
