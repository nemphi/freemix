use core::{fmt, num::NonZeroU128};

use fm_command::{CommandEnvelope, Deadline, Revision, StateEpoch};
use fm_types::InputId;

use crate::{
    ALPHA_FADE_PROTOCOL_VERSION, BASE_PROTOCOL_VERSION, FADE_TO_BLACK_PROTOCOL_VERSION,
    MANUAL_ALPHA_FADE_PROTOCOL_VERSION, MANUAL_TRANSITION_PROTOCOL_VERSION, ProtocolVersion,
    SLIDE_PROTOCOL_VERSION, WIPE_PROTOCOL_VERSION,
};

/// Stable identity of one project's durable state on one server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerIdentity {
    pub engine_id: String,
    pub project_id: String,
    pub state_epoch: u64,
    pub log_id: String,
}

impl ServerIdentity {
    #[must_use]
    pub const fn domain_state_epoch(&self) -> StateEpoch {
        StateEpoch::new(self.state_epoch)
    }
}

/// The last durable revision completely applied by a client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResumeCursor {
    pub server: ServerIdentity,
    pub revision: u64,
}

impl ResumeCursor {
    #[must_use]
    pub const fn domain_revision(&self) -> Revision {
        Revision::new(self.revision)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineIdentity {
    pub engine_id: String,
    pub state_epoch: u64,
    pub log_id: String,
}

impl EngineIdentity {
    #[must_use]
    pub const fn domain_state_epoch(&self) -> StateEpoch {
        StateEpoch::new(self.state_epoch)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventCursor {
    pub engine: EngineIdentity,
    pub revision: u64,
}

impl EventCursor {
    #[must_use]
    pub const fn domain_revision(&self) -> Revision {
        Revision::new(self.revision)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientType {
    Studio,
    Web,
    Cli,
    Integration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Viewer,
    Graphics,
    Audio,
    Replay,
    Operator,
    Admin,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientHello {
    pub versions: Vec<ProtocolVersion>,
    pub build: String,
    pub client_type: ClientType,
    pub desired_role: Role,
    pub cached_cursor: Option<EventCursor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerHello {
    pub negotiated: ProtocolVersion,
    pub granted_role: Role,
    pub permissions: Vec<String>,
    pub capabilities_digest: String,
    pub engine: EngineIdentity,
    pub current_revision: u64,
    pub resume: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandshakeRequest {
    pub versions: Vec<ProtocolVersion>,
    pub build: String,
    pub client_type: ClientType,
    pub desired_role: Role,
    pub resume_cursor: Option<ResumeCursor>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotReason {
    NoCursor,
    IdentityChanged,
    CursorAhead,
    HistoryUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HandshakeOutcome {
    Snapshot { reason: SnapshotReason },
    Resume { cursor: ResumeCursor },
    Rejected { error: StructuredError },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandshakeResponse {
    pub negotiated: ProtocolVersion,
    pub granted_role: Role,
    pub permissions: Vec<String>,
    pub capabilities: CapabilityReportSummary,
    pub server: ServerIdentity,
    pub current_revision: u64,
    pub outcome: HandshakeOutcome,
}

/// Chooses snapshot or resume from the retained durable log range.
///
/// `available_from_revision` is the first revision still available for replay.
#[must_use]
pub fn choose_handshake_outcome(
    server: &ServerIdentity,
    current_revision: u64,
    available_from_revision: u64,
    cursor: Option<&ResumeCursor>,
) -> HandshakeOutcome {
    let Some(cursor) = cursor else {
        return HandshakeOutcome::Snapshot {
            reason: SnapshotReason::NoCursor,
        };
    };
    if cursor.server != *server {
        return HandshakeOutcome::Snapshot {
            reason: SnapshotReason::IdentityChanged,
        };
    }
    if cursor.revision > current_revision {
        return HandshakeOutcome::Snapshot {
            reason: SnapshotReason::CursorAhead,
        };
    }
    if cursor.revision.saturating_add(1) < available_from_revision {
        return HandshakeOutcome::Snapshot {
            reason: SnapshotReason::HistoryUnavailable,
        };
    }
    HandshakeOutcome::Resume {
        cursor: cursor.clone(),
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WireInputId(NonZeroU128);

impl WireInputId {
    #[must_use]
    pub const fn new(value: NonZeroU128) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> NonZeroU128 {
        self.0
    }

    #[must_use]
    pub const fn from_domain(value: InputId) -> Self {
        Self(value.get())
    }

    #[must_use]
    pub const fn to_domain(self) -> InputId {
        InputId::new(self.0)
    }
}

impl fmt::Display for WireInputId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManualTransitionKind {
    Fade,
    Wipe,
    AlphaFade,
}

/// Exact normalized manual-transition position expressed in basis points.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ManualTransitionPosition(u16);

impl ManualTransitionPosition {
    pub const MAX: u16 = 10_000;
    pub const START: Self = Self(0);
    pub const END: Self = Self(Self::MAX);

    #[must_use]
    pub const fn new(basis_points: u16) -> Option<Self> {
        if basis_points <= Self::MAX {
            Some(Self(basis_points))
        } else {
            None
        }
    }

    #[must_use]
    pub const fn basis_points(self) -> u16 {
        self.0
    }
}

/// Exact active manual-transition state at one frame boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManualTransitionState {
    pub kind: ManualTransitionKind,
    pub from: WireInputId,
    pub to: WireInputId,
    pub interval_start: ManualTransitionPosition,
    pub position: ManualTransitionPosition,
}

/// Versioned additive projection of manual-transition state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManualTransitionStatus {
    Inactive,
    Active(ManualTransitionState),
}

/// Exact fixed-rational FTB position; `0` is live and `u16::MAX` is black.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FadeToBlackPosition(u16);

impl FadeToBlackPosition {
    pub const LIVE: Self = Self(0);
    pub const BLACK: Self = Self(u16::MAX);
    pub const DENOMINATOR: u32 = u16::MAX as u32;

    #[must_use]
    pub const fn new(numerator: u16) -> Self {
        Self(numerator)
    }

    #[must_use]
    pub const fn numerator(self) -> u16 {
        self.0
    }
}

/// Desired or realized Fade-to-Black state at one frame boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FadeToBlackState {
    pub target_active: bool,
    pub position: FadeToBlackPosition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandPayload {
    SelectPreview { input: WireInputId },
    Cut,
    Fade { duration_frames: u32 },
    AlphaFade { duration_frames: u32 },
    Slide { duration_frames: u32 },
    Wipe { duration_frames: u32 },
    FadeToBlack { active: bool, duration_frames: u32 },
    StartManualTransition { kind: ManualTransitionKind },
    SetManualTransitionPosition { position: ManualTransitionPosition },
    CommitManualTransition,
    CancelManualTransition,
}

impl CommandPayload {
    #[must_use]
    pub const fn minimum_protocol_version(self) -> ProtocolVersion {
        match self {
            Self::SelectPreview { .. } | Self::Cut | Self::Fade { .. } => BASE_PROTOCOL_VERSION,
            Self::AlphaFade { .. } => ALPHA_FADE_PROTOCOL_VERSION,
            Self::Slide { .. } => SLIDE_PROTOCOL_VERSION,
            Self::Wipe { .. } => WIPE_PROTOCOL_VERSION,
            Self::FadeToBlack { .. } => FADE_TO_BLACK_PROTOCOL_VERSION,
            Self::StartManualTransition {
                kind: ManualTransitionKind::AlphaFade,
            } => MANUAL_ALPHA_FADE_PROTOCOL_VERSION,
            Self::StartManualTransition { .. }
            | Self::SetManualTransitionPosition { .. }
            | Self::CommitManualTransition
            | Self::CancelManualTransition => MANUAL_TRANSITION_PROTOCOL_VERSION,
        }
    }

    #[must_use]
    pub const fn is_supported_by(self, version: ProtocolVersion) -> bool {
        let required = self.minimum_protocol_version();
        version.major == required.major && version.minor >= required.minor
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandMessage {
    pub protocol: ProtocolVersion,
    pub id: String,
    pub idempotency_key: String,
    pub expected_revision: Option<u64>,
    pub deadline_ms: Option<u64>,
    pub payload: CommandPayload,
}

impl CommandMessage {
    /// Builds the transport-neutral domain envelope after the adapter converts
    /// the wire payload to its domain command.
    #[must_use]
    pub fn domain_envelope<C>(&self, command: C) -> CommandEnvelope<C> {
        let mut envelope =
            CommandEnvelope::new(self.id.clone(), self.idempotency_key.clone(), command);
        if let Some(revision) = self.expected_revision {
            envelope = envelope.expecting(Revision::new(revision));
        }
        if let Some(deadline) = self.deadline_ms {
            envelope = envelope.with_deadline(Deadline::from_millis(deadline));
        }
        envelope
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldIssue {
    pub field: String,
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StructuredError {
    pub code: String,
    pub message: String,
    pub fields: Vec<FieldIssue>,
    pub retryable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ErrorMessage {
    pub request_id: Option<String>,
    pub current_revision: Option<u64>,
    pub error: StructuredError,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandResult {
    Accepted {
        id: String,
        revision: u64,
        scheduled_frame: Option<u64>,
    },
    Rejected {
        id: String,
        code: String,
        message: String,
        fields: Vec<FieldIssue>,
        current_revision: u64,
        retryable: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotMessage {
    pub engine: EngineIdentity,
    pub revision: u64,
    pub show_name: String,
    pub inputs: Vec<WireInputId>,
    pub desired_program: WireInputId,
    pub desired_preview: WireInputId,
    pub realized_program: WireInputId,
    pub realized_preview: WireInputId,
    /// `None` means the protocol extension was omitted for an older peer.
    pub desired_manual_transition: Option<ManualTransitionStatus>,
    /// `None` means the protocol extension was omitted for an older peer.
    pub realized_manual_transition: Option<ManualTransitionStatus>,
    /// `None` means the protocol extension was omitted for an older peer.
    pub desired_fade_to_black: Option<FadeToBlackState>,
    /// `None` means the protocol extension was omitted for an older peer.
    pub realized_fade_to_black: Option<FadeToBlackState>,
}

/// A durable state change. Runtime progress uses [`RuntimeEventMessage`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventPayload {
    DesiredSwitcher {
        program: WireInputId,
        preview: WireInputId,
        /// `None` means the protocol extension was omitted for an older peer.
        manual_transition: Option<ManualTransitionStatus>,
        /// `None` means the protocol extension was omitted for an older peer.
        fade_to_black: Option<FadeToBlackState>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventMessage {
    pub cursor: EventCursor,
    pub payload: EventPayload,
}

/// One durable event within a single-revision transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableEvent {
    pub sequence: u16,
    pub event_type: String,
    pub payload: String,
}

/// All durable events committed atomically at `cursor.revision`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableEventBatch {
    pub cursor: ResumeCursor,
    pub events: Vec<DurableEvent>,
}

/// A replay discontinuity that requires the receiver to request a snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableGap {
    pub server: ServerIdentity,
    pub requested_after_revision: u64,
    pub available_from_revision: u64,
    pub current_revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeDomainBoundary {
    pub domain: String,
    pub boundary: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeFailureDisposition {
    RolledBack,
    RetainedForRetry,
    FallbackRealized,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeLifecycleEvent {
    Accepted,
    Preparing,
    Scheduled {
        domains: Vec<RuntimeDomainBoundary>,
    },
    Realized {
        domain: String,
        /// `None` means the protocol extension was omitted for an older peer.
        manual_transition: Option<ManualTransitionStatus>,
        /// `None` means the protocol extension was omitted for an older peer.
        fade_to_black: Option<FadeToBlackState>,
    },
    Failed {
        error: StructuredError,
        disposition: RuntimeFailureDisposition,
    },
    Superseded {
        by_revision: u64,
    },
}

/// Runtime progress references durable state but has an independent sequence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeEventMessage {
    pub server: ServerIdentity,
    pub revision: u64,
    pub generation: u64,
    pub sequence: u64,
    pub event: RuntimeLifecycleEvent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeartbeatMessage {
    pub server: ServerIdentity,
    pub sequence: u64,
    pub sent_at_ms: u64,
    pub last_applied: Option<ResumeCursor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityReportSummary {
    pub digest: String,
    pub total: u32,
    pub available: u32,
    pub degraded: u32,
    pub unavailable: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityReportMessage {
    pub server: ServerIdentity,
    pub revision: u64,
    pub summary: CapabilityReportSummary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WireMessage {
    ClientHello(ClientHello),
    ServerHello(ServerHello),
    Command(CommandMessage),
    CommandResult(CommandResult),
    Snapshot(SnapshotMessage),
    Event(EventMessage),
    HandshakeRequest(HandshakeRequest),
    HandshakeResponse(HandshakeResponse),
    DurableEventBatch(DurableEventBatch),
    DurableGap(DurableGap),
    RuntimeEvent(RuntimeEventMessage),
    Heartbeat(HeartbeatMessage),
    CapabilityReport(CapabilityReportMessage),
    Error(ErrorMessage),
}

impl WireMessage {
    /// Returns the projection safe to send to one negotiated peer.
    #[must_use]
    pub fn compatible_with(&self, version: ProtocolVersion) -> Self {
        let mut message = self.clone();
        if version.major != FADE_TO_BLACK_PROTOCOL_VERSION.major
            || version.minor < FADE_TO_BLACK_PROTOCOL_VERSION.minor
        {
            match &mut message {
                Self::Snapshot(snapshot) => {
                    snapshot.desired_fade_to_black = None;
                    snapshot.realized_fade_to_black = None;
                }
                Self::Event(EventMessage {
                    payload: EventPayload::DesiredSwitcher { fade_to_black, .. },
                    ..
                })
                | Self::RuntimeEvent(RuntimeEventMessage {
                    event: RuntimeLifecycleEvent::Realized { fade_to_black, .. },
                    ..
                }) => *fade_to_black = None,
                _ => {}
            }
        }
        if version.major == MANUAL_TRANSITION_PROTOCOL_VERSION.major
            && version.minor >= MANUAL_TRANSITION_PROTOCOL_VERSION.minor
        {
            if version.minor < MANUAL_ALPHA_FADE_PROTOCOL_VERSION.minor {
                project_manual_alpha_fade(&mut message);
            }
            return message;
        }
        match &mut message {
            Self::Snapshot(snapshot) => {
                snapshot.desired_manual_transition = None;
                snapshot.realized_manual_transition = None;
            }
            Self::Event(EventMessage {
                payload:
                    EventPayload::DesiredSwitcher {
                        manual_transition, ..
                    },
                ..
            })
            | Self::RuntimeEvent(RuntimeEventMessage {
                event:
                    RuntimeLifecycleEvent::Realized {
                        manual_transition, ..
                    },
                ..
            }) => *manual_transition = None,
            _ => {}
        }
        message
    }
}

fn project_manual_alpha_fade(message: &mut WireMessage) {
    match message {
        WireMessage::Snapshot(snapshot) => {
            project_manual_alpha_fade_status(&mut snapshot.desired_manual_transition);
            project_manual_alpha_fade_status(&mut snapshot.realized_manual_transition);
        }
        WireMessage::Event(EventMessage {
            payload:
                EventPayload::DesiredSwitcher {
                    manual_transition, ..
                },
            ..
        })
        | WireMessage::RuntimeEvent(RuntimeEventMessage {
            event:
                RuntimeLifecycleEvent::Realized {
                    manual_transition, ..
                },
            ..
        }) => project_manual_alpha_fade_status(manual_transition),
        _ => {}
    }
}

fn project_manual_alpha_fade_status(status: &mut Option<ManualTransitionStatus>) {
    if matches!(
        status,
        Some(ManualTransitionStatus::Active(ManualTransitionState {
            kind: ManualTransitionKind::AlphaFade,
            ..
        }))
    ) {
        *status = Some(ManualTransitionStatus::Inactive);
    }
}
