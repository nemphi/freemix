use std::num::NonZeroU128;

use fm_frame::{
    AlphaMode, BridgeId, ChromaLocation, ColorMetadata, ColorPrimaries, EngineInstanceId,
    LocalPreviewLease, LocalPreviewLeaseError, MatrixCoefficients, MemoryDomain,
    OsHandleReferenceId, PhysicalAdapterToken, PixelFormat, PreviewImageDescriptor,
    PreviewImageDescriptorError, PreviewLeaseId, PreviewLeaseRegistry, PreviewLeaseRegistryError,
    PreviewReleaseAck, PreviewStreamDescriptor, PreviewStreamDescriptorError, PreviewStreamId,
    PreviewTarget, PreviewTransport, ReleaseOwner, ReleaseOwnerId, ReleaseOwnership, ResourceId,
    ResourceLease, SignalRange, SynchronizationId, SynchronizationToken, TransferFunction,
    VideoDimensions, VideoFrameMetadata, VideoFrameMetadataError,
};

fn nonzero(value: u128) -> NonZeroU128 {
    NonZeroU128::new(value).unwrap()
}

fn stream_id(value: u128) -> PreviewStreamId {
    PreviewStreamId::new(nonzero(value))
}

fn engine_id(value: u128) -> EngineInstanceId {
    EngineInstanceId::new(nonzero(value))
}

fn adapter(value: u128) -> PhysicalAdapterToken {
    PhysicalAdapterToken::new(nonzero(value))
}

fn shared_stream(
    stream: u128,
    engine: u128,
    runtime_generation: u64,
    adapter_value: u128,
) -> PreviewStreamDescriptor {
    PreviewStreamDescriptor::new(
        stream_id(stream),
        PreviewTarget::Preview,
        PreviewTransport::SharedImage,
        engine_id(engine),
        runtime_generation,
        Some(adapter(adapter_value)),
    )
    .unwrap()
}

fn rgb_metadata() -> VideoFrameMetadata {
    VideoFrameMetadata::new(
        ColorMetadata {
            primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Srgb,
            matrix: MatrixCoefficients::Identity,
            range: SignalRange::Full,
            chroma_location: ChromaLocation::Center,
        },
        Some(AlphaMode::Straight),
    )
}

fn image() -> PreviewImageDescriptor {
    PreviewImageDescriptor::new(
        VideoDimensions::new(1920, 1080).unwrap(),
        PixelFormat::Bgra8,
        rgb_metadata(),
    )
    .unwrap()
}

fn resource(
    memory_domain: MemoryDomain,
    ready: Option<SynchronizationToken>,
    release: Option<SynchronizationToken>,
    ownership: ReleaseOwnership,
) -> ResourceLease {
    resource_with_id(101, memory_domain, ready, release, ownership)
}

fn resource_with_id(
    resource_value: u128,
    memory_domain: MemoryDomain,
    ready: Option<SynchronizationToken>,
    release: Option<SynchronizationToken>,
    ownership: ReleaseOwnership,
) -> ResourceLease {
    ResourceLease::new(
        BridgeId::new(nonzero(100)),
        ResourceId::new(nonzero(resource_value)),
        memory_domain,
        ready,
        release,
        ReleaseOwner::new(ReleaseOwnerId::new(nonzero(102)), ownership),
    )
    .unwrap()
}

fn synchronization(value: u64) -> SynchronizationToken {
    SynchronizationToken::new(SynchronizationId::new(nonzero(103)), value)
}

fn lease(
    stream: PreviewStreamDescriptor,
    lease_value: u128,
    frame_sequence: u64,
    resize_generation: u64,
) -> LocalPreviewLease {
    lease_with_image(
        stream,
        lease_value,
        frame_sequence,
        resize_generation,
        image(),
    )
}

fn lease_with_image(
    stream: PreviewStreamDescriptor,
    lease_value: u128,
    frame_sequence: u64,
    resize_generation: u64,
    image: PreviewImageDescriptor,
) -> LocalPreviewLease {
    LocalPreviewLease::new(
        stream,
        PreviewLeaseId::new(nonzero(lease_value)),
        stream.adapter_token().unwrap(),
        OsHandleReferenceId::new(nonzero(lease_value + 1_000)),
        resource_with_id(
            lease_value + 2_000,
            MemoryDomain::Vulkan,
            Some(synchronization(frame_sequence * 2)),
            Some(synchronization(frame_sequence * 2 + 1)),
            ReleaseOwnership::LeaseHolderSignals,
        ),
        frame_sequence,
        resize_generation,
        image,
    )
    .unwrap()
}

#[test]
fn preview_ids_preserve_full_width_nonzero_values() {
    let maximum = nonzero(u128::MAX);

    assert_eq!(PreviewStreamId::new(maximum).get(), maximum);
    assert_eq!(EngineInstanceId::new(maximum).get(), maximum);
    assert_eq!(PhysicalAdapterToken::new(maximum).get(), maximum);
    assert_eq!(PreviewLeaseId::new(maximum).get(), maximum);
    assert_eq!(OsHandleReferenceId::new(maximum).get(), maximum);
}

#[test]
fn stream_descriptor_enforces_transport_adapter_rules() {
    assert_eq!(
        PreviewStreamDescriptor::new(
            stream_id(1),
            PreviewTarget::Program,
            PreviewTransport::SharedImage,
            engine_id(2),
            3,
            None,
        ),
        Err(PreviewStreamDescriptorError::SharedImageRequiresAdapter)
    );
    assert_eq!(
        PreviewStreamDescriptor::new(
            stream_id(1),
            PreviewTarget::Program,
            PreviewTransport::EncodedLoopback,
            engine_id(2),
            3,
            Some(adapter(4)),
        ),
        Err(PreviewStreamDescriptorError::EncodedLoopbackForbidsAdapter)
    );

    let encoded = PreviewStreamDescriptor::new(
        stream_id(1),
        PreviewTarget::Program,
        PreviewTransport::EncodedLoopback,
        engine_id(2),
        3,
        None,
    )
    .unwrap();
    assert_eq!(encoded.adapter_token(), None);
}

#[test]
fn image_descriptor_rejects_yuv422_and_preserves_metadata_error() {
    assert_eq!(
        PreviewImageDescriptor::new(
            VideoDimensions::new(720, 480).unwrap(),
            PixelFormat::Yuv422,
            VideoFrameMetadata::new(ColorMetadata::default(), None),
        ),
        Err(PreviewImageDescriptorError::UnsupportedSharedImageFormat {
            pixel_format: PixelFormat::Yuv422,
        })
    );

    let invalid_metadata = VideoFrameMetadata::new(
        ColorMetadata {
            matrix: MatrixCoefficients::Bt709,
            ..rgb_metadata().color()
        },
        Some(AlphaMode::Straight),
    );
    assert_eq!(
        PreviewImageDescriptor::new(
            VideoDimensions::new(1, 1).unwrap(),
            PixelFormat::Rgba8,
            invalid_metadata,
        ),
        Err(PreviewImageDescriptorError::Metadata(
            VideoFrameMetadataError::RgbMatrixMustBeIdentity {
                pixel_format: PixelFormat::Rgba8,
                matrix: MatrixCoefficients::Bt709,
            }
        ))
    );
}

#[test]
fn local_lease_rejects_cpu_and_missing_synchronization() {
    let stream = shared_stream(1, 2, 3, 4);
    let make = |resource_lease| {
        LocalPreviewLease::new(
            stream,
            PreviewLeaseId::new(nonzero(5)),
            adapter(4),
            OsHandleReferenceId::new(nonzero(6)),
            resource_lease,
            1,
            0,
            image(),
        )
    };

    assert_eq!(
        make(resource(
            MemoryDomain::Cpu,
            Some(synchronization(1)),
            Some(synchronization(2)),
            ReleaseOwnership::LeaseHolderSignals,
        )),
        Err(LocalPreviewLeaseError::CpuMemoryNotShareable)
    );
    assert_eq!(
        make(resource(
            MemoryDomain::Metal,
            None,
            Some(synchronization(2)),
            ReleaseOwnership::LeaseHolderSignals,
        )),
        Err(LocalPreviewLeaseError::MissingReadyToken)
    );
    assert_eq!(
        make(resource(
            MemoryDomain::D3D12,
            Some(synchronization(1)),
            None,
            ReleaseOwnership::OwnerReclaims,
        )),
        Err(LocalPreviewLeaseError::MissingReleaseToken)
    );
    assert_eq!(
        make(resource(
            MemoryDomain::DmaBuf,
            Some(synchronization(1)),
            Some(synchronization(2)),
            ReleaseOwnership::OwnerReclaims,
        )),
        Err(LocalPreviewLeaseError::HolderReleaseOwnershipRequired)
    );
}

#[test]
fn registry_issues_and_releases_exactly_one_lease() {
    let stream = shared_stream(1, 2, 3, 4);
    let lease = lease(stream, 10, 1, 0);
    let acknowledgement = PreviewReleaseAck::for_lease(&lease);
    let mut registry = PreviewLeaseRegistry::new(stream, 2, 100).unwrap();

    registry.issue(&lease, 1_000).unwrap();
    assert_eq!(registry.outstanding_count(), 1);
    assert_eq!(registry.capacity(), 2);
    assert_eq!(registry.stream_descriptor(), stream);
    assert_eq!(
        registry.acknowledge_release(acknowledgement),
        Ok(lease.lease_id())
    );
    assert_eq!(registry.outstanding_count(), 0);
    assert_eq!(
        registry.acknowledge_release(acknowledgement),
        Err(PreviewLeaseRegistryError::UnknownOrStaleLease {
            lease_id: lease.lease_id(),
        })
    );
}

#[test]
fn capacity_failure_does_not_advance_ordering_state() {
    let stream = shared_stream(1, 2, 3, 4);
    let first = lease(stream, 10, 1, 0);
    let second = lease(stream, 11, 2, 0);
    let mut registry = PreviewLeaseRegistry::new(stream, 1, 100).unwrap();

    registry.issue(&first, 0).unwrap();
    assert_eq!(
        registry.issue(&second, 1),
        Err(PreviewLeaseRegistryError::CapacityExhausted { maximum: 1 })
    );
    assert_eq!(registry.outstanding_count(), 1);
    registry
        .acknowledge_release(PreviewReleaseAck::for_lease(&first))
        .unwrap();
    registry.issue(&second, 1).unwrap();
}

#[test]
fn registry_enforces_lease_frame_and_resize_ordering_without_partial_mutation() {
    let stream = shared_stream(1, 2, 3, 4);
    let first = lease(stream, 10, 10, 7);
    let mut registry = PreviewLeaseRegistry::new(stream, 4, 100).unwrap();
    registry.issue(&first, 0).unwrap();

    let reused_id = lease(stream, 10, 11, 7);
    assert!(matches!(
        registry.issue(&reused_id, 1),
        Err(PreviewLeaseRegistryError::LeaseIdNotIncreasing { .. })
    ));
    let repeated_frame = lease(stream, 11, 10, 7);
    assert!(matches!(
        registry.issue(&repeated_frame, 1),
        Err(PreviewLeaseRegistryError::FrameSequenceNotIncreasing { .. })
    ));
    let skipped_resize = lease(stream, 11, 11, 9);
    assert_eq!(
        registry.issue(&skipped_resize, 1),
        Err(PreviewLeaseRegistryError::ResizeGenerationOutOfOrder {
            previous: 7,
            actual: 9,
        })
    );

    registry.issue(&lease(stream, 11, 11, 8), 1).unwrap();
    registry.issue(&lease(stream, 12, 12, 8), 2).unwrap();
}

#[test]
fn image_changes_require_a_new_resize_generation() {
    let stream = shared_stream(1, 2, 3, 4);
    let first = lease(stream, 10, 1, 0);
    let changed_image = PreviewImageDescriptor::new(
        VideoDimensions::new(1280, 720).unwrap(),
        PixelFormat::Bgra8,
        rgb_metadata(),
    )
    .unwrap();
    let changed_without_resize = lease_with_image(stream, 11, 2, 0, changed_image);
    let changed_with_resize = lease_with_image(stream, 12, 3, 1, changed_image);
    let mut registry = PreviewLeaseRegistry::new(stream, 3, 100).unwrap();

    registry.issue(&first, 0).unwrap();
    assert_eq!(
        registry.issue(&changed_without_resize, 1),
        Err(
            PreviewLeaseRegistryError::ImageChangedWithoutResizeGeneration {
                resize_generation: 0,
            }
        )
    );
    registry.issue(&changed_with_resize, 1).unwrap();
}

#[test]
fn outstanding_resources_and_handle_references_cannot_be_reissued() {
    let stream = shared_stream(1, 2, 3, 4);
    let first = lease(stream, 10, 1, 0);
    let same_resource = LocalPreviewLease::new(
        stream,
        PreviewLeaseId::new(nonzero(11)),
        adapter(4),
        OsHandleReferenceId::new(nonzero(2_000)),
        resource_with_id(
            first.resource_lease().resource_id().get().get(),
            MemoryDomain::Vulkan,
            Some(synchronization(4)),
            Some(synchronization(5)),
            ReleaseOwnership::LeaseHolderSignals,
        ),
        2,
        0,
        image(),
    )
    .unwrap();
    let same_handle = LocalPreviewLease::new(
        stream,
        PreviewLeaseId::new(nonzero(12)),
        adapter(4),
        first.handle_reference_id(),
        resource_with_id(
            3_000,
            MemoryDomain::Vulkan,
            Some(synchronization(6)),
            Some(synchronization(7)),
            ReleaseOwnership::LeaseHolderSignals,
        ),
        3,
        0,
        image(),
    )
    .unwrap();
    let mut registry = PreviewLeaseRegistry::new(stream, 3, 100).unwrap();

    registry.issue(&first, 0).unwrap();
    assert!(matches!(
        registry.issue(&same_resource, 1),
        Err(PreviewLeaseRegistryError::ResourceStillOutstanding { .. })
    ));
    assert!(matches!(
        registry.issue(&same_handle, 1),
        Err(PreviewLeaseRegistryError::HandleReferenceStillOutstanding { .. })
    ));
    registry
        .acknowledge_release(PreviewReleaseAck::for_lease(&first))
        .unwrap();
    registry.issue(&same_resource, 1).unwrap();
}

#[test]
fn registry_rejects_generation_and_adapter_mismatches() {
    let stream = shared_stream(1, 2, 3, 4);
    let mut registry = PreviewLeaseRegistry::new(stream, 2, 100).unwrap();
    let wrong_generation = lease(shared_stream(1, 2, 9, 4), 10, 1, 0);
    assert_eq!(
        registry.issue(&wrong_generation, 0),
        Err(PreviewLeaseRegistryError::RuntimeGenerationMismatch {
            expected: 3,
            actual: 9,
        })
    );

    let wrong_adapter = lease(shared_stream(1, 2, 3, 8), 10, 1, 0);
    assert_eq!(
        registry.issue(&wrong_adapter, 0),
        Err(PreviewLeaseRegistryError::AdapterMismatch {
            expected: adapter(4),
            actual: adapter(8),
        })
    );
    assert_eq!(registry.outstanding_count(), 0);
}

#[test]
fn wrong_release_token_retains_the_outstanding_lease() {
    let stream = shared_stream(1, 2, 3, 4);
    let lease = lease(stream, 10, 1, 0);
    let mut registry = PreviewLeaseRegistry::new(stream, 1, 100).unwrap();
    registry.issue(&lease, 0).unwrap();
    let wrong = PreviewReleaseAck::new(
        lease.stream_id(),
        lease.lease_id(),
        lease.engine_instance_id(),
        lease.runtime_generation(),
        synchronization(999),
    );

    assert!(matches!(
        registry.acknowledge_release(wrong),
        Err(PreviewLeaseRegistryError::WrongReleaseToken { .. })
    ));
    assert_eq!(registry.outstanding_count(), 1);
    registry
        .acknowledge_release(PreviewReleaseAck::for_lease(&lease))
        .unwrap();
}

#[test]
fn timeout_and_disconnect_reclamation_are_bounded_and_deterministic() {
    let stream = shared_stream(1, 2, 3, 4);
    let first = lease(stream, 10, 1, 0);
    let second = lease(stream, 11, 2, 0);
    let third = lease(stream, 12, 3, 0);
    let mut registry = PreviewLeaseRegistry::new(stream, 3, 10).unwrap();
    registry.issue(&first, 10).unwrap();
    registry.issue(&second, 20).unwrap();
    registry.issue(&third, 30).unwrap();

    assert_eq!(registry.reclaim_expired(29), vec![first.lease_id()]);
    assert_eq!(
        registry.reclaim_all_on_disconnect(),
        vec![second.lease_id(), third.lease_id()]
    );
    assert_eq!(registry.outstanding_count(), 0);
    assert_eq!(
        registry.acknowledge_release(PreviewReleaseAck::for_lease(&first)),
        Err(PreviewLeaseRegistryError::UnknownOrStaleLease {
            lease_id: first.lease_id(),
        })
    );
    assert_eq!(
        registry.acknowledge_release(PreviewReleaseAck::for_lease(&second)),
        Err(PreviewLeaseRegistryError::UnknownOrStaleLease {
            lease_id: second.lease_id(),
        })
    );
}

#[test]
fn registry_constructor_and_deadline_are_bounded() {
    let shared = shared_stream(1, 2, 3, 4);
    assert!(matches!(
        PreviewLeaseRegistry::new(shared, 0, 1),
        Err(PreviewLeaseRegistryError::ZeroCapacity)
    ));
    assert!(matches!(
        PreviewLeaseRegistry::new(
            shared,
            PreviewLeaseRegistry::HARD_MAXIMUM_OUTSTANDING + 1,
            1,
        ),
        Err(PreviewLeaseRegistryError::CapacityAboveHardMaximum { .. })
    ));
    assert!(matches!(
        PreviewLeaseRegistry::new(shared, 1, 0),
        Err(PreviewLeaseRegistryError::ZeroTimeout)
    ));

    let encoded = PreviewStreamDescriptor::new(
        stream_id(1),
        PreviewTarget::Program,
        PreviewTransport::EncodedLoopback,
        engine_id(2),
        3,
        None,
    )
    .unwrap();
    assert!(matches!(
        PreviewLeaseRegistry::new(encoded, 1, 1),
        Err(PreviewLeaseRegistryError::SharedImageTransportRequired)
    ));

    let mut registry = PreviewLeaseRegistry::new(shared, 1, 2).unwrap();
    assert_eq!(
        registry.issue(&lease(shared, 10, 1, 0), u64::MAX - 1),
        Err(PreviewLeaseRegistryError::TimeoutDeadlineOverflow {
            monotonic_now_ms: u64::MAX - 1,
            timeout_ms: 2,
        })
    );
    assert_eq!(registry.outstanding_count(), 0);
}
