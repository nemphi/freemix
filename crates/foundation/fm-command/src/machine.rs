use crate::{
    AcceptedReceipt, CommandEnvelope, CommandReceipt, EventSequence, IdempotencyKey,
    RejectedReceipt, Rejection, RejectionCode, Revision, StateEpoch, Transaction,
};
use core::fmt;
use std::collections::HashMap;

pub trait Mutation<S, E, R> {
    /// Applies this mutation to a transaction draft.
    ///
    /// # Errors
    ///
    /// Returns a rejection when the complete mutation must be rolled back.
    fn apply(self, state: &mut S, events: &mut Vec<E>) -> Result<R, Rejection>;
}

impl<S, E, R, F> Mutation<S, E, R> for F
where
    F: FnOnce(&mut S, &mut Vec<E>) -> Result<R, Rejection>,
{
    fn apply(self, state: &mut S, events: &mut Vec<E>) -> Result<R, Rejection> {
        self(state, events)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableEvent<E> {
    pub state_epoch: StateEpoch,
    pub sequence: EventSequence,
    pub revision: Revision,
    pub payload: E,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyOutcome<R, E> {
    pub receipt: CommandReceipt<R>,
    pub events: Vec<DurableEvent<E>>,
    pub replayed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitCounter {
    Revision,
    EventSequence,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommitError {
    StaleAuthority {
        prepared_revision: Revision,
        current_revision: Revision,
        prepared_event_sequence: EventSequence,
        current_event_sequence: EventSequence,
    },
    CounterExhausted(CommitCounter),
}

impl fmt::Display for CommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleAuthority { .. } => {
                formatter.write_str("prepared commit was based on stale authority")
            }
            Self::CounterExhausted(CommitCounter::Revision) => {
                formatter.write_str("durable revision space exhausted")
            }
            Self::CounterExhausted(CommitCounter::EventSequence) => {
                formatter.write_str("durable event sequence space exhausted")
            }
        }
    }
}

impl std::error::Error for CommitError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreparedKind {
    Accepted {
        revision: Revision,
        event_sequence: EventSequence,
    },
    Rejected,
}

/// A side-effect-free command decision ready for durable persistence and commit.
///
/// Dropping or aborting this value does not modify its originating
/// [`CommandState`].
#[derive(Debug)]
pub struct PreparedCommit<S, R, E> {
    draft: S,
    events: Vec<DurableEvent<E>>,
    receipt: CommandReceipt<R>,
    idempotency_key: IdempotencyKey,
    state_epoch: StateEpoch,
    base_revision: Revision,
    base_event_sequence: EventSequence,
    kind: PreparedKind,
}

impl<S, R, E> PreparedCommit<S, R, E> {
    #[must_use]
    pub const fn draft(&self) -> &S {
        &self.draft
    }

    #[must_use]
    pub fn events(&self) -> &[DurableEvent<E>] {
        &self.events
    }

    #[must_use]
    pub const fn receipt(&self) -> &CommandReceipt<R> {
        &self.receipt
    }

    #[must_use]
    pub const fn idempotency_key(&self) -> &IdempotencyKey {
        &self.idempotency_key
    }

    #[must_use]
    pub const fn state_epoch(&self) -> StateEpoch {
        self.state_epoch
    }

    #[must_use]
    pub const fn base_revision(&self) -> Revision {
        self.base_revision
    }

    #[must_use]
    pub const fn base_event_sequence(&self) -> EventSequence {
        self.base_event_sequence
    }

    /// Consumes this prepared value without changing command state.
    pub fn abort(self) {}
}

#[derive(Debug)]
pub enum PrepareOutcome<S, R, E> {
    Prepared(PreparedCommit<S, R, E>),
    Replayed(ApplyOutcome<R, E>),
}

impl<S, R, E> PrepareOutcome<S, R, E> {
    #[must_use]
    pub fn prepared(self) -> Option<PreparedCommit<S, R, E>> {
        match self {
            Self::Prepared(prepared) => Some(prepared),
            Self::Replayed(_) => None,
        }
    }

    #[must_use]
    pub fn replayed(self) -> Option<ApplyOutcome<R, E>> {
        match self {
            Self::Prepared(_) => None,
            Self::Replayed(outcome) => Some(outcome),
        }
    }
}

/// A single-writer durable command state machine.
///
/// State is cloned into a transaction draft. The draft, revision, event
/// sequence, and emitted events commit together only when the mutation
/// succeeds.
#[derive(Clone, Debug)]
pub struct CommandState<S, R> {
    state: S,
    state_epoch: StateEpoch,
    revision: Revision,
    event_sequence: EventSequence,
    receipts: HashMap<IdempotencyKey, CommandReceipt<R>>,
}

impl<S, R> CommandState<S, R> {
    #[must_use]
    pub fn new(state: S, state_epoch: StateEpoch) -> Self {
        Self {
            state,
            state_epoch,
            revision: Revision::default(),
            event_sequence: EventSequence::default(),
            receipts: HashMap::new(),
        }
    }

    #[must_use]
    pub fn restore(
        state: S,
        state_epoch: StateEpoch,
        revision: Revision,
        event_sequence: EventSequence,
    ) -> Self {
        Self {
            state,
            state_epoch,
            revision,
            event_sequence,
            receipts: HashMap::new(),
        }
    }

    #[must_use]
    pub fn restore_with_receipts(
        state: S,
        state_epoch: StateEpoch,
        revision: Revision,
        event_sequence: EventSequence,
        receipts: impl IntoIterator<Item = (IdempotencyKey, CommandReceipt<R>)>,
    ) -> Self {
        Self {
            state,
            state_epoch,
            revision,
            event_sequence,
            receipts: receipts.into_iter().collect(),
        }
    }

    #[must_use]
    pub const fn state(&self) -> &S {
        &self.state
    }

    #[must_use]
    pub const fn state_epoch(&self) -> StateEpoch {
        self.state_epoch
    }

    #[must_use]
    pub const fn revision(&self) -> Revision {
        self.revision
    }

    #[must_use]
    pub const fn event_sequence(&self) -> EventSequence {
        self.event_sequence
    }

    #[must_use]
    pub fn receipt(&self, key: &IdempotencyKey) -> Option<&CommandReceipt<R>> {
        self.receipts.get(key)
    }
}

impl<S: Clone, R: Clone> CommandState<S, R> {
    /// Validates and applies a command to an isolated draft without committing it.
    pub fn prepare<C, E, M, P>(
        &self,
        envelope: CommandEnvelope<C>,
        now_millis: u64,
        prepare: P,
    ) -> PrepareOutcome<S, R, E>
    where
        M: Mutation<S, E, R>,
        P: FnOnce(&S, C) -> Result<M, Rejection>,
    {
        if let Some(receipt) = self.receipts.get(&envelope.idempotency_key) {
            return PrepareOutcome::Replayed(ApplyOutcome {
                receipt: receipt.clone(),
                events: Vec::new(),
                replayed: true,
            });
        }

        let CommandEnvelope {
            id,
            idempotency_key,
            expected_revision,
            deadline,
            command,
        } = envelope;

        if deadline.is_some_and(|value| value.is_exceeded_at(now_millis)) {
            return PrepareOutcome::Prepared(self.prepared_rejection(
                id,
                idempotency_key,
                Rejection::new(RejectionCode::DeadlineExceeded, "command deadline exceeded"),
            ));
        }

        if expected_revision.is_some_and(|value| value != self.revision) {
            return PrepareOutcome::Prepared(
                self.prepared_rejection(
                    id,
                    idempotency_key,
                    Rejection::new(
                        RejectionCode::RevisionConflict,
                        "expected revision does not match current revision",
                    )
                    .retryable(true),
                ),
            );
        }

        let mutation = match prepare(&self.state, command) {
            Ok(mutation) => mutation,
            Err(rejection) => {
                return PrepareOutcome::Prepared(self.prepared_rejection(
                    id,
                    idempotency_key,
                    rejection,
                ));
            }
        };

        PrepareOutcome::Prepared(self.prepared_mutation(id, idempotency_key, mutation))
    }

    fn prepared_mutation<E, M>(
        &self,
        command_id: crate::CommandId,
        idempotency_key: IdempotencyKey,
        mutation: M,
    ) -> PreparedCommit<S, R, E>
    where
        M: Mutation<S, E, R>,
    {
        let mut draft = self.state.clone();
        let mut payloads = Vec::new();
        let result = match mutation.apply(&mut draft, &mut payloads) {
            Ok(result) => result,
            Err(rejection) => {
                return self.prepared_rejection(command_id, idempotency_key, rejection);
            }
        };

        let Ok(revision) = self.revision.checked_next() else {
            return self.prepared_rejection(
                command_id,
                idempotency_key,
                Rejection::new(
                    RejectionCode::ResourceExhausted,
                    "durable revision space exhausted",
                ),
            );
        };

        let mut sequence = self.event_sequence;
        let mut events = Vec::with_capacity(payloads.len());
        for payload in payloads {
            let Ok(next_sequence) = sequence.checked_next() else {
                return self.prepared_rejection(
                    command_id,
                    idempotency_key,
                    Rejection::new(
                        RejectionCode::ResourceExhausted,
                        "durable event sequence space exhausted",
                    ),
                );
            };
            sequence = next_sequence;
            events.push(DurableEvent {
                state_epoch: self.state_epoch,
                sequence,
                revision,
                payload,
            });
        }

        PreparedCommit {
            draft,
            events,
            receipt: CommandReceipt::Accepted {
                command_id,
                acceptance: AcceptedReceipt { revision, result },
            },
            idempotency_key,
            state_epoch: self.state_epoch,
            base_revision: self.revision,
            base_event_sequence: self.event_sequence,
            kind: PreparedKind::Accepted {
                revision,
                event_sequence: sequence,
            },
        }
    }

    /// Commits a prepared decision if its authority snapshot is still current.
    ///
    /// # Errors
    ///
    /// Returns [`CommitError::StaleAuthority`] after another accepted commit, or
    /// [`CommitError::CounterExhausted`] if a durable counter cannot advance.
    pub fn commit<E>(
        &mut self,
        prepared: PreparedCommit<S, R, E>,
    ) -> Result<ApplyOutcome<R, E>, CommitError> {
        if let Some(receipt) = self.receipts.get(&prepared.idempotency_key) {
            return Ok(ApplyOutcome {
                receipt: receipt.clone(),
                events: Vec::new(),
                replayed: true,
            });
        }

        if prepared.state_epoch != self.state_epoch
            || prepared.base_revision != self.revision
            || prepared.base_event_sequence != self.event_sequence
        {
            return Err(CommitError::StaleAuthority {
                prepared_revision: prepared.base_revision,
                current_revision: self.revision,
                prepared_event_sequence: prepared.base_event_sequence,
                current_event_sequence: self.event_sequence,
            });
        }

        if let PreparedKind::Accepted {
            revision,
            event_sequence,
        } = prepared.kind
        {
            let checked_revision = self
                .revision
                .checked_next()
                .map_err(|_| CommitError::CounterExhausted(CommitCounter::Revision))?;
            if checked_revision != revision {
                return Err(CommitError::StaleAuthority {
                    prepared_revision: revision,
                    current_revision: self.revision,
                    prepared_event_sequence: event_sequence,
                    current_event_sequence: self.event_sequence,
                });
            }

            let mut checked_sequence = self.event_sequence;
            for _ in &prepared.events {
                checked_sequence = checked_sequence
                    .checked_next()
                    .map_err(|_| CommitError::CounterExhausted(CommitCounter::EventSequence))?;
            }
            if checked_sequence != event_sequence {
                return Err(CommitError::StaleAuthority {
                    prepared_revision: revision,
                    current_revision: self.revision,
                    prepared_event_sequence: event_sequence,
                    current_event_sequence: self.event_sequence,
                });
            }
        }

        Ok(self.commit_current(prepared))
    }

    /// Validates and atomically applies an enveloped command.
    ///
    /// Domain errors are represented in the returned rejected receipt.
    pub fn apply<C, E, M, P>(
        &mut self,
        envelope: CommandEnvelope<C>,
        now_millis: u64,
        prepare: P,
    ) -> ApplyOutcome<R, E>
    where
        M: Mutation<S, E, R>,
        P: FnOnce(&S, C) -> Result<M, Rejection>,
    {
        match self.prepare(envelope, now_millis, prepare) {
            PrepareOutcome::Replayed(outcome) => outcome,
            PrepareOutcome::Prepared(prepared) => self.commit_current(prepared),
        }
    }

    fn commit_current<E>(&mut self, prepared: PreparedCommit<S, R, E>) -> ApplyOutcome<R, E> {
        let PreparedCommit {
            draft,
            events,
            receipt,
            idempotency_key,
            kind,
            ..
        } = prepared;

        if let PreparedKind::Accepted {
            revision,
            event_sequence,
        } = kind
        {
            self.state = draft;
            self.revision = revision;
            self.event_sequence = event_sequence;
        }

        self.receipts.insert(idempotency_key, receipt.clone());
        ApplyOutcome {
            receipt,
            events,
            replayed: false,
        }
    }

    fn prepared_rejection<E>(
        &self,
        command_id: crate::CommandId,
        idempotency_key: IdempotencyKey,
        rejection: Rejection,
    ) -> PreparedCommit<S, R, E> {
        PreparedCommit {
            draft: self.state.clone(),
            events: Vec::new(),
            receipt: CommandReceipt::Rejected {
                command_id,
                rejection: RejectedReceipt {
                    rejection,
                    current_revision: self.revision,
                },
            },
            idempotency_key,
            state_epoch: self.state_epoch,
            base_revision: self.revision,
            base_event_sequence: self.event_sequence,
            kind: PreparedKind::Rejected,
        }
    }
}

impl<S: Clone, R: Clone> CommandState<S, Vec<R>> {
    /// Prepares a transaction by applying each command to the same isolated draft.
    ///
    /// Commands and their events retain transaction order. Any rejection discards
    /// every draft mutation, event, and result from the batch.
    pub fn prepare_transaction<C, E, M, P>(
        &self,
        envelope: CommandEnvelope<Transaction<C>>,
        now_millis: u64,
        mut prepare: P,
    ) -> PrepareOutcome<S, Vec<R>, E>
    where
        M: Mutation<S, E, R>,
        P: FnMut(&S, C) -> Result<M, Rejection>,
    {
        self.prepare(envelope, now_millis, move |_, transaction| {
            Ok(move |draft: &mut S, events: &mut Vec<E>| {
                let mut results = Vec::with_capacity(transaction.len());
                for command in transaction {
                    let mutation = prepare(draft, command)?;
                    results.push(mutation.apply(draft, events)?);
                }
                Ok(results)
            })
        })
    }
}

#[cfg(test)]
mod tests;
