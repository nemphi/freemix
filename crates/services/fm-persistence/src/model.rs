use std::{
    collections::BTreeSet,
    error::Error,
    fmt,
    num::{NonZeroU64, NonZeroU128},
};

use fm_model::{
    Input, InputKind, MainMix, Project, ProjectSettings, SimulatedAudio, SimulatedInput,
    SimulatedVideo, SolidColor, ValidationError,
};
use fm_types::{
    AudioFormat, ChannelLayout, ColorMetadata, FrameRate, InputId, PixelFormat, ProjectId,
    SampleFormat, SampleRate, ScanMode, VideoDimensions, VideoFormat,
};

pub const CURRENT_SCHEMA_VERSION: u32 = fm_model::CURRENT_SCHEMA_VERSION.get();

/// Desired and observed switcher selections persisted independently.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimeRouting {
    pub desired_program_id: Option<InputId>,
    pub realized_program_id: Option<InputId>,
    pub desired_preview_id: Option<InputId>,
    pub realized_preview_id: Option<InputId>,
}

/// Legacy 64-bit routing view retained while existing applications migrate.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProjectRouting {
    pub desired_program_id: Option<NonZeroU64>,
    pub realized_program_id: Option<NonZeroU64>,
    pub desired_preview_id: Option<NonZeroU64>,
    pub realized_preview_id: Option<NonZeroU64>,
}

impl From<ProjectRouting> for RuntimeRouting {
    fn from(value: ProjectRouting) -> Self {
        Self {
            desired_program_id: value.desired_program_id.map(legacy_input_id),
            realized_program_id: value.realized_program_id.map(legacy_input_id),
            desired_preview_id: value.desired_preview_id.map(legacy_input_id),
            realized_preview_id: value.realized_preview_id.map(legacy_input_id),
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
        mut idempotency_receipts: Vec<IdempotencyReceipt>,
    ) -> Result<Self, ProjectValidationError> {
        idempotency_receipts.sort_by(|left, right| left.key.cmp(&right.key));
        let stored = Self {
            project,
            routing,
            position,
            idempotency_receipts,
        };
        stored.validate()?;
        Ok(stored)
    }

    /// Legacy constructor that deterministically synthesizes a simulated v3 project.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectValidationError`] for an unsupported schema or invalid state.
    pub fn new(
        schema_version: u32,
        project_id: NonZeroU64,
        show_name: impl Into<String>,
        input_ids: Vec<NonZeroU64>,
        routing: ProjectRouting,
        position: ProjectPosition,
        idempotency_receipts: Vec<IdempotencyReceipt>,
    ) -> Result<Self, ProjectValidationError> {
        if schema_version != CURRENT_SCHEMA_VERSION {
            return Err(ProjectValidationError::UnsupportedSchema {
                found: schema_version,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }
        let routing = RuntimeRouting::from(routing);
        let project = synthesize_project(
            ProjectId::new(NonZeroU128::from(project_id)),
            show_name.into(),
            input_ids.into_iter().map(legacy_input_id).collect(),
            routing,
        );
        Self::from_project(project, routing, position, idempotency_receipts)
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

    /// Returns the legacy project ID view.
    ///
    /// # Panics
    ///
    /// Panics when the canonical ID does not fit the legacy 64-bit API.
    #[must_use]
    pub fn project_id(&self) -> NonZeroU64 {
        legacy_id(self.project.id().get(), "project")
    }

    #[must_use]
    pub fn show_name(&self) -> &str {
        self.project.name()
    }

    /// Returns legacy 64-bit input IDs derived from the canonical project.
    ///
    /// # Panics
    ///
    /// Panics when a canonical input ID does not fit the legacy 64-bit API.
    #[must_use]
    pub fn input_ids(&self) -> Vec<NonZeroU64> {
        self.project
            .inputs()
            .iter()
            .map(|input| legacy_id(input.id.get(), "input"))
            .collect()
    }

    /// Returns the legacy 64-bit runtime routing view.
    ///
    /// # Panics
    ///
    /// Panics when a routed input ID does not fit the legacy 64-bit API.
    #[must_use]
    pub fn routing(&self) -> ProjectRouting {
        ProjectRouting {
            desired_program_id: self
                .routing
                .desired_program_id
                .map(|id| legacy_id(id.get(), "input")),
            realized_program_id: self
                .routing
                .realized_program_id
                .map(|id| legacy_id(id.get(), "input")),
            desired_preview_id: self
                .routing
                .desired_preview_id
                .map(|id| legacy_id(id.get(), "input")),
            realized_preview_id: self
                .routing
                .realized_preview_id
                .map(|id| legacy_id(id.get(), "input")),
        }
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

fn synthesize_project(
    id: ProjectId,
    name: String,
    input_ids: Vec<InputId>,
    routing: RuntimeRouting,
) -> Project {
    let frame_rate = FrameRate::new(60_000, 1_001).expect("legacy frame rate is valid");
    let settings = ProjectSettings {
        frame_rate,
        video: VideoFormat {
            dimensions: VideoDimensions::new(1_920, 1_080).expect("legacy dimensions are valid"),
            frame_rate,
            pixel_format: PixelFormat::Nv12,
            scan: ScanMode::Progressive,
            color: ColorMetadata::default(),
        },
        audio: AudioFormat {
            sample_rate: SampleRate::new(48_000).expect("legacy sample rate is valid"),
            sample_format: SampleFormat::F32,
            channels: ChannelLayout::stereo(),
        },
    };
    let mut project = Project::new(id, name, settings);
    for input_id in input_ids {
        let value = input_id.get().get();
        project.add_input(Input {
            id: input_id,
            name: format!("Input {value}"),
            kind: InputKind::Simulated(SimulatedInput::new(
                SimulatedVideo::Solid(SolidColor::new(
                    u8::try_from(value.wrapping_mul(73) & 0xff).expect("value is masked to u8"),
                    u8::try_from(value.wrapping_mul(151) & 0xff).expect("value is masked to u8"),
                    u8::try_from(value.wrapping_mul(199) & 0xff).expect("value is masked to u8"),
                    u8::MAX,
                )),
                SimulatedAudio::Silence,
            )),
            required_capabilities: Vec::new(),
        });
    }
    if let (Some(program), Some(preview)) = (routing.desired_program_id, routing.desired_preview_id)
        && program != preview
    {
        project.set_main_mix(MainMix::new(program, preview));
    }
    project
}

fn legacy_input_id(id: NonZeroU64) -> InputId {
    InputId::new(NonZeroU128::from(id))
}

fn legacy_id(id: NonZeroU128, kind: &str) -> NonZeroU64 {
    let value = u64::try_from(id.get())
        .unwrap_or_else(|_| panic!("{kind} ID {id} exceeds the legacy 64-bit API"));
    NonZeroU64::new(value).expect("a nonzero u128 remains nonzero after a successful conversion")
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
