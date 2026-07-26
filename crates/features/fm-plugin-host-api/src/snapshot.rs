use crate::{Deadline, ExchangeError, PluginId, ProtocolLimits, StateSnapshot, StateVersion};
use core::fmt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationRequest {
    pub plugin_id: PluginId,
    pub from_version: StateVersion,
    pub to_version: StateVersion,
    pub snapshot: StateSnapshot,
    pub deadline: Deadline,
}

pub trait SnapshotMigrator {
    /// Migrates exactly one state-version step.
    ///
    /// # Errors
    ///
    /// Returns [`MigrationError`] when the plugin cannot migrate the snapshot.
    fn migrate(&mut self, request: MigrationRequest) -> Result<StateSnapshot, MigrationError>;
}

impl<F> SnapshotMigrator for F
where
    F: FnMut(MigrationRequest) -> Result<StateSnapshot, MigrationError>,
{
    fn migrate(&mut self, request: MigrationRequest) -> Result<StateSnapshot, MigrationError> {
        self(request)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MigrationError {
    DeadlineExceeded,
    DowngradeUnsupported {
        from: StateVersion,
        to: StateVersion,
    },
    TooManySteps {
        steps: u32,
        maximum: u32,
    },
    VersionExhausted,
    WrongPlugin {
        expected: PluginId,
        actual: PluginId,
    },
    WrongVersion {
        expected: StateVersion,
        actual: StateVersion,
    },
    Bounds(ExchangeError),
    Plugin(String),
}

impl fmt::Display for MigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeadlineExceeded => formatter.write_str("snapshot migration deadline exceeded"),
            Self::DowngradeUnsupported { from, to } => {
                write!(
                    formatter,
                    "snapshot downgrade from {from} to {to} is unsupported"
                )
            }
            Self::TooManySteps { steps, maximum } => write!(
                formatter,
                "snapshot migration requires {steps} steps, exceeding the {maximum} step limit"
            ),
            Self::VersionExhausted => formatter.write_str("state version space exhausted"),
            Self::WrongPlugin { expected, actual } => {
                write!(
                    formatter,
                    "migration returned plugin {actual}, expected {expected}"
                )
            }
            Self::WrongVersion { expected, actual } => write!(
                formatter,
                "migration returned state version {actual}, expected {expected}"
            ),
            Self::Bounds(error) => error.fmt(formatter),
            Self::Plugin(message) => write!(formatter, "plugin migration failed: {message}"),
        }
    }
}

impl std::error::Error for MigrationError {}

impl From<ExchangeError> for MigrationError {
    fn from(value: ExchangeError) -> Self {
        Self::Bounds(value)
    }
}

/// Validates plugin identity, target version, and bounds on a migration result.
///
/// # Errors
///
/// Returns [`MigrationError`] when the response does not match its request.
pub fn validate_migration_response(
    request: &MigrationRequest,
    response: &StateSnapshot,
    limits: &ProtocolLimits,
) -> Result<(), MigrationError> {
    limits.validate()?;
    if response.plugin_id() != &request.plugin_id {
        return Err(MigrationError::WrongPlugin {
            expected: request.plugin_id.clone(),
            actual: response.plugin_id().clone(),
        });
    }
    if response.version() != request.to_version {
        return Err(MigrationError::WrongVersion {
            expected: request.to_version,
            actual: response.version(),
        });
    }
    ProtocolLimits::validate_payload("snapshot", response.data().len(), limits.max_snapshot_bytes)?;
    Ok(())
}

/// Migrates a snapshot forward one version at a time under one deadline.
///
/// The migrator is untrusted: every returned identity, version, and payload is
/// validated before it can become input to the next step.
///
/// # Errors
///
/// Returns [`MigrationError`] for downgrade attempts, deadline expiry,
/// excessive version distance, malformed plugin responses, or plugin failure.
pub fn migrate_snapshot<M: SnapshotMigrator>(
    mut snapshot: StateSnapshot,
    target: StateVersion,
    deadline: Deadline,
    now_millis: u64,
    limits: &ProtocolLimits,
    migrator: &mut M,
) -> Result<StateSnapshot, MigrationError> {
    limits.validate()?;
    if deadline.is_exceeded_at(now_millis) {
        return Err(MigrationError::DeadlineExceeded);
    }
    if target < snapshot.version() {
        return Err(MigrationError::DowngradeUnsupported {
            from: snapshot.version(),
            to: target,
        });
    }
    let steps = target.get() - snapshot.version().get();
    let maximum_steps = limits
        .max_migration_steps
        .min(crate::exchange::HARD_MAX_MIGRATION_STEPS);
    if steps > maximum_steps {
        return Err(MigrationError::TooManySteps {
            steps,
            maximum: maximum_steps,
        });
    }
    ProtocolLimits::validate_payload("snapshot", snapshot.data().len(), limits.max_snapshot_bytes)?;

    while snapshot.version() < target {
        let to_version = snapshot
            .version()
            .checked_next()
            .ok_or(MigrationError::VersionExhausted)?;
        let request = MigrationRequest {
            plugin_id: snapshot.plugin_id().clone(),
            from_version: snapshot.version(),
            to_version,
            snapshot,
            deadline,
        };
        let response = migrator.migrate(request.clone())?;
        validate_migration_response(&request, &response, limits)?;
        snapshot = response;
    }
    Ok(snapshot)
}
