use crate::exchange::validate_capabilities;
use crate::{
    ApiCompatibility, CapabilityDecision, CapabilityId, CommandEnvelope, CrashReport, Deadline,
    ExchangeError, HeartbeatMessage, PluginId, PluginManifest, PluginState, ProtocolLimits,
    QuarantineReason, Rejection, RejectionCode, Revision, StateEpoch, StateSnapshot,
};
use core::fmt;
use fm_command::{ApplyOutcome, CommandState, Mutation};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PluginCommand {
    Discover(PluginManifest),
    Validate {
        plugin_id: PluginId,
    },
    Load {
        plugin_id: PluginId,
        snapshot: Option<StateSnapshot>,
    },
    Start {
        plugin_id: PluginId,
    },
    Stop {
        plugin_id: PluginId,
    },
    RequestCapability {
        plugin_id: PluginId,
        capability: CapabilityId,
    },
    DecideCapability {
        plugin_id: PluginId,
        capability: CapabilityId,
        decision: CapabilityDecision,
    },
    StoreSnapshot(StateSnapshot),
    Heartbeat {
        plugin_id: PluginId,
        heartbeat: HeartbeatMessage,
    },
    CheckHeartbeatDeadline {
        plugin_id: PluginId,
        deadline: Deadline,
    },
    Quarantine {
        plugin_id: PluginId,
        reason: QuarantineReason,
    },
    ReportCrash(CrashReport),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PluginCommandResult {
    Discovered,
    Validated,
    Loaded,
    Started,
    Stopped,
    CapabilityRequested,
    CapabilityDecided(CapabilityDecision),
    SnapshotStored,
    HeartbeatRecorded,
    Quarantined,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PluginEvent {
    StateChanged {
        plugin_id: PluginId,
        from: Option<PluginState>,
        to: PluginState,
    },
    CapabilityRequested {
        plugin_id: PluginId,
        capability: CapabilityId,
    },
    CapabilityDecided {
        plugin_id: PluginId,
        capability: CapabilityId,
        decision: CapabilityDecision,
    },
    SnapshotStored {
        plugin_id: PluginId,
        version: crate::StateVersion,
    },
    HeartbeatRecorded {
        plugin_id: PluginId,
        sequence: u64,
    },
    CrashReported(CrashReport),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginRecord {
    manifest: PluginManifest,
    state: PluginState,
    requested_capabilities: HashSet<CapabilityId>,
    capability_decisions: HashMap<CapabilityId, CapabilityDecision>,
    snapshot: Option<StateSnapshot>,
    last_heartbeat: Option<HeartbeatMessage>,
    quarantine_reason: Option<QuarantineReason>,
    crash_report: Option<CrashReport>,
}

impl PluginRecord {
    #[must_use]
    pub const fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    #[must_use]
    pub const fn state(&self) -> PluginState {
        self.state
    }

    /// Returns the effective decision. Unrequested and undecided capabilities
    /// are denied rather than inheriting ambient authority.
    #[must_use]
    pub fn capability_decision(&self, capability: &CapabilityId) -> CapabilityDecision {
        self.capability_decisions
            .get(capability)
            .copied()
            .unwrap_or(CapabilityDecision::Denied)
    }

    #[must_use]
    pub fn capability_requested(&self, capability: &CapabilityId) -> bool {
        self.requested_capabilities.contains(capability)
    }

    #[must_use]
    pub const fn snapshot(&self) -> Option<&StateSnapshot> {
        self.snapshot.as_ref()
    }

    #[must_use]
    pub const fn last_heartbeat(&self) -> Option<HeartbeatMessage> {
        self.last_heartbeat
    }

    #[must_use]
    pub const fn quarantine_reason(&self) -> Option<&QuarantineReason> {
        self.quarantine_reason.as_ref()
    }

    #[must_use]
    pub const fn crash_report(&self) -> Option<&CrashReport> {
        self.crash_report.as_ref()
    }
}

#[derive(Clone, Debug)]
struct HostState {
    plugins: HashMap<PluginId, PluginRecord>,
    compatibility: ApiCompatibility,
    limits: ProtocolLimits,
}

#[derive(Clone, Debug)]
struct HostMutation {
    command: PluginCommand,
    now_millis: u64,
}

impl Mutation<HostState, PluginEvent, PluginCommandResult> for HostMutation {
    fn apply(
        self,
        state: &mut HostState,
        events: &mut Vec<PluginEvent>,
    ) -> Result<PluginCommandResult, Rejection> {
        apply_command(state, events, self.command, self.now_millis)
    }
}

#[derive(Debug)]
pub struct PluginHost {
    commands: CommandState<HostState, PluginCommandResult>,
}

impl PluginHost {
    /// Creates an empty engine-side plugin registry.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::InvalidLimits`] when protocol limits are unsafe.
    pub fn new(
        compatibility: ApiCompatibility,
        limits: ProtocolLimits,
        state_epoch: StateEpoch,
    ) -> Result<Self, HostError> {
        let limits = limits.validate().map_err(HostError::InvalidLimits)?;
        if compatibility.minimum_minor > compatibility.maximum_minor {
            return Err(HostError::InvalidCompatibility);
        }
        Ok(Self {
            commands: CommandState::new(
                HostState {
                    plugins: HashMap::new(),
                    compatibility,
                    limits,
                },
                state_epoch,
            ),
        })
    }

    /// Applies the sole mutating protocol entry point.
    #[must_use]
    pub fn execute(
        &mut self,
        envelope: CommandEnvelope<PluginCommand>,
        now_millis: u64,
    ) -> ApplyOutcome<PluginCommandResult, PluginEvent> {
        self.commands.apply(envelope, now_millis, |_, command| {
            Ok(HostMutation {
                command,
                now_millis,
            })
        })
    }

    #[must_use]
    pub fn plugin(&self, plugin_id: &PluginId) -> Option<&PluginRecord> {
        self.commands.state().plugins.get(plugin_id)
    }

    pub fn plugins(&self) -> impl Iterator<Item = (&PluginId, &PluginRecord)> {
        self.commands.state().plugins.iter()
    }

    #[must_use]
    pub const fn revision(&self) -> Revision {
        self.commands.revision()
    }

    #[must_use]
    pub const fn limits(&self) -> &ProtocolLimits {
        &self.commands.state().limits
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostError {
    InvalidLimits(ExchangeError),
    InvalidCompatibility,
}

impl fmt::Display for HostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits(error) => error.fmt(formatter),
            Self::InvalidCompatibility => {
                formatter.write_str("minimum API minor version exceeds maximum")
            }
        }
    }
}

impl std::error::Error for HostError {}

fn apply_command(
    state: &mut HostState,
    events: &mut Vec<PluginEvent>,
    command: PluginCommand,
    now_millis: u64,
) -> Result<PluginCommandResult, Rejection> {
    match command {
        PluginCommand::Discover(manifest) => discover(state, events, manifest),
        PluginCommand::Validate { plugin_id } => validate(state, events, &plugin_id),
        PluginCommand::Load {
            plugin_id,
            snapshot,
        } => load(state, events, &plugin_id, snapshot),
        PluginCommand::Start { plugin_id } => transition(
            state,
            events,
            &plugin_id,
            &[PluginState::Loaded, PluginState::Stopped],
            PluginState::Started,
            PluginCommandResult::Started,
        ),
        PluginCommand::Stop { plugin_id } => transition(
            state,
            events,
            &plugin_id,
            &[PluginState::Started],
            PluginState::Stopped,
            PluginCommandResult::Stopped,
        ),
        PluginCommand::RequestCapability {
            plugin_id,
            capability,
        } => request_capability(state, events, &plugin_id, capability),
        PluginCommand::DecideCapability {
            plugin_id,
            capability,
            decision,
        } => decide_capability(state, events, &plugin_id, capability, decision),
        PluginCommand::StoreSnapshot(snapshot) => store_snapshot(state, events, snapshot),
        PluginCommand::Heartbeat {
            plugin_id,
            heartbeat: message,
        } => heartbeat(state, events, &plugin_id, message, now_millis),
        PluginCommand::CheckHeartbeatDeadline {
            plugin_id,
            deadline,
        } => check_heartbeat_deadline(state, events, &plugin_id, deadline, now_millis),
        PluginCommand::Quarantine { plugin_id, reason } => {
            quarantine(state, events, &plugin_id, reason, None)
        }
        PluginCommand::ReportCrash(report) => {
            let plugin_id = report.plugin_id().clone();
            quarantine(
                state,
                events,
                &plugin_id,
                QuarantineReason::Crashed,
                Some(report),
            )
        }
    }
}

fn discover(
    state: &mut HostState,
    events: &mut Vec<PluginEvent>,
    manifest: PluginManifest,
) -> Result<PluginCommandResult, Rejection> {
    state
        .limits
        .validate_identifier("plugin_id", manifest.id.as_str())
        .map_err(exchange_rejection)?;
    validate_capabilities(&manifest.requested_capabilities, &state.limits)
        .map_err(exchange_rejection)?;
    if state.plugins.contains_key(&manifest.id) {
        return Err(rejection(
            RejectionCode::Conflict,
            "plugin has already been discovered",
        ));
    }
    let plugin_id = manifest.id.clone();
    let requested_capabilities = manifest.requested_capabilities.iter().cloned().collect();
    state.plugins.insert(
        plugin_id.clone(),
        PluginRecord {
            manifest,
            state: PluginState::Discovered,
            requested_capabilities,
            capability_decisions: HashMap::new(),
            snapshot: None,
            last_heartbeat: None,
            quarantine_reason: None,
            crash_report: None,
        },
    );
    events.push(PluginEvent::StateChanged {
        plugin_id,
        from: None,
        to: PluginState::Discovered,
    });
    Ok(PluginCommandResult::Discovered)
}

fn validate(
    state: &mut HostState,
    events: &mut Vec<PluginEvent>,
    plugin_id: &PluginId,
) -> Result<PluginCommandResult, Rejection> {
    let compatibility = state.compatibility;
    let record = plugin_mut(state, plugin_id)?;
    require_state(record, &[PluginState::Discovered], PluginState::Validated)?;
    if !compatibility.supports(record.manifest.api_version) {
        return Err(rejection(
            RejectionCode::InvalidCommand,
            "plugin API version is not supported",
        ));
    }
    set_state(record, events, plugin_id, PluginState::Validated);
    Ok(PluginCommandResult::Validated)
}

fn load(
    state: &mut HostState,
    events: &mut Vec<PluginEvent>,
    plugin_id: &PluginId,
    snapshot: Option<StateSnapshot>,
) -> Result<PluginCommandResult, Rejection> {
    if let Some(snapshot) = &snapshot {
        validate_snapshot(&state.limits, plugin_id, snapshot)?;
    }
    let record = plugin_mut(state, plugin_id)?;
    require_state(record, &[PluginState::Validated], PluginState::Loaded)?;
    if let Some(snapshot) = snapshot {
        if snapshot.version() != record.manifest.state_version {
            return Err(rejection(
                RejectionCode::InvalidCommand,
                "snapshot must be migrated to the manifest state version before load",
            ));
        }
        record.snapshot = Some(snapshot);
    }
    set_state(record, events, plugin_id, PluginState::Loaded);
    Ok(PluginCommandResult::Loaded)
}

fn request_capability(
    state: &mut HostState,
    events: &mut Vec<PluginEvent>,
    plugin_id: &PluginId,
    capability: CapabilityId,
) -> Result<PluginCommandResult, Rejection> {
    state
        .limits
        .validate_identifier("capability", capability.as_str())
        .map_err(exchange_rejection)?;
    let maximum = state.limits.max_capabilities;
    let record = plugin_mut(state, plugin_id)?;
    if record.state == PluginState::Quarantined {
        return Err(rejection(
            RejectionCode::Unavailable,
            "quarantined plugin cannot request capabilities",
        ));
    }
    if !record.requested_capabilities.contains(&capability)
        && record.requested_capabilities.len() >= maximum
    {
        return Err(rejection(
            RejectionCode::ResourceExhausted,
            "capability request limit exceeded",
        ));
    }
    record.requested_capabilities.insert(capability.clone());
    events.push(PluginEvent::CapabilityRequested {
        plugin_id: plugin_id.clone(),
        capability,
    });
    Ok(PluginCommandResult::CapabilityRequested)
}

fn decide_capability(
    state: &mut HostState,
    events: &mut Vec<PluginEvent>,
    plugin_id: &PluginId,
    capability: CapabilityId,
    decision: CapabilityDecision,
) -> Result<PluginCommandResult, Rejection> {
    let record = plugin_mut(state, plugin_id)?;
    if !record.requested_capabilities.contains(&capability) {
        return Err(rejection(
            RejectionCode::InvalidCommand,
            "capability must be requested before it can be decided",
        ));
    }
    if record.state == PluginState::Quarantined && decision == CapabilityDecision::Granted {
        return Err(rejection(
            RejectionCode::Unavailable,
            "cannot grant capability to quarantined plugin",
        ));
    }
    record
        .capability_decisions
        .insert(capability.clone(), decision);
    events.push(PluginEvent::CapabilityDecided {
        plugin_id: plugin_id.clone(),
        capability,
        decision,
    });
    Ok(PluginCommandResult::CapabilityDecided(decision))
}

fn store_snapshot(
    state: &mut HostState,
    events: &mut Vec<PluginEvent>,
    snapshot: StateSnapshot,
) -> Result<PluginCommandResult, Rejection> {
    let plugin_id = snapshot.plugin_id().clone();
    validate_snapshot(&state.limits, &plugin_id, &snapshot)?;
    let record = plugin_mut(state, &plugin_id)?;
    if !matches!(
        record.state,
        PluginState::Loaded | PluginState::Started | PluginState::Stopped
    ) {
        return Err(rejection(
            RejectionCode::Conflict,
            "plugin state cannot be snapshotted in its current lifecycle state",
        ));
    }
    if snapshot.version() != record.manifest.state_version {
        return Err(rejection(
            RejectionCode::InvalidCommand,
            "snapshot state version does not match the manifest",
        ));
    }
    let version = snapshot.version();
    record.snapshot = Some(snapshot);
    events.push(PluginEvent::SnapshotStored { plugin_id, version });
    Ok(PluginCommandResult::SnapshotStored)
}

fn heartbeat(
    state: &mut HostState,
    events: &mut Vec<PluginEvent>,
    plugin_id: &PluginId,
    heartbeat: HeartbeatMessage,
    now_millis: u64,
) -> Result<PluginCommandResult, Rejection> {
    if heartbeat.deadline_exceeded_at(now_millis) {
        return Err(rejection(
            RejectionCode::DeadlineExceeded,
            "heartbeat reply deadline exceeded",
        ));
    }
    let record = plugin_mut(state, plugin_id)?;
    if record.state != PluginState::Started {
        return Err(rejection(
            RejectionCode::Conflict,
            "heartbeats are accepted only from started plugins",
        ));
    }
    if record
        .last_heartbeat
        .is_some_and(|previous| heartbeat.sequence <= previous.sequence)
    {
        return Err(rejection(
            RejectionCode::InvalidCommand,
            "heartbeat sequence must increase",
        ));
    }
    record.last_heartbeat = Some(heartbeat);
    events.push(PluginEvent::HeartbeatRecorded {
        plugin_id: plugin_id.clone(),
        sequence: heartbeat.sequence,
    });
    Ok(PluginCommandResult::HeartbeatRecorded)
}

fn check_heartbeat_deadline(
    state: &mut HostState,
    events: &mut Vec<PluginEvent>,
    plugin_id: &PluginId,
    deadline: Deadline,
    now_millis: u64,
) -> Result<PluginCommandResult, Rejection> {
    if !deadline.is_exceeded_at(now_millis) {
        return Err(rejection(
            RejectionCode::InvalidCommand,
            "heartbeat deadline has not expired",
        ));
    }
    quarantine(
        state,
        events,
        plugin_id,
        QuarantineReason::DeadlineMissed {
            deadline_millis: deadline.as_millis(),
        },
        None,
    )
}

fn quarantine(
    state: &mut HostState,
    events: &mut Vec<PluginEvent>,
    plugin_id: &PluginId,
    reason: QuarantineReason,
    crash_report: Option<CrashReport>,
) -> Result<PluginCommandResult, Rejection> {
    if let QuarantineReason::Policy(message) = &reason {
        state
            .limits
            .validate_identifier("quarantine reason", message)
            .map_err(exchange_rejection)?;
    }
    if let Some(report) = &crash_report {
        if report.plugin_id() != plugin_id {
            return Err(rejection(
                RejectionCode::InvalidCommand,
                "crash report plugin identity does not match command plugin",
            ));
        }
        state
            .limits
            .validate_identifier("plugin_id", report.plugin_id().as_str())
            .map_err(exchange_rejection)?;
        state
            .limits
            .validate_identifier("crash summary", report.summary())
            .map_err(exchange_rejection)?;
        ProtocolLimits::validate_payload(
            "crash report",
            report.details().len(),
            state.limits.max_crash_report_bytes,
        )
        .map_err(exchange_rejection)?;
    }
    let record = plugin_mut(state, plugin_id)?;
    require_state(
        record,
        &[
            PluginState::Discovered,
            PluginState::Validated,
            PluginState::Loaded,
            PluginState::Started,
            PluginState::Stopped,
        ],
        PluginState::Quarantined,
    )?;
    record.quarantine_reason = Some(reason);
    record.capability_decisions.clear();
    if let Some(report) = crash_report {
        events.push(PluginEvent::CrashReported(report.clone()));
        record.crash_report = Some(report);
    }
    set_state(record, events, plugin_id, PluginState::Quarantined);
    Ok(PluginCommandResult::Quarantined)
}

fn transition(
    state: &mut HostState,
    events: &mut Vec<PluginEvent>,
    plugin_id: &PluginId,
    allowed: &[PluginState],
    to: PluginState,
    result: PluginCommandResult,
) -> Result<PluginCommandResult, Rejection> {
    let record = plugin_mut(state, plugin_id)?;
    require_state(record, allowed, to)?;
    set_state(record, events, plugin_id, to);
    Ok(result)
}

fn set_state(
    record: &mut PluginRecord,
    events: &mut Vec<PluginEvent>,
    plugin_id: &PluginId,
    to: PluginState,
) {
    let from = record.state;
    record.state = to;
    events.push(PluginEvent::StateChanged {
        plugin_id: plugin_id.clone(),
        from: Some(from),
        to,
    });
}

fn require_state(
    record: &PluginRecord,
    allowed: &[PluginState],
    to: PluginState,
) -> Result<(), Rejection> {
    if allowed.contains(&record.state) {
        Ok(())
    } else {
        Err(rejection(
            RejectionCode::Conflict,
            format!(
                "invalid plugin lifecycle transition from {:?} to {to:?}",
                record.state
            ),
        ))
    }
}

fn plugin_mut<'a>(
    state: &'a mut HostState,
    plugin_id: &PluginId,
) -> Result<&'a mut PluginRecord, Rejection> {
    state
        .plugins
        .get_mut(plugin_id)
        .ok_or_else(|| rejection(RejectionCode::NotFound, "plugin was not discovered"))
}

fn validate_snapshot(
    limits: &ProtocolLimits,
    plugin_id: &PluginId,
    snapshot: &StateSnapshot,
) -> Result<(), Rejection> {
    if snapshot.plugin_id() != plugin_id {
        return Err(rejection(
            RejectionCode::InvalidCommand,
            "snapshot plugin identity does not match command plugin",
        ));
    }
    ProtocolLimits::validate_payload("snapshot", snapshot.data().len(), limits.max_snapshot_bytes)
        .map_err(exchange_rejection)
}

fn exchange_rejection(error: ExchangeError) -> Rejection {
    let code = match error {
        ExchangeError::PayloadTooLarge { .. } | ExchangeError::BatchTooLarge { .. } => {
            RejectionCode::ResourceExhausted
        }
        _ => RejectionCode::InvalidCommand,
    };
    rejection(code, error.to_string())
}

fn rejection(code: RejectionCode, message: impl Into<String>) -> Rejection {
    Rejection::new(code, message)
}

#[cfg(test)]
mod tests;
