use core::num::NonZeroU128;

use fm_clock::{ClockDomainId, ClockTime};
use fm_command::{
    CommandEnvelope, EventSequence, IdempotencyKey, RejectionCode, Revision, RuntimeGeneration,
    StateEpoch,
};
use fm_engine::{
    Engine, EngineCommand, EngineError, EngineManualTransitionKind, EngineManualTransitionPosition,
    EnginePrepareOutcome, EngineRestoreState, EngineSnapshot, ShowState, SnapshotError,
};
use fm_scheduler::FrameNumber;
use fm_switcher::{
    FadeToBlackPosition, FadeToBlackTarget, MissingMediaFallback, StingerAudioPolicy,
    StingerDescriptor, StingerSlotId, SwitcherEvent, SwitcherState, TBarPosition, TBarState,
    TransitionKind,
};
use fm_types::{FrameRate, InputId};

fn input(value: u128) -> InputId {
    InputId::new(NonZeroU128::new(value).unwrap())
}

fn domain() -> ClockDomainId {
    ClockDomainId::new(NonZeroU128::new(100).unwrap())
}

fn engine() -> Engine {
    Engine::new(
        ShowState::new(
            "test show",
            vec![input(1), input(2), input(3)],
            input(1),
            input(2),
        )
        .unwrap(),
        FrameRate::new(25, 1).unwrap(),
        domain(),
    )
}

fn stinger_engine(
    media_available: bool,
    cut_point_frames: u32,
    fallback: MissingMediaFallback,
) -> Engine {
    let mut show = ShowState::new(
        "stinger show",
        vec![input(1), input(2), input(3)],
        input(1),
        input(2),
    )
    .unwrap();
    let slot = StingerSlotId::new(1).unwrap();
    show.configure_stinger(
        slot,
        StingerDescriptor::new(
            input(3),
            true,
            cut_point_frames,
            StingerAudioPolicy::MixWithProgram,
            fallback,
        ),
    )
    .unwrap();
    show.preload_stinger(slot, media_available).unwrap();
    Engine::new(show, FrameRate::new(25, 1).unwrap(), domain())
}

fn envelope(key: &str, command: EngineCommand) -> CommandEnvelope<EngineCommand> {
    CommandEnvelope::new(format!("command-{key}"), IdempotencyKey::new(key), command)
}

fn restore_state(snapshot: &EngineSnapshot) -> EngineRestoreState {
    EngineRestoreState {
        state_epoch: snapshot.state_epoch(),
        revision: snapshot.revision(),
        event_sequence: snapshot.event_sequence(),
        runtime_generation: snapshot.runtime_generation(),
        clock_time: snapshot.clock_time(),
        frame_cursor: FrameNumber::new(snapshot.frames_rendered()),
        receipts: snapshot.receipts().to_vec(),
    }
}

fn restore_persisted(
    snapshot: &EngineSnapshot,
    realized_switcher: SwitcherState,
    restore_state: EngineRestoreState,
) -> Result<Engine, SnapshotError> {
    Engine::restore_persisted(
        snapshot.show().clone(),
        realized_switcher,
        snapshot.frame_rate(),
        domain(),
        restore_state,
    )
}

#[test]
fn stale_revision_is_rejected_without_mutating_desired_state() {
    let mut engine = engine();
    let first = engine
        .execute(envelope("cut", EngineCommand::Cut), 0)
        .unwrap();
    assert_eq!(first.receipt.accepted().unwrap().revision, Revision::new(1));
    assert_eq!(engine.show().desired_switcher().program(), input(2));

    let stale = engine
        .execute(
            envelope("stale", EngineCommand::SelectPreview(input(3))).expecting(Revision::new(0)),
            0,
        )
        .unwrap();
    let rejection = stale.receipt.rejected().unwrap();
    assert_eq!(rejection.rejection.code, RejectionCode::RevisionConflict);
    assert_eq!(rejection.current_revision, Revision::new(1));
    assert_eq!(engine.show().desired_switcher().preview(), input(1));
}

#[test]
fn fade_duration_is_bounded_before_projection_or_scheduling() {
    let mut engine = engine();
    let before = engine.snapshot().unwrap();
    let outcome = engine
        .execute(
            envelope(
                "oversized-fade",
                EngineCommand::Fade {
                    duration_frames: 3_601,
                },
            ),
            0,
        )
        .unwrap();

    let rejection = outcome.receipt.rejected().unwrap();
    assert_eq!(rejection.rejection.code, RejectionCode::InvalidCommand);
    assert_eq!(
        rejection.rejection.message,
        "fade duration must not exceed 3600 frames"
    );
    assert_eq!(engine.snapshot().unwrap().frames_rendered(), 0);
    assert_eq!(
        engine.show().desired_switcher(),
        before.show().desired_switcher()
    );
    assert_eq!(engine.realized_switcher(), before.realized_switcher());
}

#[test]
fn wipe_duration_is_bounded_before_projection_or_scheduling() {
    for (key, duration, message) in [
        ("zero-wipe", 0, "wipe duration must be nonzero"),
        (
            "oversized-wipe",
            3_601,
            "wipe duration must not exceed 3600 frames",
        ),
    ] {
        let mut engine = engine();
        let outcome = engine
            .execute(
                envelope(
                    key,
                    EngineCommand::Wipe {
                        duration_frames: duration,
                    },
                ),
                0,
            )
            .unwrap();
        let rejection = outcome.receipt.rejected().unwrap();
        assert_eq!(rejection.rejection.code, RejectionCode::InvalidCommand);
        assert_eq!(rejection.rejection.message, message);
        assert_eq!(engine.revision(), Revision::new(0));
        assert!(engine.snapshot().is_ok());
    }
}

#[test]
fn alpha_fade_duration_is_bounded_before_projection_or_scheduling() {
    for (key, duration, message) in [
        ("zero-alpha-fade", 0, "alpha fade duration must be nonzero"),
        (
            "oversized-alpha-fade",
            3_601,
            "alpha fade duration must not exceed 3600 frames",
        ),
    ] {
        let mut engine = engine();
        let outcome = engine
            .execute(
                envelope(
                    key,
                    EngineCommand::AlphaFade {
                        duration_frames: duration,
                    },
                ),
                0,
            )
            .unwrap();
        let rejection = outcome.receipt.rejected().unwrap();
        assert_eq!(rejection.rejection.code, RejectionCode::InvalidCommand);
        assert_eq!(rejection.rejection.message, message);
        assert_eq!(engine.revision(), Revision::new(0));
        assert!(engine.snapshot().is_ok());
    }
}

#[test]
fn slide_duration_is_bounded_before_projection_or_scheduling() {
    for (key, duration, message) in [
        ("zero-slide", 0, "slide duration must be nonzero"),
        (
            "oversized-slide",
            3_601,
            "slide duration must not exceed 3600 frames",
        ),
    ] {
        let mut engine = engine();
        let outcome = engine
            .execute(
                envelope(
                    key,
                    EngineCommand::Slide {
                        duration_frames: duration,
                    },
                ),
                0,
            )
            .unwrap();
        let rejection = outcome.receipt.rejected().unwrap();
        assert_eq!(rejection.rejection.code, RejectionCode::InvalidCommand);
        assert_eq!(rejection.rejection.message, message);
        assert_eq!(engine.revision(), Revision::new(0));
        assert!(engine.snapshot().is_ok());
    }
}

#[test]
fn zoom_duration_is_bounded_before_projection_or_scheduling() {
    for (key, duration, message) in [
        ("zero-zoom", 0, "zoom duration must be nonzero"),
        (
            "oversized-zoom",
            3_601,
            "zoom duration must not exceed 3600 frames",
        ),
    ] {
        let mut engine = engine();
        let outcome = engine
            .execute(
                envelope(
                    key,
                    EngineCommand::Zoom {
                        duration_frames: duration,
                    },
                ),
                0,
            )
            .unwrap();
        let rejection = outcome.receipt.rejected().unwrap();
        assert_eq!(rejection.rejection.code, RejectionCode::InvalidCommand);
        assert_eq!(rejection.rejection.message, message);
        assert_eq!(engine.revision(), Revision::new(0));
        assert!(engine.snapshot().is_ok());
    }
}

#[test]
fn accepted_cut_is_staged_without_changing_the_live_engine() {
    let mut engine = engine();
    let before = engine.snapshot().unwrap();
    let prepared = engine
        .prepare_execute(envelope("prepared-cut", EngineCommand::Cut), 0)
        .unwrap()
        .prepared()
        .unwrap();

    assert!(prepared.outcome().receipt.accepted().is_some());
    assert_eq!(
        prepared.staged_engine().show().desired_switcher().program(),
        input(2)
    );
    assert_eq!(engine.snapshot().unwrap(), before);

    let outcome = engine.commit_execute(prepared).unwrap();
    assert_eq!(
        outcome.receipt.accepted().unwrap().revision,
        Revision::new(1)
    );
    assert_eq!(engine.show().desired_switcher().program(), input(2));
    assert_eq!(engine.realized_switcher().program(), input(1));
}

#[test]
fn rejected_receipt_is_staged_and_becomes_durable_only_on_commit() {
    let mut engine = engine();
    let prepared = engine
        .prepare_execute(
            envelope("prepared-rejection", EngineCommand::Cut).expecting(Revision::new(1)),
            0,
        )
        .unwrap()
        .prepared()
        .unwrap();

    let rejection = prepared.outcome().receipt.rejected().unwrap();
    assert_eq!(rejection.rejection.code, RejectionCode::RevisionConflict);
    assert!(engine.snapshot().unwrap().receipts().is_empty());
    assert_eq!(
        prepared
            .staged_engine()
            .snapshot()
            .unwrap()
            .receipts()
            .len(),
        1
    );

    let rejected = engine.commit_execute(prepared).unwrap();
    let replay = engine
        .prepare_execute(envelope("prepared-rejection", EngineCommand::Cut), 0)
        .unwrap()
        .replayed()
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.receipt, rejected.receipt);
}

#[test]
fn dropping_or_aborting_a_preparation_leaves_the_engine_unchanged() {
    let engine = engine();
    let before = engine.snapshot().unwrap();

    let dropped = engine
        .prepare_execute(envelope("dropped-cut", EngineCommand::Cut), 0)
        .unwrap()
        .prepared()
        .unwrap();
    drop(dropped);
    assert_eq!(engine.snapshot().unwrap(), before);

    engine
        .prepare_execute(envelope("aborted-cut", EngineCommand::Cut), 0)
        .unwrap()
        .prepared()
        .unwrap()
        .abort();
    assert_eq!(engine.snapshot().unwrap(), before);
}

#[test]
fn duplicate_prepare_is_a_replay_without_a_staged_execution() {
    let mut engine = engine();
    let original = engine
        .execute(envelope("prepared-duplicate", EngineCommand::Cut), 0)
        .unwrap();

    let replay = engine
        .prepare_execute(
            envelope("prepared-duplicate", EngineCommand::SelectPreview(input(3))),
            0,
        )
        .unwrap();
    let EnginePrepareOutcome::Replayed(replay) = replay else {
        panic!("duplicate must not create a staged execution");
    };
    assert!(replay.replayed);
    assert_eq!(replay.receipt, original.receipt);

    let boundary = engine.tick().unwrap();
    assert_eq!(boundary.events.len(), 1);
    assert!(engine.tick().unwrap().events.is_empty());
}

#[test]
fn preparation_becomes_stale_after_an_intervening_commit() {
    let mut engine = engine();
    let first = engine
        .prepare_execute(
            envelope("first-preparation", EngineCommand::SelectPreview(input(3))),
            0,
        )
        .unwrap()
        .prepared()
        .unwrap();
    let stale = engine
        .prepare_execute(envelope("stale-preparation", EngineCommand::Cut), 0)
        .unwrap()
        .prepared()
        .unwrap();

    engine.commit_execute(first).unwrap();
    assert_eq!(
        engine.commit_execute(stale),
        Err(EngineError::StalePreparation)
    );
    assert_eq!(engine.revision(), Revision::new(1));
    assert_eq!(engine.show().desired_switcher().preview(), input(3));
}

#[test]
fn cut_projection_exactly_matches_commit_followed_by_ticks() {
    let mut engine = engine();
    let prepared = engine
        .prepare_execute(envelope("projected-cut", EngineCommand::Cut), 0)
        .unwrap()
        .prepared()
        .unwrap();
    let projected = prepared.project(1).unwrap();

    engine.commit_execute(prepared).unwrap();
    engine.tick().unwrap();
    assert_eq!(projected, engine.snapshot().unwrap());
}

#[test]
fn fade_projection_exactly_matches_commit_followed_by_ticks() {
    let mut engine = engine();
    let before = engine.snapshot().unwrap();
    let prepared = engine
        .prepare_execute(
            envelope("projected-fade", EngineCommand::Fade { duration_frames: 3 }),
            0,
        )
        .unwrap()
        .prepared()
        .unwrap();

    assert_eq!(
        prepared.project(2),
        Err(EngineError::Snapshot(SnapshotError::WorkInFlight))
    );
    assert_eq!(engine.snapshot().unwrap(), before);
    let projected = prepared.project(3).unwrap();

    engine.commit_execute(prepared).unwrap();
    for _ in 0..3 {
        engine.tick().unwrap();
    }
    assert_eq!(projected, engine.snapshot().unwrap());
}

#[test]
fn preparing_does_not_schedule_work_on_the_live_engine() {
    let mut engine = engine();
    let prepared = engine
        .prepare_execute(envelope("isolated-scheduler", EngineCommand::Cut), 0)
        .unwrap()
        .prepared()
        .unwrap();

    assert!(engine.snapshot().is_ok());
    prepared.abort();
    let frame = engine.tick().unwrap();
    assert!(frame.events.is_empty());
    assert_eq!(engine.realized_switcher().program(), input(1));
    assert_eq!(engine.runtime_generation(), RuntimeGeneration::default());
}

#[test]
fn duplicate_key_replays_receipt_without_rescheduling() {
    let mut engine = engine();
    let first = engine
        .execute(
            envelope("preview", EngineCommand::SelectPreview(input(3))),
            0,
        )
        .unwrap();
    let duplicate = engine
        .execute(envelope("preview", EngineCommand::Cut), 0)
        .unwrap();

    assert!(!first.replayed);
    assert!(duplicate.replayed);
    assert_eq!(duplicate.receipt, first.receipt);

    let boundary = engine.tick().unwrap();
    assert_eq!(boundary.events.len(), 1);
    assert_eq!(engine.realized_switcher().preview(), input(3));
    let following = engine.tick().unwrap();
    assert!(following.events.is_empty());
    assert_eq!(engine.realized_switcher().program(), input(1));
}

#[test]
fn cut_realizes_only_on_the_next_frame_boundary() {
    let mut engine = engine();
    assert_eq!(engine.next_frame_deadline().unwrap(), ClockTime::ZERO);
    let outcome = engine
        .execute(envelope("cut", EngineCommand::Cut), 0)
        .unwrap();
    assert_eq!(
        outcome
            .receipt
            .accepted()
            .unwrap()
            .result
            .target_frame
            .get(),
        0
    );
    assert_eq!(engine.show().desired_switcher().program(), input(2));
    assert_eq!(engine.realized_switcher().program(), input(1));

    let frame = engine.tick().unwrap();
    assert_eq!(frame.frame.get(), 0);
    assert_eq!(frame.deadline, ClockTime::ZERO);
    assert_eq!(frame.program.primary, input(2));
    assert_eq!(engine.realized_switcher().program(), input(2));
    assert_eq!(frame.runtime_generation, RuntimeGeneration::new(1));
    assert_eq!(
        engine.next_frame_deadline().unwrap(),
        ClockTime::from_nanos(40_000_000)
    );
}

#[test]
fn fade_renders_exactly_the_requested_number_of_frames() {
    let mut engine = engine();
    engine
        .execute(
            envelope("fade", EngineCommand::Fade { duration_frames: 3 }),
            0,
        )
        .unwrap();

    let first = engine.tick().unwrap();
    assert_eq!(first.program.primary, input(1));
    assert_eq!(first.program.secondary, Some(input(2)));
    assert_eq!(
        (first.program.mix_numerator, first.program.mix_denominator),
        (0, 3)
    );

    let second = engine.tick().unwrap();
    assert_eq!(
        (second.program.mix_numerator, second.program.mix_denominator),
        (1, 3)
    );

    let third = engine.tick().unwrap();
    assert_eq!(
        (third.program.mix_numerator, third.program.mix_denominator),
        (2, 3)
    );
    assert!(third.events.iter().any(|event| matches!(
        event,
        SwitcherEvent::ProgramChanged { program, .. } if *program == input(2)
    )));
    assert_eq!(engine.realized_switcher().program(), input(2));
    assert!(engine.realized_switcher().transition().is_none());

    let after = engine.tick().unwrap();
    assert_eq!(after.program.primary, input(2));
    assert_eq!(after.program.secondary, None);
}

#[test]
fn fade_to_black_is_authoritative_reversible_and_program_orthogonal() {
    let mut engine = engine();
    engine
        .execute(
            envelope(
                "ftb-on",
                EngineCommand::FadeToBlack {
                    active: true,
                    duration_frames: 3,
                },
            ),
            0,
        )
        .unwrap();
    assert!(engine.desired_fade_to_black().active);
    assert_eq!(
        engine.desired_fade_to_black().position,
        FadeToBlackPosition::BLACK
    );
    assert_eq!(
        engine.realized_fade_to_black().position,
        FadeToBlackPosition::LIVE
    );

    let first = engine.tick().unwrap();
    assert_eq!(first.fade_to_black.target(), FadeToBlackTarget::Black);
    assert_eq!(
        first.fade_to_black.interval_start(),
        FadeToBlackPosition::LIVE
    );
    assert_eq!(first.fade_to_black.interval_end().numerator(), 21_845);
    assert_eq!(engine.realized_fade_to_black().position.numerator(), 21_845);
    assert_eq!(engine.snapshot(), Err(SnapshotError::WorkInFlight));

    engine
        .execute(
            envelope("program-fade", EngineCommand::Fade { duration_frames: 2 }),
            0,
        )
        .unwrap();
    let second = engine.tick().unwrap();
    assert_eq!(second.program.transition_kind, Some(TransitionKind::Fade));
    assert_eq!(second.fade_to_black.interval_start().numerator(), 21_845);
    assert_eq!(second.fade_to_black.interval_end().numerator(), 43_690);

    engine
        .execute(
            envelope(
                "ftb-off",
                EngineCommand::FadeToBlack {
                    active: false,
                    duration_frames: 2,
                },
            ),
            0,
        )
        .unwrap();
    assert!(!engine.desired_fade_to_black().active);
    let reversal = engine.tick().unwrap();
    assert_eq!(reversal.fade_to_black.target(), FadeToBlackTarget::Live);
    assert_eq!(reversal.fade_to_black.interval_start().numerator(), 43_690);
    assert_eq!(reversal.fade_to_black.interval_end().numerator(), 21_845);
    assert!(reversal.events.iter().any(|event| matches!(
        event,
        SwitcherEvent::ProgramChanged { program, .. } if *program == input(2)
    )));

    let completed = engine.tick().unwrap();
    assert_eq!(
        completed.fade_to_black.interval_end(),
        FadeToBlackPosition::LIVE
    );
    assert_eq!(
        engine.realized_fade_to_black(),
        engine.desired_fade_to_black()
    );
    let snapshot = engine.snapshot().unwrap();
    assert_eq!(
        Engine::restore(snapshot).unwrap().realized_fade_to_black(),
        engine.realized_fade_to_black()
    );
}

#[test]
fn fade_to_black_duration_is_validated_before_desired_mutation() {
    for (key, duration_frames, expected) in [
        ("zero-ftb", 0, "Fade-to-Black duration must be nonzero"),
        (
            "oversized-ftb",
            3_601,
            "Fade-to-Black duration must not exceed 3600 frames",
        ),
    ] {
        let mut engine = engine();
        let outcome = engine
            .execute(
                envelope(
                    key,
                    EngineCommand::FadeToBlack {
                        active: true,
                        duration_frames,
                    },
                ),
                0,
            )
            .unwrap();
        let rejection = outcome.receipt.rejected().unwrap();
        assert_eq!(rejection.rejection.code, RejectionCode::InvalidCommand);
        assert_eq!(rejection.rejection.message, expected);
        assert!(!engine.desired_fade_to_black().active);
        assert!(!engine.realized_fade_to_black().active);
    }
}

#[test]
fn manual_transition_holds_reverses_commits_and_restores_exactly() {
    let mut engine = engine();
    engine
        .execute(
            envelope(
                "manual-start",
                EngineCommand::StartManualTransition {
                    kind: EngineManualTransitionKind::Fade,
                },
            ),
            0,
        )
        .unwrap();
    let start = engine.tick().unwrap();
    assert_eq!(
        (
            start.program.mix_start_numerator,
            start.program.mix_end_numerator,
            start.program.mix_denominator,
        ),
        (0, 0, 10_000)
    );

    engine
        .execute(
            envelope(
                "manual-forward",
                EngineCommand::SetManualTransitionPosition {
                    position: EngineManualTransitionPosition::new(8_000).unwrap(),
                },
            ),
            0,
        )
        .unwrap();
    let forward = engine.tick().unwrap();
    assert_eq!(
        (
            forward.program.mix_start_numerator,
            forward.program.mix_end_numerator,
        ),
        (0, 8_000)
    );

    engine
        .execute(
            envelope(
                "manual-reverse",
                EngineCommand::SetManualTransitionPosition {
                    position: EngineManualTransitionPosition::new(2_500).unwrap(),
                },
            ),
            0,
        )
        .unwrap();
    let reverse = engine.tick().unwrap();
    assert_eq!(
        (
            reverse.program.mix_start_numerator,
            reverse.program.mix_end_numerator,
        ),
        (8_000, 2_500)
    );

    let held = engine.tick().unwrap();
    assert_eq!(
        (
            held.program.mix_start_numerator,
            held.program.mix_end_numerator,
        ),
        (2_500, 2_500)
    );

    let snapshot = engine.snapshot().unwrap();
    let mut restored = Engine::restore(snapshot).unwrap();
    assert_eq!(
        restored
            .realized_switcher()
            .t_bar()
            .unwrap()
            .position()
            .basis_points(),
        2_500
    );
    restored
        .execute(
            envelope("manual-commit", EngineCommand::CommitManualTransition),
            0,
        )
        .unwrap();
    let committed = restored.tick().unwrap();
    assert_eq!(committed.program.primary, input(2));
    assert!(committed.program.secondary.is_none());
    assert_eq!(restored.realized_switcher().program(), input(2));
    assert!(restored.realized_switcher().t_bar().is_none());
    assert_eq!(restored.snapshot().unwrap().receipts().len(), 4);
}

#[test]
fn manual_alpha_fade_preserves_kind_and_exact_position_through_restore() {
    let mut engine = engine();
    engine
        .execute(
            envelope(
                "alpha-manual-start",
                EngineCommand::StartManualTransition {
                    kind: EngineManualTransitionKind::AlphaFade,
                },
            ),
            0,
        )
        .unwrap();
    engine.tick().unwrap();
    engine
        .execute(
            envelope(
                "alpha-manual-position",
                EngineCommand::SetManualTransitionPosition {
                    position: EngineManualTransitionPosition::new(6_250).unwrap(),
                },
            ),
            0,
        )
        .unwrap();
    let moving = engine.tick().unwrap();
    assert_eq!(
        moving.program.transition_kind,
        Some(TransitionKind::AlphaFade)
    );
    assert_eq!(
        (
            moving.program.mix_start_numerator,
            moving.program.mix_end_numerator,
        ),
        (0, 6_250)
    );

    let mut restored = Engine::restore(engine.snapshot().unwrap()).unwrap();
    let held = restored.tick().unwrap();
    assert_eq!(
        held.program.transition_kind,
        Some(TransitionKind::AlphaFade)
    );
    assert_eq!(
        (
            held.program.mix_start_numerator,
            held.program.mix_end_numerator,
        ),
        (6_250, 6_250)
    );
    restored
        .execute(
            envelope("alpha-manual-commit", EngineCommand::CommitManualTransition),
            0,
        )
        .unwrap();
    let committed = restored.tick().unwrap();
    assert_eq!(committed.program.primary, input(2));
    assert!(committed.program.secondary.is_none());
}

#[test]
fn cancelling_manual_transition_preserves_program_and_preview() {
    let mut engine = engine();
    for (key, command) in [
        (
            "manual-start",
            EngineCommand::StartManualTransition {
                kind: EngineManualTransitionKind::Wipe,
            },
        ),
        (
            "manual-position",
            EngineCommand::SetManualTransitionPosition {
                position: EngineManualTransitionPosition::new(7_500).unwrap(),
            },
        ),
        ("manual-cancel", EngineCommand::CancelManualTransition),
    ] {
        engine.execute(envelope(key, command), 0).unwrap();
        engine.tick().unwrap();
    }
    assert_eq!(engine.realized_switcher().program(), input(1));
    assert_eq!(engine.realized_switcher().preview(), input(2));
    assert!(engine.realized_switcher().t_bar().is_none());
    assert_eq!(engine.revision(), Revision::new(3));
}

#[test]
fn wipe_is_idempotent_and_realizes_on_exact_frame_boundaries() {
    let mut engine = engine();
    let command = envelope("wipe", EngineCommand::Wipe { duration_frames: 3 });
    let first = engine.execute(command.clone(), 0).unwrap();
    let duplicate = engine.execute(command, 0).unwrap();

    assert_eq!(first.receipt.accepted().unwrap().revision, Revision::new(1));
    assert!(duplicate.replayed);
    assert_eq!(duplicate.receipt, first.receipt);
    assert_eq!(engine.revision(), Revision::new(1));
    assert_eq!(engine.event_sequence(), EventSequence::new(1));

    for (start, end) in [(0, 1), (1, 2), (2, 3)] {
        let frame = engine.tick().unwrap();
        assert_eq!(frame.program.transition_kind, Some(TransitionKind::Wipe));
        assert_eq!(
            (
                frame.program.mix_start_numerator,
                frame.program.mix_end_numerator
            ),
            (start, end)
        );
    }
    let endpoint = engine.tick().unwrap();
    assert_eq!(endpoint.program.primary, input(2));
    assert_eq!(endpoint.program.secondary, None);
    assert_eq!(endpoint.program.transition_kind, None);
    assert_eq!(engine.runtime_generation(), RuntimeGeneration::new(1));
}

#[test]
fn alpha_fade_is_idempotent_and_realizes_on_exact_frame_boundaries() {
    let mut engine = engine();
    let command = envelope(
        "alpha-fade",
        EngineCommand::AlphaFade { duration_frames: 3 },
    );
    let first = engine.execute(command.clone(), 0).unwrap();
    let duplicate = engine.execute(command, 0).unwrap();

    assert_eq!(first.receipt.accepted().unwrap().revision, Revision::new(1));
    assert!(duplicate.replayed);
    assert_eq!(duplicate.receipt, first.receipt);
    assert_eq!(engine.revision(), Revision::new(1));
    assert_eq!(engine.event_sequence(), EventSequence::new(1));

    for (start, end) in [(0, 1), (1, 2), (2, 3)] {
        let frame = engine.tick().unwrap();
        assert_eq!(
            frame.program.transition_kind,
            Some(TransitionKind::AlphaFade)
        );
        assert_eq!(
            (
                frame.program.mix_start_numerator,
                frame.program.mix_end_numerator
            ),
            (start, end)
        );
    }
    let endpoint = engine.tick().unwrap();
    assert_eq!(endpoint.program.primary, input(2));
    assert_eq!(endpoint.program.secondary, None);
    assert_eq!(endpoint.program.transition_kind, None);
    assert_eq!(engine.runtime_generation(), RuntimeGeneration::new(1));
}

#[test]
fn slide_is_idempotent_and_realizes_on_exact_frame_boundaries() {
    let mut engine = engine();
    let command = envelope("slide", EngineCommand::Slide { duration_frames: 3 });
    let first = engine.execute(command.clone(), 0).unwrap();
    let duplicate = engine.execute(command, 0).unwrap();

    assert_eq!(first.receipt.accepted().unwrap().revision, Revision::new(1));
    assert!(duplicate.replayed);
    assert_eq!(duplicate.receipt, first.receipt);
    assert_eq!(engine.revision(), Revision::new(1));
    assert_eq!(engine.event_sequence(), EventSequence::new(1));

    for (start, end) in [(0, 1), (1, 2), (2, 3)] {
        let frame = engine.tick().unwrap();
        assert_eq!(frame.program.transition_kind, Some(TransitionKind::Slide));
        assert_eq!(
            (
                frame.program.mix_start_numerator,
                frame.program.mix_end_numerator
            ),
            (start, end)
        );
    }
    let endpoint = engine.tick().unwrap();
    assert_eq!(endpoint.program.primary, input(2));
    assert_eq!(endpoint.program.secondary, None);
    assert_eq!(endpoint.program.transition_kind, None);
    assert_eq!(engine.runtime_generation(), RuntimeGeneration::new(1));
}

#[test]
fn zoom_is_idempotent_and_realizes_on_exact_frame_boundaries() {
    let mut engine = engine();
    let command = envelope("zoom", EngineCommand::Zoom { duration_frames: 3 });
    let first = engine.execute(command.clone(), 0).unwrap();
    let duplicate = engine.execute(command, 0).unwrap();

    assert_eq!(first.receipt.accepted().unwrap().revision, Revision::new(1));
    assert!(duplicate.replayed);
    assert_eq!(duplicate.receipt, first.receipt);
    assert_eq!(engine.revision(), Revision::new(1));
    assert_eq!(engine.event_sequence(), EventSequence::new(1));

    for (start, end) in [(0, 1), (1, 2), (2, 3)] {
        let frame = engine.tick().unwrap();
        assert_eq!(frame.program.transition_kind, Some(TransitionKind::Zoom));
        assert_eq!(
            (
                frame.program.mix_start_numerator,
                frame.program.mix_end_numerator
            ),
            (start, end)
        );
    }
    let endpoint = engine.tick().unwrap();
    assert_eq!(endpoint.program.primary, input(2));
    assert_eq!(endpoint.program.secondary, None);
    assert_eq!(endpoint.program.transition_kind, None);
    assert_eq!(engine.runtime_generation(), RuntimeGeneration::new(1));
}

#[test]
fn ready_stinger_is_idempotent_and_realizes_on_exact_media_frames() {
    let mut engine = stinger_engine(true, 1, MissingMediaFallback::KeepProgram);
    let slot = StingerSlotId::new(1).unwrap();
    let command = envelope(
        "stinger",
        EngineCommand::Stinger {
            slot,
            duration_frames: 3,
        },
    );
    let first = engine.execute(command.clone(), 0).unwrap();
    let duplicate = engine.execute(command, 0).unwrap();

    assert_eq!(first.receipt.accepted().unwrap().revision, Revision::new(1));
    assert!(duplicate.replayed);
    assert_eq!(duplicate.receipt, first.receipt);
    assert_eq!(engine.show().desired_switcher().program(), input(2));

    for (start, end) in [(0, 1), (1, 2), (2, 3)] {
        let frame = engine.tick().unwrap();
        assert_eq!(
            frame.program.transition_kind,
            Some(TransitionKind::Stinger(slot))
        );
        assert_eq!(
            (
                frame.program.mix_start_numerator,
                frame.program.mix_end_numerator
            ),
            (start, end)
        );
        assert_eq!(
            engine
                .realized_switcher()
                .stinger(slot)
                .descriptor()
                .unwrap()
                .media_input,
            input(3)
        );
    }
    let endpoint = engine.tick().unwrap();
    assert_eq!(endpoint.program.primary, input(2));
    assert_eq!(endpoint.program.transition_kind, None);
    assert!(engine.snapshot().is_ok());
}

#[test]
fn stinger_slots_configure_replace_and_remove_at_idle_boundaries() {
    let mut engine = engine();
    let slot = StingerSlotId::new(8).unwrap();
    let first = StingerDescriptor::new(
        input(3),
        true,
        2,
        StingerAudioPolicy::MixWithProgram,
        MissingMediaFallback::Fade,
    );
    engine
        .execute(
            envelope(
                "configure-stinger",
                EngineCommand::ConfigureStinger {
                    slot,
                    descriptor: first,
                },
            ),
            0,
        )
        .unwrap();
    assert_eq!(
        engine.show().desired_switcher().stinger(slot).descriptor(),
        Some(&first)
    );
    assert_eq!(
        engine
            .show()
            .desired_switcher()
            .stinger(slot)
            .preload_state(),
        fm_switcher::StingerPreloadState::Ready
    );
    assert_eq!(engine.snapshot(), Err(SnapshotError::WorkInFlight));
    engine.tick().unwrap();
    assert_eq!(
        engine.realized_switcher().stinger(slot),
        engine.show().desired_switcher().stinger(slot)
    );

    let replacement = StingerDescriptor::new(
        input(2),
        false,
        7,
        StingerAudioPolicy::Muted,
        MissingMediaFallback::KeepProgram,
    );
    engine
        .execute(
            envelope(
                "replace-stinger",
                EngineCommand::ConfigureStinger {
                    slot,
                    descriptor: replacement,
                },
            ),
            0,
        )
        .unwrap();
    engine.tick().unwrap();
    assert_eq!(
        engine.realized_switcher().stinger(slot).descriptor(),
        Some(&replacement)
    );
    assert_eq!(
        engine.realized_switcher().stinger(slot).preload_state(),
        fm_switcher::StingerPreloadState::NotRequested
    );

    engine
        .execute(
            envelope("remove-stinger", EngineCommand::RemoveStinger { slot }),
            0,
        )
        .unwrap();
    engine.tick().unwrap();
    assert_eq!(engine.realized_switcher().stinger(slot).descriptor(), None);
    assert_eq!(
        engine.show().desired_switcher().stinger(slot),
        engine.realized_switcher().stinger(slot)
    );
}

#[test]
fn stinger_slot_mutation_rejects_unknown_media_and_active_transitions() {
    let slot = StingerSlotId::new(1).unwrap();
    let mut engine = engine();
    let unknown = engine
        .execute(
            envelope(
                "unknown-media",
                EngineCommand::ConfigureStinger {
                    slot,
                    descriptor: StingerDescriptor::new(
                        input(99),
                        true,
                        1,
                        StingerAudioPolicy::Muted,
                        MissingMediaFallback::Cut,
                    ),
                },
            ),
            0,
        )
        .unwrap();
    assert_eq!(
        unknown.receipt.rejected().unwrap().rejection.code,
        RejectionCode::NotFound
    );

    engine
        .execute(
            envelope("fade", EngineCommand::Fade { duration_frames: 2 }),
            0,
        )
        .unwrap();
    let automatic = engine
        .execute(
            envelope("remove-automatic", EngineCommand::RemoveStinger { slot }),
            0,
        )
        .unwrap();
    assert_eq!(
        automatic.receipt.rejected().unwrap().rejection.code,
        RejectionCode::Conflict
    );
    for _ in 0..2 {
        engine.tick().unwrap();
    }

    engine
        .execute(
            envelope(
                "manual",
                EngineCommand::StartManualTransition {
                    kind: EngineManualTransitionKind::Fade,
                },
            ),
            0,
        )
        .unwrap();
    engine.tick().unwrap();
    let manual = engine
        .execute(
            envelope("remove-manual", EngineCommand::RemoveStinger { slot }),
            0,
        )
        .unwrap();
    assert_eq!(
        manual.receipt.rejected().unwrap().rejection.code,
        RejectionCode::Conflict
    );
}

#[test]
fn stinger_rejects_unconfigured_or_out_of_range_work_before_scheduling() {
    let slot = StingerSlotId::new(1).unwrap();
    let mut unconfigured = engine();
    let outcome = unconfigured
        .execute(
            envelope(
                "unconfigured-stinger",
                EngineCommand::Stinger {
                    slot,
                    duration_frames: 2,
                },
            ),
            0,
        )
        .unwrap();
    assert_eq!(
        outcome.receipt.rejected().unwrap().rejection.code,
        RejectionCode::NotFound
    );
    unconfigured.tick().unwrap();
    assert_eq!(unconfigured.realized_switcher().program(), input(1));

    let mut invalid_cut = stinger_engine(true, 3, MissingMediaFallback::Cut);
    let outcome = invalid_cut
        .execute(
            envelope(
                "invalid-stinger-cut",
                EngineCommand::Stinger {
                    slot,
                    duration_frames: 2,
                },
            ),
            0,
        )
        .unwrap();
    assert_eq!(
        outcome.receipt.rejected().unwrap().rejection.code,
        RejectionCode::InvalidCommand
    );
    invalid_cut.tick().unwrap();
    assert_eq!(invalid_cut.realized_switcher().program(), input(1));
}

#[test]
fn missing_stinger_media_applies_keep_or_fade_fallback_without_stale_busy_state() {
    let slot = StingerSlotId::new(1).unwrap();
    let mut keep = stinger_engine(false, 1, MissingMediaFallback::KeepProgram);
    keep.execute(
        envelope(
            "keep-program",
            EngineCommand::Stinger {
                slot,
                duration_frames: 3,
            },
        ),
        0,
    )
    .unwrap();
    assert_eq!(keep.show().desired_switcher().program(), input(1));
    let frame = keep.tick().unwrap();
    assert_eq!(frame.program.transition_kind, None);
    assert!(matches!(
        frame.events.as_slice(),
        [SwitcherEvent::StingerFallbackApplied {
            fallback: MissingMediaFallback::KeepProgram,
            ..
        }]
    ));
    assert!(keep.snapshot().is_ok());

    let mut fade = stinger_engine(false, 1, MissingMediaFallback::Fade);
    fade.execute(
        envelope(
            "fallback-fade",
            EngineCommand::Stinger {
                slot,
                duration_frames: 2,
            },
        ),
        0,
    )
    .unwrap();
    assert_eq!(fade.show().desired_switcher().program(), input(2));
    for _ in 0..2 {
        assert_eq!(
            fade.tick().unwrap().program.transition_kind,
            Some(TransitionKind::Fade)
        );
    }
    assert!(fade.snapshot().is_ok());
}

#[test]
fn idle_snapshot_restores_state_counters_timeline_and_receipts() {
    let mut engine = engine();
    let original = engine
        .execute(envelope("cut", EngineCommand::Cut), 0)
        .unwrap();
    engine.tick().unwrap();

    let snapshot = engine.snapshot().unwrap();
    assert_eq!(snapshot.revision(), Revision::new(1));
    assert_eq!(snapshot.frames_rendered(), 1);
    assert_eq!(snapshot.runtime_generation(), RuntimeGeneration::new(1));

    let mut restored = Engine::restore(snapshot).unwrap();
    assert_eq!(restored.show().desired_switcher().program(), input(2));
    assert_eq!(restored.realized_switcher().program(), input(2));
    assert_eq!(restored.clock_time(), ClockTime::ZERO);

    let duplicate = restored
        .execute(envelope("cut", EngineCommand::SelectPreview(input(3))), 0)
        .unwrap();
    assert!(duplicate.replayed);
    assert_eq!(duplicate.receipt, original.receipt);

    let next = restored.tick().unwrap();
    assert_eq!(next.frame.get(), 1);
    assert_eq!(next.deadline.as_nanos(), 40_000_000);
    assert!(next.events.is_empty());
    assert_eq!(next.runtime_generation, RuntimeGeneration::new(1));
}

#[test]
fn snapshot_is_rejected_while_work_is_in_flight() {
    let mut engine = engine();
    engine
        .execute(envelope("cut", EngineCommand::Cut), 0)
        .unwrap();
    assert_eq!(engine.snapshot().unwrap_err(), SnapshotError::WorkInFlight);
}

#[test]
fn persisted_restore_preserves_coordinates_and_replays_all_receipts() {
    let mut original = engine();
    let accepted = original
        .execute(
            envelope("accepted", EngineCommand::SelectPreview(input(3))),
            0,
        )
        .unwrap();
    original.tick().unwrap();
    let rejected = original
        .execute(
            envelope("rejected", EngineCommand::Cut).expecting(Revision::new(0)),
            0,
        )
        .unwrap();
    original.tick().unwrap();
    let snapshot = original.snapshot().unwrap();

    let mut restored = Engine::restore_persisted(
        snapshot.show().clone(),
        snapshot.realized_switcher().clone(),
        snapshot.frame_rate(),
        domain(),
        restore_state(&snapshot),
    )
    .unwrap();

    assert_eq!(restored.frame_cursor(), FrameNumber::new(2));
    assert_eq!(restored.runtime_generation(), RuntimeGeneration::new(1));
    assert_eq!(restored.state_epoch(), StateEpoch::new(1));
    assert_eq!(restored.event_sequence(), EventSequence::new(1));
    assert_eq!(restored.clock_time(), ClockTime::from_nanos(40_000_000));

    let accepted_duplicate = restored
        .execute(envelope("accepted", EngineCommand::Cut), 0)
        .unwrap();
    assert!(accepted_duplicate.replayed);
    assert_eq!(accepted_duplicate.receipt, accepted.receipt);

    let rejected_duplicate = restored
        .execute(
            envelope("rejected", EngineCommand::SelectPreview(input(1))),
            0,
        )
        .unwrap();
    assert!(rejected_duplicate.replayed);
    assert_eq!(rejected_duplicate.receipt, rejected.receipt);

    let next = restored.tick().unwrap();
    assert_eq!(next.frame, FrameNumber::new(2));
    assert_eq!(next.deadline.as_nanos(), 80_000_000);
    assert!(next.events.is_empty());
    assert_eq!(next.runtime_generation, RuntimeGeneration::new(1));
}

#[test]
fn persisted_restore_rejects_mismatched_idle_routing() {
    let snapshot = engine().snapshot().unwrap();
    let realized =
        SwitcherState::new(snapshot.show().inputs().to_vec(), input(2), input(1)).unwrap();

    assert_eq!(
        restore_persisted(&snapshot, realized, restore_state(&snapshot)).unwrap_err(),
        SnapshotError::MismatchedSwitcherRouting
    );
}

#[test]
fn persisted_restore_rejects_mismatched_stinger_configuration_or_readiness() {
    let snapshot = engine().snapshot().unwrap();
    let mut show = snapshot.show().clone();
    let slot = StingerSlotId::new(1).unwrap();
    show.configure_stinger(
        slot,
        StingerDescriptor::new(
            input(3),
            true,
            2,
            StingerAudioPolicy::Muted,
            MissingMediaFallback::Cut,
        ),
    )
    .unwrap();
    show.preload_stinger(slot, true).unwrap();

    assert_eq!(
        Engine::restore_persisted(
            show,
            snapshot.realized_switcher().clone(),
            snapshot.frame_rate(),
            domain(),
            restore_state(&snapshot),
        )
        .unwrap_err(),
        SnapshotError::MismatchedSwitcherRouting
    );
}

#[test]
fn persisted_restore_accepts_settled_realized_manual_state_with_desired_interval_origin() {
    let snapshot = engine().snapshot().unwrap();
    let mut show = snapshot.show().clone();
    show.restore_manual_transition(TBarState::restore(
        TransitionKind::Fade,
        input(1),
        input(2),
        TBarPosition::START,
        TBarPosition::new(6_250).unwrap(),
    ))
    .unwrap();
    let mut realized = snapshot.realized_switcher().clone();
    realized
        .restore_t_bar(TBarState::restore(
            TransitionKind::Fade,
            input(1),
            input(2),
            TBarPosition::new(6_250).unwrap(),
            TBarPosition::new(6_250).unwrap(),
        ))
        .unwrap();

    Engine::restore_persisted(
        show,
        realized,
        snapshot.frame_rate(),
        domain(),
        restore_state(&snapshot),
    )
    .unwrap();
}

#[test]
fn persisted_restore_rejects_unsettled_manual_interval_boundaries() {
    let snapshot = engine().snapshot().unwrap();
    let position = TBarPosition::new(6_250).unwrap();

    let mut malformed_desired = snapshot.show().clone();
    malformed_desired
        .restore_manual_transition(TBarState::restore(
            TransitionKind::Fade,
            input(1),
            input(2),
            TBarPosition::new(2_500).unwrap(),
            position,
        ))
        .unwrap();
    let mut settled_realized = snapshot.realized_switcher().clone();
    settled_realized
        .restore_t_bar(TBarState::restore(
            TransitionKind::Fade,
            input(1),
            input(2),
            position,
            position,
        ))
        .unwrap();
    assert_eq!(
        Engine::restore_persisted(
            malformed_desired,
            settled_realized,
            snapshot.frame_rate(),
            domain(),
            restore_state(&snapshot),
        )
        .unwrap_err(),
        SnapshotError::MismatchedManualTransition
    );

    let mut valid_desired = snapshot.show().clone();
    valid_desired
        .restore_manual_transition(TBarState::restore(
            TransitionKind::Fade,
            input(1),
            input(2),
            TBarPosition::START,
            position,
        ))
        .unwrap();
    let mut malformed_realized = snapshot.realized_switcher().clone();
    malformed_realized
        .restore_t_bar(TBarState::restore(
            TransitionKind::Fade,
            input(1),
            input(2),
            TBarPosition::new(2_500).unwrap(),
            position,
        ))
        .unwrap();
    assert_eq!(
        Engine::restore_persisted(
            valid_desired,
            malformed_realized,
            snapshot.frame_rate(),
            domain(),
            restore_state(&snapshot),
        )
        .unwrap_err(),
        SnapshotError::MismatchedManualTransition
    );
}

#[test]
fn persisted_restore_requires_zero_clock_at_frame_zero() {
    let snapshot = engine().snapshot().unwrap();
    let mut state = restore_state(&snapshot);
    state.clock_time = ClockTime::from_nanos(1);

    assert_eq!(
        restore_persisted(&snapshot, snapshot.realized_switcher().clone(), state).unwrap_err(),
        SnapshotError::ClockTimeMismatch {
            expected_ns: 0,
            actual_ns: 1,
        }
    );
}

#[test]
fn persisted_restore_requires_clock_at_last_rendered_deadline() {
    let mut original = engine();
    original.tick().unwrap();
    original.tick().unwrap();
    let snapshot = original.snapshot().unwrap();
    let mut state = restore_state(&snapshot);
    state.clock_time = ClockTime::ZERO;

    assert_eq!(
        restore_persisted(&snapshot, snapshot.realized_switcher().clone(), state).unwrap_err(),
        SnapshotError::ClockTimeMismatch {
            expected_ns: 40_000_000,
            actual_ns: 0,
        }
    );
}

#[test]
fn persisted_restore_rejects_an_unrealized_accepted_receipt() {
    let mut original = engine();
    original
        .execute(
            envelope("accepted", EngineCommand::SelectPreview(input(3))),
            0,
        )
        .unwrap();
    original.tick().unwrap();
    let snapshot = original.snapshot().unwrap();
    let mut state = restore_state(&snapshot);
    state.frame_cursor = FrameNumber::new(0);
    state.clock_time = ClockTime::ZERO;

    assert_eq!(
        restore_persisted(&snapshot, snapshot.realized_switcher().clone(), state).unwrap_err(),
        SnapshotError::UnrealizedAcceptedReceipt {
            target_frame: 0,
            frame_cursor: 0,
        }
    );
}

#[test]
fn persisted_restore_requires_counters_to_equal_accepted_receipts() {
    let mut original = engine();
    original
        .execute(
            envelope("accepted", EngineCommand::SelectPreview(input(3))),
            0,
        )
        .unwrap();
    original.tick().unwrap();
    let snapshot = original.snapshot().unwrap();
    let expected = SnapshotError::CounterMismatch {
        accepted_commands: 1,
        revision: 0,
        event_sequence: 1,
        runtime_generation: 1,
    };

    let mut state = restore_state(&snapshot);
    state.revision = Revision::new(0);
    assert_eq!(
        restore_persisted(&snapshot, snapshot.realized_switcher().clone(), state).unwrap_err(),
        expected
    );

    let mut state = restore_state(&snapshot);
    state.event_sequence = EventSequence::new(0);
    assert_eq!(
        restore_persisted(&snapshot, snapshot.realized_switcher().clone(), state).unwrap_err(),
        SnapshotError::CounterMismatch {
            accepted_commands: 1,
            revision: 1,
            event_sequence: 0,
            runtime_generation: 1,
        }
    );

    let mut state = restore_state(&snapshot);
    state.runtime_generation = RuntimeGeneration::new(0);
    assert_eq!(
        restore_persisted(&snapshot, snapshot.realized_switcher().clone(), state).unwrap_err(),
        SnapshotError::CounterMismatch {
            accepted_commands: 1,
            revision: 1,
            event_sequence: 1,
            runtime_generation: 0,
        }
    );
}

#[test]
fn persisted_restore_directly_restores_a_very_large_valid_cursor() {
    const CURSOR: u64 = 400_000_000_000;
    const FRAME_DURATION_NS: u64 = 40_000_000;

    let snapshot = engine().snapshot().unwrap();
    let mut state = restore_state(&snapshot);
    state.frame_cursor = FrameNumber::new(CURSOR);
    state.clock_time = ClockTime::from_nanos((CURSOR - 1) * FRAME_DURATION_NS);

    let mut restored =
        restore_persisted(&snapshot, snapshot.realized_switcher().clone(), state).unwrap();
    assert_eq!(restored.frame_cursor(), FrameNumber::new(CURSOR));
    assert_eq!(
        restored.clock_time(),
        ClockTime::from_nanos((CURSOR - 1) * FRAME_DURATION_NS)
    );

    let next = restored.tick().unwrap();
    assert_eq!(next.frame, FrameNumber::new(CURSOR));
    assert_eq!(
        next.deadline,
        ClockTime::from_nanos(CURSOR * FRAME_DURATION_NS)
    );
}
