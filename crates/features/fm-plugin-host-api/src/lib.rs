//! Engine-side plugin lifecycle and command-only mutation protocol.
//!
//! This crate deliberately models protocol and durable engine authority only.
//! Loading a plugin here means advancing the engine-side lifecycle; process and
//! runtime management belong to a host implementation.

mod exchange;
mod host;
mod snapshot;
mod types;

pub use exchange::{
    DataBatch, DataMessage, EventBatch, EventMessage, ExchangeError, HeartbeatMessage,
    MutationIntent, PluginToEngine, ProtocolLimits,
};
pub use fm_command::{
    ApplyOutcome, CommandEnvelope, CommandId, CommandReceipt, Deadline, DurableEvent,
    IdempotencyKey, Rejection, RejectionCode, Revision, StateEpoch,
};
pub use fm_plugin_api as plugin_api;
pub use host::{
    HostError, PluginCommand, PluginCommandResult, PluginEvent, PluginHost, PluginRecord,
};
pub use snapshot::{
    MigrationError, MigrationRequest, SnapshotMigrator, migrate_snapshot,
    validate_migration_response,
};
pub use types::{
    ApiCompatibility, ApiVersion, CapabilityDecision, CapabilityId, CrashReport, PluginId,
    PluginManifest, PluginState, QuarantineReason, StateSnapshot, StateVersion,
};
