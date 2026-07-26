use fm_io_macos::{MacosAudioAdapter, MacosCameraAdapter};

#[cfg(target_os = "macos")]
use fm_io_api::{Discovery, PermissionState};

#[test]
#[ignore = "requires macOS camera hardware and an explicit requirement flag"]
fn macos_camera_hardware_discovery_smoke() {
    let require_discovery = std::env::var_os("FM_REQUIRE_MACOS_CAMERA_DISCOVERY").as_deref()
        == Some(std::ffi::OsStr::new("1"));
    let require_permission =
        std::env::var_os("FM_REQUIRE_MACOS_CAMERA").as_deref() == Some(std::ffi::OsStr::new("1"));
    if !require_discovery && !require_permission {
        return;
    }
    #[cfg(not(target_os = "macos"))]
    panic!("macOS camera requirement flags require macOS");

    #[cfg(target_os = "macos")]
    {
        let adapter = MacosCameraAdapter::discover().expect("camera discovery must succeed");
        let snapshot = adapter.snapshot();
        assert!(
            !snapshot.sources.is_empty(),
            "no macOS camera was discovered"
        );
        if require_permission {
            assert!(
                snapshot
                    .sources
                    .iter()
                    .any(|source| source.permission == PermissionState::Granted),
                "camera permission is not granted"
            );
        }
    }
}

#[test]
#[ignore = "requires macOS audio hardware and an explicit requirement flag"]
fn macos_audio_hardware_discovery_smoke() {
    let require_discovery = std::env::var_os("FM_REQUIRE_MACOS_AUDIO_DISCOVERY").as_deref()
        == Some(std::ffi::OsStr::new("1"));
    let require_permission =
        std::env::var_os("FM_REQUIRE_MACOS_AUDIO").as_deref() == Some(std::ffi::OsStr::new("1"));
    if !require_discovery && !require_permission {
        return;
    }
    #[cfg(not(target_os = "macos"))]
    panic!("macOS audio requirement flags require macOS");

    #[cfg(target_os = "macos")]
    {
        let adapter = MacosAudioAdapter::discover().expect("audio discovery must succeed");
        let snapshot = adapter.snapshot();
        assert!(
            !snapshot.sources.is_empty(),
            "no macOS audio input was discovered"
        );
        if require_permission {
            assert!(
                snapshot
                    .sources
                    .iter()
                    .any(|source| source.permission == PermissionState::Granted),
                "microphone permission is not granted"
            );
        }
    }
}
