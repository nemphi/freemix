#![cfg(feature = "native-media")]

use std::{
    io::{self, Read},
    num::NonZeroU128,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::Mutex,
};

#[cfg(target_os = "macos")]
use fm_io_macos::{CameraIdKind, deterministic_camera_id};
use fm_model::{Input, InputKind, MainMix, Project, ProjectSettings};
#[cfg(target_os = "macos")]
use fm_model::{
    Layer, LayerGeometry, RectMask, Rgba8, Rotation, Scene, SimulatedAudio, SimulatedInput,
    SimulatedVideo, SolidColor, SourceRef, StingerAudioPolicy, StingerConfig,
    StingerMissingMediaFallback, StingerSlotNumber,
};
use fm_persistence::{ProjectPosition, ProjectStore, RuntimeRouting, StoredProject};
#[cfg(target_os = "macos")]
use fm_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientType, CommandMessage, CommandPayload, CommandResult,
    HandshakeRequest, ManualTransitionKind, ManualTransitionPosition, Role, RuntimeLifecycleEvent,
    SnapshotMessage, StingerStatus, WireInputId, WireMessage, WireStingerSlotId, decode_line,
    encode_line,
};
#[cfg(target_os = "macos")]
use fm_types::SceneId;
use fm_types::{
    AudioFormat, ChannelLayout, ColorMetadata, FrameRate, InputId, PixelFormat, ProjectId,
    SampleFormat, SampleRate, ScanMode, VideoDimensions, VideoFormat,
};
#[cfg(target_os = "macos")]
use freemixd::ReadinessRecord;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(20);
#[cfg(target_os = "macos")]
const RECORDING_PROCESS_TIMEOUT: Duration = Duration::from_secs(40);
const OUTPUT_LIMIT: usize = 256 * 1024;

#[cfg(target_os = "macos")]
const FRAME_PERIOD: Duration = Duration::from_millis(40);

#[cfg(target_os = "macos")]
static NATIVE_MEDIA_TEST_LOCK: Mutex<()> = Mutex::new(());

struct BoundedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Default)]
struct DrainedBytes {
    bytes: Vec<u8>,
    exceeded: bool,
}

struct CapturedChild {
    child: Option<Child>,
    stdout: Option<mpsc::Receiver<io::Result<DrainedBytes>>>,
    stderr: Option<mpsc::Receiver<io::Result<DrainedBytes>>>,
}

impl CapturedChild {
    fn new(mut child: Child) -> Self {
        let stdout = spawn_drain(child.stdout.take(), Vec::new());
        let stderr = spawn_drain(child.stderr.take(), Vec::new());
        Self {
            child: Some(child),
            stdout: Some(stdout),
            stderr: Some(stderr),
        }
    }

    #[cfg(target_os = "macos")]
    fn with_readiness(mut child: Child) -> (Self, mpsc::Receiver<Result<String, String>>) {
        let stdout = child.stdout.take().expect("daemon stdout must be piped");
        let (readiness_sender, readiness_receiver) = mpsc::sync_channel(1);
        let (stdout_sender, stdout_receiver) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let readiness = reader
                .read_line(&mut line)
                .map(|_| line.clone())
                .map_err(|error| error.to_string());
            let _ = readiness_sender.send(readiness);
            let _ = stdout_sender.send(drain_bounded(reader, line.as_bytes()));
        });
        let stderr = spawn_drain(child.stderr.take(), Vec::new());
        (
            Self {
                child: Some(child),
                stdout: Some(stdout_receiver),
                stderr: Some(stderr),
            },
            readiness_receiver,
        )
    }

    #[cfg(target_os = "macos")]
    fn is_alive(&mut self) -> bool {
        self.child
            .as_mut()
            .expect("captured child is present")
            .try_wait()
            .unwrap()
            .is_none()
    }

    fn wait(mut self, timeout: Duration) -> BoundedOutput {
        let status = self.poll_status(timeout).unwrap_or_else(|| {
            self.kill_and_poll();
            panic!("process exceeded {timeout:?}");
        });
        self.collect(status, timeout)
    }

    #[cfg(target_os = "macos")]
    fn kill_and_reap(mut self) -> BoundedOutput {
        let status = self.kill_and_poll();
        self.collect(status, PROCESS_TIMEOUT)
    }

    fn poll_status(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self
                .child
                .as_mut()
                .expect("captured child is present")
                .try_wait()
                .unwrap()
            {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn kill_and_poll(&mut self) -> ExitStatus {
        let child = self.child.as_mut().expect("captured child is present");
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        child.kill().expect("kill child process");
        self.poll_status(PROCESS_TIMEOUT)
            .expect("killed process was not reaped before timeout")
    }

    fn collect(&mut self, status: ExitStatus, timeout: Duration) -> BoundedOutput {
        self.child.take();
        let deadline = Instant::now() + timeout;
        let stdout_receiver = self.stdout.take().expect("stdout drain is present");
        let stderr_receiver = self.stderr.take().expect("stderr drain is present");
        let stdout = receive_drain(&stdout_receiver, deadline, "stdout");
        let stderr = receive_drain(&stderr_receiver, deadline, "stderr");
        assert!(!stdout.exceeded, "stdout exceeded bound");
        assert!(!stderr.exceeded, "stderr exceeded bound");
        BoundedOutput {
            status,
            stdout: stdout.bytes,
            stderr: stderr.bytes,
        }
    }
}

impl Drop for CapturedChild {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut()
            && child.try_wait().ok().flatten().is_none()
        {
            let _ = child.kill();
            let deadline = Instant::now() + PROCESS_TIMEOUT;
            while Instant::now() < deadline {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

fn spawn_drain<R: Read + Send + 'static>(
    reader: Option<R>,
    prefix: Vec<u8>,
) -> mpsc::Receiver<io::Result<DrainedBytes>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let result = match reader {
            Some(reader) => drain_bounded(reader, &prefix),
            None => Ok(DrainedBytes::default()),
        };
        let _ = sender.send(result);
    });
    receiver
}

fn drain_bounded(mut reader: impl Read, prefix: &[u8]) -> io::Result<DrainedBytes> {
    let mut output = DrainedBytes::default();
    append_bounded(&mut output, prefix);
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            return Ok(output);
        }
        append_bounded(&mut output, &chunk[..read]);
    }
}

fn append_bounded(output: &mut DrainedBytes, bytes: &[u8]) {
    let remaining = OUTPUT_LIMIT.saturating_sub(output.bytes.len());
    output
        .bytes
        .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
    output.exceeded |= bytes.len() > remaining;
}

fn receive_drain(
    receiver: &mpsc::Receiver<io::Result<DrainedBytes>>,
    deadline: Instant,
    name: &str,
) -> DrainedBytes {
    let remaining = deadline.saturating_duration_since(Instant::now());
    receiver
        .recv_timeout(remaining)
        .unwrap_or_else(|error| panic!("{name} drain timed out: {error}"))
        .unwrap_or_else(|error| panic!("cannot drain {name}: {error}"))
}

fn wait_bounded(child: Child, timeout: Duration) -> BoundedOutput {
    CapturedChild::new(child).wait(timeout)
}

#[test]
fn native_media_missing_asset_fails_before_readiness() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("missing-asset.freemix");
    let recording = directory.path().join("must-not-exist.mp4");
    save_media_project(&path, "asset://missing-one.mkv", "asset://missing-two.mkv");

    let child = Command::new(env!("CARGO_BIN_EXE_freemixd"))
        .args(["serve"])
        .arg(&path)
        .arg("--native-media")
        .arg("--record-program")
        .arg(&recording)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = wait_bounded(child, PROCESS_TIMEOUT);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!output.status.success());
    assert!(!stdout.contains("FREEMIXD_READY"));
    assert!(stderr.contains("project assets root is unavailable"));
    assert!(!stderr.contains("missing-one"));
    assert!(!stderr.contains(path.to_string_lossy().as_ref()));
    assert!(!recording.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn native_camera_startup_failure_emits_source_diagnostic_and_reaps_helper() {
    let directory = tempfile::tempdir().unwrap();
    let helper = save_failing_camera_helper(directory.path());
    let project_path = directory.path().join("camera-startup-failure.freemix");
    save_camera_project(&project_path);

    let child = Command::new(env!("CARGO_BIN_EXE_freemixd"))
        .arg("serve")
        .arg(&project_path)
        .args(["--native-media", "--camera-helper"])
        .arg(&helper)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = wait_bounded(child, PROCESS_TIMEOUT);
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("FREEMIXD_READY"));
    let stderr = String::from_utf8(output.stderr).unwrap();
    let source = camera_source_diagnostic(&stderr);
    assert_eq!(diagnostic_value(source, "input_id"), input(1).to_string());
    assert_eq!(diagnostic_value(source, "sample_lifecycle"), "open");
    assert_eq!(diagnostic_value(source, "health"), "healthy");
    for key in [
        "frames_received",
        "frames_ingested",
        "native_dropped",
        "queue_depth",
        "queue_peak_depth",
        "queue_dropped",
    ] {
        assert_eq!(diagnostic_value(source, key), "0");
    }
    assert!(!stderr.contains("FREEMIXD_TELEMETRY\t"));
    let helper_pid = fs::read_to_string(helper.with_extension("sh.pid")).unwrap();
    assert!(
        !process_exists(helper_pid.trim()),
        "camera helper {helper_pid} survived failed startup"
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires FFmpeg with libx264 and a native macOS Metal adapter"]
fn native_media_daemon_refills_beyond_startup_prefix_and_checkpoints_once() {
    let _hardware_lock = NATIVE_MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    let project_path = directory.path().join("native-daemon.freemix");
    if !prepare_native_project(&project_path) {
        return;
    }
    let startup = ProjectStore::new(&project_path).unwrap().load().unwrap();
    let startup_cursor = startup.position().frames_rendered;

    let Some(daemon) = require_native(NativeDaemonProcess::start(&project_path, true)) else {
        return;
    };

    thread::sleep(Duration::from_millis(5_700));
    let mut client = StudioClient::connect(daemon.address);
    client.handshake();
    let outcome = client.command("native-cut", "native-cut-key", CommandPayload::Cut);
    assert!(outcome.scheduled_frame >= startup_cursor.saturating_add(130));
    drop(client);

    let output = daemon.wait();
    assert!(
        output.status.success(),
        "native daemon failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let persisted = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert!(persisted.position().frames_rendered > outcome.scheduled_frame);
    assert!(persisted.position().clock_time_nanos > 0);
    assert_eq!(persisted.position().revision, 1);
    assert_eq!(
        persisted.runtime_routing().desired_program_id,
        Some(input(2))
    );
    assert_eq!(
        persisted.runtime_routing().realized_program_id,
        Some(input(2))
    );
    assert_eq!(
        persisted.runtime_routing().desired_preview_id,
        Some(input(1))
    );
    assert_eq!(
        persisted.runtime_routing().realized_preview_id,
        Some(input(1))
    );
}

#[cfg(target_os = "macos")]
#[test]
fn native_daemon_hot_configures_fires_replaces_removes_and_restarts_stinger() {
    let _hardware_lock = NATIVE_MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    let project_path = directory.path().join("native-stinger-rejection.freemix");
    save_generator_project(&project_path);
    let before = ProjectStore::new(&project_path).unwrap().load().unwrap();

    let Some(daemon) = require_native(NativeDaemonProcess::start(&project_path, true)) else {
        return;
    };
    let mut client = StudioClient::connect(daemon.address);
    let initial = client.handshake();
    assert_eq!(initial.revision, 0);
    assert_eq!(initial.revision, 0);
    let configured = client.command(
        "native-configure-stinger",
        "native-configure-stinger-key",
        CommandPayload::ConfigureStinger {
            slot: WireStingerSlotId::new(8).unwrap(),
            media_input: WireInputId::from_domain(input(2)),
            preload: true,
            cut_point_frames: 7,
            audio_policy: fm_protocol::StingerAudioPolicy::Muted,
            missing_media_fallback: fm_protocol::StingerMissingMediaFallback::KeepProgram,
        },
    );
    assert_eq!(configured.revision, 1);
    let configured_slot = configured
        .stingers
        .as_ref()
        .and_then(|stingers| stingers.iter().find(|stinger| stinger.slot.number() == 8))
        .expect("configure projects slot 8");
    assert_eq!(
        configured_slot.readiness,
        fm_protocol::StingerReadiness::Ready
    );
    let fired = client.command(
        "native-fire-stinger",
        "native-fire-stinger-key",
        CommandPayload::Stinger {
            slot: WireStingerSlotId::new(8).unwrap(),
            duration_frames: 7,
        },
    );
    assert_eq!(fired.revision, 2);
    let after_first_fire = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert_eq!(
        (
            after_first_fire.runtime_routing().desired_program_id,
            after_first_fire.runtime_routing().realized_program_id,
            after_first_fire.runtime_routing().desired_preview_id,
            after_first_fire.runtime_routing().realized_preview_id,
        ),
        (
            Some(input(2)),
            Some(input(2)),
            Some(input(1)),
            Some(input(1)),
        )
    );
    let replaced = client.command(
        "native-replace-stinger",
        "native-replace-stinger-key",
        CommandPayload::ConfigureStinger {
            slot: WireStingerSlotId::new(8).unwrap(),
            media_input: WireInputId::from_domain(input(1)),
            preload: true,
            cut_point_frames: 3,
            audio_policy: fm_protocol::StingerAudioPolicy::Muted,
            missing_media_fallback: fm_protocol::StingerMissingMediaFallback::Cut,
        },
    );
    assert_eq!(replaced.revision, 3);
    let replaced_slot = replaced
        .stingers
        .as_ref()
        .and_then(|stingers| stingers.iter().find(|stinger| stinger.slot.number() == 8))
        .expect("replacement projects slot 8");
    assert_eq!(
        replaced_slot.media_input,
        WireInputId::from_domain(input(1))
    );
    assert_eq!(
        replaced_slot.readiness,
        fm_protocol::StingerReadiness::Ready
    );
    let fired_replacement = client.command(
        "native-fire-replacement",
        "native-fire-replacement-key",
        CommandPayload::Stinger {
            slot: WireStingerSlotId::new(8).unwrap(),
            duration_frames: 4,
        },
    );
    assert_eq!(fired_replacement.revision, 4);
    let after_replacement_fire = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert_eq!(
        (
            after_replacement_fire.runtime_routing().desired_program_id,
            after_replacement_fire.runtime_routing().realized_program_id,
            after_replacement_fire.runtime_routing().desired_preview_id,
            after_replacement_fire.runtime_routing().realized_preview_id,
        ),
        (
            Some(input(1)),
            Some(input(1)),
            Some(input(2)),
            Some(input(2)),
        )
    );
    let removed = client.command(
        "native-remove-stinger",
        "native-remove-stinger-key",
        CommandPayload::RemoveStinger {
            slot: WireStingerSlotId::new(8).unwrap(),
        },
    );
    assert_eq!(removed.revision, 5);
    assert_eq!(removed.stingers, Some(Vec::new()));

    drop(client);
    let output = daemon.wait();
    assert!(
        output.status.success(),
        "native daemon failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert_eq!(after.position().revision, 5);
    assert_eq!(after.idempotency_receipts().len(), 5);
    assert!(after.project().stingers().is_empty());
    assert_eq!(after.project(), before.project());

    let Some(restarted) = require_native(NativeDaemonProcess::start(&project_path, true)) else {
        return;
    };
    let mut restarted_client = StudioClient::connect(restarted.address);
    let snapshot = restarted_client.handshake();
    assert_eq!(snapshot.revision, 5);
    assert!(snapshot.stingers.is_empty());
    drop(restarted_client);
    let output = restarted.wait();
    assert!(
        output.status.success(),
        "restarted native daemon failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires FFmpeg with local video/audio and a native macOS Metal adapter"]
fn native_daemon_hot_replaces_audible_local_stinger_pool() {
    let _hardware_lock = NATIVE_MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    let project_path = directory
        .path()
        .join("native-audible-stinger-hot-swap.freemix");
    if require_native(prepare_audible_stinger_project(&project_path)).is_none() {
        return;
    }
    let store = ProjectStore::new(&project_path).unwrap();
    let stored = store.load().unwrap();
    let mut project = stored.project().clone();
    project.add_input(Input {
        id: input(4),
        name: "Audible Stinger replacement".into(),
        kind: InputKind::Media {
            asset_uri: "asset://stinger.mov".into(),
        },
        required_capabilities: Vec::new(),
    });
    let expanded = StoredProject::from_project(
        project,
        stored.runtime_routing(),
        stored.position(),
        stored.idempotency_receipts().to_vec(),
    )
    .unwrap();
    store.save(&expanded).unwrap();

    let Some(daemon) = require_native(NativeDaemonProcess::start(&project_path, true)) else {
        return;
    };
    let mut client = StudioClient::connect(daemon.address);
    assert_eq!(client.handshake().revision, 0);
    let replaced = client.command(
        "replace-audible-stinger",
        "replace-audible-stinger-key",
        CommandPayload::ConfigureStinger {
            slot: WireStingerSlotId::new(2).unwrap(),
            media_input: WireInputId::from_domain(input(4)),
            preload: true,
            cut_point_frames: 6,
            audio_policy: fm_protocol::StingerAudioPolicy::StingerOnly,
            missing_media_fallback: fm_protocol::StingerMissingMediaFallback::Cut,
        },
    );
    assert_eq!(replaced.revision, 1);
    let slot = replaced
        .stingers
        .as_ref()
        .and_then(|stingers| stingers.iter().find(|stinger| stinger.slot.number() == 2))
        .expect("audible replacement projects slot 2");
    assert_eq!(slot.media_input, WireInputId::from_domain(input(4)));
    assert_eq!(slot.readiness, fm_protocol::StingerReadiness::Ready);
    let fired = client.command(
        "fire-audible-replacement",
        "fire-audible-replacement-key",
        CommandPayload::Stinger {
            slot: WireStingerSlotId::new(2).unwrap(),
            duration_frames: 12,
        },
    );
    assert_eq!(fired.revision, 2);
    let removed = client.command(
        "remove-audible-replacement",
        "remove-audible-replacement-key",
        CommandPayload::RemoveStinger {
            slot: WireStingerSlotId::new(2).unwrap(),
        },
    );
    assert_eq!(removed.revision, 3);
    assert!(
        removed
            .stingers
            .as_ref()
            .is_some_and(|stingers| stingers.iter().all(|stinger| stinger.slot.number() != 2))
    );
    drop(client);

    let output = daemon.wait();
    assert!(
        output.status.success(),
        "native audible hot swap failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let diagnostic = telemetry_diagnostic(&stderr);
    assert!(
        diagnostic_value(diagnostic, "audio_observed_peak_retained_blocks")
            .parse::<usize>()
            .unwrap()
            <= 1_024
    );
    assert!(
        diagnostic_value(diagnostic, "audio_observed_peak_retained_samples")
            .parse::<usize>()
            .unwrap()
            <= 1_024 * 1_024
    );
    assert!(
        diagnostic_value(diagnostic, "audio_observed_peak_retained_bytes")
            .parse::<usize>()
            .unwrap()
            <= 32 * 1_024 * 1_024
    );
    let persisted = store.load().unwrap();
    assert_eq!(persisted.position().revision, 3);
    assert_eq!(persisted.idempotency_receipts().len(), 3);
    assert!(
        persisted
            .project()
            .stingers()
            .iter()
            .all(|stinger| stinger.slot.number() != 2)
    );
}

#[cfg(target_os = "macos")]
#[test]
fn native_stinger_preflight_failure_rolls_back_and_keeps_show_cursor_live() {
    let _hardware_lock = NATIVE_MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    let project_path = directory.path().join("native-stinger-rollback.freemix");
    let rate = FrameRate::new(25, 1).unwrap();
    save_generator_project_with_sources_and_dimensions(
        &project_path,
        rate,
        0,
        VideoDimensions::new(2_900, 2_900).unwrap(),
        [
            (input(1), SimulatedVideo::Bars),
            (
                input(2),
                SimulatedVideo::Solid(SolidColor::new(24, 80, 160, 255)),
            ),
        ],
    );
    let manifest_path = project_path.join("project.json");
    let before_bytes = fs::read(&manifest_path).unwrap();
    let before = ProjectStore::new(&project_path).unwrap().load().unwrap();

    let Some(daemon) = require_native(NativeDaemonProcess::start(&project_path, true)) else {
        return;
    };
    let mut client = StudioClient::connect(daemon.address);
    let initial = client.handshake();
    assert_eq!(initial.revision, 0);
    client.send(&WireMessage::Command(CommandMessage {
        protocol: CURRENT_PROTOCOL_VERSION,
        id: "oversized-stinger".into(),
        idempotency_key: "oversized-stinger-key".into(),
        expected_revision: Some(0),
        deadline_ms: None,
        payload: CommandPayload::ConfigureStinger {
            slot: WireStingerSlotId::new(1).unwrap(),
            media_input: WireInputId::from_domain(input(2)),
            preload: true,
            cut_point_frames: 1,
            audio_policy: fm_protocol::StingerAudioPolicy::Muted,
            missing_media_fallback: fm_protocol::StingerMissingMediaFallback::Cut,
        },
    }));
    let rejected =
        client.receive_until("path-free native Stinger preflight rejection", |message| {
            if let WireMessage::CommandResult(result @ CommandResult::Rejected { id, .. }) = message
                && id == "oversized-stinger"
            {
                Some(result.clone())
            } else {
                None
            }
        });
    assert!(matches!(
        rejected,
        CommandResult::Rejected {
            code,
            message,
            current_revision: 0,
            retryable: false,
            ..
        } if code == "unavailable"
            && message == "native Stinger resources could not be prepared"
    ));
    assert_eq!(fs::read(&manifest_path).unwrap(), before_bytes);
    let rolled_back = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert_eq!(rolled_back, before);
    assert!(rolled_back.idempotency_receipts().is_empty());

    drop(client);
    let mut resumed = StudioClient::connect(daemon.address);
    let snapshot = resumed.handshake();
    assert_eq!(snapshot.revision, 0);
    assert!(snapshot.stingers.is_empty());
    let cut = resumed.command(
        "post-rollback-cut",
        "post-rollback-cut-key",
        CommandPayload::Cut,
    );
    assert_eq!(cut.revision, 1);
    assert!(cut.scheduled_frame > 0);
    drop(resumed);

    let output = daemon.wait();
    assert!(
        output.status.success(),
        "native daemon failed after rollback: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let persisted = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert_eq!(persisted.position().revision, 1);
    assert_eq!(persisted.idempotency_receipts().len(), 1);
    assert!(persisted.project().stingers().is_empty());
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires FFmpeg with libx264/AAC, ffprobe, and a native macOS Metal adapter"]
fn native_44_1k_local_audio_resamples_to_48k_master_recording() {
    let _hardware_lock = NATIVE_MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    if require_recording_tools().is_none() {
        return;
    }
    let project_path = directory.path().join("record-44-1k.freemix");
    let output_path = directory.path().join("program-48k.mp4");
    if !prepare_native_project(&project_path) {
        return;
    }

    let Some(mut daemon) = require_native_recorder(NativeDaemonProcess::start_recording(
        &project_path,
        &output_path,
    )) else {
        return;
    };
    let mut client = StudioClient::connect(daemon.address);
    client.handshake();
    thread::sleep(Duration::from_millis(1_500));
    daemon.signal_terminate();
    let output = daemon.wait_for(RECORDING_PROCESS_TIMEOUT);
    drop(client);
    assert!(
        output.status.success(),
        "44.1k recording daemon failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let audio = ffprobe_stream(&output_path, "a:0").unwrap();
    assert_eq!(probe_value(&audio, "sample_rate"), "48000");
    assert_eq!(probe_value(&audio, "channels"), "2");
    assert!(probe_count(&audio, "nb_read_packets") > 0);
    decode_recording(&output_path).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires a native macOS Metal adapter"]
fn native_generator_daemon_requires_neither_assets_nor_ffmpeg_tools() {
    let _hardware_lock = NATIVE_MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    let project_path = directory.path().join("native-generators.freemix");
    save_generator_project(&project_path);
    assert!(!project_path.join("assets").exists());

    let Some(daemon) = require_native(NativeDaemonProcess::start_without_tools(
        &project_path,
        true,
    )) else {
        return;
    };
    thread::sleep(Duration::from_millis(100));
    let mut client = StudioClient::connect(daemon.address);
    let initial = client.handshake();
    assert_snapshot_routing(&initial, 0, input(1), input(2));
    let outcome = client.command("generator-cut", "generator-cut-key", CommandPayload::Cut);
    assert_eq!(outcome.revision, 1);
    drop(client);

    let output = daemon.wait();
    assert!(
        output.status.success(),
        "generator daemon failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let persisted = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert_eq!(persisted.position().revision, 1);
    assert_eq!(
        persisted.runtime_routing().realized_program_id,
        Some(input(2))
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires a native macOS Metal adapter"]
fn native_scene_daemon_checkpoints_and_restarts_from_scene_routes() {
    let _hardware_lock = NATIVE_MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    let project_path = directory.path().join("native-scenes.freemix");
    save_scene_generator_project(&project_path);

    let mut previous_frames = 0;
    for _ in 0..2 {
        let Some(daemon) = require_native(NativeDaemonProcess::start_without_tools(
            &project_path,
            true,
        )) else {
            return;
        };
        thread::sleep(Duration::from_millis(100));
        let mut client = StudioClient::connect(daemon.address);
        let snapshot = client.handshake();
        assert_snapshot_routing(&snapshot, 0, input(3), input(4));
        drop(client);
        let output = daemon.wait();
        assert!(
            output.status.success(),
            "scene daemon failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let persisted = ProjectStore::new(&project_path).unwrap().load().unwrap();
        assert!(persisted.position().frames_rendered > previous_frames);
        previous_frames = persisted.position().frames_rendered;
        assert_eq!(
            persisted.runtime_routing().realized_program_id,
            Some(input(3))
        );
        assert_eq!(
            persisted.runtime_routing().realized_preview_id,
            Some(input(4))
        );
        assert_eq!(
            persisted.project().scenes()[2].layers[1].mask,
            Some(RectMask::new(8, 4, 32, 24).inverted(true))
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires a native macOS Metal adapter; camera input is hermetic"]
fn native_camera_daemon_renders_checkpoints_and_reaps_helper() {
    let _hardware_lock = NATIVE_MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    let helper = save_fake_camera_helper(directory.path());
    let project_path = directory.path().join("native-camera.freemix");
    save_camera_project(&project_path);

    let Some(daemon) = require_native(NativeDaemonProcess::start_camera(&project_path, &helper))
    else {
        return;
    };
    let output = daemon.wait();
    assert!(
        output.status.success(),
        "camera daemon failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    let source_telemetry = camera_source_diagnostic(&stderr);
    let telemetry = telemetry_diagnostic(&stderr);
    assert!(
        stderr.find("FREEMIXD_CAMERA_SOURCE\t").unwrap()
            < stderr.find("FREEMIXD_TELEMETRY\t").unwrap()
    );
    assert_eq!(diagnostic_value(source_telemetry, "v"), "1");
    assert_eq!(
        diagnostic_value(source_telemetry, "classification"),
        "diagnostic-not-certification"
    );
    assert_eq!(
        diagnostic_value(source_telemetry, "input_id"),
        input(1).to_string()
    );
    assert_eq!(
        diagnostic_value(source_telemetry, "sample_phase"),
        "pre_cleanup"
    );
    assert_eq!(
        diagnostic_value(source_telemetry, "sample_lifecycle"),
        "running"
    );
    assert_eq!(diagnostic_value(source_telemetry, "health"), "healthy");
    assert_eq!(diagnostic_value(telemetry, "metric_errors"), "0");
    assert_eq!(
        diagnostic_value(telemetry, "camera_configured_sources"),
        "1"
    );
    assert_eq!(diagnostic_value(telemetry, "camera_frames_received"), "12");
    assert_eq!(diagnostic_value(telemetry, "camera_native_dropped"), "2");
    assert_eq!(diagnostic_value(telemetry, "camera_queue_depth"), "0");
    assert_eq!(diagnostic_value(telemetry, "camera_queue_peak_depth"), "1");
    let camera_ingested = diagnostic_value(telemetry, "camera_frames_ingested")
        .parse::<u64>()
        .unwrap();
    let camera_queue_dropped = diagnostic_value(telemetry, "camera_queue_dropped")
        .parse::<u64>()
        .unwrap();
    for (source_key, aggregate_key) in [
        ("frames_received", "camera_frames_received"),
        ("frames_ingested", "camera_frames_ingested"),
        ("native_dropped", "camera_native_dropped"),
        ("queue_depth", "camera_queue_depth"),
        ("queue_peak_depth", "camera_queue_peak_depth"),
        ("queue_dropped", "camera_queue_dropped"),
    ] {
        assert_eq!(
            diagnostic_value(source_telemetry, source_key),
            diagnostic_value(telemetry, aggregate_key)
        );
    }
    assert!(camera_ingested > 1);
    assert_eq!(camera_ingested + camera_queue_dropped, 12);
    assert!(!source_telemetry.contains("fake-camera"));
    assert!(!source_telemetry.contains("stable_key"));
    assert!(!source_telemetry.contains("source_id"));
    assert!(
        diagnostic_value(telemetry, "host_lateness_samples_total")
            .parse::<u64>()
            .unwrap()
            > 0
    );
    let persisted = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert!(persisted.position().frames_rendered >= 20);
    assert!(persisted.position().clock_time_nanos > 0);

    let capture = fs::read_to_string(helper.with_extension("sh.capture")).unwrap();
    assert_eq!(capture, "capture\nfake-camera\n64\n48\n30000\n1001\n");
    assert!(!helper.with_extension("sh.permission").exists());
    let helper_pid = fs::read_to_string(helper.with_extension("sh.pid")).unwrap();
    assert!(
        !process_exists(helper_pid.trim()),
        "camera helper {helper_pid} survived daemon shutdown"
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires a native macOS Metal adapter and sends SIGINT during a Fade"]
fn native_generator_fade_signal_shutdown_checkpoints() {
    let _hardware_lock = NATIVE_MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    let project_path = directory.path().join("native-signal.freemix");
    save_generator_project(&project_path);

    let Some(mut daemon) = require_native(NativeDaemonProcess::start_without_tools(
        &project_path,
        false,
    )) else {
        return;
    };
    let mut client = StudioClient::connect(daemon.address);
    let initial = client.handshake();
    assert_snapshot_routing(&initial, 0, input(1), input(2));
    client.send(&WireMessage::Command(CommandMessage {
        protocol: CURRENT_PROTOCOL_VERSION,
        id: "signal-fade".into(),
        idempotency_key: "signal-fade-key".into(),
        expected_revision: None,
        deadline_ms: None,
        payload: CommandPayload::Fade {
            duration_frames: 3_600,
        },
    }));
    thread::sleep(Duration::from_millis(150));
    daemon.signal_interrupt();
    drop(client);

    let output = daemon.wait();
    assert!(
        output.status.success(),
        "signal shutdown failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    let telemetry = telemetry_diagnostic(&stderr);
    assert_eq!(diagnostic_value(telemetry, "presentation_active"), "false");
    assert_eq!(diagnostic_value(telemetry, "recorder_configured"), "false");
    assert!(
        diagnostic_value(telemetry, "host_lateness_samples_total")
            .parse::<u64>()
            .unwrap()
            > 0
    );
    assert_eq!(diagnostic_value(telemetry, "metric_errors"), "0");
    let persisted = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert_eq!(persisted.position().revision, 1);
    assert_eq!(
        persisted.runtime_routing().realized_program_id,
        Some(input(2))
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires FFmpeg with libx264/AAC, ffprobe, and a native macOS Metal adapter"]
#[allow(clippy::too_many_lines)]
fn native_generator_program_recording_is_playable_and_checkpointed() {
    const RESTORED_FRAME: u64 = 17;
    let _hardware_lock = NATIVE_MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    if require_recording_tools().is_none() {
        return;
    }
    let project_path = directory.path().join("record-generators.freemix");
    let output_path = directory.path().join("program.mp4");
    save_restored_generator_project(&project_path, RESTORED_FRAME);
    assert_ne!(
        u128::from(RESTORED_FRAME) * 48_000 * 1_001 % 30_000,
        0,
        "restored recording sequence must have an unaligned audio sample boundary"
    );

    let Some(mut daemon) = require_native_recorder(NativeDaemonProcess::start_recording(
        &project_path,
        &output_path,
    )) else {
        return;
    };
    let mut client = StudioClient::connect(daemon.address);
    let (capabilities_digest, initial) = client.handshake_with_digest();
    assert_eq!(
        capabilities_digest,
        "native-media-bounded-video-audio-master-camera-record-program-telemetry-v3"
    );
    assert_snapshot_routing(&initial, 0, input(1), input(2));
    thread::sleep(Duration::from_millis(1_500));
    daemon.signal_terminate();

    let output = daemon.wait_for(RECORDING_PROCESS_TIMEOUT);
    drop(client);
    assert!(
        output.status.success(),
        "recording daemon failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.matches("FREEMIXD_RECORDER\t").count(), 1);
    let diagnostic = stderr
        .lines()
        .find(|line| line.starts_with("FREEMIXD_RECORDER\t"))
        .unwrap_or_else(|| panic!("missing recorder diagnostic: {stderr}"));
    assert!(diagnostic.contains("\tv=1\tstate=Stopped\toutcome=Clean\t"));
    assert!(diagnostic.contains("\toutput_finalization=Synced\tcleanup=Complete\t"));
    assert!(diagnostic.contains("\tapp_capture_failure=false\tfailure=none"));
    let accepted = diagnostic_value(diagnostic, "accepted_pairs")
        .parse::<u64>()
        .unwrap();
    let completed = diagnostic_value(diagnostic, "completed_pairs")
        .parse::<u64>()
        .unwrap();
    let output_bytes = diagnostic_value(diagnostic, "output_bytes")
        .parse::<u64>()
        .unwrap();
    assert!(accepted >= 30, "too few accepted pairs: {diagnostic}");
    assert_eq!(completed, accepted);
    assert!(output_bytes > 0, "mux emitted no output: {diagnostic}");
    let telemetry = telemetry_diagnostic(&stderr);
    assert_eq!(diagnostic_value(telemetry, "presentation_active"), "false");
    assert_eq!(diagnostic_value(telemetry, "recorder_configured"), "true");
    assert_eq!(
        diagnostic_value(telemetry, "recorder_outstanding_pairs"),
        "0"
    );
    assert!(
        diagnostic_value(telemetry, "recorder_observed_peak_outstanding_pairs")
            .parse::<u64>()
            .unwrap()
            > 0
    );
    assert_eq!(diagnostic_value(telemetry, "gpu_timing"), "Supported");
    assert!(
        diagnostic_value(telemetry, "gpu_pass_samples_total")
            .parse::<u64>()
            .unwrap()
            > 0
    );
    assert_eq!(diagnostic_value(telemetry, "metric_errors"), "0");

    let video = ffprobe_stream(&output_path, "v:0").unwrap();
    assert_eq!(probe_value(&video, "codec_name"), "h264");
    assert_eq!(probe_value(&video, "width"), "64");
    assert_eq!(probe_value(&video, "height"), "48");
    assert_eq!(probe_value(&video, "r_frame_rate"), "30000/1001");
    assert!(probe_count(&video, "nb_read_frames") >= 30);
    assert!(probe_count(&video, "nb_read_packets") >= 30);

    let audio = ffprobe_stream(&output_path, "a:0").unwrap();
    assert_eq!(probe_value(&audio, "codec_name"), "aac");
    assert_eq!(probe_value(&audio, "sample_rate"), "48000");
    assert_eq!(probe_value(&audio, "channels"), "2");
    assert!(probe_count(&audio, "nb_read_frames") > 0);
    assert!(probe_count(&audio, "nb_read_packets") > 0);
    decode_recording(&output_path).unwrap();

    let persisted = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert!(persisted.position().frames_rendered >= RESTORED_FRAME + accepted);

    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let failed_output = directory.path().join("listener-failure.mp4");
    let child = Command::new(env!("CARGO_BIN_EXE_freemixd"))
        .arg("serve")
        .arg(&project_path)
        .arg("--native-media")
        .arg("--record-program")
        .arg(&failed_output)
        .arg("--listen")
        .arg(occupied.local_addr().unwrap().to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let failed = wait_bounded(child, RECORDING_PROCESS_TIMEOUT);
    assert!(!failed.status.success());
    assert!(!String::from_utf8_lossy(&failed.stdout).contains("FREEMIXD_READY"));
    let failed_stderr = String::from_utf8(failed.stderr).unwrap();
    assert_eq!(failed_stderr.matches("FREEMIXD_RECORDER\t").count(), 1);
    assert!(failed_stderr.contains("\tstate=Stopped\toutcome=Clean\t"));
    let failed_diagnostic = failed_stderr
        .lines()
        .find(|line| line.starts_with("FREEMIXD_RECORDER\t"))
        .unwrap();
    assert!(
        diagnostic_value(failed_diagnostic, "accepted_pairs")
            .parse::<u64>()
            .unwrap()
            >= 1
    );
    assert!(
        diagnostic_value(failed_diagnostic, "completed_pairs")
            .parse::<u64>()
            .unwrap()
            >= 1
    );
    assert!(failed_stderr.contains("Address already in use"));

    // Process-kill/playable-prefix evidence belongs to the codec boundary and
    // is covered by fm-codec-ffmpeg's `forced_child_kill_leaves_a_playable_fragmented_prefix`.
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires FFmpeg with libx264/AAC, ffprobe, and a native macOS Metal adapter"]
fn protocol_fade_to_black_reaches_configured_program_recording() {
    let _hardware_lock = NATIVE_MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    if require_recording_tools().is_none() {
        return;
    }
    let project_path = directory.path().join("record-ftb.freemix");
    let output_path = directory.path().join("program-ftb.mp4");
    if !prepare_native_project(&project_path) {
        return;
    }

    let Some(mut daemon) = require_native_recorder(NativeDaemonProcess::start_recording(
        &project_path,
        &output_path,
    )) else {
        return;
    };
    let mut client = StudioClient::connect(daemon.address);
    let initial = client.handshake();
    assert_snapshot_routing(&initial, 0, input(1), input(2));
    thread::sleep(Duration::from_millis(250));

    let black = client.command(
        "record-ftb-black",
        "record-ftb-black-key",
        CommandPayload::FadeToBlack {
            active: true,
            duration_frames: 4,
        },
    );
    assert_eq!(black.revision, 1);
    thread::sleep(Duration::from_millis(350));

    let live = client.command(
        "record-ftb-live",
        "record-ftb-live-key",
        CommandPayload::FadeToBlack {
            active: false,
            duration_frames: 4,
        },
    );
    assert_eq!(live.revision, 2);
    thread::sleep(Duration::from_millis(250));
    daemon.signal_terminate();

    let output = daemon.wait_for(RECORDING_PROCESS_TIMEOUT);
    drop(client);
    assert!(
        output.status.success(),
        "FTB recording daemon failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let luma = recording_average_luma(&output_path).unwrap();
    let (first_black, last_black) = assert_ordered_live_black_live(&luma);
    let black_start = f64::from(u32::try_from(first_black).unwrap()) / 25.0;
    let black_end = f64::from(u32::try_from(last_black + 1).unwrap()) / 25.0;
    let silence = recording_audio_silence_intervals(&output_path).unwrap();
    assert!(
        silence
            .iter()
            .any(|(start, end)| { end.min(black_end) - start.max(black_start) >= 0.10 }),
        "recorded Master silence {silence:?} does not overlap recorded black interval \
         {black_start:.3}..{black_end:.3}"
    );
    decode_recording(&output_path).unwrap();

    let persisted = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert_eq!(persisted.position().revision, 2);
    assert_eq!(
        persisted.runtime_fade_to_black(),
        fm_persistence::RuntimeFadeToBlack::default()
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires FFmpeg with libx264/AAC, ffprobe, and a native macOS Metal adapter"]
fn protocol_alpha_fade_reaches_configured_program_recording() {
    let _hardware_lock = NATIVE_MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    if require_recording_tools().is_none() {
        return;
    }
    let project_path = directory.path().join("record-alpha-fade.freemix");
    let output_path = directory.path().join("program-alpha-fade.mp4");
    save_alpha_fade_generator_project(&project_path);

    let Some(mut daemon) = require_native_recorder(NativeDaemonProcess::start_recording(
        &project_path,
        &output_path,
    )) else {
        return;
    };
    let mut client = StudioClient::connect(daemon.address);
    let initial = client.handshake();
    assert_snapshot_routing(&initial, 0, input(1), input(2));
    thread::sleep(Duration::from_millis(300));

    let alpha_fade = client.command(
        "record-alpha-fade",
        "record-alpha-fade-key",
        CommandPayload::AlphaFade {
            duration_frames: 12,
        },
    );
    assert_eq!(alpha_fade.revision, 1);
    thread::sleep(Duration::from_millis(300));
    daemon.signal_terminate();

    let output = daemon.wait_for(RECORDING_PROCESS_TIMEOUT);
    drop(client);
    assert!(
        output.status.success(),
        "AlphaFade recording daemon failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let luma = recording_average_luma(&output_path).unwrap();
    assert_ordered_opaque_alpha_fade_transparent(&luma);
    decode_recording(&output_path).unwrap();

    let persisted = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert_eq!(persisted.position().revision, 1);
    assert_eq!(
        persisted.runtime_routing(),
        RuntimeRouting {
            desired_program_id: Some(input(2)),
            realized_program_id: Some(input(2)),
            desired_preview_id: Some(input(1)),
            realized_preview_id: Some(input(1)),
        }
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires FFmpeg with libx264/AAC, ffprobe, and a native macOS Metal adapter"]
fn protocol_slide_reaches_configured_program_recording() {
    let _hardware_lock = NATIVE_MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    if require_recording_tools().is_none() {
        return;
    }
    let project_path = directory.path().join("record-slide.freemix");
    let output_path = directory.path().join("program-slide.mp4");
    save_white_black_generator_project(&project_path);

    let Some(mut daemon) = require_native_recorder(NativeDaemonProcess::start_recording(
        &project_path,
        &output_path,
    )) else {
        return;
    };
    let mut client = StudioClient::connect(daemon.address);
    let initial = client.handshake();
    assert_snapshot_routing(&initial, 0, input(1), input(2));
    thread::sleep(Duration::from_millis(300));

    let slide = client.command(
        "record-slide",
        "record-slide-key",
        CommandPayload::Slide {
            duration_frames: 12,
        },
    );
    assert_eq!(slide.revision, 1);
    thread::sleep(Duration::from_millis(300));
    daemon.signal_terminate();

    let output = daemon.wait_for(RECORDING_PROCESS_TIMEOUT);
    drop(client);
    assert!(
        output.status.success(),
        "Slide recording daemon failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let half_luma = recording_half_luma(&output_path).unwrap();
    assert_ordered_white_slide_black(&half_luma);
    decode_recording(&output_path).unwrap();

    let persisted = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert_eq!(persisted.position().revision, 1);
    assert_eq!(
        persisted.runtime_routing(),
        RuntimeRouting {
            desired_program_id: Some(input(2)),
            realized_program_id: Some(input(2)),
            desired_preview_id: Some(input(1)),
            realized_preview_id: Some(input(1)),
        }
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires FFmpeg with libx264/AAC, ffprobe, and a native macOS Metal adapter"]
fn protocol_zoom_reaches_configured_program_recording() {
    let _hardware_lock = NATIVE_MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    if require_recording_tools().is_none() {
        return;
    }
    let project_path = directory.path().join("record-zoom.freemix");
    let output_path = directory.path().join("program-zoom.mp4");
    save_white_black_generator_project(&project_path);

    let Some(mut daemon) = require_native_recorder(NativeDaemonProcess::start_recording(
        &project_path,
        &output_path,
    )) else {
        return;
    };
    let mut client = StudioClient::connect(daemon.address);
    let initial = client.handshake();
    assert_snapshot_routing(&initial, 0, input(1), input(2));
    thread::sleep(Duration::from_millis(300));

    let zoom = client.command(
        "record-zoom",
        "record-zoom-key",
        CommandPayload::Zoom {
            duration_frames: 12,
        },
    );
    assert_eq!(zoom.revision, 1);
    thread::sleep(Duration::from_millis(300));
    daemon.signal_terminate();

    let output = daemon.wait_for(RECORDING_PROCESS_TIMEOUT);
    drop(client);
    assert!(
        output.status.success(),
        "Zoom recording daemon failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let grid_luma = recording_grid_luma(&output_path).unwrap();
    assert_ordered_white_centered_zoom_black(&grid_luma);
    decode_recording(&output_path).unwrap();

    let persisted = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert_eq!(persisted.position().revision, 1);
    assert_eq!(
        persisted.runtime_routing(),
        RuntimeRouting {
            desired_program_id: Some(input(2)),
            realized_program_id: Some(input(2)),
            desired_preview_id: Some(input(1)),
            realized_preview_id: Some(input(1)),
        }
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires FFmpeg with FFV1/libx264/AAC, ffprobe, and a native macOS Metal adapter"]
fn protocol_stinger_retriggers_alpha_clip_and_survives_daemon_restart() {
    let _hardware_lock = NATIVE_MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    if require_recording_tools().is_none() {
        return;
    }
    let project_path = directory.path().join("record-stinger.freemix");
    let output_path = directory.path().join("program-stinger.mp4");
    if require_native(prepare_stinger_project(&project_path)).is_none() {
        return;
    }

    let Some(mut daemon) = require_native_recorder(NativeDaemonProcess::start_recording(
        &project_path,
        &output_path,
    )) else {
        return;
    };
    let mut client = StudioClient::connect(daemon.address);
    let initial = client.handshake();
    assert_snapshot_routing(&initial, 0, input(1), input(2));
    thread::sleep(Duration::from_millis(300));

    let payload = || CommandPayload::Stinger {
        slot: WireStingerSlotId::new(1).unwrap(),
        duration_frames: 12,
    };
    let first = client.command("record-stinger-1", "record-stinger-key-1", payload());
    assert_eq!(first.revision, 1);
    let second = client.command("record-stinger-2", "record-stinger-key-2", payload());
    assert_eq!(second.revision, 2);
    assert!(second.scheduled_frame >= first.scheduled_frame + 12);
    assert!(second.scheduled_frame <= first.scheduled_frame + 13);
    thread::sleep(Duration::from_millis(650));
    daemon.signal_terminate();

    let output = daemon.wait_for(RECORDING_PROCESS_TIMEOUT);
    drop(client);
    assert!(
        output.status.success(),
        "Stinger recording daemon failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let luma = recording_average_luma(&output_path).unwrap();
    assert_ordered_stinger_retrigger(&luma);
    decode_recording(&output_path).unwrap();

    let persisted = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert_eq!(persisted.position().revision, 2);
    assert_eq!(
        persisted.runtime_routing(),
        RuntimeRouting {
            desired_program_id: Some(input(1)),
            realized_program_id: Some(input(1)),
            desired_preview_id: Some(input(2)),
            realized_preview_id: Some(input(2)),
        }
    );

    let restart_output = directory.path().join("program-stinger-restart.mp4");
    let Some(mut restarted) = require_native_recorder(NativeDaemonProcess::start_recording(
        &project_path,
        &restart_output,
    )) else {
        return;
    };
    let mut restarted_client = StudioClient::connect(restarted.address);
    let snapshot = restarted_client.handshake();
    assert_snapshot_routing(&snapshot, 2, input(1), input(2));
    thread::sleep(Duration::from_millis(300));
    let third = restarted_client.command("record-stinger-3", "record-stinger-key-3", payload());
    assert_eq!(third.revision, 3);
    thread::sleep(Duration::from_millis(650));
    restarted.signal_terminate();
    let restart = restarted.wait_for(RECORDING_PROCESS_TIMEOUT);
    drop(restarted_client);
    assert!(
        restart.status.success(),
        "restarted Stinger daemon failed: {}",
        String::from_utf8_lossy(&restart.stderr)
    );
    let restart_luma = recording_average_luma(&restart_output).unwrap();
    assert_ordered_single_stinger(&restart_luma);
    decode_recording(&restart_output).unwrap();
    let restarted_persisted = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert_eq!(restarted_persisted.position().revision, 3);
    assert_eq!(
        restarted_persisted.runtime_routing(),
        RuntimeRouting {
            desired_program_id: Some(input(2)),
            realized_program_id: Some(input(2)),
            desired_preview_id: Some(input(1)),
            realized_preview_id: Some(input(1)),
        }
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires FFmpeg with FFV1/libx264/AAC, ffprobe, and a native macOS Metal adapter"]
fn protocol_stinger_audio_policies_replay_clip_local_audio() {
    let _hardware_lock = NATIVE_MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    if require_recording_tools().is_none() {
        return;
    }
    let project_path = directory.path().join("record-stinger-audio.freemix");
    let output_path = directory.path().join("program-stinger-audio.mp4");
    if require_native(prepare_audible_stinger_project(&project_path)).is_none() {
        return;
    }
    let recording_origin = ProjectStore::new(&project_path)
        .unwrap()
        .load()
        .unwrap()
        .position()
        .frames_rendered;

    let Some(mut daemon) = require_native_recorder(NativeDaemonProcess::start_recording(
        &project_path,
        &output_path,
    )) else {
        return;
    };
    let mut client = StudioClient::connect(daemon.address);
    let initial = client.handshake();
    assert_snapshot_routing(&initial, 0, input(1), input(2));
    thread::sleep(Duration::from_millis(300));

    let stinger = |slot| CommandPayload::Stinger {
        slot: WireStingerSlotId::new(slot).unwrap(),
        duration_frames: 12,
    };
    let mut scheduled_frames = Vec::new();
    for (index, (label, slot, hold_ms)) in [
        ("muted", 1, 650),
        ("only", 2, 650),
        ("mixed", 3, 650),
        ("replay-a", 2, 650),
        ("replay-b", 2, 650),
    ]
    .into_iter()
    .enumerate()
    {
        let result = client.command(
            &format!("stinger-audio-{label}"),
            &format!("stinger-audio-{label}-key"),
            stinger(slot),
        );
        assert_eq!(result.revision, u64::try_from(index + 1).unwrap());
        scheduled_frames.push(result.scheduled_frame);
        thread::sleep(Duration::from_millis(hold_ms));
    }
    daemon.signal_terminate();

    let output = daemon.wait_for(RECORDING_PROCESS_TIMEOUT);
    drop(client);
    assert!(
        output.status.success(),
        "Stinger audio recording daemon failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let signatures = recording_audio_signatures(&output_path).unwrap();
    assert_scheduled_stinger_audio_policies(&signatures, recording_origin, &scheduled_frames);
    decode_recording(&output_path).unwrap();

    let persisted = ProjectStore::new(&project_path).unwrap().load().unwrap();
    let recorded_video = ffprobe_stream(&output_path, "v:0").unwrap();
    let recorded_frames = probe_count(&recorded_video, "nb_read_frames");
    let checkpoint_cursor = persisted.position().frames_rendered;
    assert_eq!(
        checkpoint_cursor,
        recording_origin
            .checked_add(recorded_frames)
            .expect("recording cursor must not overflow"),
        "persisted cursor must advance by exactly the independently encoded video-frame count"
    );
    assert_eq!(persisted.position().revision, 5);
    assert_eq!(
        persisted.runtime_routing(),
        RuntimeRouting {
            desired_program_id: Some(input(2)),
            realized_program_id: Some(input(2)),
            desired_preview_id: Some(input(1)),
            realized_preview_id: Some(input(1)),
        }
    );

    verify_restarted_stinger_audio(
        &project_path,
        &directory.path().join("program-stinger-audio-restart.mp4"),
        checkpoint_cursor,
    );
}

#[cfg(target_os = "macos")]
fn verify_restarted_stinger_audio(project_path: &Path, output_path: &Path, recording_origin: u64) {
    let mut restarted = require_native_recorder(NativeDaemonProcess::start_recording(
        project_path,
        output_path,
    ))
    .expect("native recorder restart must succeed after the initial recording");
    let mut restarted_client = StudioClient::connect(restarted.address);
    let snapshot = restarted_client.handshake();
    assert_snapshot_routing(&snapshot, 5, input(2), input(1));
    thread::sleep(Duration::from_millis(300));
    let replay = restarted_client.command(
        "stinger-audio-restored",
        "stinger-audio-restored-key",
        CommandPayload::Stinger {
            slot: WireStingerSlotId::new(2).unwrap(),
            duration_frames: 12,
        },
    );
    assert_eq!(replay.revision, 6);
    thread::sleep(Duration::from_millis(500));
    restarted.signal_terminate();
    let restart = restarted.wait_for(RECORDING_PROCESS_TIMEOUT);
    drop(restarted_client);
    assert!(
        restart.status.success(),
        "restarted Stinger audio daemon failed: {}",
        String::from_utf8_lossy(&restart.stderr)
    );
    let restart_signatures = recording_audio_signatures(output_path).unwrap();
    assert_scheduled_clip_only_replay(
        &restart_signatures,
        recording_origin,
        replay.scheduled_frame,
    );
    decode_recording(output_path).unwrap();
    let restarted_persisted = ProjectStore::new(project_path).unwrap().load().unwrap();
    assert_eq!(restarted_persisted.position().revision, 6);
    assert_eq!(
        restarted_persisted.runtime_routing(),
        RuntimeRouting {
            desired_program_id: Some(input(1)),
            realized_program_id: Some(input(1)),
            desired_preview_id: Some(input(2)),
            realized_preview_id: Some(input(2)),
        }
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires FFmpeg with libx264/AAC, ffprobe, and a native macOS Metal adapter"]
fn protocol_stinger_not_requested_media_applies_all_configured_fallbacks() {
    let _hardware_lock = NATIVE_MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    if require_recording_tools().is_none() {
        return;
    }
    let project_path = directory.path().join("record-stinger-fallbacks.freemix");
    let output_path = directory.path().join("program-stinger-fallbacks.mp4");
    prepare_deferred_stinger_fallback_project(&project_path);

    let Some(mut daemon) = require_native_recorder(NativeDaemonProcess::start_recording(
        &project_path,
        &output_path,
    )) else {
        return;
    };
    let mut client = StudioClient::connect(daemon.address);
    let initial = client.handshake();
    assert_snapshot_routing(&initial, 0, input(1), input(2));
    thread::sleep(Duration::from_millis(240));

    let stinger = |slot| CommandPayload::Stinger {
        slot: WireStingerSlotId::new(slot).unwrap(),
        duration_frames: 4,
    };
    let keep_white = client.command("fallback-keep-white", "fallback-keep-white-key", stinger(1));
    assert_eq!(keep_white.revision, 1);
    thread::sleep(Duration::from_millis(200));
    let fade_black = client.command("fallback-fade-black", "fallback-fade-black-key", stinger(2));
    assert_eq!(fade_black.revision, 2);
    thread::sleep(Duration::from_millis(200));
    let keep_black = client.command("fallback-keep-black", "fallback-keep-black-key", stinger(1));
    assert_eq!(keep_black.revision, 3);
    thread::sleep(Duration::from_millis(200));
    let cut_white = client.command("fallback-cut-white", "fallback-cut-white-key", stinger(3));
    assert_eq!(cut_white.revision, 4);
    thread::sleep(Duration::from_millis(240));
    daemon.signal_terminate();

    let output = daemon.wait_for(RECORDING_PROCESS_TIMEOUT);
    drop(client);
    assert!(
        output.status.success(),
        "Stinger fallback recording daemon failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let luma = recording_average_luma(&output_path).unwrap();
    assert_ordered_stinger_fallbacks(&luma);
    decode_recording(&output_path).unwrap();

    let persisted = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert_eq!(persisted.position().revision, 4);
    assert_eq!(
        persisted.runtime_routing(),
        RuntimeRouting {
            desired_program_id: Some(input(1)),
            realized_program_id: Some(input(1)),
            desired_preview_id: Some(input(2)),
            realized_preview_id: Some(input(2)),
        }
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires FFmpeg with libx264/AAC, ffprobe, and a native macOS Metal adapter"]
fn protocol_manual_fade_reversal_reaches_configured_program_recording() {
    let _hardware_lock = NATIVE_MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    if require_recording_tools().is_none() {
        return;
    }
    let project_path = directory.path().join("record-manual-fade.freemix");
    let output_path = directory.path().join("program-manual-fade.mp4");
    if !prepare_native_project(&project_path) {
        return;
    }

    let Some(luma) = record_manual_transition_reversal(
        &project_path,
        &output_path,
        ManualTransitionKind::Fade,
        "manual Fade",
    ) else {
        return;
    };
    assert_ordered_manual_forward_reverse_commit(&luma);

    let persisted = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert_eq!(persisted.position().revision, 7);
    assert_eq!(
        persisted.runtime_manual_transitions(),
        fm_persistence::RuntimeManualTransitions::default()
    );
    assert_eq!(
        persisted.runtime_routing(),
        RuntimeRouting {
            desired_program_id: Some(input(2)),
            realized_program_id: Some(input(2)),
            desired_preview_id: Some(input(1)),
            realized_preview_id: Some(input(1)),
        }
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires FFmpeg with libx264/AAC, ffprobe, and a native macOS Metal adapter"]
fn protocol_manual_alpha_fade_reversal_reaches_configured_program_recording() {
    let _hardware_lock = NATIVE_MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    if require_recording_tools().is_none() {
        return;
    }
    let project_path = directory.path().join("record-manual-alpha-fade.freemix");
    let output_path = directory.path().join("program-manual-alpha-fade.mp4");
    save_alpha_fade_generator_project(&project_path);

    let Some(luma) = record_manual_transition_reversal(
        &project_path,
        &output_path,
        ManualTransitionKind::AlphaFade,
        "manual AlphaFade",
    ) else {
        return;
    };
    assert_ordered_manual_alpha_fade_forward_reverse_commit(&luma);

    let persisted = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert_eq!(persisted.position().revision, 7);
    assert_eq!(
        persisted.runtime_manual_transitions(),
        fm_persistence::RuntimeManualTransitions::default()
    );
    assert_eq!(
        persisted.runtime_routing(),
        RuntimeRouting {
            desired_program_id: Some(input(2)),
            realized_program_id: Some(input(2)),
            desired_preview_id: Some(input(1)),
            realized_preview_id: Some(input(1)),
        }
    );
}

#[cfg(all(target_os = "macos", feature = "macos-program-surface"))]
#[test]
#[ignore = "opens a fullscreen macOS Program surface and requires a Metal adapter"]
fn fullscreen_program_generated_bars_presents_and_exits_once() {
    let _hardware_lock = NATIVE_MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    let project_path = directory.path().join("fullscreen-generators.freemix");
    save_generator_project(&project_path);
    assert!(!project_path.join("assets").exists());

    let Some(daemon) = require_native(NativeDaemonProcess::start_fullscreen_without_tools(
        &project_path,
    )) else {
        return;
    };
    let mut client = StudioClient::connect(daemon.address);
    let (capabilities_digest, initial) = client.handshake_with_digest();
    assert_eq!(
        capabilities_digest,
        "native-media-bounded-video-audio-master-camera-fullscreen-sdr-telemetry-v3"
    );
    assert_snapshot_routing(&initial, 0, input(1), input(2));
    thread::sleep(Duration::from_millis(200));
    drop(client);

    let output = daemon.wait();
    assert!(
        output.status.success(),
        "fullscreen daemon failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    let frames_presented = stderr
        .lines()
        .find_map(|line| {
            line.strip_prefix("FREEMIXD_PROGRAM\tv=1\tframes_presented=")
                .and_then(|value| value.parse::<u64>().ok())
        })
        .unwrap_or_else(|| panic!("missing Program shutdown diagnostic: {stderr}"));
    assert!(
        frames_presented > 0,
        "Program lifecycle acknowledged no presented frames: {stderr}"
    );
    let telemetry = telemetry_diagnostic(&stderr);
    assert_eq!(diagnostic_value(telemetry, "presentation_active"), "true");
    assert_eq!(
        diagnostic_value(telemetry, "presentation_pending_depth"),
        "0"
    );
    assert_eq!(
        diagnostic_value(telemetry, "presentation_peak_pending_depth"),
        "1"
    );
    assert_eq!(diagnostic_value(telemetry, "gpu_timing"), "Supported");
    assert!(
        diagnostic_value(telemetry, "gpu_pass_samples_total")
            .parse::<u64>()
            .unwrap()
            > 0
    );
    assert_eq!(diagnostic_value(telemetry, "metric_errors"), "0");
    assert!(
        ProjectStore::new(&project_path)
            .unwrap()
            .load()
            .unwrap()
            .position()
            .frames_rendered
            > 0
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires FFmpeg with libx264 and a native macOS Metal adapter"]
fn native_media_daemon_survives_studio_transport_disconnect() {
    let _hardware_lock = NATIVE_MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    let project_path = directory.path().join("native-reconnect.freemix");
    if !prepare_native_project(&project_path) {
        return;
    }
    let Some(mut daemon) = require_native(NativeDaemonProcess::start(&project_path, false)) else {
        return;
    };

    let mut first_client = StudioClient::connect(daemon.address);
    let initial = first_client.handshake();
    assert_snapshot_routing(&initial, 0, input(1), input(2));
    let first = first_client.command("first-cut", "first-cut-key", CommandPayload::Cut);
    drop(first_client);

    thread::sleep(FRAME_PERIOD * 4);
    assert!(
        daemon.process.is_alive(),
        "non-once daemon exited after Studio disconnected"
    );

    let mut second_client = StudioClient::connect(daemon.address);
    let reconnected = second_client.handshake();
    assert_snapshot_routing(&reconnected, 1, input(2), input(1));
    let second = second_client.command("second-cut", "second-cut-key", CommandPayload::Cut);
    assert_eq!(second.revision, 2);
    assert!(
        second.scheduled_frame >= first.scheduled_frame.saturating_add(3),
        "reconnected command frame {} was not materially later than {}",
        second.scheduled_frame,
        first.scheduled_frame
    );
    drop(second_client);

    let output = daemon.kill_and_reap();
    assert!(
        output.stderr.is_empty(),
        "native daemon emitted stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires FFmpeg with libx264 and a native macOS Metal adapter"]
fn native_media_fade_and_wipe_are_wall_clock_paced_and_checkpointed() {
    let _hardware_lock = NATIVE_MEDIA_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    let project_path = directory.path().join("native-fade.freemix");
    if !prepare_native_project(&project_path) {
        return;
    }
    let Some(daemon) = require_native(NativeDaemonProcess::start(&project_path, true)) else {
        return;
    };

    let mut client = StudioClient::connect(daemon.address);
    let initial = client.handshake();
    assert_snapshot_routing(&initial, 0, input(1), input(2));
    let outcome = client.command(
        "paced-fade",
        "paced-fade-key",
        CommandPayload::Fade { duration_frames: 4 },
    );
    assert!(
        outcome.elapsed >= Duration::from_millis(100),
        "four-frame Fade collapsed into {:?}",
        outcome.elapsed
    );
    assert!(
        outcome.elapsed < PROCESS_TIMEOUT,
        "four-frame Fade exceeded socket timeout: {:?}",
        outcome.elapsed
    );
    let wipe = client.command(
        "paced-wipe",
        "paced-wipe-key",
        CommandPayload::Wipe { duration_frames: 4 },
    );
    assert!(
        wipe.elapsed >= Duration::from_millis(100),
        "four-frame Wipe collapsed into {:?}",
        wipe.elapsed
    );
    assert!(
        wipe.elapsed < PROCESS_TIMEOUT,
        "four-frame Wipe exceeded socket timeout: {:?}",
        wipe.elapsed
    );
    drop(client);

    let output = daemon.wait();
    assert!(
        output.status.success(),
        "native daemon failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let persisted = ProjectStore::new(&project_path).unwrap().load().unwrap();
    assert_eq!(persisted.position().revision, 2);
    assert_eq!(
        persisted.position().frames_rendered,
        wipe.scheduled_frame + 4
    );
    assert_eq!(
        persisted.runtime_routing().desired_program_id,
        Some(input(1))
    );
    assert_eq!(
        persisted.runtime_routing().realized_program_id,
        Some(input(1))
    );
    assert_eq!(
        persisted.runtime_routing().desired_preview_id,
        Some(input(2))
    );
    assert_eq!(
        persisted.runtime_routing().realized_preview_id,
        Some(input(2))
    );
}

#[cfg(target_os = "macos")]
struct NativeDaemonProcess {
    process: CapturedChild,
    address: SocketAddr,
}

#[cfg(target_os = "macos")]
impl NativeDaemonProcess {
    fn start(project_path: &Path, once: bool) -> Result<Self, String> {
        Self::start_with_path(project_path, once, None)
    }

    fn start_without_tools(project_path: &Path, once: bool) -> Result<Self, String> {
        Self::start_with_path(project_path, once, Some("/freemix-no-external-tools"))
    }

    fn start_recording(project_path: &Path, output_path: &Path) -> Result<Self, String> {
        Self::start_with_options(project_path, false, None, false, Some(output_path))
    }

    fn start_camera(project_path: &Path, helper: &Path) -> Result<Self, String> {
        let child = Command::new(env!("CARGO_BIN_EXE_freemixd"))
            .arg("serve")
            .arg(project_path)
            .args(["--native-media", "--camera-helper"])
            .arg(helper)
            .args(["--diagnostic-stop-after", "1s"])
            .env("PATH", "/freemix-no-external-tools")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("cannot spawn camera daemon: {error}"))?;
        let (process, readiness) = CapturedChild::with_readiness(child);
        let line = match readiness.recv_timeout(PROCESS_TIMEOUT) {
            Ok(Ok(line)) => line,
            Ok(Err(error)) => return Err(startup_failure(process, &error)),
            Err(error) => return Err(startup_failure(process, &error.to_string())),
        };
        match line.parse::<ReadinessRecord>() {
            Ok(readiness) => {
                if let Err(error) = fs::write(helper.with_extension("sh.release"), b"release") {
                    return Err(startup_failure(
                        process,
                        &format!("cannot release camera updates: {error}"),
                    ));
                }
                Ok(Self {
                    process,
                    address: readiness.address,
                })
            }
            Err(error) => Err(startup_failure(
                process,
                &format!("invalid readiness record {line:?}: {error}"),
            )),
        }
    }

    #[cfg(feature = "macos-program-surface")]
    fn start_fullscreen_without_tools(project_path: &Path) -> Result<Self, String> {
        Self::start_with_options(
            project_path,
            true,
            Some("/freemix-no-external-tools"),
            true,
            None,
        )
    }

    fn start_with_path(
        project_path: &Path,
        once: bool,
        path: Option<&str>,
    ) -> Result<Self, String> {
        Self::start_with_options(project_path, once, path, false, None)
    }

    fn start_with_options(
        project_path: &Path,
        once: bool,
        path: Option<&str>,
        fullscreen: bool,
        record_program: Option<&Path>,
    ) -> Result<Self, String> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_freemixd"));
        command.arg("serve").arg(project_path).arg("--native-media");
        if fullscreen {
            command
                .arg("--fullscreen-program")
                .args(["--fullscreen-display", "0"]);
        }
        if let Some(output) = record_program {
            command.arg("--record-program").arg(output);
        }
        if once {
            command.arg("--once");
        }
        if let Some(path) = path {
            command.env("PATH", path);
        }
        let child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("cannot spawn native daemon: {error}"))?;
        let (process, readiness) = CapturedChild::with_readiness(child);
        let line = match readiness.recv_timeout(PROCESS_TIMEOUT) {
            Ok(Ok(line)) => line,
            Ok(Err(error)) => return Err(startup_failure(process, &error)),
            Err(error) => return Err(startup_failure(process, &error.to_string())),
        };
        match line.parse::<ReadinessRecord>() {
            Ok(readiness) => Ok(Self {
                process,
                address: readiness.address,
            }),
            Err(error) => Err(startup_failure(
                process,
                &format!("invalid readiness record {line:?}: {error}"),
            )),
        }
    }

    fn wait(self) -> BoundedOutput {
        self.process.wait(PROCESS_TIMEOUT)
    }

    fn wait_for(self, timeout: Duration) -> BoundedOutput {
        self.process.wait(timeout)
    }

    fn signal_terminate(&mut self) {
        self.signal_process("-TERM");
    }

    fn signal_interrupt(&mut self) {
        self.signal_process("-INT");
    }

    fn signal_process(&mut self, signal: &str) {
        let process_id = self
            .process
            .child
            .as_ref()
            .expect("captured child is present")
            .id()
            .to_string();
        let status = Command::new("kill")
            .args([signal, &process_id])
            .status()
            .expect("invoke kill for daemon signal");
        assert!(status.success(), "kill failed with {status}");
    }

    fn kill_and_reap(self) -> BoundedOutput {
        self.process.kill_and_reap()
    }
}

#[cfg(target_os = "macos")]
fn startup_failure(process: CapturedChild, reason: &str) -> String {
    let output = process.kill_and_reap();
    format!(
        "native daemon unavailable: {reason}; stdout={:?}; stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[cfg(target_os = "macos")]
fn require_native<T>(result: Result<T, String>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            assert!(
                std::env::var("FM_REQUIRE_NATIVE_MEDIA").as_deref() != Ok("1"),
                "FM_REQUIRE_NATIVE_MEDIA=1: {error}"
            );
            eprintln!("native daemon integration skipped: {error}");
            None
        }
    }
}

#[cfg(target_os = "macos")]
fn require_native_recorder<T>(result: Result<T, String>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            assert!(
                std::env::var("FM_REQUIRE_NATIVE_MEDIA").as_deref() != Ok("1")
                    && std::env::var("FM_REQUIRE_FFMPEG").as_deref() != Ok("1"),
                "required native recorder integration failed: {error}"
            );
            eprintln!("native recorder integration skipped: {error}");
            None
        }
    }
}

#[cfg(target_os = "macos")]
fn require_recording_tools() -> Option<()> {
    for tool in ["ffmpeg", "ffprobe"] {
        let result = Command::new(tool)
            .arg("-version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| format!("{tool} is unavailable: {error}"))
            .and_then(|status| {
                status
                    .success()
                    .then_some(())
                    .ok_or_else(|| format!("{tool} -version failed with {status}"))
            });
        if let Err(error) = result {
            assert!(
                std::env::var("FM_REQUIRE_FFMPEG").as_deref() != Ok("1"),
                "FM_REQUIRE_FFMPEG=1: {error}"
            );
            eprintln!("native recorder integration skipped: {error}");
            return None;
        }
    }
    Some(())
}

#[cfg(target_os = "macos")]
fn diagnostic_value<'a>(diagnostic: &'a str, key: &str) -> &'a str {
    diagnostic
        .split('\t')
        .find_map(|field| {
            field
                .strip_prefix(key)
                .and_then(|value| value.strip_prefix('='))
        })
        .unwrap_or_else(|| panic!("missing {key} in diagnostic: {diagnostic}"))
}

#[cfg(target_os = "macos")]
fn telemetry_diagnostic(stderr: &str) -> &str {
    assert_eq!(stderr.matches("FREEMIXD_TELEMETRY\t").count(), 1);
    let diagnostic = stderr
        .lines()
        .find(|line| line.starts_with("FREEMIXD_TELEMETRY\t"))
        .unwrap_or_else(|| panic!("missing telemetry diagnostic: {stderr}"));
    assert_eq!(diagnostic_value(diagnostic, "v"), "4");
    diagnostic
}

#[cfg(target_os = "macos")]
fn camera_source_diagnostic(stderr: &str) -> &str {
    assert_eq!(stderr.matches("FREEMIXD_CAMERA_SOURCE\t").count(), 1);
    stderr
        .lines()
        .find(|line| line.starts_with("FREEMIXD_CAMERA_SOURCE\t"))
        .unwrap_or_else(|| panic!("missing camera source diagnostic: {stderr}"))
}

#[cfg(target_os = "macos")]
fn ffprobe_stream(path: &Path, selector: &str) -> Result<String, String> {
    let child = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-count_packets",
            "-select_streams",
            selector,
            "-show_entries",
            "stream=codec_name,width,height,r_frame_rate,sample_rate,channels,nb_read_frames,nb_read_packets",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot spawn ffprobe: {error}"))?;
    let output = wait_bounded(child, PROCESS_TIMEOUT);
    if output.status.success() {
        String::from_utf8(output.stdout)
            .map_err(|error| format!("ffprobe output is invalid: {error}"))
    } else {
        Err(format!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

#[cfg(target_os = "macos")]
fn probe_value<'a>(probe: &'a str, key: &str) -> &'a str {
    probe
        .lines()
        .find_map(|line| {
            line.strip_prefix(key)
                .and_then(|value| value.strip_prefix('='))
        })
        .unwrap_or_else(|| panic!("missing {key} in ffprobe output: {probe}"))
}

#[cfg(target_os = "macos")]
fn probe_count(probe: &str, key: &str) -> u64 {
    probe_value(probe, key)
        .parse()
        .unwrap_or_else(|error| panic!("invalid {key} in ffprobe output: {error}; {probe}"))
}

#[cfg(target_os = "macos")]
fn decode_recording(path: &Path) -> Result<(), String> {
    let child = Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-map", "0:a:0", "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot spawn FFmpeg decoder: {error}"))?;
    let output = wait_bounded(child, PROCESS_TIMEOUT);
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "recording decode failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

#[cfg(target_os = "macos")]
fn recording_average_luma(path: &Path) -> Result<Vec<u8>, String> {
    let child = Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:v:0",
            "-vf",
            "scale=1:1:flags=area",
            "-pix_fmt",
            "gray",
            "-f",
            "rawvideo",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot spawn FFmpeg luma decoder: {error}"))?;
    let output = wait_bounded(child, PROCESS_TIMEOUT);
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "recording luma decode failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

#[cfg(target_os = "macos")]
fn recording_half_luma(path: &Path) -> Result<Vec<[u8; 2]>, String> {
    let child = Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:v:0",
            "-vf",
            "scale=2:1:flags=area",
            "-pix_fmt",
            "gray",
            "-f",
            "rawvideo",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot spawn FFmpeg half-luma decoder: {error}"))?;
    let output = wait_bounded(child, PROCESS_TIMEOUT);
    if !output.status.success() {
        return Err(format!(
            "recording half-luma decode failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let chunks = output.stdout.chunks_exact(2);
    if !chunks.remainder().is_empty() {
        return Err("recording half-luma output ended mid-frame".to_owned());
    }
    Ok(chunks.map(|frame| [frame[0], frame[1]]).collect())
}

#[cfg(target_os = "macos")]
fn recording_grid_luma(path: &Path) -> Result<Vec<[u8; 9]>, String> {
    let child = Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:v:0",
            "-vf",
            "scale=3:3:flags=area",
            "-pix_fmt",
            "gray",
            "-f",
            "rawvideo",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot spawn FFmpeg grid-luma decoder: {error}"))?;
    let output = wait_bounded(child, PROCESS_TIMEOUT);
    if !output.status.success() {
        return Err(format!(
            "recording grid-luma decode failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let chunks = output.stdout.chunks_exact(9);
    if !chunks.remainder().is_empty() {
        return Err("recording grid-luma output ended mid-frame".to_owned());
    }
    Ok(chunks
        .map(|frame| {
            frame
                .try_into()
                .expect("grid-luma frames contain nine bytes")
        })
        .collect())
}

#[cfg(target_os = "macos")]
fn assert_ordered_white_centered_zoom_black(frames: &[[u8; 9]]) {
    const REQUIRED_FRAMES: usize = 3;

    let white = grid_luma_run(frames, 0, REQUIRED_FRAMES, |frame| {
        frame.iter().all(|value| *value >= 205)
    })
    .unwrap_or_else(|| panic!("recording has no stable white Program interval: {frames:?}"));
    let centered = grid_luma_run(frames, white + REQUIRED_FRAMES, 1, |frame| {
        frame[4] <= 64
            && frame
                .iter()
                .enumerate()
                .all(|(index, value)| index == 4 || *value >= 180)
    })
    .unwrap_or_else(|| {
        panic!("recording has no Zoom frame with a black center and white perimeter: {frames:?}")
    });
    grid_luma_run(frames, centered + 1, REQUIRED_FRAMES, |frame| {
        frame.iter().all(|value| *value <= 32)
    })
    .unwrap_or_else(|| panic!("recording has no stable black Program interval: {frames:?}"));
}

#[cfg(target_os = "macos")]
fn assert_ordered_stinger_retrigger(luma: &[u8]) {
    const REQUIRED_FRAMES: usize = 3;

    let white = luma_run(luma, 0, 245, u8::MAX, REQUIRED_FRAMES)
        .unwrap_or_else(|| panic!("recording has no initial white interval: {luma:?}"));
    let black = luma_run(luma, white + REQUIRED_FRAMES, 0, 32, 1)
        .unwrap_or_else(|| panic!("first Stinger did not settle on black: {luma:?}"));
    assert!(
        luma[white + REQUIRED_FRAMES..black]
            .iter()
            .any(|value| (40..=190).contains(value)),
        "first Stinger contains no decoded alpha-media frame: {luma:?}"
    );

    let final_white = luma_run(luma, black + 1, 245, u8::MAX, REQUIRED_FRAMES)
        .unwrap_or_else(|| panic!("retriggered Stinger did not settle on white: {luma:?}"));
    assert!(
        luma[black + 1..final_white]
            .iter()
            .any(|value| (40..=245).contains(value)),
        "retriggered Stinger contains no decoded alpha-media frame: {luma:?}"
    );
}

#[cfg(target_os = "macos")]
fn assert_ordered_single_stinger(luma: &[u8]) {
    const REQUIRED_FRAMES: usize = 3;

    let white = luma_run(luma, 0, 245, u8::MAX, REQUIRED_FRAMES)
        .unwrap_or_else(|| panic!("recording has no initial white interval: {luma:?}"));
    let black = luma_run(luma, white + REQUIRED_FRAMES, 0, 32, REQUIRED_FRAMES)
        .unwrap_or_else(|| panic!("Stinger did not settle on black: {luma:?}"));
    assert!(
        luma[white + REQUIRED_FRAMES..black]
            .iter()
            .any(|value| (40..=245).contains(value)),
        "Stinger contains no decoded alpha-media frame: {luma:?}"
    );
}

#[cfg(target_os = "macos")]
fn assert_ordered_stinger_fallbacks(luma: &[u8]) {
    const REQUIRED_FRAMES: usize = 4;

    let white = luma_run(luma, 0, 245, u8::MAX, REQUIRED_FRAMES)
        .unwrap_or_else(|| panic!("recording has no initial white interval: {luma:?}"));
    let black = luma_run(luma, white + REQUIRED_FRAMES, 0, 32, REQUIRED_FRAMES)
        .unwrap_or_else(|| panic!("Fade fallback did not settle on black: {luma:?}"));
    assert!(
        luma[white + REQUIRED_FRAMES..black]
            .iter()
            .any(|value| (48..=208).contains(value)),
        "Fade fallback contains no intermediate frame: {luma:?}"
    );
    luma_run(luma, black + REQUIRED_FRAMES, 245, u8::MAX, REQUIRED_FRAMES)
        .unwrap_or_else(|| panic!("Cut fallback did not settle on white: {luma:?}"));
}

#[cfg(target_os = "macos")]
fn grid_luma_run(
    frames: &[[u8; 9]],
    start: usize,
    required: usize,
    predicate: impl Fn(&[u8; 9]) -> bool,
) -> Option<usize> {
    frames[start..]
        .windows(required)
        .position(|window| window.iter().all(&predicate))
        .map(|offset| start + offset)
}

#[cfg(target_os = "macos")]
fn assert_ordered_white_slide_black(frames: &[[u8; 2]]) {
    const REQUIRED_FRAMES: usize = 3;

    let white = half_luma_run(frames, 0, REQUIRED_FRAMES, |[left, right]| {
        left >= 205 && right >= 205
    })
    .unwrap_or_else(|| panic!("recording has no stable white Program interval: {frames:?}"));
    let split = half_luma_run(frames, white + REQUIRED_FRAMES, 1, |[left, right]| {
        left >= 180 && right <= 64
    })
    .unwrap_or_else(|| {
        panic!("recording has no Slide frame with white left and black right: {frames:?}")
    });
    half_luma_run(frames, split + 1, REQUIRED_FRAMES, |[left, right]| {
        left <= 32 && right <= 32
    })
    .unwrap_or_else(|| panic!("recording has no stable black Program interval: {frames:?}"));
}

#[cfg(target_os = "macos")]
fn half_luma_run(
    frames: &[[u8; 2]],
    start: usize,
    required: usize,
    predicate: impl Fn([u8; 2]) -> bool,
) -> Option<usize> {
    frames[start..]
        .windows(required)
        .position(|window| window.iter().copied().all(&predicate))
        .map(|offset| start + offset)
}

#[cfg(target_os = "macos")]
fn assert_ordered_live_black_live(luma: &[u8]) -> (usize, usize) {
    const BLACK_MAX: u8 = 24;
    const LIVE_MIN: u8 = 64;
    const REQUIRED_FRAMES: usize = 3;

    let first_black = luma
        .iter()
        .position(|value| *value <= BLACK_MAX)
        .unwrap_or_else(|| panic!("recording contains no black frame: {luma:?}"));
    let last_black = luma
        .iter()
        .rposition(|value| *value <= BLACK_MAX)
        .expect("first black frame was present");
    assert!(
        luma[..first_black]
            .iter()
            .filter(|value| **value >= LIVE_MIN)
            .count()
            >= REQUIRED_FRAMES,
        "recording has no stable live interval before black: {luma:?}"
    );
    assert!(
        luma[first_black..=last_black]
            .iter()
            .filter(|value| **value <= BLACK_MAX)
            .count()
            >= REQUIRED_FRAMES,
        "recording has no stable black interval: {luma:?}"
    );
    assert!(
        luma[last_black + 1..]
            .iter()
            .filter(|value| **value >= LIVE_MIN)
            .count()
            >= REQUIRED_FRAMES,
        "recording has no stable live interval after black: {luma:?}"
    );
    (first_black, last_black)
}

#[cfg(target_os = "macos")]
fn assert_ordered_opaque_alpha_fade_transparent(luma: &[u8]) {
    const REQUIRED_FRAMES: usize = 3;

    let opaque = luma_run(luma, 0, 205, u8::MAX, REQUIRED_FRAMES)
        .unwrap_or_else(|| panic!("recording has no stable opaque-white interval: {luma:?}"));
    let intermediate = luma_run(luma, opaque + REQUIRED_FRAMES, 64, 190, 1)
        .unwrap_or_else(|| panic!("recording has no AlphaFade intermediate frame: {luma:?}"));
    luma_run(luma, intermediate + 1, 0, 32, REQUIRED_FRAMES)
        .unwrap_or_else(|| panic!("recording has no stable transparent-black interval: {luma:?}"));
}

#[cfg(target_os = "macos")]
fn record_manual_transition_reversal(
    project_path: &Path,
    output_path: &Path,
    kind: ManualTransitionKind,
    label: &str,
) -> Option<Vec<u8>> {
    let mut daemon = require_native_recorder(NativeDaemonProcess::start_recording(
        project_path,
        output_path,
    ))?;
    let mut client = StudioClient::connect(daemon.address);
    let initial = client.handshake();
    assert_snapshot_routing(&initial, 0, input(1), input(2));
    thread::sleep(Duration::from_millis(300));

    let steps = [
        (
            "manual-start-reverse",
            CommandPayload::StartManualTransition { kind },
            100,
        ),
        (
            "manual-forward",
            CommandPayload::SetManualTransitionPosition {
                position: ManualTransitionPosition::new(7_500).unwrap(),
            },
            250,
        ),
        (
            "manual-reverse",
            CommandPayload::SetManualTransitionPosition {
                position: ManualTransitionPosition::new(2_500).unwrap(),
            },
            250,
        ),
        ("manual-cancel", CommandPayload::CancelManualTransition, 250),
        (
            "manual-start-commit",
            CommandPayload::StartManualTransition { kind },
            100,
        ),
        (
            "manual-end",
            CommandPayload::SetManualTransitionPosition {
                position: ManualTransitionPosition::END,
            },
            250,
        ),
        ("manual-commit", CommandPayload::CommitManualTransition, 250),
    ];
    for (index, (id, payload, hold_ms)) in steps.into_iter().enumerate() {
        let outcome = client.command(id, &format!("{id}-key"), payload);
        assert_eq!(outcome.revision, u64::try_from(index + 1).unwrap());
        thread::sleep(Duration::from_millis(hold_ms));
    }
    daemon.signal_terminate();

    let output = daemon.wait_for(RECORDING_PROCESS_TIMEOUT);
    drop(client);
    assert!(
        output.status.success(),
        "{label} recording daemon failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let luma = recording_average_luma(output_path).unwrap();
    decode_recording(output_path).unwrap();
    Some(luma)
}

#[cfg(target_os = "macos")]
fn assert_ordered_manual_forward_reverse_commit(luma: &[u8]) {
    const REQUIRED_FRAMES: usize = 3;

    let initial = luma_run(luma, 0, 205, u8::MAX, REQUIRED_FRAMES)
        .unwrap_or_else(|| panic!("recording has no stable initial Program interval: {luma:?}"));
    let forward = luma_run(luma, initial + REQUIRED_FRAMES, 120, 150, REQUIRED_FRAMES)
        .unwrap_or_else(|| panic!("recording has no stable forward T-bar interval: {luma:?}"));
    let reverse = luma_run(luma, forward + REQUIRED_FRAMES, 175, 200, REQUIRED_FRAMES)
        .unwrap_or_else(|| panic!("recording has no stable reversed T-bar interval: {luma:?}"));
    let cancelled = luma_run(
        luma,
        reverse + REQUIRED_FRAMES,
        205,
        u8::MAX,
        REQUIRED_FRAMES,
    )
    .unwrap_or_else(|| panic!("recording has no stable cancelled T-bar interval: {luma:?}"));
    luma_run(luma, cancelled + REQUIRED_FRAMES, 0, 100, REQUIRED_FRAMES)
        .unwrap_or_else(|| panic!("recording has no stable committed Program interval: {luma:?}"));
}

#[cfg(target_os = "macos")]
fn assert_ordered_manual_alpha_fade_forward_reverse_commit(luma: &[u8]) {
    const REQUIRED_FRAMES: usize = 3;

    let initial = luma_run(luma, 0, 205, u8::MAX, REQUIRED_FRAMES)
        .unwrap_or_else(|| panic!("recording has no stable initial opaque interval: {luma:?}"));
    let forward = luma_run(luma, initial + REQUIRED_FRAMES, 110, 160, REQUIRED_FRAMES)
        .unwrap_or_else(|| panic!("recording has no stable forward AlphaFade interval: {luma:?}"));
    let reverse = luma_run(luma, forward + REQUIRED_FRAMES, 190, 225, REQUIRED_FRAMES)
        .unwrap_or_else(|| panic!("recording has no stable reversed AlphaFade interval: {luma:?}"));
    let cancelled = luma_run(
        luma,
        reverse + REQUIRED_FRAMES,
        230,
        u8::MAX,
        REQUIRED_FRAMES,
    )
    .unwrap_or_else(|| panic!("recording has no stable cancelled AlphaFade interval: {luma:?}"));
    luma_run(luma, cancelled + REQUIRED_FRAMES, 0, 32, REQUIRED_FRAMES).unwrap_or_else(|| {
        panic!("recording has no stable committed transparent interval: {luma:?}")
    });
}

#[cfg(target_os = "macos")]
fn luma_run(luma: &[u8], start: usize, minimum: u8, maximum: u8, length: usize) -> Option<usize> {
    luma.get(start..)?
        .windows(length)
        .position(|window| {
            window
                .iter()
                .all(|value| (minimum..=maximum).contains(value))
        })
        .map(|offset| start + offset)
}

#[cfg(target_os = "macos")]
fn recording_audio_silence_intervals(path: &Path) -> Result<Vec<(f64, f64)>, String> {
    let child = Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "info", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:a:0",
            "-af",
            "silencedetect=noise=-45dB:duration=0.15",
            "-f",
            "null",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot spawn FFmpeg silence detector: {error}"))?;
    let output = wait_bounded(child, PROCESS_TIMEOUT);
    if !output.status.success() {
        return Err(format!(
            "recording silence detection failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    let mut start = None;
    let mut intervals = Vec::new();
    for line in diagnostic.lines() {
        if let Some(value) = diagnostic_seconds(line, "silence_start:") {
            start = Some(value);
        }
        if let Some(end) = diagnostic_seconds(line, "silence_end:")
            && let Some(start) = start.take()
        {
            intervals.push((start, end));
        }
    }
    Ok(intervals)
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug)]
struct AudioSignature {
    base_program: f64,
    base_preview: f64,
    clip_lead: f64,
    clip_tail: f64,
}

#[cfg(target_os = "macos")]
fn recording_audio_signatures(path: &Path) -> Result<Vec<AudioSignature>, String> {
    const MASTER_SAMPLE_RATE: usize = 48_000;
    const MASTER_SAMPLE_RATE_HZ: f64 = 48_000.0;
    const PROJECT_FRAME_RATE: usize = 25;
    const SAMPLES_PER_FRAME: usize = MASTER_SAMPLE_RATE / PROJECT_FRAME_RATE;
    // Classify the stable body of each lossy AAC frame; exact sample-boundary
    // behavior is covered separately by the lossless PCM Stinger oracle.
    const EDGE_SAMPLES: usize = 320;

    let pcm = path.with_extension("audio.f32le");
    let child = Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:a:0",
            "-ac",
            "1",
            "-ar",
            "48000",
            "-c:a",
            "pcm_f32le",
            "-f",
            "f32le",
        ])
        .arg(&pcm)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("LC_ALL", "C")
        .spawn()
        .map_err(|error| format!("cannot spawn recording PCM decoder: {error}"))?;
    let output = wait_bounded(child, PROCESS_TIMEOUT);
    if !output.status.success() {
        return Err(format!(
            "recording PCM decode failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let bytes = fs::read(&pcm).map_err(|error| format!("cannot read decoded PCM: {error}"))?;
    let mut chunks = bytes.chunks_exact(std::mem::size_of::<f32>());
    let samples = chunks
        .by_ref()
        .map(|bytes| f32::from_le_bytes(bytes.try_into().expect("f32 chunk is exact")))
        .collect::<Vec<_>>();
    if !chunks.remainder().is_empty() {
        return Err("decoded PCM has a partial f32 sample".to_owned());
    }
    Ok(samples
        .chunks_exact(SAMPLES_PER_FRAME)
        .map(|frame| {
            let frame = &frame[EDGE_SAMPLES..SAMPLES_PER_FRAME - EDGE_SAMPLES];
            AudioSignature {
                base_program: tone_amplitude(frame, 440.0, MASTER_SAMPLE_RATE_HZ),
                base_preview: tone_amplitude(frame, 660.0, MASTER_SAMPLE_RATE_HZ),
                clip_lead: tone_amplitude(frame, 997.0, MASTER_SAMPLE_RATE_HZ),
                clip_tail: tone_amplitude(frame, 1499.0, MASTER_SAMPLE_RATE_HZ),
            }
        })
        .collect())
}

#[cfg(target_os = "macos")]
fn tone_amplitude(samples: &[f32], frequency_hz: f64, sample_rate: f64) -> f64 {
    let angular_step = 2.0 * std::f64::consts::PI * frequency_hz / sample_rate;
    let (sin, cos, _) =
        samples
            .iter()
            .fold((0.0_f64, 0.0_f64, 0.0_f64), |(sin, cos, phase), sample| {
                (
                    sin + f64::from(*sample) * phase.sin(),
                    cos + f64::from(*sample) * phase.cos(),
                    phase + angular_step,
                )
            });
    let sample_count =
        f64::from(u32::try_from(samples.len()).expect("recording frame sample count is bounded"));
    2.0 * sin.hypot(cos) / sample_count
}

#[cfg(target_os = "macos")]
fn assert_scheduled_stinger_audio_policies(
    signatures: &[AudioSignature],
    recording_origin: u64,
    scheduled_frames: &[u64],
) {
    let [muted, only, mixed, first_replay, second_replay] =
        validated_stinger_audio_schedule(scheduled_frames);
    let first_verified = recording_origin + RECORDING_STARTUP_SETTLING_FRAMES;
    assert!(
        muted >= first_verified,
        "first Stinger frame {muted} precedes recording settling boundary {first_verified}"
    );
    assert_muted_stinger_audio(signatures, recording_origin, first_verified, muted, only);
    assert_stinger_only_audio(signatures, recording_origin, only, mixed);
    assert_mixed_stinger_audio(signatures, recording_origin, mixed, first_replay);
    assert_first_stinger_replay_audio(signatures, recording_origin, first_replay, second_replay);
    assert_second_stinger_replay_audio(signatures, recording_origin, second_replay);
}

#[cfg(target_os = "macos")]
fn assert_muted_stinger_audio(
    signatures: &[AudioSignature],
    recording_origin: u64,
    first_verified: u64,
    muted: u64,
    only: u64,
) {
    let ranges: &[(u64, u64, AudioSignaturePredicate, &str)] = &[
        (
            first_verified,
            muted + STINGER_RECORDING_CUT_FRAME,
            pure_program,
            "initial and Muted Program",
        ),
        (
            muted + STINGER_RECORDING_CUT_FRAME,
            muted + STINGER_RECORDING_CUT_FRAME + 1,
            boundary_program_preview,
            "Muted Program-to-Preview AAC boundary",
        ),
        (
            muted + STINGER_RECORDING_CUT_FRAME + 1,
            only,
            pure_preview,
            "Muted Preview and settled Preview",
        ),
    ];
    assert_audio_signature_ranges(signatures, recording_origin, ranges);
}

#[cfg(target_os = "macos")]
fn assert_stinger_only_audio(
    signatures: &[AudioSignature],
    recording_origin: u64,
    only: u64,
    mixed: u64,
) {
    let ranges: &[(u64, u64, AudioSignaturePredicate, &str)] = &[
        (
            only,
            only + 1,
            boundary_preview_clip_lead,
            "StingerOnly onset AAC boundary",
        ),
        (
            only + 1,
            only + STINGER_RECORDING_CUT_FRAME,
            pure_clip_lead,
            "StingerOnly lead",
        ),
        (
            only + STINGER_RECORDING_CUT_FRAME,
            only + STINGER_RECORDING_CUT_FRAME + 1,
            boundary_clip_lead_tail,
            "StingerOnly clip-cut AAC boundary",
        ),
        (
            only + STINGER_RECORDING_CUT_FRAME + 1,
            only + STINGER_RECORDING_DURATION_FRAMES,
            pure_clip_tail,
            "StingerOnly tail",
        ),
        (
            only + STINGER_RECORDING_DURATION_FRAMES,
            only + STINGER_RECORDING_DURATION_FRAMES + 1,
            boundary_clip_tail_program,
            "StingerOnly exit AAC boundary",
        ),
        (
            only + STINGER_RECORDING_DURATION_FRAMES + 1,
            mixed,
            pure_program,
            "post-StingerOnly Program",
        ),
    ];
    assert_audio_signature_ranges(signatures, recording_origin, ranges);
}

#[cfg(target_os = "macos")]
fn assert_mixed_stinger_audio(
    signatures: &[AudioSignature],
    recording_origin: u64,
    mixed: u64,
    first_replay: u64,
) {
    let ranges: &[(u64, u64, AudioSignaturePredicate, &str)] = &[
        (
            mixed,
            mixed + 1,
            boundary_program_mixed_lead,
            "MixWithProgram onset AAC boundary",
        ),
        (
            mixed + 1,
            mixed + STINGER_RECORDING_CUT_FRAME,
            mixed_program_lead,
            "MixWithProgram Program lead",
        ),
        (
            mixed + STINGER_RECORDING_CUT_FRAME,
            mixed + STINGER_RECORDING_CUT_FRAME + 1,
            boundary_mixed_cut,
            "MixWithProgram clip/base cut AAC boundary",
        ),
        (
            mixed + STINGER_RECORDING_CUT_FRAME + 1,
            mixed + STINGER_RECORDING_DURATION_FRAMES,
            mixed_preview_tail,
            "MixWithProgram Preview tail",
        ),
        (
            mixed + STINGER_RECORDING_DURATION_FRAMES,
            mixed + STINGER_RECORDING_DURATION_FRAMES + 1,
            boundary_mixed_tail_preview,
            "MixWithProgram exit AAC boundary",
        ),
        (
            mixed + STINGER_RECORDING_DURATION_FRAMES + 1,
            first_replay,
            pure_preview,
            "post-MixWithProgram Preview",
        ),
    ];
    assert_audio_signature_ranges(signatures, recording_origin, ranges);
}

#[cfg(target_os = "macos")]
fn assert_first_stinger_replay_audio(
    signatures: &[AudioSignature],
    recording_origin: u64,
    first_replay: u64,
    second_replay: u64,
) {
    let ranges: &[(u64, u64, AudioSignaturePredicate, &str)] = &[
        (
            first_replay,
            first_replay + 1,
            boundary_preview_clip_lead,
            "first replay onset AAC boundary",
        ),
        (
            first_replay + 1,
            first_replay + STINGER_RECORDING_CUT_FRAME,
            pure_clip_lead,
            "first replay lead",
        ),
        (
            first_replay + STINGER_RECORDING_CUT_FRAME,
            first_replay + STINGER_RECORDING_CUT_FRAME + 1,
            boundary_clip_lead_tail,
            "first replay clip-cut AAC boundary",
        ),
        (
            first_replay + STINGER_RECORDING_CUT_FRAME + 1,
            first_replay + STINGER_RECORDING_DURATION_FRAMES,
            pure_clip_tail,
            "first replay tail",
        ),
        (
            first_replay + STINGER_RECORDING_DURATION_FRAMES,
            first_replay + STINGER_RECORDING_DURATION_FRAMES + 1,
            boundary_clip_tail_program,
            "first replay exit AAC boundary",
        ),
        (
            first_replay + STINGER_RECORDING_DURATION_FRAMES + 1,
            second_replay,
            pure_program,
            "between replays Program",
        ),
    ];
    assert_audio_signature_ranges(signatures, recording_origin, ranges);
}

#[cfg(target_os = "macos")]
fn assert_second_stinger_replay_audio(
    signatures: &[AudioSignature],
    recording_origin: u64,
    second_replay: u64,
) {
    let ranges: &[(u64, u64, AudioSignaturePredicate, &str)] = &[
        (
            second_replay,
            second_replay + 1,
            boundary_program_clip_lead,
            "second replay onset AAC boundary",
        ),
        (
            second_replay + 1,
            second_replay + STINGER_RECORDING_CUT_FRAME,
            pure_clip_lead,
            "second replay lead",
        ),
        (
            second_replay + STINGER_RECORDING_CUT_FRAME,
            second_replay + STINGER_RECORDING_CUT_FRAME + 1,
            boundary_clip_lead_tail,
            "second replay clip-cut AAC boundary",
        ),
        (
            second_replay + STINGER_RECORDING_CUT_FRAME + 1,
            second_replay + STINGER_RECORDING_DURATION_FRAMES,
            pure_clip_tail,
            "second replay tail",
        ),
        (
            second_replay + STINGER_RECORDING_DURATION_FRAMES,
            second_replay + STINGER_RECORDING_DURATION_FRAMES + 1,
            boundary_clip_tail_preview,
            "second replay exit AAC boundary",
        ),
        (
            second_replay + STINGER_RECORDING_DURATION_FRAMES + 1,
            second_replay + STINGER_RECORDING_DURATION_FRAMES + RECORDING_POST_ROLL_FRAMES,
            pure_preview,
            "final settled Preview",
        ),
    ];
    assert_audio_signature_ranges(signatures, recording_origin, ranges);
}

#[cfg(target_os = "macos")]
fn validated_stinger_audio_schedule(scheduled_frames: &[u64]) -> [u64; 5] {
    assert_eq!(scheduled_frames.len(), 5);
    for pair in scheduled_frames.windows(2) {
        assert!(
            pair[1] > pair[0] + STINGER_RECORDING_DURATION_FRAMES + 1,
            "Stinger commands overlap or leave no pure base-audio frame: {scheduled_frames:?}"
        );
    }
    <[u64; 5]>::try_from(scheduled_frames).unwrap()
}

#[cfg(target_os = "macos")]
fn assert_scheduled_clip_only_replay(
    signatures: &[AudioSignature],
    recording_origin: u64,
    scheduled_frame: u64,
) {
    let first_verified = recording_origin + RECORDING_STARTUP_SETTLING_FRAMES;
    assert!(
        scheduled_frame >= first_verified,
        "restored Stinger frame {scheduled_frame} precedes recording settling boundary \
         {first_verified}"
    );
    let ranges: &[(u64, u64, AudioSignaturePredicate, &str)] = &[
        (
            first_verified,
            scheduled_frame,
            pure_preview,
            "restored Program",
        ),
        (
            scheduled_frame,
            scheduled_frame + 1,
            boundary_preview_clip_lead,
            "restored replay onset AAC boundary",
        ),
        (
            scheduled_frame + 1,
            scheduled_frame + STINGER_RECORDING_CUT_FRAME,
            pure_clip_lead,
            "restored replay lead",
        ),
        (
            scheduled_frame + STINGER_RECORDING_CUT_FRAME,
            scheduled_frame + STINGER_RECORDING_CUT_FRAME + 1,
            boundary_clip_lead_tail,
            "restored replay clip-cut AAC boundary",
        ),
        (
            scheduled_frame + STINGER_RECORDING_CUT_FRAME + 1,
            scheduled_frame + STINGER_RECORDING_DURATION_FRAMES,
            pure_clip_tail,
            "restored replay tail",
        ),
        (
            scheduled_frame + STINGER_RECORDING_DURATION_FRAMES,
            scheduled_frame + STINGER_RECORDING_DURATION_FRAMES + 1,
            boundary_clip_tail_program,
            "restored replay exit AAC boundary",
        ),
        (
            scheduled_frame + STINGER_RECORDING_DURATION_FRAMES + 1,
            scheduled_frame + STINGER_RECORDING_DURATION_FRAMES + RECORDING_POST_ROLL_FRAMES,
            pure_program,
            "restored settled Program",
        ),
    ];
    assert_audio_signature_ranges(signatures, recording_origin, ranges);
}

#[cfg(target_os = "macos")]
// FFmpeg applies the AAC stream's skip-samples/edit-list metadata while
// decoding, so recording frame zero remains authoritative.
const RECORDING_STARTUP_SETTLING_FRAMES: u64 = 0;

#[cfg(target_os = "macos")]
const RECORDING_POST_ROLL_FRAMES: u64 = 3;

#[cfg(target_os = "macos")]
const STINGER_RECORDING_DURATION_FRAMES: u64 = 12;

#[cfg(target_os = "macos")]
const STINGER_RECORDING_CUT_FRAME: u64 = 6;

#[cfg(target_os = "macos")]
type AudioSignaturePredicate = fn(AudioSignature) -> bool;

#[cfg(target_os = "macos")]
fn assert_audio_signature_ranges(
    signatures: &[AudioSignature],
    recording_origin: u64,
    ranges: &[(u64, u64, AudioSignaturePredicate, &str)],
) {
    for &(start, end, predicate, label) in ranges {
        assert_audio_signature_range(signatures, recording_origin, start, end, predicate, label);
    }
}

#[cfg(target_os = "macos")]
fn assert_audio_signature_range(
    signatures: &[AudioSignature],
    recording_origin: u64,
    start: u64,
    end: u64,
    predicate: fn(AudioSignature) -> bool,
    label: &str,
) {
    assert!(start < end, "empty {label} frame range {start}..{end}");
    let recording_start =
        usize::try_from(start.checked_sub(recording_origin).unwrap_or_else(|| {
            panic!("{label} starts before recording origin {recording_origin}")
        }))
        .unwrap();
    let recording_end = usize::try_from(
        end.checked_sub(recording_origin)
            .unwrap_or_else(|| panic!("{label} ends before recording origin {recording_origin}")),
    )
    .unwrap();
    let actual = signatures
        .get(recording_start..recording_end)
        .unwrap_or_else(|| {
            panic!(
                "recording has no complete {label} range {start}..{end} \
             (origin {recording_origin}, decoded frames {})",
                signatures.len()
            )
        });
    for (offset, signature) in actual.iter().copied().enumerate() {
        assert!(
            predicate(signature),
            "{label} mismatch at authoritative frame {} (recording frame {}): {signature:?}; \
             nearby signatures: {:?}",
            start + u64::try_from(offset).unwrap(),
            recording_start + offset,
            &signatures[recording_start.saturating_add(offset).saturating_sub(2)
                ..signatures
                    .len()
                    .min(recording_start.saturating_add(offset).saturating_add(3))],
        );
    }
}

#[cfg(target_os = "macos")]
fn pure_program(signature: AudioSignature) -> bool {
    signature.base_program > 0.035
        && signature.base_preview < 0.02
        && signature.clip_lead < 0.02
        && signature.clip_tail < 0.02
}

#[cfg(target_os = "macos")]
fn pure_preview(signature: AudioSignature) -> bool {
    signature.base_program < 0.02
        && signature.base_preview > 0.035
        && signature.clip_lead < 0.02
        && signature.clip_tail < 0.02
}

#[cfg(target_os = "macos")]
fn pure_clip_lead(signature: AudioSignature) -> bool {
    signature.base_program < 0.02
        && signature.base_preview < 0.02
        && signature.clip_lead > 0.05
        && signature.clip_tail < 0.02
}

#[cfg(target_os = "macos")]
fn pure_clip_tail(signature: AudioSignature) -> bool {
    signature.base_program < 0.02
        && signature.base_preview < 0.02
        && signature.clip_lead < 0.02
        && signature.clip_tail > 0.05
}

#[cfg(target_os = "macos")]
fn mixed_program_lead(signature: AudioSignature) -> bool {
    signature.base_program > 0.035
        && signature.base_preview < 0.02
        && signature.clip_lead > 0.05
        && signature.clip_tail < 0.02
}

#[cfg(target_os = "macos")]
fn mixed_preview_tail(signature: AudioSignature) -> bool {
    signature.base_program < 0.02
        && signature.base_preview > 0.035
        && signature.clip_lead < 0.02
        && signature.clip_tail > 0.05
}

#[cfg(target_os = "macos")]
fn boundary_program_preview(signature: AudioSignature) -> bool {
    codec_boundary(signature, [true, true, false, false])
}

#[cfg(target_os = "macos")]
fn boundary_preview_clip_lead(signature: AudioSignature) -> bool {
    codec_boundary(signature, [false, true, true, false])
}

#[cfg(target_os = "macos")]
fn boundary_clip_lead_tail(signature: AudioSignature) -> bool {
    codec_boundary(signature, [false, false, true, true])
}

#[cfg(target_os = "macos")]
fn boundary_clip_tail_program(signature: AudioSignature) -> bool {
    codec_boundary(signature, [true, false, false, true])
}

#[cfg(target_os = "macos")]
fn boundary_program_mixed_lead(signature: AudioSignature) -> bool {
    codec_boundary(signature, [true, false, true, false])
}

#[cfg(target_os = "macos")]
fn boundary_mixed_cut(signature: AudioSignature) -> bool {
    codec_boundary(signature, [true, true, true, true])
}

#[cfg(target_os = "macos")]
fn boundary_mixed_tail_preview(signature: AudioSignature) -> bool {
    codec_boundary(signature, [false, true, false, true])
}

#[cfg(target_os = "macos")]
fn boundary_program_clip_lead(signature: AudioSignature) -> bool {
    codec_boundary(signature, [true, false, true, false])
}

#[cfg(target_os = "macos")]
fn boundary_clip_tail_preview(signature: AudioSignature) -> bool {
    codec_boundary(signature, [false, true, false, true])
}

#[cfg(target_os = "macos")]
fn codec_boundary(signature: AudioSignature, expected: [bool; 4]) -> bool {
    [
        signature.base_program,
        signature.base_preview,
        signature.clip_lead,
        signature.clip_tail,
    ]
    .into_iter()
    .zip(expected)
    .all(|(amplitude, present)| {
        if present {
            amplitude > 0.02
        } else {
            amplitude < 0.02
        }
    })
}

#[cfg(target_os = "macos")]
fn diagnostic_seconds(line: &str, label: &str) -> Option<f64> {
    line.split_once(label)?
        .1
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[cfg(target_os = "macos")]
fn prepare_stinger_project(project_path: &Path) -> Result<(), String> {
    save_white_black_generator_project(project_path);
    let assets = project_path.join("assets");
    fs::create_dir_all(&assets)
        .map_err(|error| format!("cannot create Stinger asset directory: {error}"))?;
    generate_alpha_stinger(&assets.join("stinger.mkv"))?;

    let store = ProjectStore::new(project_path).map_err(|error| error.to_string())?;
    let stored = store.load().map_err(|error| error.to_string())?;
    let mut project = stored.project().clone();
    project.add_input(Input {
        id: input(3),
        name: "Stinger media".into(),
        kind: InputKind::Media {
            asset_uri: "asset://stinger.mkv".into(),
        },
        required_capabilities: Vec::new(),
    });
    project.add_stinger(StingerConfig::new(
        StingerSlotNumber::new(1).unwrap(),
        input(3),
        true,
        6,
        StingerAudioPolicy::Muted,
        StingerMissingMediaFallback::Cut,
    ));
    let configured = StoredProject::from_project(
        project,
        stored.runtime_routing(),
        stored.position(),
        stored.idempotency_receipts().to_vec(),
    )
    .map_err(|error| error.to_string())?;
    store.save(&configured).map_err(|error| error.to_string())
}

#[cfg(target_os = "macos")]
fn prepare_audible_stinger_project(project_path: &Path) -> Result<(), String> {
    let assets = project_path.join("assets");
    fs::create_dir_all(&assets)
        .map_err(|error| format!("cannot create audible Stinger asset directory: {error}"))?;
    generate_tone_video(&assets.join("program.mov"), "white", 440)?;
    generate_tone_video(&assets.join("preview.mov"), "black", 660)?;
    generate_audible_alpha_stinger(&assets.join("stinger.mov"))?;
    save_media_project(project_path, "asset://program.mov", "asset://preview.mov");

    let store = ProjectStore::new(project_path).map_err(|error| error.to_string())?;
    let stored = store.load().map_err(|error| error.to_string())?;
    let mut project = stored.project().clone();
    project.add_input(Input {
        id: input(3),
        name: "Audible Stinger media".into(),
        kind: InputKind::Media {
            asset_uri: "asset://stinger.mov".into(),
        },
        required_capabilities: Vec::new(),
    });
    for (slot, audio_policy) in [
        (1, StingerAudioPolicy::Muted),
        (2, StingerAudioPolicy::StingerOnly),
        (3, StingerAudioPolicy::MixWithProgram),
    ] {
        project.add_stinger(StingerConfig::new(
            StingerSlotNumber::new(slot).unwrap(),
            input(3),
            true,
            6,
            audio_policy,
            StingerMissingMediaFallback::Cut,
        ));
    }
    let configured = StoredProject::from_project(
        project,
        stored.runtime_routing(),
        stored.position(),
        stored.idempotency_receipts().to_vec(),
    )
    .map_err(|error| error.to_string())?;
    store.save(&configured).map_err(|error| error.to_string())
}

#[cfg(target_os = "macos")]
fn generate_tone_video(path: &Path, color: &str, frequency_hz: u32) -> Result<(), String> {
    let video = format!("color=c={color}:size=64x48:rate=25:duration=8");
    let audio = format!("sine=frequency={frequency_hz}:sample_rate=48000:duration=8");
    let child = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &video,
            "-f",
            "lavfi",
            "-i",
            &audio,
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-frames:v",
            "200",
            "-vf",
            "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=tv",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-x264-params",
            "colorprim=bt709:transfer=bt709:colormatrix=bt709:fullrange=off",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
            "-colorspace",
            "bt709",
            "-color_range",
            "tv",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "pcm_s16le",
            "-ac",
            "2",
            "-f",
            "mov",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("LC_ALL", "C")
        .spawn()
        .map_err(|error| format!("cannot spawn audible base generator: {error}"))?;
    let output = wait_bounded(child, Duration::from_secs(15));
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "audible base generation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

#[cfg(target_os = "macos")]
fn generate_audible_alpha_stinger(path: &Path) -> Result<(), String> {
    let video = path.with_file_name("stinger-video.mkv");
    generate_alpha_stinger(&video)?;
    let tone = "aevalsrc=if(lt(t\\,0.24)\\,0.12*sin(2*PI*997*t)\\,0.12*sin(2*PI*1499*t))|if(lt(t\\,0.24)\\,0.12*sin(2*PI*997*t)\\,0.12*sin(2*PI*1499*t)):s=48000:d=0.48";
    let child = Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(&video)
        .args([
            "-f",
            "lavfi",
            "-i",
            tone,
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-vf",
            "format=yuva444p10le,setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=tv",
            "-c:v",
            "prores_ks",
            "-profile:v",
            "4",
            "-pix_fmt",
            "yuva444p10le",
            "-vendor",
            "apl0",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
            "-colorspace",
            "bt709",
            "-color_range",
            "tv",
            "-c:a",
            "pcm_s16le",
            "-shortest",
            "-f",
            "mov",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("LC_ALL", "C")
        .spawn()
        .map_err(|error| format!("cannot spawn audible Stinger generator: {error}"))?;
    let output = wait_bounded(child, Duration::from_secs(15));
    let _ = fs::remove_file(video.with_extension("rgba"));
    let _ = fs::remove_file(&video);
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "audible Stinger generation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

#[cfg(target_os = "macos")]
fn prepare_deferred_stinger_fallback_project(project_path: &Path) {
    save_white_black_generator_project(project_path);
    let store = ProjectStore::new(project_path).unwrap();
    let stored = store.load().unwrap();
    let mut project = stored.project().clone();
    for (slot, fallback) in [
        (1, StingerMissingMediaFallback::KeepProgram),
        (2, StingerMissingMediaFallback::Fade),
        (3, StingerMissingMediaFallback::Cut),
    ] {
        project.add_stinger(StingerConfig::new(
            StingerSlotNumber::new(slot).unwrap(),
            input(2),
            false,
            1,
            StingerAudioPolicy::Muted,
            fallback,
        ));
    }
    let configured = StoredProject::from_project(
        project,
        stored.runtime_routing(),
        stored.position(),
        stored.idempotency_receipts().to_vec(),
    )
    .unwrap();
    store.save(&configured).unwrap();
}

#[cfg(target_os = "macos")]
fn generate_alpha_stinger(path: &Path) -> Result<(), String> {
    let raw = path.with_extension("rgba");
    let pixels = 64 * 48;
    let mut frames = Vec::with_capacity(12 * pixels * 4);
    for frame in 0..12 {
        let alpha = u8::try_from(frame * 23).unwrap_or(u8::MAX);
        let color = if frame == 11 {
            [255, 255, 255, 255]
        } else {
            [0, 255, 0, alpha]
        };
        for _ in 0..pixels {
            frames.extend_from_slice(&color);
        }
    }
    fs::write(&raw, frames)
        .map_err(|error| format!("cannot write Stinger RGBA fixture: {error}"))?;
    let child = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "rawvideo",
            "-pixel_format",
            "rgba",
            "-video_size",
            "64x48",
            "-framerate",
            "25",
            "-i",
        ])
        .arg(&raw)
        .args([
            "-frames:v",
            "12",
            "-vf",
            "setsar=1/1,setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=full",
            "-c:v",
            "ffv1",
            "-pix_fmt",
            "bgra",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("LC_ALL", "C")
        .spawn()
        .map_err(|error| format!("cannot spawn alpha Stinger generator: {error}"))?;
    let output = wait_bounded(child, Duration::from_secs(15));
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "alpha Stinger generation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

#[cfg(target_os = "macos")]
fn prepare_native_project(project_path: &Path) -> bool {
    let assets = project_path.join("assets");
    fs::create_dir_all(&assets).unwrap();
    for (name, first, second) in [("one.mkv", "red", "yellow"), ("two.mkv", "blue", "green")] {
        if require_native(generate_h264(&assets.join(name), first, second)).is_none() {
            return false;
        }
    }
    save_media_project(project_path, "asset://one.mkv", "asset://two.mkv");
    true
}

#[cfg(target_os = "macos")]
fn generate_h264(path: &Path, first: &str, second: &str) -> Result<(), String> {
    let first_source = format!("color=c={first}:size=64x48:rate=25");
    let second_source = format!("color=c={second}:size=64x48:rate=25");
    let child = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &first_source,
            "-f",
            "lavfi",
            "-i",
            &second_source,
            "-f",
            "lavfi",
            "-i",
            "aevalsrc=0.10*sin(2*PI*440*t)|0.05*sin(2*PI*660*t):s=44100:d=6",
            "-filter_complex",
            "[0:v][1:v]blend=all_expr='if(lt(N,6),A,B)',setparams=range=limited:color_primaries=bt709:color_trc=bt709:colorspace=bt709[video]",
            "-map",
            "[video]",
            "-map",
            "2:a:0",
            "-frames:v",
            "150",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-x264-params",
            "colorprim=bt709:transfer=bt709:colormatrix=bt709:fullrange=off",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
            "-colorspace",
            "bt709",
            "-color_range",
            "tv",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "pcm_s16le",
            "-channel_layout",
            "stereo",
            "-f",
            "nut",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("LC_ALL", "C")
        .spawn()
        .map_err(|error| format!("cannot spawn FFmpeg fixture generator: {error}"))?;
    let output = wait_bounded(child, Duration::from_secs(15));
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "H264 fixture generation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

#[cfg(target_os = "macos")]
struct CommandOutcome {
    revision: u64,
    scheduled_frame: u64,
    elapsed: Duration,
    stingers: Option<Vec<StingerStatus>>,
}

#[cfg(target_os = "macos")]
struct StudioClient {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

#[cfg(target_os = "macos")]
impl StudioClient {
    fn connect(address: SocketAddr) -> Self {
        let stream = connect_bounded(address);
        stream.set_nodelay(true).unwrap();
        stream.set_read_timeout(Some(PROCESS_TIMEOUT)).unwrap();
        stream.set_write_timeout(Some(PROCESS_TIMEOUT)).unwrap();
        Self {
            writer: stream.try_clone().unwrap(),
            reader: BufReader::new(stream),
        }
    }

    fn handshake(&mut self) -> SnapshotMessage {
        self.handshake_with_digest().1
    }

    fn handshake_with_digest(&mut self) -> (String, SnapshotMessage) {
        self.send(&WireMessage::HandshakeRequest(HandshakeRequest {
            protocol: CURRENT_PROTOCOL_VERSION,
            build: "native-daemon-process-test".into(),
            client_type: ClientType::Studio,
            desired_role: Role::Operator,
            resume_cursor: None,
        }));
        let digest = self.receive_until("handshake response", |message| {
            if let WireMessage::HandshakeResponse(response) = message {
                Some(response.capabilities.digest.clone())
            } else {
                None
            }
        });
        let snapshot = self.receive_until("snapshot", |message| {
            if let WireMessage::Snapshot(snapshot) = message {
                Some(snapshot.clone())
            } else {
                None
            }
        });
        (digest, snapshot)
    }

    fn command(&mut self, id: &str, key: &str, payload: CommandPayload) -> CommandOutcome {
        self.try_command(id, key, payload)
            .unwrap_or_else(|error| panic!("{error}"))
    }

    fn try_command(
        &mut self,
        id: &str,
        key: &str,
        payload: CommandPayload,
    ) -> Result<CommandOutcome, String> {
        let message = WireMessage::Command(CommandMessage {
            protocol: CURRENT_PROTOCOL_VERSION,
            id: id.into(),
            idempotency_key: key.into(),
            expected_revision: None,
            deadline_ms: None,
            payload,
        });
        let started = Instant::now();
        self.send(&message);
        let deadline = started + PROCESS_TIMEOUT;
        let mut accepted = None;
        let mut durable_revisions = HashSet::new();
        let mut realized_revisions = HashSet::new();
        let mut stingers = None;
        for _ in 0..4_096 {
            let message =
                self.try_receive_before(deadline, "accepted result and runtime realization")?;
            match message {
                WireMessage::CommandResult(CommandResult::Accepted {
                    id: result_id,
                    revision,
                    scheduled_frame: Some(scheduled_frame),
                }) if result_id == id => {
                    accepted = Some((revision, scheduled_frame));
                }
                WireMessage::CommandResult(CommandResult::Rejected {
                    id: result_id,
                    code,
                    message,
                    ..
                }) if result_id == id => {
                    return Err(format!("command {id} was rejected ({code}): {message}"));
                }
                WireMessage::Event(event) => {
                    durable_revisions.insert(event.cursor.revision);
                    if let fm_protocol::EventPayload::StingerSlotsChanged {
                        stingers: projected,
                        ..
                    } = event.payload
                    {
                        stingers = Some(projected);
                    }
                }
                WireMessage::RuntimeEvent(event)
                    if matches!(
                        event.event,
                        RuntimeLifecycleEvent::Realized { ref domain, .. } if domain == "switcher"
                    ) =>
                {
                    realized_revisions.insert(event.revision);
                }
                _ => {}
            }
            if let Some((revision, scheduled_frame)) = accepted
                && durable_revisions.contains(&revision)
                && realized_revisions.contains(&revision)
            {
                return Ok(CommandOutcome {
                    revision,
                    scheduled_frame,
                    elapsed: started.elapsed(),
                    stingers,
                });
            }
        }
        Err(format!(
            "too many interleaved messages while waiting for command {id}"
        ))
    }

    fn send(&mut self, message: &WireMessage) {
        self.writer
            .write_all(encode_line(message).unwrap().as_bytes())
            .unwrap();
        self.writer.flush().unwrap();
    }

    fn receive_until<T>(
        &mut self,
        expected: &str,
        mut predicate: impl FnMut(&WireMessage) -> Option<T>,
    ) -> T {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        for _ in 0..4_096 {
            let message = self.receive_before(deadline, expected);
            if let Some(value) = predicate(&message) {
                return value;
            }
        }
        panic!("too many interleaved messages while waiting for {expected}");
    }

    fn receive_before(&mut self, deadline: Instant, expected: &str) -> WireMessage {
        self.try_receive_before(deadline, expected)
            .unwrap_or_else(|error| panic!("{error}"))
    }

    fn try_receive_before(
        &mut self,
        deadline: Instant,
        expected: &str,
    ) -> Result<WireMessage, String> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!("timed out waiting for {expected}"));
        }
        self.reader
            .get_mut()
            .set_read_timeout(Some(remaining))
            .map_err(|error| format!("cannot set timeout while waiting for {expected}: {error}"))?;
        let mut line = String::new();
        let read = self
            .reader
            .read_line(&mut line)
            .map_err(|error| format!("timed out waiting for {expected}: {error}"))?;
        if read == 0 {
            return Err(format!("daemon closed while waiting for {expected}"));
        }
        let message =
            decode_line(&line).map_err(|error| format!("invalid daemon message: {error}"))?;
        if let WireMessage::Error(error) = &message {
            return Err(format!(
                "daemon returned protocol error while waiting for {expected}: {}: {}",
                error.error.code, error.error.message
            ));
        }
        Ok(message)
    }
}

#[cfg(target_os = "macos")]
fn assert_snapshot_routing(
    snapshot: &SnapshotMessage,
    revision: u64,
    program: InputId,
    preview: InputId,
) {
    assert_eq!(snapshot.revision, revision);
    assert_eq!(snapshot.desired_program, WireInputId::from_domain(program));
    assert_eq!(snapshot.realized_program, WireInputId::from_domain(program));
    assert_eq!(snapshot.desired_preview, WireInputId::from_domain(preview));
    assert_eq!(snapshot.realized_preview, WireInputId::from_domain(preview));
}

#[cfg(target_os = "macos")]
fn connect_bounded(address: SocketAddr) -> TcpStream {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "cannot connect to ready daemon");
        match TcpStream::connect_timeout(&address, remaining.min(Duration::from_millis(100))) {
            Ok(stream) => return stream,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("cannot connect to ready daemon: {error}"),
        }
    }
}

#[cfg(target_os = "macos")]
fn process_exists(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
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

#[cfg(target_os = "macos")]
fn fake_camera_discovery() -> Vec<u8> {
    let mut bytes = b"FMCAMD2\0".to_vec();
    bytes.push(0);
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    for value in ["fake-camera", "Fake Camera"] {
        bytes.extend_from_slice(&u32::try_from(value.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(value.as_bytes());
    }
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    bytes.extend_from_slice(&64_u32.to_le_bytes());
    bytes.extend_from_slice(&48_u32.to_le_bytes());
    bytes.extend_from_slice(&30_000_u32.to_le_bytes());
    bytes.extend_from_slice(&1_001_u32.to_le_bytes());
    bytes
}

#[cfg(target_os = "macos")]
fn fake_camera_record(sequence: u64) -> Vec<u8> {
    const WIDTH: u32 = 64;
    const HEIGHT: u32 = 48;
    const STRIDE: u32 = WIDTH * 4;
    let payload_len = usize::try_from(STRIDE * HEIGHT).unwrap();
    let pixel = [
        u8::try_from(sequence.saturating_mul(11) % 256).unwrap(),
        u8::try_from(sequence.saturating_mul(17) % 256).unwrap(),
        u8::try_from(sequence.saturating_mul(23) % 256).unwrap(),
        255,
    ];
    let mut payload = Vec::with_capacity(payload_len);
    for _ in 0..WIDTH * HEIGHT {
        payload.extend_from_slice(&pixel);
    }
    let mut bytes = u32::try_from(58 + payload.len())
        .unwrap()
        .to_le_bytes()
        .to_vec();
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes.extend_from_slice(&(sequence / 4).to_le_bytes());
    bytes.extend_from_slice(&i64::try_from(sequence * 1_001).unwrap().to_le_bytes());
    bytes.extend_from_slice(&30_000_i32.to_le_bytes());
    bytes.extend_from_slice(&1_001_i64.to_le_bytes());
    bytes.extend_from_slice(&30_000_i32.to_le_bytes());
    bytes.extend_from_slice(&WIDTH.to_le_bytes());
    bytes.extend_from_slice(&HEIGHT.to_le_bytes());
    bytes.extend_from_slice(&STRIDE.to_le_bytes());
    bytes.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
    bytes.extend_from_slice(&[
        u8::try_from(1 + sequence % 3).unwrap(),
        u8::try_from(1 + (sequence / 3) % 2).unwrap(),
    ]);
    bytes.extend_from_slice(&payload);
    bytes
}

#[cfg(target_os = "macos")]
fn save_fake_camera_helper(directory: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let helper = directory.join("camera-helper.sh");
    let mut capture = b"FMCAMF3\0".to_vec();
    let mut delayed_records = String::new();
    for sequence in 0..12_u64 {
        let encoded = shell_octal(&fake_camera_record(sequence));
        if sequence == 0 {
            capture.extend_from_slice(&fake_camera_record(sequence));
        } else {
            use std::fmt::Write as _;
            writeln!(delayed_records, "    /bin/sleep 0.04; printf '{encoded}'").unwrap();
        }
    }
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in\n  discover) printf '{}' ;;\n  capture)\n    printf '%s\\n' \"$@\" > \"$0.capture\"\n    printf '%s' \"$$\" > \"$0.pid\"\n    printf '{}'\n    while [ ! -f \"$0.release\" ]; do /bin/sleep 0.01; done\n{}    exec /bin/sleep 30 ;;\n  request-permission) touch \"$0.permission\"; exit 91 ;;\n  *) exit 90 ;;\nesac\n",
        shell_octal(&fake_camera_discovery()),
        shell_octal(&capture),
        delayed_records,
    );
    fs::write(&helper, script).unwrap();
    let mut permissions = fs::metadata(&helper).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&helper, permissions).unwrap();
    helper
}

#[cfg(target_os = "macos")]
fn save_failing_camera_helper(directory: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let helper = directory.join("camera-helper.sh");
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in\n  discover) printf '{}' ;;\n  capture) printf '%s' \"$$\" > \"$0.pid\"; printf 'BADMAGIC'; exec /bin/sleep 30 ;;\n  *) exit 90 ;;\nesac\n",
        shell_octal(&fake_camera_discovery()),
    );
    fs::write(&helper, script).unwrap();
    let mut permissions = fs::metadata(&helper).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&helper, permissions).unwrap();
    helper
}

#[cfg(target_os = "macos")]
fn save_camera_project(path: &Path) {
    let rate = FrameRate::new(30_000, 1_001).unwrap();
    let source_id = deterministic_camera_id(CameraIdKind::Source, "fake-camera");
    let mut project = Project::new(
        ProjectId::new(NonZeroU128::new(7_003).unwrap()),
        "Native camera daemon test",
        ProjectSettings {
            frame_rate: rate,
            video: VideoFormat {
                dimensions: VideoDimensions::new(64, 48).unwrap(),
                frame_rate: rate,
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
        id: input(1),
        name: "Fake Camera".into(),
        kind: InputKind::Device {
            stable_key: format!("macos.avfoundation.camera.v1.{source_id}"),
        },
        required_capabilities: Vec::new(),
    });
    project.add_input(Input {
        id: input(2),
        name: "Bars".into(),
        kind: InputKind::Simulated(SimulatedInput::new(
            SimulatedVideo::Bars,
            SimulatedAudio::Silence,
        )),
        required_capabilities: Vec::new(),
    });
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
    .unwrap();
    ProjectStore::new(path).unwrap().save(&stored).unwrap();
}

fn save_media_project(path: &Path, first_uri: &str, second_uri: &str) {
    let rate = FrameRate::new(25, 1).unwrap();
    let mut project = Project::new(
        ProjectId::new(NonZeroU128::new(7_001).unwrap()),
        "Native daemon test",
        ProjectSettings {
            frame_rate: rate,
            video: VideoFormat {
                dimensions: VideoDimensions::new(64, 48).unwrap(),
                frame_rate: rate,
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
    for (id, uri) in [(input(1), first_uri), (input(2), second_uri)] {
        project.add_input(Input {
            id,
            name: format!("Media {id}"),
            kind: InputKind::Media {
                asset_uri: uri.into(),
            },
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
    .unwrap();
    ProjectStore::new(path).unwrap().save(&stored).unwrap();
}

#[cfg(target_os = "macos")]
fn save_generator_project(path: &Path) {
    save_generator_project_with_position(path, FrameRate::new(25, 1).unwrap(), 0);
}

#[cfg(target_os = "macos")]
fn save_alpha_fade_generator_project(path: &Path) {
    save_generator_project_with_sources(
        path,
        FrameRate::new(25, 1).unwrap(),
        0,
        [
            (
                input(1),
                SimulatedVideo::Solid(SolidColor::new(255, 255, 255, 255)),
            ),
            (input(2), SimulatedVideo::Solid(SolidColor::new(0, 0, 0, 0))),
        ],
    );
}

#[cfg(target_os = "macos")]
fn save_white_black_generator_project(path: &Path) {
    save_generator_project_with_sources(
        path,
        FrameRate::new(25, 1).unwrap(),
        0,
        [
            (
                input(1),
                SimulatedVideo::Solid(SolidColor::new(255, 255, 255, 255)),
            ),
            (
                input(2),
                SimulatedVideo::Solid(SolidColor::new(0, 0, 0, 255)),
            ),
        ],
    );
}

#[cfg(target_os = "macos")]
fn save_restored_generator_project(path: &Path, frames_rendered: u64) {
    save_generator_project_with_position(
        path,
        FrameRate::new(30_000, 1_001).unwrap(),
        frames_rendered,
    );
}

#[cfg(target_os = "macos")]
fn save_generator_project_with_position(path: &Path, rate: FrameRate, frames_rendered: u64) {
    save_generator_project_with_sources(
        path,
        rate,
        frames_rendered,
        [
            (input(1), SimulatedVideo::Bars),
            (
                input(2),
                SimulatedVideo::Solid(SolidColor::new(24, 80, 160, 255)),
            ),
        ],
    );
}

#[cfg(target_os = "macos")]
fn save_generator_project_with_sources(
    path: &Path,
    rate: FrameRate,
    frames_rendered: u64,
    sources: [(InputId, SimulatedVideo); 2],
) {
    save_generator_project_with_sources_and_dimensions(
        path,
        rate,
        frames_rendered,
        VideoDimensions::new(64, 48).unwrap(),
        sources,
    );
}

#[cfg(target_os = "macos")]
fn save_generator_project_with_sources_and_dimensions(
    path: &Path,
    rate: FrameRate,
    frames_rendered: u64,
    dimensions: VideoDimensions,
    sources: [(InputId, SimulatedVideo); 2],
) {
    let mut project = Project::new(
        ProjectId::new(NonZeroU128::new(7_002).unwrap()),
        "Native generator daemon test",
        ProjectSettings {
            frame_rate: rate,
            video: VideoFormat {
                dimensions,
                frame_rate: rate,
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
    for (id, video) in sources {
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
        ProjectPosition {
            frames_rendered,
            clock_time_nanos: if frames_rendered == 0 {
                0
            } else {
                (frames_rendered - 1) * u64::from(rate.denominator()) * 1_000_000_000
                    / u64::from(rate.numerator())
            },
            ..ProjectPosition::default()
        },
        Vec::new(),
    )
    .unwrap();
    ProjectStore::new(path).unwrap().save(&stored).unwrap();
}

#[cfg(target_os = "macos")]
fn save_scene_generator_project(path: &Path) {
    let rate = FrameRate::new(25, 1).unwrap();
    let mut project = Project::new(
        ProjectId::new(NonZeroU128::new(7_003).unwrap()),
        "Native scene daemon test",
        ProjectSettings {
            frame_rate: rate,
            video: VideoFormat {
                dimensions: VideoDimensions::new(64, 48).unwrap(),
                frame_rate: rate,
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
    project.add_input(Input {
        id: input(3),
        name: "Nested scene".into(),
        kind: InputKind::Scene {
            scene_id: scene_id(20),
            audio_source: Some(input(1)),
        },
        required_capabilities: Vec::new(),
    });
    project.add_input(Input {
        id: input(4),
        name: "Silent scene".into(),
        kind: InputKind::Scene {
            scene_id: scene_id(30),
            audio_source: None,
        },
        required_capabilities: Vec::new(),
    });
    project.add_scene(Scene {
        id: scene_id(10),
        name: "Leaf scene".into(),
        background: Rgba8::OPAQUE_BLACK,
        layers: vec![scene_layer(SourceRef::Input(input(1)), 0)],
    });
    project.add_scene(Scene {
        id: scene_id(20),
        name: "Nested scene".into(),
        background: Rgba8::OPAQUE_BLACK,
        layers: vec![scene_layer(SourceRef::Scene(scene_id(10)), 0)],
    });
    project.add_scene(Scene {
        id: scene_id(30),
        name: "Shared scene".into(),
        background: Rgba8::OPAQUE_BLACK,
        layers: vec![
            scene_layer(SourceRef::Scene(scene_id(10)), 0),
            Layer {
                mask: Some(RectMask::new(8, 4, 32, 24).inverted(true)),
                ..scene_layer(SourceRef::Input(input(2)), 1)
            },
        ],
    });
    project.set_main_mix(MainMix::new(input(3), input(4)));
    let stored = StoredProject::from_project(
        project,
        RuntimeRouting {
            desired_program_id: Some(input(3)),
            realized_program_id: Some(input(3)),
            desired_preview_id: Some(input(4)),
            realized_preview_id: Some(input(4)),
        },
        ProjectPosition::default(),
        Vec::new(),
    )
    .unwrap();
    ProjectStore::new(path).unwrap().save(&stored).unwrap();
}

#[cfg(target_os = "macos")]
fn scene_layer(source: SourceRef, z_order: i32) -> Layer {
    Layer {
        name: "layer".into(),
        source,
        enabled: true,
        geometry: LayerGeometry::new(0, 0, 64, 48, Rotation::Deg0),
        crop: None,
        mask: None,
        opacity: u8::MAX,
        z_order,
    }
}

#[cfg(target_os = "macos")]
fn scene_id(value: u128) -> SceneId {
    SceneId::new(NonZeroU128::new(value).unwrap())
}

fn input(value: u128) -> InputId {
    InputId::new(NonZeroU128::new(value).unwrap())
}
