use std::{
    cell::RefCell,
    collections::VecDeque,
    convert::Infallible,
    error::Error,
    fmt,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    num::NonZeroU128,
    path::{Path, PathBuf},
    process::ExitCode,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(feature = "native-media")]
use std::fs::{self, File, OpenOptions};
#[cfg(feature = "native-media")]
use std::num::NonZeroU32;
#[cfg(all(feature = "native-media", target_os = "macos"))]
use std::{collections::BTreeMap, num::NonZeroUsize, sync::Mutex, thread::JoinHandle};

use fm_auth::{Policy, Principal, Role as AuthRole, SessionId, UserId};
use fm_clock::{ClockDomainId, ClockTime};
use fm_command::{
    AcceptedReceipt, CommandId, CommandReceipt, EventSequence, IdempotencyKey, RejectedReceipt,
    Rejection, RejectionCode, Revision, RuntimeGeneration, StateEpoch,
};
use fm_control::{
    CommandSubmission, ControlLimits, ControlService, PrepareSubmitOutcome, ResumeDecision,
};
#[cfg(feature = "native-media")]
use fm_engine::FrameResult;
use fm_engine::{
    Engine, EngineAcceptance, EngineInputAudioStripState, EngineRestoreState, EngineSnapshot,
    ShowState,
};
use fm_model::{
    InputAudioStripState, InputBalanceBasisPoints, InputDelaySamples, InputGainMilliDb, MainMix,
    StingerAudioPolicy as ModelStingerAudioPolicy, StingerConfig, StingerMissingMediaFallback,
    StingerSlotNumber,
};
use fm_persistence::{
    FadeToBlackState as PersistedFadeToBlackState, IdempotencyReceipt,
    ManualTransitionKind as PersistedManualTransitionKind,
    ManualTransitionState as PersistedManualTransitionState, ProjectPosition, ProjectStore,
    ReceiptOutcome, RuntimeFadeToBlack, RuntimeManualTransitions, RuntimeOverlayBorder,
    RuntimeOverlayChannel, RuntimeOverlayPosition, RuntimeOverlayTransition, RuntimeOverlays,
    RuntimeRouting, StoredProject,
};
use fm_protocol::{
    CURRENT_PROTOCOL_VERSION, CapabilityReportSummary, CodecError, CommandMessage, CommandPayload,
    CommandResult, ErrorMessage, EventCursor, EventMessage,
    HandshakeOutcome as ProtocolHandshakeOutcome, HandshakeRequest, HandshakeResponse,
    HeartbeatAcknowledgementMessage, HeartbeatMessage, LineDecoder, ProtocolVersion, ResumeCursor,
    RuntimeEventMessage, ServerHello, ServerIdentity, StructuredError, WireMessage,
    choose_handshake_outcome, encode_line,
};
use fm_server::{
    AuthenticationMode, ControlPlane, DisconnectReason, HandshakeError, Heartbeat, InitialSync,
    Server, ServerConfig, ServerMode, Session, SessionError, SyncPayload,
};
use fm_switcher::{
    MissingMediaFallback, OverlayBorderPreset, OverlayChannelId, OverlayChannelState,
    OverlayPositionPreset, OverlayTransitionKind, StingerAudioPolicy, StingerDescriptor,
    StingerSlotId, SwitcherState, TBarPosition, TBarState, TransitionKind,
};
use fm_types::{InputId, ProjectId};
use freemixd::ReadinessRecord;

#[cfg(feature = "macos-program-surface")]
mod program_surface;
#[cfg(feature = "native-media")]
mod stinger_mutation;

#[cfg(feature = "native-media")]
use fm_codec_ffmpeg::{
    Adapter, Config as FfmpegConfig, StreamSelector, ToolAvailability,
    record::{
        CleanupStatus, EnqueueRejection, OutputFinalization, PairedFrame, RecordConfig,
        RecordFormat, Recorder, RecorderState, StopOutcome,
    },
};
#[cfg(feature = "native-media")]
use fm_codec_image::{StillDecodeLimits, decode_still, sniff_still_format};
#[cfg(all(feature = "native-media", target_os = "macos"))]
use fm_frame::CpuVideoFrame;
#[cfg(feature = "native-media")]
use fm_frame::{
    AudioBlock, ClockDomainId as MediaClockDomainId, MediaTimestamp, MediaTiming,
    NormalizedDuration, NormalizedTimestamp, OriginalTimestamp, SequenceNumber,
};
#[cfg(all(feature = "macos-program-surface", target_os = "macos"))]
use fm_gpu::NativeSurface;
#[cfg(feature = "native-media")]
use fm_gpu::{NativeBackend, NativeContext, NativeTexture};
#[cfg(all(feature = "native-media", target_os = "macos"))]
use fm_io_api::{
    EndpointHealthState, IoError, LifecycleState, MediaSource, MediaTransfer, MemoryDomain,
    OpenOptions as IoOpenOptions, Remediation, SignalLossPolicy,
};
#[cfg(all(feature = "native-media", target_os = "macos"))]
use fm_io_macos::{CameraTelemetry, CameraVideoSource, MacosCameraAdapter};
#[cfg(feature = "native-media")]
use fm_model::{InputKind, SimulatedAudio, SimulatedVideo};
#[cfg(feature = "native-media")]
use fm_observability::{Metric, MetricStore};
#[cfg(feature = "native-media")]
use fm_scheduler::{FrameNumber, FramePacer};
#[cfg(feature = "native-media")]
use fm_sim::{Rgba8, SimulatedVideoSource, SourcePattern};
#[cfg(feature = "native-media")]
use freemixd::native_media::{
    NativeAudioLimits, NativeMasterError, NativeMasterRuntime, NativeMediaRuntime,
    NativeOutputFrame, NativeProgramReadback, NativeProjectLimits, NativeProjectPlan,
    NativeResolvedSource, NativeSourceLimits, NativeSourceRenderError,
};
#[cfg(feature = "native-media")]
use stinger_mutation::{NativeStingerMutation, NativeStingerRetirements};

const DEFAULT_LISTEN: &str = "127.0.0.1:0";
const PROTOCOL_VERSION: ProtocolVersion = CURRENT_PROTOCOL_VERSION;
const CAPABILITIES_DIGEST: &str = "phase1-simulated";
const NATIVE_MEDIA_CAPABILITIES_DIGEST: &str =
    "native-media-bounded-video-audio-master-camera-telemetry-v5";
const FULLSCREEN_PROGRAM_CAPABILITIES_DIGEST: &str =
    "native-media-bounded-video-audio-master-camera-fullscreen-sdr-telemetry-v3";
const PROGRAM_RECORDER_CAPABILITIES_DIGEST: &str =
    "native-media-bounded-video-audio-master-camera-record-program-telemetry-v3";
const FULLSCREEN_PROGRAM_RECORDER_CAPABILITIES_DIGEST: &str =
    "native-media-bounded-video-audio-master-camera-fullscreen-sdr-record-program-telemetry-v3";

type AppResult<T> = Result<T, Box<dyn Error>>;
type SharedControl = Rc<RefCell<ControlService<Policy>>>;

const NATIVE_IO_POLL_INTERVAL: Duration = Duration::from_millis(5);
const CLIENT_READ_POLL_INTERVAL: Duration = NATIVE_IO_POLL_INTERVAL;
#[cfg(all(feature = "native-media", target_os = "macos"))]
const CAMERA_INITIAL_FRAME_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(all(feature = "native-media", target_os = "macos"))]
const CAMERA_STARTUP_CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(all(feature = "native-media", target_os = "macos"))]
const CAMERA_RECOVERY_POLICY: CameraRecoveryPolicy = CameraRecoveryPolicy {
    max_attempts: 3,
    initial_backoff: Duration::from_millis(100),
    max_backoff: Duration::from_millis(400),
    rearm_backoff: Duration::from_secs(10),
    shutdown_timeout: Duration::from_secs(15),
};
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_millis(250);
#[cfg(feature = "native-media")]
const PROGRAM_RECORDER_STOP_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(feature = "native-media")]
const PROGRAM_RECORDER_KILL_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(all(feature = "native-media", feature = "macos-program-surface"))]
const PROGRAM_CHECKPOINT_MARGIN: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct ProcessShutdown {
    requested: Arc<AtomicBool>,
    diagnostic_deadline: Option<Instant>,
}

impl ProcessShutdown {
    fn requested(&self) -> bool {
        self.requested.load(AtomicOrdering::Acquire)
            || self
                .diagnostic_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
    }

    fn set_diagnostic_deadline(&mut self, duration: Duration) -> AppResult<()> {
        self.diagnostic_deadline = Some(
            Instant::now()
                .checked_add(duration)
                .ok_or_else(|| AppFailure("diagnostic shutdown deadline overflow".into()))?,
        );
        Ok(())
    }
}

#[cfg(unix)]
fn register_process_shutdown() -> AppResult<ProcessShutdown> {
    use signal_hook::consts::signal::{SIGINT, SIGTERM};

    let requested = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGINT, Arc::clone(&requested))?;
    signal_hook::flag::register(SIGTERM, Arc::clone(&requested))?;
    Ok(ProcessShutdown {
        requested,
        diagnostic_deadline: None,
    })
}

#[cfg(windows)]
fn register_process_shutdown() -> AppResult<ProcessShutdown> {
    let requested = Arc::new(AtomicBool::new(false));
    let handler_flag = Arc::clone(&requested);
    ctrlc::set_handler(move || handler_flag.store(true, AtomicOrdering::Release))?;
    Ok(ProcessShutdown {
        requested,
        diagnostic_deadline: None,
    })
}

#[cfg(not(any(unix, windows)))]
fn register_process_shutdown() -> AppResult<ProcessShutdown> {
    Ok(ProcessShutdown {
        requested: Arc::new(AtomicBool::new(false)),
        diagnostic_deadline: None,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DaemonShutdownReason {
    Once,
    ProcessSignal,
    ProgramSurface,
}

#[derive(Eq, PartialEq)]
enum OnceClientOutcome {
    Unserved,
    HandshakeResponseWritten,
}

fn requested_daemon_shutdown(
    native: Option<&NativeDaemon>,
    process: Option<&ProcessShutdown>,
) -> Option<DaemonShutdownReason> {
    if process.is_some_and(ProcessShutdown::requested) {
        Some(DaemonShutdownReason::ProcessSignal)
    } else if native.is_some_and(NativeDaemon::shutdown_requested) {
        Some(DaemonShutdownReason::ProgramSurface)
    } else {
        None
    }
}

trait ProjectSaver {
    fn save(&self, project: &StoredProject) -> AppResult<()>;
}

impl ProjectSaver for ProjectStore {
    fn save(&self, project: &StoredProject) -> AppResult<()> {
        ProjectStore::save(self, project).map_err(Into::into)
    }
}

#[cfg(feature = "native-media")]
struct NativeDaemon {
    pacer: FramePacer,
    pacer_start_offset: Duration,
    origin: Instant,
    latest_output: Option<NativeTexture>,
    latest_project_outputs: Vec<NativeOutputFrame>,
    master: NativeMasterRuntime,
    project_plan: NativeProjectPlan,
    playback: freemixd::native_media::NativeSourcePlayback,
    stingers: freemixd::native_media::NativeSourcePlayback,
    projected_frame: Option<FrameResult>,
    runtime: Arc<NativeMediaRuntime>,
    resolved_sources: Arc<Vec<NativeResolvedSource>>,
    assets_root: PathBuf,
    pending_stinger_mutation: Option<NativeStingerMutation>,
    stinger_retirements: NativeStingerRetirements,
    recorder: Option<NativeProgramRecorder>,
    telemetry: NativeRuntimeTelemetry,
    telemetry_emitted: bool,
    #[cfg(target_os = "macos")]
    cameras: NativeCameraInputs,
    #[cfg(target_os = "macos")]
    camera_telemetry_frozen: bool,
    #[cfg(all(feature = "macos-program-surface", target_os = "macos"))]
    program: Option<program_surface::ProgramPresentation>,
}

#[cfg(feature = "native-media")]
struct NativeSourceResolution {
    sources: Vec<NativeResolvedSource>,
    #[cfg(target_os = "macos")]
    cameras: NativeCameraInputs,
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
struct NativeCameraInput {
    input: InputId,
    source: Option<CameraVideoSource>,
    worker: Option<NativeCameraWorker>,
    supervisor: Arc<Mutex<CameraSupervisorState>>,
    recovery_policy: CameraRecoveryPolicy,
    ingested_frames: u64,
    ingest_failed: u64,
    preflight_depth: u64,
    preflight_discarded: u64,
    last_ingested_sequence: Option<u64>,
    last_ingested_discontinuity: bool,
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
#[derive(Default)]
struct CameraFrameSlot {
    frame: Option<CpuVideoFrame>,
    replacements: u64,
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
struct CameraSupervisorState {
    frame: CameraFrameSlot,
    snapshot: CameraWorkerSnapshot,
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
impl CameraFrameSlot {
    fn replace(&mut self, mut frame: CpuVideoFrame) {
        if self.frame.as_ref().is_some_and(|pending| {
            pending
                .timing()
                .flags()
                .contains(fm_frame::MediaFlags::DISCONTINUITY)
        }) && !frame
            .timing()
            .flags()
            .contains(fm_frame::MediaFlags::DISCONTINUITY)
        {
            frame = frame_with_discontinuity(frame);
        }
        if self.frame.replace(frame).is_some() {
            self.replacements = self.replacements.saturating_add(1);
        }
    }

    fn take(&mut self) -> Option<CpuVideoFrame> {
        self.frame.take()
    }
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
fn frame_with_discontinuity(frame: CpuVideoFrame) -> CpuVideoFrame {
    let timing = frame
        .timing()
        .with_flags(frame.timing().flags() | fm_frame::MediaFlags::DISCONTINUITY);
    let metadata = frame.metadata();
    let frame = CpuVideoFrame::new(timing, frame.into_payload());
    if let Some(metadata) = metadata {
        frame
            .with_metadata(metadata)
            .expect("existing camera metadata remains valid")
    } else {
        frame
    }
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
#[derive(Clone, Copy)]
struct CameraRecoveryPolicy {
    max_attempts: u32,
    initial_backoff: Duration,
    max_backoff: Duration,
    rearm_backoff: Duration,
    shutdown_timeout: Duration,
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum CameraRecoveryOutcome {
    #[default]
    Never,
    Pending,
    Recovered,
    Exhausted,
    Rearming,
    WorkerFailed,
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
struct NativeCameraWorker {
    handle: Option<JoinHandle<CameraWorkerResult>>,
    cancel: Arc<AtomicBool>,
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
struct CameraWorkerResult {
    failure: Option<IoError>,
    cleanup_failure: Option<IoError>,
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
#[derive(Clone, Copy)]
struct CameraWorkerSnapshot {
    telemetry: CameraTelemetry,
    lifecycle: LifecycleState,
    health: EndpointHealthState,
    recovery_attempts: u64,
    recovery_successes: u64,
    recovery_exhausted: u64,
    recovery_worker_failures: u64,
    recovery_outcome: CameraRecoveryOutcome,
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
impl CameraWorkerSnapshot {
    fn running(source: &CameraVideoSource) -> Self {
        Self {
            telemetry: source.telemetry(),
            lifecycle: source.lifecycle(),
            health: source.health().state,
            recovery_attempts: 0,
            recovery_successes: 0,
            recovery_exhausted: 0,
            recovery_worker_failures: 0,
            recovery_outcome: CameraRecoveryOutcome::Never,
        }
    }
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
#[derive(Default)]
struct NativeCameraInputs {
    inputs: Vec<NativeCameraInput>,
    telemetry_emitted: bool,
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
impl NativeCameraInputs {
    fn poll_into(
        &mut self,
        runtime: &NativeMediaRuntime,
        playback: &mut freemixd::native_media::NativeSourcePlayback,
    ) -> AppResult<()> {
        self.poll_with(|input, frame| {
            runtime.ingest_live_video_frame_blocking(playback, input, frame)?;
            Ok(())
        })
    }

    fn poll_with(
        &mut self,
        mut ingest: impl FnMut(InputId, CpuVideoFrame) -> AppResult<()>,
    ) -> AppResult<()> {
        for camera in &mut self.inputs {
            Self::collect_finished_worker(camera)?;
            let frame = camera
                .supervisor
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .frame
                .take();
            if let Some(frame) = frame {
                let timing = frame.timing();
                if let Err(error) = ingest(camera.input, frame) {
                    camera.ingest_failed = camera.ingest_failed.saturating_add(1);
                    return Err(error);
                }
                camera.ingested_frames = camera.ingested_frames.saturating_add(1);
                camera.last_ingested_sequence = Some(timing.sequence().get());
                camera.last_ingested_discontinuity =
                    timing.flags().contains(fm_frame::MediaFlags::DISCONTINUITY);
            }
        }
        Ok(())
    }

    fn collect_finished_worker(camera: &mut NativeCameraInput) -> AppResult<()> {
        let finished = camera
            .worker
            .as_ref()
            .and_then(|worker| worker.handle.as_ref())
            .is_some_and(JoinHandle::is_finished);
        if !finished {
            return Ok(());
        }
        let mut worker = camera.worker.take().expect("camera worker was checked");
        let handle = worker.handle.take().expect("camera worker is owned");
        if let Ok(result) = handle.join() {
            if let Some(failure) = result.failure.or(result.cleanup_failure) {
                Err(failure.into())
            } else {
                Err(AppFailure("camera worker exited unexpectedly".to_owned()).into())
            }
        } else {
            let mut snapshot = camera
                .supervisor
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            snapshot.snapshot.recovery_worker_failures =
                snapshot.snapshot.recovery_worker_failures.saturating_add(1);
            snapshot.snapshot.recovery_outcome = CameraRecoveryOutcome::WorkerFailed;
            snapshot.snapshot.lifecycle = LifecycleState::Lost;
            snapshot.snapshot.health = EndpointHealthState::Failed;
            Err(AppFailure("camera worker panicked".to_owned()).into())
        }
    }

    fn start_workers(&mut self) -> AppResult<()> {
        for camera in &mut self.inputs {
            let source = camera
                .source
                .take()
                .expect("started camera source is available");
            let supervisor = Arc::clone(&camera.supervisor);
            let cancel = Arc::new(AtomicBool::new(false));
            let worker_cancel = Arc::clone(&cancel);
            let policy = camera.recovery_policy;
            let handle = thread::Builder::new()
                .name("freemix-camera-supervisor".to_owned())
                .spawn(move || supervise_camera(source, policy, &worker_cancel, &supervisor))?;
            camera.worker = Some(NativeCameraWorker {
                handle: Some(handle),
                cancel,
            });
        }
        Ok(())
    }

    fn cleanup_startup_sources(&mut self, timeout: Duration) -> AppResult<()> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        let mut handles = Vec::new();
        for camera in &mut self.inputs {
            if let Some(mut source) = camera.source.take() {
                handles.push(thread::spawn(move || close_camera_source(&mut source)));
            }
        }
        while handles.iter().any(|handle| !handle.is_finished()) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        let mut failure = None;
        for handle in handles {
            if handle.is_finished() {
                match handle.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => {
                        failure.get_or_insert_with(|| "camera source cleanup failed".to_owned());
                    }
                    Err(_) => {
                        failure.get_or_insert_with(|| "camera cleanup worker panicked".to_owned());
                    }
                }
            } else {
                failure.get_or_insert_with(|| {
                    "camera source cleanup missed the aggregate deadline".to_owned()
                });
            }
        }
        failure.map_or(Ok(()), |detail| Err(AppFailure(detail).into()))
    }

    fn source_telemetry(&self) -> Vec<NativeCameraSourceTelemetry> {
        let mut sources = self
            .inputs
            .iter()
            .map(|camera| {
                let state = camera
                    .supervisor
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let snapshot = state.snapshot;
                NativeCameraSourceTelemetry {
                    input: camera.input,
                    lifecycle: snapshot.lifecycle,
                    health: snapshot.health,
                    frames_received: snapshot.telemetry.received,
                    frames_ingested: camera.ingested_frames,
                    native_dropped: snapshot.telemetry.native_dropped,
                    queue_dropped: snapshot.telemetry.dropped,
                    queue_depth: saturating_u64(snapshot.telemetry.current),
                    queue_peak_depth: saturating_u64(snapshot.telemetry.peak),
                    continuity_rejected: snapshot.telemetry.continuity_rejected,
                    recovery_timeout_discarded: snapshot.telemetry.recovery_timeout_discarded,
                    terminal_error_discarded: snapshot.telemetry.terminal_error_discarded,
                    terminal_trigger_discarded: snapshot.telemetry.terminal_trigger_discarded,
                    ready_delivery_depth: snapshot.telemetry.ready_delivery_depth,
                    ready_delivery_discarded: snapshot.telemetry.ready_delivery_discarded,
                    cancellation_discarded: snapshot.telemetry.cancellation_discarded,
                    supervisor_slot_replaced: state.frame.replacements,
                    supervisor_slot_depth: u64::from(state.frame.frame.is_some()),
                    ingest_failed: camera.ingest_failed,
                    preflight_depth: camera.preflight_depth,
                    preflight_discarded: camera.preflight_discarded,
                    recovery_attempts: snapshot.recovery_attempts,
                    recovery_successes: snapshot.recovery_successes,
                    recovery_exhausted: snapshot.recovery_exhausted,
                    recovery_worker_failures: snapshot.recovery_worker_failures,
                    recovery_outcome: snapshot.recovery_outcome,
                }
            })
            .collect::<Vec<_>>();
        sort_camera_source_telemetry(&mut sources);
        sources
    }

    fn emit_source_telemetry(&mut self) -> Option<Vec<NativeCameraSourceTelemetry>> {
        if self.telemetry_emitted {
            return None;
        }
        let sources = self.source_telemetry();
        for source in &sources {
            eprintln!("{}", source.diagnostic());
        }
        self.telemetry_emitted = true;
        Some(sources)
    }

    fn mark_preflight_frames_ingested(&mut self) {
        for camera in &mut self.inputs {
            camera.ingested_frames = camera
                .ingested_frames
                .saturating_add(camera.preflight_depth);
            camera.preflight_depth = 0;
        }
    }

    fn discard_preflight_frames(&mut self) {
        for camera in &mut self.inputs {
            camera.preflight_discarded = camera
                .preflight_discarded
                .saturating_add(camera.preflight_depth);
            camera.preflight_depth = 0;
        }
    }

    fn shutdown(&mut self) -> AppResult<()> {
        for camera in &mut self.inputs {
            if let Some(worker) = &camera.worker {
                worker.cancel.store(true, AtomicOrdering::Release);
                if let Some(handle) = &worker.handle {
                    handle.thread().unpark();
                }
            }
        }
        let timeout = self
            .inputs
            .iter()
            .map(|camera| camera.recovery_policy.shutdown_timeout)
            .max()
            .unwrap_or(Duration::ZERO);
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        while self.inputs.iter().any(|camera| {
            camera
                .worker
                .as_ref()
                .and_then(|worker| worker.handle.as_ref())
                .is_some_and(|handle| !handle.is_finished())
        }) && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(5));
        }

        let mut failure = None;
        for camera in &mut self.inputs {
            if let Some(mut worker) = camera.worker.take()
                && let Some(handle) = worker.handle.take()
            {
                if handle.is_finished() {
                    match handle.join() {
                        Ok(result) => {
                            if result.failure.is_some() || result.cleanup_failure.is_some() {
                                failure.get_or_insert_with(|| {
                                    "camera worker failed during shutdown".to_owned()
                                });
                            }
                        }
                        Err(_) => {
                            failure.get_or_insert_with(|| {
                                "camera worker failed during shutdown".to_owned()
                            });
                        }
                    }
                } else {
                    failure.get_or_insert_with(|| {
                        "camera workers missed the aggregate shutdown deadline".to_owned()
                    });
                }
            }
        }
        if self
            .cleanup_startup_sources(CAMERA_STARTUP_CLEANUP_TIMEOUT)
            .is_err()
        {
            failure.get_or_insert_with(|| "camera source cleanup failed".to_owned());
        }
        failure.map_or(Ok(()), |detail| Err(AppFailure(detail).into()))
    }
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
fn supervise_camera(
    mut source: CameraVideoSource,
    policy: CameraRecoveryPolicy,
    cancel: &AtomicBool,
    supervisor: &Mutex<CameraSupervisorState>,
) -> CameraWorkerResult {
    loop {
        if cancel.load(AtomicOrdering::Acquire) {
            return finish_camera_worker(&mut source, None, supervisor);
        }
        update_camera_worker_snapshot(supervisor, &source);
        match source.try_receive() {
            Ok(Some(MediaTransfer::Live(next))) => {
                let mut state = supervisor
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                update_camera_worker_snapshot_inner(&mut state.snapshot, &source);
                state.frame.replace(next);
                thread::park_timeout(NATIVE_IO_POLL_INTERVAL);
                continue;
            }
            Ok(None) => {
                thread::park_timeout(NATIVE_IO_POLL_INTERVAL);
                continue;
            }
            Ok(Some(MediaTransfer::Fallback { .. })) => {}
            Err(error) if recoverable_camera_error(&error) => {}
            Err(error) => return finish_camera_worker(&mut source, Some(error), supervisor),
        }

        loop {
            let mut attempts = 0_u32;
            let mut backoff = policy.initial_backoff;
            while attempts < policy.max_attempts {
                if wait_for_camera_worker(cancel, backoff) {
                    return finish_camera_worker(&mut source, None, supervisor);
                }
                attempts = attempts.saturating_add(1);
                {
                    let mut state = supervisor
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.snapshot.recovery_attempts =
                        state.snapshot.recovery_attempts.saturating_add(1);
                    state.snapshot.recovery_outcome = CameraRecoveryOutcome::Pending;
                    state.snapshot.lifecycle = LifecycleState::Recovering;
                    state.snapshot.health = EndpointHealthState::SignalLost;
                }
                let result = source
                    .begin_recovery()
                    .and_then(|()| source.finish_recovery());
                update_camera_worker_snapshot(supervisor, &source);
                match result {
                    Ok(()) => {
                        let mut state = supervisor
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        state.snapshot.recovery_successes =
                            state.snapshot.recovery_successes.saturating_add(1);
                        state.snapshot.recovery_outcome = CameraRecoveryOutcome::Recovered;
                        break;
                    }
                    Err(error) if recoverable_camera_error(&error) => {}
                    Err(error) => {
                        return finish_camera_worker(&mut source, Some(error), supervisor);
                    }
                }
                backoff = backoff.saturating_mul(2).min(policy.max_backoff);
            }
            if source.lifecycle() == LifecycleState::Running {
                break;
            }
            {
                let mut state = supervisor
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.snapshot.recovery_exhausted =
                    state.snapshot.recovery_exhausted.saturating_add(1);
                state.snapshot.recovery_outcome = CameraRecoveryOutcome::Exhausted;
                state.snapshot.lifecycle = LifecycleState::Lost;
                state.snapshot.health = EndpointHealthState::SignalLost;
                state.snapshot.recovery_outcome = CameraRecoveryOutcome::Rearming;
            }
            if wait_for_camera_worker(cancel, policy.rearm_backoff) {
                return finish_camera_worker(&mut source, None, supervisor);
            }
        }
    }
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
fn recoverable_camera_error(error: &IoError) -> bool {
    match error {
        IoError::SignalLost { .. } => true,
        IoError::AdapterFailure {
            remediation: Some(remediation),
            ..
        }
        | IoError::EndpointUnavailable { remediation }
        | IoError::DriverUnavailable { remediation } => matches!(
            remediation,
            Remediation::ReconnectDevice | Remediation::RestartAdapter
        ),
        IoError::InvalidState { .. }
        | IoError::UnsupportedFormat
        | IoError::UnsupportedClock
        | IoError::UnsupportedMemoryDomain
        | IoError::QueueCapacityUnsupported { .. }
        | IoError::PermissionDenied { .. }
        | IoError::MediaTooLarge { .. }
        | IoError::MalformedTimestamp(_)
        | IoError::AdapterFailure {
            remediation: None, ..
        } => false,
    }
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
fn wait_for_camera_worker(cancel: &AtomicBool, duration: Duration) -> bool {
    let deadline = Instant::now()
        .checked_add(duration)
        .unwrap_or_else(Instant::now);
    while !cancel.load(AtomicOrdering::Acquire) && Instant::now() < deadline {
        thread::park_timeout(deadline.saturating_duration_since(Instant::now()));
    }
    cancel.load(AtomicOrdering::Acquire)
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
fn update_camera_worker_snapshot(
    supervisor: &Mutex<CameraSupervisorState>,
    source: &CameraVideoSource,
) {
    let mut state = supervisor
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    update_camera_worker_snapshot_inner(&mut state.snapshot, source);
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
fn update_camera_worker_snapshot_inner(
    snapshot: &mut CameraWorkerSnapshot,
    source: &CameraVideoSource,
) {
    snapshot.telemetry = source.telemetry();
    snapshot.lifecycle = source.lifecycle();
    snapshot.health = source.health().state;
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
fn finish_camera_worker(
    source: &mut CameraVideoSource,
    failure: Option<IoError>,
    supervisor: &Mutex<CameraSupervisorState>,
) -> CameraWorkerResult {
    let cleanup_failure = close_camera_source(source).err();
    update_camera_worker_snapshot(supervisor, source);
    if failure.is_some() || cleanup_failure.is_some() {
        let mut state = supervisor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.snapshot.recovery_worker_failures =
            state.snapshot.recovery_worker_failures.saturating_add(1);
        state.snapshot.recovery_outcome = CameraRecoveryOutcome::WorkerFailed;
        state.snapshot.health = EndpointHealthState::Failed;
    }
    CameraWorkerResult {
        failure,
        cleanup_failure,
    }
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
fn close_camera_source(source: &mut CameraVideoSource) -> Result<(), IoError> {
    if matches!(
        source.lifecycle(),
        LifecycleState::Running | LifecycleState::Recovering
    ) {
        source.stop()?;
    }
    if matches!(
        source.lifecycle(),
        LifecycleState::Open | LifecycleState::Lost
    ) {
        source.close()?;
    }
    Ok(())
}

#[cfg(feature = "native-media")]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct NativeCameraTelemetry {
    configured_sources: u64,
    frames_received: u64,
    frames_ingested: u64,
    native_dropped: u64,
    queue_dropped: u64,
    queue_depth: u64,
    queue_peak_depth: u64,
    continuity_rejected: u64,
    recovery_timeout_discarded: u64,
    terminal_error_discarded: u64,
    terminal_trigger_discarded: u64,
    ready_delivery_depth: u64,
    ready_delivery_discarded: u64,
    cancellation_discarded: u64,
    supervisor_slot_replaced: u64,
    supervisor_slot_depth: u64,
    ingest_failed: u64,
    preflight_depth: u64,
    preflight_discarded: u64,
    recovery_attempts: u64,
    recovery_successes: u64,
    recovery_exhausted: u64,
    recovery_worker_failures: u64,
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeCameraSourceTelemetry {
    input: InputId,
    lifecycle: LifecycleState,
    health: EndpointHealthState,
    frames_received: u64,
    frames_ingested: u64,
    native_dropped: u64,
    queue_dropped: u64,
    queue_depth: u64,
    queue_peak_depth: u64,
    continuity_rejected: u64,
    recovery_timeout_discarded: u64,
    terminal_error_discarded: u64,
    terminal_trigger_discarded: u64,
    ready_delivery_depth: u64,
    ready_delivery_discarded: u64,
    cancellation_discarded: u64,
    supervisor_slot_replaced: u64,
    supervisor_slot_depth: u64,
    ingest_failed: u64,
    preflight_depth: u64,
    preflight_discarded: u64,
    recovery_attempts: u64,
    recovery_successes: u64,
    recovery_exhausted: u64,
    recovery_worker_failures: u64,
    recovery_outcome: CameraRecoveryOutcome,
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
impl NativeCameraSourceTelemetry {
    fn diagnostic(self) -> String {
        format!(
            "FREEMIXD_CAMERA_SOURCE\tv=1\tclassification=diagnostic-not-certification\tinput_id={}\tsample_phase=pre_cleanup\tsample_lifecycle={}\thealth={}\tframes_received={}\tframes_ingested={}\tnative_dropped={}\tqueue_depth={}\tqueue_peak_depth={}\tqueue_dropped={}\tcontinuity_rejected={}\trecovery_timeout_discarded={}\tterminal_error_discarded={}\tterminal_trigger_discarded={}\tready_delivery_depth={}\tready_delivery_discarded={}\tcancellation_discarded={}\tsupervisor_slot_replaced={}\tsupervisor_slot_depth={}\tingest_failed={}\tpreflight_depth={}\tpreflight_discarded={}\trecovery_attempts={}\trecovery_successes={}\trecovery_exhausted={}\trecovery_worker_failures={}\trecovery_outcome={}",
            self.input,
            lifecycle_label(self.lifecycle),
            health_label(self.health),
            self.frames_received,
            self.frames_ingested,
            self.native_dropped,
            self.queue_depth,
            self.queue_peak_depth,
            self.queue_dropped,
            self.continuity_rejected,
            self.recovery_timeout_discarded,
            self.terminal_error_discarded,
            self.terminal_trigger_discarded,
            self.ready_delivery_depth,
            self.ready_delivery_discarded,
            self.cancellation_discarded,
            self.supervisor_slot_replaced,
            self.supervisor_slot_depth,
            self.ingest_failed,
            self.preflight_depth,
            self.preflight_discarded,
            self.recovery_attempts,
            self.recovery_successes,
            self.recovery_exhausted,
            self.recovery_worker_failures,
            recovery_outcome_label(self.recovery_outcome),
        )
    }

    #[cfg(test)]
    fn accounted_frames(self) -> u64 {
        self.frames_ingested
            .saturating_add(self.queue_dropped)
            .saturating_add(self.continuity_rejected)
            .saturating_add(self.recovery_timeout_discarded)
            .saturating_add(self.terminal_error_discarded)
            .saturating_add(self.terminal_trigger_discarded)
            .saturating_add(self.ready_delivery_depth)
            .saturating_add(self.ready_delivery_discarded)
            .saturating_add(self.cancellation_discarded)
            .saturating_add(self.supervisor_slot_replaced)
            .saturating_add(self.queue_depth)
            .saturating_add(self.supervisor_slot_depth)
            .saturating_add(self.ingest_failed)
            .saturating_add(self.preflight_depth)
            .saturating_add(self.preflight_discarded)
    }
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
fn aggregate_camera_telemetry(sources: &[NativeCameraSourceTelemetry]) -> NativeCameraTelemetry {
    let mut aggregate = NativeCameraTelemetry {
        configured_sources: saturating_u64(sources.len()),
        ..NativeCameraTelemetry::default()
    };
    for source in sources {
        aggregate.frames_received = aggregate
            .frames_received
            .saturating_add(source.frames_received);
        aggregate.frames_ingested = aggregate
            .frames_ingested
            .saturating_add(source.frames_ingested);
        aggregate.native_dropped = aggregate
            .native_dropped
            .saturating_add(source.native_dropped);
        aggregate.queue_dropped = aggregate.queue_dropped.saturating_add(source.queue_dropped);
        aggregate.queue_depth = aggregate.queue_depth.saturating_add(source.queue_depth);
        aggregate.queue_peak_depth = aggregate
            .queue_peak_depth
            .saturating_add(source.queue_peak_depth);
        aggregate.continuity_rejected = aggregate
            .continuity_rejected
            .saturating_add(source.continuity_rejected);
        aggregate.recovery_timeout_discarded = aggregate
            .recovery_timeout_discarded
            .saturating_add(source.recovery_timeout_discarded);
        aggregate.terminal_error_discarded = aggregate
            .terminal_error_discarded
            .saturating_add(source.terminal_error_discarded);
        aggregate.terminal_trigger_discarded = aggregate
            .terminal_trigger_discarded
            .saturating_add(source.terminal_trigger_discarded);
        aggregate.ready_delivery_depth = aggregate
            .ready_delivery_depth
            .saturating_add(source.ready_delivery_depth);
        aggregate.ready_delivery_discarded = aggregate
            .ready_delivery_discarded
            .saturating_add(source.ready_delivery_discarded);
        aggregate.cancellation_discarded = aggregate
            .cancellation_discarded
            .saturating_add(source.cancellation_discarded);
        aggregate.supervisor_slot_replaced = aggregate
            .supervisor_slot_replaced
            .saturating_add(source.supervisor_slot_replaced);
        aggregate.supervisor_slot_depth = aggregate
            .supervisor_slot_depth
            .saturating_add(source.supervisor_slot_depth);
        aggregate.ingest_failed = aggregate.ingest_failed.saturating_add(source.ingest_failed);
        aggregate.preflight_depth = aggregate
            .preflight_depth
            .saturating_add(source.preflight_depth);
        aggregate.preflight_discarded = aggregate
            .preflight_discarded
            .saturating_add(source.preflight_discarded);
        aggregate.recovery_attempts = aggregate
            .recovery_attempts
            .saturating_add(source.recovery_attempts);
        aggregate.recovery_successes = aggregate
            .recovery_successes
            .saturating_add(source.recovery_successes);
        aggregate.recovery_exhausted = aggregate
            .recovery_exhausted
            .saturating_add(source.recovery_exhausted);
        aggregate.recovery_worker_failures = aggregate
            .recovery_worker_failures
            .saturating_add(source.recovery_worker_failures);
    }
    aggregate
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
const fn recovery_outcome_label(outcome: CameraRecoveryOutcome) -> &'static str {
    match outcome {
        CameraRecoveryOutcome::Never => "never",
        CameraRecoveryOutcome::Pending => "pending",
        CameraRecoveryOutcome::Recovered => "recovered",
        CameraRecoveryOutcome::Exhausted => "exhausted",
        CameraRecoveryOutcome::Rearming => "rearming",
        CameraRecoveryOutcome::WorkerFailed => "worker_failed",
    }
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
fn sort_camera_source_telemetry(sources: &mut [NativeCameraSourceTelemetry]) {
    sources.sort_by_key(|source| source.input);
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
const fn lifecycle_label(state: LifecycleState) -> &'static str {
    match state {
        LifecycleState::Closed => "closed",
        LifecycleState::Open => "open",
        LifecycleState::Running => "running",
        LifecycleState::Lost => "lost",
        LifecycleState::Recovering => "recovering",
    }
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
const fn health_label(state: EndpointHealthState) -> &'static str {
    match state {
        EndpointHealthState::Healthy => "healthy",
        EndpointHealthState::Degraded => "degraded",
        EndpointHealthState::SignalLost => "signal_lost",
        EndpointHealthState::Failed => "failed",
    }
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
impl Drop for NativeCameraInputs {
    fn drop(&mut self) {
        self.discard_preflight_frames();
        let _ = self.emit_source_telemetry();
        let _ = self.shutdown();
    }
}

#[cfg(feature = "native-media")]
struct NativeRuntimeTelemetry {
    origin: Instant,
    gpu_adapter: String,
    gpu_backend: NativeBackend,
    metrics: MetricStore,
    metric_errors: u64,
    gpu_support: fm_gpu::NativeFullscreenTimingSupport,
    gpu_pending_samples: u64,
    gpu_dropped_samples: u64,
    gpu_unavailable_samples: u64,
    audio_retained_blocks: u64,
    audio_peak_retained_blocks: u64,
    audio_retained_samples: u64,
    audio_peak_retained_samples: u64,
    audio_retained_bytes: u64,
    audio_peak_retained_bytes: u64,
    audio_reservation_requests: u64,
    audio_reserved_blocks: u64,
    audio_peak_reserved_blocks: u64,
    audio_reserved_samples: u64,
    audio_peak_reserved_samples: u64,
    audio_reserved_bytes: u64,
    audio_peak_reserved_bytes: u64,
    audio_source_stalls: u64,
    audio_positioned_blocks: u64,
    audio_positioned_samples: u64,
    audio_leading_silence_samples: u64,
    audio_eos_padding_blocks: u64,
    audio_eos_padding_samples: u64,
    audio_sink_depth: u64,
    audio_sink_peak_depth: u64,
    audio_sink_dropped: u64,
    camera: NativeCameraTelemetry,
    recorder_configured: bool,
    recorder_outstanding_pairs: u64,
    recorder_peak_outstanding_pairs: u64,
    recorder_retained_bytes: u64,
    recorder_peak_retained_bytes: u64,
}

#[cfg(feature = "native-media")]
impl NativeRuntimeTelemetry {
    const RETAINED_SAMPLES: usize = 256;

    fn new(origin: Instant, adapter: &fm_gpu::NativeAdapterInfo) -> Self {
        Self {
            origin,
            gpu_adapter: adapter.name.clone(),
            gpu_backend: adapter.backend,
            metrics: MetricStore::new(Self::RETAINED_SAMPLES),
            metric_errors: 0,
            gpu_support: fm_gpu::NativeFullscreenTimingSupport::Unsupported,
            gpu_pending_samples: 0,
            gpu_dropped_samples: 0,
            gpu_unavailable_samples: 0,
            audio_retained_blocks: 0,
            audio_peak_retained_blocks: 0,
            audio_retained_samples: 0,
            audio_peak_retained_samples: 0,
            audio_retained_bytes: 0,
            audio_peak_retained_bytes: 0,
            audio_reservation_requests: 0,
            audio_reserved_blocks: 0,
            audio_peak_reserved_blocks: 0,
            audio_reserved_samples: 0,
            audio_peak_reserved_samples: 0,
            audio_reserved_bytes: 0,
            audio_peak_reserved_bytes: 0,
            audio_source_stalls: 0,
            audio_positioned_blocks: 0,
            audio_positioned_samples: 0,
            audio_leading_silence_samples: 0,
            audio_eos_padding_blocks: 0,
            audio_eos_padding_samples: 0,
            audio_sink_depth: 0,
            audio_sink_peak_depth: 0,
            audio_sink_dropped: 0,
            camera: NativeCameraTelemetry::default(),
            recorder_configured: false,
            recorder_outstanding_pairs: 0,
            recorder_peak_outstanding_pairs: 0,
            recorder_retained_bytes: 0,
            recorder_peak_retained_bytes: 0,
        }
    }

    fn observe_host_lateness(&mut self, deadline: Instant, observed: Instant) {
        let milliseconds = observed.saturating_duration_since(deadline).as_secs_f64() * 1_000.0;
        let time = self.monotonic_millis();
        let result =
            self.metrics
                .observe_histogram(Metric::LatencyMilliseconds, time, milliseconds);
        self.record_metric_result(result);
    }

    fn observe_gpu(&mut self, context: &NativeContext) {
        let telemetry = context.take_fullscreen_timing_telemetry();
        self.gpu_support = telemetry.support;
        self.gpu_pending_samples = saturating_u64(telemetry.pending_samples);
        self.gpu_dropped_samples = telemetry.dropped_samples;
        self.gpu_unavailable_samples = telemetry.unavailable_samples;
        for sample in telemetry.completed_samples {
            let milliseconds = sample.duration_nanoseconds() / 1_000_000.0;
            let time = self.monotonic_millis();
            let result =
                self.metrics
                    .observe_histogram(Metric::GpuTimeMilliseconds, time, milliseconds);
            self.record_metric_result(result);
        }
    }

    fn observe_audio(&mut self, master: &NativeMasterRuntime) {
        let audio = master.audio_telemetry();
        self.audio_retained_blocks = saturating_u64(master.retained_blocks());
        self.audio_peak_retained_blocks = self
            .audio_peak_retained_blocks
            .max(self.audio_retained_blocks)
            .max(saturating_u64(audio.peak_retained_blocks));
        self.audio_retained_samples = saturating_u64(master.retained_samples());
        self.audio_peak_retained_samples = self
            .audio_peak_retained_samples
            .max(self.audio_retained_samples)
            .max(saturating_u64(audio.peak_retained_samples));
        self.audio_retained_bytes = saturating_u64(master.retained_bytes());
        self.audio_peak_retained_bytes = self
            .audio_peak_retained_bytes
            .max(self.audio_retained_bytes)
            .max(saturating_u64(audio.peak_retained_bytes));
        self.audio_reservation_requests = audio.reservation_requests;
        self.audio_reserved_blocks = saturating_u64(audio.reserved_blocks);
        self.audio_peak_reserved_blocks = saturating_u64(audio.peak_reserved_blocks);
        self.audio_reserved_samples = saturating_u64(audio.reserved_samples);
        self.audio_peak_reserved_samples = saturating_u64(audio.peak_reserved_samples);
        self.audio_reserved_bytes = saturating_u64(audio.reserved_bytes);
        self.audio_peak_reserved_bytes = saturating_u64(audio.peak_reserved_bytes);
        self.audio_source_stalls = audio.source_stalls;
        self.audio_positioned_blocks = audio.positioned_blocks;
        self.audio_positioned_samples = audio.positioned_samples;
        self.audio_leading_silence_samples = audio.leading_silence_samples;
        self.audio_eos_padding_blocks = audio.eos_padding_blocks;
        self.audio_eos_padding_samples = audio.eos_padding_samples;
        self.audio_sink_depth = saturating_u64(master.sink_len());
        let sink = master.sink_telemetry();
        self.audio_sink_peak_depth = saturating_u64(sink.high_watermark());
        self.audio_sink_dropped = sink.dropped();
    }

    fn observe_recorder(&mut self, recorder: &NativeProgramRecorder) {
        self.recorder_configured = true;
        let telemetry = recorder.recorder.telemetry();
        self.recorder_outstanding_pairs = saturating_u64(telemetry.outstanding_pairs);
        self.recorder_peak_outstanding_pairs = self
            .recorder_peak_outstanding_pairs
            .max(self.recorder_outstanding_pairs);
        self.recorder_retained_bytes = saturating_u64(telemetry.retained_bytes);
        self.recorder_peak_retained_bytes = self
            .recorder_peak_retained_bytes
            .max(self.recorder_retained_bytes);
    }

    fn emit(&self, presentation: Option<fm_gpu::PresentationTelemetry>) {
        eprintln!("{}", self.diagnostic(presentation));
    }

    fn diagnostic(&self, presentation: Option<fm_gpu::PresentationTelemetry>) -> String {
        let host = self
            .metrics
            .series(Metric::LatencyMilliseconds)
            .histogram_summary()
            .expect("host lateness is a histogram");
        let gpu = self
            .metrics
            .series(Metric::GpuTimeMilliseconds)
            .histogram_summary()
            .expect("GPU time is a histogram");
        let presentation_active = presentation.is_some();
        let presentation = presentation.unwrap_or_default();
        format!(
            "FREEMIXD_TELEMETRY\tv=4\thost_lateness_samples_total={}\thost_lateness_samples_retained={}\thost_lateness_metric_samples_dropped={}\thost_lateness_p50_ms={}\thost_lateness_p95_ms={}\thost_lateness_p99_ms={}\taudio_retained_blocks={}\taudio_observed_peak_retained_blocks={}\taudio_retained_samples={}\taudio_observed_peak_retained_samples={}\taudio_retained_bytes={}\taudio_observed_peak_retained_bytes={}\taudio_reservation_requests={}\taudio_reserved_blocks={}\taudio_observed_peak_reserved_blocks={}\taudio_reserved_samples={}\taudio_observed_peak_reserved_samples={}\taudio_reserved_bytes={}\taudio_observed_peak_reserved_bytes={}\taudio_source_stalls={}\taudio_positioned_blocks={}\taudio_positioned_samples={}\taudio_leading_silence_samples={}\taudio_eos_padding_blocks={}\taudio_eos_padding_samples={}\taudio_sink_depth={}\taudio_sink_peak_depth={}\taudio_sink_dropped={}\tcamera_configured_sources={}\tcamera_frames_received={}\tcamera_frames_ingested={}\tcamera_native_dropped={}\tcamera_queue_depth={}\tcamera_queue_peak_depth={}\tcamera_queue_dropped={}\tcamera_continuity_rejected={}\tcamera_recovery_timeout_discarded={}\tcamera_terminal_error_discarded={}\tcamera_terminal_trigger_discarded={}\tcamera_ready_delivery_depth={}\tcamera_ready_delivery_discarded={}\tcamera_cancellation_discarded={}\tcamera_supervisor_slot_replaced={}\tcamera_supervisor_slot_depth={}\tcamera_ingest_failed={}\tcamera_preflight_depth={}\tcamera_preflight_discarded={}\tcamera_recovery_attempts={}\tcamera_recovery_successes={}\tcamera_recovery_exhausted={}\tcamera_recovery_worker_failures={}\tpresentation_active={}\tpresentation_pending_depth={}\tpresentation_peak_pending_depth={}\tpresentation_dropped={}\trecorder_configured={}\trecorder_outstanding_pairs={}\trecorder_observed_peak_outstanding_pairs={}\trecorder_retained_bytes={}\trecorder_observed_peak_retained_bytes={}\tgpu_backend={:?}\tgpu_adapter={}\tgpu_timing={:?}\tgpu_pass_samples_total={}\tgpu_pass_samples_retained={}\tgpu_pass_metric_samples_dropped={}\tgpu_pass_p50_ms={}\tgpu_pass_p95_ms={}\tgpu_pass_p99_ms={}\tgpu_samples_pending={}\tgpu_samples_dropped={}\tgpu_samples_unavailable={}\tmetric_errors={}\tmetric_samples_dropped={}",
            host.count,
            host.retained_samples,
            host.dropped_samples,
            metric_value(host.p50),
            metric_value(host.p95),
            metric_value(host.p99),
            self.audio_retained_blocks,
            self.audio_peak_retained_blocks,
            self.audio_retained_samples,
            self.audio_peak_retained_samples,
            self.audio_retained_bytes,
            self.audio_peak_retained_bytes,
            self.audio_reservation_requests,
            self.audio_reserved_blocks,
            self.audio_peak_reserved_blocks,
            self.audio_reserved_samples,
            self.audio_peak_reserved_samples,
            self.audio_reserved_bytes,
            self.audio_peak_reserved_bytes,
            self.audio_source_stalls,
            self.audio_positioned_blocks,
            self.audio_positioned_samples,
            self.audio_leading_silence_samples,
            self.audio_eos_padding_blocks,
            self.audio_eos_padding_samples,
            self.audio_sink_depth,
            self.audio_sink_peak_depth,
            self.audio_sink_dropped,
            self.camera.configured_sources,
            self.camera.frames_received,
            self.camera.frames_ingested,
            self.camera.native_dropped,
            self.camera.queue_depth,
            self.camera.queue_peak_depth,
            self.camera.queue_dropped,
            self.camera.continuity_rejected,
            self.camera.recovery_timeout_discarded,
            self.camera.terminal_error_discarded,
            self.camera.terminal_trigger_discarded,
            self.camera.ready_delivery_depth,
            self.camera.ready_delivery_discarded,
            self.camera.cancellation_discarded,
            self.camera.supervisor_slot_replaced,
            self.camera.supervisor_slot_depth,
            self.camera.ingest_failed,
            self.camera.preflight_depth,
            self.camera.preflight_discarded,
            self.camera.recovery_attempts,
            self.camera.recovery_successes,
            self.camera.recovery_exhausted,
            self.camera.recovery_worker_failures,
            presentation_active,
            presentation.pending_depth,
            presentation.peak_pending_depth,
            presentation.frames_dropped,
            self.recorder_configured,
            self.recorder_outstanding_pairs,
            self.recorder_peak_outstanding_pairs,
            self.recorder_retained_bytes,
            self.recorder_peak_retained_bytes,
            self.gpu_backend,
            diagnostic_field(&self.gpu_adapter),
            self.gpu_support,
            gpu.count,
            gpu.retained_samples,
            gpu.dropped_samples,
            metric_value(gpu.p50),
            metric_value(gpu.p95),
            metric_value(gpu.p99),
            self.gpu_pending_samples,
            self.gpu_dropped_samples,
            self.gpu_unavailable_samples,
            self.metric_errors,
            self.metrics.dropped_samples(),
        )
    }

    fn monotonic_millis(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn record_metric_result(&mut self, result: Result<(), fm_observability::MetricError>) {
        if result.is_err() {
            self.metric_errors = self.metric_errors.saturating_add(1);
        }
    }
}

#[cfg(feature = "native-media")]
fn saturating_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(feature = "native-media")]
fn metric_value(value: Option<f64>) -> String {
    value.map_or_else(|| "none".to_owned(), |value| format!("{value:.3}"))
}

#[cfg(feature = "native-media")]
struct NativeProgramRecorder {
    readback: NativeProgramReadback,
    format: RecordFormat,
    recorder: Recorder,
    capture: RecorderCapturePolicy,
    finalization_clean: Option<bool>,
    startup_pair_timeout: Duration,
}

#[cfg(feature = "native-media")]
#[derive(Default)]
struct RecorderCapturePolicy {
    first_failure: Option<String>,
    app_capture_failure: bool,
}

#[cfg(feature = "native-media")]
impl RecorderCapturePolicy {
    fn active(&self) -> bool {
        self.first_failure.is_none()
    }

    fn fail(&mut self, failure: String, app_capture_failure: bool) -> Option<(&str, bool)> {
        if self.first_failure.is_some() {
            return None;
        }
        self.first_failure = Some(failure);
        self.app_capture_failure = app_capture_failure;
        Some((
            self.first_failure.as_deref().expect("failure was stored"),
            app_capture_failure,
        ))
    }
}

#[cfg(feature = "native-media")]
impl NativeProgramRecorder {
    fn start(runtime: &NativeMediaRuntime, stored: &StoredProject, path: &Path) -> AppResult<Self> {
        let settings = stored.project().settings();
        let dimensions = settings.video.dimensions;
        let format = RecordFormat::new(
            dimensions.width(),
            dimensions.height(),
            settings.frame_rate,
            settings.audio.sample_rate,
            settings.audio.channels.clone(),
            SequenceNumber::new(stored.position().frames_rendered),
        )?;
        let readback = runtime.create_program_readback_blocking(
            NonZeroU32::new(dimensions.width()).expect("project width is nonzero"),
            NonZeroU32::new(dimensions.height()).expect("project height is nonzero"),
        )?;
        let output = create_record_output(path)?;
        let mut config = RecordConfig::new(format.clone());
        config.limits.stop_timeout = PROGRAM_RECORDER_STOP_TIMEOUT;
        config.limits.kill_timeout = PROGRAM_RECORDER_KILL_TIMEOUT;
        let startup_pair_timeout = config
            .limits
            .connect_timeout
            .checked_add(config.limits.no_progress_timeout)
            .ok_or_else(|| AppFailure("Program recorder startup timeout overflow".into()))?;
        let recorder = Recorder::start(output, config)?;
        Ok(Self {
            readback,
            format,
            recorder,
            capture: RecorderCapturePolicy::default(),
            finalization_clean: None,
            startup_pair_timeout,
        })
    }

    fn capture(
        &mut self,
        runtime: &NativeMediaRuntime,
        program: &NativeTexture,
        audio: AudioBlock,
    ) {
        if !self.capture.active() {
            return;
        }
        let telemetry = self.recorder.telemetry();
        if telemetry.state == RecorderState::Failed {
            self.fail(format!("backend:{:?}", telemetry.failure), false);
            return;
        }
        let sequence = audio.timing().sequence();
        // This is the existing synchronous diagnostic readback path, not a
        // nonblocking or zero-copy encoder bridge.
        let readback = match runtime.readback_program_blocking(&mut self.readback, program) {
            Ok(readback) => readback,
            Err(error) => {
                self.fail(format!("readback:{error}"), true);
                return;
            }
        };
        let Some(expected_stride) = readback.width.checked_mul(4) else {
            self.fail("readback:stride_overflow".to_owned(), true);
            return;
        };
        if readback.width != self.readback.width()
            || readback.height != self.readback.height()
            || readback.stride != expected_stride
        {
            self.fail("readback:invalid_tight_layout".to_owned(), true);
            return;
        }
        let pair = match PairedFrame::new(&self.format, sequence, readback.rgba, audio) {
            Ok(pair) => pair,
            Err(error) => {
                self.fail(format!("frame:{error}"), true);
                return;
            }
        };
        if let Err(error) = self.recorder.enqueue(pair) {
            let app_capture_failure = !matches!(&error.reason, EnqueueRejection::Failed(_));
            self.fail(format!("enqueue:{:?}", error.reason), app_capture_failure);
        }
    }

    fn fail(&mut self, failure: String, app_capture_failure: bool) {
        if let Some((failure, app_capture_failure)) = self.capture.fail(failure, app_capture_failure)
        {
            let notice = recorder_failure_notice(failure, app_capture_failure);
            eprintln!("{notice}");
            self.recorder.request_cancel();
        }
    }

    fn startup_decision(&self) -> AppResult<StartupPairDecision> {
        let telemetry = self.recorder.telemetry();
        let decision = startup_pair_decision(
            telemetry.state,
            telemetry.completed_pairs,
            telemetry.output_bytes,
            telemetry.failure.is_some() || self.capture.first_failure.is_some(),
        );
        if decision != StartupPairDecision::Failed {
            return Ok(decision);
        }
        let failure = self
            .capture
            .first_failure
            .clone()
            .or_else(|| telemetry.failure.map(|failure| format!("{failure:?}")))
            .unwrap_or_else(|| "unknown".to_owned());
        Err(AppFailure(format!(
            "Program recorder failed before readiness: {}",
            diagnostic_field(&failure)
        ))
        .into())
    }

    fn fail_startup_timeout(&mut self) -> Box<dyn Error> {
        self.fail("startup:mux_readiness_timeout".to_owned(), false);
        AppFailure("Program recorder mux output timed out before readiness".into()).into()
    }

    fn stop_and_report(&mut self) -> AppResult<()> {
        if let Some(clean) = self.finalization_clean {
            return if clean {
                Ok(())
            } else {
                Err(AppFailure("Program recording did not finalize cleanly".into()).into())
            };
        }
        let report = self.recorder.stop();
        let app_capture_failure = self.capture.app_capture_failure;
        let failure = self
            .capture
            .first_failure
            .clone()
            .or_else(|| {
                report
                    .telemetry
                    .failure
                    .as_ref()
                    .map(|value| format!("{value:?}"))
            })
            .unwrap_or_else(|| "none".to_owned());
        eprintln!(
            "FREEMIXD_RECORDER\tv=1\tstate={:?}\toutcome={:?}\taccepted_pairs={}\tcompleted_pairs={}\toutput_bytes={}\toutput_finalization={:?}\tcleanup={:?}\tapp_capture_failure={}\tfailure={}",
            report.telemetry.state,
            report.outcome,
            report.telemetry.accepted_pairs,
            report.telemetry.completed_pairs,
            report.telemetry.output_bytes,
            report.output,
            report.cleanup,
            app_capture_failure,
            diagnostic_field(&failure),
        );
        let complete = report.telemetry.state == RecorderState::Stopped
            && report.outcome == StopOutcome::Clean
            && report.output == OutputFinalization::Synced
            && report.cleanup == CleanupStatus::Complete
            && report.telemetry.accepted_pairs == report.telemetry.completed_pairs
            && report.telemetry.failed_pairs == 0
            && !app_capture_failure;
        self.finalization_clean = Some(complete);
        if complete {
            Ok(())
        } else {
            Err(AppFailure("Program recording did not finalize cleanly".into()).into())
        }
    }
}

#[cfg(feature = "native-media")]
impl Drop for NativeProgramRecorder {
    fn drop(&mut self) {
        let _ = self.stop_and_report();
    }
}

#[cfg(feature = "native-media")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StartupPairDecision {
    Pending,
    Ready,
    Failed,
}

#[cfg(feature = "native-media")]
fn startup_pair_decision(
    state: RecorderState,
    completed_pairs: u64,
    output_bytes: u64,
    failed: bool,
) -> StartupPairDecision {
    if failed || state == RecorderState::Failed {
        StartupPairDecision::Failed
    } else if completed_pairs > 0 && output_bytes > 0 {
        StartupPairDecision::Ready
    } else {
        StartupPairDecision::Pending
    }
}

#[cfg(feature = "native-media")]
#[derive(Debug)]
enum NativeRealizationError {
    Video(NativeSourceRenderError),
    Audio(NativeMasterError),
}

#[cfg(feature = "native-media")]
impl fmt::Display for NativeRealizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Video(error) => error.fmt(formatter),
            Self::Audio(error) => error.fmt(formatter),
        }
    }
}

#[cfg(feature = "native-media")]
impl Error for NativeRealizationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Video(error) => Some(error),
            Self::Audio(error) => Some(error),
        }
    }
}

#[cfg(feature = "native-media")]
impl NativeDaemon {
    fn invalidate_projection(&mut self) {
        self.projected_frame = None;
    }

    fn start(
        store: &ProjectStore,
        stored: &StoredProject,
        camera_helper: Option<&Path>,
    ) -> AppResult<Self> {
        Self::start_with_optional_context(store, stored, None, camera_helper)
    }

    fn start_with_optional_context(
        store: &ProjectStore,
        stored: &StoredProject,
        context: Option<NativeContext>,
        camera_helper: Option<&Path>,
    ) -> AppResult<Self> {
        let project_plan =
            NativeProjectPlan::compile(stored.project(), NativeProjectLimits::default())?;
        validate_native_audio_modes(stored)?;
        #[cfg(target_os = "macos")]
        let mut resolution = resolve_native_sources(store, stored, camera_helper)?;
        #[cfg(not(target_os = "macos"))]
        let resolution = resolve_native_sources(store, stored, camera_helper)?;
        let sources = resolution.sources;
        validate_native_stinger_sources(stored, &sources)?;
        let requires_ffmpeg = sources
            .iter()
            .any(|source| matches!(source, NativeResolvedSource::LocalVideo { .. }));
        let adapter = requires_ffmpeg
            .then(|| {
                Adapter::new(FfmpegConfig {
                    allowed_root: Some(store.assets_root()),
                    ..FfmpegConfig::default()
                })
            })
            .transpose()?;
        if let Some(adapter) = &adapter {
            let capabilities = adapter.capabilities();
            if !matches!(capabilities.ffmpeg, ToolAvailability::Available { .. })
                || !matches!(capabilities.ffprobe, ToolAvailability::Available { .. })
            {
                return Err(AppFailure(
                    "native video playback requires available ffmpeg and ffprobe capabilities"
                        .into(),
                )
                .into());
            }
        }

        let master = NativeMasterRuntime::preflight_project_local_blocking(
            adapter.as_ref(),
            &sources,
            &project_plan,
            &stored.project().settings().audio,
            stored.project().settings().frame_rate,
            native_clock_domain(),
            stored.position().frames_rendered,
            NativeAudioLimits::default(),
        )?;
        let runtime = Arc::new(match context {
            Some(context) => NativeMediaRuntime::from_context_blocking(context)?,
            None => NativeMediaRuntime::new_blocking([platform_native_backend()])?,
        });
        let (playback, stingers) =
            preflight_native_video(&runtime, adapter.as_ref(), sources.clone(), stored)?;
        #[cfg(target_os = "macos")]
        resolution.cameras.mark_preflight_frames_ingested();
        let pacer = FramePacer::restore(
            stored.project().settings().frame_rate,
            0,
            FrameNumber::new(stored.position().frames_rendered),
        )?;
        let pacer_start_offset = host_deadline_offset(&pacer)?;
        let origin = Instant::now();
        let telemetry = NativeRuntimeTelemetry::new(origin, runtime.context().adapter_info());
        Ok(Self {
            pacer,
            pacer_start_offset,
            origin,
            latest_output: None,
            latest_project_outputs: Vec::new(),
            master,
            project_plan,
            playback,
            stingers,
            projected_frame: None,
            runtime,
            resolved_sources: Arc::new(sources),
            assets_root: store.assets_root().to_path_buf(),
            pending_stinger_mutation: None,
            stinger_retirements: NativeStingerRetirements::start()?,
            recorder: None,
            telemetry,
            telemetry_emitted: false,
            #[cfg(target_os = "macos")]
            cameras: resolution.cameras,
            #[cfg(target_os = "macos")]
            camera_telemetry_frozen: false,
            #[cfg(all(feature = "macos-program-surface", target_os = "macos"))]
            program: None,
        })
    }

    #[cfg(all(feature = "macos-program-surface", target_os = "macos"))]
    fn start_with_program(
        store: &ProjectStore,
        stored: &StoredProject,
        context: NativeContext,
        surface: NativeSurface,
        channels: program_surface::ProgramWorkerChannels,
        camera_helper: Option<&Path>,
    ) -> AppResult<Self> {
        let mut native =
            Self::start_with_optional_context(store, stored, Some(context), camera_helper)?;
        native.program = Some(program_surface::ProgramPresentation::new(surface, channels));
        Ok(native)
    }

    fn start_recorder(&mut self, stored: &StoredProject, path: &Path) -> AppResult<()> {
        self.recorder = Some(NativeProgramRecorder::start(&self.runtime, stored, path)?);
        Ok(())
    }

    fn prime_recorder(
        &mut self,
        control: &mut ControlService<Policy>,
        server: &ServerIdentity,
        shutdown: &ProcessShutdown,
    ) -> AppResult<bool> {
        if self.recorder.is_none() {
            return Ok(true);
        }
        let startup_timeout = self
            .recorder
            .as_ref()
            .expect("recorder was checked")
            .startup_pair_timeout;
        let deadline = Instant::now()
            .checked_add(startup_timeout)
            .ok_or_else(|| AppFailure("Program recorder startup deadline overflow".into()))?;
        loop {
            if requested_daemon_shutdown(Some(&*self), Some(shutdown)).is_some() {
                return Ok(false);
            }
            match self
                .recorder
                .as_ref()
                .expect("recorder was checked")
                .startup_decision()?
            {
                StartupPairDecision::Ready => return Ok(true),
                StartupPairDecision::Failed => unreachable!("failure is returned as an error"),
                StartupPairDecision::Pending => {}
            }
            if Instant::now() >= deadline {
                return Err(self
                    .recorder
                    .as_mut()
                    .expect("recorder was checked")
                    .fail_startup_timeout());
            }
            self.tick_if_due(control, server)?;
            thread::sleep(NATIVE_IO_POLL_INTERVAL);
        }
    }

    fn tick_if_due(
        &mut self,
        control: &mut ControlService<Policy>,
        server: &ServerIdentity,
    ) -> AppResult<()> {
        let _ = self.tick_if_due_collect(control, server)?;
        Ok(())
    }

    fn tick_if_due_collect(
        &mut self,
        control: &mut ControlService<Policy>,
        server: &ServerIdentity,
    ) -> AppResult<Option<Vec<RuntimeEventMessage>>> {
        #[cfg(all(feature = "macos-program-surface", target_os = "macos"))]
        if let Some(program) = &mut self.program {
            program
                .service_control(self.runtime.context())
                .map_err(|error| -> Box<dyn Error> { Box::new(AppFailure(error)) })?;
        }
        let covered = self.service_playback(control)?;
        let host_deadline = self.next_deadline()?;
        let now = Instant::now();
        if now < host_deadline {
            return Ok(None);
        }
        if !covered {
            return Ok(None);
        }
        self.telemetry.observe_host_lateness(host_deadline, now);
        self.realize_one(control, server).map(Some)
    }

    fn wait_and_tick(
        &mut self,
        control: &mut ControlService<Policy>,
        server: &ServerIdentity,
        process_shutdown: Option<&ProcessShutdown>,
    ) -> AppResult<Option<Vec<RuntimeEventMessage>>> {
        let (host_deadline, observed) = loop {
            if requested_daemon_shutdown(Some(&*self), process_shutdown).is_some() {
                return Ok(None);
            }
            let covered = self.service_playback(control)?;
            let host_deadline = self.next_deadline()?;
            let now = Instant::now();
            if covered && now >= host_deadline {
                break (host_deadline, now);
            }
            let host_wait = host_deadline.saturating_duration_since(now);
            thread::sleep(host_wait.min(NATIVE_IO_POLL_INTERVAL));
        };
        self.telemetry
            .observe_host_lateness(host_deadline, observed);
        self.realize_one(control, server).map(Some)
    }

    fn service_playback(&mut self, control: &ControlService<Policy>) -> AppResult<bool> {
        #[cfg(target_os = "macos")]
        self.cameras.poll_into(&self.runtime, &mut self.playback)?;
        let deadline = control.next_frame_deadline()?;
        let video_covered = self
            .runtime
            .service_source_playback_blocking(&mut self.playback, deadline)
            .map_err(Box::<dyn Error>::from)?;
        if self.projected_frame.is_none() {
            self.projected_frame = Some(control.project_next_frame()?);
        }
        let projected = self
            .projected_frame
            .as_ref()
            .expect("next frame projection was initialized");
        let audio_covered = self
            .master
            .service_project_next_frame(projected, &self.project_plan)?;
        let stinger_covered = match self.project_plan.stinger_frame_request(projected)? {
            Some(request) => self
                .runtime
                .service_source_playback_for_input_blocking(
                    &mut self.stingers,
                    request.input,
                    request.deadline,
                )
                .map_err(Box::<dyn Error>::from)?,
            None => true,
        };
        Ok(audio_covered && video_covered && stinger_covered)
    }

    fn realize_one(
        &mut self,
        control: &mut ControlService<Policy>,
        server: &ServerIdentity,
    ) -> AppResult<Vec<RuntimeEventMessage>> {
        let runtime = &self.runtime;
        let registry = self.playback.registry();
        let stinger_registry = self.stingers.registry();
        let master = &mut self.master;
        let project_plan = &mut self.project_plan;
        let projected_frame = self.projected_frame.as_ref();
        let latest_output = &mut self.latest_output;
        let latest_project_outputs = &mut self.latest_project_outputs;
        let mut audio = None;
        let outcome = control.tick_with_realizer(server, |frame| {
            debug_assert_eq!(projected_frame, Some(frame));
            for &(input, state) in &frame.input_audio_strip_updates {
                let strip = model_audio_strip_state(state);
                master
                    .set_input_audio_strip(input, strip)
                    .map_err(NativeRealizationError::Audio)?;
                project_plan
                    .set_input_audio_strip(input, strip)
                    .map_err(NativeRealizationError::Audio)?;
            }
            if project_plan.output_ids().is_empty() {
                let output = runtime
                    .render_project_frame_result_with_stingers_blocking(
                        registry,
                        stinger_registry,
                        project_plan,
                        frame,
                    )
                    .map_err(NativeRealizationError::Video)?;
                *latest_output = Some(output);
                latest_project_outputs.clear();
            } else {
                let outputs = runtime
                    .render_project_outputs_with_stingers_blocking(
                        registry,
                        stinger_registry,
                        project_plan,
                        frame,
                    )
                    .map_err(NativeRealizationError::Video)?;
                *latest_project_outputs = outputs;
                *latest_output = None;
            }
            let block = master
                .render_project_frame_audio(frame, project_plan)
                .map_err(NativeRealizationError::Audio)?;
            audio = Some(block);
            Ok::<(), NativeRealizationError>(())
        })?;
        self.install_stinger_mutation()?;
        self.projected_frame = None;
        self.pacer.advance()?;
        let latest_program = self
            .latest_project_outputs
            .first()
            .map(NativeOutputFrame::texture)
            .or(self.latest_output.as_ref());
        #[cfg(all(feature = "macos-program-surface", target_os = "macos"))]
        if let (Some(program), Some(output)) = (&mut self.program, latest_program) {
            program
                .present_latest(self.runtime.context(), output)
                .map_err(|error| -> Box<dyn Error> { Box::new(AppFailure(error)) })?;
        }
        if let (Some(recorder), Some(output), Some(audio)) =
            (&mut self.recorder, latest_program, audio)
        {
            recorder.capture(&self.runtime, output, audio);
        }
        self.observe_native_telemetry();
        Ok(outcome.runtime_events)
    }

    #[allow(clippy::unused_self)]
    fn shutdown_requested(&self) -> bool {
        #[cfg(all(feature = "macos-program-surface", target_os = "macos"))]
        if let Some(program) = &self.program {
            return program.shutdown_requested();
        }
        false
    }

    fn recorder_active(&self) -> bool {
        self.recorder.is_some()
    }

    fn finalize_recorder(&mut self) -> AppResult<()> {
        let Some(mut recorder) = self.recorder.take() else {
            return Ok(());
        };
        let result = recorder.stop_and_report();
        self.telemetry.observe_recorder(&recorder);
        result
    }

    fn finalize_cameras(&mut self) -> AppResult<()> {
        #[cfg(target_os = "macos")]
        {
            self.cameras.shutdown()
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(())
        }
    }

    #[cfg(all(feature = "macos-program-surface", target_os = "macos"))]
    fn program_telemetry(&self) -> Option<fm_gpu::PresentationTelemetry> {
        self.program
            .as_ref()
            .map(program_surface::ProgramPresentation::telemetry)
    }

    fn observe_native_telemetry(&mut self) {
        self.telemetry.observe_audio(&self.master);
        #[cfg(target_os = "macos")]
        if !self.camera_telemetry_frozen {
            self.telemetry.camera = aggregate_camera_telemetry(&self.cameras.source_telemetry());
        }
        if let Some(recorder) = self.recorder.as_ref() {
            self.telemetry.observe_recorder(recorder);
        }
        self.telemetry.observe_gpu(self.runtime.context());
    }

    fn emit_telemetry(&mut self) {
        if self.telemetry_emitted {
            return;
        }
        self.emit_camera_source_telemetry();
        self.observe_native_telemetry();
        #[cfg(all(feature = "macos-program-surface", target_os = "macos"))]
        let presentation = self
            .program
            .as_mut()
            .map(program_surface::ProgramPresentation::telemetry_for_shutdown);
        #[cfg(not(all(feature = "macos-program-surface", target_os = "macos")))]
        let presentation = None;
        self.telemetry.emit(presentation);
        self.telemetry_emitted = true;
    }

    fn emit_camera_source_telemetry(&mut self) {
        #[cfg(target_os = "macos")]
        {
            if self.camera_telemetry_frozen {
                return;
            }
            if let Some(sources) = self.cameras.emit_source_telemetry() {
                self.telemetry.camera = aggregate_camera_telemetry(&sources);
            }
            self.camera_telemetry_frozen = true;
        }
    }

    fn next_deadline(&self) -> AppResult<Instant> {
        let elapsed_offset = host_deadline_offset(&self.pacer)?
            .checked_sub(self.pacer_start_offset)
            .ok_or_else(|| {
                AppFailure("native media pacer moved before its restored cursor".into())
            })?;
        self.origin.checked_add(elapsed_offset).ok_or_else(|| {
            AppFailure("native media host deadline exceeds Instant range".into()).into()
        })
    }
}

#[cfg(feature = "native-media")]
fn validate_native_stinger_sources(
    stored: &StoredProject,
    sources: &[NativeResolvedSource],
) -> AppResult<()> {
    for config in stored
        .project()
        .stingers()
        .iter()
        .filter(|config| config.preload)
    {
        let source = sources
            .iter()
            .find(|source| source.input() == config.media_input)
            .ok_or_else(|| {
                AppFailure(format!(
                    "native Stinger slot {} media input {} is unavailable",
                    config.slot.number(),
                    config.media_input
                ))
            })?;
        match source {
            NativeResolvedSource::RetainedFrame { .. }
            | NativeResolvedSource::LocalVideo { .. } => {}
            NativeResolvedSource::LiveFrame { .. } => {
                return Err(AppFailure(format!(
                    "native Stinger slot {} media input {} is live and cannot be preloaded",
                    config.slot.number(),
                    config.media_input
                ))
                .into());
            }
        }
    }
    Ok(())
}

#[cfg(feature = "native-media")]
fn partition_native_source_limits(
    width: u32,
    height: u32,
    stinger_sources: usize,
) -> AppResult<(NativeSourceLimits, NativeSourceLimits)> {
    let defaults = NativeSourceLimits::default();
    let stinger_ring_bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(8))
        .and_then(|bytes| bytes.checked_mul(u64::from(defaults.max_video_frames_per_source.get())))
        .and_then(|bytes| bytes.checked_mul(u64::try_from(stinger_sources).unwrap_or(u64::MAX)))
        .ok_or_else(|| AppFailure("native Stinger ring byte partition overflow".into()))?;
    if stinger_ring_bytes > defaults.max_retained_rgba16f_bytes {
        return Err(AppFailure(format!(
            "native Stinger rings require {stinger_ring_bytes} retained bytes, exceeding aggregate limit {}",
            defaults.max_retained_rgba16f_bytes
        ))
        .into());
    }
    Ok((
        NativeSourceLimits {
            max_retained_rgba16f_bytes: defaults
                .max_retained_rgba16f_bytes
                .saturating_sub(stinger_ring_bytes),
            ..defaults
        },
        NativeSourceLimits {
            max_retained_rgba16f_bytes: stinger_ring_bytes,
            ..defaults
        },
    ))
}

#[cfg(feature = "native-media")]
fn preflight_native_video(
    runtime: &NativeMediaRuntime,
    adapter: Option<&Adapter>,
    sources: Vec<NativeResolvedSource>,
    stored: &StoredProject,
) -> AppResult<(
    freemixd::native_media::NativeSourcePlayback,
    freemixd::native_media::NativeSourcePlayback,
)> {
    let (stingers, playback_limit) =
        preflight_native_stinger_video(runtime, adapter, &sources, stored)?;
    let playback = runtime.preflight_resolved_source_playback_mixed_blocking(
        adapter,
        sources,
        native_clock_domain(),
        StreamSelector::Best,
        NativeSourceLimits {
            max_retained_rgba16f_bytes: playback_limit,
            ..NativeSourceLimits::default()
        },
    )?;
    let expected = stored.project().settings().video.dimensions;
    validate_native_video_dimensions("media source", playback.registry(), expected)?;
    Ok((playback, stingers))
}

#[cfg(feature = "native-media")]
fn preflight_native_stinger_video(
    runtime: &NativeMediaRuntime,
    adapter: Option<&Adapter>,
    sources: &[NativeResolvedSource],
    stored: &StoredProject,
) -> AppResult<(freemixd::native_media::NativeSourcePlayback, u64)> {
    let stinger_inputs = stored
        .project()
        .stingers()
        .iter()
        .filter(|config| config.preload)
        .map(|config| config.media_input)
        .fold(Vec::new(), |mut inputs, input| {
            if !inputs.contains(&input) {
                inputs.push(input);
            }
            inputs
        });
    let stinger_sources = sources
        .iter()
        .filter(|source| stinger_inputs.contains(&source.input()))
        .cloned()
        .collect::<Vec<_>>();
    let expected = stored.project().settings().video.dimensions;
    let (playback_limits, stinger_limits) =
        partition_native_source_limits(expected.width(), expected.height(), stinger_sources.len())?;
    let mut stingers = runtime.preflight_resolved_source_playback_mixed_blocking(
        adapter,
        stinger_sources,
        native_clock_domain(),
        StreamSelector::Best,
        stinger_limits,
    )?;
    for input in stinger_inputs {
        stingers.enable_stinger_source(input)?;
    }
    validate_native_video_dimensions("Stinger", stingers.registry(), expected)?;
    Ok((stingers, playback_limits.max_retained_rgba16f_bytes))
}

#[cfg(feature = "native-media")]
fn validate_native_video_dimensions(
    label: &str,
    registry: &freemixd::native_media::NativeSourceRegistry,
    expected: fm_frame::VideoDimensions,
) -> AppResult<()> {
    if registry
        .dimensions()
        .is_some_and(|dimensions| dimensions != (expected.width(), expected.height()))
    {
        return Err(AppFailure(format!(
            "native {label} dimensions must match project output {}x{}",
            expected.width(),
            expected.height()
        ))
        .into());
    }
    Ok(())
}

#[cfg(feature = "native-media")]
impl Drop for NativeDaemon {
    fn drop(&mut self) {
        if let Some(mutation) = self.pending_stinger_mutation.take() {
            let _ = self.stinger_retirements.discard(mutation);
        }
        let _ = self.finalize_recorder();
        self.emit_camera_source_telemetry();
        #[cfg(target_os = "macos")]
        let _ = self.cameras.shutdown();
        self.emit_telemetry();
    }
}

#[cfg(feature = "native-media")]
fn validate_native_audio_modes(stored: &StoredProject) -> AppResult<()> {
    if let Some(input) = stored.project().inputs().iter().find(|input| {
        matches!(
            &input.kind,
            InputKind::Simulated(fm_model::SimulatedInput {
                audio: SimulatedAudio::Sine { .. },
                ..
            })
        )
    }) {
        return Err(AppFailure(format!(
            "native generator input {} requires unsupported simulated sine audio",
            input.id
        ))
        .into());
    }
    Ok(())
}

#[cfg(not(feature = "native-media"))]
struct NativeDaemon;

#[cfg(not(feature = "native-media"))]
impl NativeDaemon {
    #[allow(clippy::unused_self)]
    fn invalidate_projection(&mut self) {}

    fn start(
        _store: &ProjectStore,
        _stored: &StoredProject,
        _camera_helper: Option<&Path>,
    ) -> AppResult<Self> {
        Err(AppFailure("native-media support was not compiled in".into()).into())
    }

    #[allow(clippy::unused_self)]
    fn tick_if_due(
        &mut self,
        _control: &mut ControlService<Policy>,
        _server: &ServerIdentity,
    ) -> AppResult<()> {
        Err(AppFailure("native-media support was not compiled in".into()).into())
    }

    #[allow(clippy::unused_self)]
    fn wait_and_tick(
        &mut self,
        _control: &mut ControlService<Policy>,
        _server: &ServerIdentity,
        _process_shutdown: Option<&ProcessShutdown>,
    ) -> AppResult<Option<Vec<RuntimeEventMessage>>> {
        Err(AppFailure("native-media support was not compiled in".into()).into())
    }

    #[allow(clippy::unused_self)]
    fn shutdown_requested(&self) -> bool {
        false
    }

    #[allow(clippy::unused_self)]
    fn start_recorder(&mut self, _stored: &StoredProject, _path: &Path) -> AppResult<()> {
        Err(AppFailure("native-media support was not compiled in".into()).into())
    }

    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    fn prime_recorder(
        &mut self,
        _control: &mut ControlService<Policy>,
        _server: &ServerIdentity,
        _shutdown: &ProcessShutdown,
    ) -> AppResult<bool> {
        Ok(true)
    }

    #[allow(clippy::unused_self)]
    fn recorder_active(&self) -> bool {
        false
    }

    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    fn finalize_recorder(&mut self) -> AppResult<()> {
        Ok(())
    }

    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    fn finalize_cameras(&mut self) -> AppResult<()> {
        Ok(())
    }

    #[allow(clippy::unused_self)]
    fn emit_camera_source_telemetry(&mut self) {}

    #[allow(clippy::unused_self)]
    fn emit_telemetry(&mut self) {}
}

#[cfg(feature = "native-media")]
fn resolve_native_sources(
    store: &ProjectStore,
    stored: &StoredProject,
    camera_helper: Option<&Path>,
) -> AppResult<NativeSourceResolution> {
    let settings = stored.project().settings();
    let mut sources = Vec::with_capacity(stored.project().inputs().len());
    for input in stored.project().inputs() {
        let source = match &input.kind {
            InputKind::Media { asset_uri } => resolve_native_media_source(
                store,
                input.id,
                asset_uri,
                native_clock_domain(),
                settings.video.dimensions,
            ),
            InputKind::Simulated(simulated) => {
                let pattern = match simulated.video {
                    SimulatedVideo::Bars => SourcePattern::Bars,
                    SimulatedVideo::Solid(color) => SourcePattern::Solid(Rgba8::new(
                        color.red,
                        color.green,
                        color.blue,
                        color.alpha,
                    )),
                };
                let dimensions = settings.video.dimensions;
                let mut source = SimulatedVideoSource::new(
                    dimensions.width(),
                    dimensions.height(),
                    settings.frame_rate,
                    native_clock_domain(),
                    pattern,
                )
                .map_err(|error| {
                    AppFailure(format!(
                        "native generator construction failed for input {}: {error}",
                        input.id
                    ))
                })?;
                let frame = source
                    .next_frame()
                    .map_err(|error| {
                        AppFailure(format!(
                            "native generator frame failed for input {}: {error}",
                            input.id
                        ))
                    })?
                    .ok_or_else(|| {
                        AppFailure(format!(
                            "native generator produced signal loss for input {}",
                            input.id
                        ))
                    })?;
                Ok(NativeResolvedSource::RetainedFrame {
                    input: input.id,
                    frame,
                })
            }
            InputKind::Color => Err(AppFailure(format!(
                "native color input {} has no configured RGBA value",
                input.id
            ))
            .into()),
            InputKind::Device { .. } | InputKind::Scene { .. } => continue,
            InputKind::Network { .. } => Err(AppFailure(format!(
                "native network input {} is not supported by this playback mode",
                input.id
            ))
            .into()),
        }?;
        sources.push(source);
    }

    #[cfg(target_os = "macos")]
    {
        let (mut device_sources, cameras) = resolve_macos_camera_sources(stored, camera_helper)?;
        sources.append(&mut device_sources);
        Ok(NativeSourceResolution { sources, cameras })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = camera_helper;
        if let Some(input) = stored
            .project()
            .inputs()
            .iter()
            .find(|input| matches!(input.kind, InputKind::Device { .. }))
        {
            return Err(AppFailure(format!(
                "native device input {} has no adapter on this platform",
                input.id
            ))
            .into());
        }
        Ok(NativeSourceResolution { sources })
    }
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
#[allow(clippy::too_many_lines)]
fn resolve_macos_camera_sources(
    stored: &StoredProject,
    camera_helper: Option<&Path>,
) -> AppResult<(Vec<NativeResolvedSource>, NativeCameraInputs)> {
    resolve_macos_camera_sources_with_policy(stored, camera_helper, CAMERA_RECOVERY_POLICY)
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
#[allow(clippy::too_many_lines)]
fn resolve_macos_camera_sources_with_policy(
    stored: &StoredProject,
    camera_helper: Option<&Path>,
    recovery_policy: CameraRecoveryPolicy,
) -> AppResult<(Vec<NativeResolvedSource>, NativeCameraInputs)> {
    let device_inputs = stored
        .project()
        .inputs()
        .iter()
        .filter_map(|input| match &input.kind {
            InputKind::Device { stable_key } => Some((input.id, stable_key.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    if device_inputs.is_empty() {
        return Ok((Vec::new(), NativeCameraInputs::default()));
    }
    if device_inputs.len() > NativeSourceLimits::DEFAULT_MAX_MEDIA_INPUTS {
        return Err(AppFailure(format!(
            "camera input count {} exceeds native source limit {}",
            device_inputs.len(),
            NativeSourceLimits::DEFAULT_MAX_MEDIA_INPUTS
        ))
        .into());
    }
    let mut keys = std::collections::BTreeSet::new();
    if let Some((input, key)) = device_inputs.iter().find(|(_, key)| !keys.insert(*key)) {
        return Err(AppFailure(format!(
            "camera input {input} duplicates stable key `{}`",
            diagnostic_field(key)
        ))
        .into());
    }

    let adapter = match camera_helper {
        Some(path) => MacosCameraAdapter::discover_with_helper(path)?,
        None => MacosCameraAdapter::discover()?,
    };
    if !adapter.permission().is_granted() {
        return Err(AppFailure(
            "camera permission is not granted; use `freemix-capture-node cameras --request-permission` from an interactive desktop session"
                .into(),
        )
        .into());
    }
    let frame_rate = stored.project().settings().frame_rate;
    let dimensions = stored.project().settings().video.dimensions;
    let mut cameras = NativeCameraInputs::default();
    for (input, stable_key) in device_inputs {
        let mut source = adapter.open_video_source_by_stable_key(stable_key)?;
        let format = source
            .exact_video_format_at_rate(dimensions.width(), dimensions.height(), frame_rate)
            .ok_or_else(|| {
                AppFailure(format!(
                    "camera input {input} has no exact {}x{} at {}/{} fps BGRA mode",
                    dimensions.width(),
                    dimensions.height(),
                    frame_rate.numerator(),
                    frame_rate.denominator()
                ))
            })?;
        let clock_domain = source
            .descriptor()
            .capabilities
            .clocks
            .first()
            .ok_or_else(|| AppFailure(format!("camera input {input} has no advertised clock")))?
            .domain;
        source.open(IoOpenOptions {
            format,
            clock_domain,
            memory_domain: MemoryDomain::Cpu,
            queue_capacity: NonZeroUsize::MIN,
            signal_loss: SignalLossPolicy::Hold,
        })?;
        cameras.inputs.push(NativeCameraInput {
            input,
            supervisor: Arc::new(Mutex::new(CameraSupervisorState {
                frame: CameraFrameSlot::default(),
                snapshot: CameraWorkerSnapshot::running(&source),
            })),
            source: Some(source),
            worker: None,
            recovery_policy,
            ingested_frames: 0,
            ingest_failed: 0,
            preflight_depth: 0,
            preflight_discarded: 0,
            last_ingested_sequence: None,
            last_ingested_discontinuity: false,
        });
    }

    let start_results = thread::scope(|scope| -> AppResult<Vec<(InputId, Result<(), String>)>> {
        let mut handles = Vec::with_capacity(cameras.inputs.len());
        for camera in &mut cameras.inputs {
            let input = camera.input;
            let source = camera.source.as_mut().expect("camera source is configured");
            handles.push(
                thread::Builder::new()
                    .name(format!("freemix-camera-start-{input}"))
                    .spawn_scoped(scope, move || {
                        (input, source.start().map_err(|error| error.to_string()))
                    })?,
            );
        }
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .map_err(|_| AppFailure("camera startup worker panicked".into()).into())
            })
            .collect()
    });
    let start_results = match start_results {
        Ok(results) => results,
        Err(error) => {
            return Err(camera_startup_failure(
                &mut cameras,
                format!("camera startup coordination failed: {error}"),
            ));
        }
    };
    if let Some((input, Err(detail))) = start_results
        .into_iter()
        .find(|(_, result)| result.is_err())
    {
        return Err(camera_startup_failure(
            &mut cameras,
            format!("camera input {input} failed to start: {detail}"),
        ));
    }

    let deadline = Instant::now()
        .checked_add(CAMERA_INITIAL_FRAME_TIMEOUT)
        .ok_or_else(|| AppFailure("camera initial-frame deadline overflow".into()))?;
    let mut initial_frames = BTreeMap::new();
    while initial_frames.len() < cameras.inputs.len() {
        for camera in &mut cameras.inputs {
            if initial_frames.contains_key(&camera.input) {
                continue;
            }
            let transfer = {
                let source = camera.source.as_mut().expect("camera source is configured");
                let transfer = source.try_receive();
                let mut state = camera
                    .supervisor
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                update_camera_worker_snapshot_inner(&mut state.snapshot, source);
                transfer
            };
            let transfer = match transfer {
                Ok(transfer) => transfer,
                Err(error) => {
                    let detail = format!(
                        "camera input {} initial-frame acquisition failed: {error}",
                        camera.input
                    );
                    return Err(camera_startup_failure(&mut cameras, detail));
                }
            };
            match transfer {
                Some(MediaTransfer::Live(frame)) => {
                    camera.preflight_depth = 1;
                    initial_frames.insert(camera.input, frame);
                }
                Some(MediaTransfer::Fallback { .. }) => {
                    let detail = format!(
                        "camera input {} returned fallback media during startup",
                        camera.input
                    );
                    return Err(camera_startup_failure(&mut cameras, detail));
                }
                None => {}
            }
        }
        if initial_frames.len() == cameras.inputs.len() {
            break;
        }
        if Instant::now() >= deadline {
            let detail = format!(
                "camera initial-frame acquisition timed out with {}/{} sources ready",
                initial_frames.len(),
                cameras.inputs.len()
            );
            return Err(camera_startup_failure(&mut cameras, detail));
        }
        thread::sleep(Duration::from_millis(2));
    }
    let sources = cameras
        .inputs
        .iter()
        .map(|camera| NativeResolvedSource::LiveFrame {
            input: camera.input,
            frame: initial_frames
                .remove(&camera.input)
                .expect("all camera inputs produced an initial frame"),
        })
        .collect();
    cameras.start_workers()?;
    Ok((sources, cameras))
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
fn camera_startup_failure(cameras: &mut NativeCameraInputs, mut detail: String) -> Box<dyn Error> {
    cameras.discard_preflight_frames();
    if cameras
        .cleanup_startup_sources(CAMERA_STARTUP_CLEANUP_TIMEOUT)
        .is_err()
    {
        detail.push_str("; camera startup cleanup also failed");
    }
    Box::new(AppFailure(detail))
}

#[cfg(feature = "native-media")]
fn resolve_native_media_source(
    store: &ProjectStore,
    input: InputId,
    asset_uri: &str,
    clock_domain: MediaClockDomainId,
    output_dimensions: fm_types::VideoDimensions,
) -> AppResult<NativeResolvedSource> {
    let path = store.resolve_asset_uri(asset_uri).map_err(|error| {
        AppFailure(format!(
            "native media asset resolution failed for input {input}: {error}"
        ))
    })?;
    let mut file = File::open(&path).map_err(|error| {
        AppFailure(format!(
            "native media read failed for input {input}: {:?}",
            error.kind()
        ))
    })?;
    let mut prefix = Vec::with_capacity(8);
    Read::by_ref(&mut file)
        .take(8)
        .read_to_end(&mut prefix)
        .map_err(|error| {
            AppFailure(format!(
                "native media read failed for input {input}: {:?}",
                error.kind()
            ))
        })?;
    if sniff_still_format(&prefix).is_err() {
        return Ok(NativeResolvedSource::LocalVideo { input, path });
    }

    let limits = still_limits(output_dimensions)?;
    let mut encoded = prefix;
    let remaining = limits
        .max_encoded_bytes
        .saturating_add(1)
        .saturating_sub(encoded.len());
    file.take(u64::try_from(remaining).unwrap_or(u64::MAX))
        .read_to_end(&mut encoded)
        .map_err(|error| {
            AppFailure(format!(
                "native still read failed for input {input}: {:?}",
                error.kind()
            ))
        })?;
    let decoded =
        decode_still(&encoded, retained_frame_timing(clock_domain)?, limits).map_err(|error| {
            AppFailure(format!(
                "native still decode failed for input {input}: {error}"
            ))
        })?;
    Ok(NativeResolvedSource::RetainedFrame {
        input,
        frame: decoded.frame,
    })
}

#[cfg(feature = "native-media")]
fn still_limits(dimensions: fm_types::VideoDimensions) -> AppResult<StillDecodeLimits> {
    let maximum_axis = dimensions.width().max(dimensions.height());
    let decoded_bytes = usize::try_from(dimensions.width())
        .ok()
        .and_then(|width| width.checked_mul(4))
        .and_then(|stride| {
            stride.checked_mul(usize::try_from(dimensions.height()).unwrap_or(usize::MAX))
        })
        .ok_or_else(|| AppFailure("native still output dimensions overflow".into()))?;
    Ok(StillDecodeLimits {
        max_width: maximum_axis,
        max_height: maximum_axis,
        max_decoded_rgba_bytes: decoded_bytes,
        ..StillDecodeLimits::default()
    })
}

#[cfg(feature = "native-media")]
fn retained_frame_timing(clock_domain: MediaClockDomainId) -> AppResult<MediaTiming> {
    let time_base = fm_types::TimeBase::new(1, 1_000_000_000)
        .map_err(|error| AppFailure(format!("native still time base is invalid: {error}")))?;
    MediaTiming::new(
        OriginalTimestamp::new(MediaTimestamp::new(0), time_base),
        NormalizedTimestamp::from_nanos(0),
        NormalizedDuration::from_nanos(1)
            .map_err(|error| AppFailure(format!("native still duration is invalid: {error}")))?,
        clock_domain,
        SequenceNumber::new(0),
    )
    .map_err(|error| AppFailure(format!("native still timing is invalid: {error}")).into())
}

#[cfg(all(feature = "native-media", target_os = "macos"))]
const fn platform_native_backend() -> NativeBackend {
    NativeBackend::Metal
}

#[cfg(all(feature = "native-media", target_os = "windows"))]
const fn platform_native_backend() -> NativeBackend {
    NativeBackend::Dx12
}

#[cfg(all(feature = "native-media", target_os = "linux"))]
const fn platform_native_backend() -> NativeBackend {
    NativeBackend::Vulkan
}

#[cfg(feature = "native-media")]
fn host_deadline_offset(pacer: &FramePacer) -> Result<Duration, fm_scheduler::PacingError> {
    Ok(Duration::from_nanos(pacer.next_deadline()?.at_ns))
}

#[cfg(feature = "native-media")]
fn native_clock_domain() -> MediaClockDomainId {
    MediaClockDomainId::new(NonZeroU128::new(1).expect("one is nonzero"))
}

#[cfg(feature = "native-media")]
fn create_record_output(path: &Path) -> AppResult<File> {
    let file_name = path
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| AppFailure("recording output must name a final .mp4 file".into()))?;
    if path.extension().and_then(|extension| extension.to_str()) != Some("mp4") {
        return Err(
            AppFailure("recording output must have the final extension `.mp4`".into()).into(),
        );
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = fs::canonicalize(parent).map_err(|error| {
        AppFailure(format!(
            "recording output parent is not an existing canonical directory: {:?}",
            error.kind()
        ))
    })?;
    if !parent.is_dir() {
        return Err(AppFailure(
            "recording output parent is not an existing canonical directory".into(),
        )
        .into());
    }
    // `create_new` protects the final component. A hostile concurrent parent
    // replacement needs a future platform file-capability API, not unsafe path walking here.
    let output = parent.join(file_name);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .map_err(|error| {
            AppFailure(format!(
                "recording output cannot be created without overwrite: {:?}",
                error.kind()
            ))
        })?;
    sync_record_parent(&parent).map_err(|error| {
        AppFailure(format!(
            "recording output parent synchronization failed: {:?}",
            error.kind()
        ))
    })?;
    Ok(file)
}

#[cfg(all(feature = "native-media", unix))]
fn sync_record_parent(parent: &Path) -> std::io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(all(feature = "native-media", windows))]
fn sync_record_parent(parent: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(parent)?
        .sync_all()
}

#[cfg(feature = "native-media")]
fn diagnostic_field(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '\t' | '\r' | '\n' => ' ',
            character if character.is_control() => '?',
            character => character,
        })
        .collect()
}

#[cfg(feature = "native-media")]
fn recorder_failure_notice(failure: &str, app_capture_failure: bool) -> String {
    format!(
        "FREEMIXD_RECORDER_FAILURE\tv=1\tapp_capture_failure={app_capture_failure}\tfailure={}",
        diagnostic_field(failure),
    )
}

fn capabilities_digest(native: bool, fullscreen: bool, recorder: bool) -> &'static str {
    match (native, fullscreen, recorder) {
        (true, true, true) => FULLSCREEN_PROGRAM_RECORDER_CAPABILITIES_DIGEST,
        (true, true, false) => FULLSCREEN_PROGRAM_CAPABILITIES_DIGEST,
        (true, false, true) => PROGRAM_RECORDER_CAPABILITIES_DIGEST,
        (true, false, false) => NATIVE_MEDIA_CAPABILITIES_DIGEST,
        (false, _, _) => CAPABILITIES_DIGEST,
    }
}

#[derive(Clone)]
struct ControlHandle(SharedControl);

impl ControlPlane for ControlHandle {
    type Error = Infallible;

    fn initial_sync(&self, cursor: Option<&EventCursor>) -> Result<InitialSync, Self::Error> {
        let control = self.0.borrow();
        let payload = match cursor {
            Some(cursor) => match control.resume(cursor) {
                ResumeDecision::Events(events) => SyncPayload::Resume(events),
                ResumeDecision::Snapshot(record) => {
                    SyncPayload::Snapshot(Box::new(record.snapshot))
                }
            },
            None => SyncPayload::Snapshot(Box::new(control.snapshot().snapshot.clone())),
        };
        let diagnostics = control.diagnostics();
        Ok(InitialSync {
            engine: diagnostics.engine,
            current_revision: diagnostics.current_revision,
            payload,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Command {
    Serve {
        project: PathBuf,
        listen: SocketAddr,
        once: bool,
        native_media: bool,
        fullscreen_program: bool,
        fullscreen_display: usize,
        camera_helper: Option<PathBuf>,
        record_program: Option<PathBuf>,
        diagnostic_stop_after: Option<Duration>,
    },
    Help,
    Version,
}

fn main() -> ExitCode {
    let command = match parse_args(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("error: {error}");
            eprintln!("run `freemixd help` for usage");
            return ExitCode::from(2);
        }
    };
    if let Err(error) = run(command) {
        eprintln!("error: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[allow(clippy::too_many_lines)]
fn parse_args(arguments: impl IntoIterator<Item = String>) -> AppResult<Command> {
    let mut arguments = arguments.into_iter();
    let Some(command) = arguments.next() else {
        return Err(AppFailure("a command is required".into()).into());
    };
    match command.as_str() {
        "serve" => {
            let project = arguments
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| AppFailure("missing project path".into()))?;
            let mut listen = DEFAULT_LISTEN.parse()?;
            let mut once = false;
            let mut native_media = false;
            let mut fullscreen_program = false;
            let mut fullscreen_display = None;
            let mut camera_helper = None;
            let mut record_program = None;
            let mut diagnostic_stop_after = None;
            while let Some(option) = arguments.next() {
                if let Some(value) = option.strip_prefix("--record-program=") {
                    if record_program.is_some() {
                        return Err(AppFailure("duplicate option `--record-program`".into()).into());
                    }
                    if value.is_empty() {
                        return Err(AppFailure("missing value for --record-program".into()).into());
                    }
                    record_program = Some(PathBuf::from(value));
                    continue;
                }
                if let Some(value) = option.strip_prefix("--camera-helper=") {
                    if camera_helper.is_some() {
                        return Err(AppFailure("duplicate option `--camera-helper`".into()).into());
                    }
                    if value.is_empty() {
                        return Err(AppFailure("missing value for --camera-helper".into()).into());
                    }
                    camera_helper = Some(PathBuf::from(value));
                    continue;
                }
                if let Some(value) = option.strip_prefix("--diagnostic-stop-after=") {
                    if diagnostic_stop_after.is_some() {
                        return Err(AppFailure(
                            "duplicate option `--diagnostic-stop-after`".into(),
                        )
                        .into());
                    }
                    diagnostic_stop_after = Some(parse_diagnostic_duration(value)?);
                    continue;
                }
                match option.as_str() {
                    "--listen" => {
                        listen = arguments
                            .next()
                            .ok_or_else(|| AppFailure("missing value for --listen".into()))?
                            .parse()
                            .map_err(|error| {
                                AppFailure(format!("invalid listen address: {error}"))
                            })?;
                    }
                    "--once" => once = true,
                    "--native-media" if native_media => {
                        return Err(AppFailure("duplicate option `--native-media`".into()).into());
                    }
                    "--native-media" => native_media = true,
                    "--fullscreen-program" if fullscreen_program => {
                        return Err(
                            AppFailure("duplicate option `--fullscreen-program`".into()).into()
                        );
                    }
                    "--fullscreen-program" => fullscreen_program = true,
                    "--fullscreen-display" if fullscreen_display.is_some() => {
                        return Err(
                            AppFailure("duplicate option `--fullscreen-display`".into()).into()
                        );
                    }
                    "--fullscreen-display" => {
                        let value = arguments.next().ok_or_else(|| {
                            AppFailure("missing value for --fullscreen-display".into())
                        })?;
                        fullscreen_display = Some(value.parse::<usize>().map_err(|_| {
                            AppFailure(format!(
                                "invalid fullscreen display index `{value}`; expected a zero-based non-negative integer"
                            ))
                        })?);
                    }
                    "--camera-helper" if camera_helper.is_some() => {
                        return Err(AppFailure("duplicate option `--camera-helper`".into()).into());
                    }
                    "--camera-helper" => {
                        let value = arguments
                            .next()
                            .filter(|value| !value.starts_with("--"))
                            .ok_or_else(|| {
                                AppFailure("missing value for --camera-helper".into())
                            })?;
                        camera_helper = Some(PathBuf::from(value));
                    }
                    "--record-program" if record_program.is_some() => {
                        return Err(AppFailure("duplicate option `--record-program`".into()).into());
                    }
                    "--record-program" => {
                        let value = arguments
                            .next()
                            .filter(|value| !value.starts_with("--"))
                            .ok_or_else(|| {
                                AppFailure("missing value for --record-program".into())
                            })?;
                        record_program = Some(PathBuf::from(value));
                    }
                    "--diagnostic-stop-after" if diagnostic_stop_after.is_some() => {
                        return Err(AppFailure(
                            "duplicate option `--diagnostic-stop-after`".into(),
                        )
                        .into());
                    }
                    "--diagnostic-stop-after" => {
                        let value = arguments.next().ok_or_else(|| {
                            AppFailure("missing value for --diagnostic-stop-after".into())
                        })?;
                        diagnostic_stop_after = Some(parse_diagnostic_duration(&value)?);
                    }
                    _ => return Err(AppFailure(format!("unknown option `{option}`")).into()),
                }
            }
            if fullscreen_program && !native_media {
                return Err(
                    AppFailure("--fullscreen-program requires --native-media".into()).into(),
                );
            }
            if fullscreen_display.is_some() && !fullscreen_program {
                return Err(AppFailure(
                    "--fullscreen-display requires --fullscreen-program".into(),
                )
                .into());
            }
            if record_program.is_some() && !native_media {
                return Err(AppFailure("--record-program requires --native-media".into()).into());
            }
            if camera_helper.is_some() && !native_media {
                return Err(AppFailure("--camera-helper requires --native-media".into()).into());
            }
            if diagnostic_stop_after.is_some() && !native_media {
                return Err(
                    AppFailure("--diagnostic-stop-after requires --native-media".into()).into(),
                );
            }
            if diagnostic_stop_after.is_some() && once {
                return Err(AppFailure(
                    "--diagnostic-stop-after cannot be combined with --once".into(),
                )
                .into());
            }
            if diagnostic_stop_after.is_some() && fullscreen_program {
                return Err(AppFailure(
                    "--diagnostic-stop-after currently supports headless native mode only".into(),
                )
                .into());
            }
            Ok(Command::Serve {
                project,
                listen,
                once,
                native_media,
                fullscreen_program,
                fullscreen_display: fullscreen_display.unwrap_or(0),
                camera_helper,
                record_program,
                diagnostic_stop_after,
            })
        }
        "help" | "--help" | "-h" => {
            reject_extra(arguments)?;
            Ok(Command::Help)
        }
        "--version" | "version" => {
            reject_extra(arguments)?;
            Ok(Command::Version)
        }
        _ => Err(AppFailure(format!("unknown command `{command}`")).into()),
    }
}

fn parse_diagnostic_duration(value: &str) -> AppResult<Duration> {
    let (amount, unit) = if let Some(amount) = value.strip_suffix("ms") {
        (amount, "ms")
    } else if let Some(amount) = value.strip_suffix('s') {
        (amount, "s")
    } else if let Some(amount) = value.strip_suffix('m') {
        (amount, "m")
    } else if let Some(amount) = value.strip_suffix('h') {
        (amount, "h")
    } else {
        return Err(AppFailure(format!(
            "invalid diagnostic duration `{value}`; expected a positive integer followed by ms, s, m, or h"
        ))
        .into());
    };
    let amount = amount.parse::<u64>().map_err(|_| {
        AppFailure(format!(
            "invalid diagnostic duration `{value}`; expected a positive integer followed by ms, s, m, or h"
        ))
    })?;
    if amount == 0 {
        return Err(AppFailure("diagnostic duration must be nonzero".into()).into());
    }
    let duration = match unit {
        "ms" => Duration::from_millis(amount),
        "s" => Duration::from_secs(amount),
        "m" => Duration::from_secs(
            amount
                .checked_mul(60)
                .ok_or_else(|| AppFailure("diagnostic duration overflow".into()))?,
        ),
        "h" => Duration::from_secs(
            amount
                .checked_mul(3_600)
                .ok_or_else(|| AppFailure("diagnostic duration overflow".into()))?,
        ),
        _ => unreachable!("duration units are matched above"),
    };
    if duration > Duration::from_hours(24) {
        return Err(AppFailure("diagnostic duration cannot exceed 24h".into()).into());
    }
    Ok(duration)
}

fn reject_extra(mut arguments: impl Iterator<Item = String>) -> AppResult<()> {
    if let Some(argument) = arguments.next() {
        Err(AppFailure(format!("unexpected argument `{argument}`")).into())
    } else {
        Ok(())
    }
}

fn run(command: Command) -> AppResult<()> {
    match command {
        Command::Serve {
            project,
            listen,
            once,
            native_media,
            fullscreen_program,
            fullscreen_display,
            camera_helper,
            record_program,
            diagnostic_stop_after,
        } => {
            if fullscreen_program {
                run_fullscreen_program(
                    project,
                    listen,
                    once,
                    fullscreen_display,
                    camera_helper,
                    record_program,
                )
            } else {
                serve(
                    &project,
                    listen,
                    once,
                    native_media,
                    camera_helper,
                    record_program,
                    diagnostic_stop_after,
                )
            }
        }
        Command::Help => {
            print_help();
            Ok(())
        }
        Command::Version => {
            println!("freemixd {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

#[cfg(all(feature = "macos-program-surface", target_os = "macos"))]
fn run_fullscreen_program(
    project: PathBuf,
    listen: SocketAddr,
    once: bool,
    fullscreen_display: usize,
    camera_helper: Option<PathBuf>,
    record_program: Option<PathBuf>,
) -> AppResult<()> {
    program_surface::run(
        project,
        listen,
        once,
        fullscreen_display,
        camera_helper,
        record_program,
    )
}

#[cfg(not(all(feature = "macos-program-surface", target_os = "macos")))]
fn run_fullscreen_program(
    _project: PathBuf,
    _listen: SocketAddr,
    _once: bool,
    _fullscreen_display: usize,
    _camera_helper: Option<PathBuf>,
    _record_program: Option<PathBuf>,
) -> AppResult<()> {
    Err(AppFailure(
        "--fullscreen-program requires a macOS build with feature `macos-program-surface`".into(),
    )
    .into())
}

enum NativeServeMode {
    Disabled,
    Headless {
        camera_helper: Option<PathBuf>,
    },
    #[cfg(all(feature = "macos-program-surface", target_os = "macos"))]
    Program(Box<ProgramServeSetup>),
}

#[cfg(all(feature = "macos-program-surface", target_os = "macos"))]
struct ProgramServeSetup {
    context: NativeContext,
    surface: NativeSurface,
    channels: program_surface::ProgramWorkerChannels,
    camera_helper: Option<PathBuf>,
}

fn serve(
    path: &Path,
    listen: SocketAddr,
    once: bool,
    native_media: bool,
    camera_helper: Option<PathBuf>,
    record_program: Option<PathBuf>,
    diagnostic_stop_after: Option<Duration>,
) -> AppResult<()> {
    serve_inner(
        path,
        listen,
        once,
        if native_media {
            NativeServeMode::Headless { camera_helper }
        } else {
            NativeServeMode::Disabled
        },
        record_program,
        diagnostic_stop_after,
    )
}

#[cfg(all(feature = "macos-program-surface", target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
fn serve_program_worker(
    path: &Path,
    listen: SocketAddr,
    once: bool,
    context: NativeContext,
    surface: NativeSurface,
    channels: program_surface::ProgramWorkerChannels,
    camera_helper: Option<PathBuf>,
    record_program: Option<PathBuf>,
) -> AppResult<()> {
    serve_inner(
        path,
        listen,
        once,
        NativeServeMode::Program(Box::new(ProgramServeSetup {
            context,
            surface,
            channels,
            camera_helper,
        })),
        record_program,
        None,
    )
}

#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
fn serve_inner(
    path: &Path,
    listen: SocketAddr,
    once: bool,
    mode: NativeServeMode,
    record_program: Option<PathBuf>,
    diagnostic_stop_after: Option<Duration>,
) -> AppResult<()> {
    if !listen.ip().is_loopback() {
        return Err(AppFailure(format!(
            "development mode requires a loopback listen address, got {}",
            listen.ip()
        ))
        .into());
    }

    let store = ProjectStore::new(path)?;
    let project = load_and_recover(&store)?;
    let project_id = project.project().id();
    let engine = restore_engine(&project)?;
    let identity = format!("project-{project_id}");
    let control = Rc::new(RefCell::new(ControlService::new(
        engine,
        Policy::development(),
        identity.clone(),
        format!("{identity}-log"),
        ControlLimits::default(),
    )));
    #[cfg(all(feature = "macos-program-surface", target_os = "macos"))]
    let fullscreen_active = matches!(&mode, NativeServeMode::Program(_));
    let mut native = match mode {
        NativeServeMode::Disabled => None,
        NativeServeMode::Headless { camera_helper } => Some(NativeDaemon::start(
            &store,
            &project,
            camera_helper.as_deref(),
        )?),
        #[cfg(all(feature = "macos-program-surface", target_os = "macos"))]
        NativeServeMode::Program(setup) => {
            let ProgramServeSetup {
                context,
                surface,
                channels,
                camera_helper,
            } = *setup;
            Some(NativeDaemon::start_with_program(
                &store,
                &project,
                context,
                surface,
                channels,
                camera_helper.as_deref(),
            )?)
        }
    };
    let mut process_shutdown = Some(register_process_shutdown()?);
    let authority = control_server_identity(&control.borrow(), project_id);
    let mut durable = project;
    if let Some(path) = record_program {
        native
            .as_mut()
            .ok_or_else(|| AppFailure("--record-program requires --native-media".into()))?
            .start_recorder(&durable, &path)?;
        let primed = native
            .as_mut()
            .expect("recording requires native state")
            .prime_recorder(
                &mut control.borrow_mut(),
                &authority,
                process_shutdown
                    .as_ref()
                    .expect("native state has a process shutdown signal"),
            );
        match primed {
            Ok(true) => {}
            Ok(false) => {
                checkpoint_native(&control, &store, &mut durable)?;
                native
                    .as_mut()
                    .expect("recording requires native state")
                    .finalize_recorder()?;
                return Ok(());
            }
            Err(error) => {
                let _ = native
                    .as_mut()
                    .expect("recording requires native state")
                    .finalize_recorder();
                return Err(error);
            }
        }
    }
    #[cfg(not(all(feature = "macos-program-surface", target_os = "macos")))]
    let fullscreen_active = false;
    // The immutable handshake digest advertises successfully configured
    // startup support. Late recorder health is reported by FREEMIXD_RECORDER.
    let capabilities_digest = capabilities_digest(
        native.is_some(),
        fullscreen_active,
        native.as_ref().is_some_and(NativeDaemon::recorder_active),
    );
    let config = ServerConfig::new(
        ServerMode::Development,
        AuthenticationMode::Development,
        listen.ip(),
        capabilities_digest,
    );
    let mut server = Server::new(config, ControlHandle(Rc::clone(&control)))?;
    server.mark_ready()?;

    let principal = development_principal()?;
    let listener = TcpListener::bind(listen)?;
    listener.set_nonblocking(true)?;
    let readiness = ReadinessRecord {
        address: listener.local_addr()?,
        project_id,
    };
    println!("{readiness}");
    std::io::stdout().flush()?;
    if let Some(duration) = diagnostic_stop_after {
        process_shutdown
            .as_mut()
            .expect("diagnostic shutdown requires native state")
            .set_diagnostic_deadline(duration)?;
    }

    let mut once_client_outcome = OnceClientOutcome::Unserved;
    let _shutdown_reason = loop {
        if let Some(reason) = requested_daemon_shutdown(native.as_ref(), process_shutdown.as_ref())
        {
            break reason;
        }
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if let Some(native) = &mut native {
                    native.tick_if_due(&mut control.borrow_mut(), &authority)?;
                }
                thread::sleep(NATIVE_IO_POLL_INTERVAL);
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let result = handle_client(
            stream,
            &server,
            &control,
            &store,
            &mut durable,
            &principal,
            &authority,
            native.as_mut(),
            process_shutdown.as_ref(),
            &mut once_client_outcome,
        );
        if let Err(error) = result {
            if let Some(reason) =
                requested_daemon_shutdown(native.as_ref(), process_shutdown.as_ref())
            {
                break reason;
            }
            if !is_client_disconnect(error.as_ref())
                && !is_client_protocol_error(error.as_ref())
                && !is_client_session_termination(error.as_ref())
            {
                return Err(error);
            }
        }
        if let Some(reason) = requested_daemon_shutdown(native.as_ref(), process_shutdown.as_ref())
        {
            break reason;
        }
        if once && once_client_outcome == OnceClientOutcome::HandshakeResponseWritten {
            break DaemonShutdownReason::Once;
        }
    };
    if native.is_some() {
        checkpoint_native(&control, &store, &mut durable)?;
    }
    #[cfg(all(feature = "macos-program-surface", target_os = "macos"))]
    if fullscreen_active {
        let frames_presented = native
            .as_ref()
            .and_then(NativeDaemon::program_telemetry)
            .map(|telemetry| telemetry.frames_presented)
            .unwrap_or_default();
        eprintln!("FREEMIXD_PROGRAM\tv=1\tframes_presented={frames_presented}");
    }
    if let Some(native) = &mut native {
        let recorder_result = native.finalize_recorder();
        native.emit_camera_source_telemetry();
        let camera_result = native.finalize_cameras();
        native.emit_telemetry();
        recorder_result?;
        camera_result?;
    }
    Ok(())
}

fn checkpoint_native(
    control: &SharedControl,
    store: &ProjectStore,
    durable: &mut StoredProject,
) -> AppResult<()> {
    let snapshot = control.borrow().idle_engine_snapshot()?;
    *durable = stored_project_checkpoint(durable, &snapshot)?;
    store.save(durable)?;
    Ok(())
}

fn load_and_recover(store: &ProjectStore) -> AppResult<StoredProject> {
    let project = store.load()?;
    if store.journal_path().try_exists()? {
        let scan = store.recover_journal()?;
        if !scan.batches().is_empty() {
            return Err(AppFailure(
                "project has unapplied journal batches that freemixd cannot safely interpret"
                    .into(),
            )
            .into());
        }
    }
    Ok(project)
}

fn restore_engine(project: &StoredProject) -> AppResult<Engine> {
    let canonical = project.project();
    let inputs = canonical
        .inputs()
        .iter()
        .map(|input| (input.id, input.name.clone()))
        .collect::<Vec<_>>();
    let input_ids = inputs.iter().map(|(input, _)| *input).collect::<Vec<_>>();
    let main_mix = canonical
        .main_mix()
        .ok_or_else(|| AppFailure("project is missing desired main mix routing".into()))?;
    let routing = project.runtime_routing();
    let realized_program = required_routing(routing.realized_program_id, "realized program")?;
    let realized_preview = required_routing(routing.realized_preview_id, "realized preview")?;
    let mut show = ShowState::new(
        canonical.name(),
        inputs,
        main_mix.desired_program,
        main_mix.desired_preview,
    )?;
    restore_input_audio_strips(&mut show, canonical)?;
    let mut realized = SwitcherState::new(input_ids, realized_program, realized_preview)?;
    for config in canonical.stingers() {
        restore_stinger(&mut show, &mut realized, *config)?;
    }
    let manual = project.runtime_manual_transitions();
    if let Some(state) = manual.desired {
        show.restore_manual_transition(restored_t_bar(state)?)?;
    }
    if let Some(state) = manual.realized {
        realized.restore_t_bar(restored_t_bar(state)?)?;
    }
    let fade_to_black = project.runtime_fade_to_black();
    show.restore_fade_to_black(fade_to_black.desired.target_active);
    realized.restore_settled_fade_to_black(fade_to_black.realized.target_active);
    restore_overlays(&mut show, &mut realized, project.runtime_overlays())?;
    let position = project.position();
    Ok(Engine::restore_persisted(
        show,
        realized,
        canonical.settings().frame_rate,
        clock_domain(),
        EngineRestoreState {
            state_epoch: StateEpoch::new(position.state_epoch),
            revision: Revision::new(position.revision),
            event_sequence: EventSequence::new(position.event_sequence),
            runtime_generation: RuntimeGeneration::new(position.runtime_generation),
            clock_time: ClockTime::from_nanos(position.clock_time_nanos),
            frame_cursor: position.frames_rendered.into(),
            receipts: project
                .idempotency_receipts()
                .iter()
                .map(runtime_receipt)
                .collect::<AppResult<Vec<_>>>()?,
        },
    )?)
}

fn restore_stinger(
    show: &mut ShowState,
    realized: &mut SwitcherState,
    config: StingerConfig,
) -> AppResult<()> {
    let slot = StingerSlotId::new(config.slot.number())
        .expect("validated model Stinger slots are in the switcher range");
    let descriptor = StingerDescriptor::new(
        config.media_input,
        config.preload,
        config.cut_point_frames,
        match config.audio_policy {
            ModelStingerAudioPolicy::Muted => StingerAudioPolicy::Muted,
            ModelStingerAudioPolicy::StingerOnly => StingerAudioPolicy::StingerOnly,
            ModelStingerAudioPolicy::MixWithProgram => StingerAudioPolicy::MixWithProgram,
        },
        match config.missing_media_fallback {
            StingerMissingMediaFallback::Cut => MissingMediaFallback::Cut,
            StingerMissingMediaFallback::Fade => MissingMediaFallback::Fade,
            StingerMissingMediaFallback::KeepProgram => MissingMediaFallback::KeepProgram,
        },
    );
    show.configure_stinger(slot, descriptor)?;
    realized.configure_stinger(slot, descriptor)?;

    if config.preload {
        // Native startup resolves every requested source before the first tick.
        // Until live source-health events are projected into the engine, a
        // successfully restored input is the deterministic ready state.
        let _ = show.preload_stinger(slot, true)?;
        let _ = realized.preload_stinger(slot, true)?;
    }
    Ok(())
}

fn restore_input_audio_strips(show: &mut ShowState, project: &fm_model::Project) -> AppResult<()> {
    for strip in project.input_audio_strips() {
        show.set_input_audio_strip(
            strip.input,
            EngineInputAudioStripState {
                gain_millidb: strip.state.gain.get(),
                balance_basis_points: strip.state.balance.get(),
                muted: strip.state.muted,
                soloed: strip.state.soloed,
                follow_video: strip.state.follow_video,
                delay_samples: strip.state.delay_samples.get(),
            },
        )?;
    }
    Ok(())
}

fn model_audio_strip_state(state: EngineInputAudioStripState) -> InputAudioStripState {
    InputAudioStripState {
        gain: InputGainMilliDb::new(state.gain_millidb)
            .expect("engine input audio gain is bounded by the model contract"),
        balance: InputBalanceBasisPoints::new(state.balance_basis_points)
            .expect("engine input audio balance is bounded by the model contract"),
        muted: state.muted,
        soloed: state.soloed,
        follow_video: state.follow_video,
        delay_samples: InputDelaySamples::new(state.delay_samples)
            .expect("engine input audio delay is bounded by the model contract"),
    }
}

fn configure_client_socket(stream: &TcpStream) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(CLIENT_READ_POLL_INTERVAL))?;
    stream.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn handle_client(
    stream: TcpStream,
    server: &Server<ControlHandle>,
    control: &SharedControl,
    store: &ProjectStore,
    durable: &mut StoredProject,
    principal: &Principal,
    authority: &ServerIdentity,
    native: Option<&mut NativeDaemon>,
    process_shutdown: Option<&ProcessShutdown>,
    once_client_outcome: &mut OnceClientOutcome,
) -> AppResult<()> {
    let handshake_deadline = Instant::now()
        .checked_add(Duration::from_millis(
            server.config().session_limits.heartbeat_timeout_ms,
        ))
        .ok_or_else(|| AppFailure("handshake deadline exceeds Instant range".into()))?;
    configure_client_socket(&stream)?;
    let mut writer = stream.try_clone()?;
    let mut reader = MessageReader::new(stream);
    let mut native = native;
    let first = match read_client_message(
        &mut reader,
        control,
        authority,
        &mut native,
        process_shutdown,
        || Ok(Instant::now() < handshake_deadline),
    )? {
        ClientRead::Message(message) => message,
        ClientRead::Closed | ClientRead::DaemonShutdown(_) => return Ok(()),
    };
    let (hello, handshake_outcome) = if let WireMessage::HandshakeRequest(request) = first {
        current_handshake(&request, control, durable.project().id())
    } else {
        write_error(
            &mut writer,
            "handshake_required",
            "first message must be handshake_request",
        )?;
        return Ok(());
    };

    let handshake = match server.handshake(&hello, principal, now_millis()?) {
        Ok(handshake) => handshake,
        Err(error) => {
            write_message(
                &mut writer,
                &WireMessage::HandshakeResponse(rejected_handshake_response(
                    server,
                    control,
                    durable.project().id(),
                    &hello,
                    handshake_code(&error),
                    &error.to_string(),
                )),
            )?;
            *once_client_outcome = OnceClientOutcome::HandshakeResponseWritten;
            return Ok(());
        }
    };
    let mut session = handshake.session;
    let session_identity = server_identity(&handshake.server_hello, durable.project().id());
    let response = WireMessage::HandshakeResponse(handshake_response(
        &handshake.server_hello,
        session_identity.clone(),
        reconciled_handshake_outcome(handshake_outcome, &handshake.sync),
    ));
    write_session_message(&mut writer, &mut session, &response)?;
    *once_client_outcome = OnceClientOutcome::HandshakeResponseWritten;
    match handshake.sync {
        SyncPayload::Snapshot(snapshot) => {
            write_session_message(&mut writer, &mut session, &WireMessage::Snapshot(*snapshot))?;
        }
        SyncPayload::Resume(events) => {
            for event in events {
                write_session_message(&mut writer, &mut session, &WireMessage::Event(event))?;
            }
        }
    }

    loop {
        let message = match read_client_message(
            &mut reader,
            control,
            authority,
            &mut native,
            process_shutdown,
            || session_heartbeat_active(&mut session),
        )? {
            ClientRead::Message(message) => message,
            ClientRead::DaemonShutdown(DaemonShutdownReason::ProcessSignal) => {
                let _ = write_session_message(&mut writer, &mut session, &shutdown_message());
                session.disconnect(DisconnectReason::ServerShutdown);
                break;
            }
            ClientRead::Closed | ClientRead::DaemonShutdown(_) => break,
        };
        match message {
            WireMessage::Command(command) => process_command(
                &mut writer,
                &mut session,
                control,
                store,
                durable,
                principal,
                &session_identity,
                &command,
                native.as_deref_mut(),
                process_shutdown,
            )?,
            WireMessage::Heartbeat(heartbeat) => {
                match record_heartbeat(&mut session, control, &session_identity, &heartbeat) {
                    Ok(received_at_ms) => write_session_message(
                        &mut writer,
                        &mut session,
                        &WireMessage::HeartbeatAcknowledgement(HeartbeatAcknowledgementMessage {
                            server: session_identity.clone(),
                            heartbeat_sequence: heartbeat.sequence,
                            received_at_ms,
                        }),
                    )?,
                    Err(message) => write_session_error(
                        &mut writer,
                        &mut session,
                        "invalid_heartbeat",
                        &message,
                    )?,
                }
            }
            _ => write_session_error(
                &mut writer,
                &mut session,
                "unexpected_message",
                "only command and heartbeat messages are accepted after the handshake",
            )?,
        }
    }
    Ok(())
}

enum ClientRead {
    Message(WireMessage),
    Closed,
    DaemonShutdown(DaemonShutdownReason),
}

fn read_client_message(
    reader: &mut MessageReader,
    control: &SharedControl,
    server: &ServerIdentity,
    native: &mut Option<&mut NativeDaemon>,
    process_shutdown: Option<&ProcessShutdown>,
    mut idle: impl FnMut() -> AppResult<bool>,
) -> AppResult<ClientRead> {
    if let Some(reason) = requested_daemon_shutdown(native.as_deref(), process_shutdown) {
        return Ok(ClientRead::DaemonShutdown(reason));
    }
    if !idle()? {
        return Ok(ClientRead::Closed);
    }
    let message = reader.read_message_with_idle(|| {
        if requested_daemon_shutdown(native.as_deref(), process_shutdown).is_some() || !idle()? {
            return Ok(false);
        }
        if let Some(native) = native.as_deref_mut() {
            native.tick_if_due(&mut control.borrow_mut(), server)?;
        }
        Ok(true)
    })?;
    if let Some(reason) = requested_daemon_shutdown(native.as_deref(), process_shutdown) {
        Ok(ClientRead::DaemonShutdown(reason))
    } else if message.is_some() && !idle()? {
        Ok(ClientRead::Closed)
    } else {
        Ok(message.map_or(ClientRead::Closed, ClientRead::Message))
    }
}

fn session_heartbeat_active(session: &mut Session) -> AppResult<bool> {
    match session.check_heartbeat(now_millis()?) {
        Ok(()) => Ok(true),
        Err(SessionError::Disconnected(DisconnectReason::HeartbeatTimeout)) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn reconciled_handshake_outcome(
    outcome: ProtocolHandshakeOutcome,
    sync: &SyncPayload,
) -> ProtocolHandshakeOutcome {
    if matches!(&outcome, ProtocolHandshakeOutcome::Resume { .. })
        && matches!(sync, SyncPayload::Snapshot(_))
    {
        ProtocolHandshakeOutcome::Snapshot {
            reason: fm_protocol::SnapshotReason::HistoryUnavailable,
        }
    } else {
        outcome
    }
}

fn current_handshake(
    request: &HandshakeRequest,
    control: &SharedControl,
    project_id: ProjectId,
) -> (HandshakeRequest, ProtocolHandshakeOutcome) {
    let diagnostics = control.borrow().diagnostics();
    let server = ServerIdentity {
        engine_id: diagnostics.engine.engine_id.clone(),
        project_id: project_id.to_string(),
        state_epoch: diagnostics.engine.state_epoch,
        log_id: diagnostics.engine.log_id.clone(),
    };
    let available_from_revision = diagnostics
        .oldest_retained_revision
        .unwrap_or_else(|| diagnostics.current_revision.saturating_add(1));
    let outcome = choose_handshake_outcome(
        &server,
        diagnostics.current_revision,
        available_from_revision,
        request.resume_cursor.as_ref(),
    );
    let mut reconciled = request.clone();
    if !matches!(outcome, ProtocolHandshakeOutcome::Resume { .. }) {
        reconciled.resume_cursor = None;
    }
    (reconciled, outcome)
}

fn handshake_response(
    hello: &ServerHello,
    server: ServerIdentity,
    outcome: ProtocolHandshakeOutcome,
) -> HandshakeResponse {
    HandshakeResponse {
        protocol: hello.protocol,
        granted_role: hello.granted_role,
        permissions: hello.permissions.clone(),
        capabilities: CapabilityReportSummary {
            digest: hello.capabilities_digest.clone(),
            total: 0,
            available: 0,
            degraded: 0,
            unavailable: 0,
        },
        server,
        current_revision: hello.current_revision,
        outcome,
    }
}

fn rejected_handshake_response(
    server: &Server<ControlHandle>,
    control: &SharedControl,
    project_id: ProjectId,
    hello: &HandshakeRequest,
    code: &str,
    message: &str,
) -> HandshakeResponse {
    let diagnostics = control.borrow().diagnostics();
    HandshakeResponse {
        protocol: PROTOCOL_VERSION,
        granted_role: hello.desired_role,
        permissions: Vec::new(),
        capabilities: CapabilityReportSummary {
            digest: server.config().capabilities_digest.clone(),
            total: 0,
            available: 0,
            degraded: 0,
            unavailable: 0,
        },
        server: ServerIdentity {
            engine_id: diagnostics.engine.engine_id,
            project_id: project_id.to_string(),
            state_epoch: diagnostics.engine.state_epoch,
            log_id: diagnostics.engine.log_id,
        },
        current_revision: diagnostics.current_revision,
        outcome: ProtocolHandshakeOutcome::Rejected {
            error: StructuredError {
                code: code.into(),
                message: message.into(),
                fields: Vec::new(),
                retryable: false,
            },
        },
    }
}

fn server_identity(hello: &ServerHello, project_id: ProjectId) -> ServerIdentity {
    ServerIdentity {
        engine_id: hello.engine.engine_id.clone(),
        project_id: project_id.to_string(),
        state_epoch: hello.engine.state_epoch,
        log_id: hello.engine.log_id.clone(),
    }
}

fn control_server_identity(
    control: &ControlService<Policy>,
    project_id: ProjectId,
) -> ServerIdentity {
    let engine = control.diagnostics().engine;
    ServerIdentity {
        engine_id: engine.engine_id,
        project_id: project_id.to_string(),
        state_epoch: engine.state_epoch,
        log_id: engine.log_id,
    }
}

fn event_cursor(cursor: &ResumeCursor) -> EventCursor {
    EventCursor {
        engine: fm_protocol::EngineIdentity {
            engine_id: cursor.server.engine_id.clone(),
            state_epoch: cursor.server.state_epoch,
            log_id: cursor.server.log_id.clone(),
        },
        revision: cursor.revision,
    }
}

fn record_heartbeat(
    session: &mut Session,
    control: &SharedControl,
    server: &ServerIdentity,
    heartbeat: &HeartbeatMessage,
) -> Result<u64, String> {
    if heartbeat.server != *server {
        return Err("heartbeat server identity does not match the session".into());
    }
    let last_applied = if let Some(cursor) = &heartbeat.last_applied {
        if cursor.server != *server {
            return Err("heartbeat cursor identity does not match the session".into());
        }
        if cursor.revision > control.borrow().diagnostics().current_revision {
            return Err("heartbeat cursor is ahead of the server revision".into());
        }
        Some(event_cursor(cursor))
    } else {
        None
    };
    let received_at_ms = now_millis().map_err(|error| error.to_string())?;
    session
        .record_heartbeat(
            Heartbeat {
                last_applied,
                clock_sample_ms: heartbeat.sent_at_ms,
            },
            received_at_ms,
        )
        .map_err(|error| error.to_string())?;
    Ok(received_at_ms)
}

#[allow(clippy::too_many_arguments)]
fn process_command(
    writer: &mut TcpStream,
    session: &mut Session,
    control: &SharedControl,
    store: &ProjectStore,
    durable: &mut StoredProject,
    principal: &Principal,
    server: &ServerIdentity,
    command: &CommandMessage,
    native: Option<&mut NativeDaemon>,
    process_shutdown: Option<&ProcessShutdown>,
) -> AppResult<()> {
    let delivery = execute_session_command(
        session,
        control,
        store,
        durable,
        principal,
        server,
        command,
        native,
        process_shutdown,
    )?;
    write_session_message(
        writer,
        session,
        &WireMessage::CommandResult(delivery.result),
    )?;
    for event in delivery.events {
        write_session_message(writer, session, &WireMessage::Event(event))?;
    }
    for event in delivery.runtime_events {
        write_session_message(writer, session, &WireMessage::RuntimeEvent(event))?;
    }
    Ok(())
}

struct CommandDelivery {
    result: CommandResult,
    events: Vec<EventMessage>,
    runtime_events: Vec<RuntimeEventMessage>,
}

#[allow(clippy::too_many_arguments)]
fn execute_session_command(
    session: &mut Session,
    control: &SharedControl,
    store: &ProjectStore,
    durable: &mut StoredProject,
    principal: &Principal,
    server: &ServerIdentity,
    command: &CommandMessage,
    native: Option<&mut NativeDaemon>,
    process_shutdown: Option<&ProcessShutdown>,
) -> AppResult<CommandDelivery> {
    let encoded_bytes = encode_line(&WireMessage::Command(command.clone()))?.len();
    let now = now_millis()?;
    if let Err(error) = session.admit_command(command, encoded_bytes, now) {
        let result = session_rejection(
            command,
            &error,
            control.borrow().diagnostics().current_revision,
        );
        return Ok(CommandDelivery {
            result,
            events: Vec::new(),
            runtime_events: Vec::new(),
        });
    }
    let execution = {
        let mut control = control.borrow_mut();
        if let Some(native) = native {
            native.invalidate_projection();
            if is_stinger_mutation(command) {
                match execute_native_stinger_mutation(
                    &mut control,
                    store,
                    durable,
                    principal,
                    server,
                    command,
                    now,
                    native,
                    process_shutdown,
                )? {
                    Ok(execution) => execution,
                    Err(failure) => {
                        drop(control);
                        session.command_completed()?;
                        return Ok(CommandDelivery {
                            result: failure.result,
                            events: Vec::new(),
                            runtime_events: failure.runtime_events,
                        });
                    }
                }
            } else {
                execute_durable_command_with_ticks(
                    &mut control,
                    store,
                    durable,
                    principal,
                    server,
                    command,
                    now,
                    |control, server| {
                        native
                            .wait_and_tick(control, server, process_shutdown)?
                            .map_or_else(
                                || Ok(control.tick_for_shutdown(server)?.runtime_events),
                                Ok,
                            )
                    },
                )?
            }
        } else {
            execute_durable_command(
                &mut control,
                store,
                durable,
                principal,
                server,
                command,
                now,
            )?
        }
    };
    session.command_completed()?;
    let DurableExecution {
        submission,
        runtime_events,
    } = execution;

    Ok(CommandDelivery {
        result: submission.output.result,
        events: submission.output.events,
        runtime_events,
    })
}

fn is_stinger_mutation(command: &CommandMessage) -> bool {
    matches!(
        command.payload,
        CommandPayload::ConfigureStinger { .. } | CommandPayload::RemoveStinger { .. }
    )
}

#[cfg(feature = "native-media")]
fn native_stinger_preflight_rejection(
    command: &CommandMessage,
    current_revision: u64,
) -> CommandResult {
    CommandResult::Rejected {
        id: command.id.clone(),
        code: RejectionCode::Unavailable.as_str().to_owned(),
        message: "native Stinger resources could not be prepared".to_owned(),
        fields: Vec::new(),
        current_revision,
        retryable: false,
    }
}

struct NativeMutationFailure {
    result: CommandResult,
    runtime_events: Vec<RuntimeEventMessage>,
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
#[cfg(feature = "native-media")]
fn execute_native_stinger_mutation(
    control: &mut ControlService<Policy>,
    store: &ProjectStore,
    durable: &mut StoredProject,
    principal: &Principal,
    server: &ServerIdentity,
    command: &CommandMessage,
    now_millis: u64,
    native: &mut NativeDaemon,
    process_shutdown: Option<&ProcessShutdown>,
) -> AppResult<Result<DurableExecution, NativeMutationFailure>> {
    let first_preparation = control.prepare_submit(principal, command.clone(), now_millis)?;
    let first_prepared = match first_preparation {
        PrepareSubmitOutcome::Replayed(submission) => {
            return Ok(Ok(DurableExecution {
                submission,
                runtime_events: Vec::new(),
            }));
        }
        PrepareSubmitOutcome::Prepared(prepared) => prepared,
    };
    if !first_prepared.submission().is_accepted() {
        let projected = first_prepared.project(0)?;
        let updated = stored_project_from_snapshot(
            durable,
            &projected,
            command,
            &first_prepared.output().result,
        )?;
        store.save(&updated)?;
        let submission = first_prepared.commit()?;
        *durable = updated;
        return Ok(Ok(DurableExecution {
            submission,
            runtime_events: Vec::new(),
        }));
    }

    let first_projected = first_prepared.project(1)?;
    let candidate = stored_project_from_snapshot(
        durable,
        &first_projected,
        command,
        &first_prepared.output().result,
    )?;
    drop(first_prepared);

    let (mutation, mut preflight_runtime_events) = match native
        .preflight_stinger_mutation_with_ticks(
            candidate.clone(),
            control,
            server,
            process_shutdown,
        )? {
        Some((mutation, events)) => (mutation, events),
        None => (Err(()), Vec::new()),
    };
    let mutation = match mutation {
        Ok(mutation) => mutation,
        Err(()) => {
            return Ok(Err(NativeMutationFailure {
                result: native_stinger_preflight_rejection(
                    command,
                    control.diagnostics().current_revision,
                ),
                runtime_events: preflight_runtime_events,
            }));
        }
    };

    let second_preparation = control.prepare_submit(principal, command.clone(), now_millis)?;
    let second_prepared = match second_preparation {
        PrepareSubmitOutcome::Replayed(submission) => {
            native.stinger_retirements.discard(mutation)?;
            return Ok(Ok(DurableExecution {
                submission,
                runtime_events: preflight_runtime_events,
            }));
        }
        PrepareSubmitOutcome::Prepared(prepared) => prepared,
    };
    if !second_prepared.submission().is_accepted() {
        native.stinger_retirements.discard(mutation)?;
        let projected = second_prepared.project(0)?;
        let updated = stored_project_from_snapshot(
            durable,
            &projected,
            command,
            &second_prepared.output().result,
        )?;
        store.save(&updated)?;
        let submission = second_prepared.commit()?;
        *durable = updated;
        return Ok(Ok(DurableExecution {
            submission,
            runtime_events: preflight_runtime_events,
        }));
    }

    let projected = second_prepared.project(1)?;
    let updated = stored_project_from_snapshot(
        durable,
        &projected,
        command,
        &second_prepared.output().result,
    )?;
    if candidate.project().stingers() != updated.project().stingers()
        || native
            .playback
            .validate_retained_byte_limit(mutation.ordinary_video_limit)
            .is_err()
    {
        drop(second_prepared);
        native.stinger_retirements.discard(mutation)?;
        return Ok(Err(NativeMutationFailure {
            result: native_stinger_preflight_rejection(
                command,
                control.diagnostics().current_revision,
            ),
            runtime_events: preflight_runtime_events,
        }));
    }
    store.save(&updated)?;
    let submission = second_prepared.commit()?;
    native.stage_stinger_mutation(mutation);
    let runtime_events = match native.wait_and_tick(control, server, process_shutdown)? {
        Some(events) => events,
        None => control.tick_for_shutdown(server)?.runtime_events,
    };
    preflight_runtime_events.extend(runtime_events);
    *durable = updated;
    Ok(Ok(DurableExecution {
        submission,
        runtime_events: preflight_runtime_events,
    }))
}

#[allow(clippy::too_many_arguments)]
#[cfg(not(feature = "native-media"))]
fn execute_native_stinger_mutation(
    _control: &mut ControlService<Policy>,
    _store: &ProjectStore,
    _durable: &mut StoredProject,
    _principal: &Principal,
    _server: &ServerIdentity,
    _command: &CommandMessage,
    _now_millis: u64,
    _native: &mut NativeDaemon,
    _process_shutdown: Option<&ProcessShutdown>,
) -> AppResult<Result<DurableExecution, NativeMutationFailure>> {
    Err(AppFailure("native-media support was not compiled in".into()).into())
}

struct DurableExecution {
    submission: CommandSubmission,
    runtime_events: Vec<RuntimeEventMessage>,
}

#[allow(clippy::too_many_arguments)]
fn execute_durable_command(
    control: &mut ControlService<Policy>,
    store: &dyn ProjectSaver,
    durable: &mut StoredProject,
    principal: &Principal,
    server: &ServerIdentity,
    command: &CommandMessage,
    now_millis: u64,
) -> AppResult<DurableExecution> {
    execute_durable_command_with_ticks(
        control,
        store,
        durable,
        principal,
        server,
        command,
        now_millis,
        |control, server| Ok(control.tick(server)?.runtime_events),
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_durable_command_with_ticks(
    control: &mut ControlService<Policy>,
    store: &dyn ProjectSaver,
    durable: &mut StoredProject,
    principal: &Principal,
    server: &ServerIdentity,
    command: &CommandMessage,
    now_millis: u64,
    mut tick: impl FnMut(
        &mut ControlService<Policy>,
        &ServerIdentity,
    ) -> AppResult<Vec<RuntimeEventMessage>>,
) -> AppResult<DurableExecution> {
    let preparation = control.prepare_submit(principal, command.clone(), now_millis)?;
    let prepared = match preparation {
        PrepareSubmitOutcome::Replayed(submission) => {
            return Ok(DurableExecution {
                submission,
                runtime_events: Vec::new(),
            });
        }
        PrepareSubmitOutcome::Prepared(prepared) => prepared,
    };

    let ticks = if prepared.submission().is_accepted() {
        command_ticks(command, &prepared)
    } else {
        0
    };
    let projected = prepared.project(u64::from(ticks))?;
    let updated =
        stored_project_from_snapshot(durable, &projected, command, &prepared.output().result)?;
    store.save(&updated)?;

    let submission = prepared.commit()?;
    let mut runtime_events = Vec::new();
    for _ in 0..ticks {
        runtime_events.extend(tick(control, server)?);
    }
    *durable = updated;

    Ok(DurableExecution {
        submission,
        runtime_events,
    })
}

fn command_ticks(
    command: &CommandMessage,
    prepared: &fm_control::PreparedSubmission<'_, Policy>,
) -> u32 {
    match command.payload {
        CommandPayload::Fade { duration_frames }
        | CommandPayload::AlphaFade { duration_frames }
        | CommandPayload::Slide { duration_frames }
        | CommandPayload::Zoom { duration_frames }
        | CommandPayload::Wipe { duration_frames }
        | CommandPayload::FadeToBlack {
            duration_frames, ..
        } => duration_frames,
        CommandPayload::Stinger {
            duration_frames, ..
        } => {
            if prepared.project(1).is_ok() {
                1
            } else {
                duration_frames
            }
        }
        CommandPayload::TakeOverlay { channel, .. }
        | CommandPayload::OverlayOff { channel }
        | CommandPayload::TakeNextOverlay { channel } => {
            let channel =
                OverlayChannelId::new(channel.number()).expect("wire overlay channels are bounded");
            prepared.overlay_transition_ticks(channel)
        }
        CommandPayload::SetInputAudioStrip { .. }
        | CommandPayload::SelectPreview { .. }
        | CommandPayload::Cut
        | CommandPayload::ConfigureStinger { .. }
        | CommandPayload::RemoveStinger { .. }
        | CommandPayload::UpdateOverlay { .. }
        | CommandPayload::SetOverlayOutputInclusion { .. }
        | CommandPayload::ConfigureOverlayTransition { .. }
        | CommandPayload::ConfigureOverlayAppearance { .. }
        | CommandPayload::QueueOverlay { .. }
        | CommandPayload::StartManualTransition { .. }
        | CommandPayload::SetManualTransitionPosition { .. }
        | CommandPayload::CommitManualTransition
        | CommandPayload::CancelManualTransition => 1,
    }
}

fn stored_project_from_snapshot(
    durable: &StoredProject,
    snapshot: &EngineSnapshot,
    command: &CommandMessage,
    result: &CommandResult,
) -> AppResult<StoredProject> {
    let mut receipts = snapshot
        .receipts()
        .iter()
        .map(persisted_engine_receipt)
        .collect::<Vec<_>>();

    if !receipts
        .iter()
        .any(|receipt| receipt.key() == command.idempotency_key)
    {
        if !matches!(result, CommandResult::Rejected { code, .. } if code == "permission_denied") {
            return Err(AppFailure(
                "projected engine snapshot is missing the staged command receipt".into(),
            )
            .into());
        }
        receipts.push(persisted_result(command, result)?);
    }

    stored_project_with_receipts(durable, snapshot, receipts)
}

fn stored_project_checkpoint(
    durable: &StoredProject,
    snapshot: &EngineSnapshot,
) -> AppResult<StoredProject> {
    stored_project_with_receipts(
        durable,
        snapshot,
        snapshot
            .receipts()
            .iter()
            .map(persisted_engine_receipt)
            .collect(),
    )
}

fn stored_project_with_receipts(
    durable: &StoredProject,
    snapshot: &EngineSnapshot,
    receipts: Vec<IdempotencyReceipt>,
) -> AppResult<StoredProject> {
    let show = snapshot.show();
    let desired = show.desired_switcher();
    let realized = snapshot.realized_switcher();
    let mut project = durable.project().clone();
    project.set_main_mix(MainMix::new(desired.program(), desired.preview()));
    for (&input, &state) in show.input_audio_strips() {
        if !project.set_input_audio_strip(input, model_audio_strip_state(state)) {
            return Err(
                AppFailure(format!("project is missing audio strip for input {input}")).into(),
            );
        }
    }
    sync_project_stingers(&mut project, desired, realized)?;
    StoredProject::from_project_with_complete_runtime_state(
        project,
        RuntimeRouting {
            desired_program_id: Some(desired.program()),
            realized_program_id: Some(realized.program()),
            desired_preview_id: Some(desired.preview()),
            realized_preview_id: Some(realized.preview()),
        },
        RuntimeManualTransitions {
            desired: desired.t_bar().map(persisted_t_bar),
            realized: realized.t_bar().map(persisted_t_bar),
        },
        RuntimeFadeToBlack {
            desired: persisted_fade_to_black(snapshot.desired_fade_to_black()),
            realized: persisted_fade_to_black(snapshot.realized_fade_to_black()),
        },
        persisted_overlays(desired, realized),
        ProjectPosition {
            revision: snapshot.revision().get(),
            state_epoch: snapshot.state_epoch().get(),
            event_sequence: snapshot.event_sequence().get(),
            frames_rendered: snapshot.frames_rendered(),
            runtime_generation: snapshot.runtime_generation().get(),
            clock_time_nanos: snapshot.clock_time().as_nanos(),
        },
        receipts,
    )
    .map_err(Into::into)
}

fn persisted_overlays(desired: &SwitcherState, realized: &SwitcherState) -> RuntimeOverlays {
    RuntimeOverlays {
        desired: std::array::from_fn(|index| persisted_overlay(&desired.overlays()[index])),
        realized: std::array::from_fn(|index| persisted_overlay(&realized.overlays()[index])),
    }
}

fn persisted_overlay(channel: &OverlayChannelState) -> RuntimeOverlayChannel {
    RuntimeOverlayChannel {
        source: channel.source(),
        active: channel.is_active(),
        transition: match channel.transition() {
            OverlayTransitionKind::Cut => RuntimeOverlayTransition::Cut,
            OverlayTransitionKind::Fade => RuntimeOverlayTransition::Fade,
        },
        duration_frames: channel.duration_frames(),
        position: persisted_overlay_position(channel.position()),
        border: persisted_overlay_border(channel.border()),
        queued_sources: channel.queued_sources().to_vec(),
        included_outputs: channel.included_outputs().to_vec(),
    }
}

fn persisted_overlay_position(position: OverlayPositionPreset) -> RuntimeOverlayPosition {
    match position {
        OverlayPositionPreset::FullFrame => RuntimeOverlayPosition::FullFrame,
        OverlayPositionPreset::TopLeft => RuntimeOverlayPosition::TopLeft,
        OverlayPositionPreset::TopRight => RuntimeOverlayPosition::TopRight,
        OverlayPositionPreset::BottomLeft => RuntimeOverlayPosition::BottomLeft,
        OverlayPositionPreset::BottomRight => RuntimeOverlayPosition::BottomRight,
    }
}

fn restored_overlay_position(position: RuntimeOverlayPosition) -> OverlayPositionPreset {
    match position {
        RuntimeOverlayPosition::FullFrame => OverlayPositionPreset::FullFrame,
        RuntimeOverlayPosition::TopLeft => OverlayPositionPreset::TopLeft,
        RuntimeOverlayPosition::TopRight => OverlayPositionPreset::TopRight,
        RuntimeOverlayPosition::BottomLeft => OverlayPositionPreset::BottomLeft,
        RuntimeOverlayPosition::BottomRight => OverlayPositionPreset::BottomRight,
    }
}

fn persisted_overlay_border(border: OverlayBorderPreset) -> RuntimeOverlayBorder {
    match border {
        OverlayBorderPreset::None => RuntimeOverlayBorder::None,
        OverlayBorderPreset::ThinWhite => RuntimeOverlayBorder::ThinWhite,
        OverlayBorderPreset::ThickWhite => RuntimeOverlayBorder::ThickWhite,
    }
}

fn restored_overlay_border(border: RuntimeOverlayBorder) -> OverlayBorderPreset {
    match border {
        RuntimeOverlayBorder::None => OverlayBorderPreset::None,
        RuntimeOverlayBorder::ThinWhite => OverlayBorderPreset::ThinWhite,
        RuntimeOverlayBorder::ThickWhite => OverlayBorderPreset::ThickWhite,
    }
}

fn restore_overlays(
    show: &mut ShowState,
    realized: &mut SwitcherState,
    overlays: &RuntimeOverlays,
) -> AppResult<()> {
    for (index, (desired, realized_state)) in
        overlays.desired.iter().zip(&overlays.realized).enumerate()
    {
        let channel = OverlayChannelId::from_index(index).expect("overlay index is bounded");
        restore_overlay(show.desired_switcher_mut(), channel, desired)?;
        restore_overlay(realized, channel, realized_state)?;
    }
    Ok(())
}

fn restore_overlay(
    switcher: &mut SwitcherState,
    channel: OverlayChannelId,
    state: &RuntimeOverlayChannel,
) -> AppResult<()> {
    switcher.configure_overlay_transition(
        channel,
        match state.transition {
            RuntimeOverlayTransition::Cut => OverlayTransitionKind::Cut,
            RuntimeOverlayTransition::Fade => OverlayTransitionKind::Fade,
        },
        state.duration_frames,
    )?;
    let _ = switcher.configure_overlay_appearance(
        channel,
        restored_overlay_position(state.position),
        restored_overlay_border(state.border),
    );
    if let Some(source) = state.source {
        if state.active {
            switcher.take_overlay(channel, source)?;
        } else {
            switcher.update_overlay(channel, source)?;
        }
    }
    for source in &state.queued_sources {
        switcher.queue_overlay(channel, *source)?;
    }
    for output in &state.included_outputs {
        let _ = switcher.set_overlay_output_inclusion(channel, *output, true);
    }
    Ok(())
}

fn sync_project_stingers(
    project: &mut fm_model::Project,
    desired: &SwitcherState,
    realized: &SwitcherState,
) -> AppResult<()> {
    for number in 1..=u8::try_from(StingerSlotNumber::COUNT)
        .expect("Stinger slot count fits the operator-facing number")
    {
        let model_slot = StingerSlotNumber::new(number).expect("Stinger slot number is bounded");
        let slot = StingerSlotId::new(number).expect("Stinger slot number is bounded");
        let desired_state = desired.stinger(slot);
        if desired_state != realized.stinger(slot) {
            return Err(AppFailure(format!(
                "cannot persist divergent desired and realized Stinger slot {number}"
            ))
            .into());
        }
        let _ = project.remove_stinger(model_slot);
        let Some(descriptor) = desired_state.descriptor() else {
            continue;
        };
        project.set_stinger(StingerConfig::new(
            model_slot,
            descriptor.media_input,
            descriptor.preload,
            descriptor.cut_point_frames,
            match descriptor.audio_policy {
                StingerAudioPolicy::Muted => ModelStingerAudioPolicy::Muted,
                StingerAudioPolicy::StingerOnly => ModelStingerAudioPolicy::StingerOnly,
                StingerAudioPolicy::MixWithProgram => ModelStingerAudioPolicy::MixWithProgram,
            },
            match descriptor.missing_media_fallback {
                MissingMediaFallback::Cut => StingerMissingMediaFallback::Cut,
                MissingMediaFallback::Fade => StingerMissingMediaFallback::Fade,
                MissingMediaFallback::KeepProgram => StingerMissingMediaFallback::KeepProgram,
            },
        ));
    }
    Ok(())
}

fn persisted_fade_to_black(state: fm_engine::EngineFadeToBlackState) -> PersistedFadeToBlackState {
    PersistedFadeToBlackState::new(
        state.active,
        u16::try_from(state.position.numerator())
            .expect("engine fade-to-black numerator uses the u16 contract"),
    )
}

fn persisted_t_bar(state: TBarState) -> PersistedManualTransitionState {
    let kind = match state.kind() {
        TransitionKind::Fade => PersistedManualTransitionKind::Fade,
        TransitionKind::Wipe => PersistedManualTransitionKind::Wipe,
        TransitionKind::AlphaFade => PersistedManualTransitionKind::AlphaFade,
        TransitionKind::Slide => PersistedManualTransitionKind::Slide,
        _ => unreachable!("engine manual transitions are Fade, Wipe, AlphaFade, or Slide"),
    };
    PersistedManualTransitionState::new(
        kind,
        state.from(),
        state.to(),
        state.interval_start().basis_points(),
        state.position().basis_points(),
    )
    .expect("engine manual transition positions are bounded")
}

fn restored_t_bar(state: PersistedManualTransitionState) -> AppResult<TBarState> {
    let kind = match state.kind {
        PersistedManualTransitionKind::Fade => TransitionKind::Fade,
        PersistedManualTransitionKind::Wipe => TransitionKind::Wipe,
        PersistedManualTransitionKind::AlphaFade => TransitionKind::AlphaFade,
        PersistedManualTransitionKind::Slide => TransitionKind::Slide,
    };
    let interval_start = TBarPosition::new(state.interval_start_basis_points)
        .ok_or_else(|| AppFailure("invalid persisted manual-transition interval start".into()))?;
    let position = TBarPosition::new(state.position_basis_points)
        .ok_or_else(|| AppFailure("invalid persisted manual-transition position".into()))?;
    Ok(TBarState::restore(
        kind,
        state.from_id,
        state.to_id,
        interval_start,
        position,
    ))
}

fn persisted_engine_receipt(
    (key, receipt): &(IdempotencyKey, CommandReceipt<EngineAcceptance>),
) -> IdempotencyReceipt {
    match receipt {
        CommandReceipt::Accepted {
            command_id,
            acceptance,
        } => IdempotencyReceipt::accepted(
            key.as_str(),
            command_id.as_str(),
            acceptance.revision.get(),
            acceptance.result.target_frame.get(),
        ),
        CommandReceipt::Rejected {
            command_id,
            rejection,
        } => IdempotencyReceipt::rejected(
            key.as_str(),
            command_id.as_str(),
            rejection.current_revision.get(),
            rejection.rejection.code.as_str(),
            &rejection.rejection.message,
            rejection.rejection.retryable,
        ),
    }
}

fn persisted_result(
    command: &CommandMessage,
    result: &CommandResult,
) -> AppResult<IdempotencyReceipt> {
    match result {
        CommandResult::Accepted {
            id,
            revision,
            scheduled_frame: Some(frame),
        } => Ok(IdempotencyReceipt::accepted(
            &command.idempotency_key,
            id,
            *revision,
            *frame,
        )),
        CommandResult::Accepted {
            scheduled_frame: None,
            ..
        } => Err(AppFailure("accepted engine command has no scheduled frame".into()).into()),
        CommandResult::Rejected {
            id,
            code,
            message,
            current_revision,
            retryable,
            ..
        } => Ok(IdempotencyReceipt::rejected(
            &command.idempotency_key,
            id,
            *current_revision,
            code,
            message,
            *retryable,
        )),
    }
}

fn runtime_receipt(
    receipt: &IdempotencyReceipt,
) -> AppResult<(IdempotencyKey, CommandReceipt<EngineAcceptance>)> {
    let command_id = CommandId::new(receipt.command_id());
    let runtime = match receipt.outcome() {
        ReceiptOutcome::Accepted {
            revision,
            target_frame,
        } => CommandReceipt::Accepted {
            command_id,
            acceptance: AcceptedReceipt {
                revision: Revision::new(*revision),
                result: EngineAcceptance {
                    target_frame: (*target_frame).into(),
                },
            },
        },
        ReceiptOutcome::Rejected {
            current_revision,
            code,
            message,
            retryable,
        } => CommandReceipt::Rejected {
            command_id,
            rejection: RejectedReceipt {
                rejection: Rejection::new(runtime_rejection_code(code)?, message)
                    .retryable(*retryable),
                current_revision: Revision::new(*current_revision),
            },
        },
    };
    Ok((IdempotencyKey::new(receipt.key()), runtime))
}

fn runtime_rejection_code(code: &str) -> AppResult<RejectionCode> {
    match code {
        "permission_denied" => Ok(RejectionCode::PermissionDenied),
        "deadline_exceeded" => Ok(RejectionCode::DeadlineExceeded),
        "revision_conflict" => Ok(RejectionCode::RevisionConflict),
        "invalid_command" => Ok(RejectionCode::InvalidCommand),
        "not_found" => Ok(RejectionCode::NotFound),
        "conflict" => Ok(RejectionCode::Conflict),
        "unavailable" => Ok(RejectionCode::Unavailable),
        "resource_exhausted" => Ok(RejectionCode::ResourceExhausted),
        "internal" => Ok(RejectionCode::Internal),
        _ => Err(AppFailure(format!("project contains unknown rejection code `{code}`")).into()),
    }
}

fn session_rejection(
    command: &CommandMessage,
    error: &SessionError,
    current_revision: u64,
) -> CommandResult {
    let (code, retryable) = match error {
        SessionError::Authorization(_) => ("permission_denied", false),
        SessionError::ProtocolMismatch { .. } => ("protocol_mismatch", false),
        SessionError::CommandTooLarge { .. }
        | SessionError::TooManyInflightCommands
        | SessionError::InboundRateLimited
        | SessionError::OutboundRateLimited => ("resource_exhausted", true),
        _ => ("session_error", false),
    };
    CommandResult::Rejected {
        id: command.id.clone(),
        code: code.into(),
        message: error.to_string(),
        fields: Vec::new(),
        current_revision,
        retryable,
    }
}

fn write_session_message(
    writer: &mut TcpStream,
    session: &mut Session,
    message: &WireMessage,
) -> AppResult<()> {
    let line = encode_line(message)?;
    session.queue_outbound(line.len(), now_millis()?)?;
    write_client_bytes(writer, line.as_bytes())?;
    session.outbound_delivered()?;
    Ok(())
}

fn write_session_error(
    writer: &mut TcpStream,
    session: &mut Session,
    code: &str,
    message: &str,
) -> AppResult<()> {
    write_session_message(writer, session, &error_message(code, message))
}

fn write_error(writer: &mut TcpStream, code: &str, message: &str) -> AppResult<()> {
    write_message(writer, &error_message(code, message))
}

fn error_message(code: &str, message: &str) -> WireMessage {
    WireMessage::Error(ErrorMessage {
        request_id: None,
        current_revision: None,
        error: StructuredError {
            code: code.into(),
            message: message.into(),
            fields: Vec::new(),
            retryable: false,
        },
    })
}

fn shutdown_message() -> WireMessage {
    WireMessage::Error(ErrorMessage {
        request_id: None,
        current_revision: None,
        error: StructuredError {
            code: "server_shutting_down".into(),
            message: "server is shutting down".into(),
            fields: Vec::new(),
            retryable: true,
        },
    })
}

fn write_message(writer: &mut TcpStream, message: &WireMessage) -> AppResult<()> {
    let line = encode_line(message)?;
    write_client_bytes(writer, line.as_bytes())?;
    Ok(())
}

fn write_client_bytes(writer: &mut TcpStream, bytes: &[u8]) -> AppResult<()> {
    let mut pending = PendingWrite::new(bytes.to_vec());
    while !pending.write_once(writer).map_err(client_write_error)? {}
    writer.flush().map_err(client_write_error)?;
    Ok(())
}

struct PendingWrite {
    bytes: Vec<u8>,
    offset: usize,
}

impl PendingWrite {
    fn new(bytes: Vec<u8>) -> Self {
        assert!(!bytes.is_empty(), "encoded records are nonempty");
        Self { bytes, offset: 0 }
    }

    fn write_once(&mut self, writer: &mut impl Write) -> std::io::Result<bool> {
        let written = writer.write(&self.bytes[self.offset..])?;
        if written == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        self.offset += written;
        Ok(self.offset == self.bytes.len())
    }
}

fn client_write_error(error: std::io::Error) -> Box<dyn Error> {
    if matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) {
        Box::new(ClientWriteTimeout(error.kind()))
    } else {
        Box::new(error)
    }
}

#[test]
fn pending_write_preserves_offset_across_partial_writes() {
    struct PartialWriter {
        bytes: Vec<u8>,
        limit: usize,
    }

    impl Write for PartialWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let written = bytes.len().min(self.limit);
            self.bytes.extend_from_slice(&bytes[..written]);
            Ok(written)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let expected = b"encoded record";
    let mut pending = PendingWrite::new(expected.to_vec());
    let mut writer = PartialWriter {
        bytes: Vec::new(),
        limit: 3,
    };

    assert!(!pending.write_once(&mut writer).unwrap());
    assert!(!pending.write_once(&mut writer).unwrap());
    assert!(!pending.write_once(&mut writer).unwrap());
    assert!(!pending.write_once(&mut writer).unwrap());
    assert!(pending.write_once(&mut writer).unwrap());
    assert_eq!(writer.bytes, expected);
}

fn handshake_code(error: &HandshakeError<Infallible>) -> &'static str {
    match error {
        HandshakeError::ProtocolMismatch => "protocol_mismatch",
        HandshakeError::NotReady(_) => "not_ready",
        HandshakeError::DevelopmentPrincipalDenied | HandshakeError::RoleDenied(_) => {
            "permission_denied"
        }
        HandshakeError::InvalidControlSync => "invalid_control_sync",
        HandshakeError::Control(never) => match *never {},
    }
}

struct MessageReader {
    stream: TcpStream,
    decoder: LineDecoder,
    pending: VecDeque<WireMessage>,
}

impl MessageReader {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            decoder: LineDecoder::new(),
            pending: VecDeque::new(),
        }
    }

    fn read_message_with_idle(
        &mut self,
        mut idle: impl FnMut() -> AppResult<bool>,
    ) -> AppResult<Option<WireMessage>> {
        if let Some(message) = self.pending.pop_front() {
            return Ok(Some(message));
        }
        let mut chunk = [0_u8; 8 * 1024];
        loop {
            let read = match self.stream.read(&mut chunk) {
                Ok(read) => read,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::Interrupted
                            | std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if !idle()? {
                        return Ok(None);
                    }
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            if read == 0 {
                let decoder = std::mem::replace(&mut self.decoder, LineDecoder::new());
                decoder.finish().map_err(ClientProtocolError)?;
                return Ok(None);
            }
            self.pending.extend(
                self.decoder
                    .push(&chunk[..read])
                    .map_err(ClientProtocolError)?,
            );
            if let Some(message) = self.pending.pop_front() {
                return Ok(Some(message));
            }
            if !idle()? {
                return Ok(None);
            }
        }
    }
}

fn is_client_disconnect(error: &(dyn Error + 'static)) -> bool {
    if error.downcast_ref::<ClientWriteTimeout>().is_some() {
        return true;
    }
    if let Some(error) = error.downcast_ref::<std::io::Error>() {
        return matches!(
            error.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::UnexpectedEof
        );
    }
    error.source().is_some_and(is_client_disconnect)
}

fn is_client_protocol_error(error: &(dyn Error + 'static)) -> bool {
    error.downcast_ref::<ClientProtocolError>().is_some()
}

fn is_client_session_termination(error: &(dyn Error + 'static)) -> bool {
    if let Some(error) = error.downcast_ref::<SessionError>() {
        return matches!(
            error,
            SessionError::OutboundRateLimited
                | SessionError::Disconnected(DisconnectReason::SlowClient)
        );
    }
    error.source().is_some_and(is_client_session_termination)
}

fn development_principal() -> AppResult<Principal> {
    Ok(Principal::development(
        UserId::new("local-operator")?,
        SessionId::new("local-session")?,
        [AuthRole::Admin],
    ))
}

fn clock_domain() -> ClockDomainId {
    ClockDomainId::new(NonZeroU128::new(1).expect("one is nonzero"))
}

fn required_routing(value: Option<InputId>, field: &'static str) -> AppResult<InputId> {
    value.ok_or_else(|| AppFailure(format!("project is missing {field} routing")).into())
}

fn now_millis() -> AppResult<u64> {
    let millis = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    u64::try_from(millis)
        .map_err(|_| AppFailure("system time exceeds protocol range".into()).into())
}

fn print_help() {
    println!(
        "FreeMix headless production daemon\n\n\
Usage:\n  freemixd serve <show.freemix> [--listen 127.0.0.1:0] [--once] [--native-media [--camera-helper PATH]] [--record-program output.mp4] [--diagnostic-stop-after 10m] [--fullscreen-program [--fullscreen-display 0]]\n  freemixd help\n  freemixd --version\n\n\
Native media is opt-in; without it the daemon uses simulated frame realization.\n\
--camera-helper overrides the developer AVFoundation helper path for exact macOS Device inputs; it never requests permission.\n\
Program recording requires native media, an existing output parent, and a new final .mp4 file. Existing files are never overwritten.\n\
Use --record-program=<path> when the output name begins with --. Recorder capability digests describe configured startup support; FREEMIXD_RECORDER reports runtime health.\n\
macOS fullscreen display selection is a zero-based index ordered by physical position, then stable descriptive fields.\n\
--diagnostic-stop-after schedules cooperative headless native shutdown after readiness; accepted units are ms, s, m, and h up to 24h.\n\
Native mode continues across client disconnects; close the Program window or press Escape for bounded shutdown."
    );
}

#[derive(Debug)]
struct AppFailure(String);

#[derive(Debug)]
struct ClientWriteTimeout(std::io::ErrorKind);

#[derive(Debug)]
struct ClientProtocolError(CodecError);

impl fmt::Display for ClientWriteTimeout {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "client write timed out: {:?}", self.0)
    }
}

impl Error for ClientWriteTimeout {}

impl fmt::Display for ClientProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "client protocol error: {}", self.0)
    }
}

impl Error for ClientProtocolError {}

impl fmt::Display for AppFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for AppFailure {}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        fs,
        panic::{AssertUnwindSafe, catch_unwind},
        sync::mpsc::TryRecvError,
    };

    use fm_control::{LiveEvent, Subscription};
    #[cfg(all(feature = "native-media", target_os = "macos"))]
    use fm_io_macos::{CameraIdKind, deterministic_camera_id};
    use fm_model::{
        Input, InputAudioStripState, InputBalanceBasisPoints, InputGainMilliDb, InputKind, Project,
        ProjectSettings, SimulatedAudio, SimulatedInput, SimulatedVideo, SolidColor, StingerConfig,
        StingerSlotNumber,
    };
    #[cfg(feature = "native-media")]
    use fm_model::{Rgba8 as ModelRgba8, Scene};
    use fm_protocol::{ClientType, Role};
    #[cfg(feature = "native-media")]
    use fm_types::SceneId;
    use fm_types::{
        AudioFormat, ChannelLayout, ColorMetadata, FrameRate, PixelFormat, SampleFormat,
        SampleRate, ScanMode, VideoDimensions, VideoFormat,
    };
    #[cfg(feature = "native-media")]
    use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};

    use super::*;

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    fn camera_discovery(permission: u8) -> Vec<u8> {
        camera_discovery_for(permission, &[("fake-camera", "Fake Camera")])
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    fn camera_discovery_for(permission: u8, cameras: &[(&str, &str)]) -> Vec<u8> {
        let mut bytes = b"FMCAMD2\0".to_vec();
        bytes.push(permission);
        bytes.extend_from_slice(&u32::try_from(cameras.len()).unwrap().to_le_bytes());
        for &(native_id, name) in cameras {
            for value in [native_id, name] {
                bytes.extend_from_slice(&u32::try_from(value.len()).unwrap().to_le_bytes());
                bytes.extend_from_slice(value.as_bytes());
            }
            bytes.extend_from_slice(&1_u32.to_le_bytes());
            bytes.extend_from_slice(&1_u32.to_le_bytes());
            bytes.extend_from_slice(&1_u32.to_le_bytes());
            bytes.extend_from_slice(&30_000_u32.to_le_bytes());
            bytes.extend_from_slice(&1_001_u32.to_le_bytes());
        }
        bytes
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    fn camera_frame(sequence: u64, pts_micros: i64) -> Vec<u8> {
        let mut bytes = b"FMCAMF3\0".to_vec();
        bytes.extend_from_slice(&62_u32.to_le_bytes());
        bytes.extend_from_slice(&sequence.to_le_bytes());
        bytes.extend_from_slice(&2_u64.to_le_bytes());
        bytes.extend_from_slice(&pts_micros.to_le_bytes());
        bytes.extend_from_slice(&1_000_i32.to_le_bytes());
        bytes.extend_from_slice(&1_001_i64.to_le_bytes());
        bytes.extend_from_slice(&30_000_i32.to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&4_u32.to_le_bytes());
        bytes.extend_from_slice(&4_u32.to_le_bytes());
        bytes.extend_from_slice(&[1, 2]);
        bytes.extend_from_slice(&[3, 5, 7, 255]);
        bytes
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    fn camera_slot_test_frame(sequence: u64, discontinuity: bool) -> CpuVideoFrame {
        let mut source = SimulatedVideoSource::new(
            1,
            1,
            FrameRate::new(30, 1).unwrap(),
            native_clock_domain(),
            SourcePattern::Solid(Rgba8::new(1, 2, 3, 255)),
        )
        .unwrap();
        let frame = source.next_frame().unwrap().unwrap();
        let original = frame.timing();
        let mut timing = MediaTiming::new(
            original.original_timestamp(),
            original.presentation_timestamp(),
            original.duration(),
            original.clock_domain(),
            SequenceNumber::new(sequence),
        )
        .unwrap();
        if discontinuity {
            timing = timing.with_flags(fm_frame::MediaFlags::DISCONTINUITY);
        }
        let metadata = frame.metadata();
        let frame = CpuVideoFrame::new(timing, frame.into_payload());
        if let Some(metadata) = metadata {
            frame.with_metadata(metadata).unwrap()
        } else {
            frame
        }
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    fn shell_octal(bytes: &[u8]) -> String {
        use fmt::Write as _;

        let mut output = String::new();
        for byte in bytes {
            write!(output, "\\{byte:03o}").unwrap();
        }
        output
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    fn fake_camera_helper(directory: &Path, permission: u8) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let helper = directory.join("camera-helper.sh");
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n  discover) printf '{}' ;;\n  capture) printf '%s\\n' \"$@\" > \"$0.capture\"; printf '%s' \"$$\" > \"$0.pid\"; printf '{}'; exec sleep 30 ;;\n  request-permission) touch \"$0.permission\"; exit 91 ;;\n  *) exit 90 ;;\nesac\n",
            shell_octal(&camera_discovery(permission)),
            shell_octal(&camera_frame(7, 1_000)),
        );
        fs::write(&helper, script).unwrap();
        let mut permissions = fs::metadata(&helper).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&helper, permissions).unwrap();
        helper
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    fn recovering_camera_helper(
        directory: &Path,
        recover_on_capture: Option<u32>,
        initial_exit: i32,
    ) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let helper = directory.join("recovering-camera-helper.sh");
        let recovery_case = recover_on_capture.map_or_else(String::new, |capture| {
            format!(
                "  {capture}) sleep 0.05; printf '{}'; exec sleep 30 ;;\n",
                shell_octal(&camera_frame(0, 2_000))
            )
        });
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n  discover) printf '{}' ;;\n  capture)\n    count=0\n    if test -f \"$0.count\"; then read count < \"$0.count\"; fi\n    count=$((count + 1))\n    printf '%s' \"$count\" > \"$0.count\"\n    printf '%s\\n' \"$@\" >> \"$0.capture\"\n    printf '%s\\n' \"$$\" >> \"$0.pids\"\n    case \"$count\" in\n  1) printf '{}'; sleep 0.10; exit {initial_exit} ;;\n{recovery_case}  *) printf '{}'; sleep 0.03; exit 20 ;;\n    esac ;;\n  *) exit 90 ;;\nesac\n",
            shell_octal(&camera_discovery(0)),
            shell_octal(&camera_frame(7, 1_000)),
            shell_octal(b"FMCAMF3\0"),
        );
        fs::write(&helper, script).unwrap();
        let mut permissions = fs::metadata(&helper).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&helper, permissions).unwrap();
        helper
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    fn hanging_recovery_camera_helper(directory: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let helper = directory.join("hanging-recovery-camera-helper.sh");
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n  discover) printf '{}' ;;\n  capture)\n    count=0\n    if test -f \"$0.count\"; then read count < \"$0.count\"; fi\n    count=$((count + 1))\n    printf '%s' \"$count\" > \"$0.count\"\n    printf '%s\\n' \"$$\" >> \"$0.pids\"\n    case \"$count\" in\n      1) printf '{}'; sleep 0.10; exit 20 ;;\n      *) printf '{}'; exec sleep 30 ;;\n    esac ;;\n  *) exit 90 ;;\nesac\n",
            shell_octal(&camera_discovery(0)),
            shell_octal(&camera_frame(7, 1_000)),
            shell_octal(b"FMCAMF3\0"),
        );
        fs::write(&helper, script).unwrap();
        let mut permissions = fs::metadata(&helper).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&helper, permissions).unwrap();
        helper
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    fn malformed_runtime_camera_helper(directory: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let helper = directory.join("malformed-runtime-camera-helper.sh");
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n  discover) printf '{}' ;;\n  capture)\n    count=0\n    if test -f \"$0.count\"; then read count < \"$0.count\"; fi\n    count=$((count + 1))\n    printf '%s' \"$count\" > \"$0.count\"\n    printf '%s\\n' \"$$\" >> \"$0.pids\"\n    printf '{}'; sleep 0.15; printf '\\001\\000\\000\\000'; exec sleep 30 ;;\n  *) exit 90 ;;\nesac\n",
            shell_octal(&camera_discovery(0)),
            shell_octal(&camera_frame(7, 1_000)),
        );
        fs::write(&helper, script).unwrap();
        let mut permissions = fs::metadata(&helper).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&helper, permissions).unwrap();
        helper
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    fn many_camera_startup_failure_helper(directory: &Path, count: usize) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let helper = directory.join("many-camera-startup-failure-helper.sh");
        let records = (0..count)
            .map(|index| {
                (
                    format!("fake-camera-{index}"),
                    format!("Fake Camera {index}"),
                )
            })
            .collect::<Vec<_>>();
        let borrowed = records
            .iter()
            .map(|(id, name)| (id.as_str(), name.as_str()))
            .collect::<Vec<_>>();
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n  discover) printf '{}' ;;\n  capture)\n    printf '%s\\n' \"$$\" >> \"$0.pids\"\n    case \"$2\" in\n      fake-camera-0) printf 'BADFRAME'; exec sleep 30 ;;\n      *) printf '{}'; exec sleep 30 ;;\n    esac ;;\n  *) exit 90 ;;\nesac\n",
            shell_octal(&camera_discovery_for(0, &borrowed)),
            shell_octal(&camera_frame(7, 1_000)),
        );
        fs::write(&helper, script).unwrap();
        let mut permissions = fs::metadata(&helper).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&helper, permissions).unwrap();
        helper
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    fn two_camera_helper(directory: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let helper = directory.join("two-camera-helper.sh");
        let discovery = camera_discovery_for(
            0,
            &[
                ("fake-camera-a", "Fake Camera A"),
                ("fake-camera-b", "Fake Camera B"),
            ],
        );
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n  discover) printf '{}' ;;\n  capture)\n    printf '%s\\n' \"$$\" >> \"$0.pids\"\n    printf '%s\\n' \"$@\" >> \"$0.capture\"\n    case \"$2\" in\n      fake-camera-a)\n        count=0\n        if test -f \"$0.a.count\"; then read count < \"$0.a.count\"; fi\n        count=$((count + 1))\n        printf '%s' \"$count\" > \"$0.a.count\"\n        case \"$count\" in\n          1) printf '{}'; sleep 0.10; exit 20 ;;\n          *) sleep 0.05; printf '{}'; exec sleep 30 ;;\n        esac ;;\n      fake-camera-b) printf '{}'; exec sleep 30 ;;\n      *) exit 89 ;;\n    esac ;;\n  *) exit 90 ;;\nesac\n",
            shell_octal(&discovery),
            shell_octal(&camera_frame(7, 1_000)),
            shell_octal(&camera_frame(0, 2_000)),
            shell_octal(&camera_frame(11, 1_000)),
        );
        fs::write(&helper, script).unwrap();
        let mut permissions = fs::metadata(&helper).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&helper, permissions).unwrap();
        helper
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    fn test_camera_recovery_policy(
        max_attempts: u32,
        rearm_backoff: Duration,
    ) -> CameraRecoveryPolicy {
        CameraRecoveryPolicy {
            max_attempts,
            initial_backoff: Duration::from_millis(5),
            max_backoff: Duration::from_millis(10),
            rearm_backoff,
            shutdown_timeout: Duration::from_secs(4),
        }
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    fn helper_process_exists(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    fn assert_helper_processes_reaped(path: &Path) {
        let pids = fs::read_to_string(path).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && pids.lines().any(helper_process_exists) {
            thread::sleep(Duration::from_millis(10));
        }
        for pid in pids.lines() {
            assert!(
                !helper_process_exists(pid),
                "camera helper {pid} survived cleanup"
            );
        }
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    fn camera_test_project(stable_key: &str) -> StoredProject {
        let frame_rate = FrameRate::new(30_000, 1_001).unwrap();
        let mut project = Project::new(
            ProjectId::new(NonZeroU128::new(43).unwrap()),
            "Camera Unit Test",
            ProjectSettings {
                frame_rate,
                video: VideoFormat {
                    dimensions: VideoDimensions::new(1, 1).unwrap(),
                    frame_rate,
                    pixel_format: PixelFormat::Rgba8,
                    scan: ScanMode::Progressive,
                    color: ColorMetadata::default(),
                },
                audio: AudioFormat {
                    sample_rate: SampleRate::new(48_000).unwrap(),
                    sample_format: SampleFormat::F32,
                    channels: ChannelLayout::stereo(),
                },
            },
        );
        project.add_input(Input {
            id: test_input_id(1),
            name: "Camera".into(),
            kind: InputKind::Device {
                stable_key: stable_key.into(),
            },
            required_capabilities: Vec::new(),
        });
        project.add_input(Input {
            id: test_input_id(2),
            name: "Bars".into(),
            kind: InputKind::Simulated(SimulatedInput::new(
                SimulatedVideo::Bars,
                SimulatedAudio::Silence,
            )),
            required_capabilities: Vec::new(),
        });
        project.set_main_mix(MainMix::new(test_input_id(1), test_input_id(2)));
        StoredProject::from_project(
            project,
            RuntimeRouting {
                desired_program_id: Some(test_input_id(1)),
                realized_program_id: Some(test_input_id(1)),
                desired_preview_id: Some(test_input_id(2)),
                realized_preview_id: Some(test_input_id(2)),
            },
            ProjectPosition::default(),
            Vec::new(),
        )
        .unwrap()
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    fn two_camera_test_project(first_key: &str, second_key: &str) -> StoredProject {
        let frame_rate = FrameRate::new(30_000, 1_001).unwrap();
        let mut project = Project::new(
            ProjectId::new(NonZeroU128::new(44).unwrap()),
            "Two Camera Unit Test",
            ProjectSettings {
                frame_rate,
                video: VideoFormat {
                    dimensions: VideoDimensions::new(1, 1).unwrap(),
                    frame_rate,
                    pixel_format: PixelFormat::Rgba8,
                    scan: ScanMode::Progressive,
                    color: ColorMetadata::default(),
                },
                audio: AudioFormat {
                    sample_rate: SampleRate::new(48_000).unwrap(),
                    sample_format: SampleFormat::F32,
                    channels: ChannelLayout::stereo(),
                },
            },
        );
        for (value, key) in [(1, first_key), (2, second_key)] {
            project.add_input(Input {
                id: test_input_id(value),
                name: format!("Camera {value}"),
                kind: InputKind::Device {
                    stable_key: key.to_owned(),
                },
                required_capabilities: Vec::new(),
            });
        }
        project.set_main_mix(MainMix::new(test_input_id(1), test_input_id(2)));
        StoredProject::from_project(
            project,
            RuntimeRouting {
                desired_program_id: Some(test_input_id(1)),
                realized_program_id: Some(test_input_id(1)),
                desired_preview_id: Some(test_input_id(2)),
                realized_preview_id: Some(test_input_id(2)),
            },
            ProjectPosition::default(),
            Vec::new(),
        )
        .unwrap()
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    fn many_camera_test_project(keys: &[String]) -> StoredProject {
        let frame_rate = FrameRate::new(30_000, 1_001).unwrap();
        let mut project = Project::new(
            ProjectId::new(NonZeroU128::new(45).unwrap()),
            "Many Camera Unit Test",
            ProjectSettings {
                frame_rate,
                video: VideoFormat {
                    dimensions: VideoDimensions::new(1, 1).unwrap(),
                    frame_rate,
                    pixel_format: PixelFormat::Rgba8,
                    scan: ScanMode::Progressive,
                    color: ColorMetadata::default(),
                },
                audio: AudioFormat {
                    sample_rate: SampleRate::new(48_000).unwrap(),
                    sample_format: SampleFormat::F32,
                    channels: ChannelLayout::stereo(),
                },
            },
        );
        for (index, key) in keys.iter().enumerate() {
            let value = u128::try_from(index + 1).unwrap();
            project.add_input(Input {
                id: test_input_id(value),
                name: format!("Camera {value}"),
                kind: InputKind::Device {
                    stable_key: key.clone(),
                },
                required_capabilities: Vec::new(),
            });
        }
        project.set_main_mix(MainMix::new(test_input_id(1), test_input_id(2)));
        StoredProject::from_project(
            project,
            RuntimeRouting {
                desired_program_id: Some(test_input_id(1)),
                realized_program_id: Some(test_input_id(1)),
                desired_preview_id: Some(test_input_id(2)),
                realized_preview_id: Some(test_input_id(2)),
            },
            ProjectPosition::default(),
            Vec::new(),
        )
        .unwrap()
    }

    struct FailingSaver;

    impl ProjectSaver for FailingSaver {
        fn save(&self, _project: &StoredProject) -> AppResult<()> {
            Err(AppFailure("injected save failure".into()).into())
        }
    }

    struct ObservingSaver<'a> {
        subscription: &'a Subscription,
        saved: RefCell<Option<StoredProject>>,
    }

    impl ProjectSaver for ObservingSaver<'_> {
        fn save(&self, project: &StoredProject) -> AppResult<()> {
            assert_eq!(self.subscription.try_recv(), Err(TryRecvError::Empty));
            self.saved.replace(Some(project.clone()));
            Ok(())
        }
    }

    #[derive(Default)]
    struct CountingSaver(Cell<u32>);

    impl ProjectSaver for CountingSaver {
        fn save(&self, _project: &StoredProject) -> AppResult<()> {
            self.0.set(self.0.get() + 1);
            Ok(())
        }
    }

    struct CrashAfterSave<'a>(&'a ProjectStore);

    impl ProjectSaver for CrashAfterSave<'_> {
        fn save(&self, project: &StoredProject) -> AppResult<()> {
            ProjectStore::save(self.0, project)?;
            panic!("simulated crash after save");
        }
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn parses_serve_options() {
        assert_eq!(
            parse_args(strings(&[
                "serve",
                "show.freemix",
                "--listen",
                "127.0.0.1:9123",
                "--once",
            ]))
            .unwrap(),
            Command::Serve {
                project: "show.freemix".into(),
                listen: "127.0.0.1:9123".parse().unwrap(),
                once: true,
                native_media: false,
                fullscreen_program: false,
                fullscreen_display: 0,
                camera_helper: None,
                record_program: None,
                diagnostic_stop_after: None,
            }
        );
    }

    #[test]
    fn default_serve_is_ephemeral_loopback() {
        let command = parse_args(strings(&["serve", "show.freemix"])).unwrap();
        assert!(matches!(
            command,
            Command::Serve { listen, once: false, native_media: false, .. }
                if listen == DEFAULT_LISTEN.parse::<SocketAddr>().unwrap()
        ));
    }

    #[test]
    fn parses_native_media_as_an_opt_in() {
        assert!(matches!(
            parse_args(strings(&["serve", "show.freemix", "--native-media"])).unwrap(),
            Command::Serve {
                native_media: true,
                ..
            }
        ));
    }

    #[test]
    fn rejects_duplicate_native_media_option() {
        let error = parse_args(strings(&[
            "serve",
            "show.freemix",
            "--native-media",
            "--native-media",
        ]))
        .unwrap_err();
        assert_eq!(error.to_string(), "duplicate option `--native-media`");
    }

    #[test]
    fn parses_camera_helper_only_with_native_media() {
        for arguments in [
            strings(&[
                "serve",
                "show.freemix",
                "--native-media",
                "--camera-helper",
                "helper-bin",
            ]),
            strings(&[
                "serve",
                "show.freemix",
                "--native-media",
                "--camera-helper=helper-bin",
            ]),
        ] {
            assert!(matches!(
                parse_args(arguments).unwrap(),
                Command::Serve {
                    camera_helper: Some(path),
                    ..
                } if path == Path::new("helper-bin")
            ));
        }
        assert_eq!(
            parse_args(strings(&[
                "serve",
                "show.freemix",
                "--camera-helper",
                "helper-bin",
            ]))
            .unwrap_err()
            .to_string(),
            "--camera-helper requires --native-media"
        );
        for arguments in [
            strings(&["serve", "show.freemix", "--native-media", "--camera-helper"]),
            strings(&[
                "serve",
                "show.freemix",
                "--native-media",
                "--camera-helper=",
            ]),
        ] {
            assert_eq!(
                parse_args(arguments).unwrap_err().to_string(),
                "missing value for --camera-helper"
            );
        }
        assert_eq!(
            parse_args(strings(&[
                "serve",
                "show.freemix",
                "--native-media",
                "--camera-helper=a",
                "--camera-helper",
                "b",
            ]))
            .unwrap_err()
            .to_string(),
            "duplicate option `--camera-helper`"
        );
    }

    #[test]
    fn parses_bounded_diagnostic_stop_duration() {
        assert_eq!(
            parse_diagnostic_duration("1500ms").unwrap(),
            Duration::from_millis(1_500)
        );
        assert_eq!(
            parse_diagnostic_duration("2s").unwrap(),
            Duration::from_secs(2)
        );
        assert_eq!(
            parse_diagnostic_duration("3m").unwrap(),
            Duration::from_mins(3)
        );
        assert_eq!(
            parse_diagnostic_duration("24h").unwrap(),
            Duration::from_hours(24)
        );
        assert!(matches!(
            parse_args(strings(&[
                "serve",
                "show.freemix",
                "--native-media",
                "--diagnostic-stop-after=10m",
            ]))
            .unwrap(),
            Command::Serve {
                diagnostic_stop_after: Some(duration),
                ..
            } if duration == Duration::from_mins(10)
        ));
    }

    #[test]
    fn rejects_invalid_diagnostic_stop_options() {
        for (arguments, expected) in [
            (
                strings(&["serve", "show.freemix", "--diagnostic-stop-after", "1s"]),
                "--diagnostic-stop-after requires --native-media",
            ),
            (
                strings(&[
                    "serve",
                    "show.freemix",
                    "--native-media",
                    "--once",
                    "--diagnostic-stop-after",
                    "1s",
                ]),
                "--diagnostic-stop-after cannot be combined with --once",
            ),
            (
                strings(&[
                    "serve",
                    "show.freemix",
                    "--native-media",
                    "--fullscreen-program",
                    "--diagnostic-stop-after",
                    "1s",
                ]),
                "--diagnostic-stop-after currently supports headless native mode only",
            ),
            (
                strings(&[
                    "serve",
                    "show.freemix",
                    "--native-media",
                    "--diagnostic-stop-after",
                    "0s",
                ]),
                "diagnostic duration must be nonzero",
            ),
            (
                strings(&[
                    "serve",
                    "show.freemix",
                    "--native-media",
                    "--diagnostic-stop-after",
                    "25h",
                ]),
                "diagnostic duration cannot exceed 24h",
            ),
            (
                strings(&[
                    "serve",
                    "show.freemix",
                    "--native-media",
                    "--diagnostic-stop-after",
                    "1s",
                    "--diagnostic-stop-after=2s",
                ]),
                "duplicate option `--diagnostic-stop-after`",
            ),
        ] {
            assert_eq!(parse_args(arguments).unwrap_err().to_string(), expected);
        }
    }

    #[test]
    fn parses_program_recording_with_once_and_fullscreen() {
        assert!(matches!(
            parse_args(strings(&[
                "serve",
                "show.freemix",
                "--native-media",
                "--fullscreen-program",
                "--record-program",
                "capture.mp4",
                "--once",
            ]))
            .unwrap(),
            Command::Serve {
                once: true,
                native_media: true,
                fullscreen_program: true,
                record_program: Some(path),
                ..
            } if path == Path::new("capture.mp4")
        ));
    }

    #[test]
    fn parses_equals_program_recording_with_option_like_file_name() {
        assert!(matches!(
            parse_args(strings(&[
                "serve",
                "show.freemix",
                "--native-media",
                "--record-program=--capture.mp4",
            ]))
            .unwrap(),
            Command::Serve {
                record_program: Some(path),
                ..
            } if path == Path::new("--capture.mp4")
        ));
    }

    #[test]
    fn rejects_invalid_program_recording_option_relationships() {
        for (arguments, expected) in [
            (
                strings(&["serve", "show.freemix", "--record-program", "capture.mp4"]),
                "--record-program requires --native-media",
            ),
            (
                strings(&[
                    "serve",
                    "show.freemix",
                    "--native-media",
                    "--record-program",
                ]),
                "missing value for --record-program",
            ),
            (
                strings(&[
                    "serve",
                    "show.freemix",
                    "--native-media",
                    "--record-program",
                    "--once",
                ]),
                "missing value for --record-program",
            ),
            (
                strings(&[
                    "serve",
                    "show.freemix",
                    "--native-media",
                    "--record-program",
                    "one.mp4",
                    "--record-program",
                    "two.mp4",
                ]),
                "duplicate option `--record-program`",
            ),
            (
                strings(&[
                    "serve",
                    "show.freemix",
                    "--native-media",
                    "--record-program=",
                ]),
                "missing value for --record-program",
            ),
            (
                strings(&[
                    "serve",
                    "show.freemix",
                    "--native-media",
                    "--record-program=one.mp4",
                    "--record-program=two.mp4",
                ]),
                "duplicate option `--record-program`",
            ),
        ] {
            assert_eq!(parse_args(arguments).unwrap_err().to_string(), expected);
        }
    }

    #[test]
    fn capability_digest_distinguishes_recorder_modes() {
        assert_eq!(
            capabilities_digest(false, false, false),
            CAPABILITIES_DIGEST
        );
        assert_eq!(
            capabilities_digest(true, false, false),
            NATIVE_MEDIA_CAPABILITIES_DIGEST
        );
        assert_eq!(
            capabilities_digest(true, false, true),
            PROGRAM_RECORDER_CAPABILITIES_DIGEST
        );
        assert_eq!(
            capabilities_digest(true, true, false),
            FULLSCREEN_PROGRAM_CAPABILITIES_DIGEST
        );
        assert_eq!(
            capabilities_digest(true, true, true),
            FULLSCREEN_PROGRAM_RECORDER_CAPABILITIES_DIGEST
        );
    }

    #[cfg(feature = "native-media")]
    #[test]
    fn native_telemetry_diagnostic_names_scoped_metrics_and_unavailable_gpu_time() {
        let origin = Instant::now();
        let adapter = fm_gpu::NativeAdapterInfo {
            name: "Test Adapter".to_owned(),
            backend: NativeBackend::Metal,
        };
        let mut telemetry = NativeRuntimeTelemetry::new(origin, &adapter);
        telemetry.audio_retained_bytes = 10;
        telemetry.audio_peak_retained_bytes = 11;
        telemetry.audio_reservation_requests = 12;
        telemetry.audio_reserved_blocks = 13;
        telemetry.audio_peak_reserved_blocks = 14;
        telemetry.audio_reserved_samples = 15;
        telemetry.audio_peak_reserved_samples = 16;
        telemetry.audio_reserved_bytes = 17;
        telemetry.audio_peak_reserved_bytes = 18;
        telemetry.audio_source_stalls = 19;
        telemetry.audio_positioned_blocks = 20;
        telemetry.audio_positioned_samples = 21;
        telemetry.audio_leading_silence_samples = 22;
        telemetry.audio_eos_padding_blocks = 23;
        telemetry.audio_eos_padding_samples = 24;
        let deadline = origin
            .checked_sub(Duration::from_millis(10))
            .expect("test deadline is representable");
        telemetry.observe_host_lateness(deadline, origin);
        let diagnostic = telemetry.diagnostic(Some(fm_gpu::PresentationTelemetry {
            pending_depth: 1,
            peak_pending_depth: 1,
            frames_dropped: 2,
            ..fm_gpu::PresentationTelemetry::default()
        }));

        assert!(diagnostic.starts_with("FREEMIXD_TELEMETRY\tv=4\t"));
        assert!(diagnostic.contains("\thost_lateness_samples_total=1\t"));
        assert!(diagnostic.contains("\thost_lateness_samples_retained=1\t"));
        assert!(diagnostic.contains("\thost_lateness_p50_ms=10.000\t"));
        assert!(diagnostic.contains("\taudio_retained_bytes=10\t"));
        assert!(diagnostic.contains("\taudio_observed_peak_retained_bytes=11\t"));
        assert!(diagnostic.contains("\taudio_reservation_requests=12\t"));
        assert!(diagnostic.contains("\taudio_reserved_blocks=13\t"));
        assert!(diagnostic.contains("\taudio_observed_peak_reserved_blocks=14\t"));
        assert!(diagnostic.contains("\taudio_reserved_samples=15\t"));
        assert!(diagnostic.contains("\taudio_observed_peak_reserved_samples=16\t"));
        assert!(diagnostic.contains("\taudio_reserved_bytes=17\t"));
        assert!(diagnostic.contains("\taudio_observed_peak_reserved_bytes=18\t"));
        assert!(diagnostic.contains("\taudio_source_stalls=19\t"));
        assert!(diagnostic.contains("\taudio_positioned_blocks=20\t"));
        assert!(diagnostic.contains("\taudio_positioned_samples=21\t"));
        assert!(diagnostic.contains("\taudio_leading_silence_samples=22\t"));
        assert!(diagnostic.contains("\taudio_eos_padding_blocks=23\t"));
        assert!(diagnostic.contains("\taudio_eos_padding_samples=24\t"));
        assert!(diagnostic.contains("\tpresentation_active=true\t"));
        assert!(diagnostic.contains("\tpresentation_pending_depth=1\t"));
        assert!(diagnostic.contains("\tpresentation_dropped=2\t"));
        assert!(diagnostic.contains("\trecorder_configured=false\t"));
        assert!(diagnostic.contains("\tcamera_configured_sources=0\t"));
        assert!(diagnostic.contains("\tcamera_frames_received=0\t"));
        assert!(diagnostic.contains("\tcamera_frames_ingested=0\t"));
        assert!(diagnostic.contains("\tgpu_backend=Metal\t"));
        assert!(diagnostic.contains("\tgpu_adapter=Test Adapter\t"));
        assert!(diagnostic.contains("\tgpu_timing=Unsupported\t"));
        assert!(diagnostic.contains("\tgpu_pass_samples_total=0\t"));
        assert!(diagnostic.contains("\tgpu_pass_samples_retained=0\t"));
        assert!(diagnostic.contains("\tgpu_pass_p50_ms=none\t"));
        assert!(diagnostic.contains("\tmetric_errors=0\t"));
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    fn camera_source_test_telemetry() -> NativeCameraSourceTelemetry {
        NativeCameraSourceTelemetry {
            input: test_input_id(1),
            lifecycle: LifecycleState::Running,
            health: EndpointHealthState::Healthy,
            frames_received: 20,
            frames_ingested: 6,
            native_dropped: 2,
            queue_dropped: 1,
            queue_depth: 1,
            queue_peak_depth: 2,
            continuity_rejected: 0,
            recovery_timeout_discarded: 0,
            terminal_error_discarded: 1,
            terminal_trigger_discarded: 1,
            ready_delivery_depth: 1,
            ready_delivery_discarded: 1,
            cancellation_discarded: 1,
            supervisor_slot_replaced: 3,
            supervisor_slot_depth: 1,
            ingest_failed: 1,
            preflight_depth: 1,
            preflight_discarded: 1,
            recovery_attempts: 2,
            recovery_successes: 1,
            recovery_exhausted: 0,
            recovery_worker_failures: 0,
            recovery_outcome: CameraRecoveryOutcome::Recovered,
        }
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    #[test]
    fn camera_source_diagnostics_are_stable_bounded_and_saturating() {
        assert_eq!(lifecycle_label(LifecycleState::Closed), "closed");
        assert_eq!(lifecycle_label(LifecycleState::Open), "open");
        assert_eq!(lifecycle_label(LifecycleState::Running), "running");
        assert_eq!(lifecycle_label(LifecycleState::Lost), "lost");
        assert_eq!(lifecycle_label(LifecycleState::Recovering), "recovering");
        assert_eq!(health_label(EndpointHealthState::Healthy), "healthy");
        assert_eq!(health_label(EndpointHealthState::Degraded), "degraded");
        assert_eq!(health_label(EndpointHealthState::SignalLost), "signal_lost");
        assert_eq!(health_label(EndpointHealthState::Failed), "failed");

        let source = camera_source_test_telemetry();
        let diagnostic = source.diagnostic();
        assert!(diagnostic.starts_with(
            "FREEMIXD_CAMERA_SOURCE\tv=1\tclassification=diagnostic-not-certification\t"
        ));
        assert!(diagnostic.contains("\tsample_phase=pre_cleanup\t"));
        assert!(diagnostic.contains("\tsample_lifecycle=running\thealth=healthy\t"));
        assert!(diagnostic.contains("\trecovery_attempts=2\t"));
        assert!(diagnostic.contains("\tsupervisor_slot_replaced=3\t"));
        assert!(diagnostic.contains("\tsupervisor_slot_depth=1\t"));
        assert!(diagnostic.contains("\tterminal_error_discarded=1\t"));
        assert!(diagnostic.contains("\tterminal_trigger_discarded=1\t"));
        assert!(diagnostic.contains("\tready_delivery_depth=1\t"));
        assert!(diagnostic.contains("\tready_delivery_discarded=1\t"));
        assert!(diagnostic.contains("\tcancellation_discarded=1\t"));
        assert!(diagnostic.contains("\tingest_failed=1\t"));
        assert!(diagnostic.contains("\tpreflight_depth=1\t"));
        assert!(diagnostic.contains("\tpreflight_discarded=1\t"));
        assert!(diagnostic.contains("\trecovery_outcome=recovered"));
        assert!(diagnostic.len() < 768);
        assert_eq!(source.frames_received, source.accounted_frames());

        let mut second = source;
        second.input = test_input_id(2);
        let mut unsorted = [second, source];
        sort_camera_source_telemetry(&mut unsorted);
        assert_eq!(
            unsorted.map(|sample| sample.input),
            [test_input_id(1), test_input_id(2)]
        );

        let mut saturated = source;
        saturated.frames_received = u64::MAX;
        saturated.frames_ingested = u64::MAX;
        saturated.native_dropped = u64::MAX;
        saturated.queue_dropped = u64::MAX;
        saturated.queue_depth = u64::MAX;
        saturated.queue_peak_depth = u64::MAX;
        saturated.continuity_rejected = u64::MAX;
        saturated.recovery_timeout_discarded = u64::MAX;
        saturated.terminal_error_discarded = u64::MAX;
        saturated.terminal_trigger_discarded = u64::MAX;
        saturated.ready_delivery_depth = u64::MAX;
        saturated.ready_delivery_discarded = u64::MAX;
        saturated.cancellation_discarded = u64::MAX;
        saturated.supervisor_slot_replaced = u64::MAX;
        saturated.supervisor_slot_depth = u64::MAX;
        saturated.ingest_failed = u64::MAX;
        saturated.preflight_depth = u64::MAX;
        saturated.preflight_discarded = u64::MAX;
        saturated.recovery_attempts = u64::MAX;
        saturated.recovery_successes = u64::MAX;
        saturated.recovery_exhausted = u64::MAX;
        saturated.recovery_worker_failures = u64::MAX;
        let aggregate = aggregate_camera_telemetry(&[saturated, source]);
        assert_eq!(aggregate.configured_sources, 2);
        assert_eq!(aggregate.frames_received, u64::MAX);
        assert_eq!(aggregate.frames_ingested, u64::MAX);
        assert_eq!(aggregate.native_dropped, u64::MAX);
        assert_eq!(aggregate.queue_dropped, u64::MAX);
        assert_eq!(aggregate.queue_depth, u64::MAX);
        assert_eq!(aggregate.queue_peak_depth, u64::MAX);
        assert_eq!(aggregate.continuity_rejected, u64::MAX);
        assert_eq!(aggregate.recovery_timeout_discarded, u64::MAX);
        assert_eq!(aggregate.terminal_error_discarded, u64::MAX);
        assert_eq!(aggregate.terminal_trigger_discarded, u64::MAX);
        assert_eq!(aggregate.ready_delivery_depth, u64::MAX);
        assert_eq!(aggregate.ready_delivery_discarded, u64::MAX);
        assert_eq!(aggregate.cancellation_discarded, u64::MAX);
        assert_eq!(aggregate.supervisor_slot_replaced, u64::MAX);
        assert_eq!(aggregate.supervisor_slot_depth, u64::MAX);
        assert_eq!(aggregate.ingest_failed, u64::MAX);
        assert_eq!(aggregate.preflight_depth, u64::MAX);
        assert_eq!(aggregate.preflight_discarded, u64::MAX);
        assert_eq!(aggregate.recovery_attempts, u64::MAX);
        assert_eq!(aggregate.recovery_successes, u64::MAX);
        assert_eq!(aggregate.recovery_exhausted, u64::MAX);
        assert_eq!(aggregate.recovery_worker_failures, u64::MAX);
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    #[test]
    fn camera_latest_slot_carries_discontinuity_across_replacement_race() {
        let mut slot = CameraFrameSlot::default();
        slot.replace(camera_slot_test_frame(8, true));
        slot.replace(camera_slot_test_frame(9, false));
        slot.replace(camera_slot_test_frame(10, false));

        let newest = slot.take().unwrap();
        assert_eq!(newest.timing().sequence().get(), 10);
        assert!(
            newest
                .timing()
                .flags()
                .contains(fm_frame::MediaFlags::DISCONTINUITY)
        );
        assert_eq!(slot.replacements, 2);
        assert!(slot.take().is_none());
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    #[test]
    fn camera_ingest_failure_is_an_exact_accounted_drop() {
        let mut frame = CameraFrameSlot::default();
        frame.replace(camera_slot_test_frame(7, false));
        let supervisor = Arc::new(Mutex::new(CameraSupervisorState {
            frame,
            snapshot: CameraWorkerSnapshot {
                telemetry: CameraTelemetry {
                    received: 1,
                    ..CameraTelemetry::default()
                },
                lifecycle: LifecycleState::Running,
                health: EndpointHealthState::Healthy,
                recovery_attempts: 0,
                recovery_successes: 0,
                recovery_exhausted: 0,
                recovery_worker_failures: 0,
                recovery_outcome: CameraRecoveryOutcome::Never,
            },
        }));
        let mut cameras = NativeCameraInputs {
            inputs: vec![NativeCameraInput {
                input: test_input_id(1),
                source: None,
                worker: None,
                supervisor,
                recovery_policy: test_camera_recovery_policy(1, Duration::from_millis(10)),
                ingested_frames: 0,
                ingest_failed: 0,
                preflight_depth: 0,
                preflight_discarded: 0,
                last_ingested_sequence: None,
                last_ingested_discontinuity: false,
            }],
            telemetry_emitted: false,
        };

        let error = cameras
            .poll_with(|_, _| Err(AppFailure("injected ingest failure".to_owned()).into()))
            .unwrap_err();
        assert_eq!(error.to_string(), "injected ingest failure");
        let telemetry = cameras.source_telemetry()[0];
        assert_eq!(telemetry.frames_received, 1);
        assert_eq!(telemetry.frames_ingested, 0);
        assert_eq!(telemetry.ingest_failed, 1);
        assert_eq!(telemetry.queue_depth, 0);
        assert_eq!(telemetry.supervisor_slot_depth, 0);
        assert_eq!(telemetry.frames_received, telemetry.accounted_frames());
    }

    #[cfg(feature = "native-media")]
    #[test]
    fn recorder_capability_is_configured_support_not_live_health() {
        let digest = capabilities_digest(true, false, true);
        let mut policy = RecorderCapturePolicy::default();
        assert!(policy.fail("backend:failed".to_owned(), false).is_some());
        assert_eq!(digest, capabilities_digest(true, false, true));
        assert!(!policy.active());
    }

    #[cfg(feature = "native-media")]
    #[test]
    fn startup_pair_barrier_requires_mux_output_and_prioritizes_failure() {
        assert_eq!(
            startup_pair_decision(RecorderState::Recording, 0, 0, false),
            StartupPairDecision::Pending
        );
        assert_eq!(
            startup_pair_decision(RecorderState::Recording, 1, 0, false),
            StartupPairDecision::Pending
        );
        assert_eq!(
            startup_pair_decision(RecorderState::Recording, 1, 1, false),
            StartupPairDecision::Ready
        );
        assert_eq!(
            startup_pair_decision(RecorderState::Recording, 1, 1, true),
            StartupPairDecision::Failed
        );
        assert_eq!(
            startup_pair_decision(RecorderState::Failed, 1, 1, true),
            StartupPairDecision::Failed
        );
    }

    #[test]
    fn client_socket_configuration_sets_read_and_write_timeouts() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let _client = TcpStream::connect(address).unwrap();
        let (server_stream, _) = listener.accept().unwrap();

        configure_client_socket(&server_stream).unwrap();

        let read_timeout = server_stream.read_timeout().unwrap();
        let write_timeout = server_stream.write_timeout().unwrap();
        assert_eq!(read_timeout, Some(CLIENT_READ_POLL_INTERVAL));
        assert_eq!(write_timeout, Some(CLIENT_WRITE_TIMEOUT));
    }

    #[test]
    fn client_write_timeouts_are_disconnects() {
        for kind in [std::io::ErrorKind::TimedOut, std::io::ErrorKind::WouldBlock] {
            let error = client_write_error(std::io::Error::from(kind));
            assert!(is_client_disconnect(error.as_ref()));
        }
    }

    fn complete_test_handshake(stream: &TcpStream) -> (MessageReader, ServerIdentity) {
        write_message(
            &mut stream.try_clone().unwrap(),
            &WireMessage::HandshakeRequest(HandshakeRequest {
                protocol: PROTOCOL_VERSION,
                build: "control-timeout-test".into(),
                client_type: ClientType::Integration,
                desired_role: Role::Operator,
                resume_cursor: None,
            }),
        )
        .unwrap();
        let mut reader = MessageReader::new(stream.try_clone().unwrap());
        let Some(WireMessage::HandshakeResponse(response)) =
            reader.read_message_with_idle(|| Ok(false)).unwrap()
        else {
            panic!("expected handshake response");
        };
        assert!(matches!(
            reader.read_message_with_idle(|| Ok(false)).unwrap(),
            Some(WireMessage::Snapshot(_))
        ));
        (reader, response.server)
    }

    #[test]
    fn incomplete_pre_handshake_peer_releases_accept_loop() {
        const HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(100);

        let directory = tempfile::tempdir().unwrap();
        let project_path = directory.path().join("show.freemix");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = std::sync::mpsc::sync_channel(1);
        let (expired_tx, expired_rx) = std::sync::mpsc::sync_channel(1);
        let server_thread = thread::spawn(move || {
            let store = ProjectStore::new(project_path).unwrap();
            let mut durable = test_project();
            let project_id = durable.project().id();
            let control = Rc::new(RefCell::new(test_control(&durable)));
            let authority = control_server_identity(&control.borrow(), project_id);
            let config = ServerConfig::new(
                ServerMode::Development,
                AuthenticationMode::Development,
                address.ip(),
                CAPABILITIES_DIGEST,
            )
            .with_session_limits(fm_server::SessionLimits {
                heartbeat_timeout_ms: u64::try_from(HANDSHAKE_TIMEOUT.as_millis()).unwrap(),
                ..fm_server::SessionLimits::default()
            });
            let mut server = Server::new(config, ControlHandle(Rc::clone(&control))).unwrap();
            server.mark_ready().unwrap();
            let principal = development_principal().unwrap();

            for client_index in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                if client_index == 0 {
                    accepted_tx.send(Instant::now()).unwrap();
                }
                handle_client(
                    stream,
                    &server,
                    &control,
                    &store,
                    &mut durable,
                    &principal,
                    &authority,
                    None,
                    None,
                    &mut OnceClientOutcome::Unserved,
                )
                .unwrap();
                if client_index == 0 {
                    expired_tx.send(Instant::now()).unwrap();
                }
            }
        });

        let mut incomplete_peer = TcpStream::connect(address).unwrap();
        let accepted_at = accepted_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let deadline_lower_bound = accepted_at.checked_add(HANDSHAKE_TIMEOUT).unwrap();
        let mut wrote_partial_bytes = false;
        while Instant::now() < deadline_lower_bound {
            assert_eq!(incomplete_peer.write(b"x").unwrap(), 1);
            wrote_partial_bytes = true;
            thread::sleep(Duration::from_millis(10));
        }
        assert!(wrote_partial_bytes);
        let expired_at = expired_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(expired_at >= deadline_lower_bound);
        let next_peer = TcpStream::connect(address).unwrap();
        next_peer
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        complete_test_handshake(&next_peer);

        drop(next_peer);
        server_thread.join().unwrap();
    }

    #[test]
    fn outbound_rate_limited_client_releases_accept_loop() {
        let directory = tempfile::tempdir().unwrap();
        let project_path = directory.path().join("show.freemix");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (terminated_tx, terminated_rx) = std::sync::mpsc::sync_channel(1);
        let (completed_tx, completed_rx) = std::sync::mpsc::sync_channel(1);
        let server_thread = thread::spawn(move || {
            let store = ProjectStore::new(project_path).unwrap();
            let mut durable = test_project();
            let project_id = durable.project().id();
            let control = Rc::new(RefCell::new(test_control(&durable)));
            let authority = control_server_identity(&control.borrow(), project_id);
            let config = ServerConfig::new(
                ServerMode::Development,
                AuthenticationMode::Development,
                address.ip(),
                CAPABILITIES_DIGEST,
            )
            .with_session_limits(fm_server::SessionLimits {
                outbound_messages: fm_server::RateLimit::new(2, 60_000),
                ..fm_server::SessionLimits::default()
            });
            let mut server = Server::new(config, ControlHandle(Rc::clone(&control))).unwrap();
            server.mark_ready().unwrap();
            let principal = development_principal().unwrap();

            let (stream, _) = listener.accept().unwrap();
            let error = handle_client(
                stream,
                &server,
                &control,
                &store,
                &mut durable,
                &principal,
                &authority,
                None,
                None,
                &mut OnceClientOutcome::Unserved,
            )
            .unwrap_err();
            assert!(!is_client_disconnect(error.as_ref()));
            assert!(!is_client_protocol_error(error.as_ref()));
            assert!(is_client_session_termination(error.as_ref()));
            terminated_tx.send(()).unwrap();

            let (stream, _) = listener.accept().unwrap();
            handle_client(
                stream,
                &server,
                &control,
                &store,
                &mut durable,
                &principal,
                &authority,
                None,
                None,
                &mut OnceClientOutcome::Unserved,
            )
            .unwrap();
            completed_tx.send(()).unwrap();
        });

        let client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        client
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let (mut reader, mut server) = complete_test_handshake(&client);
        server.project_id = "wrong-project".into();
        write_message(
            &mut client.try_clone().unwrap(),
            &WireMessage::Heartbeat(HeartbeatMessage {
                server,
                sequence: 1,
                sent_at_ms: 1_234,
                last_applied: None,
            }),
        )
        .unwrap();
        terminated_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let mut eof = [0_u8; 1];
        assert_eq!(reader.stream.read(&mut eof).unwrap(), 0);
        drop(client);

        let next_client = TcpStream::connect(address).unwrap();
        next_client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        next_client
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        complete_test_handshake(&next_client);
        drop(next_client);

        completed_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        server_thread.join().unwrap();
    }

    #[test]
    fn expired_tcp_session_is_reclaimed_for_next_client() {
        const HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(600);
        const HEARTBEAT_DELAY: Duration = Duration::from_millis(200);

        let directory = tempfile::tempdir().unwrap();
        let project_path = directory.path().join("show.freemix");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (expired_tx, expired_rx) = std::sync::mpsc::sync_channel(1);
        let server_thread = thread::spawn(move || {
            let store = ProjectStore::new(project_path).unwrap();
            let mut durable = test_project();
            let project_id = durable.project().id();
            let control = Rc::new(RefCell::new(test_control(&durable)));
            let authority = control_server_identity(&control.borrow(), project_id);
            let config = ServerConfig::new(
                ServerMode::Development,
                AuthenticationMode::Development,
                address.ip(),
                CAPABILITIES_DIGEST,
            )
            .with_session_limits(fm_server::SessionLimits {
                heartbeat_timeout_ms: u64::try_from(HEARTBEAT_TIMEOUT.as_millis()).unwrap(),
                ..fm_server::SessionLimits::default()
            });
            let mut server = Server::new(config, ControlHandle(Rc::clone(&control))).unwrap();
            server.mark_ready().unwrap();
            let principal = development_principal().unwrap();

            for client_index in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                handle_client(
                    stream,
                    &server,
                    &control,
                    &store,
                    &mut durable,
                    &principal,
                    &authority,
                    None,
                    None,
                    &mut OnceClientOutcome::Unserved,
                )
                .unwrap();
                if client_index == 0 {
                    expired_tx.send(()).unwrap();
                }
            }
        });

        let client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let (mut reader, server) = complete_test_handshake(&client);
        let original_deadline = Instant::now() + HEARTBEAT_TIMEOUT;
        assert!(matches!(
            expired_rx.recv_timeout(HEARTBEAT_DELAY),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));

        let heartbeat_sent_at = Instant::now();
        write_message(
            &mut client.try_clone().unwrap(),
            &WireMessage::Heartbeat(HeartbeatMessage {
                server: server.clone(),
                sequence: 1,
                sent_at_ms: 1_234,
                last_applied: None,
            }),
        )
        .unwrap();
        let Some(WireMessage::HeartbeatAcknowledgement(acknowledgement)) =
            reader.read_message_with_idle(|| Ok(false)).unwrap()
        else {
            panic!("expected heartbeat acknowledgement");
        };
        assert_eq!(acknowledgement.server, server);
        assert_eq!(acknowledgement.heartbeat_sequence, 1);

        assert!(matches!(
            expired_rx.recv_timeout(original_deadline.saturating_duration_since(Instant::now())),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(Instant::now() >= original_deadline);
        expired_rx.recv_timeout(HEARTBEAT_TIMEOUT).unwrap();
        assert!(heartbeat_sent_at.elapsed() >= HEARTBEAT_TIMEOUT);

        let next_client = TcpStream::connect(address).unwrap();
        next_client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        complete_test_handshake(&next_client);
        drop(next_client);

        server_thread.join().unwrap();
    }

    #[cfg(feature = "native-media")]
    #[test]
    fn recorder_capture_failure_policy_formats_one_sanitized_notice() {
        let mut policy = RecorderCapturePolicy::default();
        let Some((failure, app_capture_failure)) =
            policy.fail("readback:first\tline\nend".to_owned(), true)
        else {
            panic!("first failure must emit a notice");
        };
        assert_eq!(
            recorder_failure_notice(failure, app_capture_failure),
            "FREEMIXD_RECORDER_FAILURE\tv=1\tapp_capture_failure=true\tfailure=readback:first line end",
        );
        assert!(policy.fail("enqueue:second".to_owned(), false).is_none());
    }

    #[cfg(feature = "native-media")]
    #[test]
    fn recording_output_requires_mp4_and_existing_directory_parent() {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            create_record_output(&directory.path().join("capture.mov"))
                .unwrap_err()
                .to_string()
                .contains("final extension `.mp4`")
        );
        assert!(
            create_record_output(&directory.path().join("missing/capture.mp4"))
                .unwrap_err()
                .to_string()
                .contains("existing canonical directory")
        );
        let not_directory = directory.path().join("parent-file");
        fs::write(&not_directory, b"parent").unwrap();
        assert!(
            create_record_output(&not_directory.join("capture.mp4"))
                .unwrap_err()
                .to_string()
                .contains("existing canonical directory")
        );
    }

    #[cfg(feature = "native-media")]
    #[test]
    fn recording_output_is_exclusive_and_never_overwrites() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.mp4");
        drop(create_record_output(&path).unwrap());
        fs::write(&path, b"keep-me").unwrap();
        assert!(create_record_output(&path).is_err());
        assert_eq!(fs::read(path).unwrap(), b"keep-me");

        let occupied_directory = directory.path().join("occupied.mp4");
        fs::create_dir(&occupied_directory).unwrap();
        assert!(create_record_output(&occupied_directory).is_err());
    }

    #[cfg(all(feature = "native-media", unix))]
    #[test]
    fn recording_output_rejects_existing_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        fs::write(&target, b"keep-me").unwrap();
        let output = directory.path().join("capture.mp4");
        symlink(&target, &output).unwrap();
        assert!(create_record_output(&output).is_err());
        assert_eq!(fs::read(target).unwrap(), b"keep-me");
    }

    #[cfg(feature = "native-media")]
    #[test]
    fn recording_path_policy_protects_final_component_not_hostile_parent_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.mp4");
        drop(create_record_output(&path).unwrap());
        assert!(create_record_output(&path).is_err());
        // Concurrent hostile replacement of the canonical parent is outside
        // this path-based API boundary and requires platform file capabilities.
    }

    #[test]
    fn parses_fullscreen_program_display_opt_in() {
        assert!(matches!(
            parse_args(strings(&[
                "serve",
                "show.freemix",
                "--native-media",
                "--fullscreen-program",
                "--fullscreen-display",
                "2",
            ]))
            .unwrap(),
            Command::Serve {
                native_media: true,
                fullscreen_program: true,
                fullscreen_display: 2,
                ..
            }
        ));
    }

    #[test]
    fn fullscreen_display_defaults_to_zero() {
        assert!(matches!(
            parse_args(strings(&[
                "serve",
                "show.freemix",
                "--native-media",
                "--fullscreen-program",
            ]))
            .unwrap(),
            Command::Serve {
                fullscreen_program: true,
                fullscreen_display: 0,
                ..
            }
        ));
    }

    #[test]
    fn rejects_invalid_fullscreen_option_relationships_and_values() {
        for (arguments, expected) in [
            (
                strings(&["serve", "show.freemix", "--fullscreen-program"]),
                "--fullscreen-program requires --native-media",
            ),
            (
                strings(&["serve", "show.freemix", "--fullscreen-display", "0"]),
                "--fullscreen-display requires --fullscreen-program",
            ),
            (
                strings(&[
                    "serve",
                    "show.freemix",
                    "--native-media",
                    "--fullscreen-program",
                    "--fullscreen-display",
                ]),
                "missing value for --fullscreen-display",
            ),
        ] {
            assert_eq!(parse_args(arguments).unwrap_err().to_string(), expected);
        }
        assert!(
            parse_args(strings(&[
                "serve",
                "show.freemix",
                "--native-media",
                "--fullscreen-program",
                "--fullscreen-display",
                "-1",
            ]))
            .unwrap_err()
            .to_string()
            .contains("invalid fullscreen display index")
        );
    }

    #[test]
    fn rejects_duplicate_fullscreen_options() {
        for arguments in [
            strings(&[
                "serve",
                "show.freemix",
                "--native-media",
                "--fullscreen-program",
                "--fullscreen-program",
            ]),
            strings(&[
                "serve",
                "show.freemix",
                "--native-media",
                "--fullscreen-program",
                "--fullscreen-display",
                "0",
                "--fullscreen-display",
                "1",
            ]),
        ] {
            assert!(parse_args(arguments).is_err());
        }
    }

    #[cfg(not(feature = "native-media"))]
    #[test]
    fn native_media_opt_in_fails_when_support_is_not_compiled() {
        let store = ProjectStore::new("unloaded-test-project.freemix").unwrap();
        let error = NativeDaemon::start(&store, &test_project(), None)
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            "native-media support was not compiled in"
        );
    }

    #[cfg(feature = "native-media")]
    #[test]
    fn native_mode_rejects_unimplemented_simulated_sine_audio() {
        let error = validate_native_audio_modes(&test_project()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("requires unsupported simulated sine audio")
        );
    }

    #[cfg(feature = "native-media")]
    #[test]
    fn native_source_extraction_builds_retained_generators_without_assets() {
        let store = ProjectStore::new("unloaded-test-project.freemix").unwrap();
        let resolution = resolve_native_sources(&store, &test_project(), None).unwrap();
        let sources = resolution.sources;
        assert_eq!(sources.len(), 2);
        for (index, source) in sources.iter().enumerate() {
            let NativeResolvedSource::RetainedFrame { input, frame } = source else {
                panic!("simulated source must be retained")
            };
            assert_eq!(*input, test_input_id(u128::try_from(index + 1).unwrap()));
            assert_eq!(frame.payload().dimensions().width(), 1_280);
            assert_eq!(frame.payload().dimensions().height(), 720);
            assert_eq!(frame.timing().sequence().get(), 0);
            assert_eq!(frame.timing().presentation_timestamp().as_nanos(), 0);
            assert!(frame.metadata().is_some());
        }
        let NativeResolvedSource::RetainedFrame { frame, .. } = &sources[0] else {
            unreachable!()
        };
        assert_eq!(
            &frame.payload().plane(0).unwrap().bytes()[..4],
            &[7, 11, 13, 255]
        );
    }

    #[cfg(feature = "native-media")]
    #[test]
    fn native_source_extraction_keeps_scene_inputs_out_of_physical_sources() {
        let store = ProjectStore::new("unloaded-test-project.freemix").unwrap();
        let resolution = resolve_native_sources(&store, &scene_test_project(), None).unwrap();

        assert_eq!(resolution.sources.len(), 1);
        assert_eq!(resolution.sources[0].input(), test_input_id(2));
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    #[test]
    fn macos_camera_resolution_is_exact_non_prompting_and_reaps_helper() {
        let source_id = deterministic_camera_id(CameraIdKind::Source, "fake-camera");
        let stable_key = format!("macos.avfoundation.camera.v1.{source_id}");

        let directory = tempfile::tempdir().unwrap();
        let helper = fake_camera_helper(directory.path(), 0);
        let capture_marker = PathBuf::from(format!("{}.capture", helper.display()));
        let pid_file = PathBuf::from(format!("{}.pid", helper.display()));
        let permission_marker = PathBuf::from(format!("{}.permission", helper.display()));
        let (sources, mut cameras) =
            resolve_macos_camera_sources(&camera_test_project(&stable_key), Some(helper.as_path()))
                .unwrap();
        assert_eq!(sources.len(), 1);
        let NativeResolvedSource::LiveFrame { input, frame } = &sources[0] else {
            panic!("camera source must enter the live frame lane")
        };
        assert_eq!(*input, test_input_id(1));
        assert_eq!(frame.timing().sequence().get(), 7);
        assert_eq!(
            frame.timing().presentation_timestamp().as_nanos(),
            1_000_000_000
        );
        assert_eq!(frame.payload().plane(0).unwrap().bytes(), &[3, 5, 7, 255]);
        assert_eq!(
            frame.metadata().unwrap().color().transfer,
            fm_types::TransferFunction::Bt709
        );
        let pending = cameras.source_telemetry()[0];
        assert_eq!(pending.frames_received, 1);
        assert_eq!(pending.frames_ingested, 0);
        assert_eq!(pending.preflight_depth, 1);
        assert_eq!(pending.preflight_discarded, 0);
        assert_eq!(pending.frames_received, pending.preflight_depth);
        drop(sources);
        cameras.discard_preflight_frames();
        let discarded = cameras.source_telemetry()[0];
        assert_eq!(discarded.preflight_depth, 0);
        assert_eq!(discarded.preflight_discarded, 1);
        assert_eq!(discarded.frames_received, discarded.preflight_discarded);
        assert_eq!(
            fs::read_to_string(&capture_marker).unwrap(),
            "capture\nfake-camera\n1\n1\n30000\n1001\n"
        );
        assert!(!permission_marker.exists());
        let pid = fs::read_to_string(&pid_file).unwrap();
        drop(cameras);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && std::process::Command::new("kill")
                .args(["-0", pid.trim()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !std::process::Command::new("kill")
                .args(["-0", pid.trim()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success()),
            "camera helper {pid} survived source cleanup"
        );

        let unknown_directory = tempfile::tempdir().unwrap();
        let unknown_helper = fake_camera_helper(unknown_directory.path(), 0);
        let unknown_capture = PathBuf::from(format!("{}.capture", unknown_helper.display()));
        let error = resolve_macos_camera_sources(
            &camera_test_project("macos.avfoundation.camera.v1.999"),
            Some(unknown_helper.as_path()),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("unknown camera stable key"));
        assert!(!unknown_capture.exists(), "unknown key invoked capture");

        let prompt_directory = tempfile::tempdir().unwrap();
        let prompt_helper = fake_camera_helper(prompt_directory.path(), 1);
        let prompt_capture = PathBuf::from(format!("{}.capture", prompt_helper.display()));
        let prompt_request = PathBuf::from(format!("{}.permission", prompt_helper.display()));
        let error = resolve_macos_camera_sources(
            &camera_test_project(&stable_key),
            Some(prompt_helper.as_path()),
        )
        .err()
        .unwrap();
        assert!(
            error
                .to_string()
                .contains("camera permission is not granted")
        );
        assert!(
            !prompt_capture.exists(),
            "permission preflight invoked capture"
        );
        assert!(
            !prompt_request.exists(),
            "daemon requested camera permission"
        );
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    #[test]
    fn many_camera_startup_failure_uses_aggregate_cleanup_and_reaps_all_helpers() {
        const CAMERA_COUNT: usize = 16;

        let keys = (0..CAMERA_COUNT)
            .map(|index| {
                let source_id =
                    deterministic_camera_id(CameraIdKind::Source, &format!("fake-camera-{index}"));
                format!("macos.avfoundation.camera.v1.{source_id}")
            })
            .collect::<Vec<_>>();
        let project = many_camera_test_project(&keys);
        let directory = tempfile::tempdir().unwrap();
        let helper = many_camera_startup_failure_helper(directory.path(), CAMERA_COUNT);
        let pid_log = PathBuf::from(format!("{}.pids", helper.display()));

        let started = Instant::now();
        let error = resolve_macos_camera_sources(&project, Some(helper.as_path()))
            .err()
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(error.to_string().contains("failed to start"));
        assert_eq!(
            fs::read_to_string(&pid_log).unwrap().lines().count(),
            CAMERA_COUNT
        );
        assert_helper_processes_reaped(&pid_log);
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    #[test]
    fn camera_poll_into_ingests_recovered_mapped_discontinuity_while_ticks_continue() {
        let source_id = deterministic_camera_id(CameraIdKind::Source, "fake-camera");
        let stable_key = format!("macos.avfoundation.camera.v1.{source_id}");
        let project = camera_test_project(&stable_key);
        let directory = tempfile::tempdir().unwrap();
        let helper = recovering_camera_helper(directory.path(), Some(3), 20);
        let capture_log = PathBuf::from(format!("{}.capture", helper.display()));
        let pid_log = PathBuf::from(format!("{}.pids", helper.display()));
        let runtime = NativeMediaRuntime::new_blocking([platform_native_backend()]).unwrap();
        let (sources, mut cameras) = resolve_macos_camera_sources_with_policy(
            &project,
            Some(helper.as_path()),
            test_camera_recovery_policy(3, Duration::from_millis(100)),
        )
        .unwrap();
        let NativeResolvedSource::LiveFrame { input, frame } = &sources[0] else {
            panic!("camera source must be live")
        };
        assert_eq!(*input, test_input_id(1));
        assert_eq!(frame.timing().sequence().get(), 7);
        let mut playback = runtime
            .preflight_resolved_source_playback_mixed_blocking(
                None,
                sources,
                native_clock_domain(),
                StreamSelector::Best,
                NativeSourceLimits::default(),
            )
            .unwrap();
        cameras.mark_preflight_frames_ingested();
        let committed = cameras.source_telemetry()[0];
        assert_eq!(committed.frames_ingested, 1);
        assert_eq!(committed.preflight_depth, 0);
        assert_eq!(committed.preflight_discarded, 0);
        assert_eq!(committed.frames_received, committed.accounted_frames());

        let mut control = test_control(&project);
        let server = test_server(&control);
        let mut rendered = 0_u64;
        let mut checkpointed = 0_u64;
        let deadline = Instant::now() + Duration::from_secs(5);
        while cameras.inputs[0].ingested_frames == 1 && Instant::now() < deadline {
            cameras.poll_into(&runtime, &mut playback).unwrap();
            control.tick(&server).unwrap();
            rendered = rendered.saturating_add(1);
            let snapshot = control.idle_engine_snapshot().unwrap();
            stored_project_checkpoint(&project, &snapshot).unwrap();
            checkpointed = checkpointed.saturating_add(1);
            thread::sleep(Duration::from_millis(1));
        }

        assert_eq!(cameras.inputs[0].ingested_frames, 2);
        assert_eq!(cameras.inputs[0].last_ingested_sequence, Some(8));
        assert!(cameras.inputs[0].last_ingested_discontinuity);
        assert!(rendered > 10, "render loop stalled during camera recovery");
        assert_eq!(checkpointed, rendered);
        let telemetry = cameras.source_telemetry();
        assert_eq!(telemetry[0].recovery_attempts, 2);
        assert_eq!(telemetry[0].recovery_successes, 1);
        assert_eq!(telemetry[0].recovery_exhausted, 0);
        assert_eq!(telemetry[0].recovery_worker_failures, 0);
        assert_eq!(
            telemetry[0].frames_received,
            telemetry[0].accounted_frames()
        );
        assert_eq!(
            telemetry[0].recovery_outcome,
            CameraRecoveryOutcome::Recovered
        );
        assert!(!telemetry[0].diagnostic().contains("fake-camera"));

        let captures = fs::read_to_string(capture_log).unwrap();
        let expected = ["capture", "fake-camera", "1", "1", "30000", "1001"];
        let captures = captures.lines().collect::<Vec<_>>();
        assert_eq!(captures.len(), 18);
        for invocation in captures.chunks_exact(expected.len()) {
            assert_eq!(invocation, expected);
        }

        cameras.shutdown().unwrap();
        assert_helper_processes_reaped(&pid_log);
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    #[test]
    fn camera_recovery_rearms_after_exhaustion_and_eventually_recovers() {
        let source_id = deterministic_camera_id(CameraIdKind::Source, "fake-camera");
        let stable_key = format!("macos.avfoundation.camera.v1.{source_id}");
        let project = camera_test_project(&stable_key);
        let directory = tempfile::tempdir().unwrap();
        let helper = recovering_camera_helper(directory.path(), Some(4), 20);
        let capture_count = PathBuf::from(format!("{}.count", helper.display()));
        let pid_log = PathBuf::from(format!("{}.pids", helper.display()));
        let (_, mut cameras) = resolve_macos_camera_sources_with_policy(
            &project,
            Some(helper.as_path()),
            test_camera_recovery_policy(2, Duration::from_millis(20)),
        )
        .unwrap();

        let mut control = test_control(&project);
        let server = test_server(&control);
        let mut rendered = 0_u64;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            cameras.poll_with(|_, _| Ok(())).unwrap();
            control.tick(&server).unwrap();
            rendered = rendered.saturating_add(1);
            let telemetry = cameras.source_telemetry();
            if telemetry[0].recovery_successes == 1 || Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let telemetry = cameras.source_telemetry();
        assert_eq!(telemetry[0].recovery_attempts, 3);
        assert_eq!(telemetry[0].recovery_successes, 1);
        assert_eq!(telemetry[0].recovery_exhausted, 1);
        assert_eq!(telemetry[0].recovery_worker_failures, 0);
        assert!(
            rendered > 10,
            "render loop stopped on recoverable camera loss"
        );
        assert_eq!(fs::read_to_string(capture_count).unwrap(), "4");
        cameras.shutdown().unwrap();
        assert_helper_processes_reaped(&pid_log);
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    #[test]
    fn generic_camera_helper_exit_is_recoverable_but_malformed_contract_is_fatal() {
        let source_id = deterministic_camera_id(CameraIdKind::Source, "fake-camera");
        let stable_key = format!("macos.avfoundation.camera.v1.{source_id}");
        let project = camera_test_project(&stable_key);
        let directory = tempfile::tempdir().unwrap();
        let helper = recovering_camera_helper(directory.path(), Some(2), 21);
        let pid_log = PathBuf::from(format!("{}.pids", helper.display()));
        let (_, mut cameras) = resolve_macos_camera_sources_with_policy(
            &project,
            Some(helper.as_path()),
            test_camera_recovery_policy(2, Duration::from_millis(20)),
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while cameras.inputs[0].ingested_frames == 0 && Instant::now() < deadline {
            let poll_started = Instant::now();
            cameras.poll_with(|_, _| Ok(())).unwrap();
            assert!(
                poll_started.elapsed() < Duration::from_millis(100),
                "render-thread camera poll performed blocking cleanup"
            );
            thread::sleep(Duration::from_millis(1));
        }
        let telemetry = cameras.source_telemetry();
        assert_eq!(telemetry[0].recovery_attempts, 1);
        assert_eq!(telemetry[0].recovery_successes, 1);
        assert_eq!(telemetry[0].recovery_exhausted, 0);
        assert_eq!(telemetry[0].recovery_worker_failures, 0);
        assert!(!recoverable_camera_error(&IoError::MalformedTimestamp(
            fm_io_api::TimestampValidationError::OriginalTimestampOverflow,
        )));
        cameras.shutdown().unwrap();
        assert_helper_processes_reaped(&pid_log);
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    #[test]
    fn malformed_camera_frame_bytes_are_fatal_and_never_restart_helper() {
        let source_id = deterministic_camera_id(CameraIdKind::Source, "fake-camera");
        let stable_key = format!("macos.avfoundation.camera.v1.{source_id}");
        let project = camera_test_project(&stable_key);
        let directory = tempfile::tempdir().unwrap();
        let helper = malformed_runtime_camera_helper(directory.path());
        let capture_count = PathBuf::from(format!("{}.count", helper.display()));
        let pid_log = PathBuf::from(format!("{}.pids", helper.display()));
        let (_, mut cameras) = resolve_macos_camera_sources_with_policy(
            &project,
            Some(helper.as_path()),
            test_camera_recovery_policy(2, Duration::from_millis(20)),
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let error = loop {
            match cameras.poll_with(|_, _| Ok(())) {
                Ok(()) => {}
                Err(error) => break error,
            }
            assert!(
                Instant::now() < deadline,
                "malformed frame bytes were swallowed"
            );
            thread::sleep(Duration::from_millis(1));
        };
        assert!(matches!(
            error.downcast_ref::<IoError>(),
            Some(IoError::AdapterFailure {
                remediation: None,
                ..
            })
        ));
        assert_eq!(fs::read_to_string(capture_count).unwrap(), "1");
        let telemetry = cameras.source_telemetry();
        assert_eq!(telemetry[0].recovery_attempts, 0);
        assert_eq!(telemetry[0].recovery_exhausted, 0);
        assert_eq!(telemetry[0].recovery_worker_failures, 1);
        cameras.shutdown().unwrap();
        assert_helper_processes_reaped(&pid_log);
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    #[test]
    fn fatal_camera_contract_error_propagates_without_counting_as_exhaustion() {
        let supervisor = Arc::new(Mutex::new(CameraSupervisorState {
            frame: CameraFrameSlot::default(),
            snapshot: CameraWorkerSnapshot {
                telemetry: CameraTelemetry::default(),
                lifecycle: LifecycleState::Lost,
                health: EndpointHealthState::Failed,
                recovery_attempts: 0,
                recovery_successes: 0,
                recovery_exhausted: 0,
                recovery_worker_failures: 1,
                recovery_outcome: CameraRecoveryOutcome::WorkerFailed,
            },
        }));
        let handle = thread::spawn(|| CameraWorkerResult {
            failure: Some(IoError::MalformedTimestamp(
                fm_io_api::TimestampValidationError::OriginalTimestampOverflow,
            )),
            cleanup_failure: None,
        });
        let mut cameras = NativeCameraInputs {
            inputs: vec![NativeCameraInput {
                input: test_input_id(1),
                source: None,
                worker: Some(NativeCameraWorker {
                    handle: Some(handle),
                    cancel: Arc::new(AtomicBool::new(false)),
                }),
                supervisor,
                recovery_policy: test_camera_recovery_policy(1, Duration::from_millis(10)),
                ingested_frames: 0,
                ingest_failed: 0,
                preflight_depth: 0,
                preflight_discarded: 0,
                last_ingested_sequence: None,
                last_ingested_discontinuity: false,
            }],
            telemetry_emitted: false,
        };
        let deadline = Instant::now() + Duration::from_secs(1);
        let error = loop {
            if let Err(error) = cameras.poll_with(|_, _| Ok(())) {
                break error;
            }
            assert!(
                Instant::now() < deadline,
                "fatal camera error was swallowed"
            );
            thread::sleep(Duration::from_millis(1));
        };
        assert!(error.to_string().contains("malformed timestamp"));
        let telemetry = cameras.source_telemetry();
        assert_eq!(telemetry[0].recovery_exhausted, 0);
        assert_eq!(telemetry[0].recovery_worker_failures, 1);
        assert_eq!(
            telemetry[0].recovery_outcome,
            CameraRecoveryOutcome::WorkerFailed
        );
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    #[test]
    fn one_recovering_camera_does_not_interrupt_second_camera() {
        let first_id = deterministic_camera_id(CameraIdKind::Source, "fake-camera-a");
        let second_id = deterministic_camera_id(CameraIdKind::Source, "fake-camera-b");
        let first_key = format!("macos.avfoundation.camera.v1.{first_id}");
        let second_key = format!("macos.avfoundation.camera.v1.{second_id}");
        let project = two_camera_test_project(&first_key, &second_key);
        let directory = tempfile::tempdir().unwrap();
        let helper = two_camera_helper(directory.path());
        let pid_log = PathBuf::from(format!("{}.pids", helper.display()));
        let (_, mut cameras) = resolve_macos_camera_sources_with_policy(
            &project,
            Some(helper.as_path()),
            test_camera_recovery_policy(2, Duration::from_millis(20)),
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut recovered_input = None;
        while recovered_input.is_none() && Instant::now() < deadline {
            cameras
                .poll_with(|input, frame| {
                    if frame
                        .timing()
                        .flags()
                        .contains(fm_frame::MediaFlags::DISCONTINUITY)
                    {
                        recovered_input = Some(input);
                    }
                    Ok(())
                })
                .unwrap();
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(recovered_input, Some(test_input_id(1)));
        let telemetry = cameras.source_telemetry();
        assert_eq!(telemetry[0].recovery_successes, 1);
        assert_eq!(telemetry[0].recovery_worker_failures, 0);
        assert_eq!(telemetry[1].lifecycle, LifecycleState::Running);
        assert_eq!(telemetry[1].recovery_attempts, 0);
        assert_eq!(telemetry[1].recovery_worker_failures, 0);
        cameras.shutdown().unwrap();
        assert_helper_processes_reaped(&pid_log);
    }

    #[cfg(all(feature = "native-media", target_os = "macos"))]
    #[test]
    fn camera_shutdown_during_recovery_is_aggregate_bounded_and_reaps_helper() {
        let source_id = deterministic_camera_id(CameraIdKind::Source, "fake-camera");
        let stable_key = format!("macos.avfoundation.camera.v1.{source_id}");
        let project = camera_test_project(&stable_key);
        let directory = tempfile::tempdir().unwrap();
        let helper = hanging_recovery_camera_helper(directory.path());
        let pid_log = PathBuf::from(format!("{}.pids", helper.display()));
        let (_, mut cameras) = resolve_macos_camera_sources_with_policy(
            &project,
            Some(helper.as_path()),
            test_camera_recovery_policy(3, Duration::from_millis(20)),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while cameras.source_telemetry()[0].recovery_attempts == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(cameras.source_telemetry()[0].recovery_attempts, 1);

        let shutdown_started = Instant::now();
        cameras.shutdown().unwrap();
        assert!(shutdown_started.elapsed() < Duration::from_secs(4));
        assert_helper_processes_reaped(&pid_log);
    }

    #[cfg(feature = "native-media")]
    #[test]
    fn native_source_extraction_keeps_asset_resolution_errors_path_free() {
        let store = ProjectStore::new("secret-native-project-marker.freemix").unwrap();
        let error = resolve_native_sources(
            &store,
            &media_test_project("asset://secret-uri-marker/../clip.mkv"),
            None,
        )
        .err()
        .unwrap();
        let message = error.to_string();
        assert!(message.contains("invalid project asset URI"));
        assert!(!message.contains("secret-native-project-marker"));
        assert!(!message.contains("secret-uri-marker"));
    }

    #[cfg(feature = "native-media")]
    #[test]
    fn native_source_extraction_routes_png_to_one_timed_retained_frame() {
        let directory = tempfile::tempdir().unwrap();
        let project_path = directory.path().join("still-project.freemix");
        let assets = project_path.join("assets");
        fs::create_dir_all(&assets).unwrap();
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(&[1, 2, 3, 4], 1, 1, ExtendedColorType::Rgba8)
            .unwrap();
        fs::write(assets.join("still.data"), png).unwrap();

        let sources = resolve_native_sources(
            &ProjectStore::new(&project_path).unwrap(),
            &media_test_project("asset://still.data"),
            None,
        )
        .unwrap()
        .sources;
        assert_eq!(sources.len(), 2);
        for source in sources {
            let NativeResolvedSource::RetainedFrame { frame, .. } = source else {
                panic!("PNG signature must route to retained still decode")
            };
            assert_eq!(
                frame.payload().dimensions(),
                VideoDimensions::new(1, 1).unwrap()
            );
            assert_eq!(frame.payload().plane(0).unwrap().bytes(), &[1, 2, 3, 4]);
            assert_eq!(frame.timing().presentation_timestamp().as_nanos(), 0);
            assert_eq!(frame.timing().duration().as_nanos(), 1);
            assert_eq!(frame.timing().sequence().get(), 0);
            assert_eq!(frame.timing().clock_domain(), native_clock_domain());
        }
    }

    #[cfg(feature = "native-media")]
    #[test]
    fn recognized_corrupt_png_does_not_fall_back_to_ffmpeg() {
        let directory = tempfile::tempdir().unwrap();
        let project_path = directory.path().join("corrupt-still.freemix");
        let assets = project_path.join("assets");
        fs::create_dir_all(&assets).unwrap();
        fs::write(assets.join("bad.bin"), b"\x89PNG\r\n\x1a\ncorrupt").unwrap();

        let error = resolve_native_sources(
            &ProjectStore::new(&project_path).unwrap(),
            &media_test_project("asset://bad.bin"),
            None,
        )
        .err()
        .unwrap();
        let message = error.to_string();
        assert!(message.contains("native still decode failed"));
        assert!(!message.contains("corrupt-still"));
        assert!(!message.contains("bad.bin"));
    }

    #[cfg(feature = "native-media")]
    #[test]
    fn host_pacer_offsets_use_exact_rational_deadlines() {
        let rate = FrameRate::new(30_000, 1_001).unwrap();
        let mut pacer = FramePacer::new(rate, 0);
        assert_eq!(host_deadline_offset(&pacer).unwrap(), Duration::ZERO);
        pacer.advance().unwrap();
        assert_eq!(host_deadline_offset(&pacer).unwrap().as_nanos(), 33_366_666);
        for _ in 1..30_000 {
            pacer.advance().unwrap();
        }
        assert_eq!(host_deadline_offset(&pacer).unwrap().as_secs(), 1_001);
    }

    #[test]
    fn failed_save_aborts_preparation_without_authority_or_output() {
        let mut durable = test_project();
        let initial_engine = restore_engine(&durable).unwrap().snapshot().unwrap();
        let mut control = test_control(&durable);
        let subscription = control.subscribe().unwrap();
        let before_diagnostics = control.diagnostics();
        let before_snapshot = control.snapshot().clone();
        let server = test_server(&control);
        let command = test_command("failed-save", "failed-save-key", CommandPayload::Cut);

        let error = execute_durable_command(
            &mut control,
            &FailingSaver,
            &mut durable,
            &operator(),
            &server,
            &command,
            0,
        )
        .err()
        .expect("save failure must not produce a command result");

        assert_eq!(error.to_string(), "injected save failure");
        assert_eq!(durable, test_project());
        assert_eq!(control.diagnostics(), before_diagnostics);
        assert_eq!(control.snapshot(), &before_snapshot);
        assert_eq!(live_engine_snapshot(&mut control), initial_engine);
        assert_eq!(subscription.try_recv(), Err(TryRecvError::Empty));
        let prepared = control
            .prepare_submit(&operator(), command, 0)
            .unwrap()
            .prepared()
            .expect("failed save must not install a receipt");
        prepared.abort();
    }

    #[test]
    fn failed_manual_position_save_preserves_restart_state_and_retryability() {
        let mut durable = test_project();
        let mut control = test_control(&durable);
        let server = test_server(&control);
        execute_durable_command(
            &mut control,
            &CountingSaver::default(),
            &mut durable,
            &operator(),
            &server,
            &test_command(
                "manual-start",
                "manual-start-key",
                CommandPayload::StartManualTransition {
                    kind: fm_protocol::ManualTransitionKind::Wipe,
                },
            ),
            0,
        )
        .unwrap();
        let before_durable = durable.clone();
        let before_engine = control.idle_engine_snapshot().unwrap();
        let position_command = test_command(
            "manual-position",
            "manual-position-key",
            CommandPayload::SetManualTransitionPosition {
                position: fm_protocol::ManualTransitionPosition::new(6_250).unwrap(),
            },
        );

        let error = execute_durable_command(
            &mut control,
            &FailingSaver,
            &mut durable,
            &operator(),
            &server,
            &position_command,
            0,
        )
        .err()
        .expect("save failure must abort the manual position command");

        assert_eq!(error.to_string(), "injected save failure");
        assert_eq!(durable, before_durable);
        assert_eq!(control.idle_engine_snapshot().unwrap(), before_engine);

        execute_durable_command(
            &mut control,
            &CountingSaver::default(),
            &mut durable,
            &operator(),
            &server,
            &position_command,
            0,
        )
        .unwrap();
        let manual = durable.runtime_manual_transitions();
        assert_eq!(manual.desired.unwrap().position_basis_points, 6_250);
        assert_eq!(manual.realized.unwrap().interval_start_basis_points, 6_250);
        assert_eq!(durable.idempotency_receipts().len(), 2);
    }

    #[test]
    fn save_precedes_commit_ticks_and_returned_result() {
        let mut durable = test_project();
        let mut control = test_control(&durable);
        let subscription = control.subscribe().unwrap();
        let saver = ObservingSaver {
            subscription: &subscription,
            saved: RefCell::new(None),
        };
        let server = test_server(&control);

        let execution = execute_durable_command(
            &mut control,
            &saver,
            &mut durable,
            &operator(),
            &server,
            &test_command(
                "ordered-fade",
                "ordered-fade-key",
                CommandPayload::Fade { duration_frames: 4 },
            ),
            0,
        )
        .unwrap();

        assert!(matches!(
            execution.submission.output.result,
            CommandResult::Accepted { revision: 1, .. }
        ));
        assert_eq!(saver.saved.borrow().as_ref(), Some(&durable));
        assert_eq!(durable.position().frames_rendered, 4);
        assert!(matches!(
            subscription.try_recv().unwrap(),
            LiveEvent::Durable(_)
        ));
        assert!(matches!(
            subscription.try_recv().unwrap(),
            LiveEvent::Runtime(_)
        ));
    }

    #[test]
    fn projected_project_equals_committed_settled_engine() {
        let mut durable = test_project();
        let original_project = durable.project().clone();
        let mut control = test_control(&durable);
        let server = test_server(&control);

        execute_durable_command(
            &mut control,
            &CountingSaver::default(),
            &mut durable,
            &operator(),
            &server,
            &test_command(
                "settled-fade",
                "settled-fade-key",
                CommandPayload::Fade { duration_frames: 7 },
            ),
            0,
        )
        .unwrap();

        let restored = restore_engine(&durable).unwrap().snapshot().unwrap();
        assert_eq!(live_engine_snapshot(&mut control), restored);
        assert_eq!(durable.position().frames_rendered, 7);
        assert_eq!(durable.position().runtime_generation, 1);
        assert_eq!(durable.position().clock_time_nanos, 240_000_000);
        let mut expected_project = original_project;
        expected_project.set_main_mix(durable.project().main_mix().unwrap());
        assert_eq!(durable.project(), &expected_project);
    }

    #[test]
    fn queued_fade_take_next_settles_before_checkpoint() {
        let mut durable = test_project();
        let mut control = test_control(&durable);
        let saver = CountingSaver::default();
        let server = test_server(&control);
        let channel = fm_protocol::WireOverlayChannelId::new(1).unwrap();
        let mut execute = |id, key, payload| {
            execute_durable_command(
                &mut control,
                &saver,
                &mut durable,
                &operator(),
                &server,
                &test_command(id, key, payload),
                0,
            )
        };

        execute(
            "overlay-transition",
            "overlay-transition-key",
            CommandPayload::ConfigureOverlayTransition {
                channel,
                transition: fm_protocol::OverlayTransitionKind::Fade,
                duration_frames: 4,
            },
        )
        .unwrap();
        execute(
            "overlay-queue",
            "overlay-queue-key",
            CommandPayload::QueueOverlay {
                channel,
                source: fm_protocol::WireInputId::from_domain(test_input_id(2)),
            },
        )
        .unwrap();
        let next = execute(
            "overlay-next",
            "overlay-next-key",
            CommandPayload::TakeNextOverlay { channel },
        )
        .unwrap();
        drop(execute);

        assert!(matches!(next.submission.output.result, CommandResult::Accepted { .. }));
        assert_eq!(durable.position().frames_rendered, 6);
        let restored = restore_engine(&durable).unwrap().snapshot().unwrap();
        let live = live_engine_snapshot(&mut control);
        assert_eq!(live, restored);
        let channel = OverlayChannelId::new(channel.number()).unwrap();
        let desired = live.show().desired_switcher().overlay(channel);
        let realized = live.realized_switcher().overlay(channel);
        assert_eq!(desired, realized);
        assert_eq!(desired.source(), Some(test_input_id(2)));
        assert!(desired.is_active());
        assert_eq!(desired.opacity(), u8::MAX);
        assert!(desired.queued_sources().is_empty());
    }

    #[test]
    fn restore_engine_honors_durable_stinger_preload_intent() {
        let baseline = test_project();
        let mut project = baseline.project().clone();
        project.add_stinger(StingerConfig::new(
            StingerSlotNumber::new(8).unwrap(),
            test_input_id(2),
            false,
            11,
            ModelStingerAudioPolicy::MixWithProgram,
            StingerMissingMediaFallback::KeepProgram,
        ));
        project.add_stinger(StingerConfig::new(
            StingerSlotNumber::new(1).unwrap(),
            test_input_id(2),
            true,
            1,
            ModelStingerAudioPolicy::Muted,
            StingerMissingMediaFallback::Cut,
        ));
        let durable = StoredProject::from_project(
            project,
            baseline.runtime_routing(),
            baseline.position(),
            baseline.idempotency_receipts().to_vec(),
        )
        .unwrap();

        let snapshot = restore_engine(&durable).unwrap().snapshot().unwrap();
        let deferred_slot = StingerSlotId::new(8).unwrap();
        let deferred = StingerDescriptor::new(
            test_input_id(2),
            false,
            11,
            StingerAudioPolicy::MixWithProgram,
            MissingMediaFallback::KeepProgram,
        );
        let preloaded_slot = StingerSlotId::new(1).unwrap();
        let preloaded = StingerDescriptor::new(
            test_input_id(2),
            true,
            1,
            StingerAudioPolicy::Muted,
            MissingMediaFallback::Cut,
        );
        for state in [
            snapshot.show().desired_switcher(),
            snapshot.realized_switcher(),
        ] {
            assert_eq!(state.stinger(deferred_slot).descriptor(), Some(&deferred));
            assert_eq!(
                state.stinger(deferred_slot).preload_state(),
                fm_switcher::StingerPreloadState::NotRequested
            );
            assert_eq!(state.stinger(preloaded_slot).descriptor(), Some(&preloaded));
            assert_eq!(
                state.stinger(preloaded_slot).preload_state(),
                fm_switcher::StingerPreloadState::Ready
            );
        }
    }

    #[test]
    fn missing_cut_stinger_uses_one_checkpoint_frame() {
        let baseline = test_project();
        let mut project = baseline.project().clone();
        project.add_stinger(StingerConfig::new(
            StingerSlotNumber::new(1).unwrap(),
            test_input_id(2),
            false,
            1,
            ModelStingerAudioPolicy::Muted,
            StingerMissingMediaFallback::Cut,
        ));
        let mut durable = StoredProject::from_project(
            project,
            baseline.runtime_routing(),
            baseline.position(),
            baseline.idempotency_receipts().to_vec(),
        )
        .unwrap();
        let mut control = test_control(&durable);
        let server = test_server(&control);

        let execution = execute_durable_command(
            &mut control,
            &CountingSaver::default(),
            &mut durable,
            &operator(),
            &server,
            &test_command(
                "missing-cut-stinger",
                "missing-cut-stinger-key",
                CommandPayload::Stinger {
                    slot: fm_protocol::WireStingerSlotId::new(1).unwrap(),
                    duration_frames: 7,
                },
            ),
            0,
        )
        .unwrap();

        assert!(matches!(
            execution.submission.output.result,
            CommandResult::Accepted { .. }
        ));
        assert_eq!(durable.position().frames_rendered, 1);
        assert_eq!(durable.position().runtime_generation, 1);
        let restored = restore_engine(&durable).unwrap().snapshot().unwrap();
        assert_eq!(live_engine_snapshot(&mut control), restored);
        for state in [restored.show().desired_switcher(), restored.realized_switcher()] {
            assert_eq!(
                (state.program(), state.preview()),
                (test_input_id(2), test_input_id(1))
            );
        }
    }

    #[cfg(feature = "native-media")]
    #[test]
    fn native_video_ring_partition_preserves_one_aggregate_gpu_limit() {
        let defaults = NativeSourceLimits::default();
        let (ordinary, stingers) = partition_native_source_limits(1_920, 1_080, 1).unwrap();
        let expected_stinger =
            1_920_u64 * 1_080 * 8 * u64::from(defaults.max_video_frames_per_source.get());

        assert_eq!(stingers.max_retained_rgba16f_bytes, expected_stinger);
        assert_eq!(
            ordinary.max_retained_rgba16f_bytes + stingers.max_retained_rgba16f_bytes,
            defaults.max_retained_rgba16f_bytes
        );
        assert_eq!(
            partition_native_source_limits(1_920, 1_080, 5)
                .unwrap_err()
                .to_string(),
            format!(
                "native Stinger rings require {} retained bytes, exceeding aggregate limit {}",
                expected_stinger * 5,
                defaults.max_retained_rgba16f_bytes
            )
        );
    }

    #[cfg(feature = "native-media")]
    #[test]
    fn native_stinger_retirement_preflight_gate_is_bounded() {
        let retirements = NativeStingerRetirements::start().unwrap();
        let limit = stinger_mutation::retirement_limit_for_test();
        retirements.set_pending_for_test(limit - 1);
        assert!(retirements.can_accept());
        retirements.set_pending_for_test(limit);
        assert!(!retirements.can_accept());
    }

    #[cfg(feature = "native-media")]
    #[test]
    fn native_stinger_source_validation_bounds_preload_kinds_and_audio() {
        let baseline = test_project();
        let mut project = baseline.project().clone();
        project.add_stinger(StingerConfig::new(
            StingerSlotNumber::new(1).unwrap(),
            test_input_id(2),
            true,
            1,
            ModelStingerAudioPolicy::Muted,
            StingerMissingMediaFallback::Cut,
        ));
        let durable = StoredProject::from_project(
            project,
            baseline.runtime_routing(),
            baseline.position(),
            Vec::new(),
        )
        .unwrap();
        let mut source = SimulatedVideoSource::new(
            1,
            1,
            FrameRate::new(25, 1).unwrap(),
            native_clock_domain(),
            SourcePattern::Solid(Rgba8::new(0, 0, 0, 0)),
        )
        .unwrap();
        let retained = NativeResolvedSource::RetainedFrame {
            input: test_input_id(2),
            frame: source.next_frame().unwrap().unwrap(),
        };
        validate_native_stinger_sources(&durable, &[retained]).unwrap();

        let local = NativeResolvedSource::LocalVideo {
            input: test_input_id(2),
            path: PathBuf::from("/secret/stinger.mov"),
        };
        validate_native_stinger_sources(&durable, std::slice::from_ref(&local)).unwrap();
        assert!(stinger_mutation::native_stinger_requires_ffmpeg(
            &durable,
            std::slice::from_ref(&local)
        ));
        assert!(!stinger_mutation::native_stinger_requires_ffmpeg(
            &baseline,
            std::slice::from_ref(&local)
        ));

        let mut audible_project = baseline.project().clone();
        audible_project.add_stinger(StingerConfig::new(
            StingerSlotNumber::new(1).unwrap(),
            test_input_id(2),
            true,
            1,
            ModelStingerAudioPolicy::StingerOnly,
            StingerMissingMediaFallback::Cut,
        ));
        let audible = StoredProject::from_project(
            audible_project,
            baseline.runtime_routing(),
            baseline.position(),
            Vec::new(),
        )
        .unwrap();
        validate_native_stinger_sources(
            &audible,
            &[NativeResolvedSource::LocalVideo {
                input: test_input_id(2),
                path: PathBuf::from("/secret/stinger.mov"),
            }],
        )
        .unwrap();

        let error = validate_native_stinger_sources(
            &durable,
            &[NativeResolvedSource::LiveFrame {
                input: test_input_id(2),
                frame: source.next_frame().unwrap().unwrap(),
            }],
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("is live and cannot be preloaded"));

        let mut deferred_project = baseline.project().clone();
        deferred_project.add_stinger(StingerConfig::new(
            StingerSlotNumber::new(1).unwrap(),
            test_input_id(2),
            false,
            1,
            ModelStingerAudioPolicy::Muted,
            StingerMissingMediaFallback::Cut,
        ));
        let deferred = StoredProject::from_project(
            deferred_project,
            baseline.runtime_routing(),
            baseline.position(),
            baseline.idempotency_receipts().to_vec(),
        )
        .unwrap();
        validate_native_stinger_sources(&deferred, std::slice::from_ref(&local)).unwrap();
        assert!(!stinger_mutation::native_stinger_requires_ffmpeg(
            &deferred,
            &[local]
        ));
    }

    #[test]
    fn alpha_fade_settles_before_checkpoint_and_restores_exact_routing() {
        let mut durable = test_project();
        let mut control = test_control(&durable);
        let server = test_server(&control);

        execute_durable_command(
            &mut control,
            &CountingSaver::default(),
            &mut durable,
            &operator(),
            &server,
            &test_command(
                "settled-alpha-fade",
                "settled-alpha-fade-key",
                CommandPayload::AlphaFade { duration_frames: 3 },
            ),
            0,
        )
        .unwrap();

        let restored = restore_engine(&durable).unwrap().snapshot().unwrap();
        assert_eq!(live_engine_snapshot(&mut control), restored);
        assert_eq!(durable.position().frames_rendered, 3);
        assert_eq!(durable.position().runtime_generation, 1);
        assert_eq!(
            (
                restored.realized_switcher().program(),
                restored.realized_switcher().preview(),
            ),
            (test_input_id(2), test_input_id(1))
        );
    }

    #[test]
    fn slide_settles_before_checkpoint_and_restores_exact_routing() {
        let mut durable = test_project();
        let mut control = test_control(&durable);
        let server = test_server(&control);

        execute_durable_command(
            &mut control,
            &CountingSaver::default(),
            &mut durable,
            &operator(),
            &server,
            &test_command(
                "settled-slide",
                "settled-slide-key",
                CommandPayload::Slide { duration_frames: 3 },
            ),
            0,
        )
        .unwrap();

        let restored = restore_engine(&durable).unwrap().snapshot().unwrap();
        assert_eq!(live_engine_snapshot(&mut control), restored);
        assert_eq!(durable.position().frames_rendered, 3);
        assert_eq!(durable.position().runtime_generation, 1);
        assert_eq!(
            (
                restored.realized_switcher().program(),
                restored.realized_switcher().preview(),
            ),
            (test_input_id(2), test_input_id(1))
        );
    }

    #[test]
    fn zoom_settles_before_checkpoint_and_restores_exact_routing() {
        let mut durable = test_project();
        let mut control = test_control(&durable);
        let server = test_server(&control);

        execute_durable_command(
            &mut control,
            &CountingSaver::default(),
            &mut durable,
            &operator(),
            &server,
            &test_command(
                "settled-zoom",
                "settled-zoom-key",
                CommandPayload::Zoom { duration_frames: 3 },
            ),
            0,
        )
        .unwrap();

        let restored = restore_engine(&durable).unwrap().snapshot().unwrap();
        assert_eq!(live_engine_snapshot(&mut control), restored);
        assert_eq!(durable.position().frames_rendered, 3);
        assert_eq!(durable.position().runtime_generation, 1);
        assert_eq!(
            (
                restored.realized_switcher().program(),
                restored.realized_switcher().preview(),
            ),
            (test_input_id(2), test_input_id(1))
        );
    }

    #[test]
    fn fade_to_black_commands_persist_exact_settled_restart_state() {
        let mut durable = test_project();
        let mut control = test_control(&durable);
        let server = test_server(&control);

        execute_durable_command(
            &mut control,
            &CountingSaver::default(),
            &mut durable,
            &operator(),
            &server,
            &test_command(
                "fade-to-black",
                "fade-to-black-key",
                CommandPayload::FadeToBlack {
                    active: true,
                    duration_frames: 4,
                },
            ),
            0,
        )
        .unwrap();

        assert_eq!(
            durable.runtime_fade_to_black(),
            RuntimeFadeToBlack {
                desired: PersistedFadeToBlackState::BLACK,
                realized: PersistedFadeToBlackState::BLACK,
            }
        );
        let black = restore_engine(&durable).unwrap().snapshot().unwrap();
        assert!(black.desired_fade_to_black().active);
        assert_eq!(
            black.desired_fade_to_black().position,
            fm_switcher::FadeToBlackPosition::BLACK
        );
        assert_eq!(live_engine_snapshot(&mut control), black);

        execute_durable_command(
            &mut control,
            &CountingSaver::default(),
            &mut durable,
            &operator(),
            &server,
            &test_command(
                "fade-from-black",
                "fade-from-black-key",
                CommandPayload::FadeToBlack {
                    active: false,
                    duration_frames: 3,
                },
            ),
            0,
        )
        .unwrap();

        assert_eq!(
            durable.runtime_fade_to_black(),
            RuntimeFadeToBlack::default()
        );
        let live = restore_engine(&durable).unwrap().snapshot().unwrap();
        assert_eq!(
            live.realized_fade_to_black().position,
            fm_switcher::FadeToBlackPosition::LIVE
        );
        assert_eq!(live_engine_snapshot(&mut control), live);
    }

    #[test]
    fn idle_snapshot_checkpoint_survives_project_store_restart() {
        let root = std::env::temp_dir().join(format!(
            "freemixd-checkpoint-{}-{}.freemix",
            std::process::id(),
            now_millis().unwrap()
        ));
        let mut durable = test_project();
        let mut control = test_control(&durable);
        let server = test_server(&control);
        execute_durable_command(
            &mut control,
            &CountingSaver::default(),
            &mut durable,
            &operator(),
            &server,
            &test_command("checkpoint-cut", "checkpoint-key", CommandPayload::Cut),
            0,
        )
        .unwrap();
        control.tick(&server).unwrap();

        let checkpoint =
            stored_project_checkpoint(&durable, &control.idle_engine_snapshot().unwrap()).unwrap();
        ProjectStore::new(&root).unwrap().save(&checkpoint).unwrap();
        let restarted = ProjectStore::new(&root).unwrap().load().unwrap();

        assert_eq!(restarted.project(), durable.project());
        assert_eq!(
            restarted.project().input_audio_strip(test_input_id(1)),
            durable.project().input_audio_strip(test_input_id(1))
        );
        assert_eq!(restarted.runtime_routing(), durable.runtime_routing());
        assert_eq!(
            restarted.idempotency_receipts(),
            durable.idempotency_receipts()
        );
        assert_eq!(
            restarted.position().frames_rendered,
            durable.position().frames_rendered + 1
        );
        assert_eq!(restarted.position().revision, durable.position().revision);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn audio_strip_command_checkpoints_every_control() {
        let mut durable = test_project();
        let mut control = test_control(&durable);
        let server = test_server(&control);
        execute_durable_command(
            &mut control,
            &CountingSaver::default(),
            &mut durable,
            &operator(),
            &server,
            &test_command(
                "audio-strip",
                "audio-strip-key",
                CommandPayload::SetInputAudioStrip {
                    input: fm_protocol::WireInputId::from_domain(test_input_id(1)),
                    gain_millidb: -9_000,
                    balance_basis_points: 2_500,
                    muted: true,
                    soloed: true,
                    follow_video: false,
                    delay_samples: 1_200,
                },
            ),
            0,
        )
        .unwrap();
        control.tick(&server).unwrap();

        let checkpoint =
            stored_project_checkpoint(&durable, &control.idle_engine_snapshot().unwrap()).unwrap();
        assert_eq!(
            checkpoint.project().input_audio_strip(test_input_id(1)),
            Some(InputAudioStripState {
                gain: InputGainMilliDb::new(-9_000).unwrap(),
                balance: InputBalanceBasisPoints::new(2_500).unwrap(),
                muted: true,
                soloed: true,
                follow_video: false,
                delay_samples: InputDelaySamples::new(1_200).unwrap(),
            })
        );
    }

    #[test]
    fn authorization_denial_is_saved_before_becoming_replayable() {
        let mut durable = test_project();
        let mut control = test_control(&durable);
        let saver = CountingSaver::default();
        let server = test_server(&control);
        let command = test_command("denied", "denied-key", CommandPayload::Cut);

        let denied = execute_durable_command(
            &mut control,
            &saver,
            &mut durable,
            &viewer(),
            &server,
            &command,
            0,
        )
        .unwrap();

        assert!(matches!(
            denied.submission.output.result,
            CommandResult::Rejected { ref code, .. } if code == "permission_denied"
        ));
        assert_eq!(saver.0.get(), 1);
        assert_eq!(durable.idempotency_receipts().len(), 1);
        let replay = execute_durable_command(
            &mut control,
            &saver,
            &mut durable,
            &viewer(),
            &server,
            &command,
            0,
        )
        .unwrap();
        assert!(replay.submission.replayed);
        assert_eq!(saver.0.get(), 1);
        assert!(replay.runtime_events.is_empty());

        let mut restarted = test_control(&durable);
        let restarted_server = test_server(&restarted);
        let mut restarted_durable = durable.clone();
        let replay_after_restart = execute_durable_command(
            &mut restarted,
            &saver,
            &mut restarted_durable,
            &operator(),
            &restarted_server,
            &command,
            0,
        )
        .unwrap();
        assert!(replay_after_restart.submission.replayed);
        assert_eq!(
            replay_after_restart.submission.output.result,
            denied.submission.output.result
        );
        assert_eq!(saver.0.get(), 1);
        assert_eq!(restarted_durable, durable);
    }

    #[test]
    fn crash_after_save_restarts_from_settled_receipt_without_reexecution() {
        let root = std::env::temp_dir().join(format!(
            "freemixd-{}-{}.freemix",
            std::process::id(),
            now_millis().unwrap()
        ));
        let store = ProjectStore::new(&root).unwrap();
        let mut durable = test_project();
        let mut control = test_control(&durable);
        let server = test_server(&control);
        let command = test_command(
            "crash-fade",
            "crash-fade-key",
            CommandPayload::Fade { duration_frames: 4 },
        );

        let crashed = catch_unwind(AssertUnwindSafe(|| {
            let _ = execute_durable_command(
                &mut control,
                &CrashAfterSave(&store),
                &mut durable,
                &operator(),
                &server,
                &command,
                0,
            );
        }));
        assert!(crashed.is_err());
        assert_eq!(control.diagnostics().current_revision, 0);
        assert_eq!(durable.position().revision, 0);

        let saved = store.load().unwrap();
        assert_eq!(saved.position().revision, 1);
        assert_eq!(saved.position().frames_rendered, 4);
        assert_eq!(saved.idempotency_receipts().len(), 1);
        let mut restarted = test_control(&saved);
        let restarted_server = test_server(&restarted);
        let save_counter = CountingSaver::default();
        let mut restarted_durable = saved.clone();
        let replay = execute_durable_command(
            &mut restarted,
            &save_counter,
            &mut restarted_durable,
            &operator(),
            &restarted_server,
            &command,
            0,
        )
        .unwrap();
        assert!(replay.submission.replayed);
        assert!(replay.runtime_events.is_empty());
        assert_eq!(save_counter.0.get(), 0);
        assert_eq!(restarted_durable, saved);

        fs::remove_dir_all(root).unwrap();
    }

    fn test_project() -> StoredProject {
        let frame_rate = FrameRate::new(25, 1).unwrap();
        let mut project = Project::new(
            ProjectId::new(NonZeroU128::new(42).unwrap()),
            "Unit Test",
            ProjectSettings {
                frame_rate,
                video: VideoFormat {
                    dimensions: VideoDimensions::new(1_280, 720).unwrap(),
                    frame_rate,
                    pixel_format: PixelFormat::Rgba8,
                    scan: ScanMode::Progressive,
                    color: ColorMetadata::default(),
                },
                audio: AudioFormat {
                    sample_rate: SampleRate::new(44_100).unwrap(),
                    sample_format: SampleFormat::I16,
                    channels: ChannelLayout::stereo(),
                },
            },
        );
        project.add_input(Input {
            id: test_input_id(1),
            name: "Custom solid and tone".into(),
            kind: InputKind::Simulated(SimulatedInput::new(
                SimulatedVideo::Solid(SolidColor::new(7, 11, 13, 255)),
                SimulatedAudio::Sine { frequency_hz: 997 },
            )),
            required_capabilities: vec!["simulation.custom".into()],
        });
        project.add_input(Input {
            id: test_input_id(2),
            name: "Bars".into(),
            kind: InputKind::Simulated(SimulatedInput::new(
                SimulatedVideo::Bars,
                SimulatedAudio::Silence,
            )),
            required_capabilities: Vec::new(),
        });
        assert!(project.set_input_audio_strip(
            test_input_id(1),
            InputAudioStripState {
                gain: InputGainMilliDb::new(-3_000).unwrap(),
                balance: InputBalanceBasisPoints::CENTER,
                delay_samples: Default::default(),
                muted: false,
                soloed: false,
                follow_video: true,
            },
        ));
        project.set_main_mix(MainMix::new(test_input_id(1), test_input_id(2)));
        StoredProject::from_project(
            project,
            RuntimeRouting {
                desired_program_id: Some(test_input_id(1)),
                realized_program_id: Some(test_input_id(1)),
                desired_preview_id: Some(test_input_id(2)),
                realized_preview_id: Some(test_input_id(2)),
            },
            ProjectPosition::default(),
            Vec::new(),
        )
        .unwrap()
    }

    #[cfg(feature = "native-media")]
    fn media_test_project(asset_uri: &str) -> StoredProject {
        let baseline = test_project();
        let mut project = Project::new(
            baseline.project().id(),
            "Media Unit Test",
            baseline.project().settings().clone(),
        );
        for value in [1, 2] {
            project.add_input(Input {
                id: test_input_id(value),
                name: format!("Media {value}"),
                kind: InputKind::Media {
                    asset_uri: asset_uri.into(),
                },
                required_capabilities: Vec::new(),
            });
        }
        project.set_main_mix(MainMix::new(test_input_id(1), test_input_id(2)));
        StoredProject::from_project(
            project,
            RuntimeRouting {
                desired_program_id: Some(test_input_id(1)),
                realized_program_id: Some(test_input_id(1)),
                desired_preview_id: Some(test_input_id(2)),
                realized_preview_id: Some(test_input_id(2)),
            },
            ProjectPosition::default(),
            Vec::new(),
        )
        .unwrap()
    }

    #[cfg(feature = "native-media")]
    fn scene_test_project() -> StoredProject {
        let baseline = test_project();
        let mut project = Project::new(
            baseline.project().id(),
            "Scene Unit Test",
            baseline.project().settings().clone(),
        );
        let scene_id = SceneId::new(NonZeroU128::new(1).unwrap());
        project.add_input(Input {
            id: test_input_id(1),
            name: "Scene".into(),
            kind: InputKind::Scene {
                scene_id,
                audio_source: Some(test_input_id(2)),
            },
            required_capabilities: Vec::new(),
        });
        project.add_input(Input {
            id: test_input_id(2),
            name: "Audio".into(),
            kind: InputKind::Simulated(SimulatedInput::new(
                SimulatedVideo::Bars,
                SimulatedAudio::Silence,
            )),
            required_capabilities: Vec::new(),
        });
        project.add_scene(Scene {
            id: scene_id,
            name: "Program".into(),
            background: ModelRgba8::OPAQUE_BLACK,
            layers: Vec::new(),
        });
        project.set_main_mix(MainMix::new(test_input_id(1), test_input_id(2)));
        StoredProject::from_project(
            project,
            RuntimeRouting {
                desired_program_id: Some(test_input_id(1)),
                realized_program_id: Some(test_input_id(1)),
                desired_preview_id: Some(test_input_id(2)),
                realized_preview_id: Some(test_input_id(2)),
            },
            ProjectPosition::default(),
            Vec::new(),
        )
        .unwrap()
    }

    fn test_input_id(value: u128) -> InputId {
        InputId::new(NonZeroU128::new(value).unwrap())
    }

    #[cfg(feature = "native-media")]
    #[test]
    fn native_stinger_preflight_rejections_are_path_free_and_retryable_by_resubmission() {
        let slot = fm_protocol::WireStingerSlotId::new(8).unwrap();
        for payload in [
            CommandPayload::ConfigureStinger {
                slot,
                media_input: fm_protocol::WireInputId::from_domain(test_input_id(2)),
                preload: true,
                cut_point_frames: 3,
                audio_policy: fm_protocol::StingerAudioPolicy::Muted,
                missing_media_fallback: fm_protocol::StingerMissingMediaFallback::Cut,
            },
            CommandPayload::RemoveStinger { slot },
        ] {
            let result = native_stinger_preflight_rejection(
                &test_command("native", "native-key", payload),
                7,
            );
            assert!(matches!(
                result,
                CommandResult::Rejected {
                    ref code,
                    ref message,
                    current_revision: 7,
                    retryable: false,
                    ..
                } if code == "unavailable"
                    && message == "native Stinger resources could not be prepared"
            ));
        }
    }

    fn test_control(project: &StoredProject) -> ControlService<Policy> {
        ControlService::new(
            restore_engine(project).unwrap(),
            Policy::development(),
            "unit-engine",
            "unit-log",
            ControlLimits::default(),
        )
    }

    fn test_server(control: &ControlService<Policy>) -> ServerIdentity {
        let engine = control.diagnostics().engine;
        ServerIdentity {
            engine_id: engine.engine_id,
            project_id: "42".into(),
            state_epoch: engine.state_epoch,
            log_id: engine.log_id,
        }
    }

    fn test_command(id: &str, key: &str, payload: CommandPayload) -> CommandMessage {
        CommandMessage {
            protocol: PROTOCOL_VERSION,
            id: id.into(),
            idempotency_key: key.into(),
            expected_revision: None,
            deadline_ms: None,
            payload,
        }
    }

    fn operator() -> Principal {
        development_principal().unwrap()
    }

    fn viewer() -> Principal {
        Principal::authenticated(
            UserId::new("unit-viewer").unwrap(),
            SessionId::new("unit-session").unwrap(),
            [AuthRole::Viewer],
        )
    }

    fn live_engine_snapshot(control: &mut ControlService<Policy>) -> EngineSnapshot {
        let prepared = control
            .prepare_submit(
                &viewer(),
                test_command("snapshot-probe", "snapshot-probe-key", CommandPayload::Cut),
                0,
            )
            .unwrap()
            .prepared()
            .unwrap();
        let snapshot = prepared.project(0).unwrap();
        prepared.abort();
        snapshot
    }
}
