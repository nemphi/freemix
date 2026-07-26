use super::*;
use crate::{CommandId, Deadline, FieldIssue, Transaction};

#[derive(Clone, Debug, Eq, PartialEq)]
struct State {
    value: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Event {
    Changed(i32),
    Audited,
}

struct SetValue(i32);

impl Mutation<State, Event, i32> for SetValue {
    fn apply(self, state: &mut State, events: &mut Vec<Event>) -> Result<i32, Rejection> {
        state.value = self.0;
        events.push(Event::Changed(self.0));
        events.push(Event::Audited);
        Ok(self.0)
    }
}

enum BatchCommand {
    Set(i32),
    FailAfterSetting(i32),
    QuietSet(i32),
}

struct BatchMutation(BatchCommand);

impl Mutation<State, Event, i32> for BatchMutation {
    fn apply(self, state: &mut State, events: &mut Vec<Event>) -> Result<i32, Rejection> {
        match self.0 {
            BatchCommand::Set(value) => {
                state.value = value;
                events.push(Event::Changed(value));
                events.push(Event::Audited);
                Ok(value)
            }
            BatchCommand::FailAfterSetting(value) => {
                state.value = value;
                events.push(Event::Changed(value));
                Err(Rejection::new(
                    RejectionCode::Conflict,
                    "transaction command failed",
                ))
            }
            BatchCommand::QuietSet(value) => {
                state.value = value;
                Ok(value)
            }
        }
    }
}

fn prepare_batch(state: &State, command: BatchCommand) -> Result<BatchMutation, Rejection> {
    if state.value == i32::MIN {
        Err(Rejection::new(
            RejectionCode::InvalidCommand,
            "minimum value cannot be changed",
        ))
    } else {
        Ok(BatchMutation(command))
    }
}

fn transaction(
    key: &str,
    commands: impl IntoIterator<Item = BatchCommand>,
) -> CommandEnvelope<Transaction<BatchCommand>> {
    CommandEnvelope::new(
        format!("transaction-{key}"),
        key,
        Transaction::new(commands).unwrap(),
    )
}

fn envelope(key: &str, value: i32) -> CommandEnvelope<i32> {
    CommandEnvelope::new(format!("command-{key}"), key, value)
}

fn set_value(state: &State, value: i32) -> Result<SetValue, Rejection> {
    if state.value == value {
        Err(Rejection::new(
            RejectionCode::Conflict,
            "value is already current",
        ))
    } else {
        Ok(SetValue(value))
    }
}

#[test]
fn accepted_commands_advance_revision_and_order_events() {
    let mut commands = CommandState::new(State { value: 0 }, StateEpoch::new(7));

    let first = commands.apply(envelope("one", 10), 0, set_value);
    let second = commands.apply(envelope("two", 20), 0, set_value);

    assert_eq!(commands.state().value, 20);
    assert_eq!(commands.revision(), Revision::new(2));
    assert_eq!(first.receipt.accepted().unwrap().revision, Revision::new(1));
    assert_eq!(
        second.receipt.accepted().unwrap().revision,
        Revision::new(2)
    );
    assert_eq!(
        first
            .events
            .iter()
            .map(|event| (event.sequence.get(), event.revision.get()))
            .collect::<Vec<_>>(),
        vec![(1, 1), (2, 1)]
    );
    assert_eq!(
        second
            .events
            .iter()
            .map(|event| (event.sequence.get(), event.revision.get()))
            .collect::<Vec<_>>(),
        vec![(3, 2), (4, 2)]
    );
    assert!(
        second
            .events
            .iter()
            .all(|event| event.state_epoch == StateEpoch::new(7))
    );
}

#[test]
fn duplicate_key_replays_original_receipt_without_running_mutation() {
    let mut commands = CommandState::new(State { value: 0 }, StateEpoch::new(1));

    let original = commands.apply(envelope("same", 10), 0, set_value);
    let duplicate = commands.apply(
        envelope("same", 99),
        0,
        |_: &State, _: i32| -> Result<SetValue, Rejection> {
            panic!("duplicate command must not be prepared")
        },
    );

    assert!(duplicate.replayed);
    assert_eq!(duplicate.receipt, original.receipt);
    assert!(duplicate.events.is_empty());
    assert_eq!(commands.state().value, 10);
    assert_eq!(commands.revision(), Revision::new(1));
}

#[test]
fn rejected_result_is_also_replayed_by_idempotency_key() {
    let mut commands = CommandState::<_, i32>::new(State { value: 0 }, StateEpoch::new(1));
    let invalid = Rejection::new(RejectionCode::InvalidCommand, "bad value")
        .with_field(FieldIssue::new("value", "out_of_range", "must be positive"));

    let first: ApplyOutcome<i32, Event> =
        commands.apply(envelope("invalid", -1), 0, |_: &State, _: i32| {
            Err::<SetValue, _>(invalid)
        });
    let replay: ApplyOutcome<i32, Event> = commands.apply(
        envelope("invalid", 1),
        0,
        |_: &State, _: i32| -> Result<SetValue, Rejection> {
            panic!("duplicate rejection must not be prepared")
        },
    );

    assert_eq!(replay.receipt, first.receipt);
    assert!(replay.replayed);
    assert_eq!(commands.revision(), Revision::default());
}

#[test]
fn optimistic_revision_conflict_does_not_run_or_advance() {
    let mut commands = CommandState::<_, i32>::new(State { value: 2 }, StateEpoch::new(1));
    let stale = envelope("stale", 3).expecting(Revision::new(9));

    let outcome: ApplyOutcome<i32, Event> =
        commands.apply(stale, 0, |_, _| -> Result<SetValue, Rejection> {
            panic!("revision conflict must be checked before preparation")
        });

    let rejection = outcome.receipt.rejected().unwrap();
    assert_eq!(rejection.rejection.code, RejectionCode::RevisionConflict);
    assert!(rejection.rejection.retryable);
    assert_eq!(rejection.current_revision, Revision::default());
    assert_eq!(commands.state().value, 2);
}

#[test]
fn failed_transaction_rolls_back_state_events_and_revision() {
    let mut commands = CommandState::<_, i32>::new(State { value: 5 }, StateEpoch::new(1));
    let failure = Rejection::new(RejectionCode::Conflict, "second operation failed");

    let outcome: ApplyOutcome<i32, Event> = commands.apply(envelope("atomic", 8), 0, |_, value| {
        Ok(
            move |state: &mut State, events: &mut Vec<Event>| -> Result<i32, Rejection> {
                state.value = value;
                events.push(Event::Changed(value));
                Err(failure)
            },
        )
    });

    assert_eq!(commands.state().value, 5);
    assert_eq!(commands.revision(), Revision::default());
    assert_eq!(commands.event_sequence(), EventSequence::default());
    assert!(outcome.events.is_empty());
    assert_eq!(
        outcome.receipt.rejected().unwrap().rejection.code,
        RejectionCode::Conflict
    );
}

#[test]
fn expired_deadline_is_rejected_before_preparation() {
    let mut commands = CommandState::<_, i32>::new(State { value: 0 }, StateEpoch::new(1));
    let expired = envelope("late", 1).with_deadline(Deadline::from_millis(10));

    let outcome: ApplyOutcome<i32, Event> =
        commands.apply(expired, 11, |_, _| -> Result<SetValue, Rejection> {
            panic!("expired command must not be prepared")
        });

    assert_eq!(
        outcome.receipt.rejected().unwrap().rejection.code,
        RejectionCode::DeadlineExceeded
    );
}

#[test]
fn duplicate_receipt_keeps_original_command_id() {
    let mut commands = CommandState::new(State { value: 0 }, StateEpoch::new(1));
    let original = commands.apply(envelope("key", 1), 0, set_value);
    let duplicate_envelope = CommandEnvelope::new("different-id", "key", 2);
    let duplicate = commands.apply(duplicate_envelope, 0, set_value);

    assert_eq!(
        original.receipt.command_id(),
        &CommandId::new("command-key")
    );
    assert_eq!(
        duplicate.receipt.command_id(),
        original.receipt.command_id()
    );
}

#[test]
fn restored_receipt_prevents_reapplying_an_action_after_restart() {
    let receipt = CommandReceipt::Accepted {
        command_id: CommandId::new("before-restart"),
        acceptance: AcceptedReceipt {
            revision: Revision::new(8),
            result: 17,
        },
    };
    let mut commands = CommandState::restore_with_receipts(
        State { value: 17 },
        StateEpoch::new(2),
        Revision::new(8),
        EventSequence::new(14),
        [(IdempotencyKey::new("durable-key"), receipt.clone())],
    );

    let replay = commands.apply(
        CommandEnvelope::new("retry", "durable-key", 99),
        0,
        |_: &State, _: i32| -> Result<SetValue, Rejection> {
            panic!("a restored receipt must suppress the action")
        },
    );

    assert!(replay.replayed);
    assert_eq!(replay.receipt, receipt);
    assert_eq!(commands.state().value, 17);
    assert_eq!(commands.revision(), Revision::new(8));
}

#[test]
fn later_transaction_failure_rolls_back_every_mutation() {
    let mut commands = CommandState::<_, Vec<i32>>::new(State { value: 5 }, StateEpoch::new(1));
    let prepared = commands
        .prepare_transaction(
            transaction(
                "rollback",
                [BatchCommand::Set(8), BatchCommand::FailAfterSetting(13)],
            ),
            0,
            prepare_batch,
        )
        .prepared()
        .unwrap();

    assert_eq!(prepared.draft().value, 5);
    assert!(prepared.events().is_empty());
    assert_eq!(
        prepared.receipt().rejected().unwrap().rejection.code,
        RejectionCode::Conflict
    );

    let outcome = commands.commit(prepared).unwrap();
    assert!(outcome.events.is_empty());
    assert_eq!(commands.state().value, 5);
    assert_eq!(commands.revision(), Revision::default());
    assert_eq!(commands.event_sequence(), EventSequence::default());
}

#[test]
fn aborting_prepared_work_leaves_authority_and_receipts_untouched() {
    let commands = CommandState::new(State { value: 1 }, StateEpoch::new(4));
    let key = IdempotencyKey::new("abort");
    let prepared = commands
        .prepare(envelope("abort", 9), 0, set_value)
        .prepared()
        .unwrap();

    assert_eq!(prepared.draft().value, 9);
    assert_eq!(prepared.events().len(), 2);
    assert_eq!(commands.state().value, 1);
    assert_eq!(commands.revision(), Revision::default());
    assert_eq!(commands.event_sequence(), EventSequence::default());
    assert!(commands.receipt(&key).is_none());

    prepared.abort();

    assert_eq!(commands.state().value, 1);
    assert_eq!(commands.revision(), Revision::default());
    assert_eq!(commands.event_sequence(), EventSequence::default());
    assert!(commands.receipt(&key).is_none());
}

#[test]
fn prepared_work_only_becomes_authoritative_at_commit() {
    let mut commands = CommandState::new(State { value: 0 }, StateEpoch::new(3));
    let key = IdempotencyKey::new("commit");
    let prepared = commands
        .prepare(envelope("commit", 7), 0, set_value)
        .prepared()
        .unwrap();

    assert_eq!(prepared.base_revision(), Revision::default());
    assert_eq!(prepared.base_event_sequence(), EventSequence::default());
    assert_eq!(
        prepared.receipt().accepted().unwrap().revision,
        Revision::new(1)
    );
    assert_eq!(commands.state().value, 0);
    assert!(commands.receipt(&key).is_none());

    let outcome = commands.commit(prepared).unwrap();

    assert!(!outcome.replayed);
    assert_eq!(commands.state().value, 7);
    assert_eq!(commands.revision(), Revision::new(1));
    assert_eq!(commands.event_sequence(), EventSequence::new(2));
    assert_eq!(commands.receipt(&key), Some(&outcome.receipt));
}

#[test]
fn concurrent_prepared_work_rejects_the_stale_commit() {
    let mut commands = CommandState::new(State { value: 0 }, StateEpoch::new(1));
    let first = commands
        .prepare(envelope("first", 1), 0, set_value)
        .prepared()
        .unwrap();
    let stale = commands
        .prepare(envelope("stale-prepare", 2), 0, set_value)
        .prepared()
        .unwrap();

    commands.commit(first).unwrap();
    let error = commands.commit(stale).unwrap_err();

    assert!(matches!(
        error,
        CommitError::StaleAuthority {
            prepared_revision,
            current_revision,
            ..
        } if prepared_revision == Revision::default()
            && current_revision == Revision::new(1)
    ));
    assert_eq!(commands.state().value, 1);
    assert_eq!(commands.event_sequence(), EventSequence::new(2));
    assert!(
        commands
            .receipt(&IdempotencyKey::new("stale-prepare"))
            .is_none()
    );
}

#[test]
fn transaction_events_keep_command_order_under_one_revision() {
    let mut commands = CommandState::<_, Vec<i32>>::new(State { value: 0 }, StateEpoch::new(9));
    let prepared = commands
        .prepare_transaction(
            transaction("ordered", [BatchCommand::Set(3), BatchCommand::Set(6)]),
            0,
            prepare_batch,
        )
        .prepared()
        .unwrap();

    assert_eq!(prepared.draft().value, 6);
    assert_eq!(
        prepared
            .events()
            .iter()
            .map(|event| (&event.payload, event.sequence, event.revision))
            .collect::<Vec<_>>(),
        vec![
            (&Event::Changed(3), EventSequence::new(1), Revision::new(1)),
            (&Event::Audited, EventSequence::new(2), Revision::new(1)),
            (&Event::Changed(6), EventSequence::new(3), Revision::new(1)),
            (&Event::Audited, EventSequence::new(4), Revision::new(1)),
        ]
    );
    assert_eq!(prepared.receipt().accepted().unwrap().result, vec![3, 6]);

    let outcome = commands.commit(prepared).unwrap();
    assert_eq!(outcome.events.len(), 4);
    assert_eq!(commands.revision(), Revision::new(1));
    assert_eq!(commands.event_sequence(), EventSequence::new(4));
}

#[test]
fn transaction_can_accept_with_no_events() {
    let mut commands = CommandState::<_, Vec<i32>>::new(State { value: 0 }, StateEpoch::new(1));
    let prepared = commands
        .prepare_transaction(
            transaction("quiet", [BatchCommand::QuietSet(11)]),
            0,
            prepare_batch,
        )
        .prepared()
        .unwrap();

    assert!(prepared.events().is_empty());
    commands.commit(prepared).unwrap();
    assert_eq!(commands.state().value, 11);
    assert_eq!(commands.revision(), Revision::new(1));
    assert_eq!(commands.event_sequence(), EventSequence::default());
}

#[test]
fn duplicate_transaction_replays_one_receipt_without_preparing_items() {
    let mut commands = CommandState::<_, Vec<i32>>::new(State { value: 0 }, StateEpoch::new(1));
    let original = commands
        .commit(
            commands
                .prepare_transaction(
                    transaction("duplicate-batch", [BatchCommand::QuietSet(4)]),
                    0,
                    prepare_batch,
                )
                .prepared()
                .unwrap(),
        )
        .unwrap();

    let duplicate = commands.prepare_transaction(
        transaction("duplicate-batch", [BatchCommand::QuietSet(99)]),
        0,
        |_, _| -> Result<BatchMutation, Rejection> {
            panic!("duplicate transaction items must not be prepared")
        },
    );
    let replay = duplicate.replayed().unwrap();

    assert!(replay.replayed);
    assert_eq!(replay.receipt, original.receipt);
    assert!(replay.events.is_empty());
    assert_eq!(commands.state().value, 4);
    assert_eq!(commands.revision(), Revision::new(1));
}

#[test]
fn exhausted_counters_prepare_rejected_receipts_without_mutation() {
    let mut revision_exhausted = CommandState::<_, i32>::restore(
        State { value: 1 },
        StateEpoch::new(1),
        Revision::new(u64::MAX),
        EventSequence::default(),
    );
    let prepared = revision_exhausted
        .prepare(envelope("revision-full", 2), 0, set_value)
        .prepared()
        .unwrap();
    assert_eq!(
        prepared.receipt().rejected().unwrap().rejection.code,
        RejectionCode::ResourceExhausted
    );
    revision_exhausted.commit(prepared).unwrap();
    assert_eq!(revision_exhausted.state().value, 1);
    assert_eq!(revision_exhausted.revision(), Revision::new(u64::MAX));

    let mut sequence_exhausted = CommandState::<_, i32>::restore(
        State { value: 1 },
        StateEpoch::new(1),
        Revision::default(),
        EventSequence::new(u64::MAX),
    );
    let prepared = sequence_exhausted
        .prepare(envelope("sequence-full", 2), 0, set_value)
        .prepared()
        .unwrap();
    assert_eq!(
        prepared.receipt().rejected().unwrap().rejection.code,
        RejectionCode::ResourceExhausted
    );
    sequence_exhausted.commit(prepared).unwrap();
    assert_eq!(sequence_exhausted.state().value, 1);
    assert_eq!(sequence_exhausted.revision(), Revision::default());
    assert_eq!(
        sequence_exhausted.event_sequence(),
        EventSequence::new(u64::MAX)
    );
}
