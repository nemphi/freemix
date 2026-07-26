//! Deterministic in-memory adapters and discovery controls.

use std::collections::{BTreeMap, VecDeque};

use fm_frame::MediaTiming;

use crate::{
    ClockCapability, Discovery, DiscoveryEvent, DiscoveryEventKind, DiscoverySnapshot, DriverState,
    EndpointHealth, EndpointHealthState, FallbackKind, IoError, LifecycleState, MediaSink,
    MediaSource, MediaTransfer, MediaUnit, OpenOptions, PermissionState, Remediation,
    SinkDescriptor, SinkId, SourceDescriptor, SourceId, TimestampValidator, WriteError,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FakeMedia {
    timing: MediaTiming,
    bytes: Vec<u8>,
}

impl FakeMedia {
    #[must_use]
    pub fn new(timing: MediaTiming, bytes: Vec<u8>) -> Self {
        Self { timing, bytes }
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl MediaUnit for FakeMedia {
    fn timing(&self) -> MediaTiming {
        self.timing
    }

    fn byte_len(&self) -> usize {
        self.bytes.len()
    }
}

#[derive(Debug)]
pub enum InjectError<M> {
    QueueFull(M),
    MediaTooLarge {
        media: M,
        actual: usize,
        maximum: usize,
    },
}

impl<M> InjectError<M> {
    #[must_use]
    pub fn into_media(self) -> M {
        match self {
            Self::QueueFull(media) | Self::MediaTooLarge { media, .. } => media,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct FakeDiscovery {
    generation: u64,
    sources: BTreeMap<SourceId, SourceDescriptor>,
    sinks: BTreeMap<SinkId, SinkDescriptor>,
    events: VecDeque<DiscoveryEvent>,
}

impl FakeDiscovery {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_source(&mut self, descriptor: SourceDescriptor) {
        let kind = if self
            .sources
            .insert(descriptor.id, descriptor.clone())
            .is_some()
        {
            DiscoveryEventKind::SourceUpdated(descriptor)
        } else {
            DiscoveryEventKind::SourceAdded(descriptor)
        };
        self.push_event(kind);
    }

    pub fn remove_source(&mut self, id: SourceId) -> bool {
        if self.sources.remove(&id).is_none() {
            return false;
        }
        self.push_event(DiscoveryEventKind::SourceRemoved(id));
        true
    }

    pub fn add_sink(&mut self, descriptor: SinkDescriptor) {
        let kind = if self
            .sinks
            .insert(descriptor.id, descriptor.clone())
            .is_some()
        {
            DiscoveryEventKind::SinkUpdated(descriptor)
        } else {
            DiscoveryEventKind::SinkAdded(descriptor)
        };
        self.push_event(kind);
    }

    pub fn remove_sink(&mut self, id: SinkId) -> bool {
        if self.sinks.remove(&id).is_none() {
            return false;
        }
        self.push_event(DiscoveryEventKind::SinkRemoved(id));
        true
    }

    pub fn clear_events(&mut self) {
        self.events.clear();
    }

    fn push_event(&mut self, kind: DiscoveryEventKind) {
        self.generation = self.generation.saturating_add(1);
        self.events.push_back(DiscoveryEvent {
            generation: self.generation,
            kind,
        });
    }
}

impl Discovery for FakeDiscovery {
    fn snapshot(&self) -> DiscoverySnapshot {
        DiscoverySnapshot {
            generation: self.generation,
            sources: self.sources.values().cloned().collect(),
            sinks: self.sinks.values().cloned().collect(),
        }
    }

    fn next_event(&mut self) -> Option<DiscoveryEvent> {
        self.events.pop_front()
    }
}

#[derive(Debug)]
pub struct FakeSource<M> {
    descriptor: SourceDescriptor,
    lifecycle: LifecycleState,
    health: EndpointHealth,
    queue: VecDeque<M>,
    configured_capacity: usize,
    validator: Option<TimestampValidator>,
    options: Option<OpenOptions>,
    last_media: Option<M>,
    slate: Option<M>,
    connected: bool,
    resume_running: bool,
}

impl<M> FakeSource<M>
where
    M: MediaUnit + Clone,
{
    #[must_use]
    pub fn new(descriptor: SourceDescriptor) -> Self {
        Self {
            configured_capacity: descriptor.capabilities.transfer.queue_capacity.get(),
            descriptor,
            lifecycle: LifecycleState::Closed,
            health: EndpointHealth::HEALTHY,
            queue: VecDeque::new(),
            validator: None,
            options: None,
            last_media: None,
            slate: None,
            connected: true,
            resume_running: false,
        }
    }

    pub fn set_slate(&mut self, media: M) {
        self.slate = Some(media);
    }

    pub fn set_permission(&mut self, state: PermissionState) {
        self.descriptor.permission = state;
    }

    pub fn set_driver(&mut self, state: DriverState) {
        self.descriptor.driver = state;
    }

    /// Adds one unit to the bounded adapter-side capture queue.
    ///
    /// # Errors
    ///
    /// Returns the media to the caller if the configured queue or media-size
    /// limit would be exceeded.
    pub fn inject(&mut self, media: M) -> Result<(), InjectError<M>> {
        let actual = media.byte_len();
        let maximum = self.descriptor.capabilities.transfer.max_media_bytes.get();
        if actual > maximum {
            return Err(InjectError::MediaTooLarge {
                media,
                actual,
                maximum,
            });
        }
        if self.queue.len() >= self.configured_capacity {
            return Err(InjectError::QueueFull(media));
        }
        self.queue.push_back(media);
        Ok(())
    }

    pub fn lose_signal(&mut self) {
        self.transition_to_lost("source signal lost");
    }

    pub fn unplug(&mut self) {
        self.connected = false;
        self.transition_to_lost("source was unplugged");
    }

    pub fn plug_in(&mut self) {
        self.connected = true;
    }

    fn transition_to_lost(&mut self, detail: &str) {
        if self.lifecycle == LifecycleState::Closed {
            return;
        }
        self.resume_running = self.lifecycle == LifecycleState::Running;
        self.lifecycle = LifecycleState::Lost;
        self.health = EndpointHealth {
            state: EndpointHealthState::SignalLost,
            detail: Some(detail.to_owned()),
            remediation: Some(Remediation::ReconnectDevice),
        };
    }

    fn clock_for(options: &OpenOptions, descriptor: &SourceDescriptor) -> ClockCapability {
        *descriptor
            .capabilities
            .clocks
            .iter()
            .find(|clock| clock.domain == options.clock_domain)
            .expect("open validation ensures the clock exists")
    }
}

impl<M> MediaSource for FakeSource<M>
where
    M: MediaUnit + Clone,
{
    type Media = M;

    fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    fn lifecycle(&self) -> LifecycleState {
        self.lifecycle
    }

    fn health(&self) -> &EndpointHealth {
        &self.health
    }

    fn open(&mut self, options: OpenOptions) -> Result<(), IoError> {
        if self.lifecycle != LifecycleState::Closed {
            return Err(invalid_state("open", self.lifecycle));
        }
        if !self.connected {
            return Err(IoError::EndpointUnavailable {
                remediation: Remediation::ReconnectDevice,
            });
        }
        crate::contract::validate_open(
            &self.descriptor.capabilities,
            &self.descriptor.permission,
            &self.descriptor.driver,
            &options,
        )?;
        let clock = Self::clock_for(&options, &self.descriptor);
        self.configured_capacity = options.queue_capacity.get();
        self.validator = Some(TimestampValidator::new(clock));
        self.options = Some(options);
        self.lifecycle = LifecycleState::Open;
        self.health = EndpointHealth::HEALTHY;
        Ok(())
    }

    fn start(&mut self) -> Result<(), IoError> {
        if self.lifecycle != LifecycleState::Open {
            return Err(invalid_state("start", self.lifecycle));
        }
        self.lifecycle = LifecycleState::Running;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), IoError> {
        if self.lifecycle != LifecycleState::Running {
            return Err(invalid_state("stop", self.lifecycle));
        }
        self.lifecycle = LifecycleState::Open;
        Ok(())
    }

    fn close(&mut self) -> Result<(), IoError> {
        if !matches!(self.lifecycle, LifecycleState::Open | LifecycleState::Lost) {
            return Err(invalid_state("close", self.lifecycle));
        }
        self.lifecycle = LifecycleState::Closed;
        self.validator = None;
        self.options = None;
        self.last_media = None;
        self.queue.clear();
        Ok(())
    }

    fn begin_recovery(&mut self) -> Result<(), IoError> {
        if self.lifecycle != LifecycleState::Lost {
            return Err(invalid_state("begin recovery", self.lifecycle));
        }
        self.lifecycle = LifecycleState::Recovering;
        Ok(())
    }

    fn finish_recovery(&mut self) -> Result<(), IoError> {
        if self.lifecycle != LifecycleState::Recovering {
            return Err(invalid_state("finish recovery", self.lifecycle));
        }
        if !self.connected {
            return Err(IoError::EndpointUnavailable {
                remediation: Remediation::ReconnectDevice,
            });
        }
        if let Some(validator) = &mut self.validator {
            validator.reset();
        }
        self.lifecycle = if self.resume_running {
            LifecycleState::Running
        } else {
            LifecycleState::Open
        };
        self.health = EndpointHealth::HEALTHY;
        Ok(())
    }

    fn try_receive(&mut self) -> Result<Option<MediaTransfer<M>>, IoError> {
        if self.lifecycle == LifecycleState::Lost {
            let policy = self
                .options
                .as_ref()
                .map_or(crate::SignalLossPolicy::Stop, |options| options.signal_loss);
            return match policy {
                crate::SignalLossPolicy::Hold => self
                    .last_media
                    .clone()
                    .map(|media| MediaTransfer::Fallback {
                        kind: FallbackKind::Hold,
                        media,
                    })
                    .map(Some)
                    .ok_or(IoError::SignalLost { policy }),
                crate::SignalLossPolicy::Slate => self
                    .slate
                    .clone()
                    .map(|media| MediaTransfer::Fallback {
                        kind: FallbackKind::Slate,
                        media,
                    })
                    .map(Some)
                    .ok_or(IoError::SignalLost { policy }),
                crate::SignalLossPolicy::Stop => Err(IoError::SignalLost { policy }),
            };
        }
        if self.lifecycle != LifecycleState::Running {
            return Err(invalid_state("receive", self.lifecycle));
        }
        let Some(media) = self.queue.pop_front() else {
            return Ok(None);
        };
        if let Some(validator) = &mut self.validator {
            validator
                .validate(media.timing())
                .map_err(IoError::MalformedTimestamp)?;
        }
        self.last_media = Some(media.clone());
        Ok(Some(MediaTransfer::Live(media)))
    }
}

#[derive(Debug)]
pub struct FakeSink<M> {
    descriptor: SinkDescriptor,
    lifecycle: LifecycleState,
    health: EndpointHealth,
    queue: VecDeque<M>,
    configured_capacity: usize,
    validator: Option<TimestampValidator>,
    options: Option<OpenOptions>,
    connected: bool,
    resume_running: bool,
    next_failure: Option<IoError>,
}

impl<M> FakeSink<M>
where
    M: MediaUnit,
{
    #[must_use]
    pub fn new(descriptor: SinkDescriptor) -> Self {
        Self {
            configured_capacity: descriptor.capabilities.transfer.queue_capacity.get(),
            descriptor,
            lifecycle: LifecycleState::Closed,
            health: EndpointHealth::HEALTHY,
            queue: VecDeque::new(),
            validator: None,
            options: None,
            connected: true,
            resume_running: false,
            next_failure: None,
        }
    }

    #[must_use]
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    pub fn consume_one(&mut self) -> Option<M> {
        self.queue.pop_front()
    }

    pub fn fail_next_send(&mut self, error: IoError) {
        self.next_failure = Some(error);
    }

    pub fn set_permission(&mut self, state: PermissionState) {
        self.descriptor.permission = state;
    }

    pub fn set_driver(&mut self, state: DriverState) {
        self.descriptor.driver = state;
    }

    pub fn unplug(&mut self) {
        self.connected = false;
        if self.lifecycle != LifecycleState::Closed {
            self.resume_running = self.lifecycle == LifecycleState::Running;
            self.lifecycle = LifecycleState::Lost;
            self.health = EndpointHealth {
                state: EndpointHealthState::Failed,
                detail: Some("sink was unplugged".to_owned()),
                remediation: Some(Remediation::ReconnectDevice),
            };
        }
    }

    pub fn plug_in(&mut self) {
        self.connected = true;
    }

    fn reject(media: M, error: IoError) -> WriteError<M> {
        WriteError::Rejected { media, error }
    }
}

impl<M> MediaSink for FakeSink<M>
where
    M: MediaUnit,
{
    type Media = M;

    fn descriptor(&self) -> &SinkDescriptor {
        &self.descriptor
    }

    fn lifecycle(&self) -> LifecycleState {
        self.lifecycle
    }

    fn health(&self) -> &EndpointHealth {
        &self.health
    }

    fn open(&mut self, options: OpenOptions) -> Result<(), IoError> {
        if self.lifecycle != LifecycleState::Closed {
            return Err(invalid_state("open", self.lifecycle));
        }
        if !self.connected {
            return Err(IoError::EndpointUnavailable {
                remediation: Remediation::ReconnectDevice,
            });
        }
        let clock = crate::contract::validate_open(
            &self.descriptor.capabilities,
            &self.descriptor.permission,
            &self.descriptor.driver,
            &options,
        )?;
        self.configured_capacity = options.queue_capacity.get();
        self.validator = Some(TimestampValidator::new(clock));
        self.options = Some(options);
        self.lifecycle = LifecycleState::Open;
        self.health = EndpointHealth::HEALTHY;
        Ok(())
    }

    fn start(&mut self) -> Result<(), IoError> {
        if self.lifecycle != LifecycleState::Open {
            return Err(invalid_state("start", self.lifecycle));
        }
        self.lifecycle = LifecycleState::Running;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), IoError> {
        if self.lifecycle != LifecycleState::Running {
            return Err(invalid_state("stop", self.lifecycle));
        }
        self.lifecycle = LifecycleState::Open;
        Ok(())
    }

    fn close(&mut self) -> Result<(), IoError> {
        if !matches!(self.lifecycle, LifecycleState::Open | LifecycleState::Lost) {
            return Err(invalid_state("close", self.lifecycle));
        }
        self.lifecycle = LifecycleState::Closed;
        self.validator = None;
        self.options = None;
        self.queue.clear();
        Ok(())
    }

    fn begin_recovery(&mut self) -> Result<(), IoError> {
        if self.lifecycle != LifecycleState::Lost {
            return Err(invalid_state("begin recovery", self.lifecycle));
        }
        self.lifecycle = LifecycleState::Recovering;
        Ok(())
    }

    fn finish_recovery(&mut self) -> Result<(), IoError> {
        if self.lifecycle != LifecycleState::Recovering {
            return Err(invalid_state("finish recovery", self.lifecycle));
        }
        if !self.connected {
            return Err(IoError::EndpointUnavailable {
                remediation: Remediation::ReconnectDevice,
            });
        }
        if let Some(validator) = &mut self.validator {
            validator.reset();
        }
        self.lifecycle = if self.resume_running {
            LifecycleState::Running
        } else {
            LifecycleState::Open
        };
        self.health = EndpointHealth::HEALTHY;
        Ok(())
    }

    fn try_send(&mut self, media: M) -> Result<(), WriteError<M>> {
        if self.lifecycle != LifecycleState::Running {
            return Err(Self::reject(media, invalid_state("send", self.lifecycle)));
        }
        if let Some(error) = self.next_failure.take() {
            return Err(Self::reject(media, error));
        }
        let actual = media.byte_len();
        let maximum = self.descriptor.capabilities.transfer.max_media_bytes.get();
        if actual > maximum {
            return Err(Self::reject(
                media,
                IoError::MediaTooLarge { actual, maximum },
            ));
        }
        if self.queue.len() >= self.configured_capacity {
            return Err(WriteError::QueueFull(media));
        }
        if let Some(validator) = &mut self.validator
            && let Err(error) = validator.validate(media.timing())
        {
            return Err(WriteError::Rejected {
                media,
                error: IoError::MalformedTimestamp(error),
            });
        }
        self.queue.push_back(media);
        Ok(())
    }
}

fn invalid_state(operation: &'static str, state: LifecycleState) -> IoError {
    IoError::InvalidState { operation, state }
}
