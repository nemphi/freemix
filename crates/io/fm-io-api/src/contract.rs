use core::{fmt, num::NonZeroU128, num::NonZeroUsize};
use fm_capabilities::FormatDescriptor;
use fm_frame::{AudioBlock, ClockDomainId, CpuVideoFrame, MediaTiming};
use fm_types::MemoryDomain;

macro_rules! stable_id {
    ($name:ident) => {
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

stable_id!(DeviceId);
stable_id!(SourceId);
stable_id!(SinkId);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Remediation {
    RequestPermission,
    OpenSystemSettings,
    InstallDriver,
    UpdateDriver,
    ReconnectDevice,
    RestartAdapter,
    ContactAdministrator,
    Other(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PermissionState {
    Granted,
    PromptRequired { remediation: Remediation },
    Denied { remediation: Remediation },
    Restricted { remediation: Remediation },
}

impl PermissionState {
    #[must_use]
    pub const fn is_granted(&self) -> bool {
        matches!(self, Self::Granted)
    }

    #[must_use]
    pub const fn remediation(&self) -> Option<&Remediation> {
        match self {
            Self::Granted => None,
            Self::PromptRequired { remediation }
            | Self::Denied { remediation }
            | Self::Restricted { remediation } => Some(remediation),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DriverState {
    Ready,
    Missing {
        remediation: Remediation,
    },
    Outdated {
        remediation: Remediation,
    },
    Failed {
        reason: String,
        remediation: Remediation,
    },
}

impl DriverState {
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    #[must_use]
    pub const fn remediation(&self) -> Option<&Remediation> {
        match self {
            Self::Ready => None,
            Self::Missing { remediation }
            | Self::Outdated { remediation }
            | Self::Failed { remediation, .. } => Some(remediation),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TimestampQuality {
    Unknown,
    Estimated,
    Monotonic,
    Hardware,
    Synchronized,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimestampCapabilities {
    pub quality: TimestampQuality,
    pub resolution_nanos: NonZeroU128,
    pub max_error_nanos: Option<u64>,
    pub monotonic: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockCapability {
    pub domain: ClockDomainId,
    pub timestamps: TimestampCapabilities,
    pub can_follow_external: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransferLimits {
    pub queue_capacity: NonZeroUsize,
    pub max_media_bytes: NonZeroUsize,
}

impl TransferLimits {
    #[must_use]
    pub const fn new(queue_capacity: NonZeroUsize, max_media_bytes: NonZeroUsize) -> Self {
        Self {
            queue_capacity,
            max_media_bytes,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndpointCapabilities {
    pub formats: Vec<FormatDescriptor>,
    pub clocks: Vec<ClockCapability>,
    pub memory_domains: Vec<MemoryDomain>,
    pub transfer: TransferLimits,
}

impl EndpointCapabilities {
    #[must_use]
    pub fn supports(&self, options: &OpenOptions) -> bool {
        self.formats
            .iter()
            .any(|format| format.supports(&options.format))
            && self
                .clocks
                .iter()
                .any(|clock| clock.domain == options.clock_domain)
            && self.memory_domains.contains(&options.memory_domain)
            && options.queue_capacity <= self.transfer.queue_capacity
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceDescriptor {
    pub id: SourceId,
    pub device_id: DeviceId,
    /// Adapter-qualified identity suitable for persisted exact source binding.
    pub stable_key: String,
    pub name: String,
    pub capabilities: EndpointCapabilities,
    pub permission: PermissionState,
    pub driver: DriverState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SinkDescriptor {
    pub id: SinkId,
    pub device_id: DeviceId,
    pub name: String,
    pub capabilities: EndpointCapabilities,
    pub permission: PermissionState,
    pub driver: DriverState,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SignalLossPolicy {
    Hold,
    Slate,
    Stop,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenOptions {
    pub format: FormatDescriptor,
    pub clock_domain: ClockDomainId,
    pub memory_domain: MemoryDomain,
    pub queue_capacity: NonZeroUsize,
    pub signal_loss: SignalLossPolicy,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LifecycleState {
    Closed,
    Open,
    Running,
    Lost,
    Recovering,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EndpointHealthState {
    Healthy,
    Degraded,
    SignalLost,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndpointHealth {
    pub state: EndpointHealthState,
    pub detail: Option<String>,
    pub remediation: Option<Remediation>,
}

impl EndpointHealth {
    pub const HEALTHY: Self = Self {
        state: EndpointHealthState::Healthy,
        detail: None,
        remediation: None,
    };

    #[must_use]
    pub const fn is_usable(&self) -> bool {
        matches!(
            self.state,
            EndpointHealthState::Healthy | EndpointHealthState::Degraded
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoverySnapshot {
    pub generation: u64,
    pub sources: Vec<SourceDescriptor>,
    pub sinks: Vec<SinkDescriptor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryEvent {
    pub generation: u64,
    pub kind: DiscoveryEventKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiscoveryEventKind {
    SourceAdded(SourceDescriptor),
    SourceUpdated(SourceDescriptor),
    SourceRemoved(SourceId),
    SinkAdded(SinkDescriptor),
    SinkUpdated(SinkDescriptor),
    SinkRemoved(SinkId),
}

pub trait Discovery {
    fn snapshot(&self) -> DiscoverySnapshot;
    fn next_event(&mut self) -> Option<DiscoveryEvent>;
}

pub trait MediaUnit {
    fn timing(&self) -> MediaTiming;
    fn byte_len(&self) -> usize;
}

impl MediaUnit for AudioBlock {
    fn timing(&self) -> MediaTiming {
        self.timing()
    }

    fn byte_len(&self) -> usize {
        self.sample_count()
            .saturating_mul(self.channel_layout().channels().len())
            .saturating_mul(size_of::<f32>())
    }
}

impl MediaUnit for CpuVideoFrame {
    fn timing(&self) -> MediaTiming {
        self.timing()
    }

    fn byte_len(&self) -> usize {
        self.payload().byte_len()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimestampValidationError {
    WrongClock {
        expected: ClockDomainId,
        actual: ClockDomainId,
    },
    OriginalTimestampOverflow,
    NormalizationMismatch {
        expected_nanos: i64,
        actual_nanos: i64,
        tolerance_nanos: u64,
    },
    NonMonotonic {
        previous_nanos: i64,
        actual_nanos: i64,
    },
    NonSequential {
        expected: u64,
        actual: u64,
    },
}

impl fmt::Display for TimestampValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongClock { expected, actual } => {
                write!(
                    formatter,
                    "timestamp clock {actual} does not match {expected}"
                )
            }
            Self::OriginalTimestampOverflow => {
                formatter.write_str("original timestamp cannot be normalized")
            }
            Self::NormalizationMismatch {
                expected_nanos,
                actual_nanos,
                tolerance_nanos,
            } => write!(
                formatter,
                "normalized timestamp {actual_nanos} differs from {expected_nanos} by more than {tolerance_nanos} ns"
            ),
            Self::NonMonotonic {
                previous_nanos,
                actual_nanos,
            } => write!(
                formatter,
                "timestamp {actual_nanos} does not follow {previous_nanos}"
            ),
            Self::NonSequential { expected, actual } => {
                write!(formatter, "sequence {actual} does not follow {expected}")
            }
        }
    }
}

impl std::error::Error for TimestampValidationError {}

#[derive(Clone, Debug)]
pub struct TimestampValidator {
    capability: ClockCapability,
    previous: Option<MediaTiming>,
}

impl TimestampValidator {
    #[must_use]
    pub const fn new(capability: ClockCapability) -> Self {
        Self {
            capability,
            previous: None,
        }
    }

    pub fn reset(&mut self) {
        self.previous = None;
    }

    /// Validates clock identity, normalization quality, monotonicity, and sequence.
    ///
    /// # Errors
    ///
    /// Returns the first malformed timestamp property found. Failed samples do
    /// not advance validator state.
    pub fn validate(&mut self, timing: MediaTiming) -> Result<(), TimestampValidationError> {
        if timing.clock_domain() != self.capability.domain {
            return Err(TimestampValidationError::WrongClock {
                expected: self.capability.domain,
                actual: timing.clock_domain(),
            });
        }

        if let Some(tolerance) = self.capability.timestamps.max_error_nanos {
            let expected = timing
                .original_timestamp()
                .normalize()
                .map_err(|_| TimestampValidationError::OriginalTimestampOverflow)?
                .as_nanos();
            let actual = timing.presentation_timestamp().as_nanos();
            if expected.abs_diff(actual) > tolerance {
                return Err(TimestampValidationError::NormalizationMismatch {
                    expected_nanos: expected,
                    actual_nanos: actual,
                    tolerance_nanos: tolerance,
                });
            }
        }

        if let Some(previous) = self.previous {
            let previous_nanos = previous.presentation_timestamp().as_nanos();
            let actual_nanos = timing.presentation_timestamp().as_nanos();
            if self.capability.timestamps.monotonic && actual_nanos <= previous_nanos {
                return Err(TimestampValidationError::NonMonotonic {
                    previous_nanos,
                    actual_nanos,
                });
            }
            if let Some(expected) = previous.sequence().get().checked_add(1)
                && timing.sequence().get() != expected
            {
                return Err(TimestampValidationError::NonSequential {
                    expected,
                    actual: timing.sequence().get(),
                });
            }
        }

        self.previous = Some(timing);
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IoError {
    InvalidState {
        operation: &'static str,
        state: LifecycleState,
    },
    UnsupportedFormat,
    UnsupportedClock,
    UnsupportedMemoryDomain,
    QueueCapacityUnsupported {
        requested: usize,
        maximum: usize,
    },
    PermissionDenied {
        remediation: Remediation,
    },
    DriverUnavailable {
        remediation: Remediation,
    },
    EndpointUnavailable {
        remediation: Remediation,
    },
    SignalLost {
        policy: SignalLossPolicy,
    },
    MediaTooLarge {
        actual: usize,
        maximum: usize,
    },
    MalformedTimestamp(TimestampValidationError),
    AdapterFailure {
        detail: String,
        remediation: Option<Remediation>,
    },
}

impl fmt::Display for IoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidState { operation, state } => {
                write!(formatter, "cannot {operation} while endpoint is {state:?}")
            }
            Self::UnsupportedFormat => formatter.write_str("media format is unsupported"),
            Self::UnsupportedClock => formatter.write_str("clock domain is unsupported"),
            Self::UnsupportedMemoryDomain => formatter.write_str("memory domain is unsupported"),
            Self::QueueCapacityUnsupported { requested, maximum } => write!(
                formatter,
                "queue capacity {requested} exceeds supported maximum {maximum}"
            ),
            Self::PermissionDenied { .. } => formatter.write_str("permission is not granted"),
            Self::DriverUnavailable { .. } => formatter.write_str("driver is unavailable"),
            Self::EndpointUnavailable { .. } => formatter.write_str("endpoint is unavailable"),
            Self::SignalLost { policy } => write!(
                formatter,
                "source signal is lost and the fallback policy is {policy:?}"
            ),
            Self::MediaTooLarge { actual, maximum } => {
                write!(formatter, "media unit size {actual} exceeds {maximum}")
            }
            Self::MalformedTimestamp(error) => write!(formatter, "malformed timestamp: {error}"),
            Self::AdapterFailure { detail, .. } => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for IoError {}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FallbackKind {
    Hold,
    Slate,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MediaTransfer<M> {
    Live(M),
    Fallback { kind: FallbackKind, media: M },
}

pub trait MediaSource {
    type Media: MediaUnit;

    fn descriptor(&self) -> &SourceDescriptor;
    fn lifecycle(&self) -> LifecycleState;
    fn health(&self) -> &EndpointHealth;

    /// Opens this source with one advertised configuration.
    ///
    /// # Errors
    ///
    /// Returns a capability, availability, permission, driver, or state error.
    fn open(&mut self, options: OpenOptions) -> Result<(), IoError>;

    /// Starts media transfer from an open source.
    ///
    /// # Errors
    ///
    /// Returns an invalid-state or adapter error when the source cannot start.
    fn start(&mut self) -> Result<(), IoError>;

    /// Stops media transfer while retaining the open configuration.
    ///
    /// # Errors
    ///
    /// Returns an invalid-state or adapter error when the source cannot stop.
    fn stop(&mut self) -> Result<(), IoError>;

    /// Releases the open source and its queued media.
    ///
    /// # Errors
    ///
    /// Returns an invalid-state or adapter error when the source cannot close.
    fn close(&mut self) -> Result<(), IoError>;

    /// Moves a lost source into its recovering state.
    ///
    /// # Errors
    ///
    /// Returns an error unless the source is lost and recovery can begin.
    fn begin_recovery(&mut self) -> Result<(), IoError>;

    /// Completes recovery and restores the pre-loss open or running state.
    ///
    /// # Errors
    ///
    /// Returns an error if recovery is incomplete or the endpoint is unavailable.
    fn finish_recovery(&mut self) -> Result<(), IoError>;

    /// Non-blockingly receives one live or fallback media unit.
    ///
    /// # Errors
    ///
    /// Returns a state, signal-loss, timestamp, or adapter error.
    fn try_receive(&mut self) -> Result<Option<MediaTransfer<Self::Media>>, IoError>;
}

pub enum WriteError<M> {
    QueueFull(M),
    Rejected { media: M, error: IoError },
}

impl<M> WriteError<M> {
    #[must_use]
    pub fn error(&self) -> Option<&IoError> {
        match self {
            Self::QueueFull(_) => None,
            Self::Rejected { error, .. } => Some(error),
        }
    }

    #[must_use]
    pub fn into_media(self) -> M {
        match self {
            Self::QueueFull(media) | Self::Rejected { media, .. } => media,
        }
    }
}

impl<M> fmt::Debug for WriteError<M> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QueueFull(_) => formatter.write_str("WriteError::QueueFull(..)"),
            Self::Rejected { error, .. } => formatter
                .debug_struct("WriteError::Rejected")
                .field("error", error)
                .finish_non_exhaustive(),
        }
    }
}

pub trait MediaSink {
    type Media: MediaUnit;

    fn descriptor(&self) -> &SinkDescriptor;
    fn lifecycle(&self) -> LifecycleState;
    fn health(&self) -> &EndpointHealth;

    /// Opens this sink with one advertised configuration.
    ///
    /// # Errors
    ///
    /// Returns a capability, availability, permission, driver, or state error.
    fn open(&mut self, options: OpenOptions) -> Result<(), IoError>;

    /// Starts media transfer to an open sink.
    ///
    /// # Errors
    ///
    /// Returns an invalid-state or adapter error when the sink cannot start.
    fn start(&mut self) -> Result<(), IoError>;

    /// Stops media transfer while retaining the open configuration.
    ///
    /// # Errors
    ///
    /// Returns an invalid-state or adapter error when the sink cannot stop.
    fn stop(&mut self) -> Result<(), IoError>;

    /// Releases the open sink and its queued media.
    ///
    /// # Errors
    ///
    /// Returns an invalid-state or adapter error when the sink cannot close.
    fn close(&mut self) -> Result<(), IoError>;

    /// Moves a lost sink into its recovering state.
    ///
    /// # Errors
    ///
    /// Returns an error unless the sink is lost and recovery can begin.
    fn begin_recovery(&mut self) -> Result<(), IoError>;

    /// Completes recovery and restores the pre-loss open or running state.
    ///
    /// # Errors
    ///
    /// Returns an error if recovery is incomplete or the endpoint is unavailable.
    fn finish_recovery(&mut self) -> Result<(), IoError>;

    /// Non-blockingly queues one media unit for output.
    ///
    /// # Errors
    ///
    /// Returns the media in [`WriteError`] when the queue is full or delivery is rejected.
    fn try_send(&mut self, media: Self::Media) -> Result<(), WriteError<Self::Media>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SinkOutcome {
    pub sink_id: SinkId,
    pub status: DeliveryStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeliveryStatus {
    Accepted,
    QueueFull,
    Failed(IoError),
}

/// Attempts delivery to every sink, even if an earlier sink rejects the unit.
#[must_use]
pub fn deliver_isolated<M>(
    sinks: &mut [&mut dyn MediaSink<Media = M>],
    media: &M,
) -> Vec<SinkOutcome>
where
    M: MediaUnit + Clone,
{
    sinks
        .iter_mut()
        .map(|sink| {
            let sink_id = sink.descriptor().id;
            let status = match sink.try_send(media.clone()) {
                Ok(()) => DeliveryStatus::Accepted,
                Err(WriteError::QueueFull(_)) => DeliveryStatus::QueueFull,
                Err(WriteError::Rejected { error, .. }) => DeliveryStatus::Failed(error),
            };
            SinkOutcome { sink_id, status }
        })
        .collect()
}

pub(crate) fn validate_open(
    capabilities: &EndpointCapabilities,
    permission: &PermissionState,
    driver: &DriverState,
    options: &OpenOptions,
) -> Result<ClockCapability, IoError> {
    if let Some(remediation) = permission.remediation() {
        return Err(IoError::PermissionDenied {
            remediation: remediation.clone(),
        });
    }
    if let Some(remediation) = driver.remediation() {
        return Err(IoError::DriverUnavailable {
            remediation: remediation.clone(),
        });
    }
    if !capabilities
        .formats
        .iter()
        .any(|format| format.supports(&options.format))
    {
        return Err(IoError::UnsupportedFormat);
    }
    let clock = capabilities
        .clocks
        .iter()
        .find(|clock| clock.domain == options.clock_domain)
        .copied()
        .ok_or(IoError::UnsupportedClock)?;
    if !capabilities.memory_domains.contains(&options.memory_domain) {
        return Err(IoError::UnsupportedMemoryDomain);
    }
    if options.queue_capacity > capabilities.transfer.queue_capacity {
        return Err(IoError::QueueCapacityUnsupported {
            requested: options.queue_capacity.get(),
            maximum: capabilities.transfer.queue_capacity.get(),
        });
    }
    Ok(clock)
}
