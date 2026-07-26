//! Deterministic SRT configuration and session contracts.
//!
//! This crate intentionally performs no socket or libSRT operations. Callers
//! drive [`SrtSession`] with events, making connection behavior reproducible in
//! tests and suitable for an adapter to consume later.

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;

/// Smallest accepted SRT latency.
pub const MIN_LATENCY_MS: u32 = 20;
/// Largest accepted SRT latency.
pub const MAX_LATENCY_MS: u32 = 120_000;
/// Largest packet queue accepted by a session configuration.
pub const MAX_QUEUE_CAPACITY: usize = 65_536;

/// An opaque reference to secret storage, never the secret value itself.
///
/// Formatting this type with either `Debug` or `Display` never reveals its
/// locator. The locator is available explicitly for a secret resolver through
/// [`SecretRef::locator`].
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SecretRef(String);

impl SecretRef {
    /// Creates a non-empty secret reference.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::EmptySecretRef`] if the locator is blank.
    pub fn new(locator: impl Into<String>) -> Result<Self, ConfigError> {
        let locator = locator.into();
        if locator.trim().is_empty() {
            return Err(ConfigError::EmptySecretRef);
        }
        Ok(Self(locator))
    }

    /// Returns the locator for an explicit secret-resolution boundary.
    #[must_use]
    pub fn locator(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretRef([REDACTED])")
    }
}

impl fmt::Display for SecretRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

/// A logical endpoint understood by the eventual transport adapter.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Endpoint(String);

impl Endpoint {
    /// Creates a non-empty endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::EmptyEndpoint`] if the endpoint is blank.
    pub fn new(value: impl Into<String>) -> Result<Self, ConfigError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(ConfigError::EmptyEndpoint);
        }
        Ok(Self(value))
    }

    /// Returns the adapter-facing endpoint representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A caller accepted by a listener, with larger priorities preferred.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallerPeer {
    /// Caller endpoint.
    pub endpoint: Endpoint,
    /// Selection priority. Ties are resolved by endpoint lexical order.
    pub priority: u16,
}

/// SRT connection mode and its endpoint configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SrtMode {
    /// Initiates a connection to one remote peer.
    Caller { remote: Endpoint },
    /// Waits for callers. An empty allowlist accepts any caller.
    Listener {
        local: Endpoint,
        allowed_callers: Vec<CallerPeer>,
    },
    /// Both peers initiate using fixed local and remote endpoints.
    Rendezvous { local: Endpoint, remote: Endpoint },
}

/// Local receive latency and latency advertised to the peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LatencyConfig {
    /// Local receive latency in milliseconds.
    pub receive_ms: u32,
    /// Peer latency in milliseconds.
    pub peer_ms: u32,
}

impl Default for LatencyConfig {
    fn default() -> Self {
        Self {
            receive_ms: 120,
            peer_ms: 120,
        }
    }
}

/// Encryption algorithm selected for the SRT connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Cipher {
    /// No payload encryption.
    #[default]
    None,
    /// AES-128.
    Aes128,
    /// AES-192.
    Aes192,
    /// AES-256.
    Aes256,
}

/// Encryption configuration. Encrypted ciphers require a passphrase reference.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EncryptionConfig {
    /// Cipher to use.
    pub cipher: Cipher,
    /// Reference resolved by the transport adapter when opening a connection.
    pub passphrase: Option<SecretRef>,
}

/// Deterministic reconnect limits and backoff.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReconnectPolicy {
    /// Number of retries after the initial connection fails.
    pub max_attempts: u32,
    /// Delay before the first retry.
    pub initial_delay_ms: u64,
    /// Upper bound for exponential retry delay.
    pub max_delay_ms: u64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay_ms: 250,
            max_delay_ms: 4_000,
        }
    }
}

impl ReconnectPolicy {
    fn retry_delay(self, attempt: u32) -> u64 {
        let shift = attempt.saturating_sub(1).min(63);
        self.initial_delay_ms
            .saturating_mul(1_u64 << shift)
            .min(self.max_delay_ms)
    }
}

/// Complete transport-independent SRT session configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SrtConfig {
    /// Caller, listener, or rendezvous behavior.
    pub mode: SrtMode,
    /// SRT latency settings.
    pub latency: LatencyConfig,
    /// Payload encryption settings.
    pub encryption: EncryptionConfig,
    /// Optional reference to a stream ID. The stream ID itself is not stored.
    pub stream_id: Option<SecretRef>,
    /// Reconnect behavior for caller and rendezvous modes.
    pub reconnect: ReconnectPolicy,
    /// Maximum outgoing packets retained.
    pub send_queue_capacity: usize,
    /// Maximum incoming packets retained.
    pub receive_queue_capacity: usize,
}

impl SrtConfig {
    /// Validates all cross-field and bounded settings.
    ///
    /// # Errors
    ///
    /// Returns the first invalid latency, encryption, queue, reconnect, or
    /// listener allowlist setting.
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_latency("receive", self.latency.receive_ms)?;
        validate_latency("peer", self.latency.peer_ms)?;

        match (self.encryption.cipher, &self.encryption.passphrase) {
            (Cipher::None, Some(_)) => return Err(ConfigError::PassphraseWithoutEncryption),
            (Cipher::Aes128 | Cipher::Aes192 | Cipher::Aes256, None) => {
                return Err(ConfigError::MissingPassphrase);
            }
            _ => {}
        }

        validate_queue_capacity("send", self.send_queue_capacity)?;
        validate_queue_capacity("receive", self.receive_queue_capacity)?;

        if self.reconnect.max_attempts > 0 {
            if self.reconnect.initial_delay_ms == 0 {
                return Err(ConfigError::ZeroReconnectDelay);
            }
            if self.reconnect.max_delay_ms < self.reconnect.initial_delay_ms {
                return Err(ConfigError::ReconnectDelayOrder);
            }
        }

        if let SrtMode::Listener {
            allowed_callers, ..
        } = &self.mode
        {
            for (index, caller) in allowed_callers.iter().enumerate() {
                if allowed_callers[index + 1..]
                    .iter()
                    .any(|other| other.endpoint == caller.endpoint)
                {
                    return Err(ConfigError::DuplicateCaller(caller.endpoint.clone()));
                }
            }
        }

        Ok(())
    }
}

fn validate_latency(name: &'static str, value: u32) -> Result<(), ConfigError> {
    if !(MIN_LATENCY_MS..=MAX_LATENCY_MS).contains(&value) {
        return Err(ConfigError::LatencyOutOfRange { name, value });
    }
    Ok(())
}

fn validate_queue_capacity(name: &'static str, value: usize) -> Result<(), ConfigError> {
    if !(1..=MAX_QUEUE_CAPACITY).contains(&value) {
        return Err(ConfigError::QueueCapacityOutOfRange { name, value });
    }
    Ok(())
}

/// Configuration validation failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// A secret reference was empty.
    EmptySecretRef,
    /// An endpoint was empty.
    EmptyEndpoint,
    /// A latency did not meet the supported bounds.
    LatencyOutOfRange { name: &'static str, value: u32 },
    /// A passphrase was supplied while encryption was disabled.
    PassphraseWithoutEncryption,
    /// An encrypted cipher had no passphrase reference.
    MissingPassphrase,
    /// A packet queue capacity was zero or too large.
    QueueCapacityOutOfRange { name: &'static str, value: usize },
    /// Retry delay was zero while reconnect was enabled.
    ZeroReconnectDelay,
    /// Maximum retry delay was smaller than the initial delay.
    ReconnectDelayOrder,
    /// The listener allowlist repeated an endpoint.
    DuplicateCaller(Endpoint),
    /// Impairment loss exceeded 100 percent.
    ImpairmentLossOutOfRange(u16),
    /// Impairment RTT exceeded the supported latency range.
    ImpairmentRttOutOfRange(u32),
    /// An impairment bandwidth limit was zero.
    ZeroImpairmentBandwidth,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySecretRef => formatter.write_str("secret reference cannot be empty"),
            Self::EmptyEndpoint => formatter.write_str("endpoint cannot be empty"),
            Self::LatencyOutOfRange { name, value } => write!(
                formatter,
                "{name} latency {value}ms is outside {MIN_LATENCY_MS}..={MAX_LATENCY_MS}ms"
            ),
            Self::PassphraseWithoutEncryption => {
                formatter.write_str("passphrase requires an encrypted cipher")
            }
            Self::MissingPassphrase => {
                formatter.write_str("encrypted cipher requires a passphrase reference")
            }
            Self::QueueCapacityOutOfRange { name, value } => write!(
                formatter,
                "{name} queue capacity {value} is outside 1..={MAX_QUEUE_CAPACITY}"
            ),
            Self::ZeroReconnectDelay => formatter.write_str("reconnect delay cannot be zero"),
            Self::ReconnectDelayOrder => {
                formatter.write_str("maximum reconnect delay is below the initial delay")
            }
            Self::DuplicateCaller(endpoint) => {
                write!(formatter, "duplicate allowed caller {endpoint}")
            }
            Self::ImpairmentLossOutOfRange(value) => {
                write!(formatter, "impairment loss {value}bp exceeds 10000bp")
            }
            Self::ImpairmentRttOutOfRange(value) => write!(
                formatter,
                "impairment RTT {value}ms exceeds {MAX_LATENCY_MS}ms"
            ),
            Self::ZeroImpairmentBandwidth => {
                formatter.write_str("impairment bandwidth cannot be zero")
            }
        }
    }
}

impl Error for ConfigError {}

/// A listener-side connection offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallerOffer {
    /// Endpoint making the call.
    pub endpoint: Endpoint,
    /// Offered stream ID reference, if available to the adapter.
    pub stream_id: Option<SecretRef>,
}

/// Selects a caller independent of offer ordering.
///
/// The configured stream ID must match, if present. Allowlisted callers win by
/// highest priority and then lexical endpoint order. An empty allowlist accepts
/// any endpoint and uses lexical order.
#[must_use]
pub fn select_caller_peer(
    allowed_callers: &[CallerPeer],
    required_stream_id: Option<&SecretRef>,
    offers: &[CallerOffer],
) -> Option<Endpoint> {
    offers
        .iter()
        .filter(|offer| {
            required_stream_id.is_none_or(|required| {
                offer
                    .stream_id
                    .as_ref()
                    .is_some_and(|value| value == required)
            })
        })
        .filter_map(|offer| {
            if allowed_callers.is_empty() {
                Some((0, &offer.endpoint))
            } else {
                allowed_callers
                    .iter()
                    .find(|caller| caller.endpoint == offer.endpoint)
                    .map(|caller| (caller.priority, &offer.endpoint))
            }
        })
        .max_by(
            |(left_priority, left_endpoint), (right_priority, right_endpoint)| {
                left_priority
                    .cmp(right_priority)
                    .then_with(|| right_endpoint.cmp(left_endpoint))
            },
        )
        .map(|(_, endpoint)| endpoint.clone())
}

/// Why a session permanently failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailureReason {
    /// Adapter-reported connection failure.
    Connection(String),
    /// Reconnect policy was exhausted.
    ReconnectExhausted(String),
    /// Explicit independent session failure.
    Fatal(String),
}

/// Observable deterministic session state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// Not started, or stopped.
    Stopped,
    /// A caller or rendezvous adapter should attempt a connection.
    Connecting {
        peer: Endpoint,
        /// Zero is the initial connection; positive values are retries.
        attempt: u32,
    },
    /// A listener adapter should wait for calls.
    Listening { local: Endpoint },
    /// A peer is connected.
    Connected {
        peer: Endpoint,
        connected_at_ms: u64,
    },
    /// Waiting for a deterministic reconnect deadline.
    Reconnecting {
        peer: Endpoint,
        attempt: u32,
        retry_at_ms: u64,
    },
    /// This session failed without affecting any other session.
    Failed(FailureReason),
}

/// Input to the deterministic session state machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    /// Begin operation.
    Start,
    /// The adapter established the requested connection.
    ConnectionEstablished { peer: Endpoint },
    /// The current connection attempt failed.
    ConnectionFailed { detail: String },
    /// A listener observed one or more simultaneous callers.
    CallerOffers(Vec<CallerOffer>),
    /// The connected peer disconnected.
    Disconnected { detail: String },
    /// Advance deterministic time and start a due retry.
    AdvanceTime,
    /// Stop and clear queued packets.
    Stop,
    /// Fail only this session immediately.
    Fatal { detail: String },
}

/// Invalid state-machine input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionError {
    /// Session configuration was invalid.
    InvalidConfig(ConfigError),
    /// Event was not valid in the current state.
    InvalidTransition {
        state: SessionState,
        event: &'static str,
    },
    /// Adapter reported a different peer than the configured peer.
    UnexpectedPeer {
        expected: Endpoint,
        actual: Endpoint,
    },
    /// No offered caller met listener policy.
    NoEligibleCaller,
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(error) => error.fmt(formatter),
            Self::InvalidTransition { state, event } => {
                write!(formatter, "event {event} is invalid in state {state:?}")
            }
            Self::UnexpectedPeer { expected, actual } => {
                write!(formatter, "expected peer {expected}, got {actual}")
            }
            Self::NoEligibleCaller => formatter.write_str("no eligible caller offer"),
        }
    }
}

impl Error for SessionError {}

impl From<ConfigError> for SessionError {
    fn from(value: ConfigError) -> Self {
        Self::InvalidConfig(value)
    }
}

/// One transport packet retained by a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    /// Adapter-defined packet sequence.
    pub sequence: u64,
    /// Packet payload.
    pub payload: Vec<u8>,
    /// Deterministic enqueue timestamp.
    pub enqueued_at_ms: u64,
}

/// Queue behavior when capacity is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverflowPolicy {
    /// Preserve queued packets and reject the new packet.
    RejectNewest,
    /// Preserve the new packet and evict the oldest packet.
    DropOldest,
}

/// Result of adding a packet to a bounded queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushOutcome {
    /// Packet was queued without eviction.
    Enqueued,
    /// The oldest packet was evicted and returned.
    DroppedOldest(Packet),
    /// The new packet was rejected and returned.
    Rejected(Packet),
}

/// A deterministic bounded FIFO packet queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PacketQueue {
    capacity: usize,
    overflow: OverflowPolicy,
    packets: VecDeque<Packet>,
}

impl PacketQueue {
    /// Creates a validated packet queue.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::QueueCapacityOutOfRange`] when capacity is zero
    /// or greater than [`MAX_QUEUE_CAPACITY`].
    pub fn new(capacity: usize, overflow: OverflowPolicy) -> Result<Self, ConfigError> {
        validate_queue_capacity("packet", capacity)?;
        Ok(Self {
            capacity,
            overflow,
            packets: VecDeque::with_capacity(capacity),
        })
    }

    /// Adds one packet according to the configured overflow policy.
    pub fn push(&mut self, packet: Packet) -> PushOutcome {
        if self.packets.len() < self.capacity {
            self.packets.push_back(packet);
            return PushOutcome::Enqueued;
        }

        match self.overflow {
            OverflowPolicy::RejectNewest => PushOutcome::Rejected(packet),
            OverflowPolicy::DropOldest => {
                let Some(dropped) = self.packets.pop_front() else {
                    return PushOutcome::Rejected(packet);
                };
                self.packets.push_back(packet);
                PushOutcome::DroppedOldest(dropped)
            }
        }
    }

    /// Removes the oldest packet.
    pub fn pop(&mut self) -> Option<Packet> {
        self.packets.pop_front()
    }

    /// Number of queued packets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.packets.len()
    }

    /// Returns whether the queue contains no packets.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    /// Maximum number of packets retained.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Clears all queued packets.
    pub fn clear(&mut self) {
        self.packets.clear();
    }
}

/// Live, validated network impairment settings for simulation adapters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Impairment {
    /// Additional round-trip delay in milliseconds.
    pub additional_rtt_ms: u32,
    /// Packet loss in basis points, from 0 through 10,000.
    pub loss_basis_points: u16,
    /// Optional bandwidth cap in bits per second.
    pub bandwidth_limit_bps: Option<u64>,
}

impl Impairment {
    /// Validates impairment bounds.
    ///
    /// # Errors
    ///
    /// Returns an error for loss above 100 percent, RTT above the supported
    /// maximum, or a zero bandwidth limit.
    pub fn validate(self) -> Result<(), ConfigError> {
        if self.loss_basis_points > 10_000 {
            return Err(ConfigError::ImpairmentLossOutOfRange(
                self.loss_basis_points,
            ));
        }
        if self.additional_rtt_ms > MAX_LATENCY_MS {
            return Err(ConfigError::ImpairmentRttOutOfRange(self.additional_rtt_ms));
        }
        if self.bandwidth_limit_bps == Some(0) {
            return Err(ConfigError::ZeroImpairmentBandwidth);
        }
        Ok(())
    }
}

/// Revisioned impairment state, allowing adapters to apply each update once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImpairmentState {
    /// Monotonically increasing update revision.
    pub revision: u64,
    /// Current settings.
    pub profile: Impairment,
}

/// Accumulated session statistics.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Statistics {
    started_at_ms: Option<u64>,
    packets_sent: u64,
    packets_received: u64,
    packets_lost: u64,
    packets_retransmitted: u64,
    bytes_sent: u64,
    bytes_received: u64,
    send_queue_drops: u64,
    receive_queue_drops: u64,
    rtt_ms: Option<u32>,
}

/// Immutable statistics calculated at a deterministic timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatisticsSnapshot {
    /// Most recently recorded round-trip time.
    pub rtt_ms: Option<u32>,
    /// Packets handed to the adapter.
    pub packets_sent: u64,
    /// Packets received from the adapter.
    pub packets_received: u64,
    /// Adapter-reported lost packets.
    pub packets_lost: u64,
    /// Adapter-reported retransmitted packets.
    pub packets_retransmitted: u64,
    /// Lost packets divided by received plus lost packets, in basis points.
    pub loss_basis_points: u16,
    /// Average outgoing bandwidth since statistics started.
    pub tx_bandwidth_bps: u64,
    /// Average incoming bandwidth since statistics started.
    pub rx_bandwidth_bps: u64,
    /// Packets rejected by the outgoing queue.
    pub send_queue_drops: u64,
    /// Packets evicted from the incoming queue.
    pub receive_queue_drops: u64,
}

impl Statistics {
    fn ensure_started(&mut self, now_ms: u64) {
        if self.started_at_ms.is_none() {
            self.started_at_ms = Some(now_ms);
        }
    }

    /// Records packets known to be lost by the adapter.
    pub fn record_loss(&mut self, count: u64) {
        self.packets_lost = self.packets_lost.saturating_add(count);
    }

    /// Records retransmitted packets.
    pub fn record_retransmit(&mut self, count: u64) {
        self.packets_retransmitted = self.packets_retransmitted.saturating_add(count);
    }

    /// Replaces the latest round-trip-time sample.
    pub fn record_rtt(&mut self, rtt_ms: u32) {
        self.rtt_ms = Some(rtt_ms);
    }

    /// Produces rates and loss from current counters.
    #[must_use]
    pub fn snapshot(&self, now_ms: u64) -> StatisticsSnapshot {
        let elapsed_ms = self
            .started_at_ms
            .map_or(0, |started| now_ms.saturating_sub(started));
        let loss_denominator = self.packets_received.saturating_add(self.packets_lost);
        let loss_basis_points = ratio_basis_points(self.packets_lost, loss_denominator);

        StatisticsSnapshot {
            rtt_ms: self.rtt_ms,
            packets_sent: self.packets_sent,
            packets_received: self.packets_received,
            packets_lost: self.packets_lost,
            packets_retransmitted: self.packets_retransmitted,
            loss_basis_points,
            tx_bandwidth_bps: bandwidth(self.bytes_sent, elapsed_ms),
            rx_bandwidth_bps: bandwidth(self.bytes_received, elapsed_ms),
            send_queue_drops: self.send_queue_drops,
            receive_queue_drops: self.receive_queue_drops,
        }
    }
}

fn ratio_basis_points(numerator: u64, denominator: u64) -> u16 {
    if denominator == 0 {
        return 0;
    }
    let value = u128::from(numerator) * 10_000 / u128::from(denominator);
    u16::try_from(value.min(10_000)).unwrap_or(10_000)
}

fn bandwidth(bytes: u64, elapsed_ms: u64) -> u64 {
    if elapsed_ms == 0 {
        return 0;
    }
    let value = u128::from(bytes) * 8 * 1_000 / u128::from(elapsed_ms);
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Pure state machine and bounded data plane for one SRT session.
#[derive(Clone, Debug)]
pub struct SrtSession {
    config: SrtConfig,
    state: SessionState,
    send_queue: PacketQueue,
    receive_queue: PacketQueue,
    statistics: Statistics,
    impairment: ImpairmentState,
}

impl SrtSession {
    /// Builds a stopped session after validating its complete configuration.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::InvalidConfig`] if any configuration setting is
    /// invalid.
    pub fn new(config: SrtConfig) -> Result<Self, SessionError> {
        config.validate()?;
        let send_queue =
            PacketQueue::new(config.send_queue_capacity, OverflowPolicy::RejectNewest)?;
        let receive_queue =
            PacketQueue::new(config.receive_queue_capacity, OverflowPolicy::DropOldest)?;
        Ok(Self {
            config,
            state: SessionState::Stopped,
            send_queue,
            receive_queue,
            statistics: Statistics::default(),
            impairment: ImpairmentState::default(),
        })
    }

    /// Current validated configuration.
    #[must_use]
    pub fn config(&self) -> &SrtConfig {
        &self.config
    }

    /// Current state.
    #[must_use]
    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// Current statistics counters.
    #[must_use]
    pub fn statistics(&self) -> &Statistics {
        &self.statistics
    }

    /// Mutable statistics for adapter-reported RTT, loss, and retransmits.
    pub fn statistics_mut(&mut self) -> &mut Statistics {
        &mut self.statistics
    }

    /// Current revisioned impairment profile.
    #[must_use]
    pub fn impairment(&self) -> ImpairmentState {
        self.impairment
    }

    /// Applies a validated impairment update and increments its revision.
    ///
    /// # Errors
    ///
    /// Returns an error without changing the current profile or revision if
    /// the new impairment is outside its supported bounds.
    pub fn update_impairment(&mut self, profile: Impairment) -> Result<u64, ConfigError> {
        profile.validate()?;
        self.impairment.revision = self.impairment.revision.saturating_add(1);
        self.impairment.profile = profile;
        Ok(self.impairment.revision)
    }

    /// Applies one event at the supplied deterministic timestamp.
    ///
    /// # Errors
    ///
    /// Returns an error without changing state when the event is invalid for
    /// the current state, a connected peer differs from configuration, or no
    /// caller offer meets listener policy.
    pub fn apply(&mut self, event: SessionEvent, now_ms: u64) -> Result<(), SessionError> {
        let event_name = event.name();
        let next = match event {
            SessionEvent::Start if self.state == SessionState::Stopped => self.initial_state(),
            SessionEvent::ConnectionEstablished { peer } => {
                self.established_state(peer, now_ms, event_name)?
            }
            SessionEvent::ConnectionFailed { detail } => {
                self.connection_failure_state(detail, now_ms, event_name)?
            }
            SessionEvent::CallerOffers(offers) => {
                self.caller_offer_state(&offers, now_ms, event_name)?
            }
            SessionEvent::Disconnected { detail } => {
                self.disconnected_state(detail, now_ms, event_name)?
            }
            SessionEvent::AdvanceTime => self.advance_time_state(now_ms, event_name)?,
            SessionEvent::Stop => {
                self.send_queue.clear();
                self.receive_queue.clear();
                SessionState::Stopped
            }
            SessionEvent::Fatal { detail } => SessionState::Failed(FailureReason::Fatal(detail)),
            SessionEvent::Start => return Err(self.invalid_transition(event_name)),
        };
        self.state = next;
        Ok(())
    }

    fn initial_state(&self) -> SessionState {
        match &self.config.mode {
            SrtMode::Caller { remote } | SrtMode::Rendezvous { remote, .. } => {
                SessionState::Connecting {
                    peer: remote.clone(),
                    attempt: 0,
                }
            }
            SrtMode::Listener { local, .. } => SessionState::Listening {
                local: local.clone(),
            },
        }
    }

    fn established_state(
        &self,
        peer: Endpoint,
        now_ms: u64,
        event: &'static str,
    ) -> Result<SessionState, SessionError> {
        let SessionState::Connecting { peer: expected, .. } = &self.state else {
            return Err(self.invalid_transition(event));
        };
        if *expected != peer {
            return Err(SessionError::UnexpectedPeer {
                expected: expected.clone(),
                actual: peer,
            });
        }
        Ok(SessionState::Connected {
            peer,
            connected_at_ms: now_ms,
        })
    }

    fn connection_failure_state(
        &self,
        detail: String,
        now_ms: u64,
        event: &'static str,
    ) -> Result<SessionState, SessionError> {
        let SessionState::Connecting { peer, attempt } = &self.state else {
            return Err(self.invalid_transition(event));
        };
        Ok(self.reconnect_or_fail(peer.clone(), *attempt, detail, now_ms))
    }

    fn caller_offer_state(
        &self,
        offers: &[CallerOffer],
        now_ms: u64,
        event: &'static str,
    ) -> Result<SessionState, SessionError> {
        if !matches!(self.state, SessionState::Listening { .. }) {
            return Err(self.invalid_transition(event));
        }
        let SrtMode::Listener {
            allowed_callers, ..
        } = &self.config.mode
        else {
            return Err(self.invalid_transition(event));
        };
        let peer = select_caller_peer(allowed_callers, self.config.stream_id.as_ref(), offers)
            .ok_or(SessionError::NoEligibleCaller)?;
        Ok(SessionState::Connected {
            peer,
            connected_at_ms: now_ms,
        })
    }

    fn disconnected_state(
        &self,
        detail: String,
        now_ms: u64,
        event: &'static str,
    ) -> Result<SessionState, SessionError> {
        let SessionState::Connected { peer, .. } = &self.state else {
            return Err(self.invalid_transition(event));
        };
        match &self.config.mode {
            SrtMode::Listener { local, .. } => Ok(SessionState::Listening {
                local: local.clone(),
            }),
            SrtMode::Caller { .. } | SrtMode::Rendezvous { .. } => {
                Ok(self.reconnect_or_fail(peer.clone(), 0, detail, now_ms))
            }
        }
    }

    fn reconnect_or_fail(
        &self,
        peer: Endpoint,
        completed_attempt: u32,
        detail: String,
        now_ms: u64,
    ) -> SessionState {
        let next_attempt = completed_attempt.saturating_add(1);
        if next_attempt > self.config.reconnect.max_attempts {
            let reason = if self.config.reconnect.max_attempts == 0 {
                FailureReason::Connection(detail)
            } else {
                FailureReason::ReconnectExhausted(detail)
            };
            return SessionState::Failed(reason);
        }
        SessionState::Reconnecting {
            peer,
            attempt: next_attempt,
            retry_at_ms: now_ms.saturating_add(self.config.reconnect.retry_delay(next_attempt)),
        }
    }

    fn advance_time_state(
        &self,
        now_ms: u64,
        event: &'static str,
    ) -> Result<SessionState, SessionError> {
        let SessionState::Reconnecting {
            peer,
            attempt,
            retry_at_ms,
        } = &self.state
        else {
            return Err(self.invalid_transition(event));
        };
        if now_ms < *retry_at_ms {
            return Ok(self.state.clone());
        }
        Ok(SessionState::Connecting {
            peer: peer.clone(),
            attempt: *attempt,
        })
    }

    fn invalid_transition(&self, event: &'static str) -> SessionError {
        SessionError::InvalidTransition {
            state: self.state.clone(),
            event,
        }
    }

    /// Queues an outgoing packet, rejecting newest on overflow.
    pub fn enqueue_send(&mut self, packet: Packet) -> PushOutcome {
        let outcome = self.send_queue.push(packet);
        if matches!(outcome, PushOutcome::Rejected(_)) {
            self.statistics.send_queue_drops = self.statistics.send_queue_drops.saturating_add(1);
        }
        outcome
    }

    /// Removes an outgoing packet and accounts it as sent.
    pub fn dequeue_send(&mut self, now_ms: u64) -> Option<Packet> {
        let packet = self.send_queue.pop()?;
        self.statistics.ensure_started(now_ms);
        self.statistics.packets_sent = self.statistics.packets_sent.saturating_add(1);
        self.statistics.bytes_sent = self
            .statistics
            .bytes_sent
            .saturating_add(packet.payload.len() as u64);
        Some(packet)
    }

    /// Queues an incoming packet, dropping oldest on overflow.
    pub fn enqueue_receive(&mut self, packet: Packet, now_ms: u64) -> PushOutcome {
        self.statistics.ensure_started(now_ms);
        self.statistics.packets_received = self.statistics.packets_received.saturating_add(1);
        self.statistics.bytes_received = self
            .statistics
            .bytes_received
            .saturating_add(packet.payload.len() as u64);
        let outcome = self.receive_queue.push(packet);
        if matches!(outcome, PushOutcome::DroppedOldest(_)) {
            self.statistics.receive_queue_drops =
                self.statistics.receive_queue_drops.saturating_add(1);
        }
        outcome
    }

    /// Removes the oldest incoming packet.
    pub fn dequeue_receive(&mut self) -> Option<Packet> {
        self.receive_queue.pop()
    }

    /// Number of queued outgoing packets.
    #[must_use]
    pub fn send_queue_len(&self) -> usize {
        self.send_queue.len()
    }

    /// Number of queued incoming packets.
    #[must_use]
    pub fn receive_queue_len(&self) -> usize {
        self.receive_queue.len()
    }
}

impl SessionEvent {
    const fn name(&self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::ConnectionEstablished { .. } => "connection-established",
            Self::ConnectionFailed { .. } => "connection-failed",
            Self::CallerOffers(_) => "caller-offers",
            Self::Disconnected { .. } => "disconnected",
            Self::AdvanceTime => "advance-time",
            Self::Stop => "stop",
            Self::Fatal { .. } => "fatal",
        }
    }
}
