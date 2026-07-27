//! Safe Rust adapter for the macOS `AVFoundation` camera helper.
//!
//! The native boundary is a child process. No Objective-C or platform handle is
//! exposed to Rust, and all helper output is parsed with explicit bounds.

pub mod audio;
pub mod protocol;

pub use audio::{
    AudioError, AudioIdKind, AudioTelemetry, MacosAudioAdapter, MacosAudioAvailability,
    MacosAudioSource, deterministic_audio_id,
};

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    num::{NonZeroU128, NonZeroUsize},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
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
};

use fm_capabilities::StableId;
use fm_frame::{ClockDomainId, CpuVideoFrame, MediaFlags, MediaTiming};
use fm_io_api::{
    ClockCapability, DeviceId, Discovery, DiscoveryEvent, DiscoveryEventKind, DiscoverySnapshot,
    DriverState, EndpointCapabilities, EndpointHealth, EndpointHealthState, FallbackKind,
    FormatDescriptor, IoError, LifecycleState, MediaSource, MediaTransfer, MemoryDomain,
    OpenOptions, PermissionState, Remediation, SignalLossPolicy, SourceDescriptor, SourceId,
    TimestampCapabilities, TimestampQuality, TimestampValidationError, TransferLimits,
};
use fm_types::FrameRate;

use protocol::{HelperDevice, HelperDiscovery, HelperPermission, MAX_DISCOVERY_BYTES};

const MAX_QUEUE_CAPACITY: usize = 8;
const MAX_MEDIA_BYTES: usize = 64 * 1024 * 1024;
#[cfg(target_os = "macos")]
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(target_os = "macos")]
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const SIGNAL_LOSS_TIMEOUT: Duration = Duration::from_secs(2);
const RECOVERY_FRAME_TIMEOUT: Duration = Duration::from_secs(2);
const CAMERA_STABLE_KEY_PREFIX: &str = "macos.avfoundation.camera.v1.";
#[cfg(target_os = "macos")]
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(target_os = "macos")]
const PERMISSION_TIMEOUT: Duration = Duration::from_mins(5);
const FNV_OFFSET_BASIS_128: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
const FNV_PRIME_128: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MacosCameraAvailability {
    Available,
    UnsupportedPlatform,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CameraIdKind {
    Device,
    Source,
    CoreMediaClock,
}

impl CameraIdKind {
    const fn namespace(self) -> &'static [u8] {
        match self {
            Self::Device => b"freemix:macos:camera:device\0",
            Self::Source => b"freemix:macos:camera:source\0",
            Self::CoreMediaClock => b"freemix:macos:coremedia:clock\0",
        }
    }
}

/// Maps native identifiers with `FNV-1a-128` and a type-specific namespace.
/// The otherwise standard result maps zero to one, making it deterministically
/// nonzero. Adapter discovery rejects any collision instead of renaming IDs.
#[must_use]
pub fn deterministic_camera_id(kind: CameraIdKind, native_id: &str) -> NonZeroU128 {
    let mut hash = FNV_OFFSET_BASIS_128;
    for byte in kind.namespace().iter().chain(native_id.as_bytes()) {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME_128);
    }
    NonZeroU128::new(hash).unwrap_or(NonZeroU128::MIN)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CameraError {
    UnsupportedPlatform,
    Protocol(protocol::ProtocolError),
    Helper(String),
    IdCollision {
        kind: CameraIdKind,
        first: String,
        second: String,
    },
    DuplicateDeviceId(String),
    UnknownSource(SourceId),
    UnknownStableKey(String),
}

impl fmt::Display for CameraError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                formatter.write_str("AVFoundation camera capture is available only on macOS")
            }
            Self::Protocol(error) => write!(formatter, "camera helper protocol error: {error}"),
            Self::Helper(detail) => formatter.write_str(detail),
            Self::IdCollision {
                kind,
                first,
                second,
            } => write!(
                formatter,
                "{kind:?} identifier collision between `{}` and `{}`",
                escaped_text(first),
                escaped_text(second),
            ),
            Self::DuplicateDeviceId(id) => {
                write!(
                    formatter,
                    "duplicate camera device id `{}`",
                    escaped_text(id)
                )
            }
            Self::UnknownSource(id) => write!(formatter, "unknown camera source {id}"),
            Self::UnknownStableKey(key) => {
                write!(
                    formatter,
                    "unknown camera stable key `{}`",
                    escaped_text(key)
                )
            }
        }
    }
}

impl std::error::Error for CameraError {}

impl From<protocol::ProtocolError> for CameraError {
    fn from(error: protocol::ProtocolError) -> Self {
        Self::Protocol(error)
    }
}

fn escaped_text(value: &str) -> String {
    value.chars().flat_map(char::escape_default).collect()
}

#[derive(Clone)]
struct CameraRecord {
    #[cfg(target_os = "macos")]
    native_id: String,
    descriptor: SourceDescriptor,
}

pub struct MacosCameraAdapter {
    generation: u64,
    sources: BTreeMap<SourceId, CameraRecord>,
    events: VecDeque<DiscoveryEvent>,
    permission: PermissionState,
    #[cfg(target_os = "macos")]
    helper_path: PathBuf,
}

impl MacosCameraAdapter {
    #[must_use]
    pub const fn availability() -> MacosCameraAvailability {
        if cfg!(target_os = "macos") {
            MacosCameraAvailability::Available
        } else {
            MacosCameraAvailability::UnsupportedPlatform
        }
    }

    /// Discovers cameras without requesting permission, using the helper path
    /// embedded by `build.rs` for developer builds.
    ///
    /// # Errors
    ///
    /// Returns a platform, helper process, protocol, or identifier error.
    pub fn discover() -> Result<Self, CameraError> {
        #[cfg(target_os = "macos")]
        {
            Self::discover_with_helper(developer_helper_path())
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(CameraError::UnsupportedPlatform)
        }
    }

    /// Discovers cameras with an application-supplied packaged helper.
    ///
    /// # Errors
    ///
    /// Returns a helper process, protocol, or identifier error.
    #[cfg(target_os = "macos")]
    pub fn discover_with_helper(path: impl AsRef<Path>) -> Result<Self, CameraError> {
        let helper_path = path.as_ref().to_path_buf();
        let bytes = run_helper(
            &helper_path,
            &[OsString::from("discover")],
            MAX_DISCOVERY_BYTES,
            DISCOVERY_TIMEOUT,
        )?;
        Self::from_discovery_with_helper(&bytes, helper_path)
    }

    /// Portable parser seam used by protocol tests and framed helper fakes.
    ///
    /// # Errors
    ///
    /// Returns a protocol or identifier error.
    pub fn from_discovery_bytes(bytes: &[u8]) -> Result<Self, CameraError> {
        #[cfg(target_os = "macos")]
        return Self::from_discovery_with_helper(bytes, developer_helper_path());
        #[cfg(not(target_os = "macos"))]
        Self::from_discovery_common(bytes)
    }

    fn from_discovery_common(bytes: &[u8]) -> Result<Self, CameraError> {
        let parsed = protocol::parse_discovery(bytes)?;
        let sources = records_from_discovery(&parsed, deterministic_camera_id)?;
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
    fn from_discovery_with_helper(bytes: &[u8], helper_path: PathBuf) -> Result<Self, CameraError> {
        let mut adapter = Self::from_discovery_common(bytes)?;
        adapter.helper_path = helper_path;
        Ok(adapter)
    }

    /// Refreshes the current snapshot and enqueues deterministic add, update,
    /// then remove events, each ordered by stable source ID.
    ///
    /// # Errors
    ///
    /// Returns a platform, helper process, protocol, or identifier error.
    pub fn refresh(&mut self) -> Result<(), CameraError> {
        #[cfg(target_os = "macos")]
        {
            let bytes = run_helper(
                &self.helper_path,
                &[OsString::from("discover")],
                MAX_DISCOVERY_BYTES,
                DISCOVERY_TIMEOUT,
            )?;
            self.refresh_from_discovery_bytes(&bytes)
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(CameraError::UnsupportedPlatform)
        }
    }

    /// Applies one complete discovery response through the same refresh path.
    ///
    /// # Errors
    ///
    /// Returns a protocol or identifier error without changing the snapshot.
    pub fn refresh_from_discovery_bytes(&mut self, bytes: &[u8]) -> Result<(), CameraError> {
        let parsed = protocol::parse_discovery(bytes)?;
        let next = records_from_discovery(&parsed, deterministic_camera_id)?;

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
        let removed: Vec<_> = self
            .sources
            .keys()
            .filter(|id| !next.contains_key(id))
            .copied()
            .collect();
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

    /// Explicitly invokes the developer-build helper's permission prompt.
    /// Discovery and refresh never call this operation.
    ///
    /// # Errors
    ///
    /// Returns a platform, helper process, or protocol error.
    pub fn request_camera_permission() -> Result<PermissionState, CameraError> {
        #[cfg(target_os = "macos")]
        {
            Self::request_camera_permission_with_helper(developer_helper_path())
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(CameraError::UnsupportedPlatform)
        }
    }

    /// Explicitly requests permission with an application-supplied helper.
    ///
    /// # Errors
    ///
    /// Returns the permission helper error without replacing it with a later
    /// discovery result, or returns a discovery/protocol error.
    #[cfg(target_os = "macos")]
    pub fn request_camera_permission_with_helper(
        path: impl AsRef<Path>,
    ) -> Result<PermissionState, CameraError> {
        let path = path.as_ref();
        run_helper(
            path,
            &[OsString::from("request-permission")],
            0,
            PERMISSION_TIMEOUT,
        )?;
        let bytes = run_helper(
            path,
            &[OsString::from("discover")],
            MAX_DISCOVERY_BYTES,
            DISCOVERY_TIMEOUT,
        )?;
        let parsed = protocol::parse_discovery(&bytes)?;
        Ok(permission_state(parsed.permission))
    }

    /// Creates a source for exactly the native device represented by `id`.
    ///
    /// # Errors
    ///
    /// Returns [`CameraError::UnknownSource`] rather than substituting a device.
    pub fn open_video_source(&self, id: SourceId) -> Result<CameraVideoSource, CameraError> {
        let record = self
            .sources
            .get(&id)
            .ok_or(CameraError::UnknownSource(id))?;
        Ok(CameraVideoSource::new(
            record.clone(),
            #[cfg(target_os = "macos")]
            self.helper_path.clone(),
        ))
    }

    /// Creates a source for exactly the adapter-qualified persisted key.
    ///
    /// # Errors
    ///
    /// Returns [`CameraError::UnknownStableKey`] rather than substituting a
    /// similarly named or differently ordered device.
    pub fn open_video_source_by_stable_key(
        &self,
        stable_key: &str,
    ) -> Result<CameraVideoSource, CameraError> {
        let record = self
            .sources
            .values()
            .find(|record| record.descriptor.stable_key == stable_key)
            .ok_or_else(|| CameraError::UnknownStableKey(stable_key.to_owned()))?;
        Ok(CameraVideoSource::new(
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

impl Discovery for MacosCameraAdapter {
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
    discovery: &HelperDiscovery,
    hasher: impl Fn(CameraIdKind, &str) -> NonZeroU128,
) -> Result<BTreeMap<SourceId, CameraRecord>, CameraError> {
    let mut native_ids = BTreeSet::new();
    let mut device_hashes = BTreeMap::<NonZeroU128, String>::new();
    let mut source_hashes = BTreeMap::<NonZeroU128, String>::new();
    let mut records = BTreeMap::new();
    for device in &discovery.devices {
        if !native_ids.insert(device.id.clone()) {
            return Err(CameraError::DuplicateDeviceId(device.id.clone()));
        }
        let device_hash = hasher(CameraIdKind::Device, &device.id);
        reject_collision(
            &mut device_hashes,
            CameraIdKind::Device,
            device_hash,
            &device.id,
        )?;
        let source_hash = hasher(CameraIdKind::Source, &device.id);
        reject_collision(
            &mut source_hashes,
            CameraIdKind::Source,
            source_hash,
            &device.id,
        )?;
        let source_id = SourceId::new(source_hash);
        records.insert(
            source_id,
            CameraRecord {
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
    kind: CameraIdKind,
    hash: NonZeroU128,
    native_id: &str,
) -> Result<(), CameraError> {
    if let Some(first) = seen.insert(hash, native_id.to_owned())
        && first != native_id
    {
        return Err(CameraError::IdCollision {
            kind,
            first,
            second: native_id.to_owned(),
        });
    }
    Ok(())
}

fn source_descriptor(
    device: &HelperDevice,
    device_id: DeviceId,
    source_id: SourceId,
    permission: PermissionState,
) -> SourceDescriptor {
    SourceDescriptor {
        id: source_id,
        device_id,
        stable_key: format!("{CAMERA_STABLE_KEY_PREFIX}{source_id}"),
        name: device.name.clone(),
        capabilities: EndpointCapabilities {
            formats: device.formats.iter().map(raw_format).collect(),
            clocks: vec![coremedia_clock()],
            memory_domains: vec![MemoryDomain::Cpu],
            transfer: TransferLimits::new(
                NonZeroUsize::new(MAX_QUEUE_CAPACITY).expect("nonzero queue limit"),
                NonZeroUsize::new(MAX_MEDIA_BYTES).expect("nonzero media limit"),
            ),
        },
        permission,
        driver: DriverState::Ready,
    }
}

fn stable_id(value: &str) -> StableId {
    StableId::new(value).expect("camera descriptor keys are valid stable IDs")
}

fn raw_format(format: &protocol::HelperFormat) -> FormatDescriptor {
    let mut descriptor = FormatDescriptor::new(stable_id("video.raw"))
        .with_field(stable_id("width"), u64::from(format.width))
        .with_field(stable_id("height"), u64::from(format.height))
        .with_field(
            stable_id("fps-numerator"),
            u64::from(format.frame_rate.numerator()),
        )
        .with_field(
            stable_id("fps-denominator"),
            u64::from(format.frame_rate.denominator()),
        )
        .with_field(stable_id("pixel-format"), "bgra8");
    if format.frame_rate.denominator() == 1 {
        descriptor =
            descriptor.with_field(stable_id("fps"), u64::from(format.frame_rate.numerator()));
    }
    descriptor
}

fn coremedia_clock() -> ClockCapability {
    ClockCapability {
        domain: ClockDomainId::new(deterministic_camera_id(
            CameraIdKind::CoreMediaClock,
            "monotonic",
        )),
        timestamps: TimestampCapabilities {
            quality: TimestampQuality::Monotonic,
            resolution_nanos: NonZeroU128::new(1_000_000).unwrap_or(NonZeroU128::MIN),
            max_error_nanos: None,
            monotonic: true,
        },
        can_follow_external: false,
    }
}

fn permission_state(permission: HelperPermission) -> PermissionState {
    match permission {
        HelperPermission::Granted => PermissionState::Granted,
        HelperPermission::PromptRequired => PermissionState::PromptRequired {
            remediation: Remediation::RequestPermission,
        },
        HelperPermission::Denied => PermissionState::Denied {
            remediation: Remediation::OpenSystemSettings,
        },
        HelperPermission::Restricted => PermissionState::Restricted {
            remediation: Remediation::ContactAdministrator,
        },
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CameraTelemetry {
    pub received: u64,
    pub dropped: u64,
    pub native_dropped: u64,
    pub continuity_rejected: u64,
    pub recovery_timeout_discarded: u64,
    pub current: usize,
    pub peak: usize,
}

#[derive(Default)]
struct QueueState {
    frames: VecDeque<CpuVideoFrame>,
    capacity: usize,
    telemetry: CameraTelemetry,
    native_dropped_base: u64,
    accepting_frames: bool,
    sticky_failure: Option<String>,
    #[cfg(target_os = "macos")]
    stderr: Vec<u8>,
    last_activity: Option<Instant>,
}

#[derive(Clone, Copy)]
struct RecoveryContinuity {
    clock: ClockDomainId,
    previous_pts_nanos: Option<i64>,
}

impl QueueState {
    fn push(&mut self, frame: CpuVideoFrame, native_dropped_total: u64) {
        self.telemetry.received = self.telemetry.received.saturating_add(1);
        self.telemetry.native_dropped = self
            .native_dropped_base
            .saturating_add(native_dropped_total);
        if self.frames.len() >= self.capacity {
            self.frames.pop_front();
            self.telemetry.dropped = self.telemetry.dropped.saturating_add(1);
        }
        self.frames.push_back(frame);
        self.telemetry.current = self.frames.len();
        self.telemetry.peak = self.telemetry.peak.max(self.frames.len());
    }

    fn pop(&mut self) -> Option<CpuVideoFrame> {
        let frame = self.frames.pop_front();
        self.telemetry.current = self.frames.len();
        frame
    }

    fn push_from_worker(&mut self, frame: CpuVideoFrame, native_dropped_total: u64) -> bool {
        if !self.accepting_frames {
            return false;
        }
        self.push(frame, native_dropped_total);
        true
    }

    fn fail(&mut self, detail: impl Into<String>) {
        if self.sticky_failure.is_none() {
            self.sticky_failure = Some(detail.into());
        }
    }
}

#[cfg(target_os = "macos")]
struct CaptureProcess {
    child: Child,
    workers: Vec<JoinHandle<()>>,
    stop_token: Arc<AtomicBool>,
}

pub struct CameraVideoSource {
    descriptor: SourceDescriptor,
    #[cfg(target_os = "macos")]
    native_id: String,
    #[cfg(target_os = "macos")]
    helper_path: PathBuf,
    lifecycle: LifecycleState,
    health: EndpointHealth,
    options: Option<OpenOptions>,
    state: Arc<Mutex<QueueState>>,
    resume_running: bool,
    last_delivered: Option<CpuVideoFrame>,
    pending_recovery: Option<RecoveryContinuity>,
    recovery_deadline: Option<Instant>,
    map_recovery_sequences: bool,
    recovery_sequence_offset: Option<i128>,
    #[cfg(target_os = "macos")]
    capture: Option<CaptureProcess>,
    #[cfg(test)]
    force_shutdown_failure: bool,
}

impl CameraVideoSource {
    fn new(record: CameraRecord, #[cfg(target_os = "macos")] helper_path: PathBuf) -> Self {
        Self {
            descriptor: record.descriptor,
            #[cfg(target_os = "macos")]
            native_id: record.native_id,
            #[cfg(target_os = "macos")]
            helper_path,
            lifecycle: LifecycleState::Closed,
            health: EndpointHealth::HEALTHY,
            options: None,
            state: Arc::new(Mutex::new(QueueState::default())),
            resume_running: false,
            last_delivered: None,
            pending_recovery: None,
            recovery_deadline: None,
            map_recovery_sequences: false,
            recovery_sequence_offset: None,
            #[cfg(target_os = "macos")]
            capture: None,
            #[cfg(test)]
            force_shutdown_failure: false,
        }
    }

    #[must_use]
    pub fn telemetry(&self) -> CameraTelemetry {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .telemetry
    }

    /// Returns the exact advertised BGRA mode, without nearest-mode fallback.
    #[must_use]
    pub fn exact_video_format(
        &self,
        width: u32,
        height: u32,
        frames_per_second: u32,
    ) -> Option<FormatDescriptor> {
        let frame_rate = FrameRate::new(frames_per_second, 1).ok()?;
        self.exact_video_format_at_rate(width, height, frame_rate)
    }

    /// Returns the exact advertised BGRA mode at a rational rate, without
    /// nearest-mode fallback.
    #[must_use]
    pub fn exact_video_format_at_rate(
        &self,
        width: u32,
        height: u32,
        frame_rate: FrameRate,
    ) -> Option<FormatDescriptor> {
        self.descriptor
            .capabilities
            .formats
            .iter()
            .find(|format| exact_format_values(format) == Ok((width, height, frame_rate)))
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
        if options.queue_capacity.get() > MAX_QUEUE_CAPACITY {
            return Err(IoError::QueueCapacityUnsupported {
                requested: options.queue_capacity.get(),
                maximum: MAX_QUEUE_CAPACITY,
            });
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn start_capture(&mut self, preserve_telemetry: bool) -> Result<(), IoError> {
        if self.lifecycle != LifecycleState::Open {
            return Err(invalid_state("start", self.lifecycle));
        }
        self.shutdown()?;
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.frames.clear();
            if preserve_telemetry {
                state.telemetry.current = 0;
                state.native_dropped_base = state.telemetry.native_dropped;
            } else {
                state.telemetry = CameraTelemetry::default();
                state.native_dropped_base = 0;
            }
            state.sticky_failure = None;
            state.accepting_frames = true;
            #[cfg(target_os = "macos")]
            state.stderr.clear();
            state.last_activity = None;
        }
        self.last_delivered = None;
        self.pending_recovery = None;
        self.recovery_deadline = None;
        self.map_recovery_sequences = false;
        self.recovery_sequence_offset = None;

        #[cfg(target_os = "macos")]
        {
            let (capture, startup) = self.spawn_capture()?;
            self.capture = Some(capture);
            let startup_result = match startup.recv_timeout(STARTUP_TIMEOUT) {
                Ok(result) => result,
                Err(RecvTimeoutError::Timeout) => {
                    Err("camera helper did not emit capture magic within 10 seconds".to_owned())
                }
                Err(RecvTimeoutError::Disconnected) => {
                    Err("camera helper startup worker disconnected".to_owned())
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
            let worker_failure = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .sticky_failure
                .clone();
            let (signal_lost_exit, child_exit) = if let Some(capture) = &mut self.capture {
                match capture.child.try_wait() {
                    Ok(Some(status)) if status.code() == Some(20) => (true, None),
                    Ok(Some(status)) => (
                        false,
                        Some(format!("camera helper exited during startup: {status}")),
                    ),
                    Ok(None) => (false, None),
                    Err(error) => (
                        false,
                        Some(format!("camera helper startup status failed: {error}")),
                    ),
                }
            } else {
                (
                    false,
                    Some("camera helper startup ownership was lost".to_owned()),
                )
            };
            if signal_lost_exit {
                self.lifecycle = LifecycleState::Running;
                self.transition_to_signal_lost(
                    "camera helper reported source signal loss during startup".to_owned(),
                );
                let policy = self
                    .options
                    .as_ref()
                    .map_or(SignalLossPolicy::Stop, |options| options.signal_loss);
                return Err(IoError::SignalLost { policy });
            }
            if let Some(detail) = worker_failure.or(child_exit) {
                let cleanup = self.shutdown();
                return Err(match cleanup {
                    Ok(()) => adapter_failure(detail),
                    Err(error) => {
                        adapter_failure(format!("{detail}; startup cleanup also failed: {error}"))
                    }
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
                        signal_lost = Some("camera helper reported source signal loss".to_owned());
                    }
                    Ok(Some(status)) => {
                        let stderr = self
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .stderr
                            .clone();
                        let suffix = escaped_diagnostic(&stderr);
                        generic_failure =
                            Some(format!("camera helper exited with {status}: {suffix}"));
                    }
                    Err(error) => {
                        generic_failure = Some(format!("camera helper status failed: {error}"));
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
            .take();
        detail.map(|detail| {
            let error = IoError::AdapterFailure {
                detail,
                remediation: Some(Remediation::RestartAdapter),
            };
            self.terminal_capture_error(&error)
        })
    }

    #[cfg(target_os = "macos")]
    #[allow(clippy::too_many_lines)]
    fn spawn_capture(&self) -> Result<(CaptureProcess, Receiver<Result<(), String>>), IoError> {
        let options = self
            .options
            .as_ref()
            .ok_or_else(|| invalid_state("start", self.lifecycle))?;
        let (width, height, frame_rate) = exact_format_values(&options.format)?;
        let mut child = Command::new(&self.helper_path)
            .arg("capture")
            .arg(&self.native_id)
            .arg(width.to_string())
            .arg(height.to_string())
            .arg(frame_rate.numerator().to_string())
            .arg(frame_rate.denominator().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| adapter_failure(format!("failed to start camera helper: {error}")))?;
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            terminate_detached_capture(CaptureProcess {
                child,
                workers: Vec::new(),
                stop_token: Arc::new(AtomicBool::new(true)),
            });
            return Err(adapter_failure("camera helper pipes were not available"));
        };

        let stop_token = Arc::new(AtomicBool::new(false));
        let (startup_sender, startup_receiver) = sync_channel(1);
        let state = Arc::clone(&self.state);
        let worker_stop = Arc::clone(&stop_token);
        let clock = options.clock_domain;
        let stdout_worker = std::thread::Builder::new()
            .name("fm-camera-stdout".to_owned())
            .spawn(move || {
                let mut reader = match protocol::FrameReader::new_with_dimensions(
                    stdout, clock, width, height,
                ) {
                    Ok(reader) => {
                        let _ = startup_sender.send(Ok(()));
                        reader
                    }
                    Err(error) => {
                        let detail = format!("camera helper startup failed: {error}");
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
                    match reader.read_captured_frame() {
                        Ok(Some(captured)) => {
                            let _ = state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push_from_worker(captured.frame, captured.native_dropped_total);
                        }
                        Ok(None) => return,
                        Err(error) => break error,
                    }
                };
                if !worker_stop.load(Ordering::Acquire) {
                    state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .fail(format!("camera helper frame stream failed: {error}"));
                }
            });
        let stdout_worker = match stdout_worker {
            Ok(worker) => worker,
            Err(error) => {
                terminate_detached_capture(CaptureProcess {
                    child,
                    workers: Vec::new(),
                    stop_token,
                });
                return Err(adapter_failure(format!(
                    "failed to start camera stdout worker: {error}"
                )));
            }
        };

        let state = Arc::clone(&self.state);
        let worker_stop = Arc::clone(&stop_token);
        let stderr_worker = std::thread::Builder::new()
            .name("fm-camera-stderr".to_owned())
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
                                    .fail(format!("camera helper stderr failed: {error}"));
                            }
                            break;
                        }
                    }
                }
            });
        let stderr_worker = match stderr_worker {
            Ok(worker) => worker,
            Err(error) => {
                terminate_detached_capture(CaptureProcess {
                    child,
                    workers: vec![stdout_worker],
                    stop_token,
                });
                return Err(adapter_failure(format!(
                    "failed to start camera stderr worker: {error}"
                )));
            }
        };
        Ok((
            CaptureProcess {
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
            "AVFoundation camera capture is available only on macOS",
        ))
    }

    fn shutdown(&mut self) -> Result<(), IoError> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .accepting_frames = false;
        #[cfg(test)]
        if self.force_shutdown_failure {
            return Err(adapter_failure("injected camera helper shutdown failure"));
        }
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
                        failure = Some(format!("failed to kill camera helper: {error}"));
                    }
                }
                Err(error) => failure = Some(format!("failed to query camera helper: {error}")),
            }
            while !child_reaped && Instant::now() < deadline {
                match capture.child.try_wait() {
                    Ok(Some(_)) => child_reaped = true,
                    Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                    Err(error) => {
                        failure.get_or_insert_with(|| {
                            format!("failed to reap camera helper: {error}")
                        });
                        break;
                    }
                }
            }
            if !child_reaped {
                failure.get_or_insert_with(|| {
                    "camera helper did not exit before shutdown deadline".to_owned()
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
                        failure.get_or_insert_with(|| "camera helper worker panicked".to_owned());
                    }
                } else {
                    index += 1;
                }
            }
            if !capture.workers.is_empty() {
                failure.get_or_insert_with(|| {
                    "camera helper worker missed shutdown deadline".to_owned()
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
                return Err(adapter_failure("camera helper cleanup is incomplete"));
            }
        }
        Ok(())
    }

    fn transition_to_signal_lost(&mut self, detail: String) {
        self.resume_running = true;
        self.pending_recovery = None;
        self.recovery_deadline = None;
        self.lifecycle = LifecycleState::Lost;
        self.health = EndpointHealth {
            state: EndpointHealthState::SignalLost,
            detail: Some(detail),
            remediation: Some(Remediation::ReconnectDevice),
        };
    }

    fn lost_transfer(&mut self) -> Result<Option<MediaTransfer<CpuVideoFrame>>, IoError> {
        let sticky_failure = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sticky_failure
            .clone();
        if let Some(detail) = sticky_failure {
            self.set_failed_health(detail.clone());
            return Err(IoError::AdapterFailure {
                detail,
                remediation: Some(Remediation::RestartAdapter),
            });
        }
        let policy = self
            .options
            .as_ref()
            .map_or(SignalLossPolicy::Stop, |options| options.signal_loss);
        if policy == SignalLossPolicy::Hold
            && let Some(frame) = self.last_delivered.clone()
        {
            return Ok(Some(MediaTransfer::Fallback {
                kind: FallbackKind::Hold,
                media: frame,
            }));
        }
        Err(IoError::SignalLost { policy })
    }

    fn set_failed_health(&mut self, detail: String) {
        self.health = EndpointHealth {
            state: EndpointHealthState::Failed,
            detail: Some(detail),
            remediation: Some(Remediation::RestartAdapter),
        };
    }

    fn prepare_frame(
        &self,
        frame: CpuVideoFrame,
        recovering: bool,
    ) -> Result<(CpuVideoFrame, Option<i128>), IoError> {
        let timing = frame.timing();
        if recovering {
            let pending = self
                .pending_recovery
                .expect("a started recovery has a continuity anchor");
            if timing.clock_domain() != pending.clock {
                return Err(IoError::MalformedTimestamp(
                    TimestampValidationError::WrongClock {
                        expected: pending.clock,
                        actual: timing.clock_domain(),
                    },
                ));
            }
            let actual_nanos = timing.presentation_timestamp().as_nanos();
            if let Some(previous_nanos) = pending.previous_pts_nanos
                && actual_nanos <= previous_nanos
            {
                return Err(IoError::MalformedTimestamp(
                    TimestampValidationError::NonMonotonic {
                        previous_nanos,
                        actual_nanos,
                    },
                ));
            }
        }
        if !recovering && !self.map_recovery_sequences {
            return Ok((frame, None));
        }

        let mut new_offset = None;
        let sequence = if self.map_recovery_sequences {
            let offset = if let Some(offset) = self.recovery_sequence_offset {
                offset
            } else {
                let previous = self
                    .last_delivered
                    .as_ref()
                    .expect("sequence mapping requires a delivered anchor");
                let next = previous
                    .timing()
                    .sequence()
                    .checked_next()
                    .ok_or_else(|| adapter_failure("camera adapter sequence overflow"))?;
                let offset = i128::from(next.get()) - i128::from(timing.sequence().get());
                new_offset = Some(offset);
                offset
            };
            let mapped = i128::from(timing.sequence().get())
                .checked_add(offset)
                .and_then(|sequence| u64::try_from(sequence).ok())
                .ok_or_else(|| adapter_failure("camera adapter sequence overflow"))?;
            fm_frame::SequenceNumber::new(mapped)
        } else {
            timing.sequence()
        };
        let mut flags = timing.flags();
        if recovering {
            flags |= MediaFlags::DISCONTINUITY;
        }
        let mut replacement_timing = MediaTiming::new(
            timing.original_timestamp(),
            timing.presentation_timestamp(),
            timing.duration(),
            timing.clock_domain(),
            sequence,
        )
        .map_err(|error| adapter_failure(format!("camera frame timing is invalid: {error}")))?
        .with_flags(flags);
        if let Some(capture_timestamp) = timing.capture_timestamp() {
            replacement_timing = replacement_timing.with_capture_timestamp(capture_timestamp);
        }
        if let Some(timecode) = timing.timecode() {
            replacement_timing = replacement_timing.with_timecode(timecode);
        }
        let metadata = frame.metadata();
        let frame = CpuVideoFrame::new(replacement_timing, frame.into_payload());
        let frame = if let Some(metadata) = metadata {
            frame.with_metadata(metadata).map_err(|error| {
                adapter_failure(format!("camera frame metadata is invalid: {error}"))
            })?
        } else {
            frame
        };
        Ok((frame, new_offset))
    }

    fn accept_frame(&mut self, frame: CpuVideoFrame) -> MediaTransfer<CpuVideoFrame> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_activity = Some(Instant::now());
        self.last_delivered = Some(frame.clone());
        MediaTransfer::Live(frame)
    }

    fn recovery_timeout_transfer(
        &mut self,
    ) -> Result<Option<MediaTransfer<CpuVideoFrame>>, IoError> {
        let cleanup = self.shutdown();
        self.pending_recovery = None;
        self.recovery_deadline = None;
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let discarded = u64::try_from(state.frames.len()).unwrap_or(u64::MAX);
            state.telemetry.recovery_timeout_discarded = state
                .telemetry
                .recovery_timeout_discarded
                .saturating_add(discarded);
            state.frames.clear();
            state.telemetry.current = 0;
            state.sticky_failure = None;
        }
        if let Err(error) = cleanup {
            let detail = format!("camera recovery timed out; cleanup failed: {error}");
            self.resume_running = true;
            self.lifecycle = LifecycleState::Lost;
            self.set_failed_health(detail.clone());
            return Err(adapter_failure(detail));
        }
        self.transition_to_signal_lost(
            "camera recovery produced no valid frame before the deadline".to_owned(),
        );
        self.lost_transfer()
    }

    fn terminal_capture_error(&mut self, error: &IoError) -> IoError {
        let detail = match self.shutdown() {
            Ok(()) => error.to_string(),
            Err(cleanup) => format!("{error}; capture cleanup also failed: {cleanup}"),
        };
        self.pending_recovery = None;
        self.recovery_deadline = None;
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.frames.clear();
            state.telemetry.current = 0;
            state.sticky_failure = None;
        }
        self.resume_running = true;
        self.lifecycle = LifecycleState::Lost;
        self.set_failed_health(detail.clone());
        adapter_failure(detail)
    }

    #[cfg(test)]
    fn start_without_helper_for_test(&mut self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.last_activity = Some(Instant::now());
        state.accepting_frames = true;
        drop(state);
        self.lifecycle = LifecycleState::Running;
        self.health = EndpointHealth::HEALTHY;
    }

    #[cfg(test)]
    fn expire_activity_for_test(&mut self) {
        let elapsed = SIGNAL_LOSS_TIMEOUT + Duration::from_millis(1);
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_activity = Some(
            Instant::now()
                .checked_sub(elapsed)
                .unwrap_or_else(Instant::now),
        );
    }
}

impl MediaSource for CameraVideoSource {
    type Media = CpuVideoFrame;

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
        state.frames.clear();
        state.telemetry = CameraTelemetry::default();
        state.native_dropped_base = 0;
        state.sticky_failure = None;
        #[cfg(target_os = "macos")]
        state.stderr.clear();
        state.last_activity = None;
        drop(state);
        self.options = Some(options);
        self.last_delivered = None;
        self.pending_recovery = None;
        self.recovery_deadline = None;
        self.map_recovery_sequences = false;
        self.recovery_sequence_offset = None;
        self.resume_running = false;
        self.lifecycle = LifecycleState::Open;
        self.health = EndpointHealth::HEALTHY;
        Ok(())
    }

    fn start(&mut self) -> Result<(), IoError> {
        self.start_capture(false)
    }

    fn stop(&mut self) -> Result<(), IoError> {
        if !matches!(
            self.lifecycle,
            LifecycleState::Running | LifecycleState::Recovering | LifecycleState::Lost
        ) {
            return Err(invalid_state("stop", self.lifecycle));
        }
        self.shutdown()?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.frames.clear();
        state.telemetry.current = 0;
        state.sticky_failure = None;
        state.last_activity = None;
        drop(state);
        self.pending_recovery = None;
        self.recovery_deadline = None;
        self.resume_running = false;
        self.map_recovery_sequences = false;
        self.recovery_sequence_offset = None;
        self.lifecycle = LifecycleState::Open;
        self.health = EndpointHealth::HEALTHY;
        Ok(())
    }

    fn close(&mut self) -> Result<(), IoError> {
        if !matches!(
            self.lifecycle,
            LifecycleState::Open | LifecycleState::Lost | LifecycleState::Recovering
        ) {
            return Err(invalid_state("close", self.lifecycle));
        }
        self.shutdown()?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.frames.clear();
        state.telemetry.current = 0;
        state.sticky_failure = None;
        state.last_activity = None;
        drop(state);
        self.options = None;
        self.last_delivered = None;
        self.pending_recovery = None;
        self.recovery_deadline = None;
        self.resume_running = false;
        self.map_recovery_sequences = false;
        self.recovery_sequence_offset = None;
        self.lifecycle = LifecycleState::Closed;
        Ok(())
    }

    fn begin_recovery(&mut self) -> Result<(), IoError> {
        if self.lifecycle != LifecycleState::Lost {
            return Err(invalid_state("begin recovery", self.lifecycle));
        }
        self.shutdown()?;
        self.pending_recovery = None;
        self.recovery_deadline = None;
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
            self.resume_running = false;
            return Ok(());
        }
        let held_frame = self.last_delivered.clone();
        let has_continuity_anchor = held_frame.is_some();
        let recovery_health = self.health.clone();
        let map_recovery_sequences = self.map_recovery_sequences;
        let recovery_sequence_offset = self.recovery_sequence_offset;
        let pending_recovery = RecoveryContinuity {
            clock: self
                .options
                .as_ref()
                .expect("a recovering source remains open")
                .clock_domain,
            previous_pts_nanos: held_frame
                .as_ref()
                .map(|frame| frame.timing().presentation_timestamp().as_nanos()),
        };
        self.lifecycle = LifecycleState::Open;
        if let Err(error) = self.start_capture(true) {
            self.last_delivered = held_frame;
            self.map_recovery_sequences = map_recovery_sequences;
            self.recovery_sequence_offset = recovery_sequence_offset;
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .sticky_failure = None;
            let detail = error.to_string();
            self.lifecycle = LifecycleState::Lost;
            self.resume_running = true;
            if !matches!(&error, IoError::SignalLost { .. }) {
                self.set_failed_health(detail);
            }
            return Err(error);
        }
        self.last_delivered = held_frame;
        self.pending_recovery = Some(pending_recovery);
        self.recovery_deadline = Some(Instant::now() + RECOVERY_FRAME_TIMEOUT);
        self.map_recovery_sequences = has_continuity_anchor;
        self.recovery_sequence_offset = None;
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_activity = None;
        self.lifecycle = LifecycleState::Recovering;
        self.health = recovery_health;
        self.resume_running = true;
        Ok(())
    }

    fn try_receive(&mut self) -> Result<Option<MediaTransfer<Self::Media>>, IoError> {
        if self.lifecycle == LifecycleState::Lost {
            if let Some(error) = self.sticky_error() {
                return Err(error);
            }
            return self.lost_transfer();
        }
        if self.lifecycle == LifecycleState::Recovering {
            if let Some(error) = self.sticky_error() {
                return Err(error);
            }
            if self.lifecycle == LifecycleState::Lost {
                return self.lost_transfer();
            }
            if self.pending_recovery.is_none() {
                return self.lost_transfer();
            }
            loop {
                let frame = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pop();
                if let Some(frame) = frame {
                    let (frame, new_offset) = match self.prepare_frame(frame, true) {
                        Ok(prepared) => prepared,
                        Err(IoError::MalformedTimestamp(_)) => {
                            let mut state = self
                                .state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            state.telemetry.continuity_rejected =
                                state.telemetry.continuity_rejected.saturating_add(1);
                            continue;
                        }
                        Err(error) => return Err(self.terminal_capture_error(&error)),
                    };
                    if let Some(offset) = new_offset {
                        self.recovery_sequence_offset = Some(offset);
                    }
                    self.pending_recovery = None;
                    self.recovery_deadline = None;
                    self.resume_running = false;
                    self.lifecycle = LifecycleState::Running;
                    self.health = EndpointHealth::HEALTHY;
                    return Ok(Some(self.accept_frame(frame)));
                }
                if self
                    .recovery_deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    return self.recovery_timeout_transfer();
                }
                return self.lost_transfer();
            }
        }
        if self.lifecycle != LifecycleState::Running {
            return Err(invalid_state("receive", self.lifecycle));
        }
        if let Some(error) = self.sticky_error() {
            return Err(error);
        }
        if self.lifecycle == LifecycleState::Lost {
            return self.lost_transfer();
        }
        let (frame, last_activity) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (state.pop(), state.last_activity)
        };
        if let Some(frame) = frame {
            let (frame, new_offset) = match self.prepare_frame(frame, false) {
                Ok(prepared) => prepared,
                Err(error) => return Err(self.terminal_capture_error(&error)),
            };
            if let Some(offset) = new_offset {
                self.recovery_sequence_offset = Some(offset);
            }
            return Ok(Some(self.accept_frame(frame)));
        }
        if last_activity.is_some_and(|activity| activity.elapsed() >= SIGNAL_LOSS_TIMEOUT) {
            self.transition_to_signal_lost(
                "camera produced no frames before the activity deadline".to_owned(),
            );
            return self.lost_transfer();
        }
        Ok(None)
    }
}

impl Drop for CameraVideoSource {
    fn drop(&mut self) {
        if self.shutdown().is_err() {
            #[cfg(target_os = "macos")]
            if let Some(capture) = self.capture.take() {
                handoff_capture_cleanup(capture);
            }
        }
    }
}

fn invalid_state(operation: &'static str, state: LifecycleState) -> IoError {
    IoError::InvalidState { operation, state }
}

fn adapter_failure(detail: impl Into<String>) -> IoError {
    IoError::AdapterFailure {
        detail: detail.into(),
        remediation: Some(Remediation::RestartAdapter),
    }
}

fn exact_format_values(format: &FormatDescriptor) -> Result<(u32, u32, FrameRate), IoError> {
    use fm_capabilities::FormatValue;

    let unsigned = |name: &str| match format.fields.get(&stable_id(name)) {
        Some(FormatValue::Unsigned(value)) => {
            u32::try_from(*value).map_err(|_| IoError::UnsupportedFormat)
        }
        _ => Err(IoError::UnsupportedFormat),
    };
    match format.fields.get(&stable_id("pixel-format")) {
        Some(FormatValue::Text(value)) if value == "bgra8" => {}
        _ => return Err(IoError::UnsupportedFormat),
    }
    let numerator = unsigned("fps-numerator")?;
    let denominator = unsigned("fps-denominator")?;
    let frame_rate =
        FrameRate::new(numerator, denominator).map_err(|_| IoError::UnsupportedFormat)?;
    if frame_rate.numerator() != numerator || frame_rate.denominator() != denominator {
        return Err(IoError::UnsupportedFormat);
    }
    if denominator == 1 && unsigned("fps")? != numerator {
        return Err(IoError::UnsupportedFormat);
    }
    Ok((unsigned("width")?, unsigned("height")?, frame_rate))
}

#[cfg(target_os = "macos")]
struct BoundedRead {
    bytes: Vec<u8>,
    exceeded: bool,
}

#[cfg(target_os = "macos")]
fn read_bounded(mut reader: impl Read, maximum: usize) -> std::io::Result<BoundedRead> {
    let mut bytes = Vec::with_capacity(maximum.min(8192));
    let mut exceeded = false;
    let mut buffer = [0; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                let available = maximum.saturating_sub(bytes.len());
                bytes.extend_from_slice(&buffer[..count.min(available)]);
                if count > available {
                    exceeded = true;
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(BoundedRead { bytes, exceeded })
}

#[cfg(target_os = "macos")]
type DrainHandle = JoinHandle<std::io::Result<BoundedRead>>;

#[cfg(target_os = "macos")]
struct HelperCleanup {
    child: Option<Child>,
    workers: Vec<DrainHandle>,
}

#[cfg(target_os = "macos")]
fn developer_helper_path() -> PathBuf {
    PathBuf::from(env!("FREEMIX_CAMERA_HELPER"))
}

#[cfg(target_os = "macos")]
fn run_helper(
    helper_path: &Path,
    arguments: &[OsString],
    maximum: usize,
    timeout: Duration,
) -> Result<Vec<u8>, CameraError> {
    let mut child = Command::new(helper_path)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| CameraError::Helper(format!("failed to start camera helper: {error}")))?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let detail = "camera helper pipes were not available".to_owned();
        terminate_helper(child, Vec::new());
        return Err(CameraError::Helper(detail));
    };
    let stdout_worker = match std::thread::Builder::new()
        .name("fm-camera-command-stdout".to_owned())
        .spawn(move || read_bounded(stdout, maximum))
    {
        Ok(worker) => worker,
        Err(error) => {
            terminate_helper(child, Vec::new());
            return Err(CameraError::Helper(format!(
                "failed to start camera helper stdout worker: {error}"
            )));
        }
    };
    let stderr_worker = match std::thread::Builder::new()
        .name("fm-camera-command-stderr".to_owned())
        .spawn(move || read_bounded(stderr, 64 * 1024))
    {
        Ok(worker) => worker,
        Err(error) => {
            terminate_helper(child, vec![stdout_worker]);
            return Err(CameraError::Helper(format!(
                "failed to start camera helper stderr worker: {error}"
            )));
        }
    };

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let detail = format!(
                    "camera helper timed out after {} seconds",
                    timeout.as_secs()
                );
                terminate_helper(child, vec![stdout_worker, stderr_worker]);
                return Err(CameraError::Helper(detail));
            }
            Err(error) => {
                let detail = format!("failed waiting for camera helper: {error}");
                terminate_helper(child, vec![stdout_worker, stderr_worker]);
                return Err(CameraError::Helper(detail));
            }
        }
    };

    let drain_deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    while (!stdout_worker.is_finished() || !stderr_worker.is_finished())
        && Instant::now() < drain_deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    if !stdout_worker.is_finished() || !stderr_worker.is_finished() {
        handoff_helper_cleanup(HelperCleanup {
            child: None,
            workers: vec![stdout_worker, stderr_worker],
        });
        return Err(CameraError::Helper(
            "camera helper drain workers missed their shutdown deadline".to_owned(),
        ));
    }
    let stdout = stdout_worker
        .join()
        .map_err(|_| CameraError::Helper("camera helper stdout worker panicked".to_owned()))?
        .map_err(|error| CameraError::Helper(format!("camera helper stdout failed: {error}")))?;
    let stderr = stderr_worker
        .join()
        .map_err(|_| CameraError::Helper("camera helper stderr worker panicked".to_owned()))?
        .map_err(|error| CameraError::Helper(format!("camera helper stderr failed: {error}")))?;
    if stdout.exceeded {
        return Err(CameraError::Helper(format!(
            "camera helper output exceeds {maximum} bytes"
        )));
    }
    if !status.success() {
        return Err(CameraError::Helper(format!(
            "camera helper exited with {status}: {}",
            escaped_diagnostic(&stderr.bytes)
        )));
    }
    Ok(stdout.bytes)
}

#[cfg(target_os = "macos")]
fn escaped_diagnostic(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .flat_map(char::escape_default)
        .collect()
}

#[cfg(target_os = "macos")]
fn terminate_helper(mut child: Child, workers: Vec<DrainHandle>) {
    let mut child_reaped = matches!(child.try_wait(), Ok(Some(_)));
    if !child_reaped {
        let _ = child.kill();
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => {
                    child_reaped = true;
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }
    }
    let child = (!child_reaped).then_some(child);
    if child.is_some() || workers.iter().any(|worker| !worker.is_finished()) {
        handoff_helper_cleanup(HelperCleanup { child, workers });
    } else {
        for worker in workers {
            let _ = worker.join();
        }
    }
}

#[cfg(target_os = "macos")]
fn terminate_detached_capture(mut capture: CaptureProcess) {
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
        handoff_capture_cleanup(capture);
    }
}

#[cfg(target_os = "macos")]
fn handoff_helper_cleanup(cleanup: HelperCleanup) {
    let slot = Arc::new(Mutex::new(Some(cleanup)));
    let worker_slot = Arc::clone(&slot);
    let spawn = std::thread::Builder::new()
        .name("fm-camera-helper-reaper".to_owned())
        .spawn(move || {
            if let Some(cleanup) = worker_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                finish_helper_cleanup(cleanup);
            }
        });
    if spawn.is_err()
        && let Some(cleanup) = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    {
        finish_helper_cleanup(cleanup);
    }
}

#[cfg(target_os = "macos")]
fn finish_helper_cleanup(mut cleanup: HelperCleanup) {
    if let Some(mut child) = cleanup.child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    for worker in cleanup.workers {
        let _ = worker.join();
    }
}

#[cfg(target_os = "macos")]
fn handoff_capture_cleanup(capture: CaptureProcess) {
    let slot = Arc::new(Mutex::new(Some(capture)));
    let worker_slot = Arc::clone(&slot);
    let spawn = std::thread::Builder::new()
        .name("fm-camera-source-reaper".to_owned())
        .spawn(move || {
            if let Some(capture) = worker_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                finish_capture_cleanup(capture);
            }
        });
    if spawn.is_err()
        && let Some(capture) = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    {
        finish_capture_cleanup(capture);
    }
}

#[cfg(target_os = "macos")]
fn finish_capture_cleanup(mut capture: CaptureProcess) {
    capture.stop_token.store(true, Ordering::Release);
    let _ = capture.child.kill();
    let _ = capture.child.wait();
    for worker in capture.workers {
        let _ = worker.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    use fm_io_api::{DiscoveryEventKind, SignalLossPolicy};

    type TestDevice<'a> = (&'a str, &'a str, &'a [(u32, u32, u32, u32)]);

    fn discovery(permission: u8, devices: &[TestDevice<'_>]) -> Vec<u8> {
        let mut bytes = b"FMCAMD2\0".to_vec();
        bytes.push(permission);
        bytes.extend_from_slice(&u32::try_from(devices.len()).unwrap().to_le_bytes());
        for (id, name, formats) in devices {
            for value in [*id, *name] {
                bytes.extend_from_slice(&u32::try_from(value.len()).unwrap().to_le_bytes());
                bytes.extend_from_slice(value.as_bytes());
            }
            bytes.extend_from_slice(&u32::try_from(formats.len()).unwrap().to_le_bytes());
            for (width, height, rate_numerator, rate_denominator) in *formats {
                bytes.extend_from_slice(&width.to_le_bytes());
                bytes.extend_from_slice(&height.to_le_bytes());
                bytes.extend_from_slice(&rate_numerator.to_le_bytes());
                bytes.extend_from_slice(&rate_denominator.to_le_bytes());
            }
        }
        bytes
    }

    fn capture_frame(sequence: u64, pts: i64) -> Vec<u8> {
        capture_frame_with_drops(sequence, 0, pts)
    }

    fn capture_frame_with_drops(sequence: u64, native_dropped: u64, pts: i64) -> Vec<u8> {
        let mut stream = b"FMCAMF3\0".to_vec();
        stream.extend_from_slice(&62_u32.to_le_bytes());
        stream.extend_from_slice(&sequence.to_le_bytes());
        stream.extend_from_slice(&native_dropped.to_le_bytes());
        stream.extend_from_slice(&pts.to_le_bytes());
        stream.extend_from_slice(&1_000_i32.to_le_bytes());
        stream.extend_from_slice(&1_i64.to_le_bytes());
        stream.extend_from_slice(&1_000_i32.to_le_bytes());
        stream.extend_from_slice(&1_u32.to_le_bytes());
        stream.extend_from_slice(&1_u32.to_le_bytes());
        stream.extend_from_slice(&4_u32.to_le_bytes());
        stream.extend_from_slice(&4_u32.to_le_bytes());
        stream.extend_from_slice(&[1, 1]);
        stream.extend_from_slice(&[u8::try_from(sequence).unwrap_or(0), 0, 0, 255]);
        stream
    }

    fn test_frame_at(sequence: u64, pts: i64, clock: ClockDomainId) -> CpuVideoFrame {
        protocol::FrameReader::new(Cursor::new(capture_frame(sequence, pts)), clock)
            .unwrap()
            .read_frame()
            .unwrap()
            .unwrap()
    }

    fn test_frame(sequence: u64) -> CpuVideoFrame {
        test_frame_at(
            sequence,
            i64::try_from(sequence).unwrap(),
            coremedia_clock().domain,
        )
    }

    #[cfg(target_os = "macos")]
    fn shell_octal(bytes: &[u8]) -> String {
        use std::fmt::Write as _;

        let mut output = String::new();
        for byte in bytes {
            write!(output, "\\{byte:03o}").unwrap();
        }
        output
    }

    fn source_with_policy(policy: SignalLossPolicy) -> CameraVideoSource {
        let adapter = MacosCameraAdapter::from_discovery_bytes(&discovery(
            0,
            &[("a", "A", &[(640, 480, 30, 1)])],
        ))
        .unwrap();
        let descriptor = adapter.snapshot().sources[0].clone();
        let mut source = adapter.open_video_source(descriptor.id).unwrap();
        source
            .open(OpenOptions {
                format: descriptor.capabilities.formats[0].clone(),
                clock_domain: descriptor.capabilities.clocks[0].domain,
                memory_domain: MemoryDomain::Cpu,
                queue_capacity: NonZeroUsize::new(2).unwrap(),
                signal_loss: policy,
            })
            .unwrap();
        source
    }

    #[test]
    fn deterministic_ids_are_namespaced_and_nonzero() {
        let first = deterministic_camera_id(CameraIdKind::Device, "abc");
        assert_eq!(first, deterministic_camera_id(CameraIdKind::Device, "abc"));
        assert_ne!(first, deterministic_camera_id(CameraIdKind::Source, "abc"));
        assert_ne!(first.get(), 0);
    }

    #[test]
    fn stable_keys_are_qualified_bounded_and_open_exact_sources() {
        let adapter = MacosCameraAdapter::from_discovery_bytes(&discovery(
            0,
            &[
                ("native-a", "Same name", &[(640, 480, 30, 1)]),
                ("native-b", "Same name", &[(640, 480, 30, 1)]),
            ],
        ))
        .unwrap();
        let snapshot = adapter.snapshot();
        let selected = &snapshot.sources[1];
        assert!(selected.stable_key.starts_with(CAMERA_STABLE_KEY_PREFIX));
        assert!(selected.stable_key.len() <= CAMERA_STABLE_KEY_PREFIX.len() + 39);

        let source = adapter
            .open_video_source_by_stable_key(&selected.stable_key)
            .unwrap();
        assert_eq!(source.descriptor().id, selected.id);
        assert_eq!(source.descriptor().stable_key, selected.stable_key);

        let error = adapter
            .open_video_source_by_stable_key("macos.avfoundation.camera.v1.missing\n")
            .err()
            .unwrap();
        assert!(matches!(error, CameraError::UnknownStableKey(_)));
        assert!(!error.to_string().contains('\n'));
    }

    #[test]
    fn identifier_errors_escape_controls() {
        let error = CameraError::DuplicateDeviceId("camera\n\u{1b}".into()).to_string();
        assert_eq!(error, "duplicate camera device id `camera\\n\\u{1b}`");
        assert!(!error.contains('\n'));
    }

    #[test]
    fn forced_identifier_collisions_are_rejected() {
        let parsed =
            protocol::parse_discovery(&discovery(0, &[("a", "A", &[]), ("b", "B", &[])])).unwrap();
        let result = records_from_discovery(&parsed, |_, _| NonZeroU128::new(1).unwrap());
        assert!(matches!(result, Err(CameraError::IdCollision { .. })));
    }

    #[test]
    fn refresh_events_are_add_update_remove_ordered() {
        let mut adapter = MacosCameraAdapter::from_discovery_bytes(&discovery(
            0,
            &[("a", "A", &[(640, 480, 30, 1)]), ("b", "B", &[])],
        ))
        .unwrap();
        adapter
            .refresh_from_discovery_bytes(&discovery(
                0,
                &[("a", "A2", &[(640, 480, 30, 1)]), ("c", "C", &[])],
            ))
            .unwrap();
        let events: Vec<_> = std::iter::from_fn(|| adapter.next_event()).collect();
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0].kind, DiscoveryEventKind::SourceAdded(_)));
        assert!(matches!(
            events[1].kind,
            DiscoveryEventKind::SourceUpdated(_)
        ));
        assert!(matches!(
            events[2].kind,
            DiscoveryEventKind::SourceRemoved(_)
        ));
        assert!(
            events
                .windows(2)
                .all(|pair| pair[0].generation < pair[1].generation)
        );
    }

    #[test]
    fn open_requires_an_exact_advertised_format_and_valid_state() {
        let adapter = MacosCameraAdapter::from_discovery_bytes(&discovery(
            0,
            &[("a", "A", &[(640, 480, 30, 1)])],
        ))
        .unwrap();
        let descriptor = adapter.snapshot().sources[0].clone();
        let mut source = adapter.open_video_source(descriptor.id).unwrap();
        assert!(matches!(source.start(), Err(IoError::InvalidState { .. })));
        let options = OpenOptions {
            format: descriptor.capabilities.formats[0].clone(),
            clock_domain: descriptor.capabilities.clocks[0].domain,
            memory_domain: MemoryDomain::Cpu,
            queue_capacity: NonZeroUsize::new(2).unwrap(),
            signal_loss: SignalLossPolicy::Stop,
        };
        source.open(options.clone()).unwrap();
        assert!(matches!(
            source.open(options),
            Err(IoError::InvalidState { .. })
        ));
        source.close().unwrap();
    }

    #[test]
    fn exact_video_format_never_selects_a_nearby_mode() {
        let adapter = MacosCameraAdapter::from_discovery_bytes(&discovery(
            0,
            &[(
                "a",
                "A",
                &[
                    (640, 480, 30, 1),
                    (1280, 720, 60, 1),
                    (1280, 720, 60_000, 1_001),
                ],
            )],
        ))
        .unwrap();
        let descriptor = adapter.snapshot().sources[0].clone();
        let source = adapter.open_video_source(descriptor.id).unwrap();
        assert_eq!(
            source.exact_video_format(1280, 720, 60),
            Some(descriptor.capabilities.formats[1].clone())
        );
        assert_eq!(source.exact_video_format(1280, 720, 30), None);
        assert_eq!(source.exact_video_format(1920, 1080, 60), None);
        assert_eq!(
            source.exact_video_format_at_rate(1280, 720, FrameRate::new(60_000, 1_001).unwrap()),
            Some(descriptor.capabilities.formats[2].clone())
        );
        assert_eq!(
            source.exact_video_format_at_rate(1280, 720, FrameRate::new(30_000, 1_001).unwrap()),
            None
        );
    }

    #[test]
    fn queue_drops_oldest_and_reports_telemetry() {
        let mut queue = QueueState {
            capacity: 2,
            ..QueueState::default()
        };
        queue.push(test_frame(0), 2);
        queue.push(test_frame(1), 2);
        queue.push(test_frame(2), 5);
        assert_eq!(
            queue.telemetry,
            CameraTelemetry {
                received: 3,
                dropped: 1,
                native_dropped: 5,
                continuity_rejected: 0,
                recovery_timeout_discarded: 0,
                current: 2,
                peak: 2,
            }
        );
        assert_eq!(queue.pop().unwrap().timing().sequence().get(), 1);
        assert_eq!(queue.pop().unwrap().timing().sequence().get(), 2);
    }

    #[test]
    fn no_frame_loss_holds_last_delivered_frame() {
        let mut source = source_with_policy(SignalLossPolicy::Hold);
        source.start_without_helper_for_test();
        source
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(test_frame(7), 0);
        assert!(matches!(
            source.try_receive().unwrap(),
            Some(MediaTransfer::Live(_))
        ));
        source.expire_activity_for_test();
        for _ in 0..2 {
            assert!(matches!(
                source.try_receive().unwrap(),
                Some(MediaTransfer::Fallback {
                    kind: FallbackKind::Hold,
                    media,
                }) if media.timing().sequence().get() == 7
            ));
            assert_eq!(source.lifecycle(), LifecycleState::Lost);
        }
        assert_eq!(source.health().state, EndpointHealthState::SignalLost);
    }

    #[test]
    fn recovery_rejects_regressions_without_consuming_continuity() {
        let mut source = source_with_policy(SignalLossPolicy::Hold);
        source.start_without_helper_for_test();
        let clock = source.options.as_ref().unwrap().clock_domain;
        source
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(test_frame_at(7, 1_000, clock), 0);
        let anchor = match source.try_receive().unwrap() {
            Some(MediaTransfer::Live(frame)) => frame,
            result => panic!("unexpected anchor transfer: {result:?}"),
        };
        assert_eq!(anchor.timing().sequence().get(), 7);
        source.expire_activity_for_test();
        assert!(matches!(
            source.try_receive().unwrap(),
            Some(MediaTransfer::Fallback {
                kind: FallbackKind::Hold,
                ..
            })
        ));
        source.begin_recovery().unwrap();
        source.pending_recovery = Some(RecoveryContinuity {
            clock,
            previous_pts_nanos: Some(anchor.timing().presentation_timestamp().as_nanos()),
        });
        source.map_recovery_sequences = true;
        source
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_activity = None;

        for (helper_sequence, pts) in [(0, 500), (1, 1_000)] {
            source
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(test_frame_at(helper_sequence, pts, clock), 0);
            assert!(matches!(
                source.try_receive().unwrap(),
                Some(MediaTransfer::Fallback {
                    kind: FallbackKind::Hold,
                    media,
                }) if media == anchor
            ));
            assert_eq!(source.lifecycle(), LifecycleState::Recovering);
            assert_eq!(source.health().state, EndpointHealthState::SignalLost);
            assert!(source.pending_recovery.is_some());
            assert_eq!(source.last_delivered.as_ref().unwrap(), &anchor);
            assert_eq!(
                source.telemetry().continuity_rejected,
                helper_sequence.saturating_add(1)
            );
            assert!(
                source
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .last_activity
                    .is_none()
            );
        }

        source
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(test_frame_at(2, 2_000, clock), 0);
        let recovered = match source.try_receive().unwrap() {
            Some(MediaTransfer::Live(frame)) => frame,
            result => panic!("unexpected recovered transfer: {result:?}"),
        };
        assert_eq!(recovered.timing().sequence().get(), 8);
        assert!(
            recovered
                .timing()
                .flags()
                .contains(MediaFlags::DISCONTINUITY)
        );
        assert!(source.pending_recovery.is_none());
        assert_eq!(source.lifecycle(), LifecycleState::Running);
        assert_eq!(source.health(), &EndpointHealth::HEALTHY);

        source
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(test_frame_at(3, 2_033, clock), 0);
        let second = match source.try_receive().unwrap() {
            Some(MediaTransfer::Live(frame)) => frame,
            result => panic!("unexpected second recovered transfer: {result:?}"),
        };
        assert_eq!(second.timing().sequence().get(), 9);
        assert!(!second.timing().flags().contains(MediaFlags::DISCONTINUITY));
    }

    #[test]
    fn queued_recovery_frame_wins_over_elapsed_deadline() {
        let mut source = source_with_policy(SignalLossPolicy::Hold);
        source.start_without_helper_for_test();
        let clock = source.options.as_ref().unwrap().clock_domain;
        source
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(test_frame_at(7, 1_000, clock), 0);
        let anchor = match source.try_receive().unwrap() {
            Some(MediaTransfer::Live(frame)) => frame,
            result => panic!("unexpected deadline anchor: {result:?}"),
        };
        source.expire_activity_for_test();
        assert!(matches!(
            source.try_receive().unwrap(),
            Some(MediaTransfer::Fallback { .. })
        ));
        source.begin_recovery().unwrap();
        source.pending_recovery = Some(RecoveryContinuity {
            clock,
            previous_pts_nanos: Some(anchor.timing().presentation_timestamp().as_nanos()),
        });
        source.map_recovery_sequences = true;
        source.recovery_deadline = Some(Instant::now() + Duration::from_secs(1));
        source
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(test_frame_at(0, 2_000, clock), 0);
        source.recovery_deadline = Some(
            Instant::now()
                .checked_sub(Duration::from_millis(1))
                .unwrap_or_else(Instant::now),
        );

        let recovered = match source.try_receive().unwrap() {
            Some(MediaTransfer::Live(frame)) => frame,
            result => panic!("queued recovery frame was not accepted: {result:?}"),
        };
        assert_eq!(recovered.timing().sequence().get(), 8);
        assert_eq!(source.lifecycle(), LifecycleState::Running);
        assert_eq!(source.telemetry().recovery_timeout_discarded, 0);
    }

    #[test]
    fn recovered_sequence_offset_preserves_helper_gap() {
        let mut source = source_with_policy(SignalLossPolicy::Hold);
        source.start_without_helper_for_test();
        let clock = source.options.as_ref().unwrap().clock_domain;
        source
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(test_frame_at(7, 1_000, clock), 0);
        let anchor = match source.try_receive().unwrap() {
            Some(MediaTransfer::Live(frame)) => frame,
            result => panic!("unexpected sequence-gap anchor: {result:?}"),
        };
        source.lifecycle = LifecycleState::Recovering;
        source.pending_recovery = Some(RecoveryContinuity {
            clock,
            previous_pts_nanos: Some(anchor.timing().presentation_timestamp().as_nanos()),
        });
        source.map_recovery_sequences = true;
        source.recovery_sequence_offset = None;
        source.recovery_deadline = Some(Instant::now() + Duration::from_secs(1));
        source
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(test_frame_at(0, 2_000, clock), 0);
        let first = match source.try_receive().unwrap() {
            Some(MediaTransfer::Live(frame)) => frame,
            result => panic!("unexpected first mapped frame: {result:?}"),
        };
        assert_eq!(first.timing().sequence().get(), 8);

        source
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(test_frame_at(2, 2_066, clock), 0);
        let after_gap = match source.try_receive().unwrap() {
            Some(MediaTransfer::Live(frame)) => frame,
            result => panic!("unexpected mapped gap frame: {result:?}"),
        };
        assert_eq!(after_gap.timing().sequence().get(), 10);
    }

    #[test]
    fn unanchored_recovery_preserves_native_starting_sequence() {
        let mut source = source_with_policy(SignalLossPolicy::Stop);
        source.start_without_helper_for_test();
        let clock = source.options.as_ref().unwrap().clock_domain;
        source.expire_activity_for_test();
        assert!(matches!(
            source.try_receive(),
            Err(IoError::SignalLost { .. })
        ));
        source.begin_recovery().unwrap();
        source.pending_recovery = Some(RecoveryContinuity {
            clock,
            previous_pts_nanos: None,
        });
        source.map_recovery_sequences = false;
        source.recovery_deadline = Some(Instant::now() + Duration::from_secs(1));
        source
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(test_frame_at(7, 1_000, clock), 0);

        let recovered = match source.try_receive().unwrap() {
            Some(MediaTransfer::Live(frame)) => frame,
            result => panic!("unexpected unanchored recovery result: {result:?}"),
        };
        assert_eq!(recovered.timing().sequence().get(), 7);
        assert!(
            recovered
                .timing()
                .flags()
                .contains(MediaFlags::DISCONTINUITY)
        );
    }

    #[test]
    fn recovery_timeout_accounts_discarded_queue() {
        let mut source = source_with_policy(SignalLossPolicy::Hold);
        source.start_without_helper_for_test();
        let clock = source.options.as_ref().unwrap().clock_domain;
        source
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(test_frame_at(7, 1_000, clock), 0);
        assert!(matches!(
            source.try_receive().unwrap(),
            Some(MediaTransfer::Live(_))
        ));
        source.lifecycle = LifecycleState::Recovering;
        {
            let mut state = source
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.push(test_frame_at(0, 500, clock), 0);
            state.push(test_frame_at(1, 600, clock), 0);
        }

        assert!(matches!(
            source.recovery_timeout_transfer().unwrap(),
            Some(MediaTransfer::Fallback {
                kind: FallbackKind::Hold,
                ..
            })
        ));
        let telemetry = source.telemetry();
        assert_eq!(telemetry.recovery_timeout_discarded, 2);
        assert_eq!(telemetry.current, 0);
        assert_eq!(telemetry.received, 3);
    }

    #[test]
    fn recovery_timeout_cleanup_failure_blocks_late_producer() {
        let mut source = source_with_policy(SignalLossPolicy::Hold);
        source.start_without_helper_for_test();
        let clock = source.options.as_ref().unwrap().clock_domain;
        source
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(test_frame_at(7, 1_000, clock), 0);
        assert!(matches!(
            source.try_receive().unwrap(),
            Some(MediaTransfer::Live(_))
        ));
        source.lifecycle = LifecycleState::Recovering;
        source.pending_recovery = Some(RecoveryContinuity {
            clock,
            previous_pts_nanos: Some(1_000_000_000),
        });
        source.recovery_deadline = Some(Instant::now());
        source
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(test_frame_at(0, 500, clock), 0);
        source.force_shutdown_failure = true;

        assert!(matches!(
            source.recovery_timeout_transfer(),
            Err(IoError::AdapterFailure { .. })
        ));
        assert_eq!(source.lifecycle(), LifecycleState::Lost);
        assert_eq!(source.health().state, EndpointHealthState::Failed);
        assert!(source.pending_recovery.is_none());
        let telemetry = source.telemetry();
        assert_eq!(telemetry.recovery_timeout_discarded, 1);
        assert_eq!(telemetry.current, 0);
        let received = telemetry.received;
        let accepted = source
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_from_worker(test_frame_at(1, 600, clock), 0);
        assert!(!accepted);
        assert_eq!(source.telemetry().received, received);
        assert_eq!(source.telemetry().current, 0);

        source.force_shutdown_failure = false;
        source.stop().unwrap();
        source.close().unwrap();
    }

    #[test]
    fn recovery_sequence_overflow_becomes_stoppable_loss() {
        let mut source = source_with_policy(SignalLossPolicy::Hold);
        source.start_without_helper_for_test();
        let clock = source.options.as_ref().unwrap().clock_domain;
        source
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(test_frame_at(u64::MAX, 1_000, clock), 0);
        let anchor = match source.try_receive().unwrap() {
            Some(MediaTransfer::Live(frame)) => frame,
            result => panic!("unexpected overflow anchor: {result:?}"),
        };
        assert_eq!(anchor.timing().sequence().get(), u64::MAX);
        source.expire_activity_for_test();
        assert!(matches!(
            source.try_receive().unwrap(),
            Some(MediaTransfer::Fallback {
                kind: FallbackKind::Hold,
                ..
            })
        ));
        source.begin_recovery().unwrap();
        source.pending_recovery = Some(RecoveryContinuity {
            clock,
            previous_pts_nanos: Some(anchor.timing().presentation_timestamp().as_nanos()),
        });
        source.map_recovery_sequences = true;
        source
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(test_frame_at(0, 2_000, clock), 0);

        assert!(matches!(
            source.try_receive(),
            Err(IoError::AdapterFailure { .. })
        ));
        assert_eq!(source.lifecycle(), LifecycleState::Lost);
        assert_eq!(source.health().state, EndpointHealthState::Failed);
        assert!(source.pending_recovery.is_none());
        source.stop().unwrap();
        assert_eq!(source.lifecycle(), LifecycleState::Open);
        source.close().unwrap();
    }

    #[test]
    fn recovering_and_lost_sources_can_be_stopped_or_closed() {
        let mut recovering = source_with_policy(SignalLossPolicy::Hold);
        recovering.lifecycle = LifecycleState::Recovering;
        recovering.stop().unwrap();
        assert_eq!(recovering.lifecycle(), LifecycleState::Open);
        recovering.close().unwrap();

        let mut closing_recovery = source_with_policy(SignalLossPolicy::Hold);
        closing_recovery.lifecycle = LifecycleState::Recovering;
        closing_recovery.close().unwrap();
        assert_eq!(closing_recovery.lifecycle(), LifecycleState::Closed);

        let mut lost = source_with_policy(SignalLossPolicy::Hold);
        lost.lifecycle = LifecycleState::Lost;
        lost.stop().unwrap();
        assert_eq!(lost.lifecycle(), LifecycleState::Open);
        lost.close().unwrap();
    }

    #[test]
    fn no_frame_loss_stays_lost_and_returns_stop_policy() {
        let mut source = source_with_policy(SignalLossPolicy::Stop);
        source.start_without_helper_for_test();
        source.expire_activity_for_test();
        for _ in 0..2 {
            assert!(matches!(
                source.try_receive(),
                Err(IoError::SignalLost {
                    policy: SignalLossPolicy::Stop
                })
            ));
            assert_eq!(source.lifecycle(), LifecycleState::Lost);
        }
    }

    #[test]
    fn sticky_failure_promotes_existing_signal_loss_to_failed_health() {
        let mut source = source_with_policy(SignalLossPolicy::Stop);
        source.start_without_helper_for_test();
        source.expire_activity_for_test();
        assert!(matches!(
            source.try_receive(),
            Err(IoError::SignalLost { .. })
        ));
        source
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fail("injected protocol failure");
        assert!(matches!(
            source.try_receive(),
            Err(IoError::AdapterFailure { .. })
        ));
        assert_eq!(source.health().state, EndpointHealthState::Failed);
        assert_eq!(
            source.health().remediation,
            Some(Remediation::RestartAdapter)
        );
    }

    #[test]
    fn clock_capability_is_conservative() {
        let clock = coremedia_clock();
        assert_eq!(clock.timestamps.quality, TimestampQuality::Monotonic);
        assert_eq!(clock.timestamps.resolution_nanos.get(), 1_000_000);
        assert_eq!(clock.timestamps.max_error_nanos, None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn helper_drains_stop_at_the_retained_byte_limit_and_escape_diagnostics() {
        let output = read_bounded(Cursor::new(b"abcd"), 2).unwrap();
        assert_eq!(output.bytes, b"ab");
        assert!(output.exceeded);
        assert_eq!(escaped_diagnostic(b"line\n\x1b[31m"), "line\\n\\u{1b}[31m");
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[allow(clippy::too_many_lines)]
    fn source_waits_for_magic_and_reaps_fake_helper() {
        use std::{
            fs,
            os::unix::fs::PermissionsExt,
            time::{SystemTime, UNIX_EPOCH},
        };

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let helper = std::env::temp_dir().join(format!("fm-camera-helper-{suffix}.sh"));
        fs::write(
            &helper,
            "#!/bin/sh\nprintf '\\106\\115\\103\\101\\115\\106\\063\\000'\nexec sleep 30\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&helper).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&helper, permissions).unwrap();

        let adapter = MacosCameraAdapter::from_discovery_with_helper(
            &discovery(0, &[("a", "A", &[(640, 480, 30, 1)])]),
            helper.clone(),
        )
        .unwrap();
        let descriptor = adapter.snapshot().sources[0].clone();
        let mut source = adapter.open_video_source(descriptor.id).unwrap();
        source
            .open(OpenOptions {
                format: descriptor.capabilities.formats[0].clone(),
                clock_domain: descriptor.capabilities.clocks[0].domain,
                memory_domain: MemoryDomain::Cpu,
                queue_capacity: NonZeroUsize::MIN,
                signal_loss: SignalLossPolicy::Stop,
            })
            .unwrap();
        source.start().unwrap();
        assert_eq!(source.lifecycle(), LifecycleState::Running);
        source.stop().unwrap();
        assert_eq!(source.lifecycle(), LifecycleState::Open);
        source.close().unwrap();

        fs::write(
            &helper,
            "#!/bin/sh\nprintf '\\106\\115\\103\\101\\115\\106\\063\\000'\nexit 20\n",
        )
        .unwrap();
        let adapter = MacosCameraAdapter::from_discovery_with_helper(
            &discovery(0, &[("a", "A", &[(640, 480, 30, 1)])]),
            helper.clone(),
        )
        .unwrap();
        let descriptor = adapter.snapshot().sources[0].clone();
        let mut source = adapter.open_video_source(descriptor.id).unwrap();
        source
            .open(OpenOptions {
                format: descriptor.capabilities.formats[0].clone(),
                clock_domain: descriptor.capabilities.clocks[0].domain,
                memory_domain: MemoryDomain::Cpu,
                queue_capacity: NonZeroUsize::MIN,
                signal_loss: SignalLossPolicy::Stop,
            })
            .unwrap();
        match source.start() {
            Ok(()) => {
                let deadline = Instant::now() + Duration::from_secs(1);
                loop {
                    match source.try_receive() {
                        Err(IoError::SignalLost { .. })
                            if source.lifecycle() == LifecycleState::Lost =>
                        {
                            break;
                        }
                        Err(IoError::SignalLost { .. }) | Ok(None) if Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        result => panic!("unexpected exit-20 result: {result:?}"),
                    }
                }
            }
            Err(IoError::SignalLost { .. }) => {}
            Err(error) => panic!("unexpected exit-20 startup error: {error}"),
        }
        assert_eq!(source.lifecycle(), LifecycleState::Lost);
        assert_eq!(source.health().state, EndpointHealthState::SignalLost);
        assert_eq!(
            source.health().remediation,
            Some(Remediation::ReconnectDevice)
        );
        source.begin_recovery().unwrap();
        match source.finish_recovery() {
            Ok(()) => {
                let deadline = Instant::now() + Duration::from_secs(1);
                loop {
                    match source.try_receive() {
                        Err(IoError::SignalLost { .. })
                            if source.lifecycle() == LifecycleState::Lost =>
                        {
                            break;
                        }
                        Err(IoError::SignalLost { .. }) | Ok(None) if Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        result => panic!("unexpected recovery exit-20 result: {result:?}"),
                    }
                }
            }
            Err(IoError::SignalLost { .. }) => {}
            Err(error) => panic!("unexpected recovery exit-20 error: {error}"),
        }
        assert_eq!(source.lifecycle(), LifecycleState::Lost);
        assert_eq!(source.health().state, EndpointHealthState::SignalLost);
        assert_eq!(
            source.health().remediation,
            Some(Remediation::ReconnectDevice)
        );
        source.close().unwrap();
        fs::remove_file(helper).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[allow(clippy::too_many_lines)]
    fn recovery_restarts_helper_preserves_hold_and_keeps_identity() {
        use std::{
            fs,
            os::unix::fs::PermissionsExt,
            process::Command,
            time::{SystemTime, UNIX_EPOCH},
        };

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let helper = std::env::temp_dir().join(format!("fm-camera-recovery-{suffix}.sh"));
        let count_file = helper.with_extension("count");
        let pid_file = helper.with_extension("pids");
        let invalid_marker = helper.with_extension("invalid");
        let script = format!(
            "#!/bin/sh\ncount=0\nif [ -f '{}' ]; then count=$(cat '{}'); fi\ncount=$((count + 1))\nprintf '%s' \"$count\" > '{}'\nprintf '%s\\n' \"$$\" >> '{}'\ncase \"$count\" in\n  1) printf '{}'; sleep 0.2; exit 20;;\n  2) printf 'BADMAGIC'; exit 91;;\n  3) printf '\\106\\115\\103\\101\\115\\106\\063\\000'; sleep 0.1; printf '{}'; sleep 0.1; printf '{}'; touch '{}'; sleep 0.5; printf '{}'; printf '{}'; exec sleep 30;;\n  4) printf '\\106\\115\\103\\101\\115\\106\\063\\000'; exec sleep 30;;\n  5) printf '\\106\\115\\103\\101\\115\\106\\063\\000'; sleep 0.1; printf '\\001\\000\\000\\000\\000'; exec sleep 30;;\n  *) exit 90;;\nesac\n",
            count_file.display(),
            count_file.display(),
            count_file.display(),
            pid_file.display(),
            shell_octal(&capture_frame_with_drops(7, 3, 1_000)),
            shell_octal(&capture_frame_with_drops(0, 1, 500)[8..]),
            shell_octal(&capture_frame_with_drops(1, 1, 1_000)[8..]),
            invalid_marker.display(),
            shell_octal(&capture_frame_with_drops(2, 2, 2_000)[8..]),
            shell_octal(&capture_frame_with_drops(3, 2, 2_033)[8..]),
        );
        fs::write(&helper, script).unwrap();
        let mut permissions = fs::metadata(&helper).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&helper, permissions).unwrap();

        let adapter = MacosCameraAdapter::from_discovery_with_helper(
            &discovery(0, &[("stable-native-id", "Camera", &[(1, 1, 30, 1)])]),
            helper.clone(),
        )
        .unwrap();
        let descriptor = adapter.snapshot().sources[0].clone();
        let expected_id = descriptor.id;
        let expected_key = descriptor.stable_key.clone();
        let mut source = adapter.open_video_source(descriptor.id).unwrap();
        source
            .open(OpenOptions {
                format: descriptor.capabilities.formats[0].clone(),
                clock_domain: descriptor.capabilities.clocks[0].domain,
                memory_domain: MemoryDomain::Cpu,
                queue_capacity: NonZeroUsize::new(2).unwrap(),
                signal_loss: SignalLossPolicy::Hold,
            })
            .unwrap();
        source.start().unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let first = loop {
            match source.try_receive().unwrap() {
                Some(MediaTransfer::Live(frame)) => break frame,
                Some(MediaTransfer::Fallback { .. }) => panic!("camera entered Hold before live"),
                None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                None => panic!("initial helper frame was not delivered"),
            }
        };
        assert_eq!(first.timing().sequence().get(), 7);
        assert_eq!(source.telemetry().received, 1);
        assert_eq!(source.telemetry().native_dropped, 3);

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match source.try_receive().unwrap() {
                Some(MediaTransfer::Fallback {
                    kind: FallbackKind::Hold,
                    media,
                }) => {
                    assert_eq!(media, first);
                    break;
                }
                None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                result => panic!("unexpected initial loss transfer: {result:?}"),
            }
        }

        source.begin_recovery().unwrap();
        assert!(matches!(
            source.finish_recovery(),
            Err(IoError::AdapterFailure { .. })
        ));
        assert!(matches!(
            source.try_receive().unwrap(),
            Some(MediaTransfer::Fallback {
                kind: FallbackKind::Hold,
                media,
            }) if media == first
        ));

        source.begin_recovery().unwrap();
        source.finish_recovery().unwrap();
        assert_eq!(source.lifecycle(), LifecycleState::Recovering);
        assert_ne!(source.health().state, EndpointHealthState::Healthy);
        assert!(!invalid_marker.exists());
        assert!(matches!(
            source.try_receive().unwrap(),
            Some(MediaTransfer::Fallback {
                kind: FallbackKind::Hold,
                media,
            }) if media == first
        ));

        let deadline = Instant::now() + Duration::from_secs(2);
        while !invalid_marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            invalid_marker.exists(),
            "invalid recovery frames were not emitted"
        );
        for _ in 0..2 {
            assert!(matches!(
                source.try_receive().unwrap(),
                Some(MediaTransfer::Fallback {
                    kind: FallbackKind::Hold,
                    media,
                }) if media == first
            ));
            assert_eq!(source.lifecycle(), LifecycleState::Recovering);
            assert_ne!(source.health().state, EndpointHealthState::Healthy);
        }
        assert!(
            source
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .last_activity
                .is_none()
        );

        let deadline = Instant::now() + Duration::from_secs(2);
        let recovered = loop {
            match source.try_receive().unwrap() {
                Some(MediaTransfer::Live(frame)) => break frame,
                Some(MediaTransfer::Fallback {
                    kind: FallbackKind::Hold,
                    ..
                })
                | None
                    if Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                result => panic!("unexpected recovered transfer: {result:?}"),
            }
        };
        assert_eq!(recovered.timing().sequence().get(), 8);
        assert_eq!(
            recovered.timing().clock_domain(),
            first.timing().clock_domain()
        );
        assert!(
            recovered.timing().presentation_timestamp() > first.timing().presentation_timestamp()
        );
        assert!(
            recovered
                .timing()
                .flags()
                .contains(MediaFlags::DISCONTINUITY)
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        let second = loop {
            match source.try_receive().unwrap() {
                Some(MediaTransfer::Live(frame)) => break frame,
                None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                result => panic!("unexpected second recovered transfer: {result:?}"),
            }
        };
        assert_eq!(second.timing().sequence().get(), 9);
        assert!(!second.timing().flags().contains(MediaFlags::DISCONTINUITY));
        assert_eq!(source.telemetry().received, 5);
        assert_eq!(source.telemetry().native_dropped, 5);
        assert_eq!(source.telemetry().continuity_rejected, 2);
        assert_eq!(source.descriptor().id, expected_id);
        assert_eq!(source.descriptor().stable_key, expected_key);

        source.transition_to_signal_lost("injected post-recovery loss".to_owned());
        source.begin_recovery().unwrap();
        source.finish_recovery().unwrap();
        assert_eq!(source.lifecycle(), LifecycleState::Recovering);
        let installed_deadline = source.recovery_deadline.unwrap();
        assert!(installed_deadline > Instant::now());
        assert!(installed_deadline <= Instant::now() + RECOVERY_FRAME_TIMEOUT);
        source.recovery_deadline = Some(
            Instant::now()
                .checked_sub(Duration::from_millis(1))
                .unwrap_or_else(Instant::now),
        );
        assert!(matches!(
            source.try_receive().unwrap(),
            Some(MediaTransfer::Fallback {
                kind: FallbackKind::Hold,
                media,
            }) if media == second
        ));
        assert_eq!(source.lifecycle(), LifecycleState::Lost);
        assert_eq!(source.health().state, EndpointHealthState::SignalLost);
        assert!(source.capture.is_none());
        assert_eq!(source.telemetry().received, 5);
        assert_eq!(source.telemetry().native_dropped, 5);
        assert_eq!(source.telemetry().recovery_timeout_discarded, 0);
        source.begin_recovery().unwrap();
        source.finish_recovery().unwrap();
        assert_eq!(source.lifecycle(), LifecycleState::Recovering);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match source.try_receive() {
                Err(IoError::AdapterFailure { .. }) => break,
                Ok(Some(MediaTransfer::Fallback {
                    kind: FallbackKind::Hold,
                    media,
                })) if media == second && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                result => panic!("unexpected malformed recovery result: {result:?}"),
            }
        }
        assert_eq!(source.lifecycle(), LifecycleState::Lost);
        assert_eq!(source.health().state, EndpointHealthState::Failed);
        assert!(source.capture.is_none());
        assert!(matches!(
            source.try_receive().unwrap(),
            Some(MediaTransfer::Fallback {
                kind: FallbackKind::Hold,
                media,
            }) if media == second
        ));
        source.stop().unwrap();
        assert_eq!(source.lifecycle(), LifecycleState::Open);
        source.close().unwrap();
        for pid in fs::read_to_string(&pid_file).unwrap().lines() {
            assert!(
                !Command::new("kill")
                    .args(["-0", pid])
                    .status()
                    .unwrap()
                    .success(),
                "camera helper {pid} was not reaped"
            );
        }
        fs::remove_file(helper).unwrap();
        fs::remove_file(count_file).unwrap();
        fs::remove_file(pid_file).unwrap();
        fs::remove_file(invalid_marker).unwrap();
    }
}
