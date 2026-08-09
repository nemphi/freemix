//! Transport-neutral wire DTOs and newline-delimited control codec.

mod codec;
mod dto;
mod version;

pub use codec::{
    CodecError, LineDecoder, MAX_BATCH_EVENTS, MAX_FIELD_NAME_BYTES, MAX_FIELD_VALUE_BYTES,
    MAX_FIELDS_PER_MESSAGE, MAX_LINE_BYTES, MAX_LIST_ITEMS, MAX_MESSAGES_PER_PUSH, decode_line,
    encode_line,
};
pub use dto::{
    CapabilityReportMessage, CapabilityReportSummary, ClientType, CommandMessage, CommandPayload,
    CommandResult, DurableEvent, DurableEventBatch, DurableGap, EngineIdentity, ErrorMessage,
    EventCursor, EventMessage, EventPayload, FadeToBlackPosition, FadeToBlackState, FieldIssue,
    HandshakeOutcome, HandshakeRequest, HandshakeResponse, HeartbeatMessage, InputAudioStripStatus,
    ManualTransitionKind, ManualTransitionPosition, ManualTransitionState, ManualTransitionStatus,
    OverlayBorderPreset, OverlayPositionPreset, OverlayStatus, OverlayTransitionKind, ResumeCursor,
    Role, RuntimeDomainBoundary, RuntimeEventMessage, RuntimeFailureDisposition,
    RuntimeLifecycleEvent, ServerHello, ServerIdentity, SnapshotMessage, SnapshotReason,
    StingerAudioPolicy, StingerMissingMediaFallback, StingerReadiness, StingerStatus,
    StructuredError, WireInputId, WireMessage, WireOutputId, WireOverlayChannelId,
    WireStingerSlotId, choose_handshake_outcome,
};
pub use version::{CURRENT_PROTOCOL_VERSION, ProtocolVersion};
