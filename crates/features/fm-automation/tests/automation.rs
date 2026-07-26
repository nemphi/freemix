use fm_automation::{
    ActivatorEngine, ActivatorMapping, ActivatorRule, AttemptOutcome, AutomationEvent,
    CancelPolicy, Chord, CommandIntent, Condition, ConditionContext, ConflictKind, ControlAddress,
    ControlMode, ControllerError, ControllerInput, ControllerManager, EventFilter, GoAction,
    GoEngine, IntentBuffer, KeyStroke, LearnRequest, MacroDecision, MacroDefinition, MacroError,
    MacroRun, Mapping, Modifiers, Predicate, ProgrammedGo, RetryPolicy, ScheduleId, ScheduleKind,
    ScheduleSet, Shortcut, ShortcutError, ShortcutRegistry, ShortcutScope, TallySnapshot, Trigger,
    TriggerEngine, Value, ValueRange,
};
use fm_command::{IdempotencyKey, MAX_TRANSACTION_COMMANDS};
use fm_types::{InputId, MediaTimestamp};
use std::num::NonZeroU128;

#[derive(Clone, Debug, Eq, PartialEq)]
enum TestCommand {
    Named(&'static str),
    Value(i32),
}

fn chord(keys: &[&str]) -> Chord {
    Chord::new(
        keys.iter()
            .map(|key| KeyStroke::new(*key, Modifiers::default())),
    )
    .unwrap()
}

fn input(value: u128) -> InputId {
    InputId::new(NonZeroU128::new(value).unwrap())
}

fn assert_close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < f64::EPSILON);
}

#[test]
fn shortcut_conflicts_cover_scope_exactness_and_prefixes() {
    let mut shortcuts = ShortcutRegistry::default();
    shortcuts
        .insert(Shortcut {
            id: "global-go".into(),
            scope: ShortcutScope::Global,
            chord: chord(&["G"]),
            intent: CommandIntent::discrete(TestCommand::Named("go")),
        })
        .unwrap();

    let exact = shortcuts.insert(Shortcut {
        id: "local-go".into(),
        scope: ShortcutScope::Local("switcher".into()),
        chord: chord(&["G"]),
        intent: CommandIntent::discrete(TestCommand::Named("other")),
    });
    assert!(matches!(
        exact,
        Err(ShortcutError::Conflict(conflicts))
            if conflicts[0].kind == ConflictKind::Exact
    ));

    let prefix = shortcuts.insert(Shortcut {
        id: "global-chord".into(),
        scope: ShortcutScope::Global,
        chord: chord(&["G", "O"]),
        intent: CommandIntent::discrete(TestCommand::Named("other")),
    });
    assert!(matches!(
        prefix,
        Err(ShortcutError::Conflict(conflicts))
            if conflicts[0].kind == ConflictKind::Prefix
    ));

    let mut locals = ShortcutRegistry::default();
    for scope in ["audio", "switcher"] {
        locals
            .insert(Shortcut {
                id: scope.into(),
                scope: ShortcutScope::Local(scope.into()),
                chord: chord(&["A"]),
                intent: CommandIntent::discrete(TestCommand::Named("local")),
            })
            .unwrap();
    }
}

#[test]
fn triggers_filter_delay_and_fire_in_deterministic_order() {
    let mut engine = TriggerEngine::default();
    for id in ["first", "second"] {
        engine
            .insert(Trigger {
                id: id.into(),
                filter: EventFilter::new("transition-in"),
                delay_ms: 10,
                conditions: Vec::new(),
                intents: vec![CommandIntent::discrete(TestCommand::Named(id))],
            })
            .unwrap();
    }
    engine
        .insert(Trigger {
            id: "filtered-out".into(),
            filter: EventFilter {
                kind: "transition-in".into(),
                source: Some("camera-2".into()),
                conditions: Vec::new(),
            },
            delay_ms: 0,
            conditions: Vec::new(),
            intents: vec![CommandIntent::discrete(TestCommand::Named("wrong"))],
        })
        .unwrap();

    let mut event = AutomationEvent::new("transition-in", 100);
    event.source = Some("camera-1".into());
    engine.ingest(&event).unwrap();
    assert!(engine.poll(109, &ConditionContext::new()).is_empty());
    let fired = engine.poll(110, &ConditionContext::new());
    assert_eq!(
        fired
            .iter()
            .map(|fire| fire.trigger_id.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert!(fired.iter().all(|fire| fire.scheduled_for_ms == 110));
}

#[test]
fn delayed_trigger_conditions_use_fire_time_state_and_can_be_cancelled() {
    let mut engine = TriggerEngine::default();
    engine
        .insert(Trigger {
            id: "safe-take".into(),
            filter: EventFilter::new("ready"),
            delay_ms: 5,
            conditions: vec![Condition::new("armed", Predicate::Equal(Value::Bool(true)))],
            intents: vec![CommandIntent::discrete(TestCommand::Named("take"))],
        })
        .unwrap();
    engine.ingest(&AutomationEvent::new("ready", 20)).unwrap();
    assert!(engine.poll(25, &ConditionContext::new()).is_empty());

    let ids = engine.ingest(&AutomationEvent::new("ready", 30)).unwrap();
    assert!(engine.cancel_action(ids[0]));
    let mut state = ConditionContext::new();
    state.insert("armed".into(), true.into());
    assert!(engine.poll(35, &state).is_empty());
}

#[test]
fn macro_plan_is_bounded_ordered_atomic_and_conditioned() {
    let definition = MacroDefinition::new(
        "take-and-play",
        [
            CommandIntent::discrete(TestCommand::Named("preview")),
            CommandIntent::discrete(TestCommand::Named("take")),
            CommandIntent::discrete(TestCommand::Named("play")),
        ],
        vec![Condition::new("armed", Predicate::Equal(true.into()))],
        RetryPolicy::once(),
        CancelPolicy::Immediate,
    )
    .unwrap();

    assert!(matches!(
        MacroRun::start(definition.clone(), "run-1", &ConditionContext::new()),
        Err(MacroError::ConditionFailed)
    ));
    let mut context = ConditionContext::new();
    context.insert("armed".into(), true.into());
    let (_run, dispatch) = MacroRun::start(definition, "run-1", &context).unwrap();
    assert_eq!(dispatch.envelope.command.len(), 3);
    assert_eq!(
        dispatch.envelope.command.commands(),
        [
            CommandIntent::discrete(TestCommand::Named("preview")),
            CommandIntent::discrete(TestCommand::Named("take")),
            CommandIntent::discrete(TestCommand::Named("play")),
        ]
    );

    let oversized = MacroDefinition::new(
        "too-big",
        (0..=MAX_TRANSACTION_COMMANDS).map(|_| CommandIntent::discrete(TestCommand::Value(1))),
        Vec::new(),
        RetryPolicy::once(),
        CancelPolicy::Immediate,
    );
    assert!(matches!(oversized, Err(MacroError::InvalidTransaction(_))));
}

#[test]
fn macro_retry_boundary_and_both_cancel_policies_are_explicit() {
    let definition = MacroDefinition::new(
        "retry",
        [CommandIntent::discrete(TestCommand::Named("work"))],
        Vec::new(),
        RetryPolicy {
            max_attempts: 2,
            delay_ms: 10,
        },
        CancelPolicy::Immediate,
    )
    .unwrap();
    let (mut run, _) = MacroRun::start(definition, "run", &ConditionContext::new()).unwrap();
    assert_eq!(
        run.complete(AttemptOutcome::Failed { retryable: true }, 100),
        Ok(MacroDecision::RetryAt(110))
    );
    assert_eq!(
        run.retry(109),
        Err(MacroError::NotReady { ready_at_ms: 110 })
    );
    assert_eq!(run.retry(110).unwrap().attempt, 2);
    assert_eq!(run.cancel(), MacroDecision::Cancelled);

    let finish_current = MacroDefinition::new(
        "finish",
        [CommandIntent::discrete(TestCommand::Named("work"))],
        Vec::new(),
        RetryPolicy {
            max_attempts: 2,
            delay_ms: 1,
        },
        CancelPolicy::FinishCurrentAttempt,
    )
    .unwrap();
    let (mut run, _) = MacroRun::start(finish_current, "run", &ConditionContext::new()).unwrap();
    assert_eq!(run.cancel(), MacroDecision::CancellationPending);
    assert_eq!(
        run.complete(AttemptOutcome::Failed { retryable: true }, 0),
        Ok(MacroDecision::Cancelled)
    );
}

#[test]
fn schedule_boundaries_are_inclusive_anchored_and_caller_driven() {
    let mut schedules = ScheduleSet::default();
    schedules
        .schedule_at(
            "once",
            10,
            ScheduleKind::Once,
            CommandIntent::discrete(TestCommand::Named("once")),
        )
        .unwrap();
    assert!(schedules.poll(9).unwrap().is_empty());
    assert_eq!(schedules.poll(10).unwrap()[0].occurrence_ms, 10);

    schedules
        .schedule_at(
            "repeat",
            20,
            ScheduleKind::Every { interval_ms: 10 },
            CommandIntent::discrete(TestCommand::Named("repeat")),
        )
        .unwrap();
    let late = schedules.poll(45).unwrap();
    assert_eq!(late[0].occurrence_ms, 20);
    assert_eq!(late[0].missed_occurrences, 2);
    assert!(schedules.poll(49).unwrap().is_empty());
    assert_eq!(schedules.poll(50).unwrap()[0].occurrence_ms, 50);
    schedules.cancel(&ScheduleId::from("repeat"));

    schedules
        .start_timer(
            "timer",
            1_000,
            25,
            CommandIntent::discrete(TestCommand::Named("timer")),
        )
        .unwrap();
    assert!(schedules.poll(1_024).unwrap().is_empty());
    assert_eq!(
        schedules.poll(1_025).unwrap()[0].id,
        ScheduleId::from("timer")
    );
}

#[test]
fn programmed_go_previews_orders_cancels_and_is_idempotent() {
    let source = input(7);
    let program = ProgrammedGo::new([
        GoAction::Intent(CommandIntent::discrete(TestCommand::Named("preview"))),
        GoAction::Delay(10),
        GoAction::Intent(CommandIntent::discrete(TestCommand::Named("take"))),
        GoAction::Delay(5),
        GoAction::Intent(CommandIntent::discrete(TestCommand::Named("play"))),
    ])
    .unwrap();
    let mut engine = GoEngine::default();
    engine.program(source, program);
    assert_eq!(
        engine
            .preview(source)
            .unwrap()
            .actions
            .iter()
            .map(|action| action.offset_ms)
            .collect::<Vec<_>>(),
        [0, 10, 15]
    );

    let key = IdempotencyKey::new("go-1");
    let first = engine.start(source, key.clone(), 100).unwrap();
    let duplicate = engine.start(source, key, 999).unwrap();
    assert_eq!(duplicate.run_id, first.run_id);
    assert!(duplicate.replayed);
    assert_eq!(engine.poll(100)[0].action.index, 0);
    assert_eq!(engine.poll(110)[0].action.index, 1);
    assert!(engine.cancel(first.run_id));
    assert!(engine.poll(115).is_empty());
}

#[test]
fn controller_maps_ranges_velocity_and_retains_mapping_across_reconnect() {
    let mut controllers = ControllerManager::default();
    let address = ControlAddress::new("deck", "pad-1");
    controllers.connect("deck", MediaTimestamp::new(1)).unwrap();
    controllers
        .add_mapping(Mapping {
            id: "take".into(),
            address: address.clone(),
            target: "switcher.take".into(),
            input_range: ValueRange::new(0.0, 127.0).unwrap(),
            output_range: ValueRange::new(0.0, 1.0).unwrap(),
            velocity_range: Some(ValueRange::new(1.0, 127.0).unwrap()),
            mode: ControlMode::Button { threshold: 64.0 },
        })
        .unwrap();

    let mapped = controllers
        .map_input(ControllerInput {
            address: &address,
            value: 127.0,
            velocity: Some(64.0),
            timestamp: MediaTimestamp::new(2),
        })
        .unwrap();
    let CommandIntent::Discrete(mapped) = &mapped[0] else {
        panic!("button must be discrete");
    };
    assert_close(mapped.value, 1.0);
    assert_close(mapped.velocity.unwrap(), 0.5);
    assert!(
        controllers
            .map_input(ControllerInput {
                address: &address,
                value: 127.0,
                velocity: None,
                timestamp: MediaTimestamp::new(3),
            })
            .unwrap()
            .is_empty()
    );

    controllers
        .disconnect("deck", MediaTimestamp::new(4))
        .unwrap();
    assert!(matches!(
        controllers.map_input(ControllerInput {
            address: &address,
            value: 127.0,
            velocity: None,
            timestamp: MediaTimestamp::new(5),
        }),
        Err(ControllerError::DeviceDisconnected(_))
    ));
    assert_eq!(
        controllers
            .connect("deck", MediaTimestamp::new(6))
            .unwrap()
            .generation,
        2
    );
    assert_eq!(controllers.mappings().len(), 1);
    assert_eq!(
        controllers
            .map_input(ControllerInput {
                address: &address,
                value: 127.0,
                velocity: None,
                timestamp: MediaTimestamp::new(7),
            })
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn controller_learn_and_activator_reconnect_are_state_only() {
    let mut controllers = ControllerManager::default();
    controllers
        .connect("surface", MediaTimestamp::new(0))
        .unwrap();
    controllers
        .begin_learn(LearnRequest {
            mapping_id: "fader".into(),
            target: "audio.gain".into(),
            input_range: ValueRange::new(0.0, 1.0).unwrap(),
            output_range: ValueRange::new(-60.0, 12.0).unwrap(),
            velocity_range: None,
            mode: ControlMode::Continuous,
        })
        .unwrap();
    let address = ControlAddress::new("surface", "fader-1");
    let learned_intent = controllers
        .map_input(ControllerInput {
            address: &address,
            value: 0.5,
            velocity: None,
            timestamp: MediaTimestamp::new(1),
        })
        .unwrap();
    assert_eq!(controllers.take_learned().unwrap().address, address);
    let CommandIntent::Continuous { command, .. } = &learned_intent[0] else {
        panic!("fader must be coalescible");
    };
    assert_close(command.value, -24.0);

    let source = input(1);
    let mut activators = ActivatorEngine::default();
    activators.add(ActivatorMapping {
        address: ControlAddress::new("surface", "program-light"),
        rule: ActivatorRule::Program(source),
        on_value: 1.0,
        off_value: 0.0,
    });
    let tally = TallySnapshot {
        program: Some(source),
        ..TallySnapshot::default()
    };
    assert_close(activators.derive(&tally)[0].value, 1.0);
    assert!(activators.derive(&tally).is_empty());
    activators.reconnect("surface");
    assert_close(activators.derive(&tally)[0].value, 1.0);
}

#[test]
fn continuous_intents_coalesce_without_absorbing_discrete_commands() {
    let mut buffer = IntentBuffer::default();
    buffer.push(CommandIntent::continuous("fader-1", TestCommand::Value(10)));
    buffer.push(CommandIntent::discrete(TestCommand::Named("cut")));
    buffer.push(CommandIntent::continuous("fader-1", TestCommand::Value(20)));
    buffer.push(CommandIntent::continuous("fader-2", TestCommand::Value(30)));

    assert_eq!(
        buffer.drain_discrete(),
        [CommandIntent::discrete(TestCommand::Named("cut"))]
    );
    assert_eq!(
        buffer.drain_continuous(),
        [
            CommandIntent::continuous("fader-1", TestCommand::Value(20)),
            CommandIntent::continuous("fader-2", TestCommand::Value(30)),
        ]
    );

    buffer.push(CommandIntent::continuous("fader-1", TestCommand::Value(40)));
    buffer.push(CommandIntent::commit_continuous(
        "fader-1",
        TestCommand::Value(50),
    ));
    assert_eq!(buffer.continuous_len(), 0);
    assert!(matches!(
        buffer.pop_discrete(),
        Some(CommandIntent::CommitContinuous {
            command: TestCommand::Value(50),
            ..
        })
    ));
}
