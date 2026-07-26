use std::path::PathBuf;
use std::time::Duration;

use fm_frame::CpuVideoPayload;

/// How an `FFmpeg` executable is located.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Executable {
    /// Resolve the conventional executable name through `PATH` when spawned.
    SearchPath,
    /// Use this explicit executable path. The adapter canonicalizes it once.
    Explicit(PathBuf),
}

/// Conservative resource limits for discovery, probing, and decoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub max_input_bytes: u64,
    pub max_streams: usize,
    pub max_width: u32,
    pub max_height: u32,
    /// Maximum video frames requested by one decode or cursor page.
    pub max_video_frames: u32,
    /// Maximum audio blocks requested by one decode or cursor page.
    pub max_audio_blocks: u32,
    /// Maximum selected per-channel audio sample frames in one decode or cursor page.
    pub max_audio_samples: usize,
    /// Maximum decoded output bytes in one decode or cursor page.
    ///
    /// A decode requesting both media types applies this to their combined output.
    pub max_total_decoded_bytes: usize,
    pub max_version_stdout_bytes: usize,
    pub max_probe_stdout_bytes: usize,
    pub max_frame_metadata_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
    pub discovery_timeout: Duration,
    pub probe_timeout: Duration,
    pub frame_metadata_timeout: Duration,
    pub decode_timeout: Duration,
    pub kill_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_input_bytes: 512 * 1024 * 1024,
            max_streams: 32,
            max_width: 4_096,
            max_height: 2_304,
            max_video_frames: 120,
            max_audio_blocks: 256,
            max_audio_samples: 1_048_576,
            max_total_decoded_bytes: 256 * 1024 * 1024,
            max_version_stdout_bytes: 64 * 1024,
            max_probe_stdout_bytes: 4 * 1024 * 1024,
            max_frame_metadata_stdout_bytes: 8 * 1024 * 1024,
            max_stderr_bytes: 64 * 1024,
            discovery_timeout: Duration::from_secs(2),
            probe_timeout: Duration::from_secs(10),
            frame_metadata_timeout: Duration::from_secs(10),
            decode_timeout: Duration::from_secs(30),
            kill_timeout: Duration::from_secs(2),
        }
    }
}

/// Runtime configuration for the local-file adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub ffmpeg: Executable,
    pub ffprobe: Executable,
    /// Canonical inputs must be descendants of this directory when set.
    pub allowed_root: Option<PathBuf>,
    pub limits: Limits,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ffmpeg: Executable::SearchPath,
            ffprobe: Executable::SearchPath,
            allowed_root: None,
            limits: Limits::default(),
        }
    }
}

pub(crate) fn validate_limits(limits: Limits) -> bool {
    limits.max_input_bytes > 0
        && limits.max_streams > 0
        && limits.max_width > 0
        && limits.max_height > 0
        && limits.max_width <= CpuVideoPayload::MAX_WIDTH
        && limits.max_height <= CpuVideoPayload::MAX_HEIGHT
        && limits.max_video_frames > 0
        && limits.max_audio_blocks > 0
        && limits.max_audio_samples > 0
        && limits.max_total_decoded_bytes > 0
        && limits.max_version_stdout_bytes > 0
        && limits.max_probe_stdout_bytes > 0
        && limits.max_frame_metadata_stdout_bytes > 0
        && limits.max_stderr_bytes > 0
        && !limits.discovery_timeout.is_zero()
        && !limits.probe_timeout.is_zero()
        && !limits.frame_metadata_timeout.is_zero()
        && !limits.decode_timeout.is_zero()
        && !limits.kill_timeout.is_zero()
}
