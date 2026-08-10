//! Replicated client-side production state.
//!
//! [`ClientModel`] is project-bound and owns no production state. It reduces
//! snapshots, ordered durable events, runtime realizations, and command results
//! into a read model for UI clients. Authoritative and optimistic state remain
//! separate so a render frame can use [`ClientModel::view`] without mistaking
//! intent for realization.

use core::{cmp::Ordering, fmt};
use std::collections::{HashMap, HashSet};

use fm_command::{CommandId, Revision};
use fm_protocol::{
    CommandResult, EngineIdentity, EventCursor, EventMessage, EventPayload, FadeToBlackState,
    FieldIssue, ManualTransitionKind, ManualTransitionPosition,
    ManualTransitionStatus as ProtocolManualTransitionStatus, ResumeCursor, ServerHello,
    ServerIdentity, SnapshotMessage, StingerAudioPolicy, StingerMissingMediaFallback,
    StingerReadiness,
};
use fm_types::{InputId, OutputId, ProjectId};

/// A project-aware cursor used at the UI/protocol boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectCursor {
    pub project_id: ProjectId,
    pub engine: EngineIdentity,
    pub revision: Revision,
}

impl ProjectCursor {
    /// Converts this cursor to the project-agnostic protocol cursor.
    #[must_use]
    pub fn protocol_cursor(&self) -> EventCursor {
        EventCursor {
            engine: self.engine.clone(),
            revision: self.revision.get(),
        }
    }

    /// Converts this cursor to the current project-aware resume DTO.
    #[must_use]
    pub fn resume_cursor(&self) -> ResumeCursor {
        ResumeCursor {
            server: ServerIdentity {
                engine_id: self.engine.engine_id.clone(),
                project_id: self.project_id.to_string(),
                state_epoch: self.engine.state_epoch,
                log_id: self.engine.log_id.clone(),
            },
            revision: self.revision.get(),
        }
    }

    /// Converts and validates a project-aware protocol resume cursor.
    ///
    /// # Errors
    ///
    /// Rejects a different project or an incomplete engine identity.
    pub fn from_resume_cursor(
        project_id: ProjectId,
        cursor: ResumeCursor,
    ) -> Result<Self, ModelError> {
        if cursor.server.project_id != project_id.to_string() {
            return Err(ModelError::ProtocolProjectMismatch {
                expected: project_id,
                observed: cursor.server.project_id,
            });
        }
        let engine = EngineIdentity {
            engine_id: cursor.server.engine_id,
            state_epoch: cursor.server.state_epoch,
            log_id: cursor.server.log_id,
        };
        validate_engine(&engine)?;
        Ok(Self {
            project_id,
            engine,
            revision: Revision::new(cursor.revision),
        })
    }
}

/// Program and Preview selections for one switcher state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BusSelection {
    pub program: InputId,
    pub preview: InputId,
}

impl BusSelection {
    #[must_use]
    pub const fn new(program: InputId, preview: InputId) -> Self {
        Self { program, preview }
    }
}

/// Authoritative desired and runtime-realized switcher state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SwitcherState {
    pub desired: BusSelection,
    pub realized: BusSelection,
    pub desired_manual_transition: ManualTransitionStatus,
    pub realized_manual_transition: ManualTransitionStatus,
    pub desired_fade_to_black: FadeToBlackState,
    pub realized_fade_to_black: FadeToBlackState,
    pub runtime_generation: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StingerStatus {
    pub slot: u8,
    pub media_input: InputId,
    pub preload: bool,
    pub cut_point_frames: u32,
    pub audio_policy: StingerAudioPolicy,
    pub missing_media_fallback: StingerMissingMediaFallback,
    pub readiness: StingerReadiness,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverlayStatus {
    pub channel: u8,
    pub source: Option<InputId>,
    pub active: bool,
    pub opacity: u8,
    pub transition: fm_protocol::OverlayTransitionKind,
    pub duration_frames: u32,
    pub position: fm_protocol::OverlayPositionPreset,
    pub border: fm_protocol::OverlayBorderPreset,
    pub queued_sources: Vec<InputId>,
    pub included_outputs: Vec<OutputId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputAudioStripStatus {
    pub input: InputId,
    pub gain_millidb: i32,
    pub balance_basis_points: i32,
    pub muted: bool,
    pub soloed: bool,
    pub follow_video: bool,
    pub delay_samples: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActiveManualTransition {
    pub kind: ManualTransitionKind,
    pub from: InputId,
    pub to: InputId,
    pub interval_start: ManualTransitionPosition,
    pub position: ManualTransitionPosition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManualTransitionStatus {
    Inactive,
    Active(ActiveManualTransition),
}

impl ManualTransitionStatus {
    #[must_use]
    pub const fn from_protocol(status: ProtocolManualTransitionStatus) -> Self {
        match status {
            ProtocolManualTransitionStatus::Inactive => Self::Inactive,
            ProtocolManualTransitionStatus::Active(state) => Self::Active(ActiveManualTransition {
                kind: state.kind,
                from: state.from.to_domain(),
                to: state.to.to_domain(),
                interval_start: state.interval_start,
                position: state.position,
            }),
        }
    }
}

/// An authoritative project snapshot suitable for the UI reducer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectSnapshot {
    pub cursor: ProjectCursor,
    pub show_name: String,
    pub inputs: Vec<InputId>,
    pub input_names: Vec<String>,
    pub input_audio_strips: Vec<InputAudioStripStatus>,
    pub stingers: Vec<StingerStatus>,
    pub desired_overlays: Vec<OverlayStatus>,
    pub realized_overlays: Vec<OverlayStatus>,
    pub switcher: SwitcherState,
}

impl ProjectSnapshot {
    /// Adds the project identity omitted by the current protocol snapshot DTO.
    #[must_use]
    pub fn from_protocol(project_id: ProjectId, message: SnapshotMessage) -> Self {
        let stingers = protocol_stingers(message.stingers);
        let desired_overlays = protocol_overlays(message.desired_overlays);
        let realized_overlays = protocol_overlays(message.realized_overlays);
        let input_audio_strips = protocol_input_audio_strips(message.input_audio_strips);
        let (inputs, input_names) = message
            .inputs
            .into_iter()
            .map(|input| (input.input.to_domain(), input.name))
            .unzip();
        Self {
            cursor: ProjectCursor {
                project_id,
                engine: message.engine,
                revision: Revision::new(message.revision),
            },
            show_name: message.show_name,
            inputs,
            input_names,
            input_audio_strips,
            stingers,
            desired_overlays,
            realized_overlays,
            switcher: SwitcherState {
                desired: BusSelection::new(
                    message.desired_program.to_domain(),
                    message.desired_preview.to_domain(),
                ),
                realized: BusSelection::new(
                    message.realized_program.to_domain(),
                    message.realized_preview.to_domain(),
                ),
                desired_manual_transition: manual_status_from_protocol(
                    message.desired_manual_transition,
                ),
                realized_manual_transition: manual_status_from_protocol(
                    message.realized_manual_transition,
                ),
                desired_fade_to_black: fade_to_black_from_protocol(message.desired_fade_to_black),
                realized_fade_to_black: fade_to_black_from_protocol(message.realized_fade_to_black),
                runtime_generation: None,
            },
        }
    }
}

/// A UI-owned durable desired-state change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableChange {
    DesiredSwitcher {
        selection: BusSelection,
        manual_transition: ManualTransitionStatus,
        fade_to_black: FadeToBlackState,
        overlays: Vec<OverlayStatus>,
        input_audio_strips: Vec<InputAudioStripStatus>,
    },
    StingerSlotsChanged {
        selection: BusSelection,
        manual_transition: ManualTransitionStatus,
        fade_to_black: FadeToBlackState,
        stingers: Vec<StingerStatus>,
        overlays: Vec<OverlayStatus>,
        input_audio_strips: Vec<InputAudioStripStatus>,
    },
}

/// One globally ordered project event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableProjectEvent {
    pub cursor: ProjectCursor,
    pub change: DurableChange,
}

impl DurableProjectEvent {
    /// Adds project identity and converts the current protocol event DTO.
    #[must_use]
    pub fn from_protocol(project_id: ProjectId, message: EventMessage) -> Self {
        let change = match message.payload {
            EventPayload::DesiredSwitcher {
                program,
                preview,
                manual_transition,
                fade_to_black,
                overlays,
                input_audio_strips,
            } => DurableChange::DesiredSwitcher {
                selection: BusSelection::new(program.to_domain(), preview.to_domain()),
                manual_transition: manual_status_from_protocol(manual_transition),
                fade_to_black: fade_to_black_from_protocol(fade_to_black),
                overlays: protocol_overlays(overlays),
                input_audio_strips: protocol_input_audio_strips(input_audio_strips),
            },
            EventPayload::StingerSlotsChanged {
                program,
                preview,
                manual_transition,
                fade_to_black,
                stingers,
                overlays,
                input_audio_strips,
            } => DurableChange::StingerSlotsChanged {
                selection: BusSelection::new(program.to_domain(), preview.to_domain()),
                manual_transition: manual_status_from_protocol(manual_transition),
                fade_to_black: fade_to_black_from_protocol(fade_to_black),
                stingers: protocol_stingers(stingers),
                overlays: protocol_overlays(overlays),
                input_audio_strips: protocol_input_audio_strips(input_audio_strips),
            },
        };
        Self {
            cursor: ProjectCursor {
                project_id,
                engine: message.cursor.engine,
                revision: Revision::new(message.cursor.revision),
            },
            change,
        }
    }

    /// Combines a project-aware resume cursor with a UI-decoded durable change.
    ///
    /// The current durable batch DTO intentionally leaves event payloads
    /// opaque, so transport adapters decode only the event types understood by
    /// this model and pass the resulting change here.
    ///
    /// # Errors
    ///
    /// Rejects a cursor for another project or an incomplete engine identity.
    pub fn from_resume_cursor(
        project_id: ProjectId,
        cursor: ResumeCursor,
        change: DurableChange,
    ) -> Result<Self, ModelError> {
        Ok(Self {
            cursor: ProjectCursor::from_resume_cursor(project_id, cursor)?,
            change,
        })
    }
}

/// Independently ordered runtime confirmation of a durable switcher revision.
///
/// The realized routing is looked up from the desired state retained for
/// `revision`; runtime messages do not carry or modify durable desired state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeRealization {
    pub project_id: ProjectId,
    pub engine: EngineIdentity,
    pub revision: Revision,
    pub generation: u64,
    pub sequence: u64,
    pub manual_transition: ManualTransitionStatus,
    pub fade_to_black: FadeToBlackState,
}

/// Authoritative project data at the last applied cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectState {
    show_name: String,
    inputs: Vec<InputId>,
    input_names: Vec<String>,
    input_audio_strips: Vec<InputAudioStripStatus>,
    stingers: Vec<StingerStatus>,
    desired_overlays: Vec<OverlayStatus>,
    realized_overlays: Vec<OverlayStatus>,
    switcher: SwitcherState,
}

impl ProjectState {
    #[must_use]
    pub fn show_name(&self) -> &str {
        &self.show_name
    }

    #[must_use]
    pub fn inputs(&self) -> &[InputId] {
        &self.inputs
    }

    #[must_use]
    pub fn input_name(&self, input: InputId) -> Option<&str> {
        self.inputs
            .iter()
            .position(|candidate| *candidate == input)
            .map(|index| self.input_names[index].as_str())
    }

    #[must_use]
    pub fn input_audio_strips(&self) -> &[InputAudioStripStatus] {
        &self.input_audio_strips
    }

    #[must_use]
    pub fn stingers(&self) -> &[StingerStatus] {
        &self.stingers
    }

    #[must_use]
    pub fn desired_overlays(&self) -> &[OverlayStatus] {
        &self.desired_overlays
    }

    #[must_use]
    pub fn realized_overlays(&self) -> &[OverlayStatus] {
        &self.realized_overlays
    }

    #[must_use]
    pub const fn switcher(&self) -> SwitcherState {
        self.switcher
    }
}

/// An immutable render snapshot, including outstanding optimistic intent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientView {
    pub cursor: ProjectCursor,
    pub show_name: String,
    pub inputs: Vec<InputId>,
    pub input_names: Vec<String>,
    pub input_audio_strips: Vec<InputAudioStripStatus>,
    pub stingers: Vec<StingerStatus>,
    pub desired_overlays: Vec<OverlayStatus>,
    pub realized_overlays: Vec<OverlayStatus>,
    pub switcher: SwitcherState,
}

/// A local change displayed while its command is outstanding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OptimisticChange {
    DesiredSwitcher(BusSelection),
    DesiredProgram(InputId),
    DesiredPreview(InputId),
}

impl OptimisticChange {
    fn apply(self, selection: &mut BusSelection) {
        match self {
            Self::DesiredSwitcher(value) => *selection = value,
            Self::DesiredProgram(value) => selection.program = value,
            Self::DesiredPreview(value) => selection.preview = value,
        }
    }

    fn inputs(self) -> [Option<InputId>; 2] {
        match self {
            Self::DesiredSwitcher(value) => [Some(value.program), Some(value.preview)],
            Self::DesiredProgram(value) | Self::DesiredPreview(value) => [Some(value), None],
        }
    }
}

/// The state of an outstanding command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingStatus {
    AwaitingResult,
    AwaitingEvent { accepted_revision: Revision },
}

/// A command and its optional optimistic UI change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingCommand {
    pub id: CommandId,
    pub optimistic: Option<OptimisticChange>,
    pub status: PendingStatus,
}

/// Stable rejection details retained for operator feedback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RejectedCommand {
    pub id: CommandId,
    pub code: String,
    pub message: String,
    pub fields: Vec<FieldIssue>,
    pub current_revision: Revision,
    pub retryable: bool,
}

/// Whether the local cursor can be treated as current.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncStatus {
    AwaitingSnapshot,
    Current,
    Behind {
        local_revision: Revision,
        known_revision: Revision,
    },
    RequiresSnapshot,
}

impl SyncStatus {
    #[must_use]
    pub const fn is_stale(&self) -> bool {
        !matches!(self, Self::Current)
    }
}

/// Result of installing an authoritative snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotInstalled {
    pub reconciled_commands: Vec<CommandId>,
    pub discarded_commands: Vec<CommandId>,
}

/// Result of reducing a durable event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EventApplied {
    Applied { reconciled_commands: Vec<CommandId> },
    Duplicate,
}

/// Result of reducing an independently ordered runtime realization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeRealizationApplied {
    Applied,
    Duplicate,
}

/// Result of reducing a command receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandReconciled {
    Accepted {
        revision: Revision,
        awaiting_event: bool,
    },
    Rejected(RejectedCommand),
}

/// A reducer input for callers that prefer one dispatch surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    InstallSnapshot(Box<ProjectSnapshot>),
    ApplyEvent(DurableProjectEvent),
    ApplyRuntimeRealization(RuntimeRealization),
    TrackCommand {
        id: CommandId,
        optimistic: Option<OptimisticChange>,
    },
    ReconcileCommand(CommandResult),
    ObserveServer(ServerHello),
}

/// The effect produced by [`ClientModel::reduce`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reduction {
    SnapshotInstalled(SnapshotInstalled),
    EventApplied(EventApplied),
    RuntimeRealizationApplied(RuntimeRealizationApplied),
    CommandTracked,
    CommandReconciled(CommandReconciled),
    ServerObserved,
}

/// Validation or ordering failure that leaves authoritative state unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelError {
    ProjectMismatch {
        expected: ProjectId,
        observed: ProjectId,
    },
    ProtocolProjectMismatch {
        expected: ProjectId,
        observed: String,
    },
    EngineMismatch {
        expected: EngineIdentity,
        observed: EngineIdentity,
    },
    InvalidEngineIdentity,
    SnapshotRequired,
    RevisionGap {
        expected: Revision,
        observed: Revision,
    },
    OutOfOrder {
        current: Revision,
        observed: Revision,
    },
    ConflictingDuplicate {
        revision: Revision,
    },
    UnknownDurableRevision {
        revision: Revision,
    },
    RuntimeOutOfOrder {
        current_sequence: u64,
        observed_sequence: u64,
    },
    RuntimeGenerationOutOfOrder {
        current_generation: u64,
        observed_generation: u64,
    },
    ConflictingRuntimeSequence {
        sequence: u64,
    },
    RevisionExhausted,
    DuplicateInput(InputId),
    InvalidInputNames,
    InvalidInputAudioStrips,
    DuplicateStingerSlot(u8),
    InvalidStingerSlot(u8),
    InvalidOverlayCount(usize),
    InvalidOverlayChannel(u8),
    InvalidOverlayTransitionDuration {
        channel: u8,
        duration_frames: u32,
    },
    ActiveOverlayMissingSource(u8),
    InvalidOverlayQueueDepth {
        channel: u8,
        depth: usize,
    },
    DuplicateOverlayOutput(u8),
    UnknownInput(InputId),
    InvalidManualTransitionRouting,
    DuplicateCommand(CommandId),
    UnknownCommand(CommandId),
    StateUnavailable,
}

impl fmt::Display for ModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProjectMismatch { expected, observed } => {
                write!(
                    formatter,
                    "project identity changed from {expected} to {observed}"
                )
            }
            Self::ProtocolProjectMismatch { expected, observed } => write!(
                formatter,
                "protocol project identity changed from {expected} to {observed}"
            ),
            Self::EngineMismatch { .. } => formatter.write_str("engine cursor identity changed"),
            Self::InvalidEngineIdentity => formatter.write_str("engine identity is incomplete"),
            Self::SnapshotRequired => formatter.write_str("a fresh snapshot is required"),
            Self::RevisionGap { expected, observed } => {
                write!(
                    formatter,
                    "event gap: expected revision {expected}, got {observed}"
                )
            }
            Self::OutOfOrder { current, observed } => write!(
                formatter,
                "out-of-order event revision {observed}; current revision is {current}"
            ),
            Self::ConflictingDuplicate { revision } => {
                write!(formatter, "revision {revision} has conflicting event data")
            }
            Self::UnknownDurableRevision { revision } => {
                write!(
                    formatter,
                    "runtime realization references unknown revision {revision}"
                )
            }
            Self::RuntimeOutOfOrder {
                current_sequence,
                observed_sequence,
            } => write!(
                formatter,
                "out-of-order runtime sequence {observed_sequence}; current sequence is {current_sequence}"
            ),
            Self::RuntimeGenerationOutOfOrder {
                current_generation,
                observed_generation,
            } => write!(
                formatter,
                "out-of-order runtime generation {observed_generation}; current generation is {current_generation}"
            ),
            Self::ConflictingRuntimeSequence { sequence } => write!(
                formatter,
                "runtime sequence {sequence} has conflicting realization data"
            ),
            Self::RevisionExhausted => formatter.write_str("revision counter is exhausted"),
            Self::DuplicateInput(input) => write!(formatter, "snapshot repeats input {input}"),
            Self::InvalidInputNames => formatter
                .write_str("input names must contain one nonempty label for each show input"),
            Self::InvalidInputAudioStrips => formatter.write_str(
                "input audio strips must contain each show input exactly once with gain in -96000..=24000 millidB, balance in -10000..=10000 basis points, and delay in 0..=48000 samples",
            ),
            Self::DuplicateStingerSlot(slot) => {
                write!(formatter, "snapshot repeats Stinger slot {slot}")
            }
            Self::InvalidStingerSlot(slot) => write!(formatter, "invalid Stinger slot {slot}"),
            Self::InvalidOverlayCount(count) => {
                write!(
                    formatter,
                    "snapshot contains {count} overlay channels; expected 8"
                )
            }
            Self::InvalidOverlayChannel(channel) => {
                write!(formatter, "invalid or duplicate overlay channel {channel}")
            }
            Self::InvalidOverlayTransitionDuration {
                channel,
                duration_frames,
            } => write!(
                formatter,
                "overlay channel {channel} transition duration {duration_frames} is outside 1..=3600 frames"
            ),
            Self::ActiveOverlayMissingSource(channel) => {
                write!(formatter, "active overlay channel {channel} has no source")
            }
            Self::InvalidOverlayQueueDepth { channel, depth } => write!(
                formatter,
                "overlay channel {channel} queue depth {depth} exceeds 64"
            ),
            Self::DuplicateOverlayOutput(channel) => {
                write!(formatter, "overlay channel {channel} repeats an output")
            }
            Self::UnknownInput(input) => write!(formatter, "unknown input {input}"),
            Self::InvalidManualTransitionRouting => formatter
                .write_str("manual transition endpoints do not match Program and Preview routing"),
            Self::DuplicateCommand(id) => write!(formatter, "command {id} is already pending"),
            Self::UnknownCommand(id) => write!(formatter, "command {id} is not pending"),
            Self::StateUnavailable => formatter.write_str("no project snapshot is installed"),
        }
    }
}

impl std::error::Error for ModelError {}

/// Project-bound replicated client state and reducer.
#[derive(Clone, Debug)]
pub struct ClientModel {
    project_id: ProjectId,
    cursor: Option<ProjectCursor>,
    expected_engine: Option<EngineIdentity>,
    state: Option<ProjectState>,
    applied_events: HashMap<Revision, DurableProjectEvent>,
    desired_by_revision: HashMap<
        Revision,
        (
            BusSelection,
            ManualTransitionStatus,
            FadeToBlackState,
            Vec<OverlayStatus>,
        ),
    >,
    last_runtime_realization: Option<RuntimeRealization>,
    pending: Vec<PendingCommand>,
    last_rejection: Option<RejectedCommand>,
    sync_status: SyncStatus,
    known_revision: Option<Revision>,
}

impl ClientModel {
    #[must_use]
    pub fn new(project_id: ProjectId) -> Self {
        Self {
            project_id,
            cursor: None,
            expected_engine: None,
            state: None,
            applied_events: HashMap::new(),
            desired_by_revision: HashMap::new(),
            last_runtime_realization: None,
            pending: Vec::new(),
            last_rejection: None,
            sync_status: SyncStatus::AwaitingSnapshot,
            known_revision: None,
        }
    }

    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    #[must_use]
    pub const fn state(&self) -> Option<&ProjectState> {
        self.state.as_ref()
    }

    /// Builds an immutable render value by layering pending intent over the
    /// authoritative desired state. Realized state is never optimistic.
    #[must_use]
    pub fn view(&self) -> Option<ClientView> {
        let state = self.state.as_ref()?;
        let cursor = self.cursor.clone()?;
        let mut switcher = state.switcher;
        for command in &self.pending {
            if let Some(change) = command.optimistic {
                change.apply(&mut switcher.desired);
            }
        }
        Some(ClientView {
            cursor,
            show_name: state.show_name.clone(),
            inputs: state.inputs.clone(),
            input_names: state.input_names.clone(),
            input_audio_strips: state.input_audio_strips.clone(),
            stingers: state.stingers.clone(),
            desired_overlays: state.desired_overlays.clone(),
            realized_overlays: state.realized_overlays.clone(),
            switcher,
        })
    }

    #[must_use]
    pub const fn sync_status(&self) -> &SyncStatus {
        &self.sync_status
    }

    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.sync_status.is_stale()
    }

    /// Returns the last validated cursor to send during reconnect.
    #[must_use]
    pub const fn reconnect_cursor(&self) -> Option<&ProjectCursor> {
        self.cursor.as_ref()
    }

    /// Returns the last validated cursor in the current protocol DTO.
    #[must_use]
    pub fn protocol_reconnect_cursor(&self) -> Option<ResumeCursor> {
        self.cursor.as_ref().map(ProjectCursor::resume_cursor)
    }

    #[must_use]
    pub fn pending_commands(&self) -> &[PendingCommand] {
        &self.pending
    }

    #[must_use]
    pub const fn last_rejection(&self) -> Option<&RejectedCommand> {
        self.last_rejection.as_ref()
    }

    /// Reduces an action in place.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError`] when identity, ordering, state, or command
    /// invariants are violated.
    pub fn reduce(&mut self, action: Action) -> Result<Reduction, ModelError> {
        match action {
            Action::InstallSnapshot(snapshot) => self
                .install_snapshot(*snapshot)
                .map(Reduction::SnapshotInstalled),
            Action::ApplyEvent(event) => self.apply_event(event).map(Reduction::EventApplied),
            Action::ApplyRuntimeRealization(realization) => self
                .apply_runtime_realization(realization)
                .map(Reduction::RuntimeRealizationApplied),
            Action::TrackCommand { id, optimistic } => {
                self.track_command(id, optimistic)?;
                Ok(Reduction::CommandTracked)
            }
            Action::ReconcileCommand(result) => self
                .reconcile_command(&result)
                .map(Reduction::CommandReconciled),
            Action::ObserveServer(hello) => {
                self.observe_server(&hello)?;
                Ok(Reduction::ServerObserved)
            }
        }
    }

    /// Clones and reduces this model, leaving the original untouched.
    ///
    /// # Errors
    ///
    /// Returns the same validation errors as [`Self::reduce`].
    pub fn reduced(&self, action: Action) -> Result<(Self, Reduction), ModelError> {
        let mut next = self.clone();
        let reduction = next.reduce(action)?;
        Ok((next, reduction))
    }

    /// Installs a complete authoritative snapshot.
    ///
    /// # Errors
    ///
    /// Rejects the snapshot when its project or expected engine identity does
    /// not match, its engine identity is incomplete, or routing references an
    /// absent or duplicate input.
    pub fn install_snapshot(
        &mut self,
        snapshot: ProjectSnapshot,
    ) -> Result<SnapshotInstalled, ModelError> {
        self.validate_project(snapshot.cursor.project_id)?;
        validate_engine(&snapshot.cursor.engine)?;
        if let Some(expected) = &self.expected_engine
            && expected != &snapshot.cursor.engine
        {
            return Err(ModelError::EngineMismatch {
                expected: expected.clone(),
                observed: snapshot.cursor.engine,
            });
        }
        if let Some(current) = &self.cursor
            && current.engine == snapshot.cursor.engine
            && snapshot.cursor.revision < current.revision
        {
            return Err(ModelError::OutOfOrder {
                current: current.revision,
                observed: snapshot.cursor.revision,
            });
        }
        if self
            .expected_engine
            .as_ref()
            .is_some_and(|expected| expected == &snapshot.cursor.engine)
            && let Some(known) = self.known_revision
            && snapshot.cursor.revision < known
        {
            return Err(ModelError::OutOfOrder {
                current: known,
                observed: snapshot.cursor.revision,
            });
        }
        validate_snapshot_inputs(&snapshot)?;

        let identity_changed = self
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.engine != snapshot.cursor.engine);
        let mut reconciled_commands = Vec::new();
        let mut discarded_commands = Vec::new();
        let input_set: HashSet<_> = snapshot.inputs.iter().copied().collect();
        self.pending.retain(|command| {
            let accepted = matches!(
                command.status,
                PendingStatus::AwaitingEvent { accepted_revision }
                    if accepted_revision <= snapshot.cursor.revision
            );
            let valid = command.optimistic.is_none_or(|change| {
                change
                    .inputs()
                    .into_iter()
                    .flatten()
                    .all(|input| input_set.contains(&input))
            });
            if accepted {
                reconciled_commands.push(command.id.clone());
                false
            } else if identity_changed || !valid {
                discarded_commands.push(command.id.clone());
                false
            } else {
                true
            }
        });

        if identity_changed {
            self.desired_by_revision.clear();
            self.last_runtime_realization = None;
        }
        self.desired_by_revision.insert(
            snapshot.cursor.revision,
            (
                snapshot.switcher.desired,
                snapshot.switcher.desired_manual_transition,
                snapshot.switcher.desired_fade_to_black,
                snapshot.desired_overlays.clone(),
            ),
        );
        self.expected_engine = Some(snapshot.cursor.engine.clone());
        self.known_revision = Some(snapshot.cursor.revision);
        self.cursor = Some(snapshot.cursor);
        self.state = Some(ProjectState {
            show_name: snapshot.show_name,
            inputs: snapshot.inputs,
            input_names: snapshot.input_names,
            input_audio_strips: snapshot.input_audio_strips,
            stingers: snapshot.stingers,
            desired_overlays: snapshot.desired_overlays,
            realized_overlays: snapshot.realized_overlays,
            switcher: snapshot.switcher,
        });
        self.applied_events.clear();
        self.sync_status = SyncStatus::Current;
        Ok(SnapshotInstalled {
            reconciled_commands,
            discarded_commands,
        })
    }

    /// Applies one contiguous, globally ordered event.
    ///
    /// # Errors
    ///
    /// Rejects project/engine changes, gaps, unknown or conflicting old
    /// revisions, invalid input references, and updates before a snapshot.
    pub fn apply_event(&mut self, event: DurableProjectEvent) -> Result<EventApplied, ModelError> {
        self.validate_project(event.cursor.project_id)?;
        validate_engine(&event.cursor.engine)?;
        let Some(cursor) = self.cursor.as_ref() else {
            return Err(ModelError::StateUnavailable);
        };
        if cursor.engine != event.cursor.engine {
            self.sync_status = SyncStatus::RequiresSnapshot;
            return Err(ModelError::EngineMismatch {
                expected: cursor.engine.clone(),
                observed: event.cursor.engine,
            });
        }
        if matches!(self.sync_status, SyncStatus::RequiresSnapshot) {
            return Err(ModelError::SnapshotRequired);
        }
        if let Some(applied) = self.applied_events.get(&event.cursor.revision) {
            return if applied == &event {
                Ok(EventApplied::Duplicate)
            } else {
                self.sync_status = SyncStatus::RequiresSnapshot;
                Err(ModelError::ConflictingDuplicate {
                    revision: event.cursor.revision,
                })
            };
        }
        if event.cursor.revision <= cursor.revision {
            return Err(ModelError::OutOfOrder {
                current: cursor.revision,
                observed: event.cursor.revision,
            });
        }
        let expected = cursor
            .revision
            .checked_next()
            .map_err(|_| ModelError::RevisionExhausted)?;
        if event.cursor.revision != expected {
            self.known_revision =
                Some(self.known_revision.map_or(event.cursor.revision, |known| {
                    known.max(event.cursor.revision)
                }));
            self.sync_status = SyncStatus::Behind {
                local_revision: cursor.revision,
                known_revision: event.cursor.revision,
            };
            return Err(ModelError::RevisionGap {
                expected,
                observed: event.cursor.revision,
            });
        }

        let state = self.state.as_ref().ok_or(ModelError::StateUnavailable)?;
        validate_change(&event.change, &state.inputs)?;
        let state = self.state.as_mut().ok_or(ModelError::StateUnavailable)?;
        apply_change(state, event.change.clone());

        let revision = event.cursor.revision;
        self.desired_by_revision.insert(
            revision,
            (
                state.switcher.desired,
                state.switcher.desired_manual_transition,
                state.switcher.desired_fade_to_black,
                state.desired_overlays.clone(),
            ),
        );
        self.cursor = Some(event.cursor.clone());
        self.applied_events.insert(revision, event);
        let reconciled_commands = self.reconcile_through(revision);
        let known = self
            .known_revision
            .map_or(revision, |value| value.max(revision));
        self.known_revision = Some(known);
        self.sync_status = if known > revision {
            SyncStatus::Behind {
                local_revision: revision,
                known_revision: known,
            }
        } else {
            SyncStatus::Current
        };
        Ok(EventApplied::Applied {
            reconciled_commands,
        })
    }

    /// Applies runtime routing for a known durable desired-state revision.
    ///
    /// This path has independent ordering and intentionally leaves the durable
    /// cursor, known revision, sync status, and command reconciliation intact.
    ///
    /// # Errors
    ///
    /// Rejects project/engine changes, unknown durable revisions, and stale or
    /// conflicting runtime sequences.
    pub fn apply_runtime_realization(
        &mut self,
        realization: RuntimeRealization,
    ) -> Result<RuntimeRealizationApplied, ModelError> {
        self.validate_project(realization.project_id)?;
        validate_engine(&realization.engine)?;
        let cursor = self.cursor.as_ref().ok_or(ModelError::StateUnavailable)?;
        if cursor.engine != realization.engine {
            return Err(ModelError::EngineMismatch {
                expected: cursor.engine.clone(),
                observed: realization.engine,
            });
        }
        if let Some(previous) = &self.last_runtime_realization {
            match realization.generation.cmp(&previous.generation) {
                Ordering::Less => {
                    return Err(ModelError::RuntimeGenerationOutOfOrder {
                        current_generation: previous.generation,
                        observed_generation: realization.generation,
                    });
                }
                Ordering::Greater => {}
                Ordering::Equal => match realization.sequence.cmp(&previous.sequence) {
                    Ordering::Less => {
                        return Err(ModelError::RuntimeOutOfOrder {
                            current_sequence: previous.sequence,
                            observed_sequence: realization.sequence,
                        });
                    }
                    Ordering::Equal if previous == &realization => {
                        return Ok(RuntimeRealizationApplied::Duplicate);
                    }
                    Ordering::Equal => {
                        return Err(ModelError::ConflictingRuntimeSequence {
                            sequence: realization.sequence,
                        });
                    }
                    Ordering::Greater => {}
                },
            }
        }
        let (desired, _, _, desired_overlays) = self
            .desired_by_revision
            .get(&realization.revision)
            .cloned()
            .ok_or(ModelError::UnknownDurableRevision {
                revision: realization.revision,
            })?;
        let realized_manual_transition = realization.manual_transition;
        let realized_fade_to_black = realization.fade_to_black;
        let state = self.state.as_ref().ok_or(ModelError::StateUnavailable)?;
        let inputs = state.inputs.iter().copied().collect();
        validate_manual_transition(realized_manual_transition, desired, &inputs)?;
        let state = self.state.as_mut().ok_or(ModelError::StateUnavailable)?;
        state.switcher.realized = desired;
        state.switcher.realized_manual_transition = realized_manual_transition;
        state.switcher.realized_fade_to_black = realized_fade_to_black;
        state.realized_overlays = desired_overlays;
        state.switcher.runtime_generation = Some(realization.generation);
        self.last_runtime_realization = Some(realization);
        Ok(RuntimeRealizationApplied::Applied)
    }

    /// Registers a command before it is sent.
    ///
    /// # Errors
    ///
    /// Rejects duplicate command IDs, optimistic references to unknown inputs,
    /// and registration before a snapshot is installed.
    pub fn track_command(
        &mut self,
        id: CommandId,
        optimistic: Option<OptimisticChange>,
    ) -> Result<(), ModelError> {
        if self.pending.iter().any(|command| command.id == id) {
            return Err(ModelError::DuplicateCommand(id));
        }
        let state = self.state.as_ref().ok_or(ModelError::StateUnavailable)?;
        if let Some(change) = optimistic {
            validate_optimistic(change, &state.inputs)?;
        }
        self.pending.push(PendingCommand {
            id,
            optimistic,
            status: PendingStatus::AwaitingResult,
        });
        Ok(())
    }

    /// Reconciles a protocol command result by command ID.
    ///
    /// Accepted optimistic state remains visible until its ordered durable
    /// revision is applied. Rejection removes it immediately.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::UnknownCommand`] when the result does not match an
    /// outstanding command.
    pub fn reconcile_command(
        &mut self,
        result: &CommandResult,
    ) -> Result<CommandReconciled, ModelError> {
        match result {
            CommandResult::Accepted { id, revision, .. } => {
                let id = CommandId::new(id.clone());
                let index = self.pending_index(&id)?;
                let revision = Revision::new(*revision);
                let current = self.cursor.as_ref().map(|cursor| cursor.revision);
                let awaiting_event = current.is_none_or(|value| value < revision);
                if awaiting_event {
                    self.pending[index].status = PendingStatus::AwaitingEvent {
                        accepted_revision: revision,
                    };
                    self.note_known_revision(revision);
                } else {
                    self.pending.remove(index);
                }
                Ok(CommandReconciled::Accepted {
                    revision,
                    awaiting_event,
                })
            }
            CommandResult::Rejected {
                id,
                code,
                message,
                fields,
                current_revision,
                retryable,
            } => {
                let id = CommandId::new(id.clone());
                let index = self.pending_index(&id)?;
                self.pending.remove(index);
                let rejection = RejectedCommand {
                    id,
                    code: code.clone(),
                    message: message.clone(),
                    fields: fields.clone(),
                    current_revision: Revision::new(*current_revision),
                    retryable: *retryable,
                };
                self.note_known_revision(rejection.current_revision);
                self.last_rejection = Some(rejection.clone());
                Ok(CommandReconciled::Rejected(rejection))
            }
        }
    }

    /// Validates a server handshake against the reconnect cursor and updates
    /// staleness from the server's current revision.
    ///
    /// # Errors
    ///
    /// Rejects an incomplete engine identity. Identity replacement itself is
    /// represented by [`SyncStatus::RequiresSnapshot`], as required by the
    /// reconnect protocol.
    pub fn observe_server(&mut self, hello: &ServerHello) -> Result<(), ModelError> {
        validate_engine(&hello.engine)?;
        self.expected_engine = Some(hello.engine.clone());
        let known = Revision::new(hello.current_revision);
        self.known_revision = Some(known);
        let Some(cursor) = &self.cursor else {
            self.sync_status = SyncStatus::AwaitingSnapshot;
            return Ok(());
        };
        if cursor.engine != hello.engine || !hello.resume {
            self.sync_status = SyncStatus::RequiresSnapshot;
            return Ok(());
        }
        self.sync_status = match known.cmp(&cursor.revision) {
            Ordering::Equal => SyncStatus::Current,
            Ordering::Greater => SyncStatus::Behind {
                local_revision: cursor.revision,
                known_revision: known,
            },
            Ordering::Less => SyncStatus::RequiresSnapshot,
        };
        Ok(())
    }

    fn validate_project(&self, observed: ProjectId) -> Result<(), ModelError> {
        if observed == self.project_id {
            Ok(())
        } else {
            Err(ModelError::ProjectMismatch {
                expected: self.project_id,
                observed,
            })
        }
    }

    fn pending_index(&self, id: &CommandId) -> Result<usize, ModelError> {
        self.pending
            .iter()
            .position(|command| &command.id == id)
            .ok_or_else(|| ModelError::UnknownCommand(id.clone()))
    }

    fn reconcile_through(&mut self, revision: Revision) -> Vec<CommandId> {
        let mut reconciled = Vec::new();
        self.pending.retain(|command| {
            if matches!(
                command.status,
                PendingStatus::AwaitingEvent { accepted_revision }
                    if accepted_revision <= revision
            ) {
                reconciled.push(command.id.clone());
                false
            } else {
                true
            }
        });
        reconciled
    }

    fn note_known_revision(&mut self, revision: Revision) {
        self.known_revision = Some(
            self.known_revision
                .map_or(revision, |known| known.max(revision)),
        );
        if let Some(cursor) = &self.cursor
            && revision > cursor.revision
            && !matches!(self.sync_status, SyncStatus::RequiresSnapshot)
        {
            self.sync_status = SyncStatus::Behind {
                local_revision: cursor.revision,
                known_revision: revision,
            };
        }
    }
}

fn validate_engine(engine: &EngineIdentity) -> Result<(), ModelError> {
    if engine.engine_id.is_empty() || engine.log_id.is_empty() {
        Err(ModelError::InvalidEngineIdentity)
    } else {
        Ok(())
    }
}

fn validate_snapshot_inputs(snapshot: &ProjectSnapshot) -> Result<(), ModelError> {
    if snapshot.input_names.len() != snapshot.inputs.len()
        || snapshot
            .input_names
            .iter()
            .any(|name| name.trim().is_empty())
    {
        return Err(ModelError::InvalidInputNames);
    }
    let mut inputs = HashSet::with_capacity(snapshot.inputs.len());
    for input in &snapshot.inputs {
        if !inputs.insert(*input) {
            return Err(ModelError::DuplicateInput(*input));
        }
    }
    for input in [
        snapshot.switcher.desired.program,
        snapshot.switcher.desired.preview,
        snapshot.switcher.realized.program,
        snapshot.switcher.realized.preview,
    ] {
        if !inputs.contains(&input) {
            return Err(ModelError::UnknownInput(input));
        }
    }
    validate_stingers(&snapshot.stingers, &inputs)?;
    validate_input_audio_strips(&snapshot.input_audio_strips, &inputs)?;
    validate_overlays(&snapshot.desired_overlays, &inputs)?;
    validate_overlays(&snapshot.realized_overlays, &inputs)?;
    validate_manual_transition(
        snapshot.switcher.desired_manual_transition,
        snapshot.switcher.desired,
        &inputs,
    )?;
    validate_manual_transition(
        snapshot.switcher.realized_manual_transition,
        snapshot.switcher.realized,
        &inputs,
    )?;
    Ok(())
}

fn validate_overlays(
    overlays: &[OverlayStatus],
    inputs: &HashSet<InputId>,
) -> Result<(), ModelError> {
    if overlays.len() != 8 {
        return Err(ModelError::InvalidOverlayCount(overlays.len()));
    }
    let mut channels = HashSet::with_capacity(8);
    for overlay in overlays {
        if !(1..=8).contains(&overlay.channel) || !channels.insert(overlay.channel) {
            return Err(ModelError::InvalidOverlayChannel(overlay.channel));
        }
        if overlay.active && overlay.source.is_none() {
            return Err(ModelError::ActiveOverlayMissingSource(overlay.channel));
        }
        if !(1..=3_600).contains(&overlay.duration_frames) {
            return Err(ModelError::InvalidOverlayTransitionDuration {
                channel: overlay.channel,
                duration_frames: overlay.duration_frames,
            });
        }
        if let Some(source) = overlay.source
            && !inputs.contains(&source)
        {
            return Err(ModelError::UnknownInput(source));
        }
        if overlay.queued_sources.len() > 64 {
            return Err(ModelError::InvalidOverlayQueueDepth {
                channel: overlay.channel,
                depth: overlay.queued_sources.len(),
            });
        }
        for source in &overlay.queued_sources {
            if !inputs.contains(source) {
                return Err(ModelError::UnknownInput(*source));
            }
        }
        let outputs = overlay
            .included_outputs
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        if outputs.len() != overlay.included_outputs.len() {
            return Err(ModelError::DuplicateOverlayOutput(overlay.channel));
        }
    }
    Ok(())
}

fn validate_stingers(
    stingers: &[StingerStatus],
    inputs: &HashSet<InputId>,
) -> Result<(), ModelError> {
    let mut slots = HashSet::with_capacity(stingers.len());
    for stinger in stingers {
        if !(1..=8).contains(&stinger.slot) {
            return Err(ModelError::InvalidStingerSlot(stinger.slot));
        }
        if !slots.insert(stinger.slot) {
            return Err(ModelError::DuplicateStingerSlot(stinger.slot));
        }
        if !inputs.contains(&stinger.media_input) {
            return Err(ModelError::UnknownInput(stinger.media_input));
        }
    }
    Ok(())
}

fn validate_input_audio_strips(
    strips: &[InputAudioStripStatus],
    inputs: &HashSet<InputId>,
) -> Result<(), ModelError> {
    if strips.len() != inputs.len() {
        return Err(ModelError::InvalidInputAudioStrips);
    }
    let mut seen = HashSet::with_capacity(strips.len());
    if strips.iter().any(|status| {
        !(-96_000..=24_000).contains(&status.gain_millidb)
            || !(-10_000..=10_000).contains(&status.balance_basis_points)
            || status.delay_samples > 48_000
            || !inputs.contains(&status.input)
            || !seen.insert(status.input)
    }) {
        return Err(ModelError::InvalidInputAudioStrips);
    }
    Ok(())
}

fn validate_change(change: &DurableChange, inputs: &[InputId]) -> Result<(), ModelError> {
    let (selection, manual_transition, stingers, overlays, input_audio_strips) = match change {
        DurableChange::DesiredSwitcher {
            selection,
            manual_transition,
            overlays,
            input_audio_strips,
            ..
        } => (
            *selection,
            *manual_transition,
            None,
            overlays,
            input_audio_strips,
        ),
        DurableChange::StingerSlotsChanged {
            selection,
            manual_transition,
            stingers,
            overlays,
            input_audio_strips,
            ..
        } => (
            *selection,
            *manual_transition,
            Some(stingers.as_slice()),
            overlays,
            input_audio_strips,
        ),
    };
    for input in [selection.program, selection.preview] {
        if !inputs.contains(&input) {
            return Err(ModelError::UnknownInput(input));
        }
    }
    let input_set = inputs.iter().copied().collect();
    validate_manual_transition(manual_transition, selection, &input_set)?;
    if let Some(stingers) = stingers {
        validate_stingers(stingers, &input_set)?;
    }
    validate_overlays(overlays, &input_set)?;
    validate_input_audio_strips(input_audio_strips, &input_set)?;
    Ok(())
}

fn validate_optimistic(change: OptimisticChange, inputs: &[InputId]) -> Result<(), ModelError> {
    for input in change.inputs().into_iter().flatten() {
        if !inputs.contains(&input) {
            return Err(ModelError::UnknownInput(input));
        }
    }
    Ok(())
}

fn apply_change(state: &mut ProjectState, change: DurableChange) {
    match change {
        DurableChange::DesiredSwitcher {
            selection,
            manual_transition,
            fade_to_black,
            overlays,
            input_audio_strips,
        } => {
            state.switcher.desired = selection;
            state.switcher.desired_manual_transition = manual_transition;
            state.switcher.desired_fade_to_black = fade_to_black;
            state.desired_overlays = overlays;
            state.input_audio_strips = input_audio_strips;
        }
        DurableChange::StingerSlotsChanged {
            selection,
            manual_transition,
            fade_to_black,
            stingers,
            overlays,
            input_audio_strips,
        } => {
            state.switcher.desired = selection;
            state.switcher.desired_manual_transition = manual_transition;
            state.switcher.desired_fade_to_black = fade_to_black;
            state.stingers = stingers;
            state.desired_overlays = overlays;
            state.input_audio_strips = input_audio_strips;
        }
    }
}

fn protocol_stingers(stingers: Vec<fm_protocol::StingerStatus>) -> Vec<StingerStatus> {
    stingers
        .into_iter()
        .map(|status| StingerStatus {
            slot: status.slot.number(),
            media_input: status.media_input.to_domain(),
            preload: status.preload,
            cut_point_frames: status.cut_point_frames,
            audio_policy: status.audio_policy,
            missing_media_fallback: status.missing_media_fallback,
            readiness: status.readiness,
        })
        .collect()
}

fn protocol_input_audio_strips(
    strips: Vec<fm_protocol::InputAudioStripStatus>,
) -> Vec<InputAudioStripStatus> {
    strips
        .into_iter()
        .map(|status| InputAudioStripStatus {
            input: status.input.to_domain(),
            gain_millidb: status.gain_millidb,
            balance_basis_points: status.balance_basis_points,
            muted: status.muted,
            soloed: status.soloed,
            follow_video: status.follow_video,
            delay_samples: status.delay_samples,
        })
        .collect()
}

fn protocol_overlays(overlays: Vec<fm_protocol::OverlayStatus>) -> Vec<OverlayStatus> {
    overlays
        .into_iter()
        .map(|status| OverlayStatus {
            channel: status.channel.number(),
            source: status.source.map(fm_protocol::WireInputId::to_domain),
            active: status.active,
            opacity: status.opacity,
            transition: status.transition,
            duration_frames: status.duration_frames,
            position: status.position,
            border: status.border,
            queued_sources: status
                .queued_sources
                .into_iter()
                .map(fm_protocol::WireInputId::to_domain)
                .collect(),
            included_outputs: status
                .included_outputs
                .into_iter()
                .map(fm_protocol::WireOutputId::to_domain)
                .collect(),
        })
        .collect()
}

fn manual_status_from_protocol(status: ProtocolManualTransitionStatus) -> ManualTransitionStatus {
    ManualTransitionStatus::from_protocol(status)
}

fn fade_to_black_from_protocol(state: FadeToBlackState) -> FadeToBlackState {
    state
}

fn validate_manual_transition(
    status: ManualTransitionStatus,
    selection: BusSelection,
    inputs: &HashSet<InputId>,
) -> Result<(), ModelError> {
    let ManualTransitionStatus::Active(state) = status else {
        return Ok(());
    };
    for input in [state.from, state.to] {
        if !inputs.contains(&input) {
            return Err(ModelError::UnknownInput(input));
        }
    }
    if state.from != selection.program || state.to != selection.preview {
        return Err(ModelError::InvalidManualTransitionRouting);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
