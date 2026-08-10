//! Exact-identity, bounded macOS microphone capture through the helper process.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    num::{NonZeroU128, NonZeroUsize},
    sync::{Arc, Mutex},
    time::Instant,
};

#[cfg(target_os = "macos")]
use std::{
    ffi::OsString,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError, sync_channel},
    },
    thread::JoinHandle,
    time::Duration,
};

use fm_frame::{AudioBlock, ClockDomainId};
use fm_io_api::{
    DeviceId, Discovery, DiscoveryEvent, DiscoveryEventKind, DiscoverySnapshot, DriverState,
    EndpointCapabilities, EndpointHealth, EndpointHealthState, FormatDescriptor, IoError,
    LifecycleState, MediaSource, MediaTransfer, MemoryDomain, OpenOptions, PermissionState,
    Remediation, SignalLossPolicy, SourceDescriptor, SourceId, TransferLimits,
};
use fm_types::SampleRate;

use crate::{
    FNV_OFFSET_BASIS_128, FNV_PRIME_128, SIGNAL_LOSS_TIMEOUT, adapter_failure, coremedia_clock,
    escaped_text, invalid_state, permission_state,
    protocol::{self, HelperAudioDevice, HelperAudioDiscovery, MAX_AUDIO_BLOCK_BYTES},
    stable_id,
};
#[cfg(target_os = "macos")]
use crate::{developer_helper_path, escaped_diagnostic, protocol::MAX_DISCOVERY_BYTES, run_helper};

const MAX_AUDIO_QUEUE_CAPACITY: usize = 32;
const AUDIO_STABLE_KEY_PREFIX: &str = "macos.avfoundation.audio.v1.";
#[cfg(target_os = "macos")]
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(target_os = "macos")]
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(target_os = "macos")]
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(target_os = "macos")]
const PERMISSION_TIMEOUT: Duration = Duration::from_mins(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MacosAudioAvailability {
    Available,
    UnsupportedPlatform,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioIdKind {
    Device,
    Source,
    CoreMediaClock,
}

impl AudioIdKind {
    const fn namespace(self) -> &'static [u8] {
        match self {
            Self::Device => b"freemix:macos:audio:device\0",
            Self::Source => b"freemix:macos:audio:source\0",
            Self::CoreMediaClock => b"freemix:macos:audio:coremedia:clock\0",
        }
    }
}

/// Maps native audio identifiers with namespaced `FNV-1a-128`.
#[must_use]
pub fn deterministic_audio_id(kind: AudioIdKind, native_id: &str) -> NonZeroU128 {
    let mut hash = FNV_OFFSET_BASIS_128;
    for byte in kind.namespace().iter().chain(native_id.as_bytes()) {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME_128);
    }
    NonZeroU128::new(hash).unwrap_or(NonZeroU128::MIN)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioError {
    UnsupportedPlatform,
    Protocol(protocol::ProtocolError),
    Helper(String),
    IdCollision {
        kind: AudioIdKind,
        first: String,
        second: String,
    },
    DuplicateDeviceId(String),
    UnknownSource(SourceId),
    UnknownStableKey(String),
}

impl fmt::Display for AudioError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                formatter.write_str("AVFoundation audio capture is available only on macOS")
            }
            Self::Protocol(error) => write!(formatter, "audio helper protocol error: {error}"),
            Self::Helper(detail) => formatter.write_str(detail),
            Self::IdCollision {
                kind,
                first,
                second,
            } => write!(
                formatter,
                "{kind:?} identifier collision between `{}` and `{}`",
                escaped_text(first),
                escaped_text(second)
            ),
            Self::DuplicateDeviceId(id) => {
                write!(
                    formatter,
                    "duplicate audio device id `{}`",
                    escaped_text(id)
                )
            }
            Self::UnknownSource(id) => write!(formatter, "unknown audio source {id}"),
            Self::UnknownStableKey(key) => write!(
                formatter,
                "unknown audio stable key `{}`",
                escaped_text(key)
            ),
        }
    }
}

impl std::error::Error for AudioError {}

impl From<protocol::ProtocolError> for AudioError {
    fn from(error: protocol::ProtocolError) -> Self {
        Self::Protocol(error)
    }
}

#[derive(Clone)]
struct AudioRecord {
    #[cfg(target_os = "macos")]
    native_id: String,
    descriptor: SourceDescriptor,
}

pub struct MacosAudioAdapter {
    generation: u64,
    sources: BTreeMap<SourceId, AudioRecord>,
    events: VecDeque<DiscoveryEvent>,
    permission: PermissionState,
    #[cfg(target_os = "macos")]
    helper_path: PathBuf,
}

impl MacosAudioAdapter {
    #[must_use]
    pub const fn availability() -> MacosAudioAvailability {
        if cfg!(target_os = "macos") {
            MacosAudioAvailability::Available
        } else {
            MacosAudioAvailability::UnsupportedPlatform
        }
    }

    /// Discovers microphones without prompting.
    ///
    /// # Errors
    ///
    /// Returns a platform, helper, protocol, or identifier error.
    pub fn discover() -> Result<Self, AudioError> {
        #[cfg(target_os = "macos")]
        {
            Self::discover_with_helper(developer_helper_path())
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(AudioError::UnsupportedPlatform)
        }
    }

    /// Discovers microphones with an application-supplied helper.
    ///
    /// # Errors
    ///
    /// Returns a helper, protocol, or identifier error.
    #[cfg(target_os = "macos")]
    pub fn discover_with_helper(path: impl AsRef<Path>) -> Result<Self, AudioError> {
        let helper_path = path.as_ref().to_path_buf();
        let bytes = run_audio_helper(
            &helper_path,
            &[OsString::from("discover-audio")],
            MAX_DISCOVERY_BYTES,
            DISCOVERY_TIMEOUT,
        )?;
        Self::from_discovery_with_helper(&bytes, helper_path)
    }

    /// Parses one complete audio discovery response.
    ///
    /// # Errors
    ///
    /// Returns a protocol or identifier error.
    pub fn from_discovery_bytes(bytes: &[u8]) -> Result<Self, AudioError> {
        #[cfg(target_os = "macos")]
        return Self::from_discovery_with_helper(bytes, developer_helper_path());
        #[cfg(not(target_os = "macos"))]
        Self::from_discovery_common(bytes)
    }

    fn from_discovery_common(bytes: &[u8]) -> Result<Self, AudioError> {
        let parsed = protocol::parse_audio_discovery(bytes)?;
        let sources = records_from_discovery(&parsed, deterministic_audio_id)?;
        Ok(Self {
            generation: 0,
            sources,
            events: VecDeque::new(),
            permission: permission_state(parsed.permission),
            #[cfg(target_os = "macos")]
            helper_path: developer_helper_path(),
        })
    }

    #[cfg(target_os = "macos")]
    fn from_discovery_with_helper(bytes: &[u8], helper_path: PathBuf) -> Result<Self, AudioError> {
        let mut adapter = Self::from_discovery_common(bytes)?;
        adapter.helper_path = helper_path;
        Ok(adapter)
    }

    /// Refreshes the microphone inventory without prompting.
    ///
    /// # Errors
    ///
    /// Returns a platform, helper, protocol, or identifier error.
    pub fn refresh(&mut self) -> Result<(), AudioError> {
        #[cfg(target_os = "macos")]
        {
            let bytes = run_audio_helper(
                &self.helper_path,
                &[OsString::from("discover-audio")],
                MAX_DISCOVERY_BYTES,
                DISCOVERY_TIMEOUT,
            )?;
            self.refresh_from_discovery_bytes(&bytes)
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(AudioError::UnsupportedPlatform)
        }
    }

    /// Applies one complete audio discovery response transactionally.
    ///
    /// # Errors
    ///
    /// Returns a protocol or identifier error without changing the snapshot.
    pub fn refresh_from_discovery_bytes(&mut self, bytes: &[u8]) -> Result<(), AudioError> {
        let parsed = protocol::parse_audio_discovery(bytes)?;
        let next = records_from_discovery(&parsed, deterministic_audio_id)?;
        for (id, record) in &next {
            if !self.sources.contains_key(id) {
                self.push_event(DiscoveryEventKind::SourceAdded(record.descriptor.clone()));
            }
        }
        for (id, record) in &next {
            if let Some(previous) = self.sources.get(id)
                && previous.descriptor != record.descriptor
            {
                self.push_event(DiscoveryEventKind::SourceUpdated(record.descriptor.clone()));
            }
        }
        let removed = self
            .sources
            .keys()
            .filter(|id| !next.contains_key(id))
            .copied()
            .collect::<Vec<_>>();
        for id in removed {
            self.push_event(DiscoveryEventKind::SourceRemoved(id));
        }
        self.sources = next;
        self.permission = permission_state(parsed.permission);
        Ok(())
    }

    #[must_use]
    pub const fn permission(&self) -> &PermissionState {
        &self.permission
    }

    /// Explicitly invokes the microphone permission prompt.
    ///
    /// # Errors
    ///
    /// Returns a platform, helper, or protocol error.
    pub fn request_microphone_permission() -> Result<PermissionState, AudioError> {
        #[cfg(target_os = "macos")]
        {
            Self::request_microphone_permission_with_helper(developer_helper_path())
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(AudioError::UnsupportedPlatform)
        }
    }

    /// Explicitly requests microphone permission with a supplied helper.
    ///
    /// # Errors
    ///
    /// Returns the prompt error, or a later discovery/protocol error.
    #[cfg(target_os = "macos")]
    pub fn request_microphone_permission_with_helper(
        path: impl AsRef<Path>,
    ) -> Result<PermissionState, AudioError> {
        let path = path.as_ref();
        run_audio_helper(
            path,
            &[OsString::from("request-audio-permission")],
            0,
            PERMISSION_TIMEOUT,
        )?;
        let bytes = run_audio_helper(
            path,
            &[OsString::from("discover-audio")],
            MAX_DISCOVERY_BYTES,
            DISCOVERY_TIMEOUT,
        )?;
        Ok(permission_state(
            protocol::parse_audio_discovery(&bytes)?.permission,
        ))
    }

    /// Opens exactly one discovered microphone source.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::UnknownSource`] without substitution.
    pub fn open_audio_source(&self, id: SourceId) -> Result<MacosAudioSource, AudioError> {
        let record = self.sources.get(&id).ok_or(AudioError::UnknownSource(id))?;
        Ok(MacosAudioSource::new(
            record.clone(),
            #[cfg(target_os = "macos")]
            self.helper_path.clone(),
        ))
    }

    /// Opens exactly one adapter-qualified persisted audio key.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::UnknownStableKey`] without substitution.
    pub fn open_audio_source_by_stable_key(
        &self,
        stable_key: &str,
    ) -> Result<MacosAudioSource, AudioError> {
        let record = self
            .sources
            .values()
            .find(|record| record.descriptor.stable_key == stable_key)
            .ok_or_else(|| AudioError::UnknownStableKey(stable_key.to_owned()))?;
        Ok(MacosAudioSource::new(
            record.clone(),
            #[cfg(target_os = "macos")]
            self.helper_path.clone(),
        ))
    }

    fn push_event(&mut self, kind: DiscoveryEventKind) {
        self.generation = self.generation.saturating_add(1);
        self.events.push_back(DiscoveryEvent {
            generation: self.generation,
            kind,
        });
    }
}

impl Discovery for MacosAudioAdapter {
    fn snapshot(&self) -> DiscoverySnapshot {
        DiscoverySnapshot {
            generation: self.generation,
            sources: self
                .sources
                .values()
                .map(|record| record.descriptor.clone())
                .collect(),
            sinks: Vec::new(),
        }
    }

    fn next_event(&mut self) -> Option<DiscoveryEvent> {
        self.events.pop_front()
    }
}

fn records_from_discovery(
    discovery: &HelperAudioDiscovery,
    hasher: impl Fn(AudioIdKind, &str) -> NonZeroU128,
) -> Result<BTreeMap<SourceId, AudioRecord>, AudioError> {
    let mut native_ids = BTreeSet::new();
    let mut device_hashes = BTreeMap::<NonZeroU128, String>::new();
    let mut source_hashes = BTreeMap::<NonZeroU128, String>::new();
    let mut records = BTreeMap::new();
    for device in &discovery.devices {
        if !native_ids.insert(device.id.clone()) {
            return Err(AudioError::DuplicateDeviceId(device.id.clone()));
        }
        let device_hash = hasher(AudioIdKind::Device, &device.id);
        reject_collision(
            &mut device_hashes,
            AudioIdKind::Device,
            device_hash,
            &device.id,
        )?;
        let source_hash = hasher(AudioIdKind::Source, &device.id);
        reject_collision(
            &mut source_hashes,
            AudioIdKind::Source,
            source_hash,
            &device.id,
        )?;
        let source_id = SourceId::new(source_hash);
        records.insert(
            source_id,
            AudioRecord {
                #[cfg(target_os = "macos")]
                native_id: device.id.clone(),
                descriptor: source_descriptor(
                    device,
                    DeviceId::new(device_hash),
                    source_id,
                    permission_state(discovery.permission),
                ),
            },
        );
    }
    Ok(records)
}

fn reject_collision(
    seen: &mut BTreeMap<NonZeroU128, String>,
    kind: AudioIdKind,
    hash: NonZeroU128,
    native_id: &str,
) -> Result<(), AudioError> {
    if let Some(first) = seen.insert(hash, native_id.to_owned())
        && first != native_id
    {
        return Err(AudioError::IdCollision {
            kind,
            first,
            second: native_id.to_owned(),
        });
    }
    Ok(())
}

fn source_descriptor(
    device: &HelperAudioDevice,
    device_id: DeviceId,
    source_id: SourceId,
    permission: PermissionState,
) -> SourceDescriptor {
    SourceDescriptor {
        id: source_id,
        device_id,
        stable_key: format!("{AUDIO_STABLE_KEY_PREFIX}{source_id}"),
        name: device.name.clone(),
        capabilities: EndpointCapabilities {
            formats: device
                .formats
                .iter()
                .copied()
                .map(raw_audio_format)
                .collect(),
            clocks: vec![audio_coremedia_clock(&device.id)],
            memory_domains: vec![MemoryDomain::Cpu],
            transfer: TransferLimits::new(
                NonZeroUsize::new(MAX_AUDIO_QUEUE_CAPACITY).expect("nonzero audio queue limit"),
                NonZeroUsize::new(MAX_AUDIO_BLOCK_BYTES).expect("nonzero audio block limit"),
            ),
        },
        permission,
        driver: DriverState::Ready,
    }
}

fn audio_coremedia_clock(native_id: &str) -> fm_io_api::ClockCapability {
    let mut clock = coremedia_clock();
    clock.domain = ClockDomainId::new(deterministic_audio_id(
        AudioIdKind::CoreMediaClock,
        native_id,
    ));
    clock
}

fn raw_audio_format(format: protocol::HelperAudioFormat) -> FormatDescriptor {
    FormatDescriptor::new(stable_id("audio.raw"))
        .with_field(
            stable_id("sample-rate"),
            u64::from(format.sample_rate.hertz()),
        )
        .with_field(stable_id("channels"), u64::from(format.channels))
        .with_field(stable_id("sample-format"), "f32-planar")
        .with_field(
            stable_id("channel-layout"),
            if format.channels == 1 {
                "mono"
            } else {
                "stereo"
            },
        )
}

fn exact_audio_format(format: &FormatDescriptor) -> Result<(SampleRate, u8), IoError> {
    use fm_capabilities::FormatValue;

    let unsigned = |name: &str| match format.fields.get(&stable_id(name)) {
        Some(FormatValue::Unsigned(value)) => Ok(*value),
        _ => Err(IoError::UnsupportedFormat),
    };
    match format.fields.get(&stable_id("sample-format")) {
        Some(FormatValue::Text(value)) if value == "f32-planar" => {}
        _ => return Err(IoError::UnsupportedFormat),
    }
    let sample_rate_hz =
        u32::try_from(unsigned("sample-rate")?).map_err(|_| IoError::UnsupportedFormat)?;
    let sample_rate = SampleRate::new(sample_rate_hz).ok_or(IoError::UnsupportedFormat)?;
    let channels = u8::try_from(unsigned("channels")?).map_err(|_| IoError::UnsupportedFormat)?;
    let expected_layout = if channels == 1 { "mono" } else { "stereo" };
    match format.fields.get(&stable_id("channel-layout")) {
        Some(FormatValue::Text(value)) if value == expected_layout && matches!(channels, 1 | 2) => {
        }
        _ => return Err(IoError::UnsupportedFormat),
    }
    Ok((sample_rate, channels))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AudioTelemetry {
    pub received: u64,
    pub overruns: u64,
    pub native_dropped: u64,
    pub current: usize,
    pub peak: usize,
}

#[derive(Default)]
struct AudioQueueState {
    blocks: VecDeque<AudioBlock>,
    capacity: usize,
    telemetry: AudioTelemetry,
    sticky_failure: Option<String>,
    #[cfg(target_os = "macos")]
    stderr: Vec<u8>,
    last_activity: Option<Instant>,
}

impl AudioQueueState {
    fn push(&mut self, block: AudioBlock, native_dropped_total: u64) {
        self.telemetry.received = self.telemetry.received.saturating_add(1);
        self.telemetry.native_dropped = native_dropped_total;
        if self.blocks.len() >= self.capacity {
            self.telemetry.overruns = self.telemetry.overruns.saturating_add(1);
            self.fail("microphone audio queue overrun");
            return;
        }
        self.blocks.push_back(block);
        self.last_activity = Some(Instant::now());
        self.telemetry.current = self.blocks.len();
        self.telemetry.peak = self.telemetry.peak.max(self.blocks.len());
    }

    fn pop(&mut self) -> Option<AudioBlock> {
        let block = self.blocks.pop_front();
        self.telemetry.current = self.blocks.len();
        block
    }

    fn fail(&mut self, detail: impl Into<String>) {
        if self.sticky_failure.is_none() {
            self.sticky_failure = Some(detail.into());
        }
    }
}

#[cfg(target_os = "macos")]
struct AudioCaptureProcess {
    child: Child,
    workers: Vec<JoinHandle<()>>,
    stop_token: Arc<AtomicBool>,
}

pub struct MacosAudioSource {
    descriptor: SourceDescriptor,
    #[cfg(target_os = "macos")]
    native_id: String,
    #[cfg(target_os = "macos")]
    helper_path: PathBuf,
    lifecycle: LifecycleState,
    health: EndpointHealth,
    options: Option<OpenOptions>,
    state: Arc<Mutex<AudioQueueState>>,
    resume_running: bool,
    #[cfg(target_os = "macos")]
    capture: Option<AudioCaptureProcess>,
}

impl MacosAudioSource {
    fn new(record: AudioRecord, #[cfg(target_os = "macos")] helper_path: PathBuf) -> Self {
        Self {
            descriptor: record.descriptor,
            #[cfg(target_os = "macos")]
            native_id: record.native_id,
            #[cfg(target_os = "macos")]
            helper_path,
            lifecycle: LifecycleState::Closed,
            health: EndpointHealth::HEALTHY,
            options: None,
            state: Arc::new(Mutex::new(AudioQueueState::default())),
            resume_running: false,
            #[cfg(target_os = "macos")]
            capture: None,
        }
    }

    #[must_use]
    pub fn telemetry(&self) -> AudioTelemetry {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .telemetry
    }

    /// Returns an exact advertised F32 planar mode.
    #[must_use]
    pub fn exact_audio_format(
        &self,
        sample_rate: SampleRate,
        channels: u8,
    ) -> Option<FormatDescriptor> {
        self.descriptor
            .capabilities
            .formats
            .iter()
            .find(|format| exact_audio_format(format) == Ok((sample_rate, channels)))
            .cloned()
    }

    fn validate_open(&self, options: &OpenOptions) -> Result<(), IoError> {
        if let Some(remediation) = self.descriptor.permission.remediation() {
            return Err(IoError::PermissionDenied {
                remediation: remediation.clone(),
            });
        }
        if let Some(remediation) = self.descriptor.driver.remediation() {
            return Err(IoError::DriverUnavailable {
                remediation: remediation.clone(),
            });
        }
        if !self
            .descriptor
            .capabilities
            .formats
            .contains(&options.format)
        {
            return Err(IoError::UnsupportedFormat);
        }
        if !self
            .descriptor
            .capabilities
            .clocks
            .iter()
            .any(|clock| clock.domain == options.clock_domain)
        {
            return Err(IoError::UnsupportedClock);
        }
        if options.memory_domain != MemoryDomain::Cpu {
            return Err(IoError::UnsupportedMemoryDomain);
        }
        if options.queue_capacity.get() > MAX_AUDIO_QUEUE_CAPACITY {
            return Err(IoError::QueueCapacityUnsupported {
                requested: options.queue_capacity.get(),
                maximum: MAX_AUDIO_QUEUE_CAPACITY,
            });
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[allow(clippy::too_many_lines)]
    fn spawn_capture(
        &self,
    ) -> Result<(AudioCaptureProcess, Receiver<Result<(), String>>), IoError> {
        let options = self
            .options
            .as_ref()
            .ok_or_else(|| invalid_state("start", self.lifecycle))?;
        let (sample_rate, channels) = exact_audio_format(&options.format)?;
        let mut child = Command::new(&self.helper_path)
            .arg("capture-audio")
            .arg(&self.native_id)
            .arg(sample_rate.hertz().to_string())
            .arg(channels.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| adapter_failure(format!("failed to start audio helper: {error}")))?;
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            terminate_audio_capture(AudioCaptureProcess {
                child,
                workers: Vec::new(),
                stop_token: Arc::new(AtomicBool::new(true)),
            });
            return Err(adapter_failure("audio helper pipes were not available"));
        };
        let stop_token = Arc::new(AtomicBool::new(false));
        let (startup_sender, startup_receiver) = sync_channel(1);
        let state = Arc::clone(&self.state);
        let worker_stop = Arc::clone(&stop_token);
        let clock = options.clock_domain;
        let stdout_worker = std::thread::Builder::new()
            .name("fm-audio-stdout".to_owned())
            .spawn(move || {
                let mut reader = match protocol::AudioBlockReader::new_with_format(
                    stdout,
                    clock,
                    sample_rate,
                    channels,
                ) {
                    Ok(reader) => {
                        let _ = startup_sender.send(Ok(()));
                        reader
                    }
                    Err(error) => {
                        let detail = format!("audio helper startup failed: {error}");
                        let _ = startup_sender.send(Err(detail.clone()));
                        if !worker_stop.load(Ordering::Acquire) {
                            state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .fail(detail);
                        }
                        return;
                    }
                };
                let error = loop {
                    match reader.read_captured_block() {
                        Ok(Some(captured)) => state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(captured.block, captured.native_dropped_total),
                        Ok(None) => return,
                        Err(error) => break error,
                    }
                };
                if !worker_stop.load(Ordering::Acquire) {
                    state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .fail(format!("audio helper block stream failed: {error}"));
                }
            });
        let stdout_worker = match stdout_worker {
            Ok(worker) => worker,
            Err(error) => {
                terminate_audio_capture(AudioCaptureProcess {
                    child,
                    workers: Vec::new(),
                    stop_token: Arc::clone(&stop_token),
                });
                return Err(adapter_failure(format!(
                    "failed to start audio stdout worker: {error}"
                )));
            }
        };
        let state = Arc::clone(&self.state);
        let worker_stop = Arc::clone(&stop_token);
        let stderr_worker = std::thread::Builder::new()
            .name("fm-audio-stderr".to_owned())
            .spawn(move || {
                let mut stderr = stderr;
                let mut buffer = [0; 4096];
                loop {
                    match stderr.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            let mut state = state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            let available = (64_usize * 1024).saturating_sub(state.stderr.len());
                            state
                                .stderr
                                .extend_from_slice(&buffer[..count.min(available)]);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(error) => {
                            if !worker_stop.load(Ordering::Acquire) {
                                state
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .fail(format!("audio helper stderr failed: {error}"));
                            }
                            break;
                        }
                    }
                }
            });
        let stderr_worker = match stderr_worker {
            Ok(worker) => worker,
            Err(error) => {
                terminate_audio_capture(AudioCaptureProcess {
                    child,
                    workers: vec![stdout_worker],
                    stop_token,
                });
                return Err(adapter_failure(format!(
                    "failed to start audio stderr worker: {error}"
                )));
            }
        };
        Ok((
            AudioCaptureProcess {
                child,
                workers: vec![stdout_worker, stderr_worker],
                stop_token,
            },
            startup_receiver,
        ))
    }

    #[cfg(not(target_os = "macos"))]
    fn spawn_capture(&self) -> Result<(), IoError> {
        Err(adapter_failure(
            "AVFoundation audio capture is available only on macOS",
        ))
    }

    fn sticky_error(&mut self) -> Option<IoError> {
        #[cfg(target_os = "macos")]
        {
            let mut signal_lost = None;
            let mut generic_failure = None;
            if let Some(capture) = &mut self.capture
                && !capture.stop_token.load(Ordering::Acquire)
            {
                match capture.child.try_wait() {
                    Ok(Some(status)) if status.code() == Some(20) => {
                        signal_lost = Some("audio helper reported source signal loss".to_owned());
                    }
                    Ok(Some(status)) => {
                        let stderr = self
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .stderr
                            .clone();
                        generic_failure = Some(format!(
                            "audio helper exited with {status}: {}",
                            escaped_diagnostic(&stderr)
                        ));
                    }
                    Err(error) => {
                        generic_failure = Some(format!("audio helper status failed: {error}"));
                    }
                    Ok(None) => {}
                }
            }
            if let Some(detail) = signal_lost {
                self.transition_to_signal_lost(detail);
            }
            if let Some(detail) = generic_failure {
                self.state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .fail(detail);
            }
        }
        let detail = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sticky_failure
            .clone();
        detail.map(|detail| {
            if self.lifecycle == LifecycleState::Running {
                self.resume_running = true;
            }
            self.lifecycle = LifecycleState::Lost;
            self.set_failed_health(detail.clone());
            IoError::AdapterFailure {
                detail,
                remediation: Some(Remediation::RestartAdapter),
            }
        })
    }

    fn shutdown(&mut self) -> Result<(), IoError> {
        #[cfg(target_os = "macos")]
        {
            let Some(capture) = self.capture.as_mut() else {
                return Ok(());
            };
            capture.stop_token.store(true, Ordering::Release);
            let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
            let mut failure = None;
            let mut child_reaped = false;
            match capture.child.try_wait() {
                Ok(Some(_)) => child_reaped = true,
                Ok(None) => {
                    if let Err(error) = capture.child.kill() {
                        failure = Some(format!("failed to kill audio helper: {error}"));
                    }
                }
                Err(error) => failure = Some(format!("failed to query audio helper: {error}")),
            }
            while !child_reaped && Instant::now() < deadline {
                match capture.child.try_wait() {
                    Ok(Some(_)) => child_reaped = true,
                    Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                    Err(error) => {
                        failure
                            .get_or_insert_with(|| format!("failed to reap audio helper: {error}"));
                        break;
                    }
                }
            }
            if !child_reaped {
                failure.get_or_insert_with(|| {
                    "audio helper did not exit before shutdown deadline".to_owned()
                });
            }
            while capture.workers.iter().any(|worker| !worker.is_finished())
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            let mut index = 0;
            while index < capture.workers.len() {
                if capture.workers[index].is_finished() {
                    let worker = capture.workers.remove(index);
                    if worker.join().is_err() {
                        failure.get_or_insert_with(|| "audio helper worker panicked".to_owned());
                    }
                } else {
                    index += 1;
                }
            }
            if !capture.workers.is_empty() {
                failure.get_or_insert_with(|| {
                    "audio helper worker missed shutdown deadline".to_owned()
                });
            }
            let cleanup_complete = child_reaped && capture.workers.is_empty();
            if cleanup_complete {
                self.capture = None;
            }
            if let Some(detail) = failure {
                return Err(adapter_failure(detail));
            }
            if !cleanup_complete {
                return Err(adapter_failure("audio helper cleanup is incomplete"));
            }
        }
        Ok(())
    }

    fn transition_to_signal_lost(&mut self, detail: String) {
        self.resume_running = true;
        self.lifecycle = LifecycleState::Lost;
        self.health = EndpointHealth {
            state: EndpointHealthState::SignalLost,
            detail: Some(detail),
            remediation: Some(Remediation::ReconnectDevice),
        };
    }

    fn set_failed_health(&mut self, detail: String) {
        self.health = EndpointHealth {
            state: EndpointHealthState::Failed,
            detail: Some(detail),
            remediation: Some(Remediation::RestartAdapter),
        };
    }
}

impl MediaSource for MacosAudioSource {
    type Media = AudioBlock;

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
        self.validate_open(&options)?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.capacity = options.queue_capacity.get();
        state.blocks.clear();
        state.telemetry = AudioTelemetry::default();
        state.sticky_failure = None;
        #[cfg(target_os = "macos")]
        state.stderr.clear();
        state.last_activity = None;
        drop(state);
        self.options = Some(options);
        self.resume_running = false;
        self.lifecycle = LifecycleState::Open;
        self.health = EndpointHealth::HEALTHY;
        Ok(())
    }

    fn start(&mut self) -> Result<(), IoError> {
        if self.lifecycle != LifecycleState::Open {
            return Err(invalid_state("start", self.lifecycle));
        }
        self.shutdown()?;
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.blocks.clear();
            state.telemetry = AudioTelemetry::default();
            state.sticky_failure = None;
            #[cfg(target_os = "macos")]
            state.stderr.clear();
            state.last_activity = None;
        }
        #[cfg(target_os = "macos")]
        {
            let (capture, startup) = self.spawn_capture()?;
            self.capture = Some(capture);
            let startup_result = match startup.recv_timeout(STARTUP_TIMEOUT) {
                Ok(result) => result,
                Err(RecvTimeoutError::Timeout) => {
                    Err("audio helper did not emit capture magic within 10 seconds".to_owned())
                }
                Err(RecvTimeoutError::Disconnected) => {
                    Err("audio helper startup worker disconnected".to_owned())
                }
            };
            if let Err(detail) = startup_result {
                let cleanup = self.shutdown();
                return Err(match cleanup {
                    Ok(()) => adapter_failure(detail),
                    Err(error) => {
                        adapter_failure(format!("{detail}; startup cleanup also failed: {error}"))
                    }
                });
            }
            if let Some(error) = self.sticky_error() {
                let cleanup = self.shutdown();
                return Err(match cleanup {
                    Ok(()) => error,
                    Err(cleanup_error) => adapter_failure(format!(
                        "{error}; startup cleanup also failed: {cleanup_error}"
                    )),
                });
            }
        }
        #[cfg(not(target_os = "macos"))]
        self.spawn_capture()?;
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_activity = Some(Instant::now());
        self.resume_running = false;
        self.lifecycle = LifecycleState::Running;
        self.health = EndpointHealth::HEALTHY;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), IoError> {
        if self.lifecycle != LifecycleState::Running {
            return Err(invalid_state("stop", self.lifecycle));
        }
        self.shutdown()?;
        self.lifecycle = LifecycleState::Open;
        Ok(())
    }

    fn close(&mut self) -> Result<(), IoError> {
        if !matches!(self.lifecycle, LifecycleState::Open | LifecycleState::Lost) {
            return Err(invalid_state("close", self.lifecycle));
        }
        self.shutdown()?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.blocks.clear();
        state.telemetry.current = 0;
        state.sticky_failure = None;
        state.last_activity = None;
        drop(state);
        self.options = None;
        self.resume_running = false;
        self.lifecycle = LifecycleState::Closed;
        Ok(())
    }

    fn begin_recovery(&mut self) -> Result<(), IoError> {
        if self.lifecycle != LifecycleState::Lost {
            return Err(invalid_state("begin recovery", self.lifecycle));
        }
        self.shutdown()?;
        self.lifecycle = LifecycleState::Recovering;
        Ok(())
    }

    fn finish_recovery(&mut self) -> Result<(), IoError> {
        if self.lifecycle != LifecycleState::Recovering {
            return Err(invalid_state("finish recovery", self.lifecycle));
        }
        if !self.resume_running {
            self.lifecycle = LifecycleState::Open;
            self.health = EndpointHealth::HEALTHY;
            return Ok(());
        }
        self.lifecycle = LifecycleState::Open;
        if let Err(error) = self.start() {
            let detail = error.to_string();
            self.lifecycle = LifecycleState::Lost;
            self.resume_running = true;
            if !matches!(&error, IoError::SignalLost { .. }) {
                self.set_failed_health(detail);
            }
            return Err(error);
        }
        self.resume_running = false;
        Ok(())
    }

    fn try_receive(&mut self) -> Result<Option<MediaTransfer<Self::Media>>, IoError> {
        if self.lifecycle == LifecycleState::Lost {
            if let Some(error) = self.sticky_error() {
                return Err(error);
            }
            let policy = self
                .options
                .as_ref()
                .map_or(SignalLossPolicy::Stop, |options| options.signal_loss);
            return Err(IoError::SignalLost { policy });
        }
        if self.lifecycle != LifecycleState::Running {
            return Err(invalid_state("receive", self.lifecycle));
        }
        if let Some(error) = self.sticky_error() {
            return Err(error);
        }
        if self.lifecycle == LifecycleState::Lost {
            return Err(IoError::SignalLost {
                policy: SignalLossPolicy::Stop,
            });
        }
        let (block, last_activity) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (state.pop(), state.last_activity)
        };
        if let Some(block) = block {
            return Ok(Some(MediaTransfer::Live(block)));
        }
        if last_activity.is_some_and(|activity| activity.elapsed() >= SIGNAL_LOSS_TIMEOUT) {
            self.transition_to_signal_lost(
                "microphone produced no blocks before the activity deadline".to_owned(),
            );
            let policy = self
                .options
                .as_ref()
                .map_or(SignalLossPolicy::Stop, |options| options.signal_loss);
            return Err(IoError::SignalLost { policy });
        }
        Ok(None)
    }
}

impl Drop for MacosAudioSource {
    fn drop(&mut self) {
        if self.shutdown().is_err() {
            #[cfg(target_os = "macos")]
            if let Some(capture) = self.capture.take() {
                handoff_audio_capture(capture);
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn run_audio_helper(
    path: &Path,
    arguments: &[OsString],
    maximum: usize,
    timeout: Duration,
) -> Result<Vec<u8>, AudioError> {
    run_helper(path, arguments, maximum, timeout)
        .map_err(|error| AudioError::Helper(error.to_string()))
}

#[cfg(target_os = "macos")]
fn terminate_audio_capture(mut capture: AudioCaptureProcess) {
    capture.stop_token.store(true, Ordering::Release);
    let mut child_reaped = matches!(capture.child.try_wait(), Ok(Some(_)));
    if !child_reaped {
        let _ = capture.child.kill();
    }
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    while !child_reaped && Instant::now() < deadline {
        match capture.child.try_wait() {
            Ok(Some(_)) => child_reaped = true,
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => break,
        }
    }
    while capture.workers.iter().any(|worker| !worker.is_finished()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    if child_reaped && capture.workers.iter().all(JoinHandle::is_finished) {
        for worker in capture.workers {
            let _ = worker.join();
        }
    } else {
        handoff_audio_capture(capture);
    }
}

#[cfg(target_os = "macos")]
fn handoff_audio_capture(capture: AudioCaptureProcess) {
    let slot = Arc::new(Mutex::new(Some(capture)));
    let worker_slot = Arc::clone(&slot);
    let spawn = std::thread::Builder::new()
        .name("fm-audio-source-reaper".to_owned())
        .spawn(move || {
            if let Some(mut capture) = worker_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                capture.stop_token.store(true, Ordering::Release);
                let _ = capture.child.kill();
                let _ = capture.child.wait();
                for worker in capture.workers {
                    let _ = worker.join();
                }
            }
        });
    if spawn.is_err()
        && let Some(mut capture) = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    {
        capture.stop_token.store(true, Ordering::Release);
        let _ = capture.child.kill();
        let _ = capture.child.wait();
        for worker in capture.workers {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fm_io_api::DiscoveryEventKind;

    type TestDevice<'a> = (&'a str, &'a str, &'a [(u32, u8)]);

    fn discovery(permission: u8, devices: &[TestDevice<'_>]) -> Vec<u8> {
        let mut bytes = b"FMAUDD1\0".to_vec();
        bytes.push(permission);
        bytes.extend_from_slice(&u32::try_from(devices.len()).unwrap().to_le_bytes());
        for (id, name, formats) in devices {
            for value in [id, name] {
                bytes.extend_from_slice(&u32::try_from(value.len()).unwrap().to_le_bytes());
                bytes.extend_from_slice(value.as_bytes());
            }
            bytes.extend_from_slice(&u32::try_from(formats.len()).unwrap().to_le_bytes());
            for (sample_rate, channels) in *formats {
                bytes.extend_from_slice(&sample_rate.to_le_bytes());
                bytes.push(*channels);
            }
        }
        bytes
    }

    #[test]
    fn exact_audio_sources_have_stable_keys_and_independent_permission() {
        let adapter = MacosAudioAdapter::from_discovery_bytes(&discovery(
            1,
            &[("mic-a", "Microphone A", &[(48_000, 2), (44_100, 1)])],
        ))
        .unwrap();
        let snapshot = adapter.snapshot();
        assert_eq!(snapshot.sources.len(), 1);
        assert!(
            snapshot.sources[0]
                .stable_key
                .starts_with(AUDIO_STABLE_KEY_PREFIX)
        );
        assert_eq!(snapshot.sources[0].capabilities.formats.len(), 2);
        assert!(matches!(
            adapter.permission(),
            PermissionState::PromptRequired { .. }
        ));
        let mut source = adapter
            .open_audio_source_by_stable_key(&snapshot.sources[0].stable_key)
            .unwrap();
        assert!(
            source
                .exact_audio_format(SampleRate::new(48_000).unwrap(), 2)
                .is_some()
        );
        assert!(
            source
                .exact_audio_format(SampleRate::new(48_000).unwrap(), 1)
                .is_none()
        );
        assert!(matches!(
            source.open(OpenOptions {
                format: snapshot.sources[0].capabilities.formats[0].clone(),
                clock_domain: snapshot.sources[0].capabilities.clocks[0].domain,
                memory_domain: MemoryDomain::Cpu,
                queue_capacity: NonZeroUsize::new(1).unwrap(),
                signal_loss: SignalLossPolicy::Stop,
            }),
            Err(IoError::PermissionDenied { .. })
        ));
    }

    #[test]
    fn refresh_events_are_add_update_remove_ordered() {
        let mut adapter = MacosAudioAdapter::from_discovery_bytes(&discovery(
            0,
            &[
                ("mic-a", "A", &[(48_000, 2)]),
                ("mic-c", "C", &[(48_000, 1)]),
            ],
        ))
        .unwrap();
        adapter
            .refresh_from_discovery_bytes(&discovery(
                0,
                &[
                    ("mic-a", "A2", &[(48_000, 2)]),
                    ("mic-b", "B", &[(44_100, 1)]),
                ],
            ))
            .unwrap();
        assert!(matches!(
            adapter.next_event().unwrap().kind,
            DiscoveryEventKind::SourceAdded(_)
        ));
        assert!(matches!(
            adapter.next_event().unwrap().kind,
            DiscoveryEventKind::SourceUpdated(_)
        ));
        assert!(matches!(
            adapter.next_event().unwrap().kind,
            DiscoveryEventKind::SourceRemoved(_)
        ));
        assert!(adapter.next_event().is_none());
    }

    #[test]
    fn audio_queue_overrun_is_sticky_instead_of_dropping_samples() {
        let mut state = AudioQueueState {
            capacity: 1,
            ..AudioQueueState::default()
        };
        let timing = fm_frame::MediaTiming::new(
            fm_frame::OriginalTimestamp::new(
                fm_frame::MediaTimestamp::new(0),
                fm_frame::TimeBase::new(1, 48_000).unwrap(),
            ),
            fm_frame::NormalizedTimestamp::from_nanos(0),
            fm_frame::NormalizedDuration::from_nanos(20_833).unwrap(),
            ClockDomainId::new(NonZeroU128::new(7).unwrap()),
            fm_frame::SequenceNumber::new(0),
        )
        .unwrap();
        let block = AudioBlock::silence(
            timing,
            SampleRate::new(48_000).unwrap(),
            fm_types::ChannelLayout::stereo(),
            1,
        )
        .unwrap();
        state.push(block.clone(), 0);
        state.push(block, 0);
        assert_eq!(state.blocks.len(), 1);
        assert_eq!(state.telemetry.received, 2);
        assert_eq!(state.telemetry.overruns, 1);
        assert_eq!(
            state.sticky_failure.as_deref(),
            Some("microphone audio queue overrun")
        );
    }
}
