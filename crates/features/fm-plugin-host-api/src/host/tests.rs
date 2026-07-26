use super::*;
use crate::{
    ApiVersion, CommandReceipt, DataMessage, EventBatch, EventMessage, IdempotencyKey,
    MigrationError, StateVersion,
};

fn host() -> PluginHost {
    PluginHost::new(
        ApiCompatibility::new(1, 0, 2),
        ProtocolLimits::default(),
        StateEpoch::new(1),
    )
    .unwrap()
}

fn manifest() -> PluginManifest {
    PluginManifest::new(
        "com.example.test",
        ApiVersion::new(1, 1),
        StateVersion::new(2),
    )
    .requesting("network.read")
}

fn execute(
    host: &mut PluginHost,
    number: u32,
    command: PluginCommand,
) -> ApplyOutcome<PluginCommandResult, PluginEvent> {
    host.execute(
        CommandEnvelope::new(
            format!("command-{number}"),
            format!("idempotency-{number}"),
            command,
        ),
        100,
    )
}

fn assert_accepted(outcome: &ApplyOutcome<PluginCommandResult, PluginEvent>) {
    assert!(matches!(outcome.receipt, CommandReceipt::Accepted { .. }));
}

fn discover_validate_load(host: &mut PluginHost) {
    assert_accepted(&execute(host, 1, PluginCommand::Discover(manifest())));
    assert_accepted(&execute(
        host,
        2,
        PluginCommand::Validate {
            plugin_id: "com.example.test".into(),
        },
    ));
    assert_accepted(&execute(
        host,
        3,
        PluginCommand::Load {
            plugin_id: "com.example.test".into(),
            snapshot: None,
        },
    ));
}

#[test]
fn lifecycle_follows_discover_validate_load_start_stop() {
    let mut host = host();
    discover_validate_load(&mut host);
    assert_accepted(&execute(
        &mut host,
        4,
        PluginCommand::Start {
            plugin_id: "com.example.test".into(),
        },
    ));
    assert_eq!(
        host.plugin(&PluginId::from("com.example.test"))
            .unwrap()
            .state(),
        PluginState::Started
    );
    assert_accepted(&execute(
        &mut host,
        5,
        PluginCommand::Stop {
            plugin_id: "com.example.test".into(),
        },
    ));
    assert_eq!(
        host.plugin(&PluginId::from("com.example.test"))
            .unwrap()
            .state(),
        PluginState::Stopped
    );
}

#[test]
fn capabilities_are_default_deny_and_require_a_request() {
    let mut host = host();
    execute(&mut host, 1, PluginCommand::Discover(manifest()));
    let record = host.plugin(&PluginId::from("com.example.test")).unwrap();
    assert_eq!(
        record.capability_decision(&CapabilityId::from("network.read")),
        CapabilityDecision::Denied
    );
    assert_eq!(
        record.capability_decision(&CapabilityId::from("filesystem.read")),
        CapabilityDecision::Denied
    );

    let rejected = execute(
        &mut host,
        2,
        PluginCommand::DecideCapability {
            plugin_id: "com.example.test".into(),
            capability: "filesystem.read".into(),
            decision: CapabilityDecision::Granted,
        },
    );
    assert!(matches!(rejected.receipt, CommandReceipt::Rejected { .. }));

    execute(
        &mut host,
        3,
        PluginCommand::DecideCapability {
            plugin_id: "com.example.test".into(),
            capability: "network.read".into(),
            decision: CapabilityDecision::Granted,
        },
    );
    assert_eq!(
        host.plugin(&PluginId::from("com.example.test"))
            .unwrap()
            .capability_decision(&CapabilityId::from("network.read")),
        CapabilityDecision::Granted
    );
}

#[test]
fn host_mutations_are_enveloped_idempotent_commands() {
    let mut host = host();
    let envelope =
        CommandEnvelope::new("discover", "same-key", PluginCommand::Discover(manifest()));
    let first = host.execute(envelope.clone(), 0);
    let replay = host.execute(envelope, 0);
    assert!(!first.replayed);
    assert!(replay.replayed);
    assert!(replay.events.is_empty());
    assert_eq!(host.revision(), Revision::new(1));
    assert!(
        host.commands
            .receipt(&IdempotencyKey::from("same-key"))
            .is_some()
    );
}

#[test]
fn event_data_and_batch_bounds_are_enforced() {
    let limits = ProtocolLimits {
        max_event_bytes: 3,
        max_data_bytes: 4,
        max_events_per_batch: 1,
        ..ProtocolLimits::default()
    };
    assert!(EventMessage::new("plugin", "topic", vec![0; 4], &limits).is_err());
    assert!(DataMessage::new("plugin", "channel", vec![0; 4], &limits).is_ok());
    let item = EventMessage::new("plugin", "topic", vec![0; 3], &limits).unwrap();
    assert!(EventBatch::new([item.clone()], &limits).is_ok());
    assert!(EventBatch::new([item.clone(), item], &limits).is_err());

    let loose_limits = ProtocolLimits::default();
    let loose_item = EventMessage::new("plugin", "topic", vec![0; 4], &loose_limits).unwrap();
    assert!(EventBatch::new([loose_item], &limits).is_err());
}

#[test]
fn snapshots_migrate_one_validated_version_at_a_time() {
    let limits = ProtocolLimits::default();
    let snapshot =
        StateSnapshot::new("com.example.test", StateVersion::new(0), [0], &limits).unwrap();
    let mut visited = Vec::new();
    let mut migrator = |request: crate::MigrationRequest| {
        visited.push((request.from_version, request.to_version));
        let mut data = request.snapshot.data().to_vec();
        data.push(u8::try_from(request.to_version.get()).unwrap());
        StateSnapshot::new(request.plugin_id, request.to_version, data, &limits)
            .map_err(MigrationError::from)
    };
    let migrated = crate::migrate_snapshot(
        snapshot,
        StateVersion::new(2),
        Deadline::from_millis(100),
        100,
        &limits,
        &mut migrator,
    )
    .unwrap();
    assert_eq!(migrated.version(), StateVersion::new(2));
    assert_eq!(migrated.data(), &[0, 1, 2]);
    assert_eq!(visited.len(), 2);
}

#[test]
fn crash_quarantines_and_revokes_grants() {
    let mut host = host();
    discover_validate_load(&mut host);
    execute(
        &mut host,
        4,
        PluginCommand::DecideCapability {
            plugin_id: "com.example.test".into(),
            capability: "network.read".into(),
            decision: CapabilityDecision::Granted,
        },
    );
    execute(
        &mut host,
        5,
        PluginCommand::Start {
            plugin_id: "com.example.test".into(),
        },
    );
    let report = CrashReport::new(
        "com.example.test",
        120,
        "trap",
        b"stack".to_vec(),
        host.limits(),
    )
    .unwrap();
    assert_accepted(&execute(&mut host, 6, PluginCommand::ReportCrash(report)));
    let record = host.plugin(&PluginId::from("com.example.test")).unwrap();
    assert_eq!(record.state(), PluginState::Quarantined);
    assert!(record.crash_report().is_some());
    assert_eq!(
        record.capability_decision(&CapabilityId::from("network.read")),
        CapabilityDecision::Denied
    );

    let restart = execute(
        &mut host,
        7,
        PluginCommand::Start {
            plugin_id: "com.example.test".into(),
        },
    );
    assert!(matches!(restart.receipt, CommandReceipt::Rejected { .. }));
}

#[test]
fn missed_heartbeat_deadline_quarantines_plugin() {
    let mut host = host();
    discover_validate_load(&mut host);
    execute(
        &mut host,
        4,
        PluginCommand::Start {
            plugin_id: "com.example.test".into(),
        },
    );
    let outcome = host.execute(
        CommandEnvelope::new(
            "deadline",
            "deadline-key",
            PluginCommand::CheckHeartbeatDeadline {
                plugin_id: "com.example.test".into(),
                deadline: Deadline::from_millis(99),
            },
        ),
        100,
    );
    assert_accepted(&outcome);
    assert_eq!(
        host.plugin(&PluginId::from("com.example.test"))
            .unwrap()
            .state(),
        PluginState::Quarantined
    );
}
