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
use fm_io_macos::AudioError;
#[cfg(target_os = "macos")]
use fm_io_macos::{MacosAudioAdapter, MacosAudioSource};
#[cfg(target_os = "macos")]
use fm_types::SampleRate;

use crate::args::{
    AudioConfig, AudioSmokeConfig, MAX_AUDIO_SMOKE_BLOCKS, MAX_CAMERA_SMOKE_TIMEOUT_MS,
    MIN_CAMERA_SMOKE_TIMEOUT_MS,
};

const HEX: &[u8; 16] = b"0123456789ABCDEF";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioDiagnosticError(String);

impl fmt::Display for AudioDiagnosticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for AudioDiagnosticError {}

impl From<AudioError> for AudioDiagnosticError {
    fn from(error: AudioError) -> Self {
        Self(error.to_string())
    }
}

/// Discovers microphone inputs and returns bounded one-line records.
///
/// Permission is requested only when `config.request_permission` is true.
///
/// # Errors
///
/// Returns a platform, helper, permission, or discovery error.
pub fn audio_diagnostics(config: &AudioConfig) -> Result<String, AudioDiagnosticError> {
    #[cfg(target_os = "macos")]
    {
        if config.request_permission {
            match config.helper.as_deref() {
                Some(helper) => {
                    MacosAudioAdapter::request_microphone_permission_with_helper(helper)?;
                }
                None => {
                    MacosAudioAdapter::request_microphone_permission()?;
                }
            }
        }
        let adapter = match config.helper.as_deref() {
            Some(helper) => MacosAudioAdapter::discover_with_helper(helper)?,
            None => MacosAudioAdapter::discover()?,
        };
        Ok(format_report(adapter.permission(), &adapter.snapshot()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = config;
        Err(AudioError::UnsupportedPlatform.into())
    }
}

/// Acquires bounded F32 blocks from one exactly selected microphone.
///
/// This diagnostic never requests permission or performs resampling.
///
/// # Errors
///
/// Returns a platform, helper, permission, selection, capture, timeout, or
/// cleanup error.
pub fn audio_smoke(config: &AudioSmokeConfig) -> Result<String, AudioDiagnosticError> {
    validate_smoke_config(config)?;
    #[cfg(target_os = "macos")]
    {
        audio_smoke_macos(config)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = config;
        Err(AudioError::UnsupportedPlatform.into())
    }
}

fn validate_smoke_config(config: &AudioSmokeConfig) -> Result<(), AudioDiagnosticError> {
    if !(1..=MAX_AUDIO_SMOKE_BLOCKS).contains(&config.blocks) {
        return Err(AudioDiagnosticError(format!(
            "audio block count {} must be between 1 and {MAX_AUDIO_SMOKE_BLOCKS}",
            config.blocks
        )));
    }
    if !(MIN_CAMERA_SMOKE_TIMEOUT_MS..=MAX_CAMERA_SMOKE_TIMEOUT_MS).contains(&config.timeout_ms) {
        return Err(AudioDiagnosticError(format!(
            "audio acquisition timeout {} must be between {MIN_CAMERA_SMOKE_TIMEOUT_MS} and {MAX_CAMERA_SMOKE_TIMEOUT_MS} ms",
            config.timeout_ms
        )));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[allow(clippy::too_many_lines)]
fn audio_smoke_macos(config: &AudioSmokeConfig) -> Result<String, AudioDiagnosticError> {
    let adapter = match config.helper.as_deref() {
        Some(helper) => MacosAudioAdapter::discover_with_helper(helper)?,
        None => MacosAudioAdapter::discover()?,
    };
    if !adapter.permission().is_granted() {
        return Err(AudioDiagnosticError(
            "microphone permission is not granted; run `freemix-capture-node audio-inputs --request-permission` from an interactive desktop session".into(),
        ));
    }
    let mut source = adapter.open_audio_source_by_stable_key(&config.stable_key)?;
    let descriptor = source.descriptor().clone();
    let sample_rate = SampleRate::new(config.sample_rate)
        .ok_or_else(|| AudioDiagnosticError("audio sample rate must be positive".into()))?;
    let format = source
        .exact_audio_format(sample_rate, config.channels)
        .ok_or_else(|| {
            AudioDiagnosticError(format!(
                "audio format {} Hz/{} channels was not advertised for the selected stable key",
                config.sample_rate, config.channels
            ))
        })?;
    source
        .open(OpenOptions {
            format,
            clock_domain: descriptor
                .capabilities
                .clocks
                .first()
                .ok_or_else(|| AudioDiagnosticError("audio source has no advertised clock".into()))?
                .domain,
            memory_domain: MemoryDomain::Cpu,
            queue_capacity: NonZeroUsize::new(16).expect("audio queue capacity is nonzero"),
            signal_loss: SignalLossPolicy::Stop,
        })
        .map_err(|error| AudioDiagnosticError(error.to_string()))?;
    if let Err(error) = source.start() {
        return Err(capture_error_with_cleanup(&mut source, error.to_string()));
    }

    let started = Instant::now();
    let deadline = started
        .checked_add(Duration::from_millis(config.timeout_ms))
        .ok_or_else(|| AudioDiagnosticError("audio smoke deadline overflow".into()))?;
    let mut first = None;
    let mut last = None;
    let mut received = 0_usize;
    let mut samples = 0_u64;
    let mut peak = 0.0_f32;
    while received < config.blocks {
        if Instant::now() >= deadline {
            return Err(capture_error_with_cleanup(
                &mut source,
                format!(
                    "audio acquisition timed out after {} ms with {received}/{} blocks",
                    config.timeout_ms, config.blocks
                ),
            ));
        }
        match source.try_receive() {
            Ok(Some(MediaTransfer::Live(block))) => {
                let timing = block.timing();
                first.get_or_insert(timing);
                last = Some(timing);
                received += 1;
                samples =
                    samples.saturating_add(u64::try_from(block.sample_count()).unwrap_or(u64::MAX));
                for sample in block.planes().iter().flatten() {
                    peak = peak.max(sample.abs());
                }
            }
            Ok(Some(MediaTransfer::Fallback { .. })) => {
                return Err(capture_error_with_cleanup(
                    &mut source,
                    "audio smoke received fallback media".into(),
                ));
            }
            Ok(None) => thread::sleep(Duration::from_millis(2)),
            Err(error) => {
                return Err(capture_error_with_cleanup(&mut source, error.to_string()));
            }
        }
    }
    let telemetry = source.telemetry();
    let elapsed = started.elapsed();
    cleanup_source(&mut source)?;
    let first = first.expect("a positive block request produces a first block");
    let last = last.expect("a positive block request produces a last block");
    Ok(format!(
        "FREEMIX_CAPTURE_AUDIO_SMOKE\tv=1\tclassification=diagnostic-not-certification\tplatform=macos\tsource_id={}\tsample_rate={}\tchannels={}\trequested_blocks={}\treceived_blocks={received}\treceived_samples={samples}\tacquisition_elapsed_ms={}\tfirst_sequence={}\tlast_sequence={}\tfirst_pts_ns={}\tlast_pts_ns={}\tnative_drop_measurement=unavailable\tqueue_overruns={}\tqueue_peak={}\tpeak_abs={peak:.6}\tname={}",
        descriptor.id,
        config.sample_rate,
        config.channels,
        config.blocks,
        elapsed.as_millis(),
        first.sequence().get(),
        last.sequence().get(),
        first.presentation_timestamp().as_nanos(),
        last.presentation_timestamp().as_nanos(),
        telemetry.overruns,
        telemetry.peak,
        encode_field(&descriptor.name),
    ))
}

#[cfg(target_os = "macos")]
fn capture_error_with_cleanup(
    source: &mut MacosAudioSource,
    detail: String,
) -> AudioDiagnosticError {
    match cleanup_source(source) {
        Ok(()) => AudioDiagnosticError(detail),
        Err(cleanup) => AudioDiagnosticError(format!("{detail}; cleanup failed: {cleanup}")),
    }
}

#[cfg(target_os = "macos")]
fn cleanup_source(source: &mut MacosAudioSource) -> Result<(), AudioDiagnosticError> {
    if source.lifecycle() == LifecycleState::Running {
        source
            .stop()
            .map_err(|error| AudioDiagnosticError(error.to_string()))?;
    }
    if matches!(
        source.lifecycle(),
        LifecycleState::Open | LifecycleState::Lost
    ) {
        source
            .close()
            .map_err(|error| AudioDiagnosticError(error.to_string()))?;
    }
    if source.lifecycle() != LifecycleState::Closed {
        return Err(AudioDiagnosticError(format!(
            "audio source cleanup ended in {:?}",
            source.lifecycle()
        )));
    }
    Ok(())
}

fn format_report(permission: &PermissionState, snapshot: &DiscoverySnapshot) -> String {
    let mut report = format!(
        "FREEMIX_CAPTURE_AUDIO_INPUTS\tv=1\tplatform={}\tpermission={}\tsources={}\n",
        std::env::consts::OS,
        permission_label(permission),
        snapshot.sources.len(),
    );
    for (index, source) in snapshot.sources.iter().enumerate() {
        writeln!(
            report,
            "FREEMIX_CAPTURE_AUDIO_INPUT\tv=1\tindex={index}\tsource_id={}\tdevice_id={}\tstable_key={}\tformats={}\tname={}",
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
        DeviceId, DriverState, EndpointCapabilities, SourceDescriptor, SourceId, TransferLimits,
    };

    use super::*;

    #[test]
    fn report_is_versioned_and_escapes_fields() {
        let snapshot = DiscoverySnapshot {
            generation: 0,
            sources: vec![SourceDescriptor {
                id: SourceId::new(NonZeroU128::new(2).unwrap()),
                device_id: DeviceId::new(NonZeroU128::new(3).unwrap()),
                stable_key: "audio\tkey".into(),
                name: "Mic\nOne".into(),
                capabilities: EndpointCapabilities {
                    formats: Vec::new(),
                    clocks: Vec::new(),
                    memory_domains: Vec::new(),
                    transfer: TransferLimits::new(
                        NonZeroUsize::new(1).unwrap(),
                        NonZeroUsize::new(1).unwrap(),
                    ),
                },
                permission: PermissionState::Granted,
                driver: DriverState::Ready,
            }],
            sinks: Vec::new(),
        };
        let report = format_report(&PermissionState::Granted, &snapshot);
        assert!(report.contains("FREEMIX_CAPTURE_AUDIO_INPUTS\tv=1"));
        assert!(report.contains("stable_key=audio%09key"));
        assert!(report.contains("name=Mic%0AOne"));
        assert_eq!(report.lines().count(), 2);
    }
}
