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
    CapabilityReportMessage, CapabilityReportSummary, ClientHello, ClientType, CommandMessage,
    CommandPayload, CommandResult, DurableEvent, DurableEventBatch, DurableGap, EngineIdentity,
    ErrorMessage, EventCursor, EventMessage, EventPayload, FadeToBlackPosition, FadeToBlackState,
    FieldIssue, HandshakeOutcome, HandshakeRequest, HandshakeResponse, HeartbeatMessage,
    ManualTransitionKind, ManualTransitionPosition, ManualTransitionState, ManualTransitionStatus,
    ResumeCursor, Role, RuntimeDomainBoundary, RuntimeEventMessage, RuntimeFailureDisposition,
    RuntimeLifecycleEvent, ServerHello, ServerIdentity, SnapshotMessage, SnapshotReason,
    StructuredError, WireInputId, WireMessage, choose_handshake_outcome,
};
pub use version::{
    ALPHA_FADE_PROTOCOL_VERSION, BASE_PROTOCOL_VERSION, CURRENT_PROTOCOL_VERSION,
    FADE_TO_BLACK_PROTOCOL_VERSION, MANUAL_ALPHA_FADE_PROTOCOL_VERSION,
    MANUAL_TRANSITION_PROTOCOL_VERSION, NegotiationError, ProtocolVersion, SLIDE_PROTOCOL_VERSION,
    WIPE_PROTOCOL_VERSION, negotiate_version,
};
