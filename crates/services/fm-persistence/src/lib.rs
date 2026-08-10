//! Durable, engine-independent `FreeMix` project manifests.

mod asset;
mod journal;
mod json;
mod model;
mod store;

pub use asset::AssetResolveError;
pub use journal::{
    CompactionReport, JournalError, JournalScan, MAX_JOURNAL_RECORD_BYTES, MutationBatch,
};
pub use model::{
    CURRENT_SCHEMA_VERSION, FadeToBlackState, IdempotencyReceipt,
    MAX_OVERLAY_TRANSITION_DURATION_FRAMES, ManualTransitionKind, ManualTransitionState,
    OVERLAY_CHANNEL_COUNT, ProjectPosition, ProjectValidationError, ReceiptOutcome, ReferenceField,
    RuntimeFadeToBlack, RuntimeManualTransitions, RuntimeOverlayBorder, RuntimeOverlayChannel,
    RuntimeOverlayPosition, RuntimeOverlayTransition, RuntimeOverlays, RuntimeRouting,
    StoredProject,
};
pub use store::{MAX_MANIFEST_BYTES, ProjectStore, StoreError};
