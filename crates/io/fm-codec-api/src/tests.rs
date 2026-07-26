use std::num::{NonZeroU32, NonZeroU64, NonZeroU128};

use fm_capabilities::{ProviderVersion, StableId};
use fm_frame::{
    ClockDomainId, CodecConfigGeneration, CpuVideoPayload, EncodedPacketMetadata, EncodedPayload,
    MediaTimestamp, MediaTiming, NormalizedDuration, NormalizedTimestamp, OriginalTimestamp,
    PacketFlags, PixelFormat, SequenceNumber, TimeBase, VideoDimensions,
};

use super::*;

fn queue_capacity(value: usize) -> QueueCapacity {
    QueueCapacity::new(value).unwrap()
}

fn time_base() -> TimeBase {
    TimeBase::new(1, 1_000).unwrap()
}

fn video_format() -> DecodedFormat {
    DecodedFormat::Video(DecodedVideoFormat::new(
        PixelFormat::Rgba8,
        VideoDimensions::new(2, 2).unwrap(),
    ))
}

fn encoded_format(codec: KnownCodec) -> EncodedFormat {
    EncodedFormat::new(codec.codec_id(), MediaKind::Video, time_base())
}

fn timing(sequence: u64) -> MediaTiming {
    let ticks = i64::try_from(sequence).unwrap() * 40;
    MediaTiming::new(
        OriginalTimestamp::new(MediaTimestamp::new(ticks), time_base()),
        NormalizedTimestamp::from_nanos(ticks * 1_000_000),
        NormalizedDuration::from_nanos(40_000_000).unwrap(),
        ClockDomainId::new(NonZeroU128::new(1).unwrap()),
        SequenceNumber::new(sequence),
    )
    .unwrap()
}

fn packet(codec: KnownCodec, sequence: u64, byte: u8, keyframe: bool) -> EncodedPacket {
    let timing = timing(sequence);
    let metadata = EncodedPacketMetadata::new(
        codec.codec_id(),
        CodecConfigGeneration::new(NonZeroU64::new(1).unwrap()),
        StreamId::new(NonZeroU32::new(1).unwrap()),
        None,
        timing,
        timing.original_timestamp(),
        if keyframe {
            PacketFlags::RANDOM_ACCESS
        } else {
            PacketFlags::DEPENDS_ON_OTHERS
        },
    )
    .unwrap();
    EncodedPacket::from_bytes(metadata, vec![byte]).unwrap()
}

fn frame(sequence: u64) -> DecodedFrame {
    let payload =
        CpuVideoPayload::allocate(PixelFormat::Rgba8, VideoDimensions::new(2, 2).unwrap()).unwrap();
    DecodedFrame::Video(fm_frame::CpuVideoFrame::new(timing(sequence), payload))
}

fn provider() -> Provider {
    Provider::new(
        StableId::new("fake-codec").unwrap(),
        ProviderVersion::new("1").unwrap(),
    )
}

struct FakeDecoder {
    config: DecoderConfig,
    output: BoundedQueue<DecodedFrame>,
    state: CodecLifecycle,
    last_timestamp: Option<NormalizedTimestamp>,
}

impl FakeDecoder {
    fn new(config: DecoderConfig) -> Self {
        Self {
            output: BoundedQueue::new(config.queue_capacity()),
            config,
            state: CodecLifecycle::Ready,
            last_timestamp: None,
        }
    }
}

impl Decoder for FakeDecoder {
    fn config(&self) -> &DecoderConfig {
        &self.config
    }

    fn state(&self) -> CodecLifecycle {
        self.state
    }

    fn submit_packet(
        &mut self,
        packet: EncodedPacket,
    ) -> Result<SubmitStatus<EncodedPacket>, CodecError> {
        if self.state != CodecLifecycle::Ready {
            return Err(CodecError::new(CodecErrorKind::InvalidState {
                state: self.state,
                operation: Operation::Submit,
            }));
        }
        if packet.metadata().codec() != self.config.input().codec() {
            return Err(CodecError::new(CodecErrorKind::StreamMismatch));
        }
        if self.output.is_full() {
            return Ok(SubmitStatus::Backpressure(packet));
        }
        let timestamp = packet.metadata().timing().presentation_timestamp();
        if self.last_timestamp.is_some_and(|last| timestamp < last) {
            return Err(CodecError::new(CodecErrorKind::TimestampRegression));
        }
        self.last_timestamp = Some(timestamp);
        self.output
            .push(frame(packet.metadata().timing().sequence().get()))
            .expect("capacity checked");
        Ok(SubmitStatus::Accepted)
    }

    fn receive_frame(&mut self) -> Result<OutputStatus<DecodedFrame>, CodecError> {
        if let Some(frame) = self.output.pop() {
            return Ok(OutputStatus::Output(frame));
        }
        if self.state == CodecLifecycle::Draining {
            self.state = CodecLifecycle::Ended;
        }
        if self.state == CodecLifecycle::Ended {
            Ok(OutputStatus::End)
        } else {
            Ok(OutputStatus::NeedInput)
        }
    }

    fn drain(&mut self) -> Result<(), CodecError> {
        if self.state != CodecLifecycle::Ready {
            return Err(CodecError::new(CodecErrorKind::InvalidState {
                state: self.state,
                operation: Operation::Drain,
            }));
        }
        self.state = CodecLifecycle::Draining;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), CodecError> {
        self.output.clear();
        self.last_timestamp = None;
        self.state = CodecLifecycle::Ready;
        Ok(())
    }

    fn reconfigure(&mut self, _config: DecoderConfig) -> Result<(), CodecError> {
        Err(CodecError::new(CodecErrorKind::ReconfigureRejected))
    }
}

struct FakeEncoder {
    config: EncoderConfig,
    output: BoundedQueue<EncodedPacket>,
    state: CodecLifecycle,
    last_timestamp: Option<NormalizedTimestamp>,
    keyframe_requested: bool,
}

impl FakeEncoder {
    fn new(config: EncoderConfig) -> Self {
        Self {
            output: BoundedQueue::new(config.queue_capacity()),
            config,
            state: CodecLifecycle::Ready,
            last_timestamp: None,
            keyframe_requested: false,
        }
    }
}

impl Encoder for FakeEncoder {
    fn config(&self) -> &EncoderConfig {
        &self.config
    }

    fn state(&self) -> CodecLifecycle {
        self.state
    }

    fn submit_frame(
        &mut self,
        frame: DecodedFrame,
    ) -> Result<SubmitStatus<DecodedFrame>, CodecError> {
        if self.state != CodecLifecycle::Ready {
            return Err(CodecError::new(CodecErrorKind::InvalidState {
                state: self.state,
                operation: Operation::Submit,
            }));
        }
        if frame.media_kind() != self.config.input().media_kind() {
            return Err(CapabilityMismatch::MediaKind.into());
        }
        if self.output.is_full() {
            return Ok(SubmitStatus::Backpressure(frame));
        }
        let timestamp = frame.timing().presentation_timestamp();
        if self.last_timestamp.is_some_and(|last| timestamp < last) {
            return Err(CodecError::new(CodecErrorKind::TimestampRegression));
        }
        self.last_timestamp = Some(timestamp);
        let sequence = frame.timing().sequence().get();
        let encoded = packet(
            KnownCodec::H264,
            sequence,
            u8::try_from(sequence).unwrap(),
            std::mem::take(&mut self.keyframe_requested),
        );
        self.output.push(encoded).expect("capacity checked");
        Ok(SubmitStatus::Accepted)
    }

    fn receive_packet(&mut self) -> Result<OutputStatus<EncodedPacket>, CodecError> {
        if let Some(packet) = self.output.pop() {
            return Ok(OutputStatus::Output(packet));
        }
        if self.state == CodecLifecycle::Draining {
            self.state = CodecLifecycle::Ended;
        }
        if self.state == CodecLifecycle::Ended {
            Ok(OutputStatus::End)
        } else {
            Ok(OutputStatus::NeedInput)
        }
    }

    fn drain(&mut self) -> Result<(), CodecError> {
        if self.state != CodecLifecycle::Ready {
            return Err(CodecError::new(CodecErrorKind::InvalidState {
                state: self.state,
                operation: Operation::Drain,
            }));
        }
        self.state = CodecLifecycle::Draining;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), CodecError> {
        self.output.clear();
        self.last_timestamp = None;
        self.keyframe_requested = false;
        self.state = CodecLifecycle::Ready;
        Ok(())
    }

    fn request_keyframe(&mut self, _request: KeyframeRequest) -> Result<(), CodecError> {
        if self.state != CodecLifecycle::Ready {
            return Err(CodecError::new(CodecErrorKind::InvalidState {
                state: self.state,
                operation: Operation::RequestKeyframe,
            }));
        }
        self.keyframe_requested = true;
        Ok(())
    }

    fn reconfigure(&mut self, _config: EncoderConfig) -> Result<(), CodecError> {
        Err(CodecError::new(CodecErrorKind::ReconfigureRejected))
    }
}

struct FakeCodecProvider {
    capabilities: CodecCapabilities,
}

impl FakeCodecProvider {
    fn new() -> Self {
        let capacity = queue_capacity(2);
        Self {
            capabilities: CodecCapabilities::new(provider())
                .with_decoder(DecoderCapability::new(
                    KnownCodec::H264.codec_id(),
                    MediaKind::Video,
                    vec![video_format()],
                    capacity,
                ))
                .with_encoder(
                    EncoderCapability::new(
                        KnownCodec::H264.codec_id(),
                        MediaKind::Video,
                        vec![video_format()],
                        capacity,
                    )
                    .with_keyframe_requests(true),
                ),
        }
    }
}

impl CodecProvider for FakeCodecProvider {
    fn capabilities(&self) -> &CodecCapabilities {
        &self.capabilities
    }

    fn create_decoder(&self, config: DecoderConfig) -> Result<Box<dyn Decoder>, CodecError> {
        if !self.capabilities.supports_decoder(&config) {
            return Err(CapabilityMismatch::Codec.into());
        }
        Ok(Box::new(FakeDecoder::new(config)))
    }

    fn create_encoder(&self, config: EncoderConfig) -> Result<Box<dyn Encoder>, CodecError> {
        if !self.capabilities.supports_encoder(&config) {
            return Err(CapabilityMismatch::Codec.into());
        }
        Ok(Box::new(FakeEncoder::new(config)))
    }
}

fn decoder_conformance(decoder: &mut dyn Decoder) {
    assert_eq!(
        decoder.submit_packet(packet(KnownCodec::H264, 0, 0, true)),
        Ok(SubmitStatus::Accepted)
    );
    assert_eq!(
        decoder.submit_packet(packet(KnownCodec::H264, 1, 1, false)),
        Ok(SubmitStatus::Accepted)
    );
    let rejected = decoder
        .submit_packet(packet(KnownCodec::H264, 2, 2, false))
        .unwrap();
    let SubmitStatus::Backpressure(rejected) = rejected else {
        panic!("full decoder must apply backpressure");
    };
    assert_eq!(rejected.metadata().timing().sequence().get(), 2);

    for expected in [0, 1] {
        let OutputStatus::Output(frame) = decoder.receive_frame().unwrap() else {
            panic!("queued frame missing");
        };
        assert_eq!(frame.timing().sequence().get(), expected);
        assert_eq!(
            frame.timing().presentation_timestamp(),
            timing(expected).presentation_timestamp()
        );
    }
    assert_eq!(decoder.receive_frame().unwrap(), OutputStatus::NeedInput);

    decoder.submit_packet(rejected).unwrap();
    decoder.flush().unwrap();
    assert_eq!(decoder.receive_frame().unwrap(), OutputStatus::NeedInput);
    assert_eq!(decoder.state(), CodecLifecycle::Ready);

    decoder
        .submit_packet(packet(KnownCodec::H264, 0, 9, true))
        .unwrap();
    decoder.drain().unwrap();
    assert!(matches!(
        decoder.receive_frame().unwrap(),
        OutputStatus::Output(_)
    ));
    assert_eq!(decoder.receive_frame().unwrap(), OutputStatus::End);
    assert_eq!(decoder.state(), CodecLifecycle::Ended);
    assert_eq!(
        decoder
            .reconfigure(decoder.config().clone())
            .unwrap_err()
            .kind(),
        &CodecErrorKind::ReconfigureRejected
    );
}

fn encoder_conformance(encoder: &mut dyn Encoder) {
    encoder
        .request_keyframe(KeyframeRequest::NextFrame)
        .unwrap();
    encoder.submit_frame(frame(0)).unwrap();
    encoder.submit_frame(frame(1)).unwrap();
    let rejected = encoder.submit_frame(frame(2)).unwrap();
    let SubmitStatus::Backpressure(rejected) = rejected else {
        panic!("full encoder must apply backpressure");
    };
    assert_eq!(rejected.timing().sequence().get(), 2);

    let OutputStatus::Output(first) = encoder.receive_packet().unwrap() else {
        panic!("first packet missing");
    };
    assert!(
        first
            .metadata()
            .flags()
            .contains(PacketFlags::RANDOM_ACCESS)
    );
    assert_eq!(first.metadata().timing().sequence().get(), 0);
    let OutputStatus::Output(second) = encoder.receive_packet().unwrap() else {
        panic!("second packet missing");
    };
    assert_eq!(second.metadata().timing().sequence().get(), 1);
    assert_eq!(encoder.receive_packet().unwrap(), OutputStatus::NeedInput);

    encoder.submit_frame(rejected).unwrap();
    encoder.flush().unwrap();
    assert_eq!(encoder.receive_packet().unwrap(), OutputStatus::NeedInput);
    encoder.submit_frame(frame(0)).unwrap();
    encoder.drain().unwrap();
    assert!(matches!(
        encoder.receive_packet().unwrap(),
        OutputStatus::Output(_)
    ));
    assert_eq!(encoder.receive_packet().unwrap(), OutputStatus::End);
    assert_eq!(
        encoder
            .reconfigure(encoder.config().clone())
            .unwrap_err()
            .kind(),
        &CodecErrorKind::ReconfigureRejected
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FakeMuxState {
    Configuring,
    Started,
    Finishing,
    Ended,
}

struct FakeMuxer {
    config: MuxerConfig,
    streams: Vec<StreamDescriptor>,
    packets: BoundedQueue<EncodedPacket>,
    state: FakeMuxState,
    segment_number: u64,
    segment_start: Option<NormalizedTimestamp>,
    segment_end: Option<NormalizedTimestamp>,
    segment_packets: u64,
    segment_bytes: u64,
    recovery_needed: bool,
    finalization: Option<SegmentFinalization>,
}

impl FakeMuxer {
    fn new(config: MuxerConfig) -> Self {
        Self {
            packets: BoundedQueue::new(config.queue_capacity()),
            config,
            streams: Vec::new(),
            state: FakeMuxState::Configuring,
            segment_number: 1,
            segment_start: None,
            segment_end: None,
            segment_packets: 0,
            segment_bytes: 0,
            recovery_needed: false,
            finalization: None,
        }
    }

    fn segment_metadata(&mut self, finalization: SegmentFinalization) -> SegmentMetadata {
        let metadata = SegmentMetadata::new(
            SegmentNumber::new(NonZeroU64::new(self.segment_number).unwrap()),
            self.segment_start,
            self.segment_end,
            vec![SegmentPacketCount::new(
                StreamId::new(NonZeroU32::new(1).unwrap()),
                self.segment_packets,
            )],
            self.segment_bytes,
            true,
            finalization,
        );
        self.segment_number += 1;
        self.segment_start = None;
        self.segment_end = None;
        self.segment_packets = 0;
        self.segment_bytes = 0;
        metadata
    }
}

impl Muxer for FakeMuxer {
    fn config(&self) -> &MuxerConfig {
        &self.config
    }

    fn streams(&self) -> &[StreamDescriptor] {
        &self.streams
    }

    fn add_stream(&mut self, stream: StreamDescriptor) -> Result<(), MuxerError> {
        if self.state != FakeMuxState::Configuring {
            return Err(MuxerError::fatal(MuxerErrorKind::InvalidState));
        }
        if self
            .streams
            .iter()
            .any(|existing| existing.stream_id() == stream.stream_id())
        {
            return Err(MuxerError::fatal(MuxerErrorKind::DuplicateStream));
        }
        self.streams.push(stream);
        Ok(())
    }

    fn start(&mut self) -> Result<(), MuxerError> {
        if self.state != FakeMuxState::Configuring || self.streams.is_empty() {
            return Err(MuxerError::fatal(MuxerErrorKind::InvalidState));
        }
        self.state = FakeMuxState::Started;
        Ok(())
    }

    fn submit_packet(
        &mut self,
        packet: EncodedPacket,
    ) -> Result<SubmitStatus<EncodedPacket>, MuxerError> {
        if self.state != FakeMuxState::Started {
            return Err(MuxerError::fatal(MuxerErrorKind::InvalidState));
        }
        if !self
            .streams
            .iter()
            .any(|stream| stream.stream_id() == packet.metadata().stream_id())
        {
            return Err(MuxerError::fatal(MuxerErrorKind::UnknownStream));
        }
        match self.packets.push(packet) {
            Ok(()) => Ok(SubmitStatus::Accepted),
            Err(full) => Ok(SubmitStatus::Backpressure(full.into_inner())),
        }
    }

    fn poll(&mut self) -> Result<MuxerStatus, MuxerError> {
        if let Some(packet) = self.packets.pop() {
            let timestamp = packet.metadata().timing().presentation_timestamp();
            if self.segment_end.is_some_and(|end| timestamp < end) {
                return Err(MuxerError::new(
                    MuxerErrorKind::TimestampRegression,
                    MuxerRecovery::FinalizeCurrentSegment,
                ));
            }
            self.segment_start.get_or_insert(timestamp);
            self.segment_end = Some(timestamp);
            self.segment_packets += 1;
            let fail = match packet.payload() {
                EncodedPayload::Bytes(bytes) => {
                    self.segment_bytes += u64::try_from(bytes.len()).unwrap();
                    bytes.first() == Some(&0xee)
                }
                EncodedPayload::Resource(_) => false,
            };
            if fail {
                self.recovery_needed = true;
                return Err(MuxerError::new(
                    MuxerErrorKind::AdapterFailure {
                        code: 7,
                        message: "deterministic write fault".to_owned(),
                    },
                    MuxerRecovery::FinalizeCurrentSegment,
                ));
            }
            return Ok(MuxerStatus::NeedInput);
        }
        if let Some(finalization) = self.finalization.take() {
            return Ok(MuxerStatus::SegmentFinalized(
                self.segment_metadata(finalization),
            ));
        }
        if self.state == FakeMuxState::Finishing {
            self.state = FakeMuxState::Ended;
        }
        if self.state == FakeMuxState::Ended {
            Ok(MuxerStatus::End)
        } else {
            Ok(MuxerStatus::NeedInput)
        }
    }

    fn flush(&mut self) -> Result<(), MuxerError> {
        self.packets.clear();
        Ok(())
    }

    fn finalize_segment(&mut self) -> Result<(), MuxerError> {
        if self.state != FakeMuxState::Started || self.finalization.is_some() {
            return Err(MuxerError::fatal(MuxerErrorKind::InvalidState));
        }
        self.finalization = Some(if std::mem::take(&mut self.recovery_needed) {
            SegmentFinalization::RecoveredAfterError
        } else {
            SegmentFinalization::Complete
        });
        Ok(())
    }

    fn finish(&mut self) -> Result<(), MuxerError> {
        if self.state != FakeMuxState::Started {
            return Err(MuxerError::fatal(MuxerErrorKind::InvalidState));
        }
        self.state = FakeMuxState::Finishing;
        Ok(())
    }
}

fn muxer_conformance(muxer: &mut dyn Muxer) {
    muxer
        .add_stream(StreamDescriptor::new(
            StreamId::new(NonZeroU32::new(1).unwrap()),
            encoded_format(KnownCodec::H264),
        ))
        .unwrap();
    muxer.start().unwrap();
    muxer
        .submit_packet(packet(KnownCodec::H264, 0, 0, true))
        .unwrap();
    muxer
        .submit_packet(packet(KnownCodec::H264, 1, 1, false))
        .unwrap();
    let status = muxer
        .submit_packet(packet(KnownCodec::H264, 2, 2, false))
        .unwrap();
    assert!(matches!(status, SubmitStatus::Backpressure(_)));
    assert_eq!(muxer.poll().unwrap(), MuxerStatus::NeedInput);
    assert_eq!(muxer.poll().unwrap(), MuxerStatus::NeedInput);
    muxer.finalize_segment().unwrap();
    let MuxerStatus::SegmentFinalized(metadata) = muxer.poll().unwrap() else {
        panic!("segment metadata missing");
    };
    assert_eq!(metadata.number().get().get(), 1);
    assert_eq!(metadata.packet_counts()[0].packets(), 2);
    assert_eq!(metadata.bytes_written(), 2);
    assert_eq!(metadata.finalization(), SegmentFinalization::Complete);

    muxer
        .submit_packet(packet(KnownCodec::H264, 3, 0xee, true))
        .unwrap();
    let error = muxer.poll().unwrap_err();
    assert_eq!(error.recovery(), MuxerRecovery::FinalizeCurrentSegment);
    muxer.finalize_segment().unwrap();
    let MuxerStatus::SegmentFinalized(metadata) = muxer.poll().unwrap() else {
        panic!("recovered segment metadata missing");
    };
    assert_eq!(metadata.number().get().get(), 2);
    assert_eq!(
        metadata.finalization(),
        SegmentFinalization::RecoveredAfterError
    );
    assert_eq!(metadata.packet_counts()[0].packets(), 1);
    muxer.finish().unwrap();
    assert_eq!(muxer.poll().unwrap(), MuxerStatus::End);
}

#[test]
fn fake_decoder_satisfies_shared_conformance() {
    let provider = FakeCodecProvider::new();
    let config = DecoderConfig::new(
        encoded_format(KnownCodec::H264),
        video_format(),
        queue_capacity(2),
    )
    .unwrap();
    let mut decoder = provider.create_decoder(config).unwrap();
    decoder_conformance(decoder.as_mut());
}

#[test]
fn fake_encoder_satisfies_shared_conformance() {
    let provider = FakeCodecProvider::new();
    let config = EncoderConfig::new(
        video_format(),
        encoded_format(KnownCodec::H264),
        queue_capacity(2),
    )
    .unwrap();
    let mut encoder = provider.create_encoder(config).unwrap();
    encoder_conformance(encoder.as_mut());
}

#[test]
fn fake_muxer_satisfies_shared_conformance_and_recovers_segments() {
    let config = MuxerConfig::new(
        ContainerFormat::new("container/fake").unwrap(),
        SegmentMode::Segmented,
        queue_capacity(2),
    );
    muxer_conformance(&mut FakeMuxer::new(config));
}

#[test]
fn provider_rejects_capability_mismatch() {
    let provider = FakeCodecProvider::new();
    let config = DecoderConfig::new(
        encoded_format(KnownCodec::H265),
        video_format(),
        queue_capacity(2),
    )
    .unwrap();
    let Err(error) = provider.create_decoder(config) else {
        panic!("unsupported decoder was created");
    };
    assert_eq!(
        error.kind(),
        &CodecErrorKind::CapabilityMismatch(CapabilityMismatch::Codec)
    );
}

#[test]
fn bounded_queue_never_evicts_and_returns_unaccepted_input() {
    let mut queue = BoundedQueue::new(queue_capacity(1));
    queue.push(10).unwrap();
    assert_eq!(queue.push(20).unwrap_err().into_inner(), 20);
    assert_eq!(queue.pop(), Some(10));
    assert_eq!(QueueCapacity::new(0), Err(QueueCapacityError::Zero));
    assert!(matches!(
        QueueCapacity::new(QueueCapacity::MAX + 1),
        Err(QueueCapacityError::TooLarge { .. })
    ));
}

#[test]
fn format_descriptors_are_typed_and_bounded() {
    let profile = CodecProfile::new("high").unwrap();
    let level = CodecLevel::new("4.1").unwrap();
    let format = encoded_format(KnownCodec::H264)
        .with_profile(profile)
        .with_level(level)
        .with_codec_config(vec![1, 2, 3])
        .unwrap();
    assert_eq!(format.codec().as_str(), "video/h264");
    assert_eq!(format.profile().unwrap().as_str(), "high");
    assert_eq!(format.level().unwrap().as_str(), "4.1");
    assert_eq!(format.codec_config(), [1, 2, 3]);
}
