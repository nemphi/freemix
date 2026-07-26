use core::fmt::{self, Write as _};

#[cfg(target_os = "macos")]
use std::{
    num::NonZeroUsize,
    thread,
    time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
use fm_io_api::{
    Discovery, LifecycleState, MediaSource, MediaTransfer, MemoryDomain, OpenOptions,
    SignalLossPolicy,
};
use fm_io_api::{DiscoverySnapshot, PermissionState};
use fm_io_macos::CameraError;
#[cfg(target_os = "macos")]
use fm_io_macos::{CameraVideoSource, MacosCameraAdapter};

use crate::args::{
    CameraConfig, CameraSmokeConfig, MAX_CAMERA_SMOKE_FRAMES, MAX_CAMERA_SMOKE_TIMEOUT_MS,
    MIN_CAMERA_SMOKE_TIMEOUT_MS,
};

const HEX: &[u8; 16] = b"0123456789ABCDEF";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CameraDiagnosticError(String);

impl fmt::Display for CameraDiagnosticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CameraDiagnosticError {}

impl From<CameraError> for CameraDiagnosticError {
    fn from(error: CameraError) -> Self {
        Self(error.to_string())
    }
}

/// Discovers platform cameras and returns bounded, one-line diagnostic records.
///
/// Permission is requested only when `config.request_permission` is true.
///
/// # Errors
///
/// Returns a platform, helper, permission, or discovery error.
pub fn camera_diagnostics(config: &CameraConfig) -> Result<String, CameraDiagnosticError> {
    #[cfg(target_os = "macos")]
    {
        if config.request_permission {
            match config.helper.as_deref() {
                Some(helper) => {
                    MacosCameraAdapter::request_camera_permission_with_helper(helper)?;
                }
                None => {
                    MacosCameraAdapter::request_camera_permission()?;
                }
            }
        }
        let adapter = match config.helper.as_deref() {
            Some(helper) => MacosCameraAdapter::discover_with_helper(helper)?,
            None => MacosCameraAdapter::discover()?,
        };
        Ok(format_report(adapter.permission(), &adapter.snapshot()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = config;
        Err(CameraError::UnsupportedPlatform.into())
    }
}

/// Acquires a bounded number of frames from one exactly selected camera.
///
/// This diagnostic never requests permission.
///
/// # Errors
///
/// Returns a platform, helper, permission, selection, capture, timeout, or
/// cleanup error.
pub fn camera_smoke(config: &CameraSmokeConfig) -> Result<String, CameraDiagnosticError> {
    validate_smoke_config(config)?;
    #[cfg(target_os = "macos")]
    {
        camera_smoke_macos(config)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = config;
        Err(CameraError::UnsupportedPlatform.into())
    }
}

fn validate_smoke_config(config: &CameraSmokeConfig) -> Result<(), CameraDiagnosticError> {
    if !(1..=MAX_CAMERA_SMOKE_FRAMES).contains(&config.frames) {
        return Err(CameraDiagnosticError(format!(
            "camera frame count {} must be between 1 and {MAX_CAMERA_SMOKE_FRAMES}",
            config.frames
        )));
    }
    if !(MIN_CAMERA_SMOKE_TIMEOUT_MS..=MAX_CAMERA_SMOKE_TIMEOUT_MS).contains(&config.timeout_ms) {
        return Err(CameraDiagnosticError(format!(
            "camera acquisition timeout {} must be between {MIN_CAMERA_SMOKE_TIMEOUT_MS} and {MAX_CAMERA_SMOKE_TIMEOUT_MS} ms",
            config.timeout_ms
        )));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[allow(clippy::too_many_lines)]
fn camera_smoke_macos(config: &CameraSmokeConfig) -> Result<String, CameraDiagnosticError> {
    let adapter = match config.helper.as_deref() {
        Some(helper) => MacosCameraAdapter::discover_with_helper(helper)?,
        None => MacosCameraAdapter::discover()?,
    };
    if !adapter.permission().is_granted() {
        return Err(CameraDiagnosticError(
            "camera permission is not granted; run `freemix-capture-node cameras --request-permission` from an interactive desktop session".into(),
        ));
    }
    let snapshot = adapter.snapshot();
    let descriptor = snapshot.sources.get(config.source_index).ok_or_else(|| {
        CameraDiagnosticError(format!(
            "camera source index {} is unavailable; discovery returned {} sources",
            config.source_index,
            snapshot.sources.len()
        ))
    })?;
    let format = descriptor
        .capabilities
        .formats
        .get(config.format_index)
        .cloned()
        .ok_or_else(|| {
            CameraDiagnosticError(format!(
                "camera format index {} is unavailable for source {}; discovery returned {} formats",
                config.format_index,
                descriptor.id,
                descriptor.capabilities.formats.len()
            ))
        })?;
    let mut source = adapter.open_video_source(descriptor.id)?;
    source
        .open(OpenOptions {
            format,
            clock_domain: descriptor
                .capabilities
                .clocks
                .first()
                .ok_or_else(|| CameraDiagnosticError("camera has no advertised clock".into()))?
                .domain,
            memory_domain: MemoryDomain::Cpu,
            queue_capacity: NonZeroUsize::new(4).expect("camera queue capacity is nonzero"),
            signal_loss: SignalLossPolicy::Stop,
        })
        .map_err(|error| CameraDiagnosticError(error.to_string()))?;
    if let Err(error) = source.start() {
        return Err(capture_error_with_cleanup(&mut source, error.to_string()));
    }

    let started = Instant::now();
    let deadline = started
        .checked_add(Duration::from_millis(config.timeout_ms))
        .ok_or_else(|| CameraDiagnosticError("camera smoke deadline overflow".into()))?;
    let mut first = None;
    let mut last = None;
    let mut received = 0_usize;
    while received < config.frames {
        if Instant::now() >= deadline {
            return Err(capture_error_with_cleanup(
                &mut source,
                format!(
                    "camera acquisition timed out after {} ms with {received}/{} frames",
                    config.timeout_ms, config.frames
                ),
            ));
        }
        match source.try_receive() {
            Ok(Some(MediaTransfer::Live(frame))) => {
                let timing = frame.timing();
                first.get_or_insert(timing);
                last = Some(timing);
                received += 1;
                if Instant::now() >= deadline {
                    return Err(capture_error_with_cleanup(
                        &mut source,
                        format!(
                            "camera acquisition exceeded {} ms with {received}/{} frames",
                            config.timeout_ms, config.frames
                        ),
                    ));
                }
            }
            Ok(Some(MediaTransfer::Fallback { .. })) => {
                return Err(capture_error_with_cleanup(
                    &mut source,
                    "camera smoke received fallback media".into(),
                ));
            }
            Ok(None) => thread::sleep(Duration::from_millis(2)),
            Err(error) => {
                return Err(capture_error_with_cleanup(&mut source, error.to_string()));
            }
        }
    }
    let telemetry = source.telemetry();
    let acquisition_elapsed = started.elapsed();
    cleanup_source(&mut source)?;
    let first = first.expect("a positive frame request produces a first frame");
    let last = last.expect("a positive frame request produces a last frame");
    Ok(format!(
        "FREEMIX_CAPTURE_CAMERA_SMOKE\tv=1\tclassification=diagnostic-not-certification\tplatform=macos\tsource_index={}\tsource_id={}\tformat_index={}\trequested_frames={}\treceived_frames={received}\tacquisition_elapsed_ms={}\tfirst_sequence={}\tlast_sequence={}\tfirst_pts_ns={}\tlast_pts_ns={}\tnative_dropped={}\tqueue_dropped={}\tqueue_peak={}\tname={}",
        config.source_index,
        descriptor.id,
        config.format_index,
        config.frames,
        acquisition_elapsed.as_millis(),
        first.sequence().get(),
        last.sequence().get(),
        first.presentation_timestamp().as_nanos(),
        last.presentation_timestamp().as_nanos(),
        telemetry.native_dropped,
        telemetry.dropped,
        telemetry.peak,
        encode_field(&descriptor.name),
    ))
}

#[cfg(target_os = "macos")]
fn capture_error_with_cleanup(
    source: &mut CameraVideoSource,
    detail: String,
) -> CameraDiagnosticError {
    match cleanup_source(source) {
        Ok(()) => CameraDiagnosticError(detail),
        Err(cleanup) => CameraDiagnosticError(format!("{detail}; cleanup failed: {cleanup}")),
    }
}

#[cfg(target_os = "macos")]
fn cleanup_source(source: &mut CameraVideoSource) -> Result<(), CameraDiagnosticError> {
    if source.lifecycle() == LifecycleState::Running {
        source
            .stop()
            .map_err(|error| CameraDiagnosticError(error.to_string()))?;
    }
    if matches!(
        source.lifecycle(),
        LifecycleState::Open | LifecycleState::Lost
    ) {
        source
            .close()
            .map_err(|error| CameraDiagnosticError(error.to_string()))?;
    }
    if source.lifecycle() != LifecycleState::Closed {
        return Err(CameraDiagnosticError(format!(
            "camera source cleanup ended in {:?}",
            source.lifecycle()
        )));
    }
    Ok(())
}

fn format_report(permission: &PermissionState, snapshot: &DiscoverySnapshot) -> String {
    let mut report = format!(
        "FREEMIX_CAPTURE_CAMERAS\tv=2\tplatform={}\tpermission={}\tsources={}\n",
        std::env::consts::OS,
        permission_label(permission),
        snapshot.sources.len(),
    );
    for (index, source) in snapshot.sources.iter().enumerate() {
        writeln!(
            report,
            "FREEMIX_CAPTURE_CAMERA\tv=2\tindex={index}\tsource_id={}\tdevice_id={}\tstable_key={}\tformats={}\tname={}",
            source.id,
            source.device_id,
            encode_field(&source.stable_key),
            source.capabilities.formats.len(),
            encode_field(&source.name),
        )
        .expect("writing to a String cannot fail");
    }
    report
}

const fn permission_label(permission: &PermissionState) -> &'static str {
    match permission {
        PermissionState::Granted => "granted",
        PermissionState::PromptRequired { .. } => "prompt-required",
        PermissionState::Denied { .. } => "denied",
        PermissionState::Restricted { .. } => "restricted",
    }
}

fn encode_field(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b' ' | b'-' | b'_' | b'.') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU128, NonZeroUsize};

    use fm_io_api::{
        DeviceId, DriverState, EndpointCapabilities, PermissionState, Remediation,
        SourceDescriptor, SourceId, TransferLimits,
    };

    use super::*;

    #[test]
    fn report_is_versioned_ordered_and_one_line_per_source() {
        let snapshot = DiscoverySnapshot {
            generation: 0,
            sources: vec![SourceDescriptor {
                id: SourceId::new(NonZeroU128::new(2).unwrap()),
                device_id: DeviceId::new(NonZeroU128::new(1).unwrap()),
                stable_key: "macos.avfoundation.camera.v1.2".into(),
                name: "Camera\tA/\u{65e5}\u{672c}".into(),
                capabilities: EndpointCapabilities {
                    formats: Vec::new(),
                    clocks: Vec::new(),
                    memory_domains: Vec::new(),
                    transfer: TransferLimits::new(NonZeroUsize::MIN, NonZeroUsize::MIN),
                },
                permission: PermissionState::Denied {
                    remediation: Remediation::OpenSystemSettings,
                },
                driver: DriverState::Ready,
            }],
            sinks: Vec::new(),
        };
        let report = format_report(
            &PermissionState::Denied {
                remediation: Remediation::OpenSystemSettings,
            },
            &snapshot,
        );
        assert_eq!(report.lines().count(), 2);
        assert!(report.starts_with("FREEMIX_CAPTURE_CAMERAS\tv=2\tplatform="));
        assert!(report.contains("\tpermission=denied\tsources=1\n"));
        assert!(report.contains("\tstable_key=macos.avfoundation.camera.v1.2\t"));
        assert!(report.contains("\tname=Camera%09A%2F%E6%97%A5%E6%9C%AC\n"));
    }

    #[test]
    fn permission_labels_are_stable() {
        assert_eq!(permission_label(&PermissionState::Granted), "granted");
        assert_eq!(
            permission_label(&PermissionState::PromptRequired {
                remediation: Remediation::RequestPermission,
            }),
            "prompt-required"
        );
        assert_eq!(
            permission_label(&PermissionState::Restricted {
                remediation: Remediation::ContactAdministrator,
            }),
            "restricted"
        );
    }

    #[test]
    fn public_smoke_api_rejects_unbounded_configs_before_platform_access() {
        let mut config = CameraSmokeConfig {
            source_index: 0,
            format_index: 0,
            frames: 0,
            timeout_ms: MIN_CAMERA_SMOKE_TIMEOUT_MS,
            helper: None,
        };
        assert!(
            camera_smoke(&config)
                .unwrap_err()
                .to_string()
                .contains("frame count")
        );
        config.frames = 1;
        config.timeout_ms = MIN_CAMERA_SMOKE_TIMEOUT_MS - 1;
        assert!(
            camera_smoke(&config)
                .unwrap_err()
                .to_string()
                .contains("timeout")
        );
    }
}
