//! Durable, engine-independent `FreeMix` project manifests.

mod asset;
mod journal;
mod json;
mod migration;
mod model;
mod store;

pub use asset::AssetResolveError;
pub use journal::{
    CompactionReport, JournalError, JournalScan, MAX_JOURNAL_RECORD_BYTES, MutationBatch,
};
pub use migration::MigrationReport;
pub use model::{
    CURRENT_SCHEMA_VERSION, IdempotencyReceipt, ManualTransitionKind, ManualTransitionState,
    ProjectPosition, ProjectRouting, ProjectValidationError, ReceiptOutcome, ReferenceField,
    RuntimeManualTransitions, RuntimeRouting, StoredProject,
};
pub use store::{MAX_MANIFEST_BYTES, ProjectStore, StoreError};
