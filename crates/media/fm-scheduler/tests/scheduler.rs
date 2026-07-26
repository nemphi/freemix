use fm_scheduler::{
    ActionError, FrameNumber, FramePacer, FrameScheduler, InputQueueError, PacingError, PlanError,
    PlanGeneration, QueuePolicy, QueuePush, TickError,
};
use fm_types::FrameRate;
use std::sync::Arc;

#[test]
fn fractional_frame_rate_pacing_is_exact_without_accumulated_drift() {
    let rate = FrameRate::new(60_000, 1_001).unwrap();
    let scheduler =
        FrameScheduler::<u8, u8, (), _>::new(rate, 5, PlanGeneration::new(1), Arc::new("plan"));

    let pacer = scheduler.pacer();
    assert_eq!(
        pacer.deadline_for(FrameNumber::new(1)).unwrap().at_ns,
        16_683_338
    );
    assert_eq!(
        pacer.deadline_for(FrameNumber::new(2)).unwrap().at_ns,
        33_366_671
    );
    assert_eq!(
        pacer.deadline_for(FrameNumber::new(60_000)).unwrap().at_ns,
        1_001_000_000_005
    );
}

fn assert_direct_pacer_restore_matches_iteration(rate: FrameRate, origin_ns: u64, cursor: u64) {
    let mut iterative = FramePacer::new(rate, origin_ns);
    for _ in 0..cursor {
        iterative.advance().unwrap();
    }

    let restored = FramePacer::restore(rate, origin_ns, FrameNumber::new(cursor)).unwrap();
    assert_eq!(restored, iterative);
    assert_eq!(restored.next_deadline(), iterative.next_deadline());
}

#[test]
fn direct_pacer_restore_matches_iteration_for_integer_and_fractional_rates() {
    assert_direct_pacer_restore_matches_iteration(FrameRate::new(25, 1).unwrap(), 17, 100_000);
    assert_direct_pacer_restore_matches_iteration(
        FrameRate::new(60_000, 1_001).unwrap(),
        23,
        1_000_000,
    );
}

#[test]
fn direct_pacer_restore_rejects_overflow_and_exhausted_cursors() {
    let one_fps = FrameRate::new(1, 1).unwrap();
    let first_overflowing_frame = u64::MAX / 1_000_000_000 + 1;
    assert_eq!(
        FramePacer::restore(one_fps, 0, FrameNumber::new(first_overflowing_frame)),
        Err(PacingError::DeadlineOverflow)
    );

    let fastest_rate = FrameRate::new(u32::MAX, 1).unwrap();
    assert_eq!(
        FramePacer::restore(fastest_rate, 0, FrameNumber::new(u64::MAX)),
        Err(PacingError::FrameNumberExhausted)
    );

    assert!(matches!(
        FrameScheduler::<u8, u8, (), _>::restore(
            one_fps,
            0,
            FrameNumber::new(first_overflowing_frame),
            PlanGeneration::new(1),
            Arc::new("plan")
        ),
        Err(PacingError::DeadlineOverflow)
    ));
    assert!(matches!(
        FrameScheduler::<u8, u8, (), _>::restore(
            fastest_rate,
            0,
            FrameNumber::new(u64::MAX),
            PlanGeneration::new(1),
            Arc::new("plan")
        ),
        Err(PacingError::FrameNumberExhausted)
    ));
}

#[test]
fn restored_scheduler_starts_idle_and_counts_only_post_restore_ticks() {
    let rate = FrameRate::new(60_000, 1_001).unwrap();
    let cursor = FrameNumber::new(2_000_000);
    let generation = PlanGeneration::new(42);
    let plan = Arc::new("restored");
    let mut scheduler = FrameScheduler::<u8, u8, &'static str, _>::restore(
        rate,
        11,
        cursor,
        generation,
        Arc::clone(&plan),
    )
    .unwrap();

    assert_eq!(scheduler.pacer().next_frame(), cursor);
    assert_eq!(scheduler.plan_generation(), generation);
    assert_eq!(scheduler.telemetry().realized_frames, 0);
    assert_eq!(scheduler.input_depth(&7), None);
    assert!(matches!(
        scheduler.schedule_plan(
            PlanGeneration::new(43),
            FrameNumber::new(cursor.get() - 1),
            Arc::new("stale boundary")
        ),
        Err(PlanError::BoundaryNotFuture { .. })
    ));

    let next_deadline = scheduler.pacer().next_deadline().unwrap();
    let tick = scheduler.tick(next_deadline.at_ns).unwrap();
    assert_eq!(tick.deadline, next_deadline);
    assert!(!tick.late);
    assert!(tick.actions.is_empty());
    assert_eq!(tick.plan_generation, generation);
    assert!(Arc::ptr_eq(&tick.plan, &plan));
    assert_eq!(tick.activation, None);
    assert_eq!(scheduler.telemetry().realized_frames, 1);
    assert_eq!(scheduler.telemetry().late_frames, 0);
    assert_eq!(scheduler.telemetry().dropped_frames, 0);
}

#[test]
fn input_queues_enforce_each_overflow_policy_and_bounds() {
    let rate = FrameRate::new(30, 1).unwrap();
    let mut scheduler =
        FrameScheduler::<_, _, (), _>::new(rate, 0, PlanGeneration::new(1), Arc::new("plan"));
    scheduler
        .register_input("live", 2, QueuePolicy::DropOldest)
        .unwrap();
    scheduler
        .register_input("preview", 1, QueuePolicy::DropNewest)
        .unwrap();
    scheduler
        .register_input("file", 1, QueuePolicy::BlockProducer)
        .unwrap();

    scheduler.push_frame(&"live", 1).unwrap();
    scheduler.push_frame(&"live", 2).unwrap();
    assert_eq!(
        scheduler.push_frame(&"live", 3),
        Ok(QueuePush::Enqueued { dropped: Some(1) })
    );
    assert_eq!(scheduler.input_depth(&"live"), Some(2));
    assert_eq!(scheduler.pop_frame(&"live"), Some(2));
    assert_eq!(scheduler.pop_frame(&"live"), Some(3));

    scheduler.push_frame(&"preview", 4).unwrap();
    assert_eq!(
        scheduler.push_frame(&"preview", 5),
        Ok(QueuePush::DroppedNewest(5))
    );
    assert_eq!(scheduler.pop_frame(&"preview"), Some(4));

    scheduler.push_frame(&"file", 6).unwrap();
    assert_eq!(
        scheduler.push_frame(&"file", 7),
        Err(InputQueueError::WouldBlock(7))
    );
    assert_eq!(scheduler.pop_frame(&"file"), Some(6));
    assert_eq!(scheduler.telemetry().dropped_frames, 2);
}

#[test]
fn actions_are_ordered_by_target_then_insertion_sequence() {
    let rate = FrameRate::new(25, 1).unwrap();
    let mut scheduler =
        FrameScheduler::<u8, u8, _, _>::new(rate, 0, PlanGeneration::new(1), Arc::new("plan"));
    scheduler
        .schedule_action(FrameNumber::new(2), "late")
        .unwrap();
    scheduler
        .schedule_action(FrameNumber::new(1), "first")
        .unwrap();
    scheduler
        .schedule_action(FrameNumber::new(1), "second")
        .unwrap();

    scheduler.tick(0).unwrap();
    let frame_one = scheduler.tick(40_000_000).unwrap();
    assert_eq!(
        frame_one
            .actions
            .into_iter()
            .map(|scheduled| scheduled.action)
            .collect::<Vec<_>>(),
        vec!["first", "second"]
    );
    let frame_two = scheduler.tick(80_000_000).unwrap();
    assert_eq!(frame_two.actions[0].action, "late");
}

#[test]
fn plan_swap_activates_only_on_the_requested_frame_boundary() {
    let rate = FrameRate::new(10, 1).unwrap();
    let old = Arc::new("old");
    let new = Arc::new("new");
    let mut scheduler = FrameScheduler::<u8, u8, (), _>::new(rate, 0, PlanGeneration::new(3), old);
    scheduler
        .schedule_plan(
            PlanGeneration::new(4),
            FrameNumber::new(2),
            Arc::clone(&new),
        )
        .unwrap();

    assert_eq!(*scheduler.tick(0).unwrap().plan, "old");
    assert_eq!(*scheduler.tick(100_000_000).unwrap().plan, "old");
    let swapped = scheduler.tick(200_000_000).unwrap();

    assert_eq!(*swapped.plan, "new");
    assert_eq!(swapped.plan_generation, PlanGeneration::new(4));
    let activation = swapped.activation.unwrap();
    assert_eq!(activation.target_frame, FrameNumber::new(2));
    assert_eq!(activation.activated_frame, FrameNumber::new(2));
}

#[test]
fn cancellation_and_supersession_remove_obsolete_actions() {
    let rate = FrameRate::new(1, 1).unwrap();
    let mut scheduler =
        FrameScheduler::<u8, u8, _, _>::new(rate, 0, PlanGeneration::new(1), Arc::new("plan"));
    let cancelled = scheduler
        .schedule_action(FrameNumber::new(1), "cancelled")
        .unwrap();
    let obsolete = scheduler
        .schedule_action(FrameNumber::new(1), "obsolete")
        .unwrap();

    assert_eq!(
        scheduler.cancel_action(cancelled).unwrap().action,
        "cancelled"
    );
    let (_, superseded) = scheduler
        .supersede_action(obsolete, FrameNumber::new(1), "replacement")
        .unwrap();
    assert_eq!(superseded.action, "obsolete");
    assert_eq!(
        scheduler.supersede_action(obsolete, FrameNumber::new(1), "missing"),
        Err(ActionError::UnknownAction(obsolete))
    );

    scheduler.tick(0).unwrap();
    let tick = scheduler.tick(1_000_000_000).unwrap();
    assert_eq!(tick.actions.len(), 1);
    assert_eq!(tick.actions[0].action, "replacement");
}

#[test]
fn early_and_late_ticks_update_telemetry_deterministically() {
    let rate = FrameRate::new(2, 1).unwrap();
    let mut scheduler =
        FrameScheduler::<u8, u8, (), _>::new(rate, 100, PlanGeneration::new(1), Arc::new("plan"));

    assert!(matches!(
        scheduler.tick(99),
        Err(TickError::TooEarly {
            now_ns: 99,
            deadline_ns: 100
        })
    ));
    assert_eq!(scheduler.telemetry().realized_frames, 0);

    let on_time = scheduler.tick(100).unwrap();
    assert!(!on_time.late);
    let late = scheduler.tick(500_000_101).unwrap();
    assert!(late.late);
    assert_eq!(scheduler.telemetry().realized_frames, 2);
    assert_eq!(scheduler.telemetry().late_frames, 1);
}
