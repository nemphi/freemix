#![forbid(unsafe_code)]

//! Bounded, process-isolated `FFmpeg` decoding for canonical local files.
//!
//! This crate intentionally does not implement `CodecProvider` or `Demuxer`:
//! ffprobe's bounded look-ahead and `FFmpeg`'s raw process outputs make this a
//! finite local-file sequence adapter, not a packet-level codec or demuxer.
//! Isolation covers the spawned direct child. A descendant that deliberately
//! retains inherited pipes cannot extend an operation beyond its timeout, but
//! the adapter does not create or terminate an operating-system process group.
//! Source-change checks compare file identity, size, and timestamps without
//! reading file contents. They cannot detect in-place changes that preserve all
//! observed metadata, or transient changes restored between checks.
//!
//! Cursor decode limits apply independently to each requested page, not to the
//! cursor's lifetime output. Audio sample limits count selected per-channel
//! sample frames. Audio cursors still scan and sum a bounded metadata prefix
//! from the start of the stream for each page; PCM decode seeks to a validated
//! timestamp anchor and trims at most one second of correction samples.

mod audio_seek;
mod config;
mod decode;
mod error;
mod probe;
mod process;
pub mod record;

use std::ffi::{OsStr, OsString};
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;

pub use config::{Config, Executable, Limits};
pub use decode::{
    AudioCursorPosition, DecodeRequest, DecodedAudioWindow, DecodedSequence, DecodedVideoWindow,
    LocalAudioDecoder, LocalVideoDecoder, SequenceRequest,
};
pub use error::{Error, LimitKind, Tool, UnavailableReason, Unsupported};
pub use probe::{FormatInfo, Probe, StreamInfo, StreamKind, StreamSelector};
use process::{RunOutput, RunRequest};

/// Bounded discovery result for one executable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolAvailability {
    Available { version: String },
    Unavailable { reason: UnavailableReason },
}

/// Independently reported `FFmpeg` and ffprobe runtime capabilities.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Capabilities {
    pub ffmpeg: ToolAvailability,
    pub ffprobe: ToolAvailability,
}

/// Configured local-file process adapter.
#[derive(Clone, Debug)]
pub struct Adapter {
    ffmpeg: OsString,
    ffprobe: OsString,
    allowed_root: Option<PathBuf>,
    limits: Limits,
}

#[derive(Clone)]
pub(crate) struct Source {
    path: PathBuf,
    redaction: String,
    fingerprint: SourceFingerprint,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SourceFingerprint {
    size: u64,
    modified: SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    change_time_seconds: i64,
    #[cfg(unix)]
    change_time_nanoseconds: i64,
    #[cfg(windows)]
    volume_serial_number: Option<u32>,
    #[cfg(windows)]
    file_index: Option<u64>,
}

impl Adapter {
    /// Validates limits and canonicalizes the optional allowed root.
    ///
    /// Search-path executables are resolved when spawned. Explicit executables
    /// are canonicalized and regular-file checked here.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidConfig`] for invalid limits or executable paths,
    /// or a typed input error when the allowed root cannot be canonicalized.
    pub fn new(config: Config) -> Result<Self, Error> {
        if !config::validate_limits(config.limits) {
            return Err(Error::InvalidConfig);
        }
        let allowed_root = config
            .allowed_root
            .map(|root| {
                let root = fs::canonicalize(root).map_err(|error| map_input_io(&error))?;
                if root.is_dir() {
                    Ok(root)
                } else {
                    Err(Error::InvalidConfig)
                }
            })
            .transpose()?;
        Ok(Self {
            ffmpeg: executable(config.ffmpeg, "ffmpeg", Tool::Ffmpeg)?,
            ffprobe: executable(config.ffprobe, "ffprobe", Tool::Ffprobe)?,
            allowed_root,
            limits: config.limits,
        })
    }

    /// Discovers both tools independently using bounded `-version` commands.
    #[must_use]
    pub fn capabilities(&self) -> Capabilities {
        Capabilities {
            ffmpeg: self.discover_tool(Tool::Ffmpeg),
            ffprobe: self.discover_tool(Tool::Ffprobe),
        }
    }

    /// Probes one canonical, regular local file using bounded ffprobe JSON.
    ///
    /// # Errors
    ///
    /// Returns a typed path, source-change, limit, tool, process, or malformed
    /// metadata error.
    pub fn probe_local(&self, path: impl AsRef<Path>) -> Result<Probe, Error> {
        let source = self.open_source(path.as_ref())?;
        self.probe_source(&source)
    }

    pub(crate) fn probe_source(&self, source: &Source) -> Result<Probe, Error> {
        let output = self.run_source(
            source,
            Tool::Ffprobe,
            &probe_args(&source.path),
            self.limits.probe_timeout,
            self.limits.max_probe_stdout_bytes,
        )?;
        probe::parse_probe(&output.stdout, self.limits.max_streams)
    }

    fn discover_tool(&self, tool: Tool) -> ToolAvailability {
        let args = [OsString::from("-version")];
        let result = process::run(RunRequest {
            executable: self.executable(tool),
            tool,
            args: &args,
            env: &[],
            timeout: self.limits.discovery_timeout,
            kill_timeout: self.limits.kill_timeout,
            max_stdout: self.limits.max_version_stdout_bytes,
            max_stderr: self.limits.max_stderr_bytes,
            redactions: &[],
        });
        match result {
            Ok(output) => parse_version(tool, &output.stdout).map_or(
                ToolAvailability::Unavailable {
                    reason: UnavailableReason::MalformedVersion,
                },
                |version| ToolAvailability::Available { version },
            ),
            Err(error) => ToolAvailability::Unavailable {
                reason: availability_reason(&error),
            },
        }
    }

    pub(crate) fn open_source(&self, path: &Path) -> Result<Source, Error> {
        let path = fs::canonicalize(path).map_err(|error| map_input_io(&error))?;
        if self
            .allowed_root
            .as_ref()
            .is_some_and(|root| !path.starts_with(root))
        {
            return Err(Error::InputOutsideAllowedRoot);
        }
        let metadata = fs::metadata(&path).map_err(|error| map_input_io(&error))?;
        if !metadata.is_file() {
            return Err(Error::InputNotRegularFile);
        }
        let fingerprint = fingerprint(&path, self.limits.max_input_bytes)?;
        Ok(Source {
            redaction: path.to_string_lossy().into_owned(),
            path,
            fingerprint,
        })
    }

    pub(crate) fn run_source(
        &self,
        source: &Source,
        tool: Tool,
        args: &[OsString],
        timeout: std::time::Duration,
        max_stdout: usize,
    ) -> Result<RunOutput, Error> {
        self.check_source(source)?;
        let redactions = [source.redaction.as_str()];
        let result = process::run(RunRequest {
            executable: self.executable(tool),
            tool,
            args,
            env: &[],
            timeout,
            kill_timeout: self.limits.kill_timeout,
            max_stdout,
            max_stderr: self.limits.max_stderr_bytes,
            redactions: &redactions,
        });
        self.check_source(source)?;
        result
    }

    fn check_source(&self, source: &Source) -> Result<(), Error> {
        let fingerprint = match fingerprint(&source.path, self.limits.max_input_bytes) {
            Ok(fingerprint) => fingerprint,
            Err(error @ Error::LimitExceeded { .. }) => return Err(error),
            Err(_) => return Err(Error::SourceChanged),
        };
        if fingerprint == source.fingerprint {
            Ok(())
        } else {
            Err(Error::SourceChanged)
        }
    }

    pub(crate) const fn limits(&self) -> Limits {
        self.limits
    }

    pub(crate) fn executable(&self, tool: Tool) -> &OsStr {
        match tool {
            Tool::Ffmpeg => &self.ffmpeg,
            Tool::Ffprobe => &self.ffprobe,
        }
    }
}

fn executable(config: Executable, default: &str, tool: Tool) -> Result<OsString, Error> {
    match config {
        Executable::SearchPath => Ok(OsString::from(default)),
        Executable::Explicit(path) if path.is_absolute() => {
            let path = fs::canonicalize(path).map_err(|error| explicit_tool_error(tool, &error))?;
            let metadata =
                fs::metadata(&path).map_err(|error| explicit_tool_error(tool, &error))?;
            if !metadata.is_file() {
                return Err(Error::ToolUnavailable {
                    tool,
                    reason: UnavailableReason::InvalidExecutable,
                });
            }
            Ok(path.into_os_string())
        }
        Executable::Explicit(_) => Err(Error::InvalidConfig),
    }
}

fn probe_args(path: &Path) -> Vec<OsString> {
    [
        OsString::from("-v"),
        OsString::from("error"),
        OsString::from("-protocol_whitelist"),
        OsString::from("file"),
        OsString::from("-show_format"),
        OsString::from("-show_streams"),
        OsString::from("-of"),
        OsString::from("json"),
        path.as_os_str().to_owned(),
    ]
    .into()
}

fn fingerprint(path: &Path, maximum: u64) -> Result<SourceFingerprint, Error> {
    let metadata = fs::metadata(path).map_err(|error| map_input_io(&error))?;
    if !metadata.is_file() {
        return Err(Error::InputNotRegularFile);
    }
    if metadata.len() > maximum {
        return Err(Error::LimitExceeded {
            kind: LimitKind::InputBytes,
            actual: metadata.len(),
            maximum,
        });
    }
    source_fingerprint(&metadata)
}

fn source_fingerprint(metadata: &Metadata) -> Result<SourceFingerprint, Error> {
    Ok(SourceFingerprint {
        size: metadata.len(),
        modified: metadata.modified().map_err(|error| map_input_io(&error))?,
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
        #[cfg(unix)]
        change_time_seconds: metadata.ctime(),
        #[cfg(unix)]
        change_time_nanoseconds: metadata.ctime_nsec(),
        #[cfg(windows)]
        volume_serial_number: metadata.volume_serial_number(),
        #[cfg(windows)]
        file_index: metadata.file_index(),
    })
}

fn parse_version(tool: Tool, bytes: &[u8]) -> Option<String> {
    let line = std::str::from_utf8(bytes).ok()?.lines().next()?;
    let prefix = format!("{tool} version ");
    line.strip_prefix(&prefix)?
        .split_whitespace()
        .next()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn availability_reason(error: &Error) -> UnavailableReason {
    match error {
        Error::ToolUnavailable { reason, .. } => *reason,
        Error::ProcessTimedOut { .. } => UnavailableReason::TimedOut,
        Error::ProcessOutputOverflow { .. } => UnavailableReason::OutputLimit,
        _ => UnavailableReason::Failed,
    }
}

fn map_input_io(error: &io::Error) -> Error {
    match error.kind() {
        io::ErrorKind::NotFound => Error::InputNotFound,
        _ => Error::InputAccessDenied,
    }
}

fn explicit_tool_error(tool: Tool, error: &io::Error) -> Error {
    let reason = match error.kind() {
        io::ErrorKind::NotFound => UnavailableReason::Missing,
        io::ErrorKind::PermissionDenied => UnavailableReason::PermissionDenied,
        _ => UnavailableReason::InvalidExecutable,
    };
    Error::ToolUnavailable { tool, reason }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn explicit_executables_must_be_absolute() {
        let config = Config {
            ffmpeg: Executable::Explicit(PathBuf::from("ffmpeg")),
            ..Config::default()
        };
        assert_eq!(Adapter::new(config).unwrap_err(), Error::InvalidConfig);
    }

    #[test]
    fn canonical_path_enforces_root_and_is_one_input_argument() {
        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let suspicious = root.path().join("movie;touch-pwned.nut");
        fs::write(&suspicious, b"media").unwrap();
        let adapter = Adapter::new(Config {
            allowed_root: Some(root.path().to_owned()),
            ..Config::default()
        })
        .unwrap();
        let source = adapter.open_source(&suspicious).unwrap();
        let args = probe_args(&source.path);
        assert_eq!(args.last(), Some(&source.path.into_os_string()));

        let other = outside.path().join("other.nut");
        fs::write(&other, b"media").unwrap();
        assert_eq!(
            adapter.open_source(&other).err(),
            Some(Error::InputOutsideAllowedRoot)
        );
    }

    #[test]
    fn source_metadata_check_detects_same_size_file_replacement() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("source.nut");
        fs::write(&path, b"first").unwrap();
        let adapter = Adapter::new(Config::default()).unwrap();
        let source = adapter.open_source(&path).unwrap();
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"other").unwrap();
        assert_eq!(adapter.check_source(&source), Err(Error::SourceChanged));
    }

    #[test]
    fn version_parser_is_tool_specific() {
        assert_eq!(
            parse_version(Tool::Ffmpeg, b"ffmpeg version 8.1.2 Copyright\n"),
            Some("8.1.2".to_owned())
        );
        assert_eq!(
            parse_version(Tool::Ffprobe, b"ffmpeg version 8.1.2\n"),
            None
        );
    }

    #[test]
    fn explicit_executables_are_canonical_regular_files_with_path_free_errors() {
        let current = std::env::current_exe().unwrap();
        let noncanonical = current
            .parent()
            .unwrap()
            .join(".")
            .join(current.file_name().unwrap());
        let adapter = Adapter::new(Config {
            ffmpeg: Executable::Explicit(noncanonical),
            ffprobe: Executable::Explicit(current.clone()),
            ..Config::default()
        })
        .unwrap();
        assert_eq!(
            adapter.executable(Tool::Ffmpeg),
            fs::canonicalize(current).unwrap()
        );

        let directory = tempdir().unwrap();
        assert_eq!(
            Adapter::new(Config {
                ffmpeg: Executable::Explicit(directory.path().to_owned()),
                ..Config::default()
            })
            .unwrap_err(),
            Error::ToolUnavailable {
                tool: Tool::Ffmpeg,
                reason: UnavailableReason::InvalidExecutable
            }
        );

        let missing = directory.path().join("missing-tool");
        let error = Adapter::new(Config {
            ffmpeg: Executable::Explicit(missing.clone()),
            ..Config::default()
        })
        .unwrap_err();
        assert_eq!(
            error,
            Error::ToolUnavailable {
                tool: Tool::Ffmpeg,
                reason: UnavailableReason::Missing
            }
        );
        assert!(!error.to_string().contains(&missing.to_string_lossy()[..]));
    }
}
