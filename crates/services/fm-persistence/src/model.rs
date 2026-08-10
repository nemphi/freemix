use std::{collections::BTreeSet, error::Error, fmt};

use fm_model::{Project, ValidationError};
use fm_types::{InputId, OutputId};

pub const CURRENT_SCHEMA_VERSION: u32 = fm_model::CURRENT_SCHEMA_VERSION.get();

/// Desired and observed switcher selections persisted independently.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimeRouting {
    pub desired_program_id: Option<InputId>,
    pub realized_program_id: Option<InputId>,
    pub desired_preview_id: Option<InputId>,
    pub realized_preview_id: Option<InputId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManualTransitionKind {
    Fade,
    Wipe,
    AlphaFade,
    Slide,
}

/// Exact state of one active manual transition at a frame boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManualTransitionState {
    pub kind: ManualTransitionKind,
    pub from_id: InputId,
    pub to_id: InputId,
    pub interval_start_basis_points: u16,
    pub position_basis_points: u16,
}

impl ManualTransitionState {
    pub const MAX_POSITION: u16 = 10_000;

    #[must_use]
    pub const fn new(
        kind: ManualTransitionKind,
        from_id: InputId,
        to_id: InputId,
        interval_start_basis_points: u16,
        position_basis_points: u16,
    ) -> Option<Self> {
        if interval_start_basis_points <= Self::MAX_POSITION
            && position_basis_points <= Self::MAX_POSITION
        {
            Some(Self {
                kind,
                from_id,
                to_id,
                interval_start_basis_points,
                position_basis_points,
            })
        } else {
            None
        }
    }
}

/// Desired and realized manual-transition state persisted independently.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimeManualTransitions {
    pub desired: Option<ManualTransitionState>,
    pub realized: Option<ManualTransitionState>,
}

/// Exact settled Fade-to-Black state at an idle checkpoint.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FadeToBlackState {
    pub target_active: bool,
    pub position_numerator: u16,
}

impl FadeToBlackState {
    pub const LIVE: Self = Self {
        target_active: false,
        position_numerator: 0,
    };
    pub const BLACK: Self = Self {
        target_active: true,
        position_numerator: u16::MAX,
    };

    #[must_use]
    pub const fn new(target_active: bool, position_numerator: u16) -> Self {
        Self {
            target_active,
            position_numerator,
        }
    }

    #[must_use]
    pub const fn is_settled(self) -> bool {
        if self.target_active {
            self.position_numerator == u16::MAX
        } else {
            self.position_numerator == 0
        }
    }
}

/// Desired and realized Fade-to-Black checkpoint state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimeFadeToBlack {
    pub desired: FadeToBlackState,
    pub realized: FadeToBlackState,
}

pub const OVERLAY_CHANNEL_COUNT: usize = 8;
pub const MAX_OVERLAY_TRANSITION_DURATION_FRAMES: u32 = 3_600;
pub const MAX_OVERLAY_QUEUE_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RuntimeOverlayTransition {
    #[default]
    Cut,
    Fade,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RuntimeOverlayPosition {
    #[default]
    FullFrame,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RuntimeOverlayBorder {
    #[default]
    None,
    ThinWhite,
    ThickWhite,
}

/// Complete desired/realized state for one downstream overlay channel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeOverlayChannel {
    pub source: Option<InputId>,
    pub active: bool,
    pub transition: RuntimeOverlayTransition,
    pub duration_frames: u32,
    pub position: RuntimeOverlayPosition,
    pub border: RuntimeOverlayBorder,
    pub queued_sources: Vec<InputId>,
    pub included_outputs: Vec<OutputId>,
}

impl RuntimeOverlayChannel {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            source: None,
            active: false,
            transition: RuntimeOverlayTransition::Cut,
            duration_frames: 1,
            position: RuntimeOverlayPosition::FullFrame,
            border: RuntimeOverlayBorder::None,
            queued_sources: Vec::new(),
            included_outputs: Vec::new(),
        }
    }
}

/// Exact desired and realized overlay state at an idle checkpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeOverlays {
    pub desired: [RuntimeOverlayChannel; OVERLAY_CHANNEL_COUNT],
    pub realized: [RuntimeOverlayChannel; OVERLAY_CHANNEL_COUNT],
}

impl Default for RuntimeOverlays {
    fn default() -> Self {
        Self {
            desired: std::array::from_fn(|_| RuntimeOverlayChannel::empty()),
            realized: std::array::from_fn(|_| RuntimeOverlayChannel::empty()),
        }
    }
}

/// Monotonic durable project coordinates.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProjectPosition {
    pub revision: u64,
    pub state_epoch: u64,
    pub event_sequence: u64,
    pub frames_rendered: u64,
    pub runtime_generation: u64,
    pub clock_time_nanos: u64,
}

/// Engine-independent result retained for idempotent command replay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReceiptOutcome {
    Accepted {
        revision: u64,
        target_frame: u64,
    },
    Rejected {
        current_revision: u64,
        code: String,
        message: String,
        retryable: bool,
    },
}

/// Durable command receipt indexed by an idempotency key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdempotencyReceipt {
    key: String,
    command_id: String,
    outcome: ReceiptOutcome,
}

impl IdempotencyReceipt {
    #[must_use]
    pub fn accepted(
        key: impl Into<String>,
        command_id: impl Into<String>,
        revision: u64,
        target_frame: u64,
    ) -> Self {
        Self {
            key: key.into(),
            command_id: command_id.into(),
            outcome: ReceiptOutcome::Accepted {
                revision,
                target_frame,
            },
        }
    }

    #[must_use]
    pub fn rejected(
        key: impl Into<String>,
        command_id: impl Into<String>,
        current_revision: u64,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self {
            key: key.into(),
            command_id: command_id.into(),
            outcome: ReceiptOutcome::Rejected {
                current_revision,
                code: code.into(),
                message: message.into(),
                retryable,
            },
        }
    }

    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    #[must_use]
    pub fn command_id(&self) -> &str {
        &self.command_id
    }

    #[must_use]
    pub const fn outcome(&self) -> &ReceiptOutcome {
        &self.outcome
    }
}

/// A canonical project configuration and its durable runtime state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredProject {
    project: Project,
    routing: RuntimeRouting,
    manual_transitions: RuntimeManualTransitions,
    fade_to_black: RuntimeFadeToBlack,
    overlays: RuntimeOverlays,
    position: ProjectPosition,
    idempotency_receipts: Vec<IdempotencyReceipt>,
}

impl StoredProject {
    /// Creates a manifest from the canonical domain project and runtime state.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectValidationError`] when the domain project or runtime
    /// metadata is inconsistent.
    pub fn from_project(
        project: Project,
        routing: RuntimeRouting,
        position: ProjectPosition,
        idempotency_receipts: Vec<IdempotencyReceipt>,
    ) -> Result<Self, ProjectValidationError> {
        Self::from_project_with_manual_transitions(
            project,
            routing,
            RuntimeManualTransitions::default(),
            position,
            idempotency_receipts,
        )
    }

    /// Creates a manifest with exact desired and realized manual-transition state.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectValidationError`] when the domain project or runtime
    /// metadata is inconsistent.
    pub fn from_project_with_manual_transitions(
        project: Project,
        routing: RuntimeRouting,
        manual_transitions: RuntimeManualTransitions,
        position: ProjectPosition,
        idempotency_receipts: Vec<IdempotencyReceipt>,
    ) -> Result<Self, ProjectValidationError> {
        Self::from_project_with_runtime_state(
            project,
            routing,
            manual_transitions,
            RuntimeFadeToBlack::default(),
            position,
            idempotency_receipts,
        )
    }

    /// Creates a manifest with all exact switcher runtime checkpoint state.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectValidationError`] when the domain project or runtime
    /// metadata is inconsistent or Fade-to-Black is not settled.
    pub fn from_project_with_runtime_state(
        project: Project,
        routing: RuntimeRouting,
        manual_transitions: RuntimeManualTransitions,
        fade_to_black: RuntimeFadeToBlack,
        position: ProjectPosition,
        idempotency_receipts: Vec<IdempotencyReceipt>,
    ) -> Result<Self, ProjectValidationError> {
        Self::from_project_with_complete_runtime_state(
            project,
            routing,
            manual_transitions,
            fade_to_black,
            RuntimeOverlays::default(),
            position,
            idempotency_receipts,
        )
    }

    /// Creates a manifest using the complete current runtime contract.
    ///
    /// # Errors
    ///
    /// Returns a validation error when project references or persisted runtime
    /// state do not satisfy the current schema.
    pub fn from_project_with_complete_runtime_state(
        project: Project,
        routing: RuntimeRouting,
        manual_transitions: RuntimeManualTransitions,
        fade_to_black: RuntimeFadeToBlack,
        overlays: RuntimeOverlays,
        position: ProjectPosition,
        mut idempotency_receipts: Vec<IdempotencyReceipt>,
    ) -> Result<Self, ProjectValidationError> {
        idempotency_receipts.sort_by(|left, right| left.key.cmp(&right.key));
        let stored = Self {
            project,
            routing,
            manual_transitions,
            fade_to_black,
            overlays,
            position,
            idempotency_receipts,
        };
        stored.validate()?;
        Ok(stored)
    }

    /// Revalidates all manifest invariants.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectValidationError`] when an invariant is violated.
    pub fn validate(&self) -> Result<(), ProjectValidationError> {
        if self.project.schema_version() != fm_model::CURRENT_SCHEMA_VERSION {
            return Err(ProjectValidationError::UnsupportedSchema {
                found: self.project.schema_version().get(),
                supported: CURRENT_SCHEMA_VERSION,
            });
        }
        self.project
            .validate()
            .map_err(ProjectValidationError::DomainProject)?;

        for (field, selection) in [
            (
                ReferenceField::DesiredProgram,
                self.routing.desired_program_id,
            ),
            (
                ReferenceField::RealizedProgram,
                self.routing.realized_program_id,
            ),
            (
                ReferenceField::DesiredPreview,
                self.routing.desired_preview_id,
            ),
            (
                ReferenceField::RealizedPreview,
                self.routing.realized_preview_id,
            ),
        ] {
            if let Some(id) = selection
                && !self.project.inputs().iter().any(|input| input.id == id)
            {
                return Err(ProjectValidationError::MissingInputReference { field, id });
            }
        }

        self.validate_manual_transitions()?;
        self.validate_fade_to_black()?;
        self.validate_overlays()?;

        let mut keys = BTreeSet::new();
        for receipt in &self.idempotency_receipts {
            if receipt.key.trim().is_empty() {
                return Err(ProjectValidationError::EmptyIdempotencyKey);
            }
            if receipt.command_id.trim().is_empty() {
                return Err(ProjectValidationError::EmptyCommandId {
                    key: receipt.key.clone(),
                });
            }
            if !keys.insert(receipt.key.as_str()) {
                return Err(ProjectValidationError::DuplicateIdempotencyKey {
                    key: receipt.key.clone(),
                });
            }
            let receipt_revision = match &receipt.outcome {
                ReceiptOutcome::Accepted { revision, .. } => *revision,
                ReceiptOutcome::Rejected {
                    current_revision,
                    code,
                    ..
                } => {
                    if code.trim().is_empty() {
                        return Err(ProjectValidationError::EmptyRejectionCode {
                            key: receipt.key.clone(),
                        });
                    }
                    *current_revision
                }
            };
            if receipt_revision > self.position.revision {
                return Err(ProjectValidationError::ReceiptRevisionAhead {
                    key: receipt.key.clone(),
                    receipt_revision,
                    project_revision: self.position.revision,
                });
            }
        }
        Ok(())
    }

    fn validate_manual_transitions(&self) -> Result<(), ProjectValidationError> {
        for (state, program, preview) in [
            (
                self.manual_transitions.desired,
                self.routing.desired_program_id,
                self.routing.desired_preview_id,
            ),
            (
                self.manual_transitions.realized,
                self.routing.realized_program_id,
                self.routing.realized_preview_id,
            ),
        ] {
            if let Some(state) = state {
                if state.interval_start_basis_points > ManualTransitionState::MAX_POSITION
                    || state.position_basis_points > ManualTransitionState::MAX_POSITION
                {
                    return Err(ProjectValidationError::InvalidManualTransitionPosition);
                }
                if Some(state.from_id) != program || Some(state.to_id) != preview {
                    return Err(ProjectValidationError::ManualTransitionRoutingMismatch);
                }
            }
        }
        if self
            .manual_transitions
            .desired
            .is_some_and(|state| state.interval_start_basis_points != 0)
        {
            return Err(ProjectValidationError::InvalidDesiredManualTransitionInterval);
        }
        if self
            .manual_transitions
            .realized
            .is_some_and(|state| state.interval_start_basis_points != state.position_basis_points)
        {
            return Err(ProjectValidationError::InvalidRealizedManualTransitionInterval);
        }
        Ok(())
    }

    fn validate_fade_to_black(&self) -> Result<(), ProjectValidationError> {
        if !self.fade_to_black.desired.is_settled() || !self.fade_to_black.realized.is_settled() {
            return Err(ProjectValidationError::UnsettledFadeToBlack);
        }
        if self.fade_to_black.desired != self.fade_to_black.realized {
            return Err(ProjectValidationError::FadeToBlackCheckpointMismatch);
        }
        Ok(())
    }

    fn validate_overlays(&self) -> Result<(), ProjectValidationError> {
        if self.overlays.desired != self.overlays.realized {
            return Err(ProjectValidationError::OverlayCheckpointMismatch);
        }
        for (index, channel) in self.overlays.desired.iter().enumerate() {
            if !(1..=MAX_OVERLAY_TRANSITION_DURATION_FRAMES).contains(&channel.duration_frames) {
                return Err(ProjectValidationError::InvalidOverlayTransitionDuration {
                    channel: index + 1,
                    duration_frames: channel.duration_frames,
                });
            }
            if channel.active && channel.source.is_none() {
                return Err(ProjectValidationError::ActiveOverlayMissingSource {
                    channel: index + 1,
                });
            }
            if let Some(source) = channel.source
                && !self.project.inputs().iter().any(|input| input.id == source)
            {
                return Err(ProjectValidationError::MissingOverlayInput {
                    channel: index + 1,
                    input: source,
                });
            }
            if channel.queued_sources.len() > MAX_OVERLAY_QUEUE_DEPTH {
                return Err(ProjectValidationError::OverlayQueueTooDeep {
                    channel: index + 1,
                    depth: channel.queued_sources.len(),
                    maximum: MAX_OVERLAY_QUEUE_DEPTH,
                });
            }
            for source in &channel.queued_sources {
                if !self
                    .project
                    .inputs()
                    .iter()
                    .any(|input| input.id == *source)
                {
                    return Err(ProjectValidationError::MissingOverlayInput {
                        channel: index + 1,
                        input: *source,
                    });
                }
            }
            let mut outputs = BTreeSet::new();
            for output in &channel.included_outputs {
                if !self
                    .project
                    .outputs()
                    .iter()
                    .any(|candidate| candidate.id == *output)
                {
                    return Err(ProjectValidationError::MissingOverlayOutput {
                        channel: index + 1,
                        output: *output,
                    });
                }
                if !outputs.insert(*output) {
                    return Err(ProjectValidationError::DuplicateOverlayOutput {
                        channel: index + 1,
                        output: *output,
                    });
                }
            }
        }
        Ok(())
    }

    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        CURRENT_SCHEMA_VERSION
    }

    #[must_use]
    pub const fn project(&self) -> &Project {
        &self.project
    }

    #[must_use]
    pub const fn runtime_routing(&self) -> RuntimeRouting {
        self.routing
    }

    #[must_use]
    pub const fn runtime_manual_transitions(&self) -> RuntimeManualTransitions {
        self.manual_transitions
    }

    #[must_use]
    pub const fn runtime_fade_to_black(&self) -> RuntimeFadeToBlack {
        self.fade_to_black
    }

    #[must_use]
    pub const fn runtime_overlays(&self) -> &RuntimeOverlays {
        &self.overlays
    }

    #[must_use]
    pub fn show_name(&self) -> &str {
        self.project.name()
    }

    #[must_use]
    pub const fn position(&self) -> ProjectPosition {
        self.position
    }

    #[must_use]
    pub fn idempotency_receipts(&self) -> &[IdempotencyReceipt] {
        &self.idempotency_receipts
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReferenceField {
    DesiredProgram,
    RealizedProgram,
    DesiredPreview,
    RealizedPreview,
}

impl fmt::Display for ReferenceField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DesiredProgram => "desired_program_id",
            Self::RealizedProgram => "realized_program_id",
            Self::DesiredPreview => "desired_preview_id",
            Self::RealizedPreview => "realized_preview_id",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectValidationError {
    UnsupportedSchema {
        found: u32,
        supported: u32,
    },
    DomainProject(Vec<ValidationError>),
    MissingInputReference {
        field: ReferenceField,
        id: InputId,
    },
    InvalidManualTransitionPosition,
    ManualTransitionRoutingMismatch,
    InvalidDesiredManualTransitionInterval,
    InvalidRealizedManualTransitionInterval,
    UnsettledFadeToBlack,
    FadeToBlackCheckpointMismatch,
    OverlayCheckpointMismatch,
    InvalidOverlayTransitionDuration {
        channel: usize,
        duration_frames: u32,
    },
    ActiveOverlayMissingSource {
        channel: usize,
    },
    MissingOverlayInput {
        channel: usize,
        input: InputId,
    },
    MissingOverlayOutput {
        channel: usize,
        output: OutputId,
    },
    DuplicateOverlayOutput {
        channel: usize,
        output: OutputId,
    },
    OverlayQueueTooDeep {
        channel: usize,
        depth: usize,
        maximum: usize,
    },
    EmptyIdempotencyKey,
    EmptyCommandId {
        key: String,
    },
    DuplicateIdempotencyKey {
        key: String,
    },
    EmptyRejectionCode {
        key: String,
    },
    ReceiptRevisionAhead {
        key: String,
        receipt_revision: u64,
        project_revision: u64,
    },
}

impl fmt::Display for ProjectValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchema { found, supported } => write!(
                formatter,
                "unsupported schema {found}; expected {supported}"
            ),
            Self::DomainProject(errors) => write!(
                formatter,
                "domain project failed validation with {} error(s)",
                errors.len()
            ),
            Self::MissingInputReference { field, id } => {
                write!(formatter, "{field} references missing input ID {id}")
            }
            Self::InvalidManualTransitionPosition => {
                formatter.write_str("manual transition position exceeds 10000 basis points")
            }
            Self::ManualTransitionRoutingMismatch => formatter.write_str(
                "manual transition from/to routes do not match persisted program/preview routing",
            ),
            Self::InvalidDesiredManualTransitionInterval => formatter.write_str(
                "desired manual transition interval must start at zero at an idle checkpoint",
            ),
            Self::InvalidRealizedManualTransitionInterval => formatter.write_str(
                "realized manual transition interval start must equal its position at an idle checkpoint",
            ),
            Self::UnsettledFadeToBlack => formatter.write_str(
                "fade-to-black position must match its target at an idle checkpoint",
            ),
            Self::FadeToBlackCheckpointMismatch => formatter.write_str(
                "desired and realized fade-to-black state must match at an idle checkpoint",
            ),
            Self::OverlayCheckpointMismatch => formatter.write_str(
                "desired and realized overlay state must match at an idle checkpoint",
            ),
            Self::InvalidOverlayTransitionDuration {
                channel,
                duration_frames,
            } => write!(
                formatter,
                "overlay channel {channel} transition duration {duration_frames} is outside 1..={MAX_OVERLAY_TRANSITION_DURATION_FRAMES} frames"
            ),
            Self::ActiveOverlayMissingSource { channel } => {
                write!(formatter, "active overlay channel {channel} has no source")
            }
            Self::MissingOverlayInput { channel, input } => {
                write!(formatter, "overlay channel {channel} references missing input {input}")
            }
            Self::MissingOverlayOutput { channel, output } => {
                write!(formatter, "overlay channel {channel} references missing output {output}")
            }
            Self::DuplicateOverlayOutput { channel, output } => {
                write!(formatter, "overlay channel {channel} repeats output {output}")
            }
            Self::OverlayQueueTooDeep {
                channel,
                depth,
                maximum,
            } => write!(
                formatter,
                "overlay channel {channel} queue depth {depth} exceeds maximum {maximum}"
            ),
            Self::EmptyIdempotencyKey => formatter.write_str("idempotency key is blank"),
            Self::EmptyCommandId { key } => {
                write!(formatter, "command ID for idempotency key `{key}` is blank")
            }
            Self::DuplicateIdempotencyKey { key } => {
                write!(formatter, "idempotency key `{key}` is duplicated")
            }
            Self::EmptyRejectionCode { key } => write!(
                formatter,
                "rejection code for idempotency key `{key}` is blank"
            ),
            Self::ReceiptRevisionAhead {
                key,
                receipt_revision,
                project_revision,
            } => write!(
                formatter,
                "receipt `{key}` revision {receipt_revision} exceeds project revision {project_revision}"
            ),
        }
    }
}

impl Error for ProjectValidationError {}
