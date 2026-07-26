use core::fmt;
use std::path::PathBuf;

use crate::{BackoffPolicy, BackoffPolicyError, PublicationRegistry};

pub const DEFAULT_MAX_PUBLICATIONS: usize = 32;
pub const DEFAULT_INITIAL_BACKOFF_MS: u64 = 250;
pub const DEFAULT_MAX_BACKOFF_MS: u64 = 10_000;
pub const DEFAULT_CAMERA_SMOKE_FRAMES: usize = 30;
pub const MAX_CAMERA_SMOKE_FRAMES: usize = 300;
pub const DEFAULT_CAMERA_SMOKE_TIMEOUT_MS: u64 = 10_000;
pub const MIN_CAMERA_SMOKE_TIMEOUT_MS: u64 = 1_000;
pub const MAX_CAMERA_SMOKE_TIMEOUT_MS: u64 = 60_000;
pub const DEFAULT_AUDIO_SMOKE_BLOCKS: usize = 100;
pub const MAX_AUDIO_SMOKE_BLOCKS: usize = 1_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    Help,
    Version,
    Cameras(CameraConfig),
    CameraSmoke(CameraSmokeConfig),
    AudioInputs(AudioConfig),
    AudioSmoke(AudioSmokeConfig),
    Serve(ServeConfig),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CameraConfig {
    pub request_permission: bool,
    pub helper: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CameraSmokeConfig {
    pub source_index: usize,
    pub format_index: usize,
    pub frames: usize,
    pub timeout_ms: u64,
    pub helper: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioConfig {
    pub request_permission: bool,
    pub helper: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioSmokeConfig {
    pub stable_key: String,
    pub sample_rate: u32,
    pub channels: u8,
    pub blocks: usize,
    pub timeout_ms: u64,
    pub helper: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServeConfig {
    pub session_id: String,
    pub endpoint: String,
    pub max_publications: usize,
    pub backoff: BackoffPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArgsError {
    MissingCommand,
    MissingOption(&'static str),
    MissingValue(&'static str),
    BlankValue(&'static str),
    DuplicateOption(String),
    UnknownOption(String),
    UnknownCommand(String),
    InvalidNumber {
        field: &'static str,
        value: String,
    },
    ValueOutOfRange {
        field: &'static str,
        value: u64,
        minimum: u64,
        maximum: u64,
    },
    PublicationLimit {
        value: usize,
        maximum: usize,
    },
    InvalidBackoff(BackoffPolicyError),
    UnexpectedArgument(String),
}

impl fmt::Display for ArgsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCommand => formatter.write_str("a command is required"),
            Self::MissingOption(option) => write!(formatter, "required option {option} is missing"),
            Self::MissingValue(field) => write!(formatter, "missing value for {field}"),
            Self::BlankValue(field) => write!(formatter, "{field} must not be blank"),
            Self::DuplicateOption(option) => {
                write!(formatter, "option `{option}` was provided twice")
            }
            Self::UnknownOption(option) => write!(formatter, "unknown option `{option}`"),
            Self::UnknownCommand(command) => write!(formatter, "unknown command `{command}`"),
            Self::InvalidNumber { field, value } => {
                write!(formatter, "invalid {field} value `{value}`")
            }
            Self::ValueOutOfRange {
                field,
                value,
                minimum,
                maximum,
            } => write!(
                formatter,
                "{field} value {value} must be between {minimum} and {maximum}"
            ),
            Self::PublicationLimit { value, maximum } => write!(
                formatter,
                "publication limit {value} must be between 1 and {maximum}"
            ),
            Self::InvalidBackoff(error) => error.fmt(formatter),
            Self::UnexpectedArgument(argument) => {
                write!(formatter, "unexpected argument `{argument}`")
            }
        }
    }
}

impl std::error::Error for ArgsError {}

/// Parses a capture-node command without reading process-global state.
///
/// # Errors
///
/// Returns a typed error for missing, duplicate, unknown, or invalid arguments.
pub fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Command, ArgsError> {
    let mut arguments = arguments.into_iter();
    let Some(command) = arguments.next() else {
        return Err(ArgsError::MissingCommand);
    };

    match command.as_str() {
        "help" | "--help" | "-h" => {
            reject_extra(&mut arguments)?;
            Ok(Command::Help)
        }
        "version" | "--version" | "-V" => {
            reject_extra(&mut arguments)?;
            Ok(Command::Version)
        }
        "cameras" => parse_cameras(arguments).map(Command::Cameras),
        "camera-smoke" => parse_camera_smoke(arguments).map(Command::CameraSmoke),
        "audio-inputs" => parse_audio_inputs(arguments).map(Command::AudioInputs),
        "audio-smoke" => parse_audio_smoke(arguments).map(Command::AudioSmoke),
        "serve" => parse_serve(arguments).map(Command::Serve),
        _ => Err(ArgsError::UnknownCommand(command)),
    }
}

fn parse_audio_smoke(
    mut arguments: impl Iterator<Item = String>,
) -> Result<AudioSmokeConfig, ArgsError> {
    let mut stable_key = None;
    let mut sample_rate = None;
    let mut channels = None;
    let mut blocks = None;
    let mut timeout_ms = None;
    let mut helper = None;
    while let Some(option) = arguments.next() {
        match option.as_str() {
            "--stable-key" => {
                let value = required_nonblank(&mut arguments, "audio stable key")?;
                set_once(&mut stable_key, option, value)?;
            }
            "--sample-rate" => {
                let value = required_option_value(&mut arguments, "audio sample rate")?;
                set_once(
                    &mut sample_rate,
                    option,
                    number(&value, "audio sample rate")?,
                )?;
            }
            "--channels" => {
                let value = required_option_value(&mut arguments, "audio channel count")?;
                let parsed: usize = number(&value, "audio channel count")?;
                let parsed = u8::try_from(parsed).map_err(|_| ArgsError::InvalidNumber {
                    field: "audio channel count",
                    value,
                })?;
                set_once(&mut channels, option, parsed)?;
            }
            "--blocks" => {
                let value = required_option_value(&mut arguments, "audio block count")?;
                set_once(
                    &mut blocks,
                    option,
                    bounded_number(&value, "audio block count", 1, MAX_AUDIO_SMOKE_BLOCKS)?,
                )?;
            }
            "--timeout-ms" => {
                let value = required_option_value(&mut arguments, "audio timeout")?;
                set_once(
                    &mut timeout_ms,
                    option,
                    bounded_number(
                        &value,
                        "audio timeout",
                        MIN_CAMERA_SMOKE_TIMEOUT_MS,
                        MAX_CAMERA_SMOKE_TIMEOUT_MS,
                    )?,
                )?;
            }
            "--helper" => set_once(
                &mut helper,
                option,
                PathBuf::from(required_option_value(&mut arguments, "audio helper path")?),
            )?,
            _ if option.starts_with('-') => return Err(ArgsError::UnknownOption(option)),
            _ => return Err(ArgsError::UnexpectedArgument(option)),
        }
    }
    Ok(AudioSmokeConfig {
        stable_key: stable_key.ok_or(ArgsError::MissingOption("--stable-key"))?,
        sample_rate: sample_rate.ok_or(ArgsError::MissingOption("--sample-rate"))?,
        channels: channels.ok_or(ArgsError::MissingOption("--channels"))?,
        blocks: blocks.unwrap_or(DEFAULT_AUDIO_SMOKE_BLOCKS),
        timeout_ms: timeout_ms.unwrap_or(DEFAULT_CAMERA_SMOKE_TIMEOUT_MS),
        helper,
    })
}

fn parse_audio_inputs(
    mut arguments: impl Iterator<Item = String>,
) -> Result<AudioConfig, ArgsError> {
    let mut request_permission = false;
    let mut helper = None;
    while let Some(option) = arguments.next() {
        match option.as_str() {
            "--request-permission" if request_permission => {
                return Err(ArgsError::DuplicateOption(option));
            }
            "--request-permission" => request_permission = true,
            "--helper" => set_once(
                &mut helper,
                option,
                PathBuf::from(required_option_value(&mut arguments, "audio helper path")?),
            )?,
            _ if option.starts_with('-') => return Err(ArgsError::UnknownOption(option)),
            _ => return Err(ArgsError::UnexpectedArgument(option)),
        }
    }
    Ok(AudioConfig {
        request_permission,
        helper,
    })
}

fn parse_camera_smoke(
    mut arguments: impl Iterator<Item = String>,
) -> Result<CameraSmokeConfig, ArgsError> {
    let mut source_index = None;
    let mut format_index = None;
    let mut frames = None;
    let mut timeout_ms = None;
    let mut helper = None;
    while let Some(option) = arguments.next() {
        match option.as_str() {
            "--source-index" => {
                let value = required_option_value(&mut arguments, "camera source index")?;
                set_once(
                    &mut source_index,
                    option,
                    number(&value, "camera source index")?,
                )?;
            }
            "--format-index" => {
                let value = required_option_value(&mut arguments, "camera format index")?;
                set_once(
                    &mut format_index,
                    option,
                    number(&value, "camera format index")?,
                )?;
            }
            "--frames" => {
                let value = required_option_value(&mut arguments, "camera frame count")?;
                set_once(
                    &mut frames,
                    option,
                    bounded_number(&value, "camera frame count", 1, MAX_CAMERA_SMOKE_FRAMES)?,
                )?;
            }
            "--timeout-ms" => {
                let value = required_option_value(&mut arguments, "camera timeout")?;
                set_once(
                    &mut timeout_ms,
                    option,
                    bounded_number(
                        &value,
                        "camera timeout",
                        MIN_CAMERA_SMOKE_TIMEOUT_MS,
                        MAX_CAMERA_SMOKE_TIMEOUT_MS,
                    )?,
                )?;
            }
            "--helper" => set_once(
                &mut helper,
                option,
                PathBuf::from(required_option_value(&mut arguments, "camera helper path")?),
            )?,
            _ if option.starts_with('-') => return Err(ArgsError::UnknownOption(option)),
            _ => return Err(ArgsError::UnexpectedArgument(option)),
        }
    }
    Ok(CameraSmokeConfig {
        source_index: source_index.ok_or(ArgsError::MissingOption("--source-index"))?,
        format_index: format_index.unwrap_or(0),
        frames: frames.unwrap_or(DEFAULT_CAMERA_SMOKE_FRAMES),
        timeout_ms: timeout_ms.unwrap_or(DEFAULT_CAMERA_SMOKE_TIMEOUT_MS),
        helper,
    })
}

fn parse_cameras(mut arguments: impl Iterator<Item = String>) -> Result<CameraConfig, ArgsError> {
    let mut request_permission = false;
    let mut helper = None;
    while let Some(option) = arguments.next() {
        match option.as_str() {
            "--request-permission" if request_permission => {
                return Err(ArgsError::DuplicateOption(option));
            }
            "--request-permission" => request_permission = true,
            "--helper" => set_once(
                &mut helper,
                option,
                PathBuf::from(required_option_value(&mut arguments, "camera helper path")?),
            )?,
            _ if option.starts_with('-') => return Err(ArgsError::UnknownOption(option)),
            _ => return Err(ArgsError::UnexpectedArgument(option)),
        }
    }
    Ok(CameraConfig {
        request_permission,
        helper,
    })
}

fn parse_serve(mut arguments: impl Iterator<Item = String>) -> Result<ServeConfig, ArgsError> {
    let mut session_id = None;
    let mut endpoint = None;
    let mut max_publications = None;
    let mut initial_backoff_ms = None;
    let mut max_backoff_ms = None;

    while let Some(option) = arguments.next() {
        match option.as_str() {
            "--session-id" => set_once(
                &mut session_id,
                option,
                required_nonblank(&mut arguments, "session id")?,
            )?,
            "--endpoint" => set_once(
                &mut endpoint,
                option,
                required_nonblank(&mut arguments, "endpoint")?,
            )?,
            "--max-publications" => {
                let value = required_nonblank(&mut arguments, "maximum publications")?;
                set_once(
                    &mut max_publications,
                    option,
                    number(&value, "maximum publications")?,
                )?;
            }
            "--initial-backoff-ms" => {
                let value = required_nonblank(&mut arguments, "initial backoff")?;
                set_once(
                    &mut initial_backoff_ms,
                    option,
                    number(&value, "initial backoff")?,
                )?;
            }
            "--max-backoff-ms" => {
                let value = required_nonblank(&mut arguments, "maximum backoff")?;
                set_once(
                    &mut max_backoff_ms,
                    option,
                    number(&value, "maximum backoff")?,
                )?;
            }
            _ if option.starts_with('-') => return Err(ArgsError::UnknownOption(option)),
            _ => return Err(ArgsError::UnexpectedArgument(option)),
        }
    }

    let max_publications = max_publications.unwrap_or(DEFAULT_MAX_PUBLICATIONS);
    if !(1..=PublicationRegistry::MAX_PUBLICATIONS).contains(&max_publications) {
        return Err(ArgsError::PublicationLimit {
            value: max_publications,
            maximum: PublicationRegistry::MAX_PUBLICATIONS,
        });
    }
    let backoff = BackoffPolicy::new(
        initial_backoff_ms.unwrap_or(DEFAULT_INITIAL_BACKOFF_MS),
        max_backoff_ms.unwrap_or(DEFAULT_MAX_BACKOFF_MS),
    )
    .map_err(ArgsError::InvalidBackoff)?;

    Ok(ServeConfig {
        session_id: session_id.ok_or(ArgsError::MissingOption("--session-id"))?,
        endpoint: endpoint.ok_or(ArgsError::MissingOption("--endpoint"))?,
        max_publications,
        backoff,
    })
}

fn set_once<T>(slot: &mut Option<T>, option: String, value: T) -> Result<(), ArgsError> {
    if slot.replace(value).is_some() {
        Err(ArgsError::DuplicateOption(option))
    } else {
        Ok(())
    }
}

fn required_nonblank(
    arguments: &mut impl Iterator<Item = String>,
    field: &'static str,
) -> Result<String, ArgsError> {
    let value = arguments.next().ok_or(ArgsError::MissingValue(field))?;
    if value.trim().is_empty() {
        Err(ArgsError::BlankValue(field))
    } else {
        Ok(value)
    }
}

fn required_option_value(
    arguments: &mut impl Iterator<Item = String>,
    field: &'static str,
) -> Result<String, ArgsError> {
    let value = required_nonblank(arguments, field)?;
    if value.starts_with('-') {
        Err(ArgsError::MissingValue(field))
    } else {
        Ok(value)
    }
}

fn number<T>(value: &str, field: &'static str) -> Result<T, ArgsError>
where
    T: core::str::FromStr,
{
    value.parse().map_err(|_| ArgsError::InvalidNumber {
        field,
        value: value.to_owned(),
    })
}

fn bounded_number<T>(
    value: &str,
    field: &'static str,
    minimum: T,
    maximum: T,
) -> Result<T, ArgsError>
where
    T: core::str::FromStr + Copy + Ord + TryInto<u64>,
{
    let parsed = number(value, field)?;
    if (minimum..=maximum).contains(&parsed) {
        Ok(parsed)
    } else {
        Err(ArgsError::ValueOutOfRange {
            field,
            value: parsed.try_into().ok().unwrap_or(u64::MAX),
            minimum: minimum.try_into().ok().unwrap_or(u64::MAX),
            maximum: maximum.try_into().ok().unwrap_or(u64::MAX),
        })
    }
}

fn reject_extra(arguments: &mut impl Iterator<Item = String>) -> Result<(), ArgsError> {
    arguments.next().map_or(Ok(()), |argument| {
        Err(ArgsError::UnexpectedArgument(argument))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn parses_help_and_version_forms() {
        assert_eq!(parse(args(&["--help"])), Ok(Command::Help));
        assert_eq!(parse(args(&["version"])), Ok(Command::Version));
        assert!(matches!(
            parse(args(&["help", "extra"])),
            Err(ArgsError::UnexpectedArgument(_))
        ));
    }

    #[test]
    fn parses_camera_diagnostics_and_rejects_duplicate_options() {
        assert_eq!(
            parse(args(&["cameras"])),
            Ok(Command::Cameras(CameraConfig {
                request_permission: false,
                helper: None,
            }))
        );
        assert_eq!(
            parse(args(&[
                "cameras",
                "--request-permission",
                "--helper",
                "/Applications/FreeMix.app/Contents/Helpers/camera"
            ])),
            Ok(Command::Cameras(CameraConfig {
                request_permission: true,
                helper: Some(PathBuf::from(
                    "/Applications/FreeMix.app/Contents/Helpers/camera"
                )),
            }))
        );
        assert!(matches!(
            parse(args(&[
                "cameras",
                "--request-permission",
                "--request-permission"
            ])),
            Err(ArgsError::DuplicateOption(_))
        ));
        assert!(matches!(
            parse(args(&["cameras", "--helper", "a", "--helper", "b"])),
            Err(ArgsError::DuplicateOption(_))
        ));
        assert_eq!(
            parse(args(&["cameras", "--helper", "--request-permission"])),
            Err(ArgsError::MissingValue("camera helper path"))
        );
        assert_eq!(
            parse(args(&["cameras", "--helper", "-h"])),
            Err(ArgsError::MissingValue("camera helper path"))
        );
    }

    #[test]
    fn parses_bounded_camera_smoke_configuration() {
        assert_eq!(
            parse(args(&["camera-smoke", "--source-index", "2"])),
            Ok(Command::CameraSmoke(CameraSmokeConfig {
                source_index: 2,
                format_index: 0,
                frames: DEFAULT_CAMERA_SMOKE_FRAMES,
                timeout_ms: DEFAULT_CAMERA_SMOKE_TIMEOUT_MS,
                helper: None,
            }))
        );
        assert_eq!(
            parse(args(&[
                "camera-smoke",
                "--source-index",
                "1",
                "--format-index",
                "3",
                "--frames",
                "2",
                "--timeout-ms",
                "1500",
                "--helper",
                "/tmp/camera-helper"
            ])),
            Ok(Command::CameraSmoke(CameraSmokeConfig {
                source_index: 1,
                format_index: 3,
                frames: 2,
                timeout_ms: 1_500,
                helper: Some(PathBuf::from("/tmp/camera-helper")),
            }))
        );
        assert_eq!(
            parse(args(&["camera-smoke"])),
            Err(ArgsError::MissingOption("--source-index"))
        );
        for frames in ["0", "301"] {
            assert!(matches!(
                parse(args(&[
                    "camera-smoke",
                    "--source-index",
                    "0",
                    "--frames",
                    frames
                ])),
                Err(ArgsError::ValueOutOfRange { .. })
            ));
        }
    }

    #[test]
    fn parses_serve_config_and_defaults() {
        let Command::Serve(config) = parse(args(&[
            "serve",
            "--endpoint",
            "local.sock",
            "--session-id",
            "user-7",
        ]))
        .expect("serve arguments should parse") else {
            panic!("expected serve command");
        };
        assert_eq!(config.session_id, "user-7");
        assert_eq!(config.endpoint, "local.sock");
        assert_eq!(config.max_publications, DEFAULT_MAX_PUBLICATIONS);
        assert_eq!(config.backoff.initial_ms(), DEFAULT_INITIAL_BACKOFF_MS);
        assert_eq!(config.backoff.maximum_ms(), DEFAULT_MAX_BACKOFF_MS);
    }

    #[test]
    fn parses_serve_overrides_and_rejects_invalid_values() {
        let Command::Serve(config) = parse(args(&[
            "serve",
            "--session-id",
            "user-7",
            "--endpoint",
            "pipe",
            "--max-publications",
            "8",
            "--initial-backoff-ms",
            "50",
            "--max-backoff-ms",
            "400",
        ]))
        .expect("serve overrides should parse") else {
            panic!("expected serve command");
        };
        assert_eq!(config.max_publications, 8);
        assert_eq!(config.backoff, BackoffPolicy::new(50, 400).unwrap());

        assert!(matches!(
            parse(args(&[
                "serve",
                "--session-id",
                "u",
                "--endpoint",
                "e",
                "--max-publications",
                "0"
            ])),
            Err(ArgsError::PublicationLimit { .. })
        ));
        assert_eq!(
            parse(args(&["serve", "--endpoint", "e"])),
            Err(ArgsError::MissingOption("--session-id"))
        );
        assert!(matches!(
            parse(args(&[
                "serve",
                "--session-id",
                "u",
                "--endpoint",
                "e",
                "--endpoint",
                "other"
            ])),
            Err(ArgsError::DuplicateOption(_))
        ));
    }
}
