//! Deterministic recorder coordination with crash-repairable segment files.
//!
//! The on-disk format is intentionally private. Segment and manifest records
//! are append-only, length-delimited, and protected by CRC32 checksums. Recovery
//! truncates an incomplete final record, but never hides checksum corruption.

mod coordinator;
mod format;
mod types;

pub use coordinator::{
    DurableWriter, ReconciliationFailure, ReconciliationReport, RecorderCoordinator,
    StdWriterFactory, WriterFactory, repair_recording,
};
pub use types::{
    ActionOutcome, ActionReceiptId, AppendError, AudioPacketMetadata, Discontinuity, EnqueueError,
    PacketCommon, QueueLimits, RecordEvent, RecorderConfig, RecorderError, RecorderId,
    RecorderSnapshot, RecorderState, RecoveredSegment, RecoveryReport, SegmentPolicy,
    TimedPacketMetadata, VideoPacketMetadata,
};

#[cfg(test)]
mod tests;
