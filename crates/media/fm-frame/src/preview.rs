use core::{fmt, num::NonZeroU128};

use fm_types::{
    InputId, MemoryDomain, PixelFormat, VideoDimensions, VideoFrameMetadata,
    VideoFrameMetadataError,
};

use crate::{ReleaseOwnership, ResourceId, ResourceLease, SynchronizationToken};

macro_rules! preview_id {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(NonZeroU128);

        impl $name {
            #[must_use]
            pub const fn new(value: NonZeroU128) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn get(self) -> NonZeroU128 {
                self.0
            }
        }

        impl From<NonZeroU128> for $name {
            fn from(value: NonZeroU128) -> Self {
                Self::new(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

preview_id!(
    PreviewStreamId,
    "Stable identity for one local-preview stream subscription."
);
preview_id!(
    EngineInstanceId,
    "Stable identity for one running media-engine instance."
);
preview_id!(
    PhysicalAdapterToken,
    "Opaque registration token for a physical adapter, not a native adapter handle."
);
preview_id!(PreviewLeaseId, "Stable identity for one preview lease.");
preview_id!(
    OsHandleReferenceId,
    "Opaque registration for a duplicated OS-handle reference, not the native handle itself."
);

/// Selects the engine output represented by a preview stream.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PreviewTarget {
    Program,
    Preview,
    Input(InputId),
}

/// Makes shared-image use and encoded fallback selection explicit.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PreviewTransport {
    SharedImage,
    EncodedLoopback,
}

/// Portable identity and compatibility contract for one preview stream.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PreviewStreamDescriptor {
    stream_id: PreviewStreamId,
    target: PreviewTarget,
    transport: PreviewTransport,
    engine_instance_id: EngineInstanceId,
    runtime_generation: u64,
    adapter_token: Option<PhysicalAdapterToken>,
}

impl PreviewStreamDescriptor {
    /// Creates a stream descriptor without exposing an adapter handle.
    ///
    /// # Errors
    ///
    /// Shared-image streams require an adapter token. Encoded loopback streams
    /// reject one because they do not claim adapter-local image compatibility.
    pub const fn new(
        stream_id: PreviewStreamId,
        target: PreviewTarget,
        transport: PreviewTransport,
        engine_instance_id: EngineInstanceId,
        runtime_generation: u64,
        adapter_token: Option<PhysicalAdapterToken>,
    ) -> Result<Self, PreviewStreamDescriptorError> {
        match (transport, adapter_token) {
            (PreviewTransport::SharedImage, None) => {
                return Err(PreviewStreamDescriptorError::SharedImageRequiresAdapter);
            }
            (PreviewTransport::EncodedLoopback, Some(_)) => {
                return Err(PreviewStreamDescriptorError::EncodedLoopbackForbidsAdapter);
            }
            _ => {}
        }
        Ok(Self {
            stream_id,
            target,
            transport,
            engine_instance_id,
            runtime_generation,
            adapter_token,
        })
    }

    #[must_use]
    pub const fn stream_id(self) -> PreviewStreamId {
        self.stream_id
    }

    #[must_use]
    pub const fn target(self) -> PreviewTarget {
        self.target
    }

    #[must_use]
    pub const fn transport(self) -> PreviewTransport {
        self.transport
    }

    #[must_use]
    pub const fn engine_instance_id(self) -> EngineInstanceId {
        self.engine_instance_id
    }

    #[must_use]
    pub const fn runtime_generation(self) -> u64 {
        self.runtime_generation
    }

    #[must_use]
    pub const fn adapter_token(self) -> Option<PhysicalAdapterToken> {
        self.adapter_token
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewStreamDescriptorError {
    SharedImageRequiresAdapter,
    EncodedLoopbackForbidsAdapter,
}

impl fmt::Display for PreviewStreamDescriptorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SharedImageRequiresAdapter => {
                formatter.write_str("shared-image preview requires an adapter token")
            }
            Self::EncodedLoopbackForbidsAdapter => {
                formatter.write_str("encoded-loopback preview must not carry an adapter token")
            }
        }
    }
}

impl std::error::Error for PreviewStreamDescriptorError {}

/// Importable image shape and interpretation for a shared preview lease.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PreviewImageDescriptor {
    dimensions: VideoDimensions,
    pixel_format: PixelFormat,
    metadata: VideoFrameMetadata,
}

impl PreviewImageDescriptor {
    /// Creates a descriptor restricted to portable shared-image formats.
    ///
    /// # Errors
    ///
    /// Returns a format error for `Yuv422`, or preserves the typed metadata
    /// validation error when color and alpha interpretation is incompatible.
    pub const fn new(
        dimensions: VideoDimensions,
        pixel_format: PixelFormat,
        metadata: VideoFrameMetadata,
    ) -> Result<Self, PreviewImageDescriptorError> {
        if matches!(pixel_format, PixelFormat::Yuv422) {
            return Err(PreviewImageDescriptorError::UnsupportedSharedImageFormat { pixel_format });
        }
        if let Err(error) = metadata.validate_for(pixel_format) {
            return Err(PreviewImageDescriptorError::Metadata(error));
        }
        Ok(Self {
            dimensions,
            pixel_format,
            metadata,
        })
    }

    #[must_use]
    pub const fn dimensions(self) -> VideoDimensions {
        self.dimensions
    }

    #[must_use]
    pub const fn pixel_format(self) -> PixelFormat {
        self.pixel_format
    }

    #[must_use]
    pub const fn metadata(self) -> VideoFrameMetadata {
        self.metadata
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewImageDescriptorError {
    UnsupportedSharedImageFormat { pixel_format: PixelFormat },
    Metadata(VideoFrameMetadataError),
}

impl fmt::Display for PreviewImageDescriptorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSharedImageFormat { pixel_format } => write!(
                formatter,
                "pixel format {pixel_format:?} is not supported by the shared-image preview contract"
            ),
            Self::Metadata(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for PreviewImageDescriptorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Metadata(error) => Some(error),
            Self::UnsupportedSharedImageFormat { .. } => None,
        }
    }
}

/// One holder-released shared-image resource registration.
///
/// This type owns its [`ResourceLease`] and is intentionally not cloneable. It
/// carries only opaque registrations and synchronization values, never native
/// OS or graphics API handles.
#[derive(Debug, Eq, PartialEq)]
pub struct LocalPreviewLease {
    stream_id: PreviewStreamId,
    lease_id: PreviewLeaseId,
    engine_instance_id: EngineInstanceId,
    runtime_generation: u64,
    adapter_token: PhysicalAdapterToken,
    handle_reference_id: OsHandleReferenceId,
    resource_lease: ResourceLease,
    frame_sequence: u64,
    resize_generation: u64,
    image: PreviewImageDescriptor,
}

impl LocalPreviewLease {
    /// Creates a synchronized shared-image lease for a stream descriptor.
    ///
    /// # Errors
    ///
    /// Rejects encoded streams, adapter disagreement, CPU memory, absent ready
    /// or release tokens, and release ownership not assigned to the holder.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stream: PreviewStreamDescriptor,
        lease_id: PreviewLeaseId,
        adapter_token: PhysicalAdapterToken,
        handle_reference_id: OsHandleReferenceId,
        resource_lease: ResourceLease,
        frame_sequence: u64,
        resize_generation: u64,
        image: PreviewImageDescriptor,
    ) -> Result<Self, LocalPreviewLeaseError> {
        if stream.transport != PreviewTransport::SharedImage {
            return Err(LocalPreviewLeaseError::SharedImageTransportRequired);
        }
        let Some(expected_adapter) = stream.adapter_token else {
            return Err(LocalPreviewLeaseError::SharedImageAdapterMissing);
        };
        if adapter_token != expected_adapter {
            return Err(LocalPreviewLeaseError::AdapterMismatch {
                expected: expected_adapter,
                actual: adapter_token,
            });
        }
        if resource_lease.memory_domain() == MemoryDomain::Cpu {
            return Err(LocalPreviewLeaseError::CpuMemoryNotShareable);
        }
        if resource_lease.ready_token().is_none() {
            return Err(LocalPreviewLeaseError::MissingReadyToken);
        }
        if resource_lease.release_token().is_none() {
            return Err(LocalPreviewLeaseError::MissingReleaseToken);
        }
        if resource_lease.release_owner().ownership() != ReleaseOwnership::LeaseHolderSignals {
            return Err(LocalPreviewLeaseError::HolderReleaseOwnershipRequired);
        }
        Ok(Self {
            stream_id: stream.stream_id,
            lease_id,
            engine_instance_id: stream.engine_instance_id,
            runtime_generation: stream.runtime_generation,
            adapter_token,
            handle_reference_id,
            resource_lease,
            frame_sequence,
            resize_generation,
            image,
        })
    }

    #[must_use]
    pub const fn stream_id(&self) -> PreviewStreamId {
        self.stream_id
    }

    #[must_use]
    pub const fn lease_id(&self) -> PreviewLeaseId {
        self.lease_id
    }

    #[must_use]
    pub const fn engine_instance_id(&self) -> EngineInstanceId {
        self.engine_instance_id
    }

    #[must_use]
    pub const fn runtime_generation(&self) -> u64 {
        self.runtime_generation
    }

    #[must_use]
    pub const fn adapter_token(&self) -> PhysicalAdapterToken {
        self.adapter_token
    }

    #[must_use]
    pub const fn handle_reference_id(&self) -> OsHandleReferenceId {
        self.handle_reference_id
    }

    #[must_use]
    pub const fn resource_lease(&self) -> &ResourceLease {
        &self.resource_lease
    }

    #[must_use]
    pub const fn ready_token(&self) -> SynchronizationToken {
        match self.resource_lease.ready_token() {
            Some(token) => token,
            None => unreachable!(),
        }
    }

    #[must_use]
    pub const fn release_token(&self) -> SynchronizationToken {
        match self.resource_lease.release_token() {
            Some(token) => token,
            None => unreachable!(),
        }
    }

    #[must_use]
    pub const fn frame_sequence(&self) -> u64 {
        self.frame_sequence
    }

    #[must_use]
    pub const fn resize_generation(&self) -> u64 {
        self.resize_generation
    }

    #[must_use]
    pub const fn image(&self) -> PreviewImageDescriptor {
        self.image
    }

    #[must_use]
    pub fn into_resource_lease(self) -> ResourceLease {
        self.resource_lease
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalPreviewLeaseError {
    SharedImageTransportRequired,
    SharedImageAdapterMissing,
    AdapterMismatch {
        expected: PhysicalAdapterToken,
        actual: PhysicalAdapterToken,
    },
    CpuMemoryNotShareable,
    MissingReadyToken,
    MissingReleaseToken,
    HolderReleaseOwnershipRequired,
}

impl fmt::Display for LocalPreviewLeaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SharedImageTransportRequired => {
                formatter.write_str("local preview lease requires shared-image transport")
            }
            Self::SharedImageAdapterMissing => {
                formatter.write_str("shared-image stream has no adapter token")
            }
            Self::AdapterMismatch { expected, actual } => write!(
                formatter,
                "preview lease adapter token {actual} does not match stream adapter token {expected}"
            ),
            Self::CpuMemoryNotShareable => {
                formatter.write_str("CPU memory cannot back a shared-image preview lease")
            }
            Self::MissingReadyToken => {
                formatter.write_str("shared-image preview lease requires a ready token")
            }
            Self::MissingReleaseToken => {
                formatter.write_str("shared-image preview lease requires a release token")
            }
            Self::HolderReleaseOwnershipRequired => formatter
                .write_str("shared-image preview lease requires holder-signaled release ownership"),
        }
    }
}

impl std::error::Error for LocalPreviewLeaseError {}

/// Consumer acknowledgement for one expected holder-signaled release value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PreviewReleaseAck {
    stream_id: PreviewStreamId,
    lease_id: PreviewLeaseId,
    engine_instance_id: EngineInstanceId,
    runtime_generation: u64,
    release_token: SynchronizationToken,
}

impl PreviewReleaseAck {
    #[must_use]
    pub const fn new(
        stream_id: PreviewStreamId,
        lease_id: PreviewLeaseId,
        engine_instance_id: EngineInstanceId,
        runtime_generation: u64,
        release_token: SynchronizationToken,
    ) -> Self {
        Self {
            stream_id,
            lease_id,
            engine_instance_id,
            runtime_generation,
            release_token,
        }
    }

    #[must_use]
    pub const fn for_lease(lease: &LocalPreviewLease) -> Self {
        Self::new(
            lease.stream_id,
            lease.lease_id,
            lease.engine_instance_id,
            lease.runtime_generation,
            lease.release_token(),
        )
    }

    #[must_use]
    pub const fn stream_id(self) -> PreviewStreamId {
        self.stream_id
    }

    #[must_use]
    pub const fn lease_id(self) -> PreviewLeaseId {
        self.lease_id
    }

    #[must_use]
    pub const fn engine_instance_id(self) -> EngineInstanceId {
        self.engine_instance_id
    }

    #[must_use]
    pub const fn runtime_generation(self) -> u64 {
        self.runtime_generation
    }

    #[must_use]
    pub const fn release_token(self) -> SynchronizationToken {
        self.release_token
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OutstandingPreviewLease {
    lease_id: PreviewLeaseId,
    handle_reference_id: OsHandleReferenceId,
    resource_id: ResourceId,
    release_token: SynchronizationToken,
    deadline_ms: u64,
}

/// Bounded producer-side lifecycle state for one shared-image preview stream.
///
/// The registry tracks only compact identities, expected release values, and
/// deadlines. It does not retain or clone [`ResourceLease`] values.
#[derive(Debug)]
pub struct PreviewLeaseRegistry {
    stream: PreviewStreamDescriptor,
    maximum_outstanding: usize,
    timeout_ms: u64,
    outstanding: Vec<OutstandingPreviewLease>,
    last_lease_id: Option<PreviewLeaseId>,
    last_frame_sequence: Option<u64>,
    last_resize_generation: Option<u64>,
    last_image: Option<PreviewImageDescriptor>,
}

impl PreviewLeaseRegistry {
    pub const HARD_MAXIMUM_OUTSTANDING: usize = 16;

    /// Creates bounded state for exactly one shared-image stream.
    ///
    /// # Errors
    ///
    /// Rejects encoded streams, zero or excessive capacity, and zero timeout.
    pub fn new(
        stream: PreviewStreamDescriptor,
        maximum_outstanding: usize,
        timeout_ms: u64,
    ) -> Result<Self, PreviewLeaseRegistryError> {
        if stream.transport != PreviewTransport::SharedImage {
            return Err(PreviewLeaseRegistryError::SharedImageTransportRequired);
        }
        if maximum_outstanding == 0 {
            return Err(PreviewLeaseRegistryError::ZeroCapacity);
        }
        if maximum_outstanding > Self::HARD_MAXIMUM_OUTSTANDING {
            return Err(PreviewLeaseRegistryError::CapacityAboveHardMaximum {
                actual: maximum_outstanding,
                maximum: Self::HARD_MAXIMUM_OUTSTANDING,
            });
        }
        if timeout_ms == 0 {
            return Err(PreviewLeaseRegistryError::ZeroTimeout);
        }
        Ok(Self {
            stream,
            maximum_outstanding,
            timeout_ms,
            outstanding: Vec::with_capacity(maximum_outstanding),
            last_lease_id: None,
            last_frame_sequence: None,
            last_resize_generation: None,
            last_image: None,
        })
    }

    /// Publishes one validated lease into bounded outstanding state.
    ///
    /// All identity, ordering, deadline, and capacity checks complete before
    /// any registry state is changed.
    ///
    /// # Errors
    ///
    /// Returns a precise mismatch, ordering, overflow, or capacity error.
    pub fn issue(
        &mut self,
        lease: &LocalPreviewLease,
        monotonic_now_ms: u64,
    ) -> Result<(), PreviewLeaseRegistryError> {
        if lease.stream_id != self.stream.stream_id {
            return Err(PreviewLeaseRegistryError::StreamMismatch {
                expected: self.stream.stream_id,
                actual: lease.stream_id,
            });
        }
        if lease.engine_instance_id != self.stream.engine_instance_id {
            return Err(PreviewLeaseRegistryError::EngineInstanceMismatch {
                expected: self.stream.engine_instance_id,
                actual: lease.engine_instance_id,
            });
        }
        if lease.runtime_generation != self.stream.runtime_generation {
            return Err(PreviewLeaseRegistryError::RuntimeGenerationMismatch {
                expected: self.stream.runtime_generation,
                actual: lease.runtime_generation,
            });
        }
        let Some(expected_adapter) = self.stream.adapter_token else {
            return Err(PreviewLeaseRegistryError::SharedImageAdapterMissing);
        };
        if lease.adapter_token != expected_adapter {
            return Err(PreviewLeaseRegistryError::AdapterMismatch {
                expected: expected_adapter,
                actual: lease.adapter_token,
            });
        }
        if let Some(previous) = self.last_lease_id
            && lease.lease_id <= previous
        {
            return Err(PreviewLeaseRegistryError::LeaseIdNotIncreasing {
                previous,
                actual: lease.lease_id,
            });
        }
        if let Some(previous) = self.last_frame_sequence
            && lease.frame_sequence <= previous
        {
            return Err(PreviewLeaseRegistryError::FrameSequenceNotIncreasing {
                previous,
                actual: lease.frame_sequence,
            });
        }
        if let Some(previous) = self.last_resize_generation {
            let valid = lease.resize_generation == previous
                || previous
                    .checked_add(1)
                    .is_some_and(|next| lease.resize_generation == next);
            if !valid {
                return Err(PreviewLeaseRegistryError::ResizeGenerationOutOfOrder {
                    previous,
                    actual: lease.resize_generation,
                });
            }
            if lease.resize_generation == previous && self.last_image != Some(lease.image) {
                return Err(
                    PreviewLeaseRegistryError::ImageChangedWithoutResizeGeneration {
                        resize_generation: previous,
                    },
                );
            }
        }
        let resource_id = self.validate_resource_available(lease)?;
        let deadline_ms = monotonic_now_ms.checked_add(self.timeout_ms).ok_or(
            PreviewLeaseRegistryError::TimeoutDeadlineOverflow {
                monotonic_now_ms,
                timeout_ms: self.timeout_ms,
            },
        )?;
        if self.outstanding.len() >= self.maximum_outstanding {
            return Err(PreviewLeaseRegistryError::CapacityExhausted {
                maximum: self.maximum_outstanding,
            });
        }

        self.outstanding.push(OutstandingPreviewLease {
            lease_id: lease.lease_id,
            handle_reference_id: lease.handle_reference_id,
            resource_id,
            release_token: lease.release_token(),
            deadline_ms,
        });
        self.last_lease_id = Some(lease.lease_id);
        self.last_frame_sequence = Some(lease.frame_sequence);
        self.last_resize_generation = Some(lease.resize_generation);
        self.last_image = Some(lease.image);
        Ok(())
    }

    fn validate_resource_available(
        &self,
        lease: &LocalPreviewLease,
    ) -> Result<ResourceId, PreviewLeaseRegistryError> {
        if let Some(record) = self
            .outstanding
            .iter()
            .find(|record| record.handle_reference_id == lease.handle_reference_id)
        {
            return Err(PreviewLeaseRegistryError::HandleReferenceStillOutstanding {
                handle_reference_id: lease.handle_reference_id,
                outstanding_lease_id: record.lease_id,
            });
        }
        let resource_id = lease.resource_lease.resource_id();
        if let Some(record) = self
            .outstanding
            .iter()
            .find(|record| record.resource_id == resource_id)
        {
            return Err(PreviewLeaseRegistryError::ResourceStillOutstanding {
                resource_id,
                outstanding_lease_id: record.lease_id,
            });
        }
        Ok(resource_id)
    }

    /// Removes exactly the acknowledged outstanding lease.
    ///
    /// Failed, stale, and duplicate acknowledgements leave state unchanged.
    ///
    /// # Errors
    ///
    /// Returns a typed identity, generation, unknown/stale lease, or release
    /// token mismatch without removing any outstanding lease.
    pub fn acknowledge_release(
        &mut self,
        acknowledgement: PreviewReleaseAck,
    ) -> Result<PreviewLeaseId, PreviewLeaseRegistryError> {
        if acknowledgement.stream_id != self.stream.stream_id {
            return Err(PreviewLeaseRegistryError::AcknowledgementStreamMismatch {
                expected: self.stream.stream_id,
                actual: acknowledgement.stream_id,
            });
        }
        if acknowledgement.engine_instance_id != self.stream.engine_instance_id {
            return Err(
                PreviewLeaseRegistryError::AcknowledgementEngineInstanceMismatch {
                    expected: self.stream.engine_instance_id,
                    actual: acknowledgement.engine_instance_id,
                },
            );
        }
        if acknowledgement.runtime_generation != self.stream.runtime_generation {
            return Err(
                PreviewLeaseRegistryError::AcknowledgementRuntimeGenerationMismatch {
                    expected: self.stream.runtime_generation,
                    actual: acknowledgement.runtime_generation,
                },
            );
        }
        let Some(position) = self
            .outstanding
            .iter()
            .position(|record| record.lease_id == acknowledgement.lease_id)
        else {
            return Err(PreviewLeaseRegistryError::UnknownOrStaleLease {
                lease_id: acknowledgement.lease_id,
            });
        };
        let expected_token = self.outstanding[position].release_token;
        if acknowledgement.release_token != expected_token {
            return Err(PreviewLeaseRegistryError::WrongReleaseToken {
                lease_id: acknowledgement.lease_id,
                expected: expected_token,
                actual: acknowledgement.release_token,
            });
        }
        self.outstanding.remove(position);
        Ok(acknowledgement.lease_id)
    }

    /// Reclaims leases whose deadlines are at or before the supplied time.
    ///
    /// Returned IDs are bounded by capacity and ordered by increasing lease ID.
    #[must_use]
    pub fn reclaim_expired(&mut self, monotonic_now_ms: u64) -> Vec<PreviewLeaseId> {
        let mut reclaimed = Vec::with_capacity(self.outstanding.len());
        self.outstanding.retain(|record| {
            if record.deadline_ms <= monotonic_now_ms {
                reclaimed.push(record.lease_id);
                false
            } else {
                true
            }
        });
        reclaimed
    }

    /// Reclaims every outstanding lease after client disconnection.
    ///
    /// Returned IDs are bounded by capacity and ordered by increasing lease ID.
    #[must_use]
    pub fn reclaim_all_on_disconnect(&mut self) -> Vec<PreviewLeaseId> {
        self.outstanding
            .drain(..)
            .map(|record| record.lease_id)
            .collect()
    }

    #[must_use]
    pub const fn stream_descriptor(&self) -> PreviewStreamDescriptor {
        self.stream
    }

    #[must_use]
    pub fn outstanding_count(&self) -> usize {
        self.outstanding.len()
    }

    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.maximum_outstanding
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewLeaseRegistryError {
    SharedImageTransportRequired,
    SharedImageAdapterMissing,
    ZeroCapacity,
    CapacityAboveHardMaximum {
        actual: usize,
        maximum: usize,
    },
    ZeroTimeout,
    StreamMismatch {
        expected: PreviewStreamId,
        actual: PreviewStreamId,
    },
    EngineInstanceMismatch {
        expected: EngineInstanceId,
        actual: EngineInstanceId,
    },
    RuntimeGenerationMismatch {
        expected: u64,
        actual: u64,
    },
    AdapterMismatch {
        expected: PhysicalAdapterToken,
        actual: PhysicalAdapterToken,
    },
    HandleReferenceStillOutstanding {
        handle_reference_id: OsHandleReferenceId,
        outstanding_lease_id: PreviewLeaseId,
    },
    ResourceStillOutstanding {
        resource_id: ResourceId,
        outstanding_lease_id: PreviewLeaseId,
    },
    LeaseIdNotIncreasing {
        previous: PreviewLeaseId,
        actual: PreviewLeaseId,
    },
    FrameSequenceNotIncreasing {
        previous: u64,
        actual: u64,
    },
    ResizeGenerationOutOfOrder {
        previous: u64,
        actual: u64,
    },
    ImageChangedWithoutResizeGeneration {
        resize_generation: u64,
    },
    TimeoutDeadlineOverflow {
        monotonic_now_ms: u64,
        timeout_ms: u64,
    },
    CapacityExhausted {
        maximum: usize,
    },
    AcknowledgementStreamMismatch {
        expected: PreviewStreamId,
        actual: PreviewStreamId,
    },
    AcknowledgementEngineInstanceMismatch {
        expected: EngineInstanceId,
        actual: EngineInstanceId,
    },
    AcknowledgementRuntimeGenerationMismatch {
        expected: u64,
        actual: u64,
    },
    UnknownOrStaleLease {
        lease_id: PreviewLeaseId,
    },
    WrongReleaseToken {
        lease_id: PreviewLeaseId,
        expected: SynchronizationToken,
        actual: SynchronizationToken,
    },
}

impl fmt::Display for PreviewLeaseRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SharedImageTransportRequired => {
                formatter.write_str("preview lease registry requires shared-image transport")
            }
            Self::SharedImageAdapterMissing => {
                formatter.write_str("shared-image registry stream has no adapter token")
            }
            Self::ZeroCapacity => {
                formatter.write_str("preview lease registry capacity must be nonzero")
            }
            Self::CapacityAboveHardMaximum { actual, maximum } => write!(
                formatter,
                "preview lease registry capacity {actual} exceeds hard maximum {maximum}"
            ),
            Self::ZeroTimeout => {
                formatter.write_str("preview lease registry timeout must be nonzero")
            }
            Self::StreamMismatch { expected, actual } => write!(
                formatter,
                "preview lease stream {actual} does not match registry stream {expected}"
            ),
            Self::EngineInstanceMismatch { expected, actual } => write!(
                formatter,
                "preview lease engine instance {actual} does not match registry engine instance {expected}"
            ),
            Self::RuntimeGenerationMismatch { expected, actual } => write!(
                formatter,
                "preview lease runtime generation {actual} does not match registry generation {expected}"
            ),
            Self::AdapterMismatch { expected, actual } => write!(
                formatter,
                "preview lease adapter token {actual} does not match registry adapter token {expected}"
            ),
            Self::HandleReferenceStillOutstanding {
                handle_reference_id,
                outstanding_lease_id,
            } => write!(
                formatter,
                "preview handle reference {handle_reference_id} is still owned by lease {outstanding_lease_id}"
            ),
            Self::ResourceStillOutstanding {
                resource_id,
                outstanding_lease_id,
            } => write!(
                formatter,
                "preview resource {resource_id} is still owned by lease {outstanding_lease_id}"
            ),
            Self::LeaseIdNotIncreasing { previous, actual } => write!(
                formatter,
                "preview lease ID {actual} is not greater than previous ID {previous}"
            ),
            Self::FrameSequenceNotIncreasing { previous, actual } => write!(
                formatter,
                "preview frame sequence {actual} is not greater than previous sequence {previous}"
            ),
            Self::ResizeGenerationOutOfOrder { previous, actual } => write!(
                formatter,
                "preview resize generation {actual} must equal {previous} or advance it by one"
            ),
            Self::ImageChangedWithoutResizeGeneration { resize_generation } => write!(
                formatter,
                "preview image descriptor changed without advancing resize generation {resize_generation}"
            ),
            Self::TimeoutDeadlineOverflow {
                monotonic_now_ms,
                timeout_ms,
            } => write!(
                formatter,
                "preview deadline overflows for monotonic time {monotonic_now_ms} plus timeout {timeout_ms}"
            ),
            Self::CapacityExhausted { maximum } => write!(
                formatter,
                "preview lease registry has reached capacity {maximum}"
            ),
            Self::AcknowledgementStreamMismatch { expected, actual } => write!(
                formatter,
                "release acknowledgement stream {actual} does not match registry stream {expected}"
            ),
            Self::AcknowledgementEngineInstanceMismatch { expected, actual } => write!(
                formatter,
                "release acknowledgement engine instance {actual} does not match registry engine instance {expected}"
            ),
            Self::AcknowledgementRuntimeGenerationMismatch { expected, actual } => write!(
                formatter,
                "release acknowledgement runtime generation {actual} does not match registry generation {expected}"
            ),
            Self::UnknownOrStaleLease { lease_id } => write!(
                formatter,
                "release acknowledgement refers to unknown or stale lease {lease_id}"
            ),
            Self::WrongReleaseToken {
                lease_id,
                expected,
                actual,
            } => write!(
                formatter,
                "release acknowledgement for lease {lease_id} has token {actual:?}, expected {expected:?}"
            ),
        }
    }
}

impl std::error::Error for PreviewLeaseRegistryError {}
