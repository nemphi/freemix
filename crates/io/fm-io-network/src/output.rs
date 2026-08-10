use core::fmt;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use fm_frame::{NormalizedDuration, NormalizedTimestamp};

use crate::{
    DestinationConfig, DestinationId, Endpoint, MAX_DESTINATIONS, RenditionId, RenditionPlan,
};

const MAX_FAILURE_RECORDS: usize = 32;
const MAX_PACKET_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputPacket {
    rendition: RenditionId,
    sequence: u64,
    timestamp: NormalizedTimestamp,
    duration: NormalizedDuration,
    random_access: bool,
    encode_latency_ms: Option<u64>,
    payload: Arc<[u8]>,
}

impl OutputPacket {
    /// Creates a bounded immutable packet suitable for cheap fan-out clones.
    ///
    /// # Errors
    ///
    /// Rejects empty packets and packets larger than 64 MiB.
    pub fn new(
        rendition: RenditionId,
        sequence: u64,
        timestamp: NormalizedTimestamp,
        duration: NormalizedDuration,
        random_access: bool,
        payload: impl Into<Arc<[u8]>>,
    ) -> Result<Self, OutputError> {
        let payload = payload.into();
        if payload.is_empty() || payload.len() > MAX_PACKET_BYTES {
            return Err(OutputError::InvalidPacketSize(payload.len()));
        }
        Ok(Self {
            rendition,
            sequence,
            timestamp,
            duration,
            random_access,
            encode_latency_ms: None,
            payload,
        })
    }

    #[must_use]
    pub const fn with_encode_latency_ms(mut self, latency_ms: u64) -> Self {
        self.encode_latency_ms = Some(latency_ms);
        self
    }

    #[must_use]
    pub const fn rendition(&self) -> RenditionId {
        self.rendition
    }

    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub const fn timestamp(&self) -> NormalizedTimestamp {
        self.timestamp
    }

    #[must_use]
    pub const fn duration(&self) -> NormalizedDuration {
        self.duration
    }

    #[must_use]
    pub const fn is_random_access(&self) -> bool {
        self.random_access
    }

    #[must_use]
    pub const fn encode_latency_ms(&self) -> Option<u64> {
        self.encode_latency_ms
    }

    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    #[must_use]
    pub fn shared_payload(&self) -> Arc<[u8]> {
        Arc::clone(&self.payload)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureStage {
    Dns,
    Tls,
    Authentication,
    Connect,
    Write,
    Protocol,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SinkError {
    stage: FailureStage,
    code: Option<i64>,
    message: String,
    retryable: bool,
}

impl SinkError {
    #[must_use]
    pub fn new(
        stage: FailureStage,
        code: Option<i64>,
        message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self {
            stage,
            code,
            message: message.into(),
            retryable,
        }
    }

    #[must_use]
    pub const fn stage(&self) -> FailureStage {
        self.stage
    }

    #[must_use]
    pub const fn code(&self) -> Option<i64> {
        self.code
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        self.retryable
    }
}

impl fmt::Display for SinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?} failure: {}", self.stage, self.message)
    }
}

impl std::error::Error for SinkError {}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ConnectionObservation {
    pub round_trip_time_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SendObservation {
    pub round_trip_time_ms: Option<u64>,
    pub packet_loss_ppm: Option<u32>,
    pub retransmitted_packets: u64,
    pub bitrate_bps: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CongestionObservation {
    pub round_trip_time_ms: Option<u64>,
    pub available_bitrate_bps: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SinkWrite {
    Sent(SendObservation),
    Congested(CongestionObservation),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionTarget {
    Primary,
    Backup,
}

/// Adapter boundary for fake or real transports. Implementations own I/O;
/// this crate owns policy and never blocks inside its state model.
pub trait TransportSink: Send {
    /// Establishes the selected endpoint with the destination configuration.
    ///
    /// # Errors
    ///
    /// Returns a classified DNS, TLS, authentication, connect, or protocol error.
    fn connect(
        &mut self,
        config: &DestinationConfig,
        endpoint: &Endpoint,
    ) -> Result<ConnectionObservation, SinkError>;

    /// Attempts one packet write without taking ownership of the queued packet.
    ///
    /// # Errors
    ///
    /// Returns a classified transport error. Temporary queue pressure is
    /// represented by [`SinkWrite::Congested`], not an error.
    fn write(&mut self, packet: &OutputPacket) -> Result<SinkWrite, SinkError>;

    fn disconnect(&mut self);
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailureRecord {
    pub at_ms: u64,
    pub stage: FailureStage,
    pub code: Option<i64>,
    pub message: String,
    pub retryable: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DestinationState {
    Stopped,
    Connecting,
    Live,
    Congested,
    WaitingToReconnect { attempt: u32, retry_at_ms: u64 },
    Failed,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NetworkTelemetry {
    connect_attempts: u64,
    reconnects: u64,
    packets_accepted: u64,
    packets_sent: u64,
    bytes_sent: u64,
    backpressure_events: u64,
    congestion_events: u64,
    retransmitted_packets: u64,
    queue_high_water_packets: usize,
    queue_high_water_bytes: usize,
    failure_count: u64,
    round_trip_time_ms: Option<u64>,
    packet_loss_ppm: Option<u32>,
    bitrate_bps: Option<u64>,
    available_bitrate_bps: Option<u64>,
    encoder_latency_ms: Option<u64>,
    last_successful_media_timestamp: Option<NormalizedTimestamp>,
    failures: VecDeque<FailureRecord>,
}

impl NetworkTelemetry {
    #[must_use]
    pub const fn connect_attempts(&self) -> u64 {
        self.connect_attempts
    }

    #[must_use]
    pub const fn reconnects(&self) -> u64 {
        self.reconnects
    }

    #[must_use]
    pub const fn packets_accepted(&self) -> u64 {
        self.packets_accepted
    }

    #[must_use]
    pub const fn packets_sent(&self) -> u64 {
        self.packets_sent
    }

    #[must_use]
    pub const fn bytes_sent(&self) -> u64 {
        self.bytes_sent
    }

    #[must_use]
    pub const fn backpressure_events(&self) -> u64 {
        self.backpressure_events
    }

    #[must_use]
    pub const fn congestion_events(&self) -> u64 {
        self.congestion_events
    }

    #[must_use]
    pub const fn retransmitted_packets(&self) -> u64 {
        self.retransmitted_packets
    }

    #[must_use]
    pub const fn queue_high_water_packets(&self) -> usize {
        self.queue_high_water_packets
    }

    #[must_use]
    pub const fn queue_high_water_bytes(&self) -> usize {
        self.queue_high_water_bytes
    }

    #[must_use]
    pub const fn failure_count(&self) -> u64 {
        self.failure_count
    }

    #[must_use]
    pub const fn round_trip_time_ms(&self) -> Option<u64> {
        self.round_trip_time_ms
    }

    #[must_use]
    pub const fn packet_loss_ppm(&self) -> Option<u32> {
        self.packet_loss_ppm
    }

    #[must_use]
    pub const fn bitrate_bps(&self) -> Option<u64> {
        self.bitrate_bps
    }

    #[must_use]
    pub const fn available_bitrate_bps(&self) -> Option<u64> {
        self.available_bitrate_bps
    }

    #[must_use]
    pub const fn encoder_latency_ms(&self) -> Option<u64> {
        self.encoder_latency_ms
    }

    #[must_use]
    pub const fn last_successful_media_timestamp(&self) -> Option<NormalizedTimestamp> {
        self.last_successful_media_timestamp
    }

    #[must_use]
    pub fn failures(&self) -> impl ExactSizeIterator<Item = &FailureRecord> {
        self.failures.iter()
    }

    #[must_use]
    pub fn latest_failure(&self) -> Option<&FailureRecord> {
        self.failures.back()
    }

    fn record_failure(&mut self, record: FailureRecord) {
        self.failure_count = self.failure_count.saturating_add(1);
        if self.failures.len() == MAX_FAILURE_RECORDS {
            self.failures.pop_front();
        }
        self.failures.push_back(record);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EnqueueStatus {
    Accepted,
    Backpressure(OutputPacket),
    DestinationUnavailable(OutputPacket),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DestinationEnqueue {
    pub destination: DestinationId,
    pub status: EnqueueStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PollEvent {
    Idle,
    Connected,
    PacketSent { sequence: u64 },
    Congested,
    WaitingToReconnect { retry_at_ms: u64 },
    ReconnectScheduled { retry_at_ms: u64 },
    Failed,
}

struct DestinationOutput {
    config: DestinationConfig,
    connection_target: ConnectionTarget,
    state: DestinationState,
    queue: VecDeque<OutputPacket>,
    queued_bytes: usize,
    reconnect_attempt: u32,
    has_connected: bool,
    telemetry: NetworkTelemetry,
}

impl DestinationOutput {
    fn new(config: DestinationConfig) -> Self {
        Self {
            queue: VecDeque::with_capacity(config.queue_capacity().get()),
            config,
            connection_target: ConnectionTarget::Primary,
            state: DestinationState::Stopped,
            queued_bytes: 0,
            reconnect_attempt: 0,
            has_connected: false,
            telemetry: NetworkTelemetry::default(),
        }
    }

    fn enqueue(&mut self, packet: OutputPacket) -> EnqueueStatus {
        if self.queue.len() == self.config.queue_capacity().get() {
            self.telemetry.backpressure_events =
                self.telemetry.backpressure_events.saturating_add(1);
            if matches!(self.state, DestinationState::Live) {
                self.state = DestinationState::Congested;
                self.telemetry.congestion_events =
                    self.telemetry.congestion_events.saturating_add(1);
            }
            return EnqueueStatus::Backpressure(packet);
        }
        self.queued_bytes = self.queued_bytes.saturating_add(packet.payload.len());
        self.queue.push_back(packet);
        self.telemetry.packets_accepted = self.telemetry.packets_accepted.saturating_add(1);
        self.telemetry.queue_high_water_packets = self
            .telemetry
            .queue_high_water_packets
            .max(self.queue.len());
        self.telemetry.queue_high_water_bytes =
            self.telemetry.queue_high_water_bytes.max(self.queued_bytes);
        EnqueueStatus::Accepted
    }

    fn schedule_failure(&mut self, now_ms: u64, error: SinkError) -> PollEvent {
        let retryable = error.retryable;
        self.telemetry.record_failure(FailureRecord {
            at_ms: now_ms,
            stage: error.stage,
            code: error.code,
            message: error.message,
            retryable,
        });
        self.reconnect_attempt = self.reconnect_attempt.saturating_add(1);
        if retryable
            && self
                .config
                .reconnect()
                .permits_attempt(self.reconnect_attempt)
        {
            if self.connection_target == ConnectionTarget::Primary
                && self.config.backup_endpoint().is_some()
            {
                self.connection_target = ConnectionTarget::Backup;
            }
            let retry_at_ms =
                now_ms.saturating_add(self.config.reconnect().delay_ms(self.reconnect_attempt));
            self.state = DestinationState::WaitingToReconnect {
                attempt: self.reconnect_attempt,
                retry_at_ms,
            };
            PollEvent::ReconnectScheduled { retry_at_ms }
        } else {
            self.state = DestinationState::Failed;
            PollEvent::Failed
        }
    }

    fn connect(&mut self, now_ms: u64, sink: &mut dyn TransportSink) -> PollEvent {
        self.telemetry.connect_attempts = self.telemetry.connect_attempts.saturating_add(1);
        let endpoint = match self.connection_target {
            ConnectionTarget::Primary => self.config.endpoint(),
            ConnectionTarget::Backup => self
                .config
                .backup_endpoint()
                .expect("backup target requires a configured endpoint"),
        };
        match sink.connect(&self.config, endpoint) {
            Ok(observation) => {
                if self.has_connected {
                    self.telemetry.reconnects = self.telemetry.reconnects.saturating_add(1);
                }
                self.has_connected = true;
                self.reconnect_attempt = 0;
                self.state = DestinationState::Live;
                self.telemetry.round_trip_time_ms = observation.round_trip_time_ms;
                PollEvent::Connected
            }
            Err(error) => self.schedule_failure(now_ms, error),
        }
    }

    fn poll(&mut self, now_ms: u64, sink: &mut dyn TransportSink) -> PollEvent {
        match self.state {
            DestinationState::Stopped | DestinationState::Failed => return PollEvent::Idle,
            DestinationState::Connecting => return self.connect(now_ms, sink),
            DestinationState::WaitingToReconnect { retry_at_ms, .. } => {
                if now_ms < retry_at_ms {
                    return PollEvent::WaitingToReconnect { retry_at_ms };
                }
                self.state = DestinationState::Connecting;
                return self.connect(now_ms, sink);
            }
            DestinationState::Live | DestinationState::Congested => {}
        }

        let Some(packet) = self.queue.front() else {
            self.state = DestinationState::Live;
            return PollEvent::Idle;
        };
        match sink.write(packet) {
            Ok(SinkWrite::Sent(observation)) => {
                let packet = self.queue.pop_front().expect("front packet exists");
                self.queued_bytes = self.queued_bytes.saturating_sub(packet.payload.len());
                self.state = DestinationState::Live;
                self.telemetry.packets_sent = self.telemetry.packets_sent.saturating_add(1);
                self.telemetry.bytes_sent = self
                    .telemetry
                    .bytes_sent
                    .saturating_add(u64::try_from(packet.payload.len()).unwrap_or(u64::MAX));
                self.telemetry.retransmitted_packets = self
                    .telemetry
                    .retransmitted_packets
                    .saturating_add(observation.retransmitted_packets);
                self.telemetry.round_trip_time_ms = observation.round_trip_time_ms;
                self.telemetry.packet_loss_ppm = observation.packet_loss_ppm;
                self.telemetry.bitrate_bps = observation.bitrate_bps;
                self.telemetry.encoder_latency_ms = packet.encode_latency_ms;
                self.telemetry.last_successful_media_timestamp = Some(packet.timestamp);
                PollEvent::PacketSent {
                    sequence: packet.sequence,
                }
            }
            Ok(SinkWrite::Congested(observation)) => {
                if self.state != DestinationState::Congested {
                    self.telemetry.congestion_events =
                        self.telemetry.congestion_events.saturating_add(1);
                }
                self.state = DestinationState::Congested;
                self.telemetry.round_trip_time_ms = observation.round_trip_time_ms;
                self.telemetry.available_bitrate_bps = observation.available_bitrate_bps;
                PollEvent::Congested
            }
            Err(error) => {
                sink.disconnect();
                self.schedule_failure(now_ms, error)
            }
        }
    }
}

#[derive(Default)]
pub struct OutputSet {
    destinations: BTreeMap<DestinationId, DestinationOutput>,
}

impl OutputSet {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            destinations: BTreeMap::new(),
        }
    }

    /// Adds an independently queued destination.
    ///
    /// # Errors
    ///
    /// Rejects duplicate IDs and sets larger than five.
    pub fn add_destination(&mut self, config: DestinationConfig) -> Result<(), OutputError> {
        if self.destinations.len() == MAX_DESTINATIONS {
            return Err(OutputError::TooManyDestinations);
        }
        if self.destinations.contains_key(&config.id()) {
            return Err(OutputError::DuplicateDestination(config.id()));
        }
        self.destinations
            .insert(config.id(), DestinationOutput::new(config));
        Ok(())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.destinations.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.destinations.is_empty()
    }

    /// Marks one destination ready for a nonblocking connect attempt.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError::UnknownDestination`] for an unconfigured slot.
    pub fn start(&mut self, destination: DestinationId) -> Result<(), OutputError> {
        let output = self
            .destinations
            .get_mut(&destination)
            .ok_or(OutputError::UnknownDestination(destination))?;
        if matches!(
            output.state,
            DestinationState::Stopped | DestinationState::Failed
        ) {
            output.state = DestinationState::Connecting;
            output.reconnect_attempt = 0;
            output.connection_target = ConnectionTarget::Primary;
        }
        Ok(())
    }

    /// Stops one destination and discards only its queued packets.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError::UnknownDestination`] for an unconfigured slot.
    pub fn stop(
        &mut self,
        destination: DestinationId,
        sink: &mut dyn TransportSink,
    ) -> Result<(), OutputError> {
        let output = self
            .destinations
            .get_mut(&destination)
            .ok_or(OutputError::UnknownDestination(destination))?;
        sink.disconnect();
        output.state = DestinationState::Stopped;
        output.queue.clear();
        output.queued_bytes = 0;
        output.reconnect_attempt = 0;
        Ok(())
    }

    /// Enqueues one packet without evicting old media.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError::UnknownDestination`] for an unconfigured slot.
    pub fn enqueue(
        &mut self,
        destination: DestinationId,
        packet: OutputPacket,
    ) -> Result<EnqueueStatus, OutputError> {
        self.destinations
            .get_mut(&destination)
            .map(|output| output.enqueue(packet))
            .ok_or(OutputError::UnknownDestination(destination))
    }

    /// Fans one shared-rendition packet into every independently bounded route.
    /// A full or missing destination never prevents attempts for other routes.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError::UnknownRendition`] when the plan has no such rendition.
    pub fn enqueue_rendition(
        &mut self,
        plan: &RenditionPlan,
        packet: &OutputPacket,
    ) -> Result<Vec<DestinationEnqueue>, OutputError> {
        let destinations = plan
            .destinations_for(packet.rendition)
            .ok_or(OutputError::UnknownRendition(packet.rendition))?;
        let mut outcomes = Vec::with_capacity(destinations.len());
        for destination in destinations {
            let packet = packet.clone();
            let status = if let Some(output) = self.destinations.get_mut(destination) {
                output.enqueue(packet)
            } else {
                EnqueueStatus::DestinationUnavailable(packet)
            };
            outcomes.push(DestinationEnqueue {
                destination: *destination,
                status,
            });
        }
        Ok(outcomes)
    }

    /// Advances one destination by at most one connection or packet operation.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError::UnknownDestination`] for an unconfigured slot.
    pub fn poll(
        &mut self,
        destination: DestinationId,
        now_ms: u64,
        sink: &mut dyn TransportSink,
    ) -> Result<PollEvent, OutputError> {
        self.destinations
            .get_mut(&destination)
            .map(|output| output.poll(now_ms, sink))
            .ok_or(OutputError::UnknownDestination(destination))
    }

    #[must_use]
    pub fn state(&self, destination: DestinationId) -> Option<DestinationState> {
        self.destinations
            .get(&destination)
            .map(|output| output.state)
    }

    #[must_use]
    pub fn connection_target(&self, destination: DestinationId) -> Option<ConnectionTarget> {
        self.destinations
            .get(&destination)
            .map(|output| output.connection_target)
    }

    #[must_use]
    pub fn config(&self, destination: DestinationId) -> Option<&DestinationConfig> {
        self.destinations
            .get(&destination)
            .map(|output| &output.config)
    }

    #[must_use]
    pub fn telemetry(&self, destination: DestinationId) -> Option<&NetworkTelemetry> {
        self.destinations
            .get(&destination)
            .map(|output| &output.telemetry)
    }

    #[must_use]
    pub fn queue_depth(&self, destination: DestinationId) -> Option<usize> {
        self.destinations
            .get(&destination)
            .map(|output| output.queue.len())
    }

    #[must_use]
    pub fn queued_bytes(&self, destination: DestinationId) -> Option<usize> {
        self.destinations
            .get(&destination)
            .map(|output| output.queued_bytes)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutputError {
    TooManyDestinations,
    DuplicateDestination(DestinationId),
    UnknownDestination(DestinationId),
    UnknownRendition(RenditionId),
    InvalidPacketSize(usize),
}

impl fmt::Display for OutputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyDestinations => {
                formatter.write_str("output set exceeds five destinations")
            }
            Self::DuplicateDestination(destination) => {
                write!(formatter, "destination {destination} is already configured")
            }
            Self::UnknownDestination(destination) => {
                write!(formatter, "destination {destination} is not configured")
            }
            Self::UnknownRendition(rendition) => {
                write!(formatter, "rendition {} is not planned", rendition.get())
            }
            Self::InvalidPacketSize(size) => write!(formatter, "packet size {size} is invalid"),
        }
    }
}

impl std::error::Error for OutputError {}
