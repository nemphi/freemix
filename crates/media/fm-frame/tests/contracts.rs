use std::num::{NonZeroU32, NonZeroU64, NonZeroU128};

use fm_frame::{
    AlphaMode, AudioBlock, AudioBlockError, BridgeId, ChannelLayout, ClockDomainId,
    CodecConfigGeneration, CodecId, ColorMetadata, CpuVideoFrame, CpuVideoPayload, CpuVideoPlane,
    EncodedPacket, EncodedPacketError, EncodedPacketMetadata, MediaFlags, MediaTimestamp,
    MediaTiming, MemoryDomain, NormalizedDuration, NormalizedTimestamp, OriginalTimestamp,
    PacketFlags, PixelFormat, ReleaseOwner, ReleaseOwnerId, ReleaseOwnership, ResourceId,
    ResourceLease, SampleRate, SequenceNumber, StreamId, SynchronizationId, SynchronizationToken,
    TimeBase, Timecode, TimingError, VideoDimensions, VideoFrameMetadata, VideoFrameMetadataError,
    VideoPayloadError,
};
use fm_types::{ChromaLocation, ColorPrimaries, MatrixCoefficients, SignalRange, TransferFunction};

fn id(value: u128) -> NonZeroU128 {
    NonZeroU128::new(value).unwrap()
}

fn timing() -> MediaTiming {
    let time_base = TimeBase::new(1, 90_000).unwrap();
    MediaTiming::new(
        OriginalTimestamp::new(MediaTimestamp::new(180_000), time_base),
        NormalizedTimestamp::from_nanos(2_000_000_000),
        NormalizedDuration::from_nanos(33_366_667).unwrap(),
        ClockDomainId::new(id(1)),
        SequenceNumber::new(42),
    )
    .unwrap()
}

#[test]
fn timing_preserves_source_and_normalized_domains() {
    let source = OriginalTimestamp::new(
        MediaTimestamp::new(180_000),
        TimeBase::new(1, 90_000).unwrap(),
    );
    assert_eq!(source.normalize().unwrap().as_nanos(), 2_000_000_000);

    let capture =
        OriginalTimestamp::new(MediaTimestamp::new(1_000), TimeBase::new(1, 1_000).unwrap());
    let timecode = Timecode::new(1, 2, 3, 4, 30).unwrap();
    let timing = timing()
        .with_flags(MediaFlags::DISCONTINUITY | MediaFlags::CORRUPTED)
        .with_capture_timestamp(capture)
        .with_timecode(timecode);

    assert_eq!(timing.original_timestamp(), source);
    assert_eq!(timing.capture_timestamp(), Some(capture));
    assert_eq!(timing.timecode(), Some(timecode));
    assert_eq!(timing.sequence().get(), 42);
    assert!(timing.flags().contains(MediaFlags::DISCONTINUITY));
    assert!(timing.flags().contains(MediaFlags::CORRUPTED));
    assert_eq!(MediaFlags::from_bits(0x80).unwrap_err().bits(), 0x80);
}

#[test]
fn timing_rejects_overflow_and_empty_duration() {
    assert_eq!(
        NormalizedDuration::from_nanos(0),
        Err(TimingError::ZeroDuration)
    );
    assert_eq!(
        OriginalTimestamp::new(
            MediaTimestamp::new(i64::MAX),
            TimeBase::new(u32::MAX, 1).unwrap(),
        )
        .normalize(),
        Err(TimingError::NormalizationOverflow)
    );
    assert_eq!(
        MediaTiming::new(
            OriginalTimestamp::new(MediaTimestamp::new(0), TimeBase::new(1, 1_000).unwrap(),),
            NormalizedTimestamp::from_nanos(i64::MAX),
            NormalizedDuration::from_nanos(1).unwrap(),
            ClockDomainId::new(id(1)),
            SequenceNumber::new(0),
        ),
        Err(TimingError::PresentationEndOverflow)
    );
}

#[test]
fn cpu_video_validates_format_stride_and_allocation_limits() {
    let dimensions = VideoDimensions::new(2, 2).unwrap();
    let payload = CpuVideoPayload::new(
        PixelFormat::Rgba8,
        dimensions,
        vec![CpuVideoPlane::new(12, vec![0; 24]).unwrap()],
    )
    .unwrap();
    assert_eq!(payload.byte_len(), 24);
    assert_eq!(payload.plane(0).unwrap().stride(), 12);

    assert_eq!(
        CpuVideoPayload::new(
            PixelFormat::Rgba8,
            dimensions,
            vec![CpuVideoPlane::new(7, vec![0; 14]).unwrap()],
        ),
        Err(VideoPayloadError::StrideTooSmall {
            plane: 0,
            minimum: 8,
            actual: 7,
        })
    );
    assert_eq!(
        CpuVideoPayload::allocate(
            PixelFormat::Rgba16Float,
            VideoDimensions::new(CpuVideoPayload::MAX_WIDTH, CpuVideoPayload::MAX_HEIGHT,).unwrap(),
        ),
        Err(VideoPayloadError::PayloadTooLarge {
            required: 2_147_483_648,
            maximum: CpuVideoPayload::MAX_TOTAL_BYTES,
        })
    );
    assert_eq!(
        CpuVideoPayload::allocate(PixelFormat::Nv12, VideoDimensions::new(3, 2).unwrap()),
        Err(VideoPayloadError::SubsampledDimensionsMustBeEven)
    );
}

#[test]
fn cpu_video_metadata_is_unresolved_until_valid_attachment() {
    let dimensions = VideoDimensions::new(1, 1).unwrap();
    let payload = CpuVideoPayload::allocate(PixelFormat::Rgba8, dimensions).unwrap();
    let unannotated = CpuVideoFrame::new(timing(), payload.clone());
    assert_eq!(unannotated.metadata(), None);

    let color = ColorMetadata {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferFunction::Srgb,
        matrix: MatrixCoefficients::Identity,
        range: SignalRange::Full,
        chroma_location: ChromaLocation::Center,
    };
    let metadata = VideoFrameMetadata::new(color, Some(AlphaMode::Straight));
    let resolved = CpuVideoFrame::new(timing(), payload.clone())
        .with_metadata(metadata)
        .unwrap();
    assert_eq!(resolved.metadata(), Some(metadata));
    assert_eq!(resolved.payload(), &payload);

    let invalid = VideoFrameMetadata::new(color, None);
    assert_eq!(
        CpuVideoFrame::new(timing(), payload).with_metadata(invalid),
        Err(VideoFrameMetadataError::RgbAlphaModeRequired {
            pixel_format: PixelFormat::Rgba8,
        })
    );
}

#[test]
fn audio_is_planar_and_allocation_is_bounded() {
    let rate = SampleRate::new(48_000).unwrap();
    let block = AudioBlock::new(
        timing(),
        rate,
        ChannelLayout::stereo(),
        vec![vec![0.1, 0.2, 0.3], vec![0.4, 0.5, 0.6]],
    )
    .unwrap();
    assert_eq!(block.sample_count(), 3);
    assert_eq!(block.plane(0), Some([0.1, 0.2, 0.3].as_slice()));
    assert_eq!(block.sample(1, 2), Some(0.6));
    assert_eq!(block.sample(2, 0), None);

    assert_eq!(
        AudioBlock::new(
            timing(),
            rate,
            ChannelLayout::stereo(),
            vec![vec![0.0; 2], vec![0.0; 3]],
        ),
        Err(AudioBlockError::PlaneLengthMismatch {
            plane: 1,
            expected: 2,
            actual: 3,
        })
    );
    assert_eq!(
        AudioBlock::silence(timing(), rate, ChannelLayout::stereo(), usize::MAX,),
        Err(AudioBlockError::TooManySamples {
            actual: usize::MAX,
            maximum: AudioBlock::MAX_SAMPLES_PER_CHANNEL,
        })
    );
}

#[test]
fn resource_lease_preserves_identity_and_release_ownership() {
    let ready = SynchronizationToken::new(SynchronizationId::new(id(30)), 7);
    let release = SynchronizationToken::new(SynchronizationId::new(id(30)), 8);
    let owner = ReleaseOwner::new(
        ReleaseOwnerId::new(id(40)),
        ReleaseOwnership::LeaseHolderSignals,
    );
    let lease = ResourceLease::new(
        BridgeId::new(id(10)),
        ResourceId::new(id(20)),
        MemoryDomain::Vulkan,
        Some(ready),
        Some(release),
        owner,
    )
    .unwrap();

    assert_eq!(lease.bridge_id().get(), id(10));
    assert_eq!(lease.resource_id().get(), id(20));
    assert_eq!(lease.memory_domain(), MemoryDomain::Vulkan);
    assert_eq!(lease.ready_token(), Some(ready));
    assert_eq!(lease.release_token(), Some(release));
    assert_eq!(lease.release_owner(), owner);
    assert_eq!(
        ResourceLease::new(
            BridgeId::new(id(10)),
            ResourceId::new(id(20)),
            MemoryDomain::Metal,
            None,
            None,
            owner,
        )
        .unwrap_err()
        .to_string(),
        "lease holder release requires a synchronization token"
    );
}

#[test]
fn encoded_packet_keeps_distinct_pts_and_dts() {
    let time_base = TimeBase::new(1, 90_000).unwrap();
    let metadata = EncodedPacketMetadata::new(
        CodecId::new("video/h264").unwrap(),
        CodecConfigGeneration::new(NonZeroU64::new(3).unwrap()),
        StreamId::new(NonZeroU32::new(1).unwrap()),
        None,
        timing(),
        OriginalTimestamp::new(MediaTimestamp::new(177_000), time_base),
        PacketFlags::RANDOM_ACCESS,
    )
    .unwrap();
    let packet = EncodedPacket::from_bytes(metadata, vec![0, 0, 1, 0x65]).unwrap();

    assert_eq!(packet.metadata().codec().as_str(), "video/h264");
    assert_eq!(
        packet
            .metadata()
            .timing()
            .original_timestamp()
            .timestamp()
            .ticks(),
        180_000
    );
    assert_eq!(
        packet.metadata().decode_timestamp().timestamp().ticks(),
        177_000
    );
    assert!(
        packet
            .metadata()
            .flags()
            .contains(PacketFlags::RANDOM_ACCESS)
    );
    assert_eq!(
        EncodedPacket::from_bytes(packet.metadata().clone(), Vec::new()),
        Err(EncodedPacketError::EmptyPayload)
    );
    assert_eq!(
        CodecId::new("video h264"),
        Err(EncodedPacketError::InvalidCodecId)
    );
}
