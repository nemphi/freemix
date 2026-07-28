//! Transport-independent diagnostic protocol client state machine.

#[cfg(feature = "std-tcp")]
mod tcp;

#[cfg(feature = "std-tcp")]
pub use tcp::{
    DisconnectCause, SessionEvent, TcpConnection, TcpConnectionError, TcpSession, TcpSessionError,
};

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fmt;

use fm_command::{CommandId, Revision};
use fm_protocol::{
    ClientType, CommandMessage, CommandPayload, CommandResult, DurableGap, EngineIdentity,
    EventMessage, FADE_TO_BLACK_PROTOCOL_VERSION, HandshakeOutcome, HandshakeRequest,
    HandshakeResponse, HeartbeatMessage, MANUAL_TRANSITION_PROTOCOL_VERSION, ProtocolVersion,
    ResumeCursor, Role, RuntimeEventMessage, RuntimeLifecycleEvent, ServerHello, ServerIdentity,
    SnapshotMessage, StructuredError, WireMessage, negotiate_version,
};
use fm_types::ProjectId;
use fm_ui_model::{
    ClientModel, DurableProjectEvent, ManualTransitionStatus as ModelManualTransitionStatus,
    ModelError, OptimisticChange, ProjectSnapshot, RuntimeRealization,
};

/// Default number of recent completed command statuses retained locally.
pub const DEFAULT_COMPLETED_COMMAND_CAPACITY: usize = 256;
/// Largest accepted completed-command history bound.
pub const MAX_COMPLETED_COMMAND_CAPACITY: usize = 65_536;

/// Static client settings. An external scheduler supplies all clock behavior.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientConfig {
    pub supported_versions: Vec<ProtocolVersion>,
    pub build: String,
    pub client_type: ClientType,
    pub desired_role: Role,
    pub client_id: String,
    pub project_id: ProjectId,
    pub outbound_capacity: usize,
    /// Maximum locally retained terminal records. Eviction forgets the key only
    /// locally; callers must keep keys globally unique over server receipt retention.
    pub completed_command_capacity: usize,
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
}

impl ClientConfig {
    /// Creates settings with conservative queue and reconnect defaults.
    #[must_use]
    pub fn new(
        supported_versions: Vec<ProtocolVersion>,
        build: impl Into<String>,
        client_type: ClientType,
        desired_role: Role,
        client_id: impl Into<String>,
        project_id: ProjectId,
    ) -> Self {
        Self {
            supported_versions,
            build: build.into(),
            client_type,
            desired_role,
            client_id: client_id.into(),
            project_id,
            outbound_capacity: 256,
            completed_command_capacity: DEFAULT_COMPLETED_COMMAND_CAPACITY,
            initial_backoff_ms: 250,
            max_backoff_ms: 30_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncMode {
    Snapshot,
    Resume,
}

/// A reconnect delay for the transport owner's scheduler to elapse.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReconnectBackoff {
    pub attempt: u32,
    pub delay_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    AwaitingHandshake,
    Synchronizing {
        mode: SyncMode,
        target_revision: u64,
    },
    Ready,
    Backoff(ReconnectBackoff),
    ResyncRequired {
        expected_revision: u64,
        received_revision: u64,
    },
    PendingIncompatible {
        command_id: String,
        negotiated: ProtocolVersion,
        required: ProtocolVersion,
        backoff: ReconnectBackoff,
    },
    Incompatible {
        negotiated: ProtocolVersion,
    },
}

/// Metadata accepted from the current handshake.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Session {
    pub protocol: ProtocolVersion,
    pub granted_role: Role,
    pub permissions: Vec<String>,
    pub server: ServerIdentity,
    pub capabilities_digest: String,
}

/// Items waiting for a transport adapter. The bounded queue deliberately does
/// not contain connection setup; [`Client::transport_connected`] returns it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outbound {
    Command(CommandMessage),
    Heartbeat(HeartbeatMessage),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandStatus {
    Queued,
    Sent,
    Completed(CommandResult),
    TerminalUncertain(CommandUncertainty),
}

impl CommandStatus {
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed(_) | Self::TerminalUncertain(_))
    }

    const fn is_active(&self) -> bool {
        matches!(self, Self::Queued | Self::Sent)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandUncertainty {
    IdempotencyReplayCollision { received_command_id: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandRecord {
    pub command: CommandMessage,
    pub status: CommandStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Intake {
    Handshake,
    SnapshotApplied,
    EventApplied,
    RuntimeEventObserved,
    ResultReconciled,
    DuplicateResult,
    ResyncRequested,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientError {
    InvalidConfig(&'static str),
    InvalidState {
        operation: &'static str,
        state: ConnectionState,
    },
    IncompatibleProtocol(ProtocolVersion),
    HandshakeRejected(StructuredError),
    InvalidHandshake(&'static str),
    InvalidSnapshot(&'static str),
    Model(ModelError),
    StaleEvent {
        expected_revision: u64,
        received_revision: u64,
    },
    ResyncRequired {
        expected_revision: u64,
        received_revision: u64,
    },
    RuntimeIdentityMismatch {
        expected: Box<ServerIdentity>,
        received: Box<ServerIdentity>,
    },
    RuntimeRevisionAhead {
        current_revision: u64,
        received_revision: u64,
    },
    RuntimeSequenceGap {
        generation: u64,
        expected_sequence: u64,
        received_sequence: u64,
    },
    QueueFull {
        capacity: usize,
    },
    UnsupportedCommandVersion {
        negotiated: ProtocolVersion,
        required: ProtocolVersion,
    },
    EmptyIdempotencyKey,
    DuplicateIdempotencyKey(String),
    CommandIdExhausted,
    HeartbeatSequenceExhausted,
    UnknownCommand(String),
    CommandAlreadyCompleted(String),
    CommandTerminalUncertain(String),
    ConflictingResult(String),
    IdempotencyReplayCollision {
        received_command_id: String,
        affected_command_ids: Vec<String>,
    },
    UnexpectedMessage,
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message)
            | Self::InvalidHandshake(message)
            | Self::InvalidSnapshot(message) => formatter.write_str(message),
            Self::InvalidState { operation, state } => {
                write!(formatter, "cannot {operation} while client is {state:?}")
            }
            Self::IncompatibleProtocol(version) => {
                write!(formatter, "server selected incompatible protocol {version}")
            }
            Self::HandshakeRejected(error) => {
                write!(
                    formatter,
                    "handshake rejected: {}: {}",
                    error.code, error.message
                )
            }
            Self::Model(error) => error.fmt(formatter),
            Self::StaleEvent {
                expected_revision,
                received_revision,
            } => write!(
                formatter,
                "stale event revision {received_revision}; expected {expected_revision}"
            ),
            Self::ResyncRequired {
                expected_revision,
                received_revision,
            } => write!(
                formatter,
                "event gap at revision {received_revision}; expected {expected_revision}"
            ),
            Self::RuntimeIdentityMismatch { .. } => {
                formatter.write_str("runtime event identity does not match the session")
            }
            Self::RuntimeRevisionAhead {
                current_revision,
                received_revision,
            } => write!(
                formatter,
                "runtime event revision {received_revision} is ahead of durable revision {current_revision}"
            ),
            Self::RuntimeSequenceGap {
                generation,
                expected_sequence,
                received_sequence,
            } => write!(
                formatter,
                "runtime event sequence gap for generation {generation}: expected {expected_sequence}, got {received_sequence}"
            ),
            Self::QueueFull { capacity } => {
                write!(formatter, "outbound queue reached capacity {capacity}")
            }
            Self::UnsupportedCommandVersion {
                negotiated,
                required,
            } => write!(
                formatter,
                "command requires protocol {required}, but the session negotiated {negotiated}"
            ),
            Self::EmptyIdempotencyKey => formatter.write_str("idempotency key must not be empty"),
            Self::DuplicateIdempotencyKey(key) => {
                write!(formatter, "idempotency key {key:?} is already in use")
            }
            Self::CommandIdExhausted => formatter.write_str("client command ID space exhausted"),
            Self::HeartbeatSequenceExhausted => {
                formatter.write_str("heartbeat sequence space exhausted")
            }
            Self::UnknownCommand(id) => write!(formatter, "unknown command result ID {id:?}"),
            Self::CommandAlreadyCompleted(id) => {
                write!(formatter, "command {id:?} is already complete")
            }
            Self::CommandTerminalUncertain(id) => {
                write!(formatter, "command {id:?} has a terminal uncertain outcome")
            }
            Self::ConflictingResult(id) => {
                write!(formatter, "command {id:?} received conflicting results")
            }
            Self::IdempotencyReplayCollision {
                received_command_id,
                affected_command_ids,
            } => write!(
                formatter,
                "server replayed unknown command ID {received_command_id:?} while sent command(s) {affected_command_ids:?} were active; outcomes are uncertain and an authoritative snapshot is required"
            ),
            Self::UnexpectedMessage => formatter.write_str("unexpected inbound message type"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<ModelError> for ClientError {
    fn from(value: ModelError) -> Self {
        Self::Model(value)
    }
}

/// Deterministic client state driven entirely by explicit transport events.
#[derive(Debug)]
pub struct Client {
    config: ClientConfig,
    state: ConnectionState,
    session: Option<Session>,
    model: ClientModel,
    outbound: VecDeque<Outbound>,
    commands: BTreeMap<String, CommandRecord>,
    idempotency_keys: HashSet<String>,
    completed_command_ids: VecDeque<String>,
    next_command_id: u64,
    next_heartbeat_sequence: u64,
    reconnect_attempt: u32,
    force_snapshot: bool,
    runtime_server: Option<ServerIdentity>,
    runtime_sequences: BTreeMap<u64, u64>,
}

impl Client {
    /// Creates a disconnected client.
    ///
    /// # Errors
    ///
    /// Rejects settings that cannot provide bounded operation or negotiation.
    pub fn new(config: ClientConfig) -> Result<Self, ClientError> {
        if config.supported_versions.is_empty() {
            return Err(ClientError::InvalidConfig(
                "at least one supported protocol version is required",
            ));
        }
        if config.client_id.is_empty() {
            return Err(ClientError::InvalidConfig("client ID must not be empty"));
        }
        if config.outbound_capacity == 0 {
            return Err(ClientError::InvalidConfig(
                "outbound capacity must be greater than zero",
            ));
        }
        if config.completed_command_capacity == 0
            || config.completed_command_capacity > MAX_COMPLETED_COMMAND_CAPACITY
        {
            return Err(ClientError::InvalidConfig(
                "completed command capacity must be finite and between 1 and 65536",
            ));
        }
        if config.initial_backoff_ms == 0 || config.max_backoff_ms < config.initial_backoff_ms {
            return Err(ClientError::InvalidConfig(
                "invalid reconnect backoff range",
            ));
        }
        let project_id = config.project_id;
        Ok(Self {
            config,
            state: ConnectionState::Disconnected,
            session: None,
            model: ClientModel::new(project_id),
            outbound: VecDeque::new(),
            commands: BTreeMap::new(),
            idempotency_keys: HashSet::new(),
            completed_command_ids: VecDeque::new(),
            next_command_id: 1,
            next_heartbeat_sequence: 1,
            reconnect_attempt: 0,
            force_snapshot: false,
            runtime_server: None,
            runtime_sequences: BTreeMap::new(),
        })
    }

    #[must_use]
    pub const fn state(&self) -> &ConnectionState {
        &self.state
    }

    #[must_use]
    pub const fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    #[must_use]
    pub const fn model(&self) -> &ClientModel {
        &self.model
    }

    #[must_use]
    pub fn last_applied_cursor(&self) -> Option<ResumeCursor> {
        let cursor = self.model.reconnect_cursor()?;
        Some(ResumeCursor {
            server: ServerIdentity {
                engine_id: cursor.engine.engine_id.clone(),
                project_id: self.config.project_id.to_string(),
                state_epoch: cursor.engine.state_epoch,
                log_id: cursor.engine.log_id.clone(),
            },
            revision: cursor.revision.get(),
        })
    }

    #[must_use]
    pub fn outbound_len(&self) -> usize {
        self.outbound.len()
    }

    /// Maximum number of queued transport records and TCP in-flight commands.
    #[must_use]
    pub const fn outbound_capacity(&self) -> usize {
        self.config.outbound_capacity
    }

    #[must_use]
    pub fn command(&self, id: &str) -> Option<&CommandRecord> {
        self.commands.get(id)
    }

    /// Number of active commands plus bounded, completion-ordered local history.
    #[must_use]
    pub fn retained_command_count(&self) -> usize {
        self.commands.len()
    }

    /// Begins a connection attempt. Backoff timing remains caller-owned.
    ///
    /// # Errors
    ///
    /// Requires a disconnected, backed-off, or resync-required state.
    pub fn start_connect(&mut self) -> Result<(), ClientError> {
        match self.state {
            ConnectionState::Disconnected
            | ConnectionState::Backoff(_)
            | ConnectionState::ResyncRequired { .. }
            | ConnectionState::PendingIncompatible { .. } => {
                self.state = ConnectionState::Connecting;
                Ok(())
            }
            _ => Err(self.invalid_state("start a connection")),
        }
    }

    /// Constructs the initial handshake after transport establishment.
    ///
    /// # Errors
    ///
    /// Requires an active connection attempt.
    pub fn transport_connected(&mut self) -> Result<HandshakeRequest, ClientError> {
        if self.state != ConnectionState::Connecting {
            return Err(self.invalid_state("report transport connected"));
        }
        let request = HandshakeRequest {
            versions: self.config.supported_versions.clone(),
            build: self.config.build.clone(),
            client_type: self.config.client_type,
            desired_role: self.config.desired_role,
            resume_cursor: if self.force_snapshot {
                None
            } else {
                self.last_applied_cursor()
            },
        };
        self.state = ConnectionState::AwaitingHandshake;
        Ok(request)
    }

    /// Enters exponential backoff without sleeping or consulting a clock.
    #[must_use]
    pub fn transport_disconnected(&mut self) -> ReconnectBackoff {
        self.session = None;
        self.reset_runtime_sequences();
        self.outbound
            .retain(|item| matches!(item, Outbound::Command(_)));
        let backoff = self.next_reconnect_backoff();
        self.state = ConnectionState::Backoff(backoff);
        backoff
    }

    #[cfg(feature = "std-tcp")]
    fn command_protocol_incompatible(
        &mut self,
        command_id: String,
        negotiated: ProtocolVersion,
        required: ProtocolVersion,
    ) -> ReconnectBackoff {
        self.session = None;
        self.reset_runtime_sequences();
        self.outbound
            .retain(|item| matches!(item, Outbound::Command(_)));
        let backoff = self.next_reconnect_backoff();
        self.state = ConnectionState::PendingIncompatible {
            command_id,
            negotiated,
            required,
            backoff,
        };
        backoff
    }

    fn next_reconnect_backoff(&mut self) -> ReconnectBackoff {
        self.reconnect_attempt = self.reconnect_attempt.saturating_add(1);
        let exponent = self.reconnect_attempt.saturating_sub(1).min(63);
        let delay_ms = self
            .config
            .initial_backoff_ms
            .saturating_mul(1_u64 << exponent)
            .min(self.config.max_backoff_ms);
        ReconnectBackoff {
            attempt: self.reconnect_attempt,
            delay_ms,
        }
    }

    /// Dispatches one inbound protocol DTO.
    ///
    /// # Errors
    ///
    /// Rejects messages invalid for the current state or replicated model.
    pub fn intake(&mut self, message: WireMessage) -> Result<Intake, ClientError> {
        match message {
            WireMessage::HandshakeResponse(response) => {
                self.accept_handshake(response)?;
                Ok(Intake::Handshake)
            }
            WireMessage::Snapshot(snapshot) => {
                self.apply_snapshot(snapshot)?;
                Ok(Intake::SnapshotApplied)
            }
            WireMessage::Event(event) => {
                self.apply_event(event)?;
                Ok(Intake::EventApplied)
            }
            WireMessage::RuntimeEvent(event) => {
                self.apply_runtime_event(event)?;
                Ok(Intake::RuntimeEventObserved)
            }
            WireMessage::CommandResult(result) => self.reconcile_result(result),
            WireMessage::DurableGap(gap) => {
                self.apply_gap(&gap)?;
                Ok(Intake::ResyncRequested)
            }
            WireMessage::ClientHello(_)
            | WireMessage::ServerHello(_)
            | WireMessage::Command(_)
            | WireMessage::HandshakeRequest(_)
            | WireMessage::DurableEventBatch(_)
            | WireMessage::Heartbeat(_)
            | WireMessage::CapabilityReport(_)
            | WireMessage::Error(_) => Err(ClientError::UnexpectedMessage),
        }
    }

    /// Validates protocol and identity before choosing snapshot or resume.
    ///
    /// # Errors
    ///
    /// Rejects incompatible versions, project identities, resume cursors, and
    /// structured handshake failures.
    pub fn accept_handshake(&mut self, response: HandshakeResponse) -> Result<(), ClientError> {
        if self.state != ConnectionState::AwaitingHandshake {
            return Err(self.invalid_state("accept a handshake"));
        }
        if negotiate_version(&self.config.supported_versions, &[response.negotiated])
            != Ok(response.negotiated)
        {
            self.state = ConnectionState::Incompatible {
                negotiated: response.negotiated,
            };
            return Err(ClientError::IncompatibleProtocol(response.negotiated));
        }
        if response.server.project_id != self.config.project_id.to_string() {
            return Err(ClientError::InvalidHandshake(
                "server selected a different project",
            ));
        }

        let resume = match &response.outcome {
            HandshakeOutcome::Snapshot { .. } => false,
            HandshakeOutcome::Resume { cursor } => {
                if self.force_snapshot
                    || cursor.server != response.server
                    || self.last_applied_cursor().as_ref() != Some(cursor)
                    || cursor.revision > response.current_revision
                {
                    return Err(ClientError::InvalidHandshake(
                        "server offered an invalid resume cursor",
                    ));
                }
                true
            }
            HandshakeOutcome::Rejected { error } => {
                self.state = ConnectionState::Disconnected;
                return Err(ClientError::HandshakeRejected(error.clone()));
            }
        };

        let engine = engine_identity(&response.server);
        if self
            .runtime_server
            .as_ref()
            .is_some_and(|server| server != &response.server)
        {
            self.reset_runtime_sequences();
        }
        self.model.observe_server(&ServerHello {
            negotiated: response.negotiated,
            granted_role: response.granted_role,
            permissions: response.permissions.clone(),
            capabilities_digest: response.capabilities.digest.clone(),
            engine,
            current_revision: response.current_revision,
            resume,
        })?;
        self.session = Some(Session {
            protocol: response.negotiated,
            granted_role: response.granted_role,
            permissions: response.permissions,
            server: response.server,
            capabilities_digest: response.capabilities.digest,
        });
        for record in self.commands.values_mut() {
            if record.status.is_active()
                && record.command.payload.is_supported_by(response.negotiated)
            {
                record.command.protocol = response.negotiated;
            }
        }
        for item in &mut self.outbound {
            if let Outbound::Command(command) = item
                && command.payload.is_supported_by(response.negotiated)
            {
                command.protocol = response.negotiated;
            }
        }
        let mode = if resume {
            SyncMode::Resume
        } else {
            SyncMode::Snapshot
        };
        self.state = ConnectionState::Synchronizing {
            mode,
            target_revision: response.current_revision,
        };
        if resume
            && self
                .model
                .reconnect_cursor()
                .map(|cursor| cursor.revision.get())
                == Some(response.current_revision)
        {
            self.finish_sync();
        }
        Ok(())
    }

    /// Installs an authoritative snapshot through [`ClientModel`].
    ///
    /// # Errors
    ///
    /// Requires synchronization and exact session identity/revision.
    pub fn apply_snapshot(&mut self, snapshot: SnapshotMessage) -> Result<(), ClientError> {
        let ConnectionState::Synchronizing {
            target_revision, ..
        } = self.state
        else {
            return Err(self.invalid_state("apply a snapshot"));
        };
        let session = self
            .session
            .as_ref()
            .ok_or(ClientError::InvalidSnapshot("snapshot has no session"))?;
        if snapshot.engine != engine_identity(&session.server) {
            return Err(ClientError::InvalidSnapshot(
                "snapshot identity does not match the session",
            ));
        }
        if snapshot.revision != target_revision {
            return Err(ClientError::InvalidSnapshot(
                "snapshot revision does not match the handshake",
            ));
        }
        if supports_manual_state(session.protocol)
            && (snapshot.desired_manual_transition.is_none()
                || snapshot.realized_manual_transition.is_none())
        {
            return Err(ClientError::InvalidSnapshot(
                "protocol 1.4 snapshot omitted manual-transition state",
            ));
        }
        if supports_fade_to_black_state(session.protocol)
            && (snapshot.desired_fade_to_black.is_none()
                || snapshot.realized_fade_to_black.is_none())
        {
            return Err(ClientError::InvalidSnapshot(
                "protocol 1.5 snapshot omitted fade-to-black state",
            ));
        }
        self.model.install_snapshot(ProjectSnapshot::from_protocol(
            self.config.project_id,
            snapshot,
        ))?;
        self.finish_sync();
        Ok(())
    }

    /// Applies exactly the next ordered event through [`ClientModel`].
    ///
    /// # Errors
    ///
    /// Forward gaps and model-invalid events force a fresh-snapshot reconnect.
    pub fn apply_event(&mut self, event: EventMessage) -> Result<(), ClientError> {
        let target_revision = match self.state {
            ConnectionState::Synchronizing {
                mode: SyncMode::Resume,
                target_revision,
            } => Some(target_revision),
            ConnectionState::Ready => None,
            _ => return Err(self.invalid_state("apply an event")),
        };
        let current = self
            .model
            .reconnect_cursor()
            .ok_or(ClientError::InvalidSnapshot("event has no base snapshot"))?;
        let protocol = self
            .session
            .as_ref()
            .ok_or(ClientError::InvalidSnapshot("event has no session"))?
            .protocol;
        if supports_manual_state(protocol)
            && matches!(
                event.payload,
                fm_protocol::EventPayload::DesiredSwitcher {
                    manual_transition: None,
                    ..
                }
            )
        {
            return Err(ClientError::InvalidSnapshot(
                "protocol 1.4 event omitted manual-transition state",
            ));
        }
        if supports_fade_to_black_state(protocol)
            && matches!(
                event.payload,
                fm_protocol::EventPayload::DesiredSwitcher {
                    fade_to_black: None,
                    ..
                }
            )
        {
            return Err(ClientError::InvalidSnapshot(
                "protocol 1.5 event omitted fade-to-black state",
            ));
        }
        let expected_revision = current
            .revision
            .get()
            .checked_add(1)
            .ok_or(ClientError::InvalidSnapshot("event revision overflow"))?;
        if event.cursor.revision < expected_revision {
            return Err(ClientError::StaleEvent {
                expected_revision,
                received_revision: event.cursor.revision,
            });
        }
        if event.cursor.engine != current.engine
            || event.cursor.revision > expected_revision
            || target_revision.is_some_and(|target| event.cursor.revision > target)
        {
            return self.require_resync(expected_revision, event.cursor.revision);
        }
        let received_revision = event.cursor.revision;
        if let Err(error) = self.model.apply_event(DurableProjectEvent::from_protocol(
            self.config.project_id,
            event,
        )) {
            self.force_snapshot = true;
            self.state = ConnectionState::ResyncRequired {
                expected_revision,
                received_revision,
            };
            return Err(ClientError::Model(error));
        }
        if target_revision == Some(received_revision) {
            self.finish_sync();
        }
        Ok(())
    }

    /// Observes the next independently ordered runtime lifecycle event.
    ///
    /// Runtime events may reference any retained durable revision, but never
    /// advance the durable cursor or trigger durable snapshot recovery.
    ///
    /// # Errors
    ///
    /// Rejects events outside an active replicated session, for another server,
    /// ahead of the durable cursor, or with a gap in their generation sequence.
    pub fn apply_runtime_event(&mut self, event: RuntimeEventMessage) -> Result<(), ClientError> {
        if !matches!(
            self.state,
            ConnectionState::Synchronizing {
                mode: SyncMode::Resume,
                ..
            } | ConnectionState::Ready
        ) {
            return Err(self.invalid_state("apply a runtime event"));
        }
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| self.invalid_state("apply a runtime event"))?;
        if event.server != session.server {
            return Err(ClientError::RuntimeIdentityMismatch {
                expected: Box::new(session.server.clone()),
                received: Box::new(event.server),
            });
        }
        let current_revision = self
            .model
            .reconnect_cursor()
            .ok_or(ClientError::InvalidSnapshot(
                "runtime event has no base snapshot",
            ))?
            .revision
            .get();
        if event.revision > current_revision {
            return Err(ClientError::RuntimeRevisionAhead {
                current_revision,
                received_revision: event.revision,
            });
        }

        let expected_sequence = self
            .runtime_sequences
            .get(&event.generation)
            .map_or(1, |sequence| sequence.saturating_add(1));
        if event.sequence != expected_sequence {
            return Err(ClientError::RuntimeSequenceGap {
                generation: event.generation,
                expected_sequence,
                received_sequence: event.sequence,
            });
        }
        if supports_manual_state(session.protocol)
            && matches!(
                &event.event,
                RuntimeLifecycleEvent::Realized {
                    domain,
                    manual_transition: None,
                    ..
                } if domain == "switcher"
            )
        {
            return Err(ClientError::InvalidSnapshot(
                "protocol 1.4 runtime event omitted manual-transition state",
            ));
        }
        if supports_fade_to_black_state(session.protocol)
            && matches!(
                &event.event,
                RuntimeLifecycleEvent::Realized {
                    domain,
                    fade_to_black: None,
                    ..
                } if domain == "switcher"
            )
        {
            return Err(ClientError::InvalidSnapshot(
                "protocol 1.5 runtime event omitted fade-to-black state",
            ));
        }

        if let RuntimeLifecycleEvent::Realized {
            domain,
            manual_transition,
            fade_to_black,
        } = &event.event
            && domain == "switcher"
        {
            self.model.apply_runtime_realization(RuntimeRealization {
                project_id: self.config.project_id,
                engine: engine_identity(&event.server),
                revision: Revision::new(event.revision),
                generation: event.generation,
                sequence: event.sequence,
                manual_transition: manual_transition
                    .map(ModelManualTransitionStatus::from_protocol),
                fade_to_black: *fade_to_black,
            })?;
        }
        self.runtime_server = Some(event.server);
        self.runtime_sequences
            .insert(event.generation, event.sequence);
        Ok(())
    }

    /// Adds a command with a monotonic client-local ID and explicit replay key.
    /// [`CommandPayload::SelectPreview`] is also tracked as optimistic intent
    /// by the UI model. A key remains reserved while its command is active or
    /// retained in terminal local history. Local eviction forgets the key, but
    /// callers must still generate globally unique keys for at least as long as
    /// the server can retain receipts.
    ///
    /// # Errors
    ///
    /// Requires ready state, queue space, and a unique nonempty idempotency key.
    pub fn queue_command(
        &mut self,
        payload: CommandPayload,
        idempotency_key: impl Into<String>,
        expected_revision: Option<u64>,
        deadline_ms: Option<u64>,
    ) -> Result<CommandMessage, ClientError> {
        if self.state != ConnectionState::Ready {
            return Err(self.invalid_state("queue a command"));
        }
        let protocol = self
            .session
            .as_ref()
            .ok_or_else(|| self.invalid_state("queue a command"))?
            .protocol;
        if !payload.is_supported_by(protocol) {
            return Err(ClientError::UnsupportedCommandVersion {
                negotiated: protocol,
                required: payload.minimum_protocol_version(),
            });
        }
        self.ensure_queue_space()?;
        let idempotency_key = idempotency_key.into();
        if idempotency_key.is_empty() {
            return Err(ClientError::EmptyIdempotencyKey);
        }
        if self.idempotency_keys.contains(&idempotency_key) {
            return Err(ClientError::DuplicateIdempotencyKey(idempotency_key));
        }
        let next = self
            .next_command_id
            .checked_add(1)
            .ok_or(ClientError::CommandIdExhausted)?;
        let client_id = &self.config.client_id;
        let sequence = self.next_command_id;
        let command = CommandMessage {
            protocol,
            id: format!("{client_id}:{sequence}"),
            idempotency_key: idempotency_key.clone(),
            expected_revision,
            deadline_ms,
            payload,
        };
        let optimistic = match payload {
            CommandPayload::SelectPreview { input } => {
                Some(OptimisticChange::DesiredPreview(input.to_domain()))
            }
            CommandPayload::Cut
            | CommandPayload::Fade { .. }
            | CommandPayload::AlphaFade { .. }
            | CommandPayload::Slide { .. }
            | CommandPayload::Wipe { .. }
            | CommandPayload::FadeToBlack { .. }
            | CommandPayload::StartManualTransition { .. }
            | CommandPayload::SetManualTransitionPosition { .. }
            | CommandPayload::CommitManualTransition
            | CommandPayload::CancelManualTransition => None,
        };
        self.model
            .track_command(CommandId::new(command.id.clone()), optimistic)?;
        self.next_command_id = next;
        self.idempotency_keys.insert(idempotency_key);
        self.commands.insert(
            command.id.clone(),
            CommandRecord {
                command: command.clone(),
                status: CommandStatus::Queued,
            },
        );
        self.outbound.push_back(Outbound::Command(command.clone()));
        Ok(command)
    }

    /// Requeues an unresolved command with its original idempotency envelope.
    ///
    /// # Errors
    ///
    /// Requires ready state, queue space, and a known unresolved command.
    pub fn retry_command(&mut self, id: &str) -> Result<(), ClientError> {
        if self.state != ConnectionState::Ready {
            return Err(self.invalid_state("retry a command"));
        }
        self.ensure_queue_space()?;
        let record = self
            .commands
            .get_mut(id)
            .ok_or_else(|| ClientError::UnknownCommand(id.to_owned()))?;
        match &record.status {
            CommandStatus::Completed(_) => {
                return Err(ClientError::CommandAlreadyCompleted(id.to_owned()));
            }
            CommandStatus::TerminalUncertain(_) => {
                return Err(ClientError::CommandTerminalUncertain(id.to_owned()));
            }
            CommandStatus::Queued | CommandStatus::Sent => {}
        }
        if !matches!(record.status, CommandStatus::Queued) {
            record.status = CommandStatus::Queued;
            self.outbound
                .push_back(Outbound::Command(record.command.clone()));
        }
        Ok(())
    }

    /// Enqueues a protocol heartbeat carrying the last fully applied cursor.
    /// `sent_at_ms` is supplied by the caller's clock.
    ///
    /// # Errors
    ///
    /// Requires a negotiated session, queue space, and sequence capacity.
    pub fn queue_heartbeat(&mut self, sent_at_ms: u64) -> Result<HeartbeatMessage, ClientError> {
        if !matches!(
            self.state,
            ConnectionState::Synchronizing { .. } | ConnectionState::Ready
        ) {
            return Err(self.invalid_state("queue a heartbeat"));
        }
        self.ensure_queue_space()?;
        let next = self
            .next_heartbeat_sequence
            .checked_add(1)
            .ok_or(ClientError::HeartbeatSequenceExhausted)?;
        let heartbeat = HeartbeatMessage {
            server: self
                .session
                .as_ref()
                .ok_or_else(|| self.invalid_state("queue a heartbeat"))?
                .server
                .clone(),
            sequence: self.next_heartbeat_sequence,
            sent_at_ms,
            last_applied: self.last_applied_cursor(),
        };
        self.next_heartbeat_sequence = next;
        self.outbound
            .push_back(Outbound::Heartbeat(heartbeat.clone()));
        Ok(heartbeat)
    }

    /// Removes the oldest item and marks dequeued commands as sent.
    pub fn pop_outbound(&mut self) -> Option<Outbound> {
        if matches!(self.outbound.front(), Some(Outbound::Command(_)))
            && self.state != ConnectionState::Ready
        {
            return None;
        }
        if self.unresolved_incompatible_command().is_some() {
            return None;
        }
        if let Some(Outbound::Command(command)) = self.outbound.front()
            && self
                .session
                .as_ref()
                .is_some_and(|session| !command.payload.is_supported_by(session.protocol))
        {
            return None;
        }
        let item = self.outbound.pop_front()?;
        if let Outbound::Command(command) = &item
            && let Some(record) = self.commands.get_mut(&command.id)
            && !record.status.is_terminal()
        {
            record.status = CommandStatus::Sent;
        }
        Some(item)
    }

    /// Reconciles a result through both command records and the UI model.
    ///
    /// # Errors
    ///
    /// Rejects unknown command IDs and conflicting duplicate results.
    pub fn reconcile_result(&mut self, result: CommandResult) -> Result<Intake, ClientError> {
        let id = result_id(&result);
        let Some(record) = self.commands.get(id) else {
            return self.handle_unknown_result_collision(&result);
        };
        if let CommandStatus::Completed(previous) = &record.status {
            return if previous == &result {
                Ok(Intake::DuplicateResult)
            } else {
                Err(ClientError::ConflictingResult(id.to_owned()))
            };
        }
        if matches!(record.status, CommandStatus::TerminalUncertain(_)) {
            return Err(ClientError::CommandTerminalUncertain(id.to_owned()));
        }
        self.model.reconcile_command(&result)?;
        let resolves_pending_incompatible = matches!(
            &self.state,
            ConnectionState::PendingIncompatible { command_id, .. } if command_id == id
        );
        let completed_id = id.to_owned();
        if let Some(record) = self.commands.get_mut(id) {
            record.status = CommandStatus::Completed(result);
        }
        self.outbound.retain(
            |item| !matches!(item, Outbound::Command(command) if command.id == completed_id),
        );
        self.completed_command_ids.push_back(completed_id);
        self.prune_completed_commands();
        if resolves_pending_incompatible {
            self.state = ConnectionState::Disconnected;
        }
        Ok(Intake::ResultReconciled)
    }

    fn unresolved_incompatible_command(
        &self,
    ) -> Option<(String, ProtocolVersion, ProtocolVersion)> {
        let negotiated = self.session.as_ref()?.protocol;
        self.commands.values().find_map(|record| {
            (record.status.is_active() && !record.command.payload.is_supported_by(negotiated)).then(
                || {
                    (
                        record.command.id.clone(),
                        negotiated,
                        record.command.payload.minimum_protocol_version(),
                    )
                },
            )
        })
    }

    fn handle_unknown_result_collision<T>(
        &mut self,
        result: &CommandResult,
    ) -> Result<T, ClientError> {
        let received_command_id = result_id(result).to_owned();
        let affected_command_ids = self
            .commands
            .iter()
            .filter(|(_, record)| matches!(record.status, CommandStatus::Sent))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        if affected_command_ids.is_empty() {
            return Err(ClientError::UnknownCommand(received_command_id));
        }

        let current_revision = self
            .last_applied_cursor()
            .map_or(0, |cursor| cursor.revision);
        for id in &affected_command_ids {
            self.model.reconcile_command(&CommandResult::Rejected {
                id: id.clone(),
                code: "idempotency_replay_collision".to_owned(),
                message: format!(
                    "server returned retained receipt for unknown command {received_command_id:?}"
                ),
                fields: Vec::new(),
                current_revision,
                retryable: false,
            })?;
            if let Some(record) = self.commands.get_mut(id) {
                record.status = CommandStatus::TerminalUncertain(
                    CommandUncertainty::IdempotencyReplayCollision {
                        received_command_id: received_command_id.clone(),
                    },
                );
            }
            self.completed_command_ids.push_back(id.clone());
        }
        self.outbound.retain(|item| {
            !matches!(item, Outbound::Command(command) if affected_command_ids.contains(&command.id))
        });
        self.prune_completed_commands();
        self.force_snapshot = true;
        self.state = ConnectionState::ResyncRequired {
            expected_revision: current_revision.saturating_add(1),
            received_revision: result_revision(result),
        };
        Err(ClientError::IdempotencyReplayCollision {
            received_command_id,
            affected_command_ids,
        })
    }

    fn prune_completed_commands(&mut self) {
        while self.completed_command_ids.len() > self.config.completed_command_capacity {
            let id = self
                .completed_command_ids
                .pop_front()
                .expect("completed command retention must have an oldest record");
            let record = self
                .commands
                .remove(&id)
                .expect("completed command index must reference a retained record");
            debug_assert!(record.status.is_terminal());
            self.idempotency_keys
                .remove(&record.command.idempotency_key);
        }
    }

    fn apply_gap(&mut self, gap: &DurableGap) -> Result<(), ClientError> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| self.invalid_state("apply a durable gap"))?;
        if gap.server != session.server {
            return Err(ClientError::InvalidHandshake(
                "durable gap identity does not match the session",
            ));
        }
        self.require_resync(
            gap.requested_after_revision.saturating_add(1),
            gap.available_from_revision,
        )
    }

    fn finish_sync(&mut self) {
        self.state = ConnectionState::Ready;
        // Synchronizing is not recovery while an uncertain command cannot be retried.
        if self.unresolved_incompatible_command().is_none() {
            self.reconnect_attempt = 0;
        }
        self.force_snapshot = false;
    }

    fn reset_runtime_sequences(&mut self) {
        self.runtime_server = None;
        self.runtime_sequences.clear();
    }

    fn require_resync<T>(
        &mut self,
        expected_revision: u64,
        received_revision: u64,
    ) -> Result<T, ClientError> {
        self.force_snapshot = true;
        self.state = ConnectionState::ResyncRequired {
            expected_revision,
            received_revision,
        };
        Err(ClientError::ResyncRequired {
            expected_revision,
            received_revision,
        })
    }

    fn ensure_queue_space(&self) -> Result<(), ClientError> {
        if self.outbound.len() >= self.config.outbound_capacity {
            Err(ClientError::QueueFull {
                capacity: self.config.outbound_capacity,
            })
        } else {
            Ok(())
        }
    }

    fn invalid_state(&self, operation: &'static str) -> ClientError {
        ClientError::InvalidState {
            operation,
            state: self.state.clone(),
        }
    }
}

const fn supports_manual_state(version: ProtocolVersion) -> bool {
    version.major == MANUAL_TRANSITION_PROTOCOL_VERSION.major
        && version.minor >= MANUAL_TRANSITION_PROTOCOL_VERSION.minor
}

const fn supports_fade_to_black_state(version: ProtocolVersion) -> bool {
    version.major == FADE_TO_BLACK_PROTOCOL_VERSION.major
        && version.minor >= FADE_TO_BLACK_PROTOCOL_VERSION.minor
}

fn engine_identity(server: &ServerIdentity) -> EngineIdentity {
    EngineIdentity {
        engine_id: server.engine_id.clone(),
        state_epoch: server.state_epoch,
        log_id: server.log_id.clone(),
    }
}

fn result_id(result: &CommandResult) -> &str {
    match result {
        CommandResult::Accepted { id, .. } | CommandResult::Rejected { id, .. } => id,
    }
}

fn result_revision(result: &CommandResult) -> u64 {
    match result {
        CommandResult::Accepted { revision, .. } => *revision,
        CommandResult::Rejected {
            current_revision, ..
        } => *current_revision,
    }
}
