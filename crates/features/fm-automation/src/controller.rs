use crate::CommandIntent;
use core::fmt;
use fm_types::{InputId, MediaTimestamp};
use std::collections::{BTreeSet, HashMap};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ControlAddress {
    pub device: String,
    pub control: String,
}

impl ControlAddress {
    #[must_use]
    pub fn new(device: impl Into<String>, control: impl Into<String>) -> Self {
        Self {
            device: device.into(),
            control: control.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ValueRange {
    pub minimum: f64,
    pub maximum: f64,
}

impl ValueRange {
    /// Creates a finite, increasing value range.
    ///
    /// # Errors
    ///
    /// Returns an error for non-finite or non-increasing bounds.
    pub fn new(minimum: f64, maximum: f64) -> Result<Self, ControllerError> {
        if !minimum.is_finite() || !maximum.is_finite() {
            return Err(ControllerError::NonFiniteValue);
        }
        if minimum >= maximum {
            return Err(ControllerError::InvalidRange);
        }
        Ok(Self { minimum, maximum })
    }

    fn normalize(self, value: f64) -> f64 {
        ((value - self.minimum) / (self.maximum - self.minimum)).clamp(0.0, 1.0)
    }

    fn denormalize(self, value: f64) -> f64 {
        self.minimum + value * (self.maximum - self.minimum)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ControlMode {
    Button { threshold: f64 },
    Continuous,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Mapping {
    pub id: String,
    pub address: ControlAddress,
    pub target: String,
    pub input_range: ValueRange,
    pub output_range: ValueRange,
    /// Optional raw velocity range. Mapped velocity is normalized to `0..=1`.
    pub velocity_range: Option<ValueRange>,
    pub mode: ControlMode,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LearnRequest {
    pub mapping_id: String,
    pub target: String,
    pub input_range: ValueRange,
    pub output_range: ValueRange,
    pub velocity_range: Option<ValueRange>,
    pub mode: ControlMode,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ControllerInput<'a> {
    pub address: &'a ControlAddress,
    pub value: f64,
    pub velocity: Option<f64>,
    pub timestamp: MediaTimestamp,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MappedControllerIntent {
    pub mapping_id: String,
    pub target: String,
    pub value: f64,
    pub velocity: Option<f64>,
    pub timestamp: MediaTimestamp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceState {
    pub connected: bool,
    pub generation: u64,
    pub last_seen: MediaTimestamp,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ControllerError {
    NonFiniteValue,
    InvalidRange,
    DuplicateMapping(String),
    UnknownDevice(String),
    DeviceDisconnected(String),
    AlreadyLearning,
    GenerationExhausted,
}

impl fmt::Display for ControllerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFiniteValue => formatter.write_str("controller value must be finite"),
            Self::InvalidRange => formatter.write_str("controller range must be increasing"),
            Self::DuplicateMapping(id) => write!(formatter, "controller mapping {id} exists"),
            Self::UnknownDevice(device) => write!(formatter, "controller {device} is unknown"),
            Self::DeviceDisconnected(device) => {
                write!(formatter, "controller {device} is disconnected")
            }
            Self::AlreadyLearning => formatter.write_str("controller learn is already active"),
            Self::GenerationExhausted => {
                formatter.write_str("controller connection generation exhausted")
            }
        }
    }
}

impl std::error::Error for ControllerError {}

#[derive(Clone, Debug, Default)]
pub struct ControllerManager {
    devices: HashMap<String, DeviceState>,
    mappings: Vec<Mapping>,
    previous_values: HashMap<ControlAddress, f64>,
    learn: Option<LearnRequest>,
    learned: Option<Mapping>,
}

impl ControllerManager {
    /// Marks a device connected at a caller timestamp. Reconnect increments its
    /// generation and clears transient button state while retaining mappings.
    ///
    /// # Errors
    ///
    /// Returns an error if the reconnect generation is exhausted.
    pub fn connect(
        &mut self,
        device: impl Into<String>,
        timestamp: MediaTimestamp,
    ) -> Result<DeviceState, ControllerError> {
        let device = device.into();
        let reset_transient = self
            .devices
            .get(&device)
            .is_none_or(|state| !state.connected);
        let generation = self.devices.get(&device).map_or(Ok(1), |state| {
            if state.connected {
                Ok(state.generation)
            } else {
                state
                    .generation
                    .checked_add(1)
                    .ok_or(ControllerError::GenerationExhausted)
            }
        })?;
        if reset_transient {
            self.previous_values
                .retain(|address, _| address.device != device);
        }
        let state = DeviceState {
            connected: true,
            generation,
            last_seen: timestamp,
        };
        self.devices.insert(device, state);
        Ok(state)
    }

    /// Marks a known controller disconnected at a caller timestamp.
    ///
    /// # Errors
    ///
    /// Returns an error when the device has never connected.
    pub fn disconnect(
        &mut self,
        device: &str,
        timestamp: MediaTimestamp,
    ) -> Result<DeviceState, ControllerError> {
        let state = self
            .devices
            .get_mut(device)
            .ok_or_else(|| ControllerError::UnknownDevice(device.to_owned()))?;
        state.connected = false;
        state.last_seen = timestamp;
        self.previous_values
            .retain(|address, _| address.device != device);
        Ok(*state)
    }

    #[must_use]
    pub fn device_state(&self, device: &str) -> Option<DeviceState> {
        self.devices.get(device).copied()
    }

    #[must_use]
    pub fn mappings(&self) -> &[Mapping] {
        &self.mappings
    }

    /// Adds a declarative mapping; no device access is performed.
    ///
    /// # Errors
    ///
    /// Returns an error when its stable identifier is already present.
    pub fn add_mapping(&mut self, mapping: Mapping) -> Result<(), ControllerError> {
        if self.mappings.iter().any(|entry| entry.id == mapping.id) {
            return Err(ControllerError::DuplicateMapping(mapping.id));
        }
        self.mappings.push(mapping);
        Ok(())
    }

    /// Arms a one-sample learn operation.
    ///
    /// # Errors
    ///
    /// Returns an error when learn is already armed or the mapping ID exists.
    pub fn begin_learn(&mut self, request: LearnRequest) -> Result<(), ControllerError> {
        if self.learn.is_some() {
            return Err(ControllerError::AlreadyLearning);
        }
        if self
            .mappings
            .iter()
            .any(|entry| entry.id == request.mapping_id)
        {
            return Err(ControllerError::DuplicateMapping(request.mapping_id));
        }
        self.learn = Some(request);
        Ok(())
    }

    pub fn cancel_learn(&mut self) -> Option<LearnRequest> {
        self.learn.take()
    }

    pub fn take_learned(&mut self) -> Option<Mapping> {
        self.learned.take()
    }

    /// Maps a normalized controller sample into command intents. Button
    /// mappings emit only on a rising edge; continuous mappings are explicitly
    /// coalescible by target.
    ///
    /// # Errors
    ///
    /// Returns an error for non-finite input or disconnected devices.
    pub fn map_input(
        &mut self,
        input: ControllerInput<'_>,
    ) -> Result<Vec<CommandIntent<MappedControllerIntent>>, ControllerError> {
        if !input.value.is_finite() || input.velocity.is_some_and(|value| !value.is_finite()) {
            return Err(ControllerError::NonFiniteValue);
        }
        let device = self
            .devices
            .get_mut(&input.address.device)
            .ok_or_else(|| ControllerError::UnknownDevice(input.address.device.clone()))?;
        if !device.connected {
            return Err(ControllerError::DeviceDisconnected(
                input.address.device.clone(),
            ));
        }
        device.last_seen = input.timestamp;

        if let Some(request) = self.learn.take() {
            let mapping = Mapping {
                id: request.mapping_id,
                address: input.address.clone(),
                target: request.target,
                input_range: request.input_range,
                output_range: request.output_range,
                velocity_range: request.velocity_range,
                mode: request.mode,
            };
            self.mappings.push(mapping.clone());
            self.learned = Some(mapping);
        }

        let previous = self
            .previous_values
            .insert(input.address.clone(), input.value);
        Ok(self
            .mappings
            .iter()
            .filter(|mapping| mapping.address == *input.address)
            .filter_map(|mapping| {
                let normalized = mapping.input_range.normalize(input.value);
                let value = mapping.output_range.denormalize(normalized);
                let velocity = input.velocity.map(|velocity| {
                    mapping
                        .velocity_range
                        .map_or(velocity, |range| range.normalize(velocity))
                });
                let mapped = MappedControllerIntent {
                    mapping_id: mapping.id.clone(),
                    target: mapping.target.clone(),
                    value,
                    velocity,
                    timestamp: input.timestamp,
                };
                match mapping.mode {
                    ControlMode::Continuous => {
                        Some(CommandIntent::continuous(mapping.target.clone(), mapped))
                    }
                    ControlMode::Button { threshold }
                        if input.value >= threshold
                            && previous.is_none_or(|value| value < threshold) =>
                    {
                        Some(CommandIntent::discrete(mapped))
                    }
                    ControlMode::Button { .. } => None,
                }
            })
            .collect())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TallySnapshot {
    pub program: Option<InputId>,
    pub preview: Option<InputId>,
    pub overlays: BTreeSet<InputId>,
    pub audio_active: BTreeSet<InputId>,
    pub recording: bool,
    pub streaming: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivatorRule {
    Program(InputId),
    Preview(InputId),
    Overlay(InputId),
    AudioActive(InputId),
    Recording,
    Streaming,
}

impl ActivatorRule {
    fn active(self, tally: &TallySnapshot) -> bool {
        match self {
            Self::Program(input) => tally.program == Some(input),
            Self::Preview(input) => tally.preview == Some(input),
            Self::Overlay(input) => tally.overlays.contains(&input),
            Self::AudioActive(input) => tally.audio_active.contains(&input),
            Self::Recording => tally.recording,
            Self::Streaming => tally.streaming,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActivatorMapping {
    pub address: ControlAddress,
    pub rule: ActivatorRule,
    pub on_value: f64,
    pub off_value: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ControllerFeedback {
    pub address: ControlAddress,
    pub value: f64,
}

#[derive(Clone, Debug, Default)]
pub struct ActivatorEngine {
    mappings: Vec<ActivatorMapping>,
    last_values: HashMap<ControlAddress, f64>,
}

impl ActivatorEngine {
    pub fn add(&mut self, mapping: ActivatorMapping) {
        self.mappings.push(mapping);
    }

    /// Derives changed activator/tally values from observed state.
    #[must_use]
    pub fn derive(&mut self, tally: &TallySnapshot) -> Vec<ControllerFeedback> {
        let mut feedback = Vec::new();
        for mapping in &self.mappings {
            let value = if mapping.rule.active(tally) {
                mapping.on_value
            } else {
                mapping.off_value
            };
            if self.last_values.get(&mapping.address) != Some(&value) {
                self.last_values.insert(mapping.address.clone(), value);
                feedback.push(ControllerFeedback {
                    address: mapping.address.clone(),
                    value,
                });
            }
        }
        feedback
    }

    /// Invalidates cached feedback so a reconnected adapter receives a full
    /// state derivation on the next call to [`Self::derive`].
    pub fn reconnect(&mut self, device: &str) {
        self.last_values
            .retain(|address, _| address.device != device);
    }
}
