use std::num::{NonZeroU128, NonZeroUsize};

use fm_capabilities::StableId;
use fm_frame::{
    ClockDomainId, MediaTimestamp, MediaTiming, NormalizedDuration, NormalizedTimestamp,
    OriginalTimestamp, SequenceNumber, TimeBase,
};
use fm_io_api::fake::{FakeDiscovery, FakeMedia, FakeSink, FakeSource, InjectError};
use fm_io_api::{
    ClockCapability, DeliveryStatus, DeviceId, Discovery, DiscoveryEventKind, DriverState,
    EndpointCapabilities, FallbackKind, FormatDescriptor, IoError, LifecycleState, MediaSink,
    MediaSource, MediaTransfer, MemoryDomain, OpenOptions, PermissionState, Remediation,
    SignalLossPolicy, SinkDescriptor, SinkId, SourceDescriptor, SourceId, TimestampCapabilities,
    TimestampQuality, TransferLimits, WriteError, deliver_isolated,
};

fn nonzero(value: u128) -> NonZeroU128 {
    NonZeroU128::new(value).unwrap()
}

fn format(name: &str) -> FormatDescriptor {
    FormatDescriptor::new(StableId::new(name).unwrap())
}

fn clock() -> ClockCapability {
    ClockCapability {
        domain: ClockDomainId::new(nonzero(7)),
        timestamps: TimestampCapabilities {
            quality: TimestampQuality::Hardware,
            resolution_nanos: nonzero(1),
            max_error_nanos: Some(0),
            monotonic: true,
        },
        can_follow_external: true,
    }
}

fn capabilities() -> EndpointCapabilities {
    EndpointCapabilities {
        formats: vec![format("video.raw")],
        clocks: vec![clock()],
        memory_domains: vec![MemoryDomain::Cpu],
        transfer: TransferLimits::new(NonZeroUsize::new(2).unwrap(), NonZeroUsize::new(8).unwrap()),
    }
}

fn source_descriptor(id: u128) -> SourceDescriptor {
    SourceDescriptor {
        id: SourceId::new(nonzero(id)),
        device_id: DeviceId::new(nonzero(100 + id)),
        stable_key: format!("fake.source.v1:{id}"),
        name: format!("source-{id}"),
        capabilities: capabilities(),
        permission: PermissionState::Granted,
        driver: DriverState::Ready,
    }
}

fn sink_descriptor(id: u128) -> SinkDescriptor {
    SinkDescriptor {
        id: SinkId::new(nonzero(id)),
        device_id: DeviceId::new(nonzero(200 + id)),
        name: format!("sink-{id}"),
        capabilities: capabilities(),
        permission: PermissionState::Granted,
        driver: DriverState::Ready,
    }
}

fn options(policy: SignalLossPolicy) -> OpenOptions {
    OpenOptions {
        format: format("video.raw"),
        clock_domain: clock().domain,
        memory_domain: MemoryDomain::Cpu,
        queue_capacity: NonZeroUsize::new(1).unwrap(),
        signal_loss: policy,
    }
}

fn media(sequence: u64, timestamp_nanos: i64) -> FakeMedia {
    let time_base = TimeBase::new(1, 1_000_000_000).unwrap();
    let timing = MediaTiming::new(
        OriginalTimestamp::new(MediaTimestamp::new(timestamp_nanos), time_base),
        NormalizedTimestamp::from_nanos(timestamp_nanos),
        NormalizedDuration::from_nanos(1).unwrap(),
        clock().domain,
        SequenceNumber::new(sequence),
    )
    .unwrap();
    FakeMedia::new(timing, vec![u8::try_from(sequence).unwrap()])
}

fn assert_source_lifecycle<S>(source: &mut S)
where
    S: MediaSource<Media = FakeMedia>,
{
    assert_eq!(source.lifecycle(), LifecycleState::Closed);
    source.open(options(SignalLossPolicy::Stop)).unwrap();
    assert_eq!(source.lifecycle(), LifecycleState::Open);
    source.start().unwrap();
    assert_eq!(source.lifecycle(), LifecycleState::Running);
    source.stop().unwrap();
    source.close().unwrap();
    source.open(options(SignalLossPolicy::Stop)).unwrap();
    source.close().unwrap();
}

fn assert_sink_lifecycle<S>(sink: &mut S)
where
    S: MediaSink<Media = FakeMedia>,
{
    assert_eq!(sink.lifecycle(), LifecycleState::Closed);
    sink.open(options(SignalLossPolicy::Stop)).unwrap();
    sink.start().unwrap();
    sink.stop().unwrap();
    sink.close().unwrap();
    sink.open(options(SignalLossPolicy::Stop)).unwrap();
    sink.close().unwrap();
}

#[test]
fn shared_source_and_sink_lifecycle_conformance() {
    assert_source_lifecycle(&mut FakeSource::new(source_descriptor(1)));
    assert_sink_lifecycle(&mut FakeSink::new(sink_descriptor(1)));
}

#[test]
fn discovery_snapshots_and_hotplug_events_are_ordered() {
    let mut discovery = FakeDiscovery::new();
    discovery.add_source(source_descriptor(2));
    discovery.add_source(source_descriptor(1));
    discovery.add_sink(sink_descriptor(1));

    let snapshot = discovery.snapshot();
    assert_eq!(snapshot.generation, 3);
    assert_eq!(snapshot.sources[0].id, SourceId::new(nonzero(1)));
    assert!(matches!(
        discovery.next_event().unwrap().kind,
        DiscoveryEventKind::SourceAdded(_)
    ));

    assert!(discovery.remove_source(SourceId::new(nonzero(1))));
    let mut last = None;
    while let Some(event) = discovery.next_event() {
        last = Some(event);
    }
    assert!(matches!(
        last.unwrap().kind,
        DiscoveryEventKind::SourceRemoved(id) if id == SourceId::new(nonzero(1))
    ));
}

#[test]
fn source_stable_key_survives_discovery_without_aliasing_runtime_ids() {
    let descriptor = source_descriptor(7);
    let expected_key = descriptor.stable_key.clone();
    let runtime_id = descriptor.id.to_string();
    let display_name = descriptor.name.clone();
    let mut discovery = FakeDiscovery::new();
    discovery.add_source(descriptor);

    let discovered = discovery.snapshot().sources.pop().unwrap();
    assert_eq!(discovered.stable_key, expected_key);
    assert_ne!(discovered.stable_key, runtime_id);
    assert_ne!(discovered.stable_key, display_name);
}

#[test]
fn open_reports_unsupported_format_and_permission_remediation() {
    let mut source = FakeSource::<FakeMedia>::new(source_descriptor(1));
    let mut unsupported = options(SignalLossPolicy::Stop);
    unsupported.format = format("audio.raw");
    assert_eq!(source.open(unsupported), Err(IoError::UnsupportedFormat));

    source.set_permission(PermissionState::Denied {
        remediation: Remediation::OpenSystemSettings,
    });
    assert_eq!(
        source.open(options(SignalLossPolicy::Stop)),
        Err(IoError::PermissionDenied {
            remediation: Remediation::OpenSystemSettings
        })
    );
}

#[test]
fn loss_uses_hold_fallback_and_recovery_resumes_running() {
    let mut source = FakeSource::new(source_descriptor(1));
    source.open(options(SignalLossPolicy::Hold)).unwrap();
    source.start().unwrap();
    source.inject(media(0, 1)).unwrap();
    assert!(matches!(
        source.try_receive().unwrap(),
        Some(MediaTransfer::Live(_))
    ));

    source.unplug();
    assert_eq!(source.lifecycle(), LifecycleState::Lost);
    assert!(matches!(
        source.try_receive().unwrap(),
        Some(MediaTransfer::Fallback {
            kind: FallbackKind::Hold,
            ..
        })
    ));
    source.begin_recovery().unwrap();
    assert!(matches!(
        source.finish_recovery(),
        Err(IoError::EndpointUnavailable { .. })
    ));
    source.plug_in();
    source.finish_recovery().unwrap();
    assert_eq!(source.lifecycle(), LifecycleState::Running);
}

#[test]
fn slate_and_stop_policies_are_explicit() {
    let mut slate_source = FakeSource::new(source_descriptor(1));
    slate_source.set_slate(media(9, 9));
    slate_source.open(options(SignalLossPolicy::Slate)).unwrap();
    slate_source.start().unwrap();
    slate_source.lose_signal();
    assert!(matches!(
        slate_source.try_receive().unwrap(),
        Some(MediaTransfer::Fallback {
            kind: FallbackKind::Slate,
            ..
        })
    ));

    let mut stop_source = FakeSource::<FakeMedia>::new(source_descriptor(2));
    stop_source.open(options(SignalLossPolicy::Stop)).unwrap();
    stop_source.start().unwrap();
    stop_source.lose_signal();
    assert!(matches!(
        stop_source.try_receive(),
        Err(IoError::SignalLost {
            policy: SignalLossPolicy::Stop
        })
    ));
}

#[test]
fn malformed_timestamps_are_rejected_without_advancing_state() {
    let mut source = FakeSource::new(source_descriptor(1));
    source.open(options(SignalLossPolicy::Stop)).unwrap();
    source.start().unwrap();
    source.inject(media(0, 10)).unwrap();
    source.try_receive().unwrap().unwrap();
    source.inject(media(2, 20)).unwrap();
    assert!(matches!(
        source.try_receive(),
        Err(IoError::MalformedTimestamp(_))
    ));
    source.inject(media(1, 20)).unwrap();
    assert!(source.try_receive().unwrap().is_some());
}

#[test]
fn source_and_sink_queues_enforce_opened_bounds() {
    let mut source = FakeSource::new(source_descriptor(1));
    source.open(options(SignalLossPolicy::Stop)).unwrap();
    source.inject(media(0, 1)).unwrap();
    assert!(matches!(
        source.inject(media(1, 2)),
        Err(InjectError::QueueFull(_))
    ));

    let mut sink = FakeSink::new(sink_descriptor(1));
    sink.open(options(SignalLossPolicy::Stop)).unwrap();
    sink.start().unwrap();
    sink.try_send(media(0, 1)).unwrap();
    assert!(matches!(
        sink.try_send(media(1, 2)),
        Err(WriteError::QueueFull(_))
    ));
}

#[test]
fn sink_failure_is_isolated_from_other_sinks() {
    let mut failed = FakeSink::new(sink_descriptor(1));
    let mut healthy = FakeSink::new(sink_descriptor(2));
    for sink in [
        &mut failed as &mut dyn MediaSink<Media = FakeMedia>,
        &mut healthy,
    ] {
        sink.open(options(SignalLossPolicy::Stop)).unwrap();
        sink.start().unwrap();
    }
    failed.fail_next_send(IoError::AdapterFailure {
        detail: "injected failure".to_owned(),
        remediation: Some(Remediation::RestartAdapter),
    });

    let outcomes = deliver_isolated(
        &mut [
            &mut failed as &mut dyn MediaSink<Media = FakeMedia>,
            &mut healthy,
        ],
        &media(0, 1),
    );
    assert!(matches!(outcomes[0].status, DeliveryStatus::Failed(_)));
    assert_eq!(outcomes[1].status, DeliveryStatus::Accepted);
    assert_eq!(failed.queued(), 0);
    assert_eq!(healthy.queued(), 1);
}
