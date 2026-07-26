use fm_capabilities::{Health, Provider};
use fm_frame::EncodedPacket;

use crate::{
    CapabilityMismatch, CodecError, CodecProfile, DecodedFormat, DecodedFrame, EncodedFormat,
    MediaKind, QueueCapacity,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodecLifecycle {
    Ready,
    Draining,
    Ended,
    Failed,
}

/// Result of a push operation. Backpressure returns unconsumed input.
#[derive(Debug, PartialEq)]
pub enum SubmitStatus<T> {
    Accepted,
    Backpressure(T),
}

#[derive(Debug, PartialEq)]
pub enum OutputStatus<T> {
    Output(T),
    NeedInput,
    End,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyframeRequest {
    NextFrame,
    AtOrAfter(fm_frame::NormalizedTimestamp),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecoderCapability {
    codec: fm_frame::CodecId,
    media_kind: MediaKind,
    profiles: Vec<CodecProfile>,
    outputs: Vec<DecodedFormat>,
    maximum_queue_capacity: QueueCapacity,
    supports_reconfiguration: bool,
}

impl DecoderCapability {
    #[must_use]
    pub fn new(
        codec: fm_frame::CodecId,
        media_kind: MediaKind,
        outputs: Vec<DecodedFormat>,
        maximum_queue_capacity: QueueCapacity,
    ) -> Self {
        Self {
            codec,
            media_kind,
            profiles: Vec::new(),
            outputs,
            maximum_queue_capacity,
            supports_reconfiguration: false,
        }
    }

    #[must_use]
    pub fn with_profiles(mut self, profiles: Vec<CodecProfile>) -> Self {
        self.profiles = profiles;
        self
    }

    #[must_use]
    pub const fn with_reconfiguration(mut self, supported: bool) -> Self {
        self.supports_reconfiguration = supported;
        self
    }

    #[must_use]
    pub const fn codec(&self) -> &fm_frame::CodecId {
        &self.codec
    }

    #[must_use]
    pub const fn media_kind(&self) -> MediaKind {
        self.media_kind
    }

    #[must_use]
    pub fn profiles(&self) -> &[CodecProfile] {
        &self.profiles
    }

    #[must_use]
    pub fn outputs(&self) -> &[DecodedFormat] {
        &self.outputs
    }

    #[must_use]
    pub const fn maximum_queue_capacity(&self) -> QueueCapacity {
        self.maximum_queue_capacity
    }

    #[must_use]
    pub const fn supports_reconfiguration(&self) -> bool {
        self.supports_reconfiguration
    }

    #[must_use]
    pub fn supports(&self, config: &DecoderConfig) -> bool {
        self.codec == *config.input.codec()
            && self.media_kind == config.input.media_kind()
            && config.output.media_kind() == self.media_kind
            && (self.profiles.is_empty()
                || config
                    .input
                    .profile()
                    .is_some_and(|profile| self.profiles.contains(profile)))
            && self.outputs.contains(&config.output)
            && config.queue_capacity.get() <= self.maximum_queue_capacity.get()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncoderCapability {
    codec: fm_frame::CodecId,
    media_kind: MediaKind,
    profiles: Vec<CodecProfile>,
    inputs: Vec<DecodedFormat>,
    maximum_queue_capacity: QueueCapacity,
    supports_reconfiguration: bool,
    supports_keyframe_requests: bool,
}

impl EncoderCapability {
    #[must_use]
    pub fn new(
        codec: fm_frame::CodecId,
        media_kind: MediaKind,
        inputs: Vec<DecodedFormat>,
        maximum_queue_capacity: QueueCapacity,
    ) -> Self {
        Self {
            codec,
            media_kind,
            profiles: Vec::new(),
            inputs,
            maximum_queue_capacity,
            supports_reconfiguration: false,
            supports_keyframe_requests: false,
        }
    }

    #[must_use]
    pub fn with_profiles(mut self, profiles: Vec<CodecProfile>) -> Self {
        self.profiles = profiles;
        self
    }

    #[must_use]
    pub const fn with_reconfiguration(mut self, supported: bool) -> Self {
        self.supports_reconfiguration = supported;
        self
    }

    #[must_use]
    pub const fn with_keyframe_requests(mut self, supported: bool) -> Self {
        self.supports_keyframe_requests = supported;
        self
    }

    #[must_use]
    pub const fn codec(&self) -> &fm_frame::CodecId {
        &self.codec
    }

    #[must_use]
    pub const fn media_kind(&self) -> MediaKind {
        self.media_kind
    }

    #[must_use]
    pub fn profiles(&self) -> &[CodecProfile] {
        &self.profiles
    }

    #[must_use]
    pub fn inputs(&self) -> &[DecodedFormat] {
        &self.inputs
    }

    #[must_use]
    pub const fn maximum_queue_capacity(&self) -> QueueCapacity {
        self.maximum_queue_capacity
    }

    #[must_use]
    pub const fn supports_reconfiguration(&self) -> bool {
        self.supports_reconfiguration
    }

    #[must_use]
    pub const fn supports_keyframe_requests(&self) -> bool {
        self.supports_keyframe_requests
    }

    #[must_use]
    pub fn supports(&self, config: &EncoderConfig) -> bool {
        self.codec == *config.output.codec()
            && self.media_kind == config.output.media_kind()
            && config.input.media_kind() == self.media_kind
            && (self.profiles.is_empty()
                || config
                    .output
                    .profile()
                    .is_some_and(|profile| self.profiles.contains(profile)))
            && self.inputs.contains(&config.input)
            && config.queue_capacity.get() <= self.maximum_queue_capacity.get()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodecCapabilities {
    provider: Provider,
    health: Health,
    decoders: Vec<DecoderCapability>,
    encoders: Vec<EncoderCapability>,
}

impl CodecCapabilities {
    #[must_use]
    pub const fn new(provider: Provider) -> Self {
        Self {
            provider,
            health: Health::Healthy,
            decoders: Vec::new(),
            encoders: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_health(mut self, health: Health) -> Self {
        self.health = health;
        self
    }

    #[must_use]
    pub fn with_decoder(mut self, capability: DecoderCapability) -> Self {
        self.decoders.push(capability);
        self
    }

    #[must_use]
    pub fn with_encoder(mut self, capability: EncoderCapability) -> Self {
        self.encoders.push(capability);
        self
    }

    #[must_use]
    pub const fn provider(&self) -> &Provider {
        &self.provider
    }

    #[must_use]
    pub const fn health(&self) -> &Health {
        &self.health
    }

    #[must_use]
    pub fn decoders(&self) -> &[DecoderCapability] {
        &self.decoders
    }

    #[must_use]
    pub fn encoders(&self) -> &[EncoderCapability] {
        &self.encoders
    }

    #[must_use]
    pub fn supports_decoder(&self, config: &DecoderConfig) -> bool {
        self.health.is_usable() && self.decoders.iter().any(|item| item.supports(config))
    }

    #[must_use]
    pub fn supports_encoder(&self, config: &EncoderConfig) -> bool {
        self.health.is_usable() && self.encoders.iter().any(|item| item.supports(config))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecoderConfig {
    input: EncodedFormat,
    output: DecodedFormat,
    queue_capacity: QueueCapacity,
}

impl DecoderConfig {
    /// Creates a decoder configuration.
    ///
    /// # Errors
    ///
    /// Returns a media-kind mismatch before an adapter is opened.
    pub fn new(
        input: EncodedFormat,
        output: DecodedFormat,
        queue_capacity: QueueCapacity,
    ) -> Result<Self, CodecError> {
        if input.media_kind() != output.media_kind() {
            return Err(CapabilityMismatch::MediaKind.into());
        }
        Ok(Self {
            input,
            output,
            queue_capacity,
        })
    }

    #[must_use]
    pub const fn input(&self) -> &EncodedFormat {
        &self.input
    }

    #[must_use]
    pub const fn output(&self) -> &DecodedFormat {
        &self.output
    }

    #[must_use]
    pub const fn queue_capacity(&self) -> QueueCapacity {
        self.queue_capacity
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncoderConfig {
    input: DecodedFormat,
    output: EncodedFormat,
    queue_capacity: QueueCapacity,
}

impl EncoderConfig {
    /// Creates an encoder configuration.
    ///
    /// # Errors
    ///
    /// Returns a media-kind mismatch before an adapter is opened.
    pub fn new(
        input: DecodedFormat,
        output: EncodedFormat,
        queue_capacity: QueueCapacity,
    ) -> Result<Self, CodecError> {
        if input.media_kind() != output.media_kind() {
            return Err(CapabilityMismatch::MediaKind.into());
        }
        Ok(Self {
            input,
            output,
            queue_capacity,
        })
    }

    #[must_use]
    pub const fn input(&self) -> &DecodedFormat {
        &self.input
    }

    #[must_use]
    pub const fn output(&self) -> &EncodedFormat {
        &self.output
    }

    #[must_use]
    pub const fn queue_capacity(&self) -> QueueCapacity {
        self.queue_capacity
    }
}

pub trait Decoder: Send {
    fn config(&self) -> &DecoderConfig;
    fn state(&self) -> CodecLifecycle;

    /// Submits one packet or returns it unchanged under backpressure.
    ///
    /// # Errors
    ///
    /// Returns a typed state, stream, timestamp, or adapter error.
    fn submit_packet(
        &mut self,
        packet: EncodedPacket,
    ) -> Result<SubmitStatus<EncodedPacket>, CodecError>;

    /// Polls the next decoded frame without blocking.
    ///
    /// # Errors
    ///
    /// Returns a typed receive or adapter error.
    fn receive_frame(&mut self) -> Result<OutputStatus<DecodedFrame>, CodecError>;

    /// Declares that no more packets will be submitted before a flush.
    ///
    /// # Errors
    ///
    /// Returns an invalid-state or adapter error.
    fn drain(&mut self) -> Result<(), CodecError>;

    /// Discards queued work and returns the decoder to the ready state.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when reset cannot be completed.
    fn flush(&mut self) -> Result<(), CodecError>;

    /// Requests an atomic format change.
    ///
    /// # Errors
    ///
    /// Returns [`CodecErrorKind::ReconfigureRejected`] when unsupported, or a
    /// typed capability, state, or adapter error.
    fn reconfigure(&mut self, config: DecoderConfig) -> Result<(), CodecError>;
}

pub trait Encoder: Send {
    fn config(&self) -> &EncoderConfig;
    fn state(&self) -> CodecLifecycle;

    /// Submits one frame or returns it unchanged under backpressure.
    ///
    /// # Errors
    ///
    /// Returns a typed state, format, timestamp, or adapter error.
    fn submit_frame(
        &mut self,
        frame: DecodedFrame,
    ) -> Result<SubmitStatus<DecodedFrame>, CodecError>;

    /// Polls the next compressed packet without blocking.
    ///
    /// # Errors
    ///
    /// Returns a typed receive or adapter error.
    fn receive_packet(&mut self) -> Result<OutputStatus<EncodedPacket>, CodecError>;

    /// Declares that no more frames will be submitted before a flush.
    ///
    /// # Errors
    ///
    /// Returns an invalid-state or adapter error.
    fn drain(&mut self) -> Result<(), CodecError>;

    /// Discards queued work and returns the encoder to the ready state.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when reset cannot be completed.
    fn flush(&mut self) -> Result<(), CodecError>;

    /// Requests a random-access output packet.
    ///
    /// # Errors
    ///
    /// Returns a capability, state, or adapter error.
    fn request_keyframe(&mut self, request: KeyframeRequest) -> Result<(), CodecError>;

    /// Requests an atomic format change.
    ///
    /// # Errors
    ///
    /// Returns [`CodecErrorKind::ReconfigureRejected`] when unsupported, or a
    /// typed capability, state, or adapter error.
    fn reconfigure(&mut self, config: EncoderConfig) -> Result<(), CodecError>;
}

pub trait CodecProvider: Send + Sync {
    fn capabilities(&self) -> &CodecCapabilities;

    /// Opens a decoder after matching the requested configuration.
    ///
    /// # Errors
    ///
    /// Returns a capability mismatch or adapter creation error.
    fn create_decoder(&self, config: DecoderConfig) -> Result<Box<dyn Decoder>, CodecError>;

    /// Opens an encoder after matching the requested configuration.
    ///
    /// # Errors
    ///
    /// Returns a capability mismatch or adapter creation error.
    fn create_encoder(&self, config: EncoderConfig) -> Result<Box<dyn Encoder>, CodecError>;
}
