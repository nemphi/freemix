use std::collections::VecDeque;

use fm_capabilities::{CapabilityKey, Provider, ProviderVersion, StableId};

use super::*;

#[derive(Default)]
struct FakeVerifier;

impl SignatureVerifier for FakeVerifier {
    fn verify(&self, artifact: &PluginArtifact) -> Result<(), SignatureError> {
        if artifact.signature.as_bytes() == b"trusted" {
            Ok(())
        } else {
            Err(SignatureError::new("untrusted test signature"))
        }
    }
}

#[derive(Default)]
struct FakeChildController {
    next_child: ChildId,
    spawn_results: VecDeque<Result<ChildId, ChildError>>,
    spawned: Vec<(PluginId, ResourceBudget)>,
    sent: Vec<(ChildId, IpcMessage)>,
    terminated: Vec<ChildId>,
}

impl ChildController for FakeChildController {
    fn spawn(&mut self, plugin: &PluginArtifact) -> Result<ChildId, ChildError> {
        self.spawned
            .push((plugin.manifest.plugin_id(), plugin.manifest.budget));
        if let Some(result) = self.spawn_results.pop_front() {
            return result;
        }
        self.next_child += 1;
        Ok(self.next_child)
    }

    fn send(&mut self, child: ChildId, message: &IpcMessage) -> Result<(), ChildError> {
        self.sent.push((child, message.clone()));
        Ok(())
    }

    fn terminate(&mut self, child: ChildId) {
        self.terminated.push(child);
    }
}

const fn version(major: u32, minor: u32, patch: u32) -> AbiVersion {
    AbiVersion::new(major, minor, patch)
}

fn provider_id(value: &str) -> StableId {
    StableId::new(value).unwrap()
}

fn plugin_id(value: &str) -> PluginId {
    let low = value.bytes().fold(0_u64, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(u64::from(byte))
    });
    PluginId::new(1, low)
}

fn capability(plugin_name: &str, plugin_version: AbiVersion, key: &str) -> Capability {
    Capability::new(
        CapabilityKey::new(key).unwrap(),
        Provider::new(
            provider_id(plugin_name),
            ProviderVersion::new(format!(
                "{}.{}.{}",
                plugin_version.major, plugin_version.minor, plugin_version.patch
            ))
            .unwrap(),
        ),
    )
}

fn artifact(plugin_name: &str, key: &str) -> PluginArtifact {
    let plugin_version = version(1, 2, 3);
    let api = fm_plugin_api::PluginManifest::new(
        plugin_id(plugin_name),
        plugin_version,
        AbiVersionRange::exact(version(1, 1, 0)),
        plugin_name,
    )
    .unwrap();
    PluginArtifact::new(
        format!("/plugins/{plugin_name}"),
        PluginManifest {
            api,
            provider_id: provider_id(plugin_name),
            budget: ResourceBudget::new(1_024, 500, 50),
            capabilities: vec![capability(plugin_name, plugin_version, key)],
        },
        Signature::new(b"trusted".to_vec()).unwrap(),
    )
}

fn discovery_policy() -> DiscoveryPolicy {
    DiscoveryPolicy {
        host_abi: AbiVersionRange::new(version(1, 0, 0), version(1, 9, 0)).unwrap(),
        granted_capabilities: CapabilitySet::default(),
        maximum_budget: ResourceBudget::new(2_048, 1_000, 100),
    }
}

fn supervisor_policy() -> SupervisorPolicy {
    SupervisorPolicy {
        heartbeat_timeout_ms: 100,
        crash_window_ms: 1_000,
        initial_backoff_ms: 10,
        maximum_backoff_ms: 40,
        crashes_before_quarantine: 3,
    }
}

fn catalog(artifacts: Vec<PluginArtifact>) -> Catalog {
    let report = Catalog::discover(artifacts, discovery_policy(), &FakeVerifier);
    assert!(report.rejections.is_empty());
    report.catalog
}

fn supervisor(artifacts: Vec<PluginArtifact>) -> Supervisor<FakeChildController> {
    Supervisor::new(
        catalog(artifacts),
        FakeChildController::default(),
        supervisor_policy(),
        IpcLimits::new(2, 4),
    )
    .unwrap()
}

#[test]
fn discovery_checks_duplicates_signatures_api_budget_and_provider() {
    let duplicate_a = artifact("duplicate", "plugin.duplicate-a");
    let duplicate_b = artifact("duplicate", "plugin.duplicate-b");
    let mut bad_signature = artifact("bad-signature", "plugin.bad-signature");
    bad_signature.signature = Signature::new(b"wrong".to_vec()).unwrap();
    let mut bad_api = artifact("bad-api", "plugin.bad-api");
    bad_api.manifest.api.abi = AbiVersionRange::exact(version(2, 0, 0));
    let mut excessive = artifact("excessive", "plugin.excessive");
    excessive.manifest.budget.memory_bytes = 2_049;
    let mut impersonator = artifact("impersonator", "plugin.impersonator");
    impersonator.manifest.capabilities[0].provider.id = provider_id("someone-else");
    let good = artifact("good", "plugin.good");

    let report = Catalog::discover(
        vec![
            duplicate_a,
            good,
            bad_signature,
            duplicate_b,
            bad_api,
            excessive,
            impersonator,
        ],
        discovery_policy(),
        &FakeVerifier,
    );

    assert_eq!(report.catalog.len(), 1);
    assert!(report.catalog.get(&plugin_id("good")).is_some());
    assert_eq!(report.rejections.len(), 6);
    assert_eq!(
        report
            .rejections
            .iter()
            .filter(|rejection| matches!(rejection.reason, DiscoveryRejectionReason::DuplicateId))
            .count(),
        2
    );
    assert!(report.rejections.iter().any(|rejection| matches!(
        rejection.reason,
        DiscoveryRejectionReason::InvalidSignature { .. }
    )));
    assert!(report.rejections.iter().any(|rejection| matches!(
        rejection.reason,
        DiscoveryRejectionReason::UnsupportedApi(_)
    )));
    assert!(report.rejections.iter().any(|rejection| matches!(
        rejection.reason,
        DiscoveryRejectionReason::BudgetExceeded(_)
    )));
    assert!(report.rejections.iter().any(|rejection| matches!(
        rejection.reason,
        DiscoveryRejectionReason::CapabilityProviderMismatch { .. }
    )));
}

#[test]
fn startup_is_isolated_and_publishes_capabilities_only_when_ready() {
    let plugin = artifact("camera", "input.camera");
    let key = plugin.manifest.capabilities[0].key.clone();
    let plugin_id = plugin.manifest.plugin_id();
    let mut supervisor = supervisor(vec![plugin]);

    supervisor.start(plugin_id, 5).unwrap();
    assert_eq!(
        supervisor.status(plugin_id).unwrap().state,
        InstanceState::Starting
    );
    assert!(supervisor.registry().is_empty());
    assert_eq!(supervisor.controller().spawned.len(), 1);
    assert_eq!(
        supervisor.controller().spawned[0].1,
        ResourceBudget::new(1_024, 500, 50)
    );

    supervisor.poll(6, [ChildEvent::Ready { child: 1 }]);
    assert_eq!(
        supervisor.status(plugin_id).unwrap().state,
        InstanceState::Running
    );
    assert!(supervisor.registry().get(&key).is_some());
    assert!(
        supervisor
            .audit()
            .windows(2)
            .all(|records| records[0].sequence < records[1].sequence)
    );
}

#[test]
fn reported_resource_limit_terminates_and_withdraws_plugin() {
    let plugin = artifact("limiter", "effect.limiter");
    let key = plugin.manifest.capabilities[0].key.clone();
    let plugin_id = plugin.manifest.plugin_id();
    let mut supervisor = supervisor(vec![plugin]);
    supervisor.start(plugin_id, 0).unwrap();
    supervisor.poll(1, [ChildEvent::Ready { child: 1 }]);

    supervisor.poll(
        2,
        [ChildEvent::Heartbeat {
            child: 1,
            usage: ResourceUsage {
                memory_bytes: 1_025,
                fuel: 1,
                operation_ms: 1,
            },
        }],
    );

    let status = supervisor.status(plugin_id).unwrap();
    assert_eq!(status.state, InstanceState::Backoff);
    assert_eq!(status.restart_at_ms, Some(12));
    assert_eq!(supervisor.controller().terminated, vec![1]);
    assert!(supervisor.registry().get(&key).is_none());
    assert!(supervisor.audit().iter().any(|record| matches!(
        record.event,
        AuditEvent::Failed(Failure::BudgetExceeded {
            resource: BudgetResource::Memory,
            ..
        })
    )));
}

#[test]
fn crashes_restart_with_backoff_then_quarantine() {
    let plugin = artifact("crashy", "plugin.crashy");
    let plugin_id = plugin.manifest.plugin_id();
    let mut supervisor = supervisor(vec![plugin]);
    supervisor.start(plugin_id, 0).unwrap();
    supervisor.poll(1, [ChildEvent::Ready { child: 1 }]);

    supervisor.poll(
        2,
        [ChildEvent::Exited {
            child: 1,
            reason: "first".to_owned(),
        }],
    );
    assert_eq!(
        supervisor.status(plugin_id).unwrap().restart_at_ms,
        Some(12)
    );
    supervisor.poll(11, []);
    assert_eq!(supervisor.controller().spawned.len(), 1);
    supervisor.poll(12, []);
    supervisor.poll(13, [ChildEvent::Ready { child: 2 }]);
    supervisor.poll(
        14,
        [ChildEvent::Exited {
            child: 2,
            reason: "second".to_owned(),
        }],
    );
    assert_eq!(
        supervisor.status(plugin_id).unwrap().restart_at_ms,
        Some(34)
    );
    supervisor.poll(34, []);
    supervisor.poll(35, [ChildEvent::Ready { child: 3 }]);
    supervisor.poll(
        36,
        [ChildEvent::Exited {
            child: 3,
            reason: "third".to_owned(),
        }],
    );

    assert_eq!(
        supervisor.status(plugin_id).unwrap().state,
        InstanceState::Quarantined
    );
    supervisor.poll(1_000, []);
    assert_eq!(supervisor.controller().spawned.len(), 3);
    supervisor.release_quarantine(plugin_id, 1_001).unwrap();
    assert_eq!(
        supervisor.status(plugin_id).unwrap().state,
        InstanceState::Discovered
    );
}

#[test]
fn ipc_queue_is_bounded_and_flushes_fifo_after_ready() {
    let plugin = artifact("ipc", "plugin.ipc");
    let plugin_id = plugin.manifest.plugin_id();
    let mut supervisor = supervisor(vec![plugin]);

    supervisor
        .enqueue(plugin_id, IpcMessage::new(1, b"one".to_vec()), 0)
        .unwrap();
    supervisor
        .enqueue(plugin_id, IpcMessage::new(2, b"two".to_vec()), 0)
        .unwrap();
    assert!(matches!(
        supervisor.enqueue(plugin_id, IpcMessage::new(3, b"tri".to_vec()), 0),
        Err(SupervisorError::Queue(QueueError::Full { capacity: 2 }))
    ));
    let mut separate = BoundedIpcQueue::new(IpcLimits::new(1, 4)).unwrap();
    assert!(matches!(
        separate.push(IpcMessage::new(4, b"large".to_vec())),
        Err(QueueError::MessageTooLarge {
            size: 5,
            maximum: 4
        })
    ));

    supervisor.start(plugin_id, 1).unwrap();
    supervisor.poll(2, [ChildEvent::Ready { child: 1 }]);
    assert_eq!(
        supervisor
            .controller()
            .sent
            .iter()
            .map(|(_, message)| message.id)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(supervisor.status(plugin_id).unwrap().queued_messages, 0);
}

#[test]
fn missed_heartbeat_withdraws_capability_before_restart() {
    let plugin = artifact("heartbeat", "plugin.heartbeat");
    let plugin_id = plugin.manifest.plugin_id();
    let key = plugin.manifest.capabilities[0].key.clone();
    let mut supervisor = supervisor(vec![plugin]);
    supervisor.start(plugin_id, 0).unwrap();
    supervisor.poll(1, [ChildEvent::Ready { child: 1 }]);
    assert!(supervisor.registry().get(&key).is_some());

    supervisor.poll(102, []);

    assert_eq!(
        supervisor.status(plugin_id).unwrap().state,
        InstanceState::Backoff
    );
    assert!(supervisor.registry().get(&key).is_none());
    let withdrawal = supervisor
        .audit()
        .iter()
        .position(|record| matches!(record.event, AuditEvent::CapabilitiesWithdrawn { .. }));
    let restart = supervisor
        .audit()
        .iter()
        .position(|record| matches!(record.event, AuditEvent::RestartScheduled { .. }));
    assert!(withdrawal < restart);
}
