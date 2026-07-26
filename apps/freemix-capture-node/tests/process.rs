use std::process::Command;

#[cfg(target_os = "macos")]
fn output_before(
    mut command: Command,
    timeout: std::time::Duration,
    helper_pid_file: Option<&std::path::Path>,
) -> std::process::Output {
    use std::{fs, process::Stdio, thread, time::Instant};

    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().unwrap(),
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(std::time::Duration::from_millis(5));
            }
            Ok(None) => {
                let _ = child.kill();
                if let Some(pid_file) = helper_pid_file
                    && let Ok(pid) = fs::read_to_string(pid_file)
                {
                    let _ = Command::new("kill").args(["-9", pid.trim()]).status();
                }
                let output = child.wait_with_output().unwrap();
                panic!(
                    "capture-node process timed out; stderr={}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Err(error) => panic!("cannot poll capture-node process: {error}"),
        }
    }
}

#[cfg(target_os = "macos")]
fn audio_octal(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::new();
    for byte in bytes {
        write!(output, "\\{byte:03o}").unwrap();
    }
    output
}

#[cfg(target_os = "macos")]
fn fake_audio_discovery() -> Vec<u8> {
    let mut bytes = b"FMAUDD1\0".to_vec();
    bytes.push(0);
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    for value in ["fake-microphone", "Fake Microphone"] {
        bytes.extend_from_slice(&u32::try_from(value.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(value.as_bytes());
    }
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    bytes.extend_from_slice(&48_000_u32.to_le_bytes());
    bytes.push(2);
    bytes
}

#[cfg(target_os = "macos")]
fn fake_audio_capture() -> Vec<u8> {
    let samples = [0.25_f32, -0.5, 1.0, -1.0];
    let payload_len = u32::try_from(std::mem::size_of_val(&samples)).unwrap();
    let mut bytes = b"FMAUDF1\0".to_vec();
    bytes.extend_from_slice(&(41 + payload_len).to_le_bytes());
    bytes.extend_from_slice(&0_u64.to_le_bytes());
    bytes.extend_from_slice(&2_u64.to_le_bytes());
    bytes.extend_from_slice(&48_000_i64.to_le_bytes());
    bytes.extend_from_slice(&48_000_i32.to_le_bytes());
    bytes.extend_from_slice(&48_000_u32.to_le_bytes());
    bytes.push(2);
    bytes.extend_from_slice(&2_u32.to_le_bytes());
    bytes.extend_from_slice(&payload_len.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

#[test]
fn help_documents_camera_diagnostics() {
    let output = Command::new(env!("CARGO_BIN_EXE_freemix-capture-node"))
        .arg("help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("cameras [--request-permission] [--helper <PATH>]"));
    assert!(stdout.contains("camera-smoke --source-index <INDEX>"));
    assert!(stdout.contains("never prompts unless"));
}

#[cfg(target_os = "macos")]
#[test]
#[allow(clippy::too_many_lines)]
fn camera_smoke_acquires_bounded_fake_helper_frames() {
    use std::{
        fmt::Write as _,
        fs,
        os::unix::fs::PermissionsExt,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn octal(bytes: &[u8]) -> String {
        let mut output = String::new();
        for byte in bytes {
            write!(output, "\\{byte:03o}").unwrap();
        }
        output
    }

    fn discovery() -> Vec<u8> {
        let mut bytes = b"FMCAMD2\0".to_vec();
        bytes.push(0);
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        for value in ["fake-camera", "Fake Camera"] {
            bytes.extend_from_slice(&u32::try_from(value.len()).unwrap().to_le_bytes());
            bytes.extend_from_slice(value.as_bytes());
        }
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&30_000_u32.to_le_bytes());
        bytes.extend_from_slice(&1_001_u32.to_le_bytes());
        bytes
    }

    fn capture() -> Vec<u8> {
        let mut bytes = b"FMCAMF3\0".to_vec();
        for (sequence, dropped, pts) in [(0_u64, 0_u64, 0_i64), (1, 1, 1_001)] {
            bytes.extend_from_slice(&62_u32.to_le_bytes());
            bytes.extend_from_slice(&sequence.to_le_bytes());
            bytes.extend_from_slice(&dropped.to_le_bytes());
            bytes.extend_from_slice(&pts.to_le_bytes());
            bytes.extend_from_slice(&30_000_i32.to_le_bytes());
            bytes.extend_from_slice(&1_001_i64.to_le_bytes());
            bytes.extend_from_slice(&30_000_i32.to_le_bytes());
            bytes.extend_from_slice(&1_u32.to_le_bytes());
            bytes.extend_from_slice(&1_u32.to_le_bytes());
            bytes.extend_from_slice(&4_u32.to_le_bytes());
            bytes.extend_from_slice(&4_u32.to_le_bytes());
            bytes.extend_from_slice(&[
                u8::try_from(2 + sequence).unwrap(),
                if sequence == 0 { 1 } else { 2 },
            ]);
            bytes.extend_from_slice(&[0, 0, 0, 255]);
        }
        bytes
    }

    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let helper = std::env::temp_dir().join(format!("fm-camera-smoke-{suffix}.sh"));
    let pid_file = std::env::temp_dir().join(format!("fm-camera-smoke-{suffix}.pid"));
    let args_file = std::env::temp_dir().join(format!("fm-camera-smoke-{suffix}.args"));
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in\n  discover) printf '{}';;\n  capture) printf '%s\\n' \"$@\" > \"$FM_HELPER_ARGS_FILE\"; printf '%s' \"$$\" > \"$FM_HELPER_PID_FILE\"; printf '{}'; exec sleep 30;;\n  *) exit 90;;\nesac\n",
        octal(&discovery()),
        octal(&capture()),
    );
    fs::write(&helper, script).unwrap();
    let mut permissions = fs::metadata(&helper).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&helper, permissions).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_freemix-capture-node"));
    command
        .args([
            "camera-smoke",
            "--source-index",
            "0",
            "--frames",
            "2",
            "--timeout-ms",
            "2000",
            "--helper",
        ])
        .arg(&helper)
        .env("FM_HELPER_PID_FILE", &pid_file)
        .env("FM_HELPER_ARGS_FILE", &args_file);
    let output = output_before(command, std::time::Duration::from_secs(5), Some(&pid_file));
    fs::remove_file(helper).unwrap();
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with(
        "FREEMIX_CAPTURE_CAMERA_SMOKE\tv=1\tclassification=diagnostic-not-certification"
    ));
    assert!(stdout.contains("\trequested_frames=2\treceived_frames=2\t"));
    assert!(stdout.contains("\tfirst_sequence=0\tlast_sequence=1\t"));
    assert!(stdout.contains("\tnative_dropped=1\tqueue_dropped=0\t"));
    assert_eq!(
        fs::read_to_string(&args_file).unwrap(),
        "capture\nfake-camera\n1\n1\n30000\n1001\n"
    );
    fs::remove_file(args_file).unwrap();
    let helper_pid = fs::read_to_string(&pid_file).unwrap();
    fs::remove_file(pid_file).unwrap();
    let helper_alive = Command::new("kill")
        .args(["-0", helper_pid.trim()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success();
    if helper_alive {
        let _ = Command::new("kill")
            .args(["-9", helper_pid.trim()])
            .status();
        panic!("camera helper process {helper_pid} survived smoke cleanup");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn camera_smoke_prompt_required_never_invokes_capture() {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        time::{SystemTime, UNIX_EPOCH},
    };

    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let helper = std::env::temp_dir().join(format!("fm-camera-preflight-{suffix}.sh"));
    let capture_marker = std::env::temp_dir().join(format!("fm-camera-preflight-{suffix}.capture"));
    fs::write(
        &helper,
        r#"#!/bin/sh
case "$1" in
  discover) printf '\106\115\103\101\115\104\062\000\001\001\000\000\000\013\000\000\000fake-camera\013\000\000\000Fake Camera\001\000\000\000\001\000\000\000\001\000\000\000\036\000\000\000\001\000\000\000';;
  capture) touch "$FM_CAPTURE_MARKER"; exit 91;;
  *) exit 90;;
esac
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&helper).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&helper, permissions).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_freemix-capture-node"));
    command
        .args([
            "camera-smoke",
            "--source-index",
            "0",
            "--frames",
            "1",
            "--timeout-ms",
            "1000",
            "--helper",
        ])
        .arg(&helper)
        .env("FM_CAPTURE_MARKER", &capture_marker);
    let output = output_before(command, std::time::Duration::from_secs(5), None);
    fs::remove_file(helper).unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("camera permission is not granted"));
    assert!(
        !capture_marker.exists(),
        "permission preflight invoked capture"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn camera_diagnostic_discovers_without_requesting_permission() {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        time::{SystemTime, UNIX_EPOCH},
    };

    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let helper = std::env::temp_dir().join(format!("fm-camera-discovery-{suffix}.sh"));
    fs::write(
        &helper,
        "#!/bin/sh\n[ \"$1\" = discover ] || exit 90\nprintf '\\106\\115\\103\\101\\115\\104\\062\\000\\001\\000\\000\\000\\000'\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&helper).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&helper, permissions).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_freemix-capture-node"));
    command.arg("cameras").arg("--helper").arg(&helper);
    let output = output_before(command, std::time::Duration::from_secs(5), None);
    fs::remove_file(helper).unwrap();
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        stdout,
        "FREEMIX_CAPTURE_CAMERAS\tv=2\tplatform=macos\tpermission=prompt-required\tsources=0\n"
    );
}

#[cfg(target_os = "macos")]
#[test]
#[allow(clippy::too_many_lines)]
fn audio_commands_discover_and_capture_without_requesting_permission() {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        time::{SystemTime, UNIX_EPOCH},
    };

    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let helper = std::env::temp_dir().join(format!("fm-audio-command-{suffix}.sh"));
    let pid_file = helper.with_extension("pid");
    let args_file = helper.with_extension("args");
    let permission_marker = helper.with_extension("permission");
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in\n  discover-audio) printf '{}';;\n  request-audio-permission) touch \"$FM_PERMISSION_MARKER\";;\n  capture-audio) printf '%s\\n' \"$@\" > \"$FM_HELPER_ARGS_FILE\"; printf '%s' \"$$\" > \"$FM_HELPER_PID_FILE\"; printf '{}'; exec sleep 30;;\n  *) exit 90;;\nesac\n",
        audio_octal(&fake_audio_discovery()),
        audio_octal(&fake_audio_capture()),
    );
    fs::write(&helper, script).unwrap();
    let mut permissions = fs::metadata(&helper).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&helper, permissions).unwrap();

    let discovery = Command::new(env!("CARGO_BIN_EXE_freemix-capture-node"))
        .args(["audio-inputs", "--helper"])
        .arg(&helper)
        .env("FM_PERMISSION_MARKER", &permission_marker)
        .output()
        .unwrap();
    assert!(discovery.status.success());
    let stdout = String::from_utf8(discovery.stdout).unwrap();
    assert!(stdout.contains("FREEMIX_CAPTURE_AUDIO_INPUTS\tv=1"));
    assert!(stdout.contains("stable_key=macos.avfoundation.audio.v1."));
    let stable_key = stdout
        .lines()
        .find(|line| line.starts_with("FREEMIX_CAPTURE_AUDIO_INPUT\t"))
        .and_then(|line| {
            line.split('\t')
                .find_map(|field| field.strip_prefix("stable_key="))
        })
        .unwrap()
        .to_owned();
    assert!(!permission_marker.exists());

    let mut command = Command::new(env!("CARGO_BIN_EXE_freemix-capture-node"));
    command
        .args(["audio-smoke", "--stable-key"])
        .arg(stable_key)
        .args([
            "--sample-rate",
            "48000",
            "--channels",
            "2",
            "--blocks",
            "1",
            "--timeout-ms",
            "2000",
            "--helper",
        ])
        .arg(&helper)
        .env("FM_PERMISSION_MARKER", &permission_marker)
        .env("FM_HELPER_PID_FILE", &pid_file)
        .env("FM_HELPER_ARGS_FILE", &args_file);
    let output = output_before(command, std::time::Duration::from_secs(5), Some(&pid_file));
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("FREEMIX_CAPTURE_AUDIO_SMOKE\tv=1"));
    assert!(stdout.contains("\treceived_blocks=1\treceived_samples=2\t"));
    assert!(stdout.contains("\tnative_drop_measurement=unavailable\tqueue_overruns=0\t"));
    assert!(stdout.contains("\tpeak_abs=1.000000\t"));
    assert_eq!(
        fs::read_to_string(&args_file).unwrap(),
        "capture-audio\nfake-microphone\n48000\n2\n"
    );
    assert!(!permission_marker.exists());

    let helper_pid = fs::read_to_string(&pid_file).unwrap();
    let helper_alive = Command::new("kill")
        .args(["-0", helper_pid.trim()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success();
    if helper_alive {
        let _ = Command::new("kill")
            .args(["-9", helper_pid.trim()])
            .status();
        panic!("audio helper process {helper_pid} survived smoke cleanup");
    }
    fs::remove_file(helper).unwrap();
    fs::remove_file(pid_file).unwrap();
    fs::remove_file(args_file).unwrap();
}
