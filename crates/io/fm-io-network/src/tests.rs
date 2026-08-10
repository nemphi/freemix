use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::sync::Arc;

use fm_frame::{CodecId, NormalizedDuration, NormalizedTimestamp, TimeBase, VideoDimensions};

use super::*;

fn destination_id(value: u8) -> DestinationId {
    DestinationId::new(value).unwrap()
}

fn rendition_id(value: u32) -> RenditionId {
    RenditionId::new(NonZeroU32::new(value).unwrap())
}

fn duration(seconds: u64) -> NormalizedDuration {
    NormalizedDuration::from_nanos(seconds * 1_000_000_000).unwrap()
}

fn profile(width: u32, height: u32, video_bitrate: u64) -> RenditionProfile {
    let video = VideoRendition::new(
        CodecId::new("video/h264").unwrap(),
        "high",
        VideoDimensions::new(width, height).unwrap(),
        FrameRate::new(60_000, 1_001).unwrap(),
        ColorDescription::Rec709Limited,
        video_bitrate,
        120,
    )
    .unwrap()
    .with_codec_setting("b_frames", "2")
    .unwrap();
    let audio = AudioRendition::new(
        CodecId::new("audio/aac").unwrap(),
        "lc",
        128_000,
        48_000,
        2,
        vec![0, 1],
    )
    .unwrap();
    RenditionProfile::new(
        video,
        audio,
        TimingProfile::new(TimeBase::new(1, 90_000).unwrap(), 0),
    )
}

fn config(id: u8, capacity: usize, protocol: OutputProtocol) -> DestinationConfig {
    let tls = protocol
        .requires_tls()
        .then(|| TlsConfig::system_roots(Some("stream.example.test".to_owned())).unwrap());
    DestinationConfig::new(
        destination_id(id),
        protocol,
        Endpoint::new("stream.example.test", 1_935, "/live").unwrap(),
        Some(Endpoint::new("backup.example.test", 1_935, "/live").unwrap()),
        tls,
        Some(CredentialReference::new(format!("secret://stream-key/{id}")).unwrap()),
        QueueCapacity::new(capacity).unwrap(),
        ReconnectPolicy::new(100, 800, 2, Some(4)).unwrap(),
    )
    .unwrap()
}

fn packet(rendition: RenditionId, sequence: u64) -> OutputPacket {
    packet_with_random_access(rendition, sequence, sequence == 0, 1)
}

fn packet_with_random_access(
    rendition: RenditionId,
    sequence: u64,
    random_access: bool,
    payload_bytes: usize,
) -> OutputPacket {
    OutputPacket::new(
        rendition,
        sequence,
        NormalizedTimestamp::from_nanos(i64::try_from(sequence).unwrap() * 1_000_000),
        NormalizedDuration::from_nanos(1_000_000).unwrap(),
        random_access,
        Arc::<[u8]>::from(vec![
            u8::try_from(sequence).unwrap_or(u8::MAX);
            payload_bytes
        ]),
    )
    .unwrap()
    .with_encode_latency_ms(4)
}

#[derive(Default)]
struct FakeSink {
    connect_results: VecDeque<Result<ConnectionObservation, SinkError>>,
    write_results: VecDeque<Result<SinkWrite, SinkError>>,
    connected: bool,
    disconnects: u64,
    connection_hosts: Vec<String>,
    sequences: Vec<u64>,
    payloads: Vec<Arc<[u8]>>,
}

impl FakeSink {
    fn with_connect_results(
        results: impl IntoIterator<Item = Result<ConnectionObservation, SinkError>>,
    ) -> Self {
        Self {
            connect_results: results.into_iter().collect(),
            ..Self::default()
        }
    }

    fn with_write_results(results: impl IntoIterator<Item = Result<SinkWrite, SinkError>>) -> Self {
        Self {
            write_results: results.into_iter().collect(),
            ..Self::default()
        }
    }
}

impl TransportSink for FakeSink {
    fn connect(
        &mut self,
        _config: &DestinationConfig,
        endpoint: &Endpoint,
    ) -> Result<ConnectionObservation, SinkError> {
        self.connection_hosts.push(endpoint.host().to_owned());
        let result = self
            .connect_results
            .pop_front()
            .unwrap_or(Ok(ConnectionObservation {
                round_trip_time_ms: Some(12),
            }));
        self.connected = result.is_ok();
        result
    }

    fn write(&mut self, packet: &OutputPacket) -> Result<SinkWrite, SinkError> {
        assert!(self.connected);
        let result =
            self.write_results
                .pop_front()
                .unwrap_or(Ok(SinkWrite::Sent(SendObservation {
                    round_trip_time_ms: Some(14),
                    packet_loss_ppm: Some(0),
                    retransmitted_packets: 0,
                    bitrate_bps: Some(4_000_000),
                })));
        if matches!(result, Ok(SinkWrite::Sent(_))) {
            self.sequences.push(packet.sequence());
            self.payloads.push(packet.shared_payload());
        }
        result
    }

    fn disconnect(&mut self) {
        self.connected = false;
        self.disconnects += 1;
    }
}

#[test]
fn five_outputs_share_one_exact_rendition_and_payload() {
    let shared_profile = profile(1_920, 1_080, 6_000_000);
    let requests: Vec<_> = (1..=5)
        .map(|id| DestinationRenditions::single(destination_id(id), shared_profile.clone()))
        .collect();
    let plan = RenditionPlanner::plan(&requests).unwrap();
    assert_eq!(plan.renditions().len(), 1);
    assert_eq!(plan.renditions()[0].destinations().len(), 5);

    let mut outputs = OutputSet::new();
    let mut sinks: Vec<_> = (1..=5).map(|_| FakeSink::default()).collect();
    for id in 1..=5 {
        let protocol = match id {
            1 | 5 => OutputProtocol::Rtmp,
            2 => OutputProtocol::Rtmps,
            3 => OutputProtocol::Hls,
            _ => OutputProtocol::LiveLan,
        };
        outputs.add_destination(config(id, 2, protocol)).unwrap();
        outputs.start(destination_id(id)).unwrap();
        assert_eq!(
            outputs
                .poll(destination_id(id), 0, &mut sinks[usize::from(id - 1)])
                .unwrap(),
            PollEvent::Connected
        );
    }

    let encoded = packet(plan.renditions()[0].id(), 0);
    let original_payload = encoded.shared_payload();
    let outcomes = outputs.enqueue_rendition(&plan, &encoded).unwrap();
    assert_eq!(outcomes.len(), 5);
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome.status == EnqueueStatus::Accepted)
    );
    for id in 1..=5 {
        assert_eq!(
            outputs
                .poll(destination_id(id), 1, &mut sinks[usize::from(id - 1)])
                .unwrap(),
            PollEvent::PacketSent { sequence: 0 }
        );
        assert!(Arc::ptr_eq(
            &original_payload,
            &sinks[usize::from(id - 1)].payloads[0]
        ));
        assert_eq!(
            outputs
                .telemetry(destination_id(id))
                .unwrap()
                .packets_sent(),
            1
        );
    }
}

#[test]
fn rendition_planner_shares_exact_profiles_only() {
    let exact = profile(1_920, 1_080, 6_000_000);
    let different_bitrate = profile(1_920, 1_080, 6_000_001);
    let plan = RenditionPlanner::plan(&[
        DestinationRenditions::single(destination_id(1), exact.clone()),
        DestinationRenditions::single(destination_id(2), exact),
        DestinationRenditions::single(destination_id(3), different_bitrate),
    ])
    .unwrap();
    assert_eq!(plan.renditions().len(), 2);
    assert_eq!(
        plan.renditions()[0].destinations(),
        [destination_id(1), destination_id(2)]
    );
    assert_eq!(plan.renditions()[1].destinations(), [destination_id(3)]);
}

#[test]
fn reconnect_drops_interframes_and_resumes_at_queued_random_access() {
    let plan = RenditionPlanner::plan(&[
        DestinationRenditions::single(destination_id(1), profile(1_920, 1_080, 6_000_000)),
        DestinationRenditions::single(destination_id(2), profile(1_920, 1_080, 6_000_000)),
    ])
    .unwrap();
    let rendition = plan.renditions()[0].id();
    let mut outputs = OutputSet::new();
    outputs
        .add_destination(config(1, 4, OutputProtocol::Rtmp))
        .unwrap();
    outputs
        .add_destination(config(2, 2, OutputProtocol::Rtmp))
        .unwrap();
    outputs.start(destination_id(1)).unwrap();
    outputs.start(destination_id(2)).unwrap();

    let write_failure = SinkError::new(FailureStage::Write, Some(54), "reset", true);
    let mut sink = FakeSink::with_write_results([
        Err(write_failure),
        Ok(SinkWrite::Sent(SendObservation::default())),
    ]);
    let mut healthy = FakeSink::default();
    assert_eq!(
        outputs.poll(destination_id(1), 0, &mut sink).unwrap(),
        PollEvent::Connected
    );
    outputs.poll(destination_id(2), 0, &mut healthy).unwrap();
    outputs
        .enqueue_rendition(&plan, &packet(rendition, 1))
        .unwrap();

    assert_eq!(
        outputs.poll(destination_id(1), 10, &mut sink).unwrap(),
        PollEvent::ReconnectScheduled { retry_at_ms: 110 }
    );
    assert_eq!(
        outputs.poll(destination_id(2), 10, &mut healthy).unwrap(),
        PollEvent::PacketSent { sequence: 1 }
    );
    for sequence in 2..=4 {
        outputs
            .enqueue(
                destination_id(1),
                packet_with_random_access(
                    rendition,
                    sequence,
                    sequence == 3,
                    usize::try_from(sequence).unwrap(),
                ),
            )
            .unwrap();
    }
    assert_eq!(
        outputs.state(destination_id(2)),
        Some(DestinationState::Live)
    );
    assert_eq!(
        outputs.connection_target(destination_id(1)),
        Some(ConnectionTarget::Backup)
    );
    assert_eq!(outputs.queue_depth(destination_id(1)), Some(4));
    assert_eq!(
        outputs.poll(destination_id(1), 109, &mut sink).unwrap(),
        PollEvent::WaitingToReconnect { retry_at_ms: 110 }
    );
    assert_eq!(
        outputs.poll(destination_id(1), 110, &mut sink).unwrap(),
        PollEvent::AwaitingRandomAccess {
            dropped_packets: 2,
            dropped_bytes: 3,
        }
    );
    assert_eq!(outputs.queue_depth(destination_id(1)), Some(2));
    assert_eq!(outputs.queued_bytes(destination_id(1)), Some(7));
    assert_eq!(
        outputs.poll(destination_id(1), 111, &mut sink).unwrap(),
        PollEvent::PacketSent { sequence: 3 }
    );
    assert_eq!(
        outputs.poll(destination_id(1), 112, &mut sink).unwrap(),
        PollEvent::PacketSent { sequence: 4 }
    );
    assert_eq!(outputs.queue_depth(destination_id(1)), Some(0));
    assert_eq!(
        sink.connection_hosts,
        ["stream.example.test", "backup.example.test"]
    );
    assert_eq!(sink.sequences, [3, 4]);
    let telemetry = outputs.telemetry(destination_id(1)).unwrap();
    assert_eq!(telemetry.reconnects(), 1);
    assert_eq!(telemetry.recovery_dropped_packets(), 2);
    assert_eq!(telemetry.recovery_dropped_bytes(), 3);
    assert_eq!(telemetry.failure_count(), 1);
    assert_eq!(sink.disconnects, 1);
}

#[test]
fn recovery_queue_priority_accepts_first_random_access() {
    let (mut outputs, mut sink, id) = empty_recovery_queue(3);
    for sequence in 2..=4 {
        outputs
            .enqueue(
                destination_id(1),
                packet_with_random_access(
                    id,
                    sequence,
                    false,
                    usize::try_from(sequence).unwrap(),
                ),
            )
            .unwrap();
    }
    assert_eq!(outputs.queue_depth(destination_id(1)), Some(3));
    assert_eq!(
        outputs
            .enqueue(
                destination_id(1),
                packet_with_random_access(id, 5, true, 5),
            )
            .unwrap(),
        EnqueueStatus::Accepted
    );
    assert_eq!(outputs.queue_depth(destination_id(1)), Some(1));
    assert_eq!(outputs.queued_bytes(destination_id(1)), Some(5));
    let telemetry = outputs.telemetry(destination_id(1)).unwrap();
    assert_eq!(telemetry.recovery_dropped_packets(), 4);
    assert_eq!(telemetry.recovery_dropped_bytes(), 10);
    assert_eq!(telemetry.backpressure_events(), 0);
    assert_eq!(
        outputs.poll(destination_id(1), 111, &mut sink).unwrap(),
        PollEvent::PacketSent { sequence: 5 }
    );
    assert_eq!(sink.sequences, [5]);
}

fn empty_recovery_queue(capacity: usize) -> (OutputSet, FakeSink, RenditionId) {
    let id = rendition_id(1);
    let mut outputs = OutputSet::new();
    outputs
        .add_destination(config(1, capacity, OutputProtocol::Rtmp))
        .unwrap();
    outputs.start(destination_id(1)).unwrap();
    let mut sink = FakeSink::with_write_results([Err(SinkError::new(
        FailureStage::Write,
        Some(54),
        "reset",
        true,
    ))]);

    outputs.poll(destination_id(1), 0, &mut sink).unwrap();
    outputs
        .enqueue(
            destination_id(1),
            packet_with_random_access(id, 1, false, 1),
        )
        .unwrap();
    assert_eq!(
        outputs.poll(destination_id(1), 10, &mut sink).unwrap(),
        PollEvent::ReconnectScheduled { retry_at_ms: 110 }
    );
    assert_eq!(
        outputs.poll(destination_id(1), 110, &mut sink).unwrap(),
        PollEvent::AwaitingRandomAccess {
            dropped_packets: 1,
            dropped_bytes: 1,
        }
    );
    (outputs, sink, id)
}

#[test]
fn recovery_queue_priority_preserves_queued_random_access() {
    let (mut outputs, mut sink, id) = empty_recovery_queue(3);
    outputs
        .enqueue(
            destination_id(1),
            packet_with_random_access(id, 2, true, 2),
        )
        .unwrap();
    for sequence in 3..=4 {
        outputs
            .enqueue(
                destination_id(1),
                packet_with_random_access(
                    id,
                    sequence,
                    false,
                    usize::try_from(sequence).unwrap(),
                ),
            )
            .unwrap();
    }
    let rejected = outputs
        .enqueue(
            destination_id(1),
            packet_with_random_access(id, 5, true, 5),
        )
        .unwrap();
    let EnqueueStatus::Backpressure(rejected) = rejected else {
        panic!("protected recovery packet did not keep normal backpressure");
    };
    assert_eq!(rejected.sequence(), 5);
    assert_eq!(outputs.queue_depth(destination_id(1)), Some(3));
    assert_eq!(outputs.queued_bytes(destination_id(1)), Some(9));
    let telemetry = outputs.telemetry(destination_id(1)).unwrap();
    assert_eq!(telemetry.recovery_dropped_packets(), 1);
    assert_eq!(telemetry.recovery_dropped_bytes(), 1);
    assert_eq!(telemetry.backpressure_events(), 1);
    assert_eq!(
        outputs.poll(destination_id(1), 111, &mut sink).unwrap(),
        PollEvent::PacketSent { sequence: 2 }
    );
    assert_eq!(sink.sequences, [2]);
}

#[test]
fn non_retryable_primary_connection_failure_never_contacts_backup() {
    let mut outputs = OutputSet::new();
    outputs
        .add_destination(config(1, 1, OutputProtocol::Rtmp))
        .unwrap();
    outputs.start(destination_id(1)).unwrap();
    let mut sink = FakeSink::with_connect_results([Err(SinkError::new(
        FailureStage::Authentication,
        Some(401),
        "rejected",
        false,
    ))]);

    assert_eq!(
        outputs.poll(destination_id(1), 0, &mut sink).unwrap(),
        PollEvent::Failed
    );
    assert_eq!(
        outputs.connection_target(destination_id(1)),
        Some(ConnectionTarget::Primary)
    );
    assert_eq!(
        outputs.poll(destination_id(1), 1_000, &mut sink).unwrap(),
        PollEvent::Idle
    );
    assert_eq!(sink.connection_hosts, ["stream.example.test"]);
}

#[test]
fn bounded_queue_and_sink_congestion_retain_order() {
    let mut outputs = OutputSet::new();
    outputs
        .add_destination(config(1, 1, OutputProtocol::Rtmp))
        .unwrap();
    outputs.start(destination_id(1)).unwrap();
    let mut sink = FakeSink::with_write_results([
        Ok(SinkWrite::Congested(CongestionObservation {
            round_trip_time_ms: Some(200),
            available_bitrate_bps: Some(500_000),
        })),
        Ok(SinkWrite::Sent(SendObservation::default())),
    ]);
    outputs.poll(destination_id(1), 0, &mut sink).unwrap();
    let id = rendition_id(1);
    assert_eq!(
        outputs.enqueue(destination_id(1), packet(id, 1)).unwrap(),
        EnqueueStatus::Accepted
    );
    let rejected = outputs.enqueue(destination_id(1), packet(id, 2)).unwrap();
    let EnqueueStatus::Backpressure(rejected) = rejected else {
        panic!("full destination did not apply backpressure");
    };
    assert_eq!(rejected.sequence(), 2);
    assert_eq!(outputs.queue_depth(destination_id(1)), Some(1));
    assert_eq!(
        outputs.poll(destination_id(1), 1, &mut sink).unwrap(),
        PollEvent::Congested
    );
    assert_eq!(outputs.queue_depth(destination_id(1)), Some(1));
    assert_eq!(
        outputs.poll(destination_id(1), 2, &mut sink).unwrap(),
        PollEvent::PacketSent { sequence: 1 }
    );
    assert_eq!(sink.sequences, [1]);
    let telemetry = outputs.telemetry(destination_id(1)).unwrap();
    assert_eq!(telemetry.backpressure_events(), 1);
    assert!(telemetry.congestion_events() >= 1);
    assert_eq!(telemetry.queue_high_water_packets(), 1);
}

#[test]
fn dns_and_tls_failures_are_classified_and_retained() {
    let mut outputs = OutputSet::new();
    outputs
        .add_destination(config(1, 1, OutputProtocol::Rtmps))
        .unwrap();
    outputs
        .add_destination(config(2, 1, OutputProtocol::Rtmps))
        .unwrap();
    outputs.start(destination_id(1)).unwrap();
    outputs.start(destination_id(2)).unwrap();
    let mut dns = FakeSink::with_connect_results([Err(SinkError::new(
        FailureStage::Dns,
        Some(-2),
        "name not found",
        true,
    ))]);
    let mut tls = FakeSink::with_connect_results([Err(SinkError::new(
        FailureStage::Tls,
        Some(42),
        "certificate expired",
        false,
    ))]);
    assert_eq!(
        outputs.poll(destination_id(1), 50, &mut dns).unwrap(),
        PollEvent::ReconnectScheduled { retry_at_ms: 150 }
    );
    assert_eq!(
        outputs.poll(destination_id(2), 50, &mut tls).unwrap(),
        PollEvent::Failed
    );
    let dns_record = outputs
        .telemetry(destination_id(1))
        .unwrap()
        .latest_failure()
        .unwrap();
    assert_eq!(dns_record.stage, FailureStage::Dns);
    assert!(dns_record.retryable);
    let tls_record = outputs
        .telemetry(destination_id(2))
        .unwrap()
        .latest_failure()
        .unwrap();
    assert_eq!(tls_record.stage, FailureStage::Tls);
    assert_eq!(tls_record.message, "certificate expired");
    assert!(!tls_record.retryable);
}

#[test]
fn impairment_model_is_deterministic_and_reports_telemetry() {
    let model = ImpairmentModel::new(
        100,
        20,
        1_000_000,
        1_000_000,
        1_000_000,
        Some(750_000),
        true,
        true,
        true,
    )
    .unwrap();
    let first = model.evaluate(7);
    assert_eq!(first, model.evaluate(7));
    assert!((80..=120).contains(&first.delay_ms));
    assert!(first.dropped && first.reordered && first.duplicated);
    let mut telemetry = ImpairmentTelemetry::default();
    telemetry.observe(first);
    assert_eq!(telemetry.evaluated_packets(), 1);
    assert_eq!(telemetry.dropped_packets(), 1);
    assert_eq!(telemetry.disconnect_events(), 1);
    assert_eq!(telemetry.dns_failures(), 1);
    assert_eq!(telemetry.tls_failures(), 1);
    assert_eq!(telemetry.bandwidth_limit_bps(), Some(750_000));
}

#[test]
fn abr_validation_and_hls_sequences_are_aligned_and_bounded() {
    let low = profile(1_280, 720, 3_000_000);
    let high = profile(1_920, 1_080, 6_000_000);
    let ladder = AbrLadder::new(vec![
        AbrVariant::new("720p", low.clone()).unwrap(),
        AbrVariant::new("1080p", high.clone()).unwrap(),
    ])
    .unwrap();
    let plan = RenditionPlanner::plan(&[DestinationRenditions::ladder(destination_id(1), ladder)])
        .unwrap();
    assert_eq!(plan.renditions().len(), 2);
    let low_id = plan.renditions()[0].id();
    let high_id = plan.renditions()[1].id();

    let invalid_high = RenditionProfile::new(
        VideoRendition::new(
            CodecId::new("video/h264").unwrap(),
            "high",
            VideoDimensions::new(1_920, 1_080).unwrap(),
            FrameRate::new(30, 1).unwrap(),
            ColorDescription::Rec709Limited,
            7_000_000,
            120,
        )
        .unwrap()
        .with_codec_setting("b_frames", "2")
        .unwrap(),
        high.audio().clone(),
        high.timing(),
    );
    assert_eq!(
        AbrLadder::new(vec![
            AbrVariant::new("low", low).unwrap(),
            AbrVariant::new("bad", invalid_high).unwrap(),
        ]),
        Err(RenditionError::AbrIncompatibleProfiles)
    );

    let variants = vec![
        HlsVariantMetadata::new(
            low_id,
            "720p",
            3_128_000,
            VideoDimensions::new(1_280, 720).unwrap(),
            "avc1.64002a,mp4a.40.2",
            "720p.m3u8",
        )
        .unwrap(),
        HlsVariantMetadata::new(
            high_id,
            "1080p",
            6_128_000,
            VideoDimensions::new(1_920, 1_080).unwrap(),
            "avc1.64002a,mp4a.40.2",
            "1080p.m3u8",
        )
        .unwrap(),
    ];
    let mut hls =
        HlsAbrCoordinator::new(variants, 7, duration(2), 10, 2, HlsPlaylistType::Live).unwrap();
    hls.append(segment(low_id, 10, 0, 0)).unwrap();
    hls.append(segment(high_id, 10, 0, 0)).unwrap();
    hls.append(segment(low_id, 11, 2, 0)).unwrap();
    let misaligned = HlsSegmentMetadata::new(
        high_id,
        11,
        0,
        NormalizedTimestamp::from_nanos(2_100_000_000),
        duration(2),
        1_000,
        true,
        "high-11.ts",
    )
    .unwrap();
    assert_eq!(hls.append(misaligned), Err(HlsError::AbrSequenceMismatch));
    hls.append(segment(high_id, 11, 2, 0)).unwrap();
    hls.append(segment(low_id, 12, 4, 1)).unwrap();
    hls.append(segment(high_id, 12, 4, 1)).unwrap();
    let metadata = hls.playlist(low_id).unwrap();
    assert_eq!(metadata.media_sequence(), 11);
    assert_eq!(metadata.discontinuity_sequence(), 0);
    assert_eq!(metadata.segments().len(), 2);
    assert_eq!(metadata.segments()[1].discontinuity_sequence(), 1);
    hls.finish();
    assert!(hls.playlist(high_id).unwrap().is_end_list());
}

fn segment(
    rendition: RenditionId,
    sequence: u64,
    start_seconds: i64,
    discontinuity: u64,
) -> HlsSegmentMetadata {
    HlsSegmentMetadata::new(
        rendition,
        sequence,
        discontinuity,
        NormalizedTimestamp::from_nanos(start_seconds * 1_000_000_000),
        duration(2),
        1_000,
        true,
        format!("{}-{sequence}.ts", rendition.get()),
    )
    .unwrap()
}
