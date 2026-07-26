//! Transport-neutral command execution primitives.
//!
//! The crate separates durable command acceptance from runtime realization.
//! [`CommandState`] is a single-writer, idempotent state machine for durable
//! mutations. [`Lifecycle`] tracks what happens after a revision is accepted.

mod counters;
mod envelope;
mod lifecycle;
mod machine;
mod transaction;

pub use counters::{
    CounterOverflow, EventSequence, Revision, RuntimeGeneration, RuntimeSequence, StateEpoch,
};
pub use envelope::{
    AcceptedReceipt, CommandEnvelope, CommandId, CommandReceipt, Deadline, FieldIssue,
    IdempotencyKey, RejectedReceipt, Rejection, RejectionCode,
};
pub use lifecycle::{
    FailureDisposition, Lifecycle, LifecycleError, LifecycleEvent, LifecyclePhase, LifecycleRecord,
    ScheduledDomain,
};
pub use machine::{
    ApplyOutcome, CommandState, CommitCounter, CommitError, DurableEvent, Mutation, PrepareOutcome,
    PreparedCommit,
};
pub use transaction::{MAX_TRANSACTION_COMMANDS, Transaction, TransactionError};
