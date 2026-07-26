#![cfg(target_os = "macos")]

use std::{
    fmt::Write as _,
    fs,
    num::NonZeroUsize,
    os::unix::fs::PermissionsExt,
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use fm_io_api::{
    Discovery, MediaSource, MediaTransfer, MemoryDomain, OpenOptions, SignalLossPolicy,
};
use fm_io_macos::MacosAudioAdapter;

fn octal(bytes: &[u8]) -> String {
    let mut output = String::new();
    for byte in bytes {
        write!(output, "\\{byte:03o}").unwrap();
    }
    output
}

fn discovery() -> Vec<u8> {
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

fn capture() -> Vec<u8> {
    let samples = [0.25_f32, -0.5, 1.0, -1.0];
    let payload_len = u32::try_from(samples.len() * size_of::<f32>()).unwrap();
    let mut bytes = b"FMAUDF1\0".to_vec();
    bytes.extend_from_slice(&(41 + payload_len).to_le_bytes());
    bytes.extend_from_slice(&0_u64.to_le_bytes());
    bytes.extend_from_slice(&3_u64.to_le_bytes());
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
fn audio_source_streams_exact_bounded_pcm_and_reaps_helper() {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let helper = std::env::temp_dir().join(format!("fm-audio-helper-{suffix}.sh"));
    let pid_file = helper.with_extension("pid");
    let args_file = helper.with_extension("args");
    let permission_marker = helper.with_extension("permission");
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in\n  discover-audio) printf '{}';;\n  request-audio-permission) touch '{}';;\n  capture-audio) printf '%s\\n' \"$@\" > '{}'; printf '%s' \"$$\" > '{}'; printf '{}'; exec sleep 30;;\n  *) exit 90;;\nesac\n",
        octal(&discovery()),
        permission_marker.display(),
        args_file.display(),
        pid_file.display(),
        octal(&capture()),
    );
    fs::write(&helper, script).unwrap();
    let mut permissions = fs::metadata(&helper).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&helper, permissions).unwrap();

    let adapter = MacosAudioAdapter::discover_with_helper(&helper).unwrap();
    assert!(!permission_marker.exists());
    let descriptor = adapter.snapshot().sources[0].clone();
    let mut source = adapter.open_audio_source(descriptor.id).unwrap();
    source
        .open(OpenOptions {
            format: descriptor.capabilities.formats[0].clone(),
            clock_domain: descriptor.capabilities.clocks[0].domain,
            memory_domain: MemoryDomain::Cpu,
            queue_capacity: NonZeroUsize::new(2).unwrap(),
            signal_loss: SignalLossPolicy::Stop,
        })
        .unwrap();
    source.start().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let block = loop {
        match source.try_receive().unwrap() {
            Some(MediaTransfer::Live(block)) => break block,
            Some(MediaTransfer::Fallback { .. }) => panic!("audio source returned fallback media"),
            None if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            None => panic!("audio source did not deliver a block"),
        }
    };
    assert_eq!(block.sample_count(), 2);
    assert_eq!(block.plane(0).unwrap(), &[0.25, 1.0]);
    assert_eq!(block.plane(1).unwrap(), &[-0.5, -1.0]);
    assert_eq!(
        block.timing().presentation_timestamp().as_nanos(),
        1_000_000_000
    );
    assert_eq!(source.telemetry().received, 1);
    assert_eq!(source.telemetry().native_dropped, 3);
    source.stop().unwrap();
    source.close().unwrap();

    assert_eq!(
        fs::read_to_string(&args_file).unwrap(),
        "capture-audio\nfake-microphone\n48000\n2\n"
    );
    let pid = fs::read_to_string(&pid_file).unwrap();
    assert!(
        !Command::new("kill")
            .args(["-0", pid.trim()])
            .status()
            .unwrap()
            .success()
    );
    assert!(!permission_marker.exists());
    fs::remove_file(helper).unwrap();
    fs::remove_file(pid_file).unwrap();
    fs::remove_file(args_file).unwrap();
}
