use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    error::Error,
    fmt,
};

use fm_capabilities::CapabilityRegistry;
use fm_plugin_api::PluginId;

use crate::{BoundedIpcQueue, Catalog, IpcLimits, IpcMessage, PluginArtifact, QueueError};

pub type ChildId = u64;
type PluginKey = (u64, u64);

const fn plugin_key(id: PluginId) -> PluginKey {
    (id.high, id.low)
}

const fn plugin_id((high, low): PluginKey) -> PluginId {
    PluginId::new(high, low)
}

/// Process-control boundary. Implementations must create a distinct child for
/// each successful `spawn` call.
pub trait ChildController {
    /// Starts one isolated instance for `plugin`.
    ///
    /// # Errors
    ///
    /// Returns a process-launch failure.
    fn spawn(&mut self, plugin: &PluginArtifact) -> Result<ChildId, ChildError>;

    /// Sends a bounded message to a live child.
    ///
    /// # Errors
    ///
    /// Returns a transport or child failure.
    fn send(&mut self, child: ChildId, message: &IpcMessage) -> Result<(), ChildError>;
    fn terminate(&mut self, child: ChildId);
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildError {
    pub reason: String,
}

impl ChildError {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl fmt::Display for ChildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl Error for ChildError {}

/// Resource counters reported by the isolated child heartbeat.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResourceUsage {
    pub memory_bytes: u64,
    pub fuel: u64,
    pub operation_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChildEvent {
    Ready {
        child: ChildId,
    },
    Heartbeat {
        child: ChildId,
        usage: ResourceUsage,
    },
    Exited {
        child: ChildId,
        reason: String,
    },
}

impl ChildEvent {
    const fn child(&self) -> ChildId {
        match self {
            Self::Ready { child } | Self::Heartbeat { child, .. } | Self::Exited { child, .. } => {
                *child
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstanceState {
    Discovered,
    Starting,
    Running,
    Backoff,
    Quarantined,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetResource {
    Memory,
    Fuel,
    Deadline,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Failure {
    SpawnFailed(String),
    ChildExited(String),
    IpcFailed(String),
    StartupDeadline,
    HeartbeatTimeout,
    BudgetExceeded {
        resource: BudgetResource,
        observed: u64,
        limit: u64,
    },
    CapabilityConflict(String),
}

/// Restart and liveness policy. All time values use a caller-provided monotonic
/// millisecond clock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupervisorPolicy {
    pub heartbeat_timeout_ms: u64,
    pub crash_window_ms: u64,
    pub initial_backoff_ms: u64,
    pub maximum_backoff_ms: u64,
    pub crashes_before_quarantine: usize,
}

impl SupervisorPolicy {
    const fn is_valid(self) -> bool {
        self.heartbeat_timeout_ms > 0
            && self.crash_window_ms > 0
            && self.initial_backoff_ms > 0
            && self.maximum_backoff_ms >= self.initial_backoff_ms
            && self.crashes_before_quarantine > 0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginStatus {
    pub state: InstanceState,
    pub child: Option<ChildId>,
    pub restart_at_ms: Option<u64>,
    pub queued_messages: usize,
    pub recent_crashes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditRecord {
    pub sequence: u64,
    pub at_ms: u64,
    pub plugin_id: PluginId,
    pub event: AuditEvent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuditEvent {
    Discovered,
    Spawned {
        child: ChildId,
    },
    Ready {
        child: ChildId,
    },
    Heartbeat {
        child: ChildId,
        usage: ResourceUsage,
    },
    IpcQueued {
        message_id: u64,
    },
    IpcSent {
        message_id: u64,
    },
    IpcRejected {
        message_id: u64,
        error: QueueError,
    },
    Failed(Failure),
    RestartScheduled {
        at_ms: u64,
    },
    Quarantined,
    QuarantineReleased,
    CapabilitiesPublished {
        count: usize,
    },
    CapabilitiesWithdrawn {
        count: usize,
    },
    Stopped,
    StaleChildEvent {
        child: ChildId,
    },
}

struct Instance {
    state: InstanceState,
    child: Option<ChildId>,
    started_at_ms: Option<u64>,
    last_heartbeat_ms: Option<u64>,
    restart_at_ms: Option<u64>,
    crashes: VecDeque<u64>,
    queue: BoundedIpcQueue,
}

/// Deterministic supervisor for one isolated child per catalog plugin.
pub struct Supervisor<C> {
    catalog: Catalog,
    controller: C,
    policy: SupervisorPolicy,
    instances: BTreeMap<PluginKey, Instance>,
    published: BTreeSet<PluginKey>,
    registry: CapabilityRegistry,
    audit: Vec<AuditRecord>,
    next_sequence: u64,
}

impl<C: ChildController> Supervisor<C> {
    /// Creates stopped instances for every accepted catalog entry.
    ///
    /// # Errors
    ///
    /// Returns an error when restart or IPC limits are invalid.
    pub fn new(
        catalog: Catalog,
        controller: C,
        policy: SupervisorPolicy,
        ipc_limits: IpcLimits,
    ) -> Result<Self, SupervisorError> {
        if !policy.is_valid() {
            return Err(SupervisorError::InvalidPolicy);
        }
        if !ipc_limits.is_valid() {
            return Err(SupervisorError::InvalidIpcLimits);
        }
        let mut instances = BTreeMap::new();
        for (id, _) in catalog.iter() {
            instances.insert(
                plugin_key(id),
                Instance {
                    state: InstanceState::Discovered,
                    child: None,
                    started_at_ms: None,
                    last_heartbeat_ms: None,
                    restart_at_ms: None,
                    crashes: VecDeque::new(),
                    queue: BoundedIpcQueue::new(ipc_limits)
                        .map_err(|_| SupervisorError::InvalidIpcLimits)?,
                },
            );
        }
        let mut supervisor = Self {
            catalog,
            controller,
            policy,
            instances,
            published: BTreeSet::new(),
            registry: CapabilityRegistry::new(),
            audit: Vec::new(),
            next_sequence: 0,
        };
        let ids: Vec<_> = supervisor
            .instances
            .keys()
            .copied()
            .map(plugin_id)
            .collect();
        for id in ids {
            supervisor.record(0, id, AuditEvent::Discovered);
        }
        Ok(supervisor)
    }

    pub fn controller(&self) -> &C {
        &self.controller
    }

    pub fn registry(&self) -> &CapabilityRegistry {
        &self.registry
    }

    pub fn audit(&self) -> &[AuditRecord] {
        &self.audit
    }

    /// Returns a plugin's current externally visible status.
    ///
    /// # Errors
    ///
    /// Returns [`SupervisorError::UnknownPlugin`] for an uncatalogued ID.
    pub fn status(&self, id: PluginId) -> Result<PluginStatus, SupervisorError> {
        let instance = self.instance(id)?;
        Ok(PluginStatus {
            state: instance.state,
            child: instance.child,
            restart_at_ms: instance.restart_at_ms,
            queued_messages: instance.queue.len(),
            recent_crashes: instance.crashes.len(),
        })
    }

    pub fn start_all(&mut self, now_ms: u64) {
        let ids: Vec<_> = self.instances.keys().copied().map(plugin_id).collect();
        for id in ids {
            let _ = self.start(id, now_ms);
        }
    }

    /// Starts a discovered or explicitly stopped plugin.
    ///
    /// # Errors
    ///
    /// Returns an unknown-plugin or invalid-state error.
    pub fn start(&mut self, id: PluginId, now_ms: u64) -> Result<(), SupervisorError> {
        let state = self.instance(id)?.state;
        if !matches!(state, InstanceState::Discovered | InstanceState::Stopped) {
            return Err(SupervisorError::InvalidState { state });
        }
        self.spawn(id, now_ms);
        Ok(())
    }

    /// Enqueues a message without exceeding configured memory bounds.
    ///
    /// # Errors
    ///
    /// Returns an unknown-plugin or queue bounds error.
    pub fn enqueue(
        &mut self,
        id: PluginId,
        message: IpcMessage,
        now_ms: u64,
    ) -> Result<(), SupervisorError> {
        let message_id = message.id;
        let result = self.instance_mut(id)?.queue.push(message);
        match result {
            Ok(()) => {
                self.record(now_ms, id, AuditEvent::IpcQueued { message_id });
                Ok(())
            }
            Err(error) => {
                self.record(
                    now_ms,
                    id,
                    AuditEvent::IpcRejected {
                        message_id,
                        error: error.clone(),
                    },
                );
                Err(SupervisorError::Queue(error))
            }
        }
    }

    /// Applies child observations, liveness checks, due restarts, and queued IPC.
    pub fn poll(&mut self, now_ms: u64, events: impl IntoIterator<Item = ChildEvent>) {
        for event in events {
            self.handle_event(now_ms, event);
        }

        let ids: Vec<_> = self.instances.keys().copied().map(plugin_id).collect();
        for &id in &ids {
            self.check_timeouts(id, now_ms);
        }
        for &id in &ids {
            if self.instance(id).is_ok_and(|instance| {
                instance.state == InstanceState::Backoff
                    && instance.restart_at_ms.is_some_and(|at| now_ms >= at)
            }) {
                self.spawn(id, now_ms);
            }
        }
        for id in ids {
            self.flush_ipc(id, now_ms);
        }
    }

    /// Terminates a plugin and atomically withdraws its capabilities.
    ///
    /// # Errors
    ///
    /// Returns [`SupervisorError::UnknownPlugin`] for an uncatalogued ID.
    pub fn stop(&mut self, id: PluginId, now_ms: u64) -> Result<(), SupervisorError> {
        let child = self.instance(id)?.child;
        if let Some(child) = child {
            self.controller.terminate(child);
        }
        self.withdraw_capabilities(id, now_ms);
        let instance = self.instance_mut(id)?;
        instance.state = InstanceState::Stopped;
        instance.child = None;
        instance.restart_at_ms = None;
        self.record(now_ms, id, AuditEvent::Stopped);
        Ok(())
    }

    /// Returns a quarantined plugin to the discovered state.
    ///
    /// # Errors
    ///
    /// Returns an unknown-plugin error or an invalid-state error if the plugin
    /// is not quarantined.
    pub fn release_quarantine(&mut self, id: PluginId, now_ms: u64) -> Result<(), SupervisorError> {
        let instance = self.instance_mut(id)?;
        if instance.state != InstanceState::Quarantined {
            return Err(SupervisorError::InvalidState {
                state: instance.state,
            });
        }
        instance.state = InstanceState::Discovered;
        instance.crashes.clear();
        self.record(now_ms, id, AuditEvent::QuarantineReleased);
        Ok(())
    }

    fn spawn(&mut self, id: PluginId, now_ms: u64) {
        let result = self
            .catalog
            .get(&id)
            .map(|plugin| self.controller.spawn(plugin))
            .expect("instance ids originate from the catalog");
        match result {
            Ok(child) => {
                let instance = self
                    .instances
                    .get_mut(&plugin_key(id))
                    .expect("instance ids originate from the catalog");
                instance.state = InstanceState::Starting;
                instance.child = Some(child);
                instance.started_at_ms = Some(now_ms);
                instance.last_heartbeat_ms = Some(now_ms);
                instance.restart_at_ms = None;
                self.record(now_ms, id, AuditEvent::Spawned { child });
            }
            Err(error) => self.fail(id, now_ms, Failure::SpawnFailed(error.reason), false),
        }
    }

    fn handle_event(&mut self, now_ms: u64, event: ChildEvent) {
        let child = event.child();
        let Some(id) = self.id_for_child(child) else {
            return;
        };
        match event {
            ChildEvent::Ready { .. } => self.ready(id, child, now_ms),
            ChildEvent::Heartbeat { usage, .. } => self.heartbeat(id, child, usage, now_ms),
            ChildEvent::Exited { reason, .. } => {
                self.fail(id, now_ms, Failure::ChildExited(reason), false);
            }
        }
    }

    fn ready(&mut self, id: PluginId, child: ChildId, now_ms: u64) {
        if self.instance(id).map_or(true, |instance| {
            instance.state != InstanceState::Starting || instance.child != Some(child)
        }) {
            self.record(now_ms, id, AuditEvent::StaleChildEvent { child });
            return;
        }
        match self.publish_capabilities(id) {
            Ok(count) => {
                let instance = self
                    .instances
                    .get_mut(&plugin_key(id))
                    .expect("id was resolved from an instance");
                instance.state = InstanceState::Running;
                instance.last_heartbeat_ms = Some(now_ms);
                self.record(now_ms, id, AuditEvent::Ready { child });
                self.record(now_ms, id, AuditEvent::CapabilitiesPublished { count });
            }
            Err(error) => self.fail(id, now_ms, Failure::CapabilityConflict(error), true),
        }
    }

    fn heartbeat(&mut self, id: PluginId, child: ChildId, usage: ResourceUsage, now_ms: u64) {
        let state = self
            .instance(id)
            .map_or(InstanceState::Stopped, |value| value.state);
        if !matches!(state, InstanceState::Starting | InstanceState::Running) {
            self.record(now_ms, id, AuditEvent::StaleChildEvent { child });
            return;
        }
        if let Some(failure) = self.budget_failure(id, usage) {
            self.fail(id, now_ms, failure, true);
            return;
        }
        self.instances
            .get_mut(&plugin_key(id))
            .expect("id was resolved from an instance")
            .last_heartbeat_ms = Some(now_ms);
        self.record(now_ms, id, AuditEvent::Heartbeat { child, usage });
    }

    fn budget_failure(&self, id: PluginId, usage: ResourceUsage) -> Option<Failure> {
        let budget = self.catalog.get(&id)?.manifest.budget;
        [
            (
                BudgetResource::Memory,
                usage.memory_bytes,
                budget.memory_bytes,
            ),
            (BudgetResource::Fuel, usage.fuel, budget.fuel),
            (
                BudgetResource::Deadline,
                usage.operation_ms,
                budget.deadline_ms,
            ),
        ]
        .into_iter()
        .find(|(_, observed, limit)| observed > limit)
        .map(|(resource, observed, limit)| Failure::BudgetExceeded {
            resource,
            observed,
            limit,
        })
    }

    fn check_timeouts(&mut self, id: PluginId, now_ms: u64) {
        let Some(instance) = self.instances.get(&plugin_key(id)) else {
            return;
        };
        let failure = match instance.state {
            InstanceState::Starting
                if instance.started_at_ms.is_some_and(|started| {
                    now_ms.saturating_sub(started)
                        > self
                            .catalog
                            .get(&id)
                            .expect("instance ids originate from catalog")
                            .manifest
                            .budget
                            .deadline_ms
                }) =>
            {
                Some(Failure::StartupDeadline)
            }
            InstanceState::Running
                if instance.last_heartbeat_ms.is_some_and(|heartbeat| {
                    now_ms.saturating_sub(heartbeat) > self.policy.heartbeat_timeout_ms
                }) =>
            {
                Some(Failure::HeartbeatTimeout)
            }
            _ => None,
        };
        if let Some(failure) = failure {
            self.fail(id, now_ms, failure, true);
        }
    }

    fn flush_ipc(&mut self, id: PluginId, now_ms: u64) {
        loop {
            let child = match self.instances.get(&plugin_key(id)) {
                Some(Instance {
                    state: InstanceState::Running,
                    child: Some(child),
                    ..
                }) => *child,
                _ => return,
            };
            let Some(message) = self
                .instances
                .get(&plugin_key(id))
                .expect("id was resolved from an instance")
                .queue
                .front()
            else {
                return;
            };
            let message_id = message.id;
            if let Err(error) = self.controller.send(child, message) {
                self.fail(id, now_ms, Failure::IpcFailed(error.reason), true);
                return;
            }
            self.instances
                .get_mut(&plugin_key(id))
                .expect("id was resolved from an instance")
                .queue
                .pop();
            self.record(now_ms, id, AuditEvent::IpcSent { message_id });
        }
    }

    fn fail(&mut self, id: PluginId, now_ms: u64, failure: Failure, terminate: bool) {
        let child = self
            .instances
            .get(&plugin_key(id))
            .and_then(|instance| instance.child);
        if terminate && let Some(child) = child {
            self.controller.terminate(child);
        }
        self.withdraw_capabilities(id, now_ms);
        self.record(now_ms, id, AuditEvent::Failed(failure));

        let instance = self
            .instances
            .get_mut(&plugin_key(id))
            .expect("failure ids originate from an instance");
        instance.child = None;
        instance.started_at_ms = None;
        instance.last_heartbeat_ms = None;
        while instance
            .crashes
            .front()
            .is_some_and(|crash| now_ms.saturating_sub(*crash) > self.policy.crash_window_ms)
        {
            instance.crashes.pop_front();
        }
        instance.crashes.push_back(now_ms);
        if instance.crashes.len() >= self.policy.crashes_before_quarantine {
            instance.state = InstanceState::Quarantined;
            instance.restart_at_ms = None;
            self.record(now_ms, id, AuditEvent::Quarantined);
            return;
        }

        let shift = u32::try_from(instance.crashes.len().saturating_sub(1)).unwrap_or(u32::MAX);
        let multiplier = 1_u64.checked_shl(shift).unwrap_or(u64::MAX);
        let backoff = self
            .policy
            .initial_backoff_ms
            .saturating_mul(multiplier)
            .min(self.policy.maximum_backoff_ms);
        let restart_at = now_ms.saturating_add(backoff);
        instance.state = InstanceState::Backoff;
        instance.restart_at_ms = Some(restart_at);
        self.record(
            now_ms,
            id,
            AuditEvent::RestartScheduled { at_ms: restart_at },
        );
    }

    fn publish_capabilities(&mut self, id: PluginId) -> Result<usize, String> {
        let mut registry = self.build_registry(Some(id))?;
        std::mem::swap(&mut self.registry, &mut registry);
        self.published.insert(plugin_key(id));
        Ok(self
            .catalog
            .get(&id)
            .expect("instance ids originate from catalog")
            .manifest
            .capabilities
            .len())
    }

    fn withdraw_capabilities(&mut self, id: PluginId, now_ms: u64) {
        if !self.published.remove(&plugin_key(id)) {
            return;
        }
        let count = self
            .catalog
            .get(&id)
            .expect("instance ids originate from catalog")
            .manifest
            .capabilities
            .len();
        self.registry = self
            .build_registry(None)
            .expect("published capability sets were already checked");
        self.record(now_ms, id, AuditEvent::CapabilitiesWithdrawn { count });
    }

    fn build_registry(&self, additional: Option<PluginId>) -> Result<CapabilityRegistry, String> {
        let mut ids = self.published.clone();
        if let Some(id) = additional {
            ids.insert(plugin_key(id));
        }
        let mut registry = CapabilityRegistry::new();
        for key in ids {
            let id = plugin_id(key);
            let plugin = self
                .catalog
                .get(&id)
                .expect("published ids originate from catalog");
            for capability in &plugin.manifest.capabilities {
                registry
                    .register(capability.clone())
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(registry)
    }

    fn id_for_child(&self, child: ChildId) -> Option<PluginId> {
        if let Some(id) = self
            .instances
            .iter()
            .find_map(|(id, instance)| (instance.child == Some(child)).then(|| plugin_id(*id)))
        {
            return Some(id);
        }
        None
    }

    fn instance(&self, id: PluginId) -> Result<&Instance, SupervisorError> {
        self.instances
            .get(&plugin_key(id))
            .ok_or(SupervisorError::UnknownPlugin(id))
    }

    fn instance_mut(&mut self, id: PluginId) -> Result<&mut Instance, SupervisorError> {
        self.instances
            .get_mut(&plugin_key(id))
            .ok_or(SupervisorError::UnknownPlugin(id))
    }

    fn record(&mut self, at_ms: u64, plugin_id: PluginId, event: AuditEvent) {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.audit.push(AuditRecord {
            sequence,
            at_ms,
            plugin_id,
            event,
        });
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SupervisorError {
    InvalidPolicy,
    InvalidIpcLimits,
    UnknownPlugin(PluginId),
    InvalidState { state: InstanceState },
    Queue(QueueError),
}

impl fmt::Display for SupervisorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPolicy => formatter.write_str("supervisor policy is invalid"),
            Self::InvalidIpcLimits => formatter.write_str("IPC limits are invalid"),
            Self::UnknownPlugin(id) => write!(
                formatter,
                "plugin `{:016x}{:016x}` is not in the catalog",
                id.high, id.low
            ),
            Self::InvalidState { state } => write!(formatter, "operation is invalid in {state:?}"),
            Self::Queue(error) => error.fmt(formatter),
        }
    }
}

impl Error for SupervisorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Queue(error) => Some(error),
            _ => None,
        }
    }
}
