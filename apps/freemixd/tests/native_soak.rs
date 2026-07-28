#![cfg(feature = "native-media")]

use std::{
    collections::BTreeMap,
    io::{self, BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    num::NonZeroU128,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use fm_model::{
    Input, InputKind, MainMix, Project, ProjectSettings, SimulatedAudio, SimulatedInput,
    SimulatedVideo, SolidColor,
};
use fm_persistence::{
    ProjectPosition, ProjectStore, ReceiptOutcome, RuntimeRouting, StoredProject,
};
use fm_protocol::{
    ClientHello, ClientType, CommandMessage, CommandPayload, CommandResult, EngineIdentity,
    EventMessage, EventPayload, ProtocolVersion, Role, RuntimeEventMessage, RuntimeLifecycleEvent,
    ServerIdentity, SnapshotMessage, WireInputId, WireMessage, decode_line, encode_line,
};
use fm_types::{
    AudioFormat, ChannelLayout, ColorMetadata, FrameRate, InputId, PixelFormat, ProjectId,
    SampleFormat, SampleRate, ScanMode, VideoDimensions, VideoFormat,
};
use freemixd::ReadinessRecord;

const DEFAULT_SOAK_SECONDS: u64 = 60;
const MIN_SOAK_SECONDS: u64 = 3;
const MAX_SOAK_SECONDS: u64 = 86_400;
const OUTPUT_LIMIT: usize = 256 * 1024;
const READY_LINE_LIMIT: usize = 4 * 1024;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const EXIT_MARGIN: Duration = Duration::from_secs(15);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const COMMAND_PERIOD: Duration = Duration::from_secs(1);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const PROTOCOL_LINE_LIMIT: usize = 64 * 1024;
const FRAME_RATE: u64 = 25;
const RESTART_SECONDS: u64 = 3;
const PROJECT_ID: u128 = 72_002;

#[derive(Default)]
struct DrainedOutput {
    bytes: Vec<u8>,
    exceeded: bool,
}

struct ProcessOutput {
    status: ExitStatus,
    stdout: DrainedOutput,
    stderr: DrainedOutput,
}

struct CapturedChild {
    child: Option<Child>,
    stdout: Option<mpsc::Receiver<io::Result<DrainedOutput>>>,
    stderr: Option<mpsc::Receiver<io::Result<DrainedOutput>>>,
}

impl CapturedChild {
    fn start(mut child: Child) -> (Self, mpsc::Receiver<Result<String, String>>) {
        let stdout = child.stdout.take().expect("daemon stdout must be piped");
        let stderr = child.stderr.take().expect("daemon stderr must be piped");
        let (stdout_receiver, readiness) = spawn_stdout_drain(stdout);
        let stderr_receiver = spawn_drain(stderr);
        (
            Self {
                child: Some(child),
                stdout: Some(stdout_receiver),
                stderr: Some(stderr_receiver),
            },
            readiness,
        )
    }

    fn wait_until(mut self, deadline: Instant) -> Result<ProcessOutput, String> {
        loop {
            match self
                .child
                .as_mut()
                .expect("captured child is present")
                .try_wait()
            {
                Ok(Some(status)) => return self.collect(status),
                Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
                Ok(None) => {
                    let output = self.kill_and_collect()?;
                    return Err(format!(
                        "daemon exceeded cooperative exit deadline; {}",
                        output_diagnostic(&output)
                    ));
                }
                Err(error) => {
                    let cleanup = self.kill_and_collect();
                    return Err(match cleanup {
                        Ok(output) => format!(
                            "cannot poll daemon: {error}; {}",
                            output_diagnostic(&output)
                        ),
                        Err(cleanup) => {
                            format!("cannot poll daemon: {error}; cleanup failed: {cleanup}")
                        }
                    });
                }
            }
        }
    }

    fn stop_for_startup_failure(mut self, reason: &str) -> String {
        match self.kill_and_collect() {
            Ok(output) => format!("{reason}; {}", output_diagnostic(&output)),
            Err(error) => format!("{reason}; cleanup failed: {error}"),
        }
    }

    fn kill_and_collect(&mut self) -> Result<ProcessOutput, String> {
        let mut child = self.child.take().expect("captured child is present");
        let status = match kill_and_reap(&mut child, CLEANUP_TIMEOUT) {
            Ok(status) => status,
            Err(error) => {
                self.child = Some(child);
                return Err(error);
            }
        };
        self.collect(status)
    }

    fn collect(&mut self, status: ExitStatus) -> Result<ProcessOutput, String> {
        self.child.take();
        let stdout_receiver = self.stdout.take().expect("stdout drain must be present");
        let stderr_receiver = self.stderr.take().expect("stderr drain must be present");
        let stdout = receive_drain(&stdout_receiver, "stdout")?;
        let stderr = receive_drain(&stderr_receiver, "stderr")?;
        Ok(ProcessOutput {
            status,
            stdout,
            stderr,
        })
    }
}

impl Drop for CapturedChild {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if let Err(error) = kill_and_reap(&mut child, CLEANUP_TIMEOUT) {
            eprintln!("PHASE2_NATIVE_SOAK cleanup failure: {error}");
        }
    }
}

fn kill_and_reap(child: &mut Child, timeout: Duration) -> Result<ExitStatus, String> {
    if let Some(status) = child
        .try_wait()
        .map_err(|error| format!("cannot poll daemon during cleanup: {error}"))?
    {
        return Ok(status);
    }
    let kill_error = child.kill().err();
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| "cleanup deadline overflow".to_owned())?;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                let detail = kill_error
                    .map_or_else(String::new, |error| format!("; kill also failed: {error}"));
                return Err(format!(
                    "could not confirm daemon reap within {timeout:?}{detail}"
                ));
            }
            Err(error) => {
                return Err(format!("cannot poll killed daemon for reap: {error}"));
            }
        }
    }
}

struct NativeProcess {
    process: CapturedChild,
    address: SocketAddr,
    ready_at: Instant,
}

impl NativeProcess {
    fn start(project: &Path, seconds: u64) -> Result<Self, String> {
        let child = Command::new(env!("CARGO_BIN_EXE_freemixd"))
            .arg("serve")
            .arg(project)
            .arg("--native-media")
            .arg("--diagnostic-stop-after")
            .arg(format!("{seconds}s"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("cannot spawn freemixd: {error}"))?;
        let (process, readiness) = CapturedChild::start(child);
        let line = match readiness.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok(line)) => line,
            Ok(Err(error)) => return Err(process.stop_for_startup_failure(&error)),
            Err(error) => {
                return Err(process.stop_for_startup_failure(&format!(
                    "timed out waiting for FREEMIXD_READY: {error}"
                )));
            }
        };
        let ready_at = Instant::now();
        let readiness = match line.parse::<ReadinessRecord>() {
            Ok(readiness) => readiness,
            Err(error) => {
                return Err(process.stop_for_startup_failure(&format!(
                    "invalid FREEMIXD_READY record {line:?}: {error}"
                )));
            }
        };
        Ok(Self {
            process,
            address: readiness.address,
            ready_at,
        })
    }
}

struct SoakClient {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl SoakClient {
    fn connect(address: SocketAddr) -> Result<Self, String> {
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let stream = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!("timed out connecting to ready daemon at {address}"));
            }
            match TcpStream::connect_timeout(&address, remaining.min(Duration::from_millis(100))) {
                Ok(stream) => break stream,
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    thread::sleep(POLL_INTERVAL);
                }
                Err(error) => return Err(format!("cannot connect to ready daemon: {error}")),
            }
        };
        stream
            .set_nodelay(true)
            .map_err(|error| format!("cannot configure client socket: {error}"))?;
        stream
            .set_write_timeout(Some(COMMAND_TIMEOUT))
            .map_err(|error| format!("cannot set client write timeout: {error}"))?;
        Ok(Self {
            writer: stream
                .try_clone()
                .map_err(|error| format!("cannot clone client socket: {error}"))?,
            reader: BufReader::new(stream),
        })
    }

    fn handshake(&mut self) -> Result<HandshakeState, String> {
        self.send(&WireMessage::ClientHello(ClientHello {
            versions: vec![ProtocolVersion::new(1, 0)],
            build: "phase2-native-soak-v1".into(),
            client_type: ClientType::Integration,
            desired_role: Role::Operator,
            cached_cursor: None,
        }))?;
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let hello = self.receive_before(deadline, "ServerHello")?;
        let WireMessage::ServerHello(hello) = hello else {
            return Err("expected ServerHello after legacy ClientHello".into());
        };
        if hello.negotiated != ProtocolVersion::new(1, 0) || hello.granted_role != Role::Operator {
            return Err(format!(
                "unexpected handshake negotiation: version={:?}, role={:?}",
                hello.negotiated, hello.granted_role
            ));
        }
        let snapshot = self.receive_before(deadline, "Snapshot")?;
        let WireMessage::Snapshot(snapshot) = snapshot else {
            return Err("expected Snapshot after ServerHello".into());
        };
        if hello.engine != snapshot.engine || hello.current_revision != snapshot.revision {
            return Err(format!(
                "handshake identity/revision disagrees with snapshot: hello={hello:?}, snapshot={snapshot:?}"
            ));
        }
        let server = ServerIdentity {
            engine_id: hello.engine.engine_id.clone(),
            project_id: project_id().to_string(),
            state_epoch: hello.engine.state_epoch,
            log_id: hello.engine.log_id.clone(),
        };
        Ok(HandshakeState { snapshot, server })
    }

    fn command(
        &mut self,
        sequence: u64,
        payload: CommandPayload,
        server: &ServerIdentity,
        expected_program: InputId,
        expected_preview: InputId,
    ) -> Result<CommandCompletion, String> {
        let id = format!("phase2-native-soak-command-{sequence}");
        let key = format!("phase2-native-soak-key-{sequence}");
        self.send(&WireMessage::Command(CommandMessage {
            protocol: ProtocolVersion::new(1, 0),
            id: id.clone(),
            idempotency_key: key,
            expected_revision: None,
            deadline_ms: None,
            payload,
        }))?;

        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let mut accepted = None;
        let mut durable_seen = false;
        let mut realized_seen = false;
        for _ in 0..4_096 {
            match self.receive_before(deadline, "command result, durable event, and realization")? {
                WireMessage::CommandResult(CommandResult::Accepted {
                    id: result_id,
                    revision,
                    scheduled_frame: Some(frame),
                }) if result_id == id => {
                    if revision != sequence {
                        return Err(format!(
                            "command {id} completed at revision {revision}, expected {sequence}"
                        ));
                    }
                    if accepted.replace(frame).is_some() {
                        return Err(format!("command {id} received duplicate accepted results"));
                    }
                }
                WireMessage::CommandResult(CommandResult::Accepted {
                    id: result_id,
                    scheduled_frame: None,
                    ..
                }) if result_id == id => {
                    return Err(format!(
                        "command {id} was accepted without a scheduled frame"
                    ));
                }
                WireMessage::CommandResult(CommandResult::Rejected {
                    id: result_id,
                    code,
                    message,
                    ..
                }) if result_id == id => {
                    return Err(format!("command {id} was rejected ({code}): {message}"));
                }
                WireMessage::Event(event) if event.cursor.revision == sequence => {
                    validate_durable_event(
                        &event,
                        server,
                        expected_program,
                        expected_preview,
                        &id,
                    )?;
                    if std::mem::replace(&mut durable_seen, true) {
                        return Err(format!("command {id} received duplicate durable events"));
                    }
                }
                WireMessage::RuntimeEvent(event) if event.revision == sequence => {
                    validate_runtime_event(&event, server, sequence, &id)?;
                    if std::mem::replace(&mut realized_seen, true) {
                        return Err(format!("command {id} received duplicate runtime events"));
                    }
                }
                WireMessage::Error(error) => {
                    return Err(format!(
                        "protocol error while waiting for {id}: {}: {}",
                        error.error.code, error.error.message
                    ));
                }
                _ => {}
            }
            if let Some(scheduled_frame) = accepted
                && durable_seen
                && realized_seen
            {
                return Ok(CommandCompletion { scheduled_frame });
            }
        }
        Err(format!(
            "too many interleaved messages while waiting for {id}"
        ))
    }

    fn send(&mut self, message: &WireMessage) -> Result<(), String> {
        let line =
            encode_line(message).map_err(|error| format!("cannot encode message: {error}"))?;
        self.writer
            .write_all(line.as_bytes())
            .and_then(|()| self.writer.flush())
            .map_err(|error| format!("cannot write protocol message: {error}"))
    }

    fn receive_before(&mut self, deadline: Instant, expected: &str) -> Result<WireMessage, String> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!("timed out waiting for {expected}"));
        }
        self.reader
            .get_mut()
            .set_read_timeout(Some(remaining))
            .map_err(|error| format!("cannot set client read timeout: {error}"))?;
        let Some(line) = read_protocol_line(&mut self.reader)
            .map_err(|error| format!("cannot read {expected}: {error}"))?
        else {
            return Err(format!(
                "daemon closed the connection while waiting for {expected}"
            ));
        };
        decode_line(&line).map_err(|error| format!("cannot decode {expected}: {error}"))
    }
}

fn validate_durable_event(
    event: &EventMessage,
    server: &ServerIdentity,
    expected_program: InputId,
    expected_preview: InputId,
    command_id: &str,
) -> Result<(), String> {
    if event.cursor.engine.engine_id != server.engine_id
        || event.cursor.engine.state_epoch != server.state_epoch
        || event.cursor.engine.log_id != server.log_id
        || event.payload
            != (EventPayload::DesiredSwitcher {
                program: WireInputId::from_domain(expected_program),
                preview: WireInputId::from_domain(expected_preview),
                manual_transition: None,
            })
    {
        return Err(format!(
            "command {command_id} received an invalid durable event: {event:?}"
        ));
    }
    Ok(())
}

fn validate_runtime_event(
    event: &RuntimeEventMessage,
    server: &ServerIdentity,
    revision: u64,
    command_id: &str,
) -> Result<(), String> {
    if event.server != *server
        || event.server.project_id != project_id().to_string()
        || event.generation != revision
        || event.sequence != 1
        || !matches!(
            event.event,
            RuntimeLifecycleEvent::Realized {
                ref domain,
                manual_transition: None,
            } if domain == "switcher"
        )
    {
        return Err(format!(
            "command {command_id} received an invalid runtime event: {event:?}"
        ));
    }
    Ok(())
}

struct HandshakeState {
    snapshot: SnapshotMessage,
    server: ServerIdentity,
}

#[derive(Clone, Copy, Debug)]
struct CommandCompletion {
    scheduled_frame: u64,
}

fn read_protocol_line(reader: &mut impl BufRead) -> Result<Option<String>, String> {
    let mut line = Vec::new();
    loop {
        let (consumed, complete) = {
            let available = reader
                .fill_buf()
                .map_err(|error| format!("socket read failed: {error}"))?;
            if available.is_empty() {
                if line.is_empty() {
                    return Ok(None);
                }
                return Err("protocol line ended at EOF without a newline".into());
            }
            let consumed = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |index| index + 1);
            let new_len = line
                .len()
                .checked_add(consumed)
                .ok_or_else(|| "protocol line length overflow".to_owned())?;
            if new_len > PROTOCOL_LINE_LIMIT {
                return Err(format!(
                    "protocol line exceeded {PROTOCOL_LINE_LIMIT} bytes"
                ));
            }
            line.extend_from_slice(&available[..consumed]);
            (consumed, available[consumed - 1] == b'\n')
        };
        reader.consume(consumed);
        if complete {
            return String::from_utf8(line)
                .map(Some)
                .map_err(|error| format!("protocol line was not UTF-8: {error}"));
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct Telemetry {
    host: MetricPopulation,
    audio: AudioTelemetry,
    camera: CameraTelemetry,
    presentation: PresentationTelemetry,
    recorder: RecorderTelemetry,
    gpu: GpuTelemetry,
    metric_errors: u64,
    metric_samples_dropped: u64,
}

#[derive(Debug, Eq, PartialEq)]
struct CameraTelemetry {
    configured_sources: u64,
    frames_received: u64,
    frames_ingested: u64,
    native_dropped: u64,
    queue_depth: u64,
    queue_peak_depth: u64,
    queue_dropped: u64,
}

#[derive(Debug, Eq, PartialEq)]
struct MetricPopulation {
    total: u64,
    retained: u64,
    dropped: u64,
}

#[derive(Debug, Eq, PartialEq)]
struct AudioTelemetry {
    retained_blocks: u64,
    peak_retained_blocks: u64,
    retained_samples: u64,
    peak_retained_samples: u64,
    retained_bytes: u64,
    peak_retained_bytes: u64,
    reservation_requests: u64,
    reserved_blocks: u64,
    peak_reserved_blocks: u64,
    reserved_samples: u64,
    peak_reserved_samples: u64,
    reserved_bytes: u64,
    peak_reserved_bytes: u64,
    source_stalls: u64,
    positioned_blocks: u64,
    positioned_samples: u64,
    leading_silence_samples: u64,
    eos_padding_blocks: u64,
    eos_padding_samples: u64,
    sink_depth: u64,
    sink_peak_depth: u64,
    sink_dropped: u64,
}

#[derive(Debug, Eq, PartialEq)]
struct PresentationTelemetry {
    active: bool,
    pending_depth: u64,
    peak_pending_depth: u64,
    dropped: u64,
}

#[derive(Debug, Eq, PartialEq)]
struct RecorderTelemetry {
    configured: bool,
    outstanding_pairs: u64,
    peak_outstanding_pairs: u64,
    retained_bytes: u64,
    peak_retained_bytes: u64,
}

#[derive(Debug, Eq, PartialEq)]
struct GpuTelemetry {
    backend: String,
    adapter: String,
    timing: String,
    passes: MetricPopulation,
    pending: u64,
    dropped: u64,
    unavailable: u64,
}

struct SoakResult {
    requested: Duration,
    observed: Duration,
    expected_requested_frames: u64,
    expected_min_frames: u64,
    receipts: Vec<ExpectedReceipt>,
    frames: u64,
    engine: EngineIdentity,
    telemetry: Telemetry,
}

#[derive(Clone, Copy, Debug)]
struct ExpectedReceipt {
    sequence: u64,
    target_frame: u64,
}

#[test]
#[ignore = "Phase-2 native diagnostic soak; not Basic Show certification"]
fn phase2_native_diagnostic_soak() {
    if let Err(error) = run_soak() {
        panic!("Phase-2 native diagnostic soak failed: {error}");
    }
}

fn run_soak() -> Result<(), String> {
    let seconds = soak_seconds_from_env()?;
    let require_native = required_flag("FM_REQUIRE_NATIVE_MEDIA")?;
    let require_gpu_timing = required_flag("FM_REQUIRE_GPU_TIMING")?;
    let directory =
        tempfile::tempdir().map_err(|error| format!("cannot create tempdir: {error}"))?;
    let project_path = directory.path().join("phase2-native-soak.freemix");
    save_project(&project_path)?;

    let daemon = match NativeProcess::start(&project_path, seconds) {
        Ok(daemon) => daemon,
        Err(error)
            if !require_native && !require_gpu_timing && native_startup_unavailable(&error) =>
        {
            eprintln!("PHASE2_NATIVE_SOAK skipped: native-media startup unavailable; {error}");
            return Ok(());
        }
        Err(error) => return Err(format!("required native-media startup failed: {error}")),
    };
    let result = run_initial_phase(daemon, &project_path, seconds, require_gpu_timing)?;
    let post_restart_frames = verify_native_restart(&project_path, &result, require_gpu_timing)?;
    print_report(&result, post_restart_frames);
    Ok(())
}

fn run_initial_phase(
    daemon: NativeProcess,
    project_path: &Path,
    seconds: u64,
    require_gpu_timing: bool,
) -> Result<SoakResult, String> {
    let requested = Duration::from_secs(seconds);
    let command_window = requested
        .checked_sub(Duration::from_secs(1))
        .ok_or_else(|| "command window underflow".to_owned())?;
    let stop_commands_at = daemon
        .ready_at
        .checked_add(command_window)
        .ok_or_else(|| "command deadline overflow".to_owned())?;
    let mut client = SoakClient::connect(daemon.address)?;
    let handshake = client.handshake()?;
    assert_snapshot(&handshake.snapshot, 0)?;
    let engine = handshake.snapshot.engine.clone();
    let receipts = run_commands(&mut client, &handshake.server, stop_commands_at)?;
    drop(client);

    let (observed, telemetry) = finish_process(daemon, requested)?;
    let (expected_requested_frames, expected_min_frames) = expected_frame_counts(seconds)?;
    validate_telemetry(&telemetry, expected_min_frames, require_gpu_timing)?;
    let persisted = load_project(project_path)?;
    validate_checkpoint(&persisted, &receipts, expected_min_frames)?;
    Ok(SoakResult {
        requested,
        observed,
        expected_requested_frames,
        expected_min_frames,
        receipts,
        frames: persisted.position().frames_rendered,
        engine,
        telemetry,
    })
}

fn run_commands(
    client: &mut SoakClient,
    server: &ServerIdentity,
    stop_commands_at: Instant,
) -> Result<Vec<ExpectedReceipt>, String> {
    let mut receipts = Vec::new();
    let mut previous_frame = 0_u64;
    let mut next_command_at = Instant::now();
    loop {
        if Instant::now() >= stop_commands_at {
            break;
        }
        let completed = u64::try_from(receipts.len())
            .map_err(|_| "completed command count does not fit u64".to_owned())?;
        let payload = if completed.is_multiple_of(2) {
            CommandPayload::Cut
        } else {
            CommandPayload::Fade { duration_frames: 4 }
        };
        let sequence = completed
            .checked_add(1)
            .ok_or_else(|| "command sequence overflow".to_owned())?;
        let (program, preview) = expected_routing(sequence);
        let completion = client.command(sequence, payload, server, program, preview)?;
        if completion.scheduled_frame <= previous_frame {
            return Err(format!(
                "command {sequence} scheduled frame {} did not increase past {previous_frame}",
                completion.scheduled_frame
            ));
        }
        previous_frame = completion.scheduled_frame;
        receipts.push(ExpectedReceipt {
            sequence,
            target_frame: completion.scheduled_frame,
        });
        next_command_at = next_command_at
            .checked_add(COMMAND_PERIOD)
            .ok_or_else(|| "command cadence overflow".to_owned())?;
        let now = Instant::now();
        if next_command_at >= stop_commands_at {
            break;
        }
        thread::sleep(next_command_at.saturating_duration_since(now));
    }
    if receipts.is_empty() {
        return Err("diagnostic command window closed before any command completed".into());
    }
    Ok(receipts)
}

fn finish_process(
    daemon: NativeProcess,
    requested: Duration,
) -> Result<(Duration, Telemetry), String> {
    let ready_at = daemon.ready_at;
    let exit_window = requested
        .checked_add(EXIT_MARGIN)
        .ok_or_else(|| "process exit window overflow".to_owned())?;
    let process_deadline = ready_at
        .checked_add(exit_window)
        .ok_or_else(|| "process deadline overflow".to_owned())?;
    let output = daemon.process.wait_until(process_deadline)?;
    let observed = ready_at.elapsed();
    if !output.status.success() {
        return Err(format!(
            "daemon exited unsuccessfully; {}",
            output_diagnostic(&output)
        ));
    }
    ensure_bounded(&output)?;
    if observed + Duration::from_millis(250) < requested {
        return Err(format!(
            "daemon exited before diagnostic deadline: requested={requested:?}, observed={observed:?}"
        ));
    }

    let stderr = String::from_utf8(output.stderr.bytes)
        .map_err(|error| format!("daemon stderr was not UTF-8: {error}"))?;
    if stderr.contains("FREEMIXD_CAMERA_SOURCE\t") {
        return Err("camera-free native soak emitted camera source telemetry".to_owned());
    }
    let telemetry = parse_telemetry(&stderr)?;
    Ok((observed, telemetry))
}

fn verify_native_restart(
    project_path: &Path,
    initial: &SoakResult,
    require_gpu_timing: bool,
) -> Result<u64, String> {
    let daemon = NativeProcess::start(project_path, RESTART_SECONDS)
        .map_err(|error| format!("native restart startup failed: {error}"))?;
    let mut client = SoakClient::connect(daemon.address)?;
    let handshake = client.handshake()?;
    let revision = completed_commands(&initial.receipts)?;
    assert_snapshot(&handshake.snapshot, revision)?;
    if handshake.snapshot.engine != initial.engine {
        return Err(format!(
            "engine identity changed across restart: before={:?}, after={:?}",
            initial.engine, handshake.snapshot.engine
        ));
    }
    drop(client);

    let (_, telemetry) = finish_process(daemon, Duration::from_secs(RESTART_SECONDS))?;
    let (_, restart_minimum) = expected_frame_counts(RESTART_SECONDS)?;
    validate_telemetry(&telemetry, restart_minimum, require_gpu_timing)?;
    let persisted = load_project(project_path)?;
    let minimum_total = initial
        .frames
        .checked_add(restart_minimum)
        .ok_or_else(|| "restart frame threshold overflow".to_owned())?;
    validate_checkpoint(&persisted, &initial.receipts, minimum_total)?;
    Ok(persisted.position().frames_rendered)
}

fn load_project(project_path: &Path) -> Result<StoredProject, String> {
    ProjectStore::new(project_path)
        .map_err(|error| format!("cannot reopen project store: {error}"))?
        .load()
        .map_err(|error| format!("cannot reload project: {error}"))
}

fn print_report(result: &SoakResult, post_restart_frames: u64) {
    println!(
        "PHASE2_NATIVE_SOAK\tv=1\tclassification=diagnostic-not-certification\tbasic_show_complete=false\tos={}\tarch={}\trequested_ms={}\tobserved_ms={}\tcommands={}\texpected_requested_frames={}\texpected_min_frames={}\tpre_restart_soak_frames={}\tpost_restart_frames={}\tgpu_backend={}\tgpu_adapter={}\tgpu_timing={}\tgpu_samples={}\tfake_sink_drops={}\tomitted=cameras,title,browser,two-box-scene,stream",
        std::env::consts::OS,
        std::env::consts::ARCH,
        result.requested.as_millis(),
        result.observed.as_millis(),
        result.receipts.len(),
        result.expected_requested_frames,
        result.expected_min_frames,
        result.frames,
        post_restart_frames,
        result.telemetry.gpu.backend,
        result.telemetry.gpu.adapter,
        result.telemetry.gpu.timing,
        result.telemetry.gpu.passes.total,
        result.telemetry.audio.sink_dropped,
    );
}

fn assert_snapshot(snapshot: &SnapshotMessage, revision: u64) -> Result<(), String> {
    let (program, preview) = expected_routing(revision);
    if snapshot.revision != revision
        || snapshot.desired_program != WireInputId::from_domain(program)
        || snapshot.realized_program != WireInputId::from_domain(program)
        || snapshot.desired_preview != WireInputId::from_domain(preview)
        || snapshot.realized_preview != WireInputId::from_domain(preview)
    {
        return Err(format!(
            "unexpected snapshot at revision {revision}: {snapshot:?}"
        ));
    }
    Ok(())
}

fn validate_telemetry(
    telemetry: &Telemetry,
    expected_min_frames: u64,
    require_gpu_timing: bool,
) -> Result<(), String> {
    validate_population("host lateness", &telemetry.host)?;
    validate_population("GPU pass", &telemetry.gpu.passes)?;
    if telemetry.host.total < expected_min_frames {
        return Err(format!(
            "insufficient host lateness samples: expected at least {expected_min_frames}, observed {}",
            telemetry.host.total
        ));
    }
    if telemetry.metric_errors != 0 {
        return Err(format!(
            "telemetry reported metric_errors={}",
            telemetry.metric_errors
        ));
    }
    validate_resource_telemetry(telemetry)?;
    validate_gpu_telemetry(&telemetry.gpu)?;
    let expected_metric_drops = telemetry
        .host
        .dropped
        .checked_add(telemetry.gpu.passes.dropped)
        .ok_or_else(|| "aggregate metric dropped count overflow".to_owned())?;
    if telemetry.metric_samples_dropped != expected_metric_drops {
        return Err(format!(
            "metric_samples_dropped={} does not equal host+GPU drops {expected_metric_drops}",
            telemetry.metric_samples_dropped
        ));
    }
    if require_gpu_timing
        && (telemetry.gpu.timing != "Supported" || telemetry.gpu.passes.total == 0)
    {
        return Err(format!(
            "FM_REQUIRE_GPU_TIMING=1 but gpu_timing={} and gpu_pass_samples_total={}",
            telemetry.gpu.timing, telemetry.gpu.passes.total
        ));
    }
    Ok(())
}

fn validate_population(label: &str, population: &MetricPopulation) -> Result<(), String> {
    let observed = population
        .retained
        .checked_add(population.dropped)
        .ok_or_else(|| format!("{label} population overflow"))?;
    if population.total != observed {
        return Err(format!(
            "{label} population is inconsistent: total={}, retained={}, metric_dropped={}",
            population.total, population.retained, population.dropped
        ));
    }
    Ok(())
}

fn validate_resource_telemetry(telemetry: &Telemetry) -> Result<(), String> {
    for (label, current, peak) in [
        (
            "audio retained blocks",
            telemetry.audio.retained_blocks,
            telemetry.audio.peak_retained_blocks,
        ),
        (
            "audio retained samples",
            telemetry.audio.retained_samples,
            telemetry.audio.peak_retained_samples,
        ),
        (
            "audio sink depth",
            telemetry.audio.sink_depth,
            telemetry.audio.sink_peak_depth,
        ),
        (
            "camera queue depth",
            telemetry.camera.queue_depth,
            telemetry.camera.queue_peak_depth,
        ),
        (
            "presentation pending depth",
            telemetry.presentation.pending_depth,
            telemetry.presentation.peak_pending_depth,
        ),
        (
            "recorder outstanding pairs",
            telemetry.recorder.outstanding_pairs,
            telemetry.recorder.peak_outstanding_pairs,
        ),
        (
            "recorder retained bytes",
            telemetry.recorder.retained_bytes,
            telemetry.recorder.peak_retained_bytes,
        ),
    ] {
        if current > peak {
            return Err(format!(
                "telemetry {label} current value {current} exceeds observed peak {peak}"
            ));
        }
    }
    if telemetry.presentation.active
        || telemetry.presentation.pending_depth != 0
        || telemetry.presentation.peak_pending_depth != 0
        || telemetry.presentation.dropped != 0
    {
        return Err(format!(
            "headless presentation must be inactive with zero pending depth: {:?}",
            telemetry.presentation
        ));
    }
    if telemetry.recorder.configured
        || telemetry.recorder.outstanding_pairs != 0
        || telemetry.recorder.peak_outstanding_pairs != 0
        || telemetry.recorder.retained_bytes != 0
        || telemetry.recorder.peak_retained_bytes != 0
    {
        return Err(format!(
            "unconfigured recorder reported pressure: {:?}",
            telemetry.recorder
        ));
    }
    if telemetry.camera
        != (CameraTelemetry {
            configured_sources: 0,
            frames_received: 0,
            frames_ingested: 0,
            native_dropped: 0,
            queue_depth: 0,
            queue_peak_depth: 0,
            queue_dropped: 0,
        })
    {
        return Err(format!(
            "camera-free soak reported camera activity: {:?}",
            telemetry.camera
        ));
    }
    Ok(())
}

fn validate_gpu_telemetry(gpu: &GpuTelemetry) -> Result<(), String> {
    if gpu.backend.is_empty() || gpu.adapter.is_empty() {
        return Err(format!(
            "GPU backend and adapter must be nonempty: backend={:?}, adapter={:?}",
            gpu.backend, gpu.adapter
        ));
    }
    if let Some(expected) = expected_gpu_backend()
        && gpu.backend != expected
    {
        return Err(format!(
            "unexpected GPU backend on {}: expected {expected}, observed {}",
            std::env::consts::OS,
            gpu.backend
        ));
    }
    if gpu.pending > 4 {
        return Err(format!(
            "GPU timing pending samples {} exceeds slot bound 4",
            gpu.pending
        ));
    }
    if gpu.timing == "Unsupported"
        && (gpu.passes.total != 0
            || gpu.passes.retained != 0
            || gpu.passes.dropped != 0
            || gpu.pending != 0
            || gpu.dropped != 0
            || gpu.unavailable != 0)
    {
        return Err(format!("unsupported GPU timing reported samples: {gpu:?}"));
    }
    Ok(())
}

const fn expected_gpu_backend() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some("Metal")
    } else if cfg!(target_os = "windows") {
        Some("Dx12")
    } else if cfg!(target_os = "linux") {
        Some("Vulkan")
    } else {
        None
    }
}

fn validate_checkpoint(
    persisted: &StoredProject,
    receipts: &[ExpectedReceipt],
    expected_min_frames: u64,
) -> Result<(), String> {
    let completed_commands = completed_commands(receipts)?;
    let position = persisted.position();
    if position.revision != completed_commands {
        return Err(format!(
            "persisted revision {} does not equal {completed_commands} completed commands",
            position.revision
        ));
    }
    if position.event_sequence != completed_commands
        || position.runtime_generation != completed_commands
    {
        return Err(format!(
            "persisted command coordinates are incoherent: {position:?}"
        ));
    }
    if position.frames_rendered < expected_min_frames {
        return Err(format!(
            "persisted frames below sustained-render threshold: expected at least {expected_min_frames}, observed {}",
            position.frames_rendered
        ));
    }
    validate_clock(position)?;
    if persisted.idempotency_receipts().len() != receipts.len() {
        return Err(format!(
            "persisted {} receipts for {completed_commands} commands",
            persisted.idempotency_receipts().len()
        ));
    }
    for expected in receipts {
        validate_receipt(persisted, *expected)?;
    }

    let routing = persisted.runtime_routing();
    let (expected_program, expected_preview) = expected_routing(completed_commands);
    if routing.desired_program_id != Some(expected_program)
        || routing.realized_program_id != Some(expected_program)
        || routing.desired_preview_id != Some(expected_preview)
        || routing.realized_preview_id != Some(expected_preview)
    {
        return Err(format!(
            "persisted routing is not idle/coherent: {routing:?}"
        ));
    }
    let main_mix = persisted
        .project()
        .main_mix()
        .ok_or_else(|| "persisted project has no main mix".to_owned())?;
    if main_mix != MainMix::new(expected_program, expected_preview) {
        return Err(format!(
            "canonical main mix disagrees with realized routing: {main_mix:?}"
        ));
    }
    Ok(())
}

fn validate_receipt(persisted: &StoredProject, expected: ExpectedReceipt) -> Result<(), String> {
    let key = format!("phase2-native-soak-key-{}", expected.sequence);
    let command_id = format!("phase2-native-soak-command-{}", expected.sequence);
    let receipt = persisted
        .idempotency_receipts()
        .iter()
        .find(|receipt| receipt.key() == key)
        .ok_or_else(|| format!("missing persisted receipt {key}"))?;
    if receipt.command_id() != command_id
        || receipt.outcome()
            != &(ReceiptOutcome::Accepted {
                revision: expected.sequence,
                target_frame: expected.target_frame,
            })
    {
        return Err(format!(
            "invalid persisted receipt for sequence {}: key={}, command_id={}, outcome={:?}",
            expected.sequence,
            receipt.key(),
            receipt.command_id(),
            receipt.outcome()
        ));
    }
    Ok(())
}

fn validate_clock(position: ProjectPosition) -> Result<(), String> {
    let expected = position
        .frames_rendered
        .checked_sub(1)
        .map_or(Ok(0), |frame| {
            frame
                .checked_mul(1_000_000_000 / FRAME_RATE)
                .ok_or_else(|| "expected clock time overflow".to_owned())
        })?;
    if position.clock_time_nanos != expected {
        return Err(format!(
            "persisted clock_time_nanos={} does not match {} frames at 25 fps (expected {expected})",
            position.clock_time_nanos, position.frames_rendered
        ));
    }
    Ok(())
}

fn expected_routing(revision: u64) -> (InputId, InputId) {
    if revision.is_multiple_of(2) {
        (input(1), input(2))
    } else {
        (input(2), input(1))
    }
}

fn completed_commands(receipts: &[ExpectedReceipt]) -> Result<u64, String> {
    u64::try_from(receipts.len()).map_err(|_| "completed command count does not fit u64".to_owned())
}

fn expected_frame_counts(seconds: u64) -> Result<(u64, u64), String> {
    let requested = seconds
        .checked_mul(FRAME_RATE)
        .ok_or_else(|| "requested frame count overflow".to_owned())?;
    let minimum = requested
        .checked_mul(90)
        .and_then(|value| value.checked_add(99))
        .ok_or_else(|| "minimum frame count overflow".to_owned())?
        / 100;
    Ok((requested, minimum))
}

fn save_project(path: &Path) -> Result<(), String> {
    let frame_rate = FrameRate::new(25, 1).map_err(|error| error.to_string())?;
    let mut project = Project::new(
        project_id(),
        "Phase-2 native diagnostic soak",
        ProjectSettings {
            frame_rate,
            video: VideoFormat {
                dimensions: VideoDimensions::new(64, 48)
                    .ok_or_else(|| "invalid 64x48 video dimensions".to_owned())?,
                frame_rate,
                pixel_format: PixelFormat::Rgba8,
                scan: ScanMode::Progressive,
                color: ColorMetadata::default(),
            },
            audio: AudioFormat {
                sample_rate: SampleRate::new(48_000)
                    .ok_or_else(|| "invalid 48000 Hz sample rate".to_owned())?,
                sample_format: SampleFormat::F32,
                channels: ChannelLayout::stereo(),
            },
        },
    );
    for (id, video) in [
        (input(1), SimulatedVideo::Bars),
        (
            input(2),
            SimulatedVideo::Solid(SolidColor::new(24, 80, 160, 255)),
        ),
    ] {
        project.add_input(Input {
            id,
            name: format!("Generator {id}"),
            kind: InputKind::Simulated(SimulatedInput::new(video, SimulatedAudio::Silence)),
            required_capabilities: Vec::new(),
        });
    }
    project.set_main_mix(MainMix::new(input(1), input(2)));
    let stored = StoredProject::from_project(
        project,
        RuntimeRouting {
            desired_program_id: Some(input(1)),
            realized_program_id: Some(input(1)),
            desired_preview_id: Some(input(2)),
            realized_preview_id: Some(input(2)),
        },
        ProjectPosition::default(),
        Vec::new(),
    )
    .map_err(|error| format!("cannot construct soak project: {error}"))?;
    ProjectStore::new(path)
        .map_err(|error| format!("cannot create project store: {error}"))?
        .save(&stored)
        .map_err(|error| format!("cannot save soak project: {error}"))
}

fn input(value: u128) -> InputId {
    InputId::new(NonZeroU128::new(value).expect("input ID is nonzero"))
}

fn project_id() -> ProjectId {
    ProjectId::new(NonZeroU128::new(PROJECT_ID).expect("project ID is nonzero"))
}

fn soak_seconds_from_env() -> Result<u64, String> {
    match std::env::var("FM_PHASE2_SOAK_SECONDS") {
        Ok(value) => parse_soak_seconds(Some(&value)),
        Err(std::env::VarError::NotPresent) => parse_soak_seconds(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err("FM_PHASE2_SOAK_SECONDS must be valid UTF-8".into())
        }
    }
}

fn parse_soak_seconds(value: Option<&str>) -> Result<u64, String> {
    let Some(value) = value else {
        return Ok(DEFAULT_SOAK_SECONDS);
    };
    let seconds = value.parse::<u64>().map_err(|_| {
        format!(
            "invalid FM_PHASE2_SOAK_SECONDS={value:?}; expected an integer in {MIN_SOAK_SECONDS}..={MAX_SOAK_SECONDS}"
        )
    })?;
    if !(MIN_SOAK_SECONDS..=MAX_SOAK_SECONDS).contains(&seconds) {
        return Err(format!(
            "invalid FM_PHASE2_SOAK_SECONDS={value:?}; expected an integer in {MIN_SOAK_SECONDS}..={MAX_SOAK_SECONDS}"
        ));
    }
    Ok(seconds)
}

fn required_flag(name: &str) -> Result<bool, String> {
    match std::env::var(name) {
        Ok(value) => parse_required_flag(name, Some(&value)),
        Err(std::env::VarError::NotPresent) => parse_required_flag(name, None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(format!("{name} must be valid UTF-8 and either 0 or 1"))
        }
    }
}

fn native_startup_unavailable(error: &str) -> bool {
    !error.contains("cleanup failed")
        && (error.contains("failed to request GPU adapter")
            || error.contains("failed to request GPU device"))
}

fn parse_required_flag(name: &str, value: Option<&str>) -> Result<bool, String> {
    match value {
        None | Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(value) => Err(format!("invalid {name}={value:?}; expected 0 or 1")),
    }
}

fn parse_audio_telemetry(
    fields: &BTreeMap<&str, &str>,
    record: &str,
) -> Result<AudioTelemetry, String> {
    let field = |key| -> Result<u64, String> {
        telemetry_field(fields, key)?
            .parse::<u64>()
            .map_err(|error| format!("invalid telemetry {key}: {error}; {record}"))
    };
    Ok(AudioTelemetry {
        retained_blocks: field("audio_retained_blocks")?,
        peak_retained_blocks: field("audio_observed_peak_retained_blocks")?,
        retained_samples: field("audio_retained_samples")?,
        peak_retained_samples: field("audio_observed_peak_retained_samples")?,
        retained_bytes: field("audio_retained_bytes")?,
        peak_retained_bytes: field("audio_observed_peak_retained_bytes")?,
        reservation_requests: field("audio_reservation_requests")?,
        reserved_blocks: field("audio_reserved_blocks")?,
        peak_reserved_blocks: field("audio_observed_peak_reserved_blocks")?,
        reserved_samples: field("audio_reserved_samples")?,
        peak_reserved_samples: field("audio_observed_peak_reserved_samples")?,
        reserved_bytes: field("audio_reserved_bytes")?,
        peak_reserved_bytes: field("audio_observed_peak_reserved_bytes")?,
        source_stalls: field("audio_source_stalls")?,
        positioned_blocks: field("audio_positioned_blocks")?,
        positioned_samples: field("audio_positioned_samples")?,
        leading_silence_samples: field("audio_leading_silence_samples")?,
        eos_padding_blocks: field("audio_eos_padding_blocks")?,
        eos_padding_samples: field("audio_eos_padding_samples")?,
        sink_depth: field("audio_sink_depth")?,
        sink_peak_depth: field("audio_sink_peak_depth")?,
        sink_dropped: field("audio_sink_dropped")?,
    })
}

fn parse_telemetry(stderr: &str) -> Result<Telemetry, String> {
    let records = stderr
        .lines()
        .filter(|line| line.starts_with("FREEMIXD_TELEMETRY\t"))
        .collect::<Vec<_>>();
    if records.len() != 1 {
        return Err(format!(
            "expected exactly one FREEMIXD_TELEMETRY record, found {}: {stderr}",
            records.len()
        ));
    }
    let record = records[0];
    let mut fields = BTreeMap::new();
    for field in record.split('\t').skip(1) {
        let (key, value) = field
            .split_once('=')
            .ok_or_else(|| format!("invalid telemetry field {field:?}: {record}"))?;
        if fields.insert(key, value).is_some() {
            return Err(format!("duplicate telemetry field {key}: {record}"));
        }
    }
    if fields.get("v") != Some(&"4") {
        return Err(format!("telemetry is not v=4: {record}"));
    }
    let u64_field = |key| -> Result<u64, String> {
        telemetry_field(&fields, key)?
            .parse::<u64>()
            .map_err(|error| format!("invalid telemetry {key}: {error}; {record}"))
    };
    let bool_field = |key| -> Result<bool, String> {
        telemetry_field(&fields, key)?
            .parse::<bool>()
            .map_err(|error| format!("invalid telemetry {key}: {error}; {record}"))
    };
    let gpu_timing = telemetry_field(&fields, "gpu_timing")?;
    if !matches!(gpu_timing, "Supported" | "Unsupported") {
        return Err(format!(
            "invalid telemetry gpu_timing={gpu_timing}: {record}"
        ));
    }
    Ok(Telemetry {
        host: MetricPopulation {
            total: u64_field("host_lateness_samples_total")?,
            retained: u64_field("host_lateness_samples_retained")?,
            dropped: u64_field("host_lateness_metric_samples_dropped")?,
        },
        audio: parse_audio_telemetry(&fields, record)?,
        camera: CameraTelemetry {
            configured_sources: u64_field("camera_configured_sources")?,
            frames_received: u64_field("camera_frames_received")?,
            frames_ingested: u64_field("camera_frames_ingested")?,
            native_dropped: u64_field("camera_native_dropped")?,
            queue_depth: u64_field("camera_queue_depth")?,
            queue_peak_depth: u64_field("camera_queue_peak_depth")?,
            queue_dropped: u64_field("camera_queue_dropped")?,
        },
        presentation: PresentationTelemetry {
            active: bool_field("presentation_active")?,
            pending_depth: u64_field("presentation_pending_depth")?,
            peak_pending_depth: u64_field("presentation_peak_pending_depth")?,
            dropped: u64_field("presentation_dropped")?,
        },
        recorder: RecorderTelemetry {
            configured: bool_field("recorder_configured")?,
            outstanding_pairs: u64_field("recorder_outstanding_pairs")?,
            peak_outstanding_pairs: u64_field("recorder_observed_peak_outstanding_pairs")?,
            retained_bytes: u64_field("recorder_retained_bytes")?,
            peak_retained_bytes: u64_field("recorder_observed_peak_retained_bytes")?,
        },
        gpu: GpuTelemetry {
            backend: telemetry_field(&fields, "gpu_backend")?.to_owned(),
            adapter: telemetry_field(&fields, "gpu_adapter")?.to_owned(),
            timing: gpu_timing.to_owned(),
            passes: MetricPopulation {
                total: u64_field("gpu_pass_samples_total")?,
                retained: u64_field("gpu_pass_samples_retained")?,
                dropped: u64_field("gpu_pass_metric_samples_dropped")?,
            },
            pending: u64_field("gpu_samples_pending")?,
            dropped: u64_field("gpu_samples_dropped")?,
            unavailable: u64_field("gpu_samples_unavailable")?,
        },
        metric_errors: u64_field("metric_errors")?,
        metric_samples_dropped: u64_field("metric_samples_dropped")?,
    })
}

fn telemetry_field<'a>(fields: &'a BTreeMap<&str, &str>, key: &str) -> Result<&'a str, String> {
    fields
        .get(key)
        .copied()
        .ok_or_else(|| format!("missing telemetry field {key}"))
}

fn spawn_stdout_drain(
    mut stdout: impl Read + Send + 'static,
) -> (
    mpsc::Receiver<io::Result<DrainedOutput>>,
    mpsc::Receiver<Result<String, String>>,
) {
    let (output_sender, output_receiver) = mpsc::sync_channel(1);
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut output = DrainedOutput::default();
        let mut pending_line = Vec::new();
        let mut line_exceeded = false;
        let mut readiness_sent = false;
        let mut chunk = [0_u8; 8 * 1024];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => {
                    if !readiness_sent {
                        let _ = ready_sender
                            .send(Err("daemon stdout closed before FREEMIXD_READY".into()));
                    }
                    let _ = output_sender.send(Ok(output));
                    return;
                }
                Ok(read) => {
                    append_bounded(&mut output, &chunk[..read]);
                    for byte in &chunk[..read] {
                        if pending_line.len() < READY_LINE_LIMIT {
                            pending_line.push(*byte);
                        } else {
                            line_exceeded = true;
                        }
                        if *byte == b'\n' {
                            if !readiness_sent
                                && !line_exceeded
                                && pending_line.starts_with(b"FREEMIXD_READY\t")
                            {
                                let line = String::from_utf8(pending_line.clone())
                                    .map_err(|error| format!("readiness was not UTF-8: {error}"));
                                let _ = ready_sender.send(line);
                                readiness_sent = true;
                            }
                            pending_line.clear();
                            line_exceeded = false;
                        }
                    }
                }
                Err(error) => {
                    if !readiness_sent {
                        let _ = ready_sender.send(Err(format!(
                            "cannot read daemon stdout before readiness: {error}"
                        )));
                    }
                    let _ = output_sender.send(Err(error));
                    return;
                }
            }
        }
    });
    (output_receiver, ready_receiver)
}

fn spawn_drain(
    mut reader: impl Read + Send + 'static,
) -> mpsc::Receiver<io::Result<DrainedOutput>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut output = DrainedOutput::default();
        let mut chunk = [0_u8; 8 * 1024];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => {
                    let _ = sender.send(Ok(output));
                    return;
                }
                Ok(read) => append_bounded(&mut output, &chunk[..read]),
                Err(error) => {
                    let _ = sender.send(Err(error));
                    return;
                }
            }
        }
    });
    receiver
}

fn append_bounded(output: &mut DrainedOutput, bytes: &[u8]) {
    let remaining = OUTPUT_LIMIT.saturating_sub(output.bytes.len());
    output
        .bytes
        .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
    output.exceeded |= bytes.len() > remaining;
}

fn receive_drain(
    receiver: &mpsc::Receiver<io::Result<DrainedOutput>>,
    name: &str,
) -> Result<DrainedOutput, String> {
    receiver
        .recv_timeout(DRAIN_TIMEOUT)
        .map_err(|error| format!("timed out draining daemon {name}: {error}"))?
        .map_err(|error| format!("cannot drain daemon {name}: {error}"))
}

fn ensure_bounded(output: &ProcessOutput) -> Result<(), String> {
    if output.stdout.exceeded || output.stderr.exceeded {
        return Err(format!(
            "daemon output exceeded {OUTPUT_LIMIT} bytes per stream; {}",
            output_diagnostic(output)
        ));
    }
    Ok(())
}

fn output_diagnostic(output: &ProcessOutput) -> String {
    format!(
        "status={}, stdout={:?}, stderr={:?}, stdout_exceeded={}, stderr_exceeded={}",
        output.status,
        String::from_utf8_lossy(&output.stdout.bytes),
        String::from_utf8_lossy(&output.stderr.bytes),
        output.stdout.exceeded,
        output.stderr.exceeded
    )
}

#[test]
fn soak_duration_defaults_and_accepts_bounds() {
    assert_eq!(parse_soak_seconds(None), Ok(60));
    assert_eq!(parse_soak_seconds(Some("3")), Ok(3));
    assert_eq!(parse_soak_seconds(Some("86400")), Ok(86_400));
}

#[test]
fn soak_duration_rejects_non_integer_and_out_of_range_values() {
    for value in ["", "1", "2", "86401", "2s", " 60"] {
        assert!(
            parse_soak_seconds(Some(value)).is_err(),
            "accepted {value:?}"
        );
    }
}

#[test]
fn required_flags_accept_only_absent_zero_or_one() {
    assert_eq!(parse_required_flag("FLAG", None), Ok(false));
    assert_eq!(parse_required_flag("FLAG", Some("0")), Ok(false));
    assert_eq!(parse_required_flag("FLAG", Some("1")), Ok(true));
    for value in ["", "true", "2", " 1"] {
        assert!(parse_required_flag("FLAG", Some(value)).is_err());
    }
}

#[test]
fn optional_native_skip_is_limited_to_adapter_or_device_unavailability() {
    assert!(native_startup_unavailable(
        "error: failed to request GPU adapter: no adapter"
    ));
    assert!(native_startup_unavailable(
        "error: failed to request GPU device: unavailable"
    ));
    assert!(!native_startup_unavailable("invalid FREEMIXD_READY record"));
    assert!(!native_startup_unavailable(
        "failed to request GPU adapter; cleanup failed: could not confirm reap"
    ));
}

#[test]
fn sustained_frame_threshold_uses_checked_ceiling() {
    assert_eq!(expected_frame_counts(3), Ok((75, 68)));
    assert_eq!(expected_frame_counts(60), Ok((1_500, 1_350)));
}

#[test]
fn protocol_line_reader_is_bounded_and_requires_utf8_newline() {
    let mut valid = io::Cursor::new(b"{}\n".to_vec());
    assert_eq!(read_protocol_line(&mut valid), Ok(Some("{}\n".into())));

    let mut unterminated = io::Cursor::new(b"{}".to_vec());
    assert!(read_protocol_line(&mut unterminated).is_err());
    let mut invalid_utf8 = io::Cursor::new(vec![0xff, b'\n']);
    assert!(read_protocol_line(&mut invalid_utf8).is_err());
    let mut oversized = io::Cursor::new(vec![b'x'; PROTOCOL_LINE_LIMIT + 1]);
    assert!(read_protocol_line(&mut oversized).is_err());
}

fn expected_audio_telemetry() -> AudioTelemetry {
    AudioTelemetry {
        retained_blocks: 3,
        peak_retained_blocks: 4,
        retained_samples: 5,
        peak_retained_samples: 6,
        retained_bytes: 20,
        peak_retained_bytes: 21,
        reservation_requests: 22,
        reserved_blocks: 23,
        peak_reserved_blocks: 24,
        reserved_samples: 25,
        peak_reserved_samples: 26,
        reserved_bytes: 27,
        peak_reserved_bytes: 28,
        source_stalls: 29,
        positioned_blocks: 30,
        positioned_samples: 31,
        leading_silence_samples: 32,
        eos_padding_blocks: 33,
        eos_padding_samples: 34,
        sink_depth: 1,
        sink_peak_depth: 2,
        sink_dropped: 7,
    }
}

#[test]
fn telemetry_parser_requires_one_v4_record_and_extracts_soak_fields() {
    let line = concat!(
        "FREEMIXD_TELEMETRY\tv=4",
        "\thost_lateness_samples_total=42\thost_lateness_samples_retained=40",
        "\thost_lateness_metric_samples_dropped=2",
        "\taudio_retained_blocks=3\taudio_observed_peak_retained_blocks=4",
        "\taudio_retained_samples=5\taudio_observed_peak_retained_samples=6",
        "\taudio_retained_bytes=20\taudio_observed_peak_retained_bytes=21",
        "\taudio_reservation_requests=22\taudio_reserved_blocks=23",
        "\taudio_observed_peak_reserved_blocks=24\taudio_reserved_samples=25",
        "\taudio_observed_peak_reserved_samples=26\taudio_reserved_bytes=27",
        "\taudio_observed_peak_reserved_bytes=28\taudio_source_stalls=29",
        "\taudio_positioned_blocks=30\taudio_positioned_samples=31",
        "\taudio_leading_silence_samples=32\taudio_eos_padding_blocks=33",
        "\taudio_eos_padding_samples=34",
        "\taudio_sink_depth=1\taudio_sink_peak_depth=2\taudio_sink_dropped=7",
        "\tcamera_configured_sources=2\tcamera_frames_received=30",
        "\tcamera_frames_ingested=27\tcamera_native_dropped=3",
        "\tcamera_queue_depth=1\tcamera_queue_peak_depth=2\tcamera_queue_dropped=3",
        "\tpresentation_active=false\tpresentation_pending_depth=0",
        "\tpresentation_peak_pending_depth=0\tpresentation_dropped=0",
        "\trecorder_configured=false\trecorder_outstanding_pairs=0",
        "\trecorder_observed_peak_outstanding_pairs=0\trecorder_retained_bytes=0",
        "\trecorder_observed_peak_retained_bytes=0",
        "\tgpu_backend=Metal\tgpu_adapter=Test Adapter\tgpu_timing=Supported",
        "\tgpu_pass_samples_total=41\tgpu_pass_samples_retained=40",
        "\tgpu_pass_metric_samples_dropped=1\tgpu_samples_pending=2",
        "\tgpu_samples_dropped=3\tgpu_samples_unavailable=4",
        "\tmetric_errors=0\tmetric_samples_dropped=3",
    );
    assert_eq!(
        parse_telemetry(line),
        Ok(Telemetry {
            host: MetricPopulation {
                total: 42,
                retained: 40,
                dropped: 2,
            },
            audio: expected_audio_telemetry(),
            camera: CameraTelemetry {
                configured_sources: 2,
                frames_received: 30,
                frames_ingested: 27,
                native_dropped: 3,
                queue_depth: 1,
                queue_peak_depth: 2,
                queue_dropped: 3,
            },
            presentation: PresentationTelemetry {
                active: false,
                pending_depth: 0,
                peak_pending_depth: 0,
                dropped: 0,
            },
            recorder: RecorderTelemetry {
                configured: false,
                outstanding_pairs: 0,
                peak_outstanding_pairs: 0,
                retained_bytes: 0,
                peak_retained_bytes: 0,
            },
            gpu: GpuTelemetry {
                backend: "Metal".into(),
                adapter: "Test Adapter".into(),
                timing: "Supported".into(),
                passes: MetricPopulation {
                    total: 41,
                    retained: 40,
                    dropped: 1,
                },
                pending: 2,
                dropped: 3,
                unavailable: 4,
            },
            metric_errors: 0,
            metric_samples_dropped: 3,
        })
    );
    assert!(parse_telemetry(&format!("{line}\n{line}")).is_err());
    assert!(parse_telemetry(&line.replacen("v=4", "v=3", 1)).is_err());
}
