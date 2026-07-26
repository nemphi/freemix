use fm_ptz::{
    CameraId, CoalescingQueue, ContinuousMove, ContinuousSource, PresetId, PtzIntent, PushOutcome,
    QueueError, QueuedIntent,
};

fn camera() -> CameraId {
    CameraId::new("camera").unwrap()
}

fn continuous(source: ContinuousSource, pan: i32) -> QueuedIntent {
    QueuedIntent::new(
        camera(),
        PtzIntent::MoveContinuous(ContinuousMove {
            source,
            pan,
            tilt: 0,
            zoom: 0,
            focus: 0,
        }),
    )
}

#[test]
fn adjacent_continuous_moves_coalesce_to_the_latest_value() {
    let mut queue = CoalescingQueue::new(2).unwrap();
    assert_eq!(
        queue.push(continuous(ContinuousSource::Joystick, 1)),
        Ok(PushOutcome::Enqueued)
    );
    assert_eq!(
        queue.push(continuous(ContinuousSource::Joystick, 8)),
        Ok(PushOutcome::Coalesced)
    );
    assert_eq!(queue.len(), 1);
    assert_eq!(
        queue.pop().unwrap(),
        continuous(ContinuousSource::Joystick, 8)
    );
}

#[test]
fn discrete_intents_keep_order_across_coalescing_and_eviction() {
    let mut queue = CoalescingQueue::new(4).unwrap();
    queue
        .push(continuous(ContinuousSource::Joystick, 1))
        .unwrap();
    queue
        .push(QueuedIntent::new(
            camera(),
            PtzIntent::RecallPreset(PresetId::new(1)),
        ))
        .unwrap();
    queue.push(continuous(ContinuousSource::Mouse, 2)).unwrap();
    queue
        .push(QueuedIntent::new(camera(), PtzIntent::Stop))
        .unwrap();

    assert_eq!(
        queue.push(QueuedIntent::new(
            camera(),
            PtzIntent::RecallPreset(PresetId::new(2)),
        )),
        Ok(PushOutcome::ReplacedContinuous)
    );
    let intents: Vec<_> = queue.iter().map(|queued| &queued.intent).collect();
    assert_eq!(
        intents,
        vec![
            &PtzIntent::RecallPreset(PresetId::new(1)),
            &PtzIntent::MoveContinuous(ContinuousMove {
                source: ContinuousSource::Mouse,
                pan: 2,
                tilt: 0,
                zoom: 0,
                focus: 0,
            }),
            &PtzIntent::Stop,
            &PtzIntent::RecallPreset(PresetId::new(2)),
        ]
    );
}

#[test]
fn a_queue_full_of_discrete_intents_rejects_only_new_work() {
    let mut queue = CoalescingQueue::new(2).unwrap();
    queue
        .push(QueuedIntent::new(camera(), PtzIntent::Stop))
        .unwrap();
    queue
        .push(QueuedIntent::new(
            camera(),
            PtzIntent::RecallPreset(PresetId::new(4)),
        ))
        .unwrap();

    assert_eq!(
        queue.push(continuous(ContinuousSource::Joystick, 2)),
        Ok(PushOutcome::DroppedContinuous)
    );
    assert_eq!(
        queue.push(QueuedIntent::new(camera(), PtzIntent::Home)),
        Err(QueueError::FullOfDiscreteIntents)
    );
    assert!(matches!(queue.pop().unwrap().intent, PtzIntent::Stop));
    assert_eq!(
        queue.pop().unwrap().intent,
        PtzIntent::RecallPreset(PresetId::new(4))
    );
}
