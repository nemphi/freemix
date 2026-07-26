//! Deterministic, deadline-bound sample-block DSP host coordination.
//!
//! This crate models the coordination boundary around DSP plugins. It does not
//! discover or load native VST, Audio Unit, or LV2 binaries. Audio is
//! interleaved `f32`, and configured hosts retain exactly one block of latency.

use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

/// Maximum accepted channel count.
pub const MAX_CHANNELS: usize = 64;
/// Maximum accepted frame count in one block.
pub const MAX_BLOCK_FRAMES: usize = 65_536;
/// Maximum number of plugins in one chain.
pub const MAX_CHAIN_PLUGINS: usize = 128;
/// Maximum delay retained by the deterministic fake delay plugin.
pub const MAX_DELAY_FRAMES: usize = MAX_BLOCK_FRAMES;
/// The fixed number of preallocated pipeline slots.
pub const PIPELINE_RING_BLOCKS: usize = 2;

/// Stable identity of a plugin instance.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PluginId(u64);

impl PluginId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Stable identity of an ordered plugin chain.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ChainId(u64);

impl ChainId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ChainId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Plugin implementation family exposed by scan metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginKind {
    Gain,
    Delay,
    Crash,
    Other,
}

/// Immutable metadata recorded by a plugin scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PluginMetadata {
    pub id: PluginId,
    pub name: &'static str,
    pub vendor: &'static str,
    pub version: u32,
    pub kind: PluginKind,
}

/// Failure reported by the bounded metadata scanner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanError {
    InvalidCapacity,
    NotScanning,
    CapacityExceeded { capacity: usize },
    DuplicatePlugin(PluginId),
}

impl fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCapacity => formatter.write_str("scan capacity must be nonzero"),
            Self::NotScanning => formatter.write_str("no plugin scan is in progress"),
            Self::CapacityExceeded { capacity } => {
                write!(formatter, "plugin scan exceeds its capacity of {capacity}")
            }
            Self::DuplicatePlugin(id) => write!(formatter, "duplicate scanned plugin {id}"),
        }
    }
}

impl std::error::Error for ScanError {}

/// Observable scanner lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanState {
    Idle,
    Scanning,
    Ready { generation: u64, plugins: usize },
    Failed(ScanError),
}

/// Bounded metadata catalog. Scanning never loads executable plugin code.
#[derive(Clone, Debug)]
pub struct PluginScanner {
    state: ScanState,
    generation: u64,
    metadata: Vec<PluginMetadata>,
}

impl PluginScanner {
    /// Creates a scanner with a fixed metadata capacity.
    ///
    /// # Errors
    ///
    /// Returns [`ScanError::InvalidCapacity`] for zero or excessive capacity.
    pub fn new(capacity: usize) -> Result<Self, ScanError> {
        if capacity == 0 || capacity > MAX_CHAIN_PLUGINS {
            return Err(ScanError::InvalidCapacity);
        }
        Ok(Self {
            state: ScanState::Idle,
            generation: 0,
            metadata: Vec::with_capacity(capacity),
        })
    }

    /// Starts a metadata-only scan and discards the previous result.
    pub fn begin_scan(&mut self) {
        self.metadata.clear();
        self.state = ScanState::Scanning;
    }

    /// Records one discovered metadata item.
    ///
    /// # Errors
    ///
    /// Returns an error when no scan is active, the catalog is full, or the ID
    /// was already recorded. A discovery error ends the scan.
    pub fn record(&mut self, metadata: PluginMetadata) -> Result<(), ScanError> {
        if self.state != ScanState::Scanning {
            return Err(ScanError::NotScanning);
        }
        let error = if self.metadata.len() == self.metadata.capacity() {
            Some(ScanError::CapacityExceeded {
                capacity: self.metadata.capacity(),
            })
        } else if self.metadata.iter().any(|item| item.id == metadata.id) {
            Some(ScanError::DuplicatePlugin(metadata.id))
        } else {
            None
        };
        if let Some(error) = error {
            self.metadata.clear();
            self.state = ScanState::Failed(error);
            return Err(error);
        }
        self.metadata.push(metadata);
        Ok(())
    }

    /// Completes the active scan.
    ///
    /// # Errors
    ///
    /// Returns [`ScanError::NotScanning`] when no scan is active.
    pub fn finish_scan(&mut self) -> Result<&[PluginMetadata], ScanError> {
        if self.state != ScanState::Scanning {
            return Err(ScanError::NotScanning);
        }
        self.generation = self.generation.saturating_add(1);
        self.state = ScanState::Ready {
            generation: self.generation,
            plugins: self.metadata.len(),
        };
        Ok(&self.metadata)
    }

    /// Marks the active scan as failed and clears partial metadata.
    pub fn fail_scan(&mut self, error: ScanError) {
        self.metadata.clear();
        self.state = ScanState::Failed(error);
    }

    #[must_use]
    pub const fn state(&self) -> ScanState {
        self.state
    }

    #[must_use]
    pub fn metadata(&self) -> &[PluginMetadata] {
        &self.metadata
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.metadata.capacity()
    }
}

/// Fixed interleaved sample-block format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockFormat {
    pub sample_rate_hz: u32,
    pub channels: usize,
    pub frames: usize,
}

impl BlockFormat {
    /// Creates and validates a block format.
    ///
    /// # Errors
    ///
    /// Returns a typed error for zero or unsupported dimensions.
    pub fn new(sample_rate_hz: u32, channels: usize, frames: usize) -> Result<Self, ConfigError> {
        let format = Self {
            sample_rate_hz,
            channels,
            frames,
        };
        format.validate()?;
        Ok(format)
    }

    #[must_use]
    pub fn samples(self) -> usize {
        self.channels * self.frames
    }

    fn validate(self) -> Result<(), ConfigError> {
        if self.sample_rate_hz == 0 {
            return Err(ConfigError::InvalidSampleRate);
        }
        if !(1..=MAX_CHANNELS).contains(&self.channels) {
            return Err(ConfigError::InvalidChannelCount(self.channels));
        }
        if !(1..=MAX_BLOCK_FRAMES).contains(&self.frames) {
            return Err(ConfigError::InvalidBlockFrames(self.frames));
        }
        self.channels
            .checked_mul(self.frames)
            .ok_or(ConfigError::SampleCountOverflow)?;
        Ok(())
    }
}

/// Host deadline and bypass policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostConfig {
    pub format: BlockFormat,
    pub deadline: Duration,
    pub misses_before_bypass: u32,
    pub bypass_ramp_frames: usize,
}

impl HostConfig {
    #[must_use]
    pub const fn new(format: BlockFormat, deadline: Duration) -> Self {
        Self {
            format,
            deadline,
            misses_before_bypass: 1,
            bypass_ramp_frames: 64,
        }
    }

    fn validate(self) -> Result<(), ConfigError> {
        self.format.validate()?;
        if self.deadline.is_zero() {
            return Err(ConfigError::ZeroDeadline);
        }
        if self.misses_before_bypass == 0 {
            return Err(ConfigError::ZeroMissThreshold);
        }
        if self.bypass_ramp_frames > MAX_BLOCK_FRAMES {
            return Err(ConfigError::RampTooLong(self.bypass_ramp_frames));
        }
        Ok(())
    }
}

/// Host or plugin configuration failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigError {
    InvalidSampleRate,
    InvalidChannelCount(usize),
    InvalidBlockFrames(usize),
    SampleCountOverflow,
    ZeroDeadline,
    ZeroMissThreshold,
    RampTooLong(usize),
    EmptyChain,
    TooManyPlugins(usize),
    DuplicatePlugin(PluginId),
    Plugin { id: PluginId, error: PluginError },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSampleRate => formatter.write_str("sample rate must be nonzero"),
            Self::InvalidChannelCount(count) => write!(
                formatter,
                "channel count {count} is outside 1..={MAX_CHANNELS}"
            ),
            Self::InvalidBlockFrames(frames) => write!(
                formatter,
                "block frame count {frames} is outside 1..={MAX_BLOCK_FRAMES}"
            ),
            Self::SampleCountOverflow => formatter.write_str("block sample count overflowed"),
            Self::ZeroDeadline => formatter.write_str("block deadline must be nonzero"),
            Self::ZeroMissThreshold => {
                formatter.write_str("deadline miss threshold must be nonzero")
            }
            Self::RampTooLong(frames) => write!(formatter, "bypass ramp of {frames} is too long"),
            Self::EmptyChain => formatter.write_str("DSP chain must contain a plugin"),
            Self::TooManyPlugins(count) => write!(
                formatter,
                "DSP chain contains {count} plugins; maximum is {MAX_CHAIN_PLUGINS}"
            ),
            Self::DuplicatePlugin(id) => write!(formatter, "duplicate plugin instance {id}"),
            Self::Plugin { id, error } => {
                write!(formatter, "plugin {id} could not be configured: {error}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Cooperative plugin error. Panics are separately caught and treated as a crash.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PluginError {
    Crashed,
    NotConfigured,
    InvalidParameter(&'static str),
    InvalidState,
}

impl fmt::Display for PluginError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Crashed => formatter.write_str("plugin crashed"),
            Self::NotConfigured => formatter.write_str("plugin is not configured"),
            Self::InvalidParameter(parameter) => write!(formatter, "invalid {parameter}"),
            Self::InvalidState => formatter.write_str("plugin state is invalid"),
        }
    }
}

impl std::error::Error for PluginError {}

/// In-process deterministic DSP contract used by the coordination model.
pub trait DspPlugin: Send {
    fn metadata(&self) -> PluginMetadata;

    /// Allocates any plugin-owned realtime storage for the fixed format.
    ///
    /// # Errors
    ///
    /// Returns an implementation-defined error when the format is unsupported.
    fn configure(&mut self, format: BlockFormat) -> Result<(), PluginError>;

    /// Deterministic cost charged before processing this block.
    fn processing_cost(&self) -> Duration {
        Duration::ZERO
    }

    /// Processes one exact-format interleaved block in place.
    ///
    /// # Errors
    ///
    /// Returns an implementation-defined processing or lifecycle error.
    fn process(&mut self, block: &mut [f32]) -> Result<(), PluginError>;

    /// Returns an opaque versioned state payload.
    fn save_state(&self) -> Vec<u8>;

    /// Validates state without changing the plugin.
    ///
    /// # Errors
    ///
    /// Returns an error when the payload cannot be restored by this plugin.
    fn validate_state(&self, state: &[u8]) -> Result<(), PluginError>;

    /// Restores state previously accepted by [`DspPlugin::validate_state`].
    ///
    /// # Errors
    ///
    /// Returns an error when the payload is invalid for this plugin.
    fn restore_state(&mut self, state: &[u8]) -> Result<(), PluginError>;

    /// Observable plugin-owned realtime sample capacity.
    fn realtime_capacity_samples(&self) -> usize {
        0
    }
}

/// Why the host's wet path is being bypassed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BypassReason {
    Manual,
    Deadline,
}

/// Persistent plugin health after failure substitution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginHealth {
    Active,
    ManuallyBypassed,
    Crashed,
    TimedOut,
}

/// Snapshot of one plugin's host-side runtime status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PluginStatus {
    pub id: PluginId,
    pub health: PluginHealth,
    pub substitutions: u64,
}

struct PluginNode {
    plugin: Box<dyn DspPlugin>,
    status: PluginStatus,
    manually_bypassed: bool,
}

impl PluginNode {
    fn is_runnable(&self) -> bool {
        self.status.health == PluginHealth::Active && !self.manually_bypassed
    }

    fn public_status(&self) -> PluginStatus {
        let health = if self.manually_bypassed && self.status.health == PluginHealth::Active {
            PluginHealth::ManuallyBypassed
        } else {
            self.status.health
        };
        PluginStatus {
            health,
            ..self.status
        }
    }
}

#[derive(Clone, Debug)]
struct PipelineSlot {
    dry: Vec<f32>,
    wet: Vec<f32>,
    sequence: Option<u64>,
}

impl PipelineSlot {
    fn new(samples: usize) -> Self {
        Self {
            dry: vec![0.0; samples],
            wet: vec![0.0; samples],
            sequence: None,
        }
    }

    fn clear(&mut self) {
        self.dry.fill(0.0);
        self.wet.fill(0.0);
        self.sequence = None;
    }
}

#[derive(Clone, Copy, Debug)]
struct WetRamp {
    current: f32,
    target: f32,
    remaining: usize,
}

impl WetRamp {
    const fn new() -> Self {
        Self {
            current: 1.0,
            target: 1.0,
            remaining: 0,
        }
    }

    fn set_target(&mut self, target: f32, frames: usize) {
        if self.target.to_bits() == target.to_bits() {
            return;
        }
        self.target = target;
        self.remaining = frames;
        if frames == 0 {
            self.current = target;
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn next(&mut self) -> f32 {
        if self.remaining != 0 {
            self.current += (self.target - self.current) / self.remaining as f32;
            self.remaining -= 1;
            if self.remaining == 0 {
                self.current = self.target;
            }
        }
        self.current
    }
}

/// Observable capacities which must remain stable during block processing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapacitySnapshot {
    pub chain_slots: usize,
    pub ring_slots: usize,
    pub samples_per_ring_buffer: usize,
    pub scratch_samples: usize,
    pub output_samples: usize,
    pub plugin_realtime_samples: usize,
}

/// Accounting result for one submitted block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockReport {
    pub input_sequence: u64,
    pub output_sequence: Option<u64>,
    pub elapsed: Duration,
    pub deadline: Duration,
    pub deadline_missed: bool,
    pub consecutive_deadline_misses: u32,
    pub total_deadline_misses: u64,
    pub substitutions: u32,
    pub bypass_reason: Option<BypassReason>,
}

/// Borrowed output and accounting for one processing call.
#[derive(Debug)]
pub struct ProcessedBlock<'a> {
    pub samples: &'a [f32],
    pub report: BlockReport,
}

/// Block submission failure. No host state changes on format validation errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessError {
    WrongSampleCount { expected: usize, actual: usize },
    NonFiniteSample { index: usize },
}

impl fmt::Display for ProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongSampleCount { expected, actual } => {
                write!(formatter, "received {actual} samples; expected {expected}")
            }
            Self::NonFiniteSample { index } => {
                write!(formatter, "sample {index} is not finite")
            }
        }
    }
}

impl std::error::Error for ProcessError {}

/// Opaque state for one plugin instance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SavedPluginState {
    pub id: PluginId,
    pub version: u32,
    pub manually_bypassed: bool,
    pub payload: Vec<u8>,
}

/// Versioned state for a configured chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SavedHostState {
    pub chain_id: ChainId,
    pub format: BlockFormat,
    pub manual_bypass: bool,
    pub plugins: Vec<SavedPluginState>,
}

/// Atomic state-restore validation failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StateError {
    ChainMismatch {
        expected: ChainId,
        actual: ChainId,
    },
    FormatMismatch,
    PluginCountMismatch {
        expected: usize,
        actual: usize,
    },
    PluginMismatch {
        index: usize,
        expected: PluginId,
        actual: PluginId,
    },
    VersionMismatch {
        id: PluginId,
        saved: u32,
        current: u32,
    },
    InvalidPluginState {
        id: PluginId,
        error: PluginError,
    },
}

impl fmt::Display for StateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChainMismatch { expected, actual } => {
                write!(formatter, "state chain {actual} does not match {expected}")
            }
            Self::FormatMismatch => formatter.write_str("state block format does not match"),
            Self::PluginCountMismatch { expected, actual } => {
                write!(formatter, "state has {actual} plugins; expected {expected}")
            }
            Self::PluginMismatch {
                index,
                expected,
                actual,
            } => write!(
                formatter,
                "state plugin {actual} at index {index} does not match {expected}"
            ),
            Self::VersionMismatch { id, saved, current } => write!(
                formatter,
                "state version {saved} for plugin {id} does not match {current}"
            ),
            Self::InvalidPluginState { id, error } => {
                write!(formatter, "invalid state for plugin {id}: {error}")
            }
        }
    }
}

impl std::error::Error for StateError {}

/// Fixed-format, one-block-latency DSP coordinator.
pub struct DspHost {
    chain_id: ChainId,
    config: HostConfig,
    plugins: Vec<PluginNode>,
    ring: Vec<PipelineSlot>,
    write_slot: usize,
    scratch: Vec<f32>,
    output: Vec<f32>,
    ramp: WetRamp,
    manual_bypass: bool,
    deadline_bypass: bool,
    input_sequence: u64,
    consecutive_deadline_misses: u32,
    total_deadline_misses: u64,
}

impl DspHost {
    /// Configures the complete chain and preallocates all host realtime storage.
    ///
    /// Plugins are processed in the exact order supplied. Configuration cannot
    /// be changed after construction.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid host format or policy, an empty,
    /// duplicate, or oversized chain, or a plugin configuration failure.
    pub fn configure(
        chain_id: ChainId,
        config: HostConfig,
        plugins: Vec<Box<dyn DspPlugin>>,
    ) -> Result<Self, ConfigError> {
        config.validate()?;
        if plugins.is_empty() {
            return Err(ConfigError::EmptyChain);
        }
        if plugins.len() > MAX_CHAIN_PLUGINS {
            return Err(ConfigError::TooManyPlugins(plugins.len()));
        }
        for (index, plugin) in plugins.iter().enumerate() {
            let id = plugin.metadata().id;
            if plugins[..index]
                .iter()
                .any(|prior| prior.metadata().id == id)
            {
                return Err(ConfigError::DuplicatePlugin(id));
            }
        }

        let mut nodes = Vec::with_capacity(plugins.len());
        for mut plugin in plugins {
            let id = plugin.metadata().id;
            plugin
                .configure(config.format)
                .map_err(|error| ConfigError::Plugin { id, error })?;
            nodes.push(PluginNode {
                plugin,
                status: PluginStatus {
                    id,
                    health: PluginHealth::Active,
                    substitutions: 0,
                },
                manually_bypassed: false,
            });
        }

        let samples = config.format.samples();
        let mut ring = Vec::with_capacity(PIPELINE_RING_BLOCKS);
        for _ in 0..PIPELINE_RING_BLOCKS {
            ring.push(PipelineSlot::new(samples));
        }
        Ok(Self {
            chain_id,
            config,
            plugins: nodes,
            ring,
            write_slot: 0,
            scratch: vec![0.0; samples],
            output: vec![0.0; samples],
            ramp: WetRamp::new(),
            manual_bypass: false,
            deadline_bypass: false,
            input_sequence: 0,
            consecutive_deadline_misses: 0,
            total_deadline_misses: 0,
        })
    }

    #[must_use]
    pub const fn chain_id(&self) -> ChainId {
        self.chain_id
    }

    #[must_use]
    pub const fn config(&self) -> HostConfig {
        self.config
    }

    /// Returns plugin instance IDs in processing order.
    #[must_use]
    pub fn plugin_ids(&self) -> impl ExactSizeIterator<Item = PluginId> + '_ {
        self.plugins.iter().map(|node| node.status.id)
    }

    #[must_use]
    pub fn plugin_status(&self, id: PluginId) -> Option<PluginStatus> {
        self.plugins
            .iter()
            .find(|node| node.status.id == id)
            .map(PluginNode::public_status)
    }

    /// Returns current runtime statuses in processing order.
    #[must_use]
    pub fn plugin_statuses(&self) -> impl ExactSizeIterator<Item = PluginStatus> + '_ {
        self.plugins.iter().map(PluginNode::public_status)
    }

    /// Bypasses one plugin without changing chain order.
    ///
    /// # Errors
    ///
    /// Returns [`PluginControlError::UnknownPlugin`] for an absent ID.
    pub fn set_plugin_bypass(
        &mut self,
        id: PluginId,
        bypassed: bool,
    ) -> Result<(), PluginControlError> {
        let node = self
            .plugins
            .iter_mut()
            .find(|node| node.status.id == id)
            .ok_or(PluginControlError::UnknownPlugin(id))?;
        node.manually_bypassed = bypassed;
        Ok(())
    }

    /// Schedules a click-free wet-to-dry or dry-to-wet host bypass ramp.
    pub fn set_bypass(&mut self, bypassed: bool) {
        self.manual_bypass = bypassed;
        self.update_ramp_target();
    }

    /// Clears sticky deadline bypass and schedules a return to the wet path.
    pub fn clear_deadline_bypass(&mut self) {
        self.deadline_bypass = false;
        self.consecutive_deadline_misses = 0;
        self.update_ramp_target();
    }

    #[must_use]
    pub const fn bypass_reason(&self) -> Option<BypassReason> {
        if self.manual_bypass {
            Some(BypassReason::Manual)
        } else if self.deadline_bypass {
            Some(BypassReason::Deadline)
        } else {
            None
        }
    }

    #[must_use]
    pub const fn current_wet_gain(&self) -> f32 {
        self.ramp.current
    }

    /// Processes one exact-format block and emits the previous block.
    ///
    /// The first call emits silence. Plugin crashes and deadline timeouts restore
    /// the signal at that plugin's input and continue the remaining chain.
    /// Processing uses only storage allocated by [`DspHost::configure`].
    ///
    /// # Errors
    ///
    /// Returns an error before changing host state when the interleaved block
    /// length is wrong or contains a non-finite sample.
    pub fn process_block(&mut self, input: &[f32]) -> Result<ProcessedBlock<'_>, ProcessError> {
        let expected = self.config.format.samples();
        if input.len() != expected {
            return Err(ProcessError::WrongSampleCount {
                expected,
                actual: input.len(),
            });
        }
        if let Some(index) = input.iter().position(|sample| !sample.is_finite()) {
            return Err(ProcessError::NonFiniteSample { index });
        }

        self.input_sequence = self.input_sequence.saturating_add(1);
        let sequence = self.input_sequence;
        let write = self.write_slot;
        let read = (write + 1) % PIPELINE_RING_BLOCKS;
        self.ring[write].dry.copy_from_slice(input);
        self.ring[write].wet.copy_from_slice(input);
        self.ring[write].sequence = Some(sequence);

        let mut elapsed = Duration::ZERO;
        let mut substitutions = 0_u32;
        for node in &mut self.plugins {
            if !node.is_runnable() {
                continue;
            }
            self.scratch.copy_from_slice(&self.ring[write].wet);
            let cost = node.plugin.processing_cost();
            elapsed = elapsed.saturating_add(cost);
            if elapsed > self.config.deadline {
                self.ring[write].wet.copy_from_slice(&self.scratch);
                node.status.health = PluginHealth::TimedOut;
                node.status.substitutions = node.status.substitutions.saturating_add(1);
                substitutions = substitutions.saturating_add(1);
                continue;
            }

            let result = catch_unwind(AssertUnwindSafe(|| {
                node.plugin.process(&mut self.ring[write].wet)
            }));
            if !matches!(result, Ok(Ok(()))) {
                self.ring[write].wet.copy_from_slice(&self.scratch);
                node.status.health = PluginHealth::Crashed;
                node.status.substitutions = node.status.substitutions.saturating_add(1);
                substitutions = substitutions.saturating_add(1);
            }
        }

        let deadline_missed = elapsed > self.config.deadline;
        if deadline_missed {
            self.total_deadline_misses = self.total_deadline_misses.saturating_add(1);
            self.consecutive_deadline_misses = self.consecutive_deadline_misses.saturating_add(1);
            if self.consecutive_deadline_misses >= self.config.misses_before_bypass {
                self.deadline_bypass = true;
                self.update_ramp_target();
            }
        } else {
            self.consecutive_deadline_misses = 0;
        }

        let output_sequence = self.ring[read].sequence;
        if output_sequence.is_some() {
            let channels = self.config.format.channels;
            for frame in 0..self.config.format.frames {
                let wet_gain = self.ramp.next();
                let dry_gain = 1.0 - wet_gain;
                let first = frame * channels;
                for sample in first..first + channels {
                    self.output[sample] = self.ring[read].wet[sample] * wet_gain
                        + self.ring[read].dry[sample] * dry_gain;
                }
            }
        } else {
            self.output.fill(0.0);
        }
        self.write_slot = read;

        let report = BlockReport {
            input_sequence: sequence,
            output_sequence,
            elapsed,
            deadline: self.config.deadline,
            deadline_missed,
            consecutive_deadline_misses: self.consecutive_deadline_misses,
            total_deadline_misses: self.total_deadline_misses,
            substitutions,
            bypass_reason: self.bypass_reason(),
        };
        Ok(ProcessedBlock {
            samples: &self.output,
            report,
        })
    }

    /// Returns all capacities relevant to the no-growth processing contract.
    #[must_use]
    pub fn capacities(&self) -> CapacitySnapshot {
        CapacitySnapshot {
            chain_slots: self.plugins.capacity(),
            ring_slots: self.ring.capacity(),
            samples_per_ring_buffer: self
                .ring
                .first()
                .map_or(0, |slot| slot.wet.capacity().min(slot.dry.capacity())),
            scratch_samples: self.scratch.capacity(),
            output_samples: self.output.capacity(),
            plugin_realtime_samples: self
                .plugins
                .iter()
                .map(|node| node.plugin.realtime_capacity_samples())
                .sum(),
        }
    }

    /// Saves plugin parameters and host bypass controls.
    #[must_use]
    pub fn save_state(&self) -> SavedHostState {
        SavedHostState {
            chain_id: self.chain_id,
            format: self.config.format,
            manual_bypass: self.manual_bypass,
            plugins: self
                .plugins
                .iter()
                .map(|node| {
                    let metadata = node.plugin.metadata();
                    SavedPluginState {
                        id: metadata.id,
                        version: metadata.version,
                        manually_bypassed: node.manually_bypassed,
                        payload: node.plugin.save_state(),
                    }
                })
                .collect(),
        }
    }

    /// Validates and restores a saved chain state.
    ///
    /// Identity, order, versions, and every payload are validated before any
    /// plugin or host state is changed.
    ///
    /// # Errors
    ///
    /// Returns a typed mismatch or invalid plugin-state error.
    pub fn restore_state(&mut self, state: &SavedHostState) -> Result<(), StateError> {
        if state.chain_id != self.chain_id {
            return Err(StateError::ChainMismatch {
                expected: self.chain_id,
                actual: state.chain_id,
            });
        }
        if state.format != self.config.format {
            return Err(StateError::FormatMismatch);
        }
        if state.plugins.len() != self.plugins.len() {
            return Err(StateError::PluginCountMismatch {
                expected: self.plugins.len(),
                actual: state.plugins.len(),
            });
        }
        for (index, (saved, node)) in state.plugins.iter().zip(&self.plugins).enumerate() {
            let metadata = node.plugin.metadata();
            if saved.id != metadata.id {
                return Err(StateError::PluginMismatch {
                    index,
                    expected: metadata.id,
                    actual: saved.id,
                });
            }
            if saved.version != metadata.version {
                return Err(StateError::VersionMismatch {
                    id: metadata.id,
                    saved: saved.version,
                    current: metadata.version,
                });
            }
            node.plugin
                .validate_state(&saved.payload)
                .map_err(|error| StateError::InvalidPluginState {
                    id: metadata.id,
                    error,
                })?;
        }

        for (saved, node) in state.plugins.iter().zip(&mut self.plugins) {
            node.plugin.restore_state(&saved.payload).map_err(|error| {
                StateError::InvalidPluginState {
                    id: saved.id,
                    error,
                }
            })?;
            node.manually_bypassed = saved.manually_bypassed;
            node.status.health = PluginHealth::Active;
            node.status.substitutions = 0;
        }
        self.manual_bypass = state.manual_bypass;
        self.deadline_bypass = false;
        self.consecutive_deadline_misses = 0;
        self.ramp.set_target(
            if self.manual_bypass { 0.0 } else { 1.0 },
            self.config.bypass_ramp_frames,
        );
        self.flush_pipeline();
        Ok(())
    }

    fn update_ramp_target(&mut self) {
        let target = if self.manual_bypass || self.deadline_bypass {
            0.0
        } else {
            1.0
        };
        self.ramp.set_target(target, self.config.bypass_ramp_frames);
    }

    fn flush_pipeline(&mut self) {
        for slot in &mut self.ring {
            slot.clear();
        }
        self.output.fill(0.0);
        self.scratch.fill(0.0);
        self.write_slot = 0;
    }
}

/// Plugin control error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginControlError {
    UnknownPlugin(PluginId),
}

impl fmt::Display for PluginControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPlugin(id) => write!(formatter, "unknown plugin {id}"),
        }
    }
}

impl std::error::Error for PluginControlError {}

/// Deterministic fake gain plugin.
#[derive(Clone, Debug)]
pub struct FakeGainPlugin {
    metadata: PluginMetadata,
    gain: f32,
    processing_cost: Duration,
    configured_samples: usize,
}

impl FakeGainPlugin {
    /// Creates a gain plugin.
    ///
    /// # Errors
    ///
    /// Returns an error unless `gain` is finite.
    pub fn new(id: PluginId, gain: f32) -> Result<Self, PluginError> {
        if !gain.is_finite() {
            return Err(PluginError::InvalidParameter("gain"));
        }
        Ok(Self {
            metadata: PluginMetadata {
                id,
                name: "Deterministic Gain",
                vendor: "FreeMix Test",
                version: 1,
                kind: PluginKind::Gain,
            },
            gain,
            processing_cost: Duration::ZERO,
            configured_samples: 0,
        })
    }

    #[must_use]
    pub const fn gain(&self) -> f32 {
        self.gain
    }

    /// Changes deterministic deadline cost without sleeping.
    #[must_use]
    pub const fn with_processing_cost(mut self, cost: Duration) -> Self {
        self.processing_cost = cost;
        self
    }
}

impl DspPlugin for FakeGainPlugin {
    fn metadata(&self) -> PluginMetadata {
        self.metadata
    }

    fn configure(&mut self, format: BlockFormat) -> Result<(), PluginError> {
        self.configured_samples = format.samples();
        Ok(())
    }

    fn processing_cost(&self) -> Duration {
        self.processing_cost
    }

    fn process(&mut self, block: &mut [f32]) -> Result<(), PluginError> {
        if block.len() != self.configured_samples || self.configured_samples == 0 {
            return Err(PluginError::NotConfigured);
        }
        for sample in block {
            *sample *= self.gain;
        }
        Ok(())
    }

    fn save_state(&self) -> Vec<u8> {
        self.gain.to_le_bytes().to_vec()
    }

    fn validate_state(&self, state: &[u8]) -> Result<(), PluginError> {
        let bytes: [u8; 4] = state.try_into().map_err(|_| PluginError::InvalidState)?;
        if f32::from_le_bytes(bytes).is_finite() {
            Ok(())
        } else {
            Err(PluginError::InvalidState)
        }
    }

    fn restore_state(&mut self, state: &[u8]) -> Result<(), PluginError> {
        self.validate_state(state)?;
        let bytes: [u8; 4] = state.try_into().map_err(|_| PluginError::InvalidState)?;
        self.gain = f32::from_le_bytes(bytes);
        Ok(())
    }
}

/// Deterministic fake sample delay plugin.
#[derive(Clone, Debug)]
pub struct FakeDelayPlugin {
    metadata: PluginMetadata,
    delay_frames: usize,
    processing_cost: Duration,
    channels: usize,
    configured_samples: usize,
    delay_line: Vec<f32>,
    cursor: usize,
}

impl FakeDelayPlugin {
    #[must_use]
    pub const fn new(id: PluginId, delay_frames: usize) -> Self {
        Self {
            metadata: PluginMetadata {
                id,
                name: "Deterministic Delay",
                vendor: "FreeMix Test",
                version: 1,
                kind: PluginKind::Delay,
            },
            delay_frames,
            processing_cost: Duration::ZERO,
            channels: 0,
            configured_samples: 0,
            delay_line: Vec::new(),
            cursor: 0,
        }
    }

    /// Changes deterministic deadline cost without sleeping.
    #[must_use]
    pub const fn with_processing_cost(mut self, cost: Duration) -> Self {
        self.processing_cost = cost;
        self
    }
}

impl DspPlugin for FakeDelayPlugin {
    fn metadata(&self) -> PluginMetadata {
        self.metadata
    }

    fn configure(&mut self, format: BlockFormat) -> Result<(), PluginError> {
        if self.delay_frames > MAX_DELAY_FRAMES {
            return Err(PluginError::InvalidParameter("delay frames"));
        }
        let samples = self
            .delay_frames
            .checked_mul(format.channels)
            .ok_or(PluginError::InvalidParameter("delay frames"))?;
        self.channels = format.channels;
        self.configured_samples = format.samples();
        self.delay_line = vec![0.0; samples];
        self.cursor = 0;
        Ok(())
    }

    fn processing_cost(&self) -> Duration {
        self.processing_cost
    }

    fn process(&mut self, block: &mut [f32]) -> Result<(), PluginError> {
        if block.len() != self.configured_samples || self.channels == 0 {
            return Err(PluginError::NotConfigured);
        }
        if self.delay_line.is_empty() {
            return Ok(());
        }
        for sample in block {
            std::mem::swap(sample, &mut self.delay_line[self.cursor]);
            self.cursor += 1;
            if self.cursor == self.delay_line.len() {
                self.cursor = 0;
            }
        }
        Ok(())
    }

    fn save_state(&self) -> Vec<u8> {
        let mut state = Vec::with_capacity(16 + self.delay_line.len() * size_of::<f32>());
        state.extend_from_slice(&(self.delay_frames as u64).to_le_bytes());
        state.extend_from_slice(&(self.cursor as u64).to_le_bytes());
        for sample in &self.delay_line {
            state.extend_from_slice(&sample.to_le_bytes());
        }
        state
    }

    fn validate_state(&self, state: &[u8]) -> Result<(), PluginError> {
        let expected = 16 + self.delay_line.len() * size_of::<f32>();
        if state.len() != expected {
            return Err(PluginError::InvalidState);
        }
        let delay = read_u64(state, 0)?;
        let cursor = read_u64(state, 8)?;
        if delay != self.delay_frames as u64
            || usize::try_from(cursor).map_or(true, |value| {
                if self.delay_line.is_empty() {
                    value != 0
                } else {
                    value >= self.delay_line.len()
                }
            })
        {
            return Err(PluginError::InvalidState);
        }
        for bytes in state[16..].chunks_exact(4) {
            let value = f32::from_le_bytes(bytes.try_into().expect("four-byte chunk"));
            if !value.is_finite() {
                return Err(PluginError::InvalidState);
            }
        }
        Ok(())
    }

    fn restore_state(&mut self, state: &[u8]) -> Result<(), PluginError> {
        self.validate_state(state)?;
        self.cursor =
            usize::try_from(read_u64(state, 8)?).map_err(|_| PluginError::InvalidState)?;
        for (sample, bytes) in self.delay_line.iter_mut().zip(state[16..].chunks_exact(4)) {
            *sample = f32::from_le_bytes(bytes.try_into().expect("four-byte chunk"));
        }
        Ok(())
    }

    fn realtime_capacity_samples(&self) -> usize {
        self.delay_line.capacity()
    }
}

/// Deterministic fake plugin which cooperatively crashes on a selected call.
#[derive(Clone, Debug)]
pub struct FakeCrashPlugin {
    metadata: PluginMetadata,
    crash_on_call: Option<u64>,
    processing_cost: Duration,
    calls: u64,
    configured_samples: usize,
}

impl FakeCrashPlugin {
    #[must_use]
    pub const fn new(id: PluginId, crash_on_call: Option<u64>) -> Self {
        Self {
            metadata: PluginMetadata {
                id,
                name: "Deterministic Crash",
                vendor: "FreeMix Test",
                version: 1,
                kind: PluginKind::Crash,
            },
            crash_on_call,
            processing_cost: Duration::ZERO,
            calls: 0,
            configured_samples: 0,
        }
    }

    /// Changes deterministic deadline cost without sleeping.
    #[must_use]
    pub const fn with_processing_cost(mut self, cost: Duration) -> Self {
        self.processing_cost = cost;
        self
    }
}

impl DspPlugin for FakeCrashPlugin {
    fn metadata(&self) -> PluginMetadata {
        self.metadata
    }

    fn configure(&mut self, format: BlockFormat) -> Result<(), PluginError> {
        self.configured_samples = format.samples();
        self.calls = 0;
        Ok(())
    }

    fn processing_cost(&self) -> Duration {
        self.processing_cost
    }

    fn process(&mut self, block: &mut [f32]) -> Result<(), PluginError> {
        if block.len() != self.configured_samples || self.configured_samples == 0 {
            return Err(PluginError::NotConfigured);
        }
        self.calls = self.calls.saturating_add(1);
        if self.crash_on_call == Some(self.calls) {
            Err(PluginError::Crashed)
        } else {
            Ok(())
        }
    }

    fn save_state(&self) -> Vec<u8> {
        self.calls.to_le_bytes().to_vec()
    }

    fn validate_state(&self, state: &[u8]) -> Result<(), PluginError> {
        <[u8; 8]>::try_from(state)
            .map(|_| ())
            .map_err(|_| PluginError::InvalidState)
    }

    fn restore_state(&mut self, state: &[u8]) -> Result<(), PluginError> {
        self.validate_state(state)?;
        self.calls = read_u64(state, 0)?;
        Ok(())
    }
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, PluginError> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or(PluginError::InvalidState)?;
    Ok(u64::from_le_bytes(
        value.try_into().map_err(|_| PluginError::InvalidState)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: u64) -> PluginId {
        PluginId::new(value)
    }

    fn format(frames: usize) -> BlockFormat {
        BlockFormat::new(48_000, 1, frames).unwrap()
    }

    fn config(frames: usize) -> HostConfig {
        HostConfig {
            format: format(frames),
            deadline: Duration::from_micros(100),
            misses_before_bypass: 1,
            bypass_ramp_frames: frames,
        }
    }

    fn gain(id_value: u64, value: f32) -> Box<dyn DspPlugin> {
        Box::new(FakeGainPlugin::new(id(id_value), value).unwrap())
    }

    #[test]
    fn scanner_tracks_metadata_and_state_without_loading_plugins() {
        let mut scanner = PluginScanner::new(2).unwrap();
        scanner.begin_scan();
        scanner
            .record(FakeGainPlugin::new(id(1), 1.0).unwrap().metadata())
            .unwrap();
        scanner
            .record(FakeDelayPlugin::new(id(2), 4).metadata())
            .unwrap();
        let metadata = scanner.finish_scan().unwrap();
        assert_eq!(metadata.len(), 2);
        assert_eq!(metadata[0].kind, PluginKind::Gain);
        assert_eq!(
            scanner.state(),
            ScanState::Ready {
                generation: 1,
                plugins: 2
            }
        );
    }

    #[test]
    fn chain_order_is_exact() {
        let mut host =
            DspHost::configure(ChainId::new(7), config(2), vec![gain(1, 2.0), gain(2, 3.0)])
                .unwrap();
        assert_eq!(host.plugin_ids().collect::<Vec<_>>(), vec![id(1), id(2)]);
        assert_eq!(
            host.process_block(&[1.0, 2.0]).unwrap().samples,
            &[0.0, 0.0]
        );
        assert_eq!(
            host.process_block(&[0.0, 0.0]).unwrap().samples,
            &[6.0, 12.0]
        );
    }

    #[test]
    fn pipeline_latency_is_exactly_one_block() {
        let mut host = DspHost::configure(ChainId::new(1), config(2), vec![gain(1, 1.0)]).unwrap();
        let first = host.process_block(&[1.0, 2.0]).unwrap();
        assert_eq!(first.report.output_sequence, None);
        assert_eq!(first.samples, &[0.0, 0.0]);
        let second = host.process_block(&[3.0, 4.0]).unwrap();
        assert_eq!(second.report.output_sequence, Some(1));
        assert_eq!(second.samples, &[1.0, 2.0]);
        let third = host.process_block(&[5.0, 6.0]).unwrap();
        assert_eq!(third.report.output_sequence, Some(2));
        assert_eq!(third.samples, &[3.0, 4.0]);
    }

    #[test]
    fn capacities_do_not_grow_after_configuration() {
        let plugins: Vec<Box<dyn DspPlugin>> =
            vec![gain(1, 2.0), Box::new(FakeDelayPlugin::new(id(2), 17))];
        let mut host = DspHost::configure(ChainId::new(1), config(8), plugins).unwrap();
        let configured = host.capacities();
        assert_eq!(configured.ring_slots, PIPELINE_RING_BLOCKS);
        assert_eq!(configured.plugin_realtime_samples, 17);
        for _ in 0..1_000 {
            host.process_block(&[0.25; 8]).unwrap();
        }
        assert_eq!(host.capacities(), configured);
    }

    #[test]
    fn deadline_timeout_substitutes_and_engages_ramped_bypass() {
        let slow = FakeDelayPlugin::new(id(2), 0).with_processing_cost(Duration::from_micros(101));
        let mut host = DspHost::configure(
            ChainId::new(1),
            config(2),
            vec![gain(1, 2.0), Box::new(slow)],
        )
        .unwrap();
        let first = host.process_block(&[1.0, 1.0]).unwrap();
        assert!(first.report.deadline_missed);
        assert_eq!(first.report.substitutions, 1);
        assert_eq!(first.report.bypass_reason, Some(BypassReason::Deadline));
        assert_eq!(
            host.plugin_status(id(2)).unwrap().health,
            PluginHealth::TimedOut
        );

        let second = host.process_block(&[1.0, 1.0]).unwrap();
        assert_eq!(second.samples, &[1.5, 1.0]);
    }

    #[test]
    fn crash_is_isolated_and_later_plugins_continue() {
        let crash = FakeCrashPlugin::new(id(2), Some(1));
        let mut host = DspHost::configure(
            ChainId::new(1),
            config(2),
            vec![gain(1, 2.0), Box::new(crash), gain(3, 3.0)],
        )
        .unwrap();
        let first = host.process_block(&[1.0, 2.0]).unwrap();
        assert_eq!(first.report.substitutions, 1);
        assert_eq!(
            host.plugin_status(id(2)).unwrap().health,
            PluginHealth::Crashed
        );
        assert_eq!(
            host.process_block(&[0.0, 0.0]).unwrap().samples,
            &[6.0, 12.0]
        );
    }

    #[test]
    fn bypass_ramps_are_linear_and_click_free_in_both_directions() {
        let mut host = DspHost::configure(ChainId::new(1), config(4), vec![gain(1, 2.0)]).unwrap();
        host.process_block(&[1.0; 4]).unwrap();
        host.set_bypass(true);
        let down = host.process_block(&[1.0; 4]).unwrap();
        assert_eq!(down.samples, &[1.75, 1.5, 1.25, 1.0]);
        host.set_bypass(false);
        let up = host.process_block(&[1.0; 4]).unwrap();
        assert_eq!(up.samples, &[1.25, 1.5, 1.75, 2.0]);
    }

    #[test]
    fn delay_is_deterministic_across_blocks() {
        let mut host = DspHost::configure(
            ChainId::new(1),
            config(2),
            vec![Box::new(FakeDelayPlugin::new(id(1), 3))],
        )
        .unwrap();
        host.process_block(&[1.0, 2.0]).unwrap();
        assert_eq!(
            host.process_block(&[3.0, 4.0]).unwrap().samples,
            &[0.0, 0.0]
        );
        assert_eq!(
            host.process_block(&[5.0, 6.0]).unwrap().samples,
            &[0.0, 1.0]
        );
        assert_eq!(
            host.process_block(&[7.0, 8.0]).unwrap().samples,
            &[2.0, 3.0]
        );
    }

    #[test]
    fn state_round_trips_and_version_mismatch_is_rejected_atomically() {
        let mut source = DspHost::configure(
            ChainId::new(9),
            config(2),
            vec![gain(1, 2.0), Box::new(FakeDelayPlugin::new(id(2), 1))],
        )
        .unwrap();
        source.set_plugin_bypass(id(2), true).unwrap();
        source.set_bypass(true);
        let saved = source.save_state();

        let mut restored = DspHost::configure(
            ChainId::new(9),
            config(2),
            vec![gain(1, 9.0), Box::new(FakeDelayPlugin::new(id(2), 1))],
        )
        .unwrap();
        restored.restore_state(&saved).unwrap();
        assert_eq!(restored.bypass_reason(), Some(BypassReason::Manual));
        assert_eq!(
            restored.plugin_status(id(2)).unwrap().health,
            PluginHealth::ManuallyBypassed
        );
        restored.set_bypass(false);
        restored.process_block(&[1.0, 1.0]).unwrap();
        assert_eq!(
            restored.process_block(&[0.0, 0.0]).unwrap().samples,
            &[2.0, 2.0]
        );

        let before = restored.save_state();
        let mut incompatible = saved;
        incompatible.plugins[0].version = 99;
        assert_eq!(
            restored.restore_state(&incompatible),
            Err(StateError::VersionMismatch {
                id: id(1),
                saved: 99,
                current: 1
            })
        );
        assert_eq!(restored.save_state(), before);
    }

    #[test]
    fn malformed_blocks_do_not_advance_pipeline() {
        let mut host = DspHost::configure(ChainId::new(1), config(2), vec![gain(1, 1.0)]).unwrap();
        assert!(matches!(
            host.process_block(&[1.0]),
            Err(ProcessError::WrongSampleCount { .. })
        ));
        assert!(matches!(
            host.process_block(&[f32::NAN, 1.0]),
            Err(ProcessError::NonFiniteSample { .. })
        ));
        assert_eq!(
            host.process_block(&[1.0, 2.0])
                .unwrap()
                .report
                .input_sequence,
            1
        );
    }

    #[test]
    fn fake_delay_rejects_unbounded_storage() {
        let result = DspHost::configure(
            ChainId::new(1),
            config(2),
            vec![Box::new(FakeDelayPlugin::new(id(1), MAX_DELAY_FRAMES + 1))],
        );
        assert!(matches!(
            result,
            Err(ConfigError::Plugin {
                error: PluginError::InvalidParameter("delay frames"),
                ..
            })
        ));
    }
}
