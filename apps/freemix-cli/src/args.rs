use std::{net::SocketAddr, path::PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManualTransitionKind {
    Fade,
    Wipe,
    AlphaFade,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TBarAction {
    Start(ManualTransitionKind),
    SetPosition(u16),
    Commit,
    Cancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FadeToBlackTarget {
    Live,
    Black,
}

impl FadeToBlackTarget {
    #[must_use]
    pub const fn active(self) -> bool {
        matches!(self, Self::Black)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    New {
        path: PathBuf,
        name: String,
    },
    Status {
        path: PathBuf,
    },
    Preview {
        path: PathBuf,
        input: u128,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    Cut {
        path: PathBuf,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    Fade {
        path: PathBuf,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    AlphaFade {
        path: PathBuf,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    Slide {
        path: PathBuf,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    Wipe {
        path: PathBuf,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    TBar {
        path: PathBuf,
        action: TBarAction,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    FadeToBlack {
        path: PathBuf,
        target: FadeToBlackTarget,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteStatus {
        address: SocketAddr,
    },
    RemotePreview {
        address: SocketAddr,
        input: u128,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteCut {
        address: SocketAddr,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteFade {
        address: SocketAddr,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteAlphaFade {
        address: SocketAddr,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteSlide {
        address: SocketAddr,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteWipe {
        address: SocketAddr,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteTBar {
        address: SocketAddr,
        action: TBarAction,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    RemoteFadeToBlack {
        address: SocketAddr,
        target: FadeToBlackTarget,
        frames: u32,
        key: Option<String>,
        expected_revision: Option<u64>,
    },
    Render {
        path: PathBuf,
        output: PathBuf,
        width: u32,
        height: u32,
    },
    Demo {
        path: PathBuf,
        output: Option<PathBuf>,
    },
    Help,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArgsError {
    MissingCommand,
    MissingValue(&'static str),
    BlankValue(&'static str),
    InvalidNumber {
        field: &'static str,
        value: String,
    },
    InvalidChoice {
        field: &'static str,
        value: String,
    },
    OutOfRange {
        field: &'static str,
        minimum: u16,
        maximum: u16,
        value: u16,
    },
    UnexpectedArgument(String),
    UnknownOption(String),
    UnknownCommand(String),
}

impl core::fmt::Display for ArgsError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MissingCommand => formatter.write_str("a command is required"),
            Self::MissingValue(field) => write!(formatter, "missing value for {field}"),
            Self::BlankValue(field) => write!(formatter, "{field} must not be blank"),
            Self::InvalidNumber { field, value } | Self::InvalidChoice { field, value } => {
                write!(formatter, "invalid {field} value `{value}`")
            }
            Self::OutOfRange {
                field,
                minimum,
                maximum,
                value,
            } => write!(
                formatter,
                "{field} must be in {minimum}..={maximum}, got {value}"
            ),
            Self::UnexpectedArgument(argument) => {
                write!(formatter, "unexpected argument `{argument}`")
            }
            Self::UnknownOption(option) => write!(formatter, "unknown option `{option}`"),
            Self::UnknownCommand(command) => write!(formatter, "unknown command `{command}`"),
        }
    }
}

impl std::error::Error for ArgsError {}

pub fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Command, ArgsError> {
    let mut arguments = arguments.into_iter();
    let Some(command) = arguments.next() else {
        return Err(ArgsError::MissingCommand);
    };
    match command.as_str() {
        "new" => parse_new(arguments),
        "status" => {
            let path = required_path(&mut arguments, "project path")?;
            reject_extra(&mut arguments)?;
            Ok(Command::Status { path })
        }
        "preview" => {
            let path = required_path(&mut arguments, "project path")?;
            let input = number(&required(&mut arguments, "input")?, "input")?;
            let (key, expected_revision) = command_options(arguments)?;
            Ok(Command::Preview {
                path,
                input,
                key,
                expected_revision,
            })
        }
        "cut" => {
            let path = required_path(&mut arguments, "project path")?;
            let (key, expected_revision) = command_options(arguments)?;
            Ok(Command::Cut {
                path,
                key,
                expected_revision,
            })
        }
        "fade" | "alpha-fade" | "slide" | "wipe" => {
            parse_local_timed_transition(&command, arguments)
        }
        "tbar-start" | "tbar-position" | "tbar-commit" | "tbar-cancel" => {
            parse_local_t_bar(&command, arguments)
        }
        "ftb" => parse_local_fade_to_black(arguments),
        "remote-status" => parse_remote_status(arguments),
        "remote-preview" => parse_remote_preview(arguments),
        "remote-cut" => parse_remote_cut(arguments),
        "remote-fade" | "remote-alpha-fade" | "remote-slide" | "remote-wipe" => {
            parse_remote_timed_transition(&command, arguments)
        }
        "remote-tbar-start"
        | "remote-tbar-position"
        | "remote-tbar-commit"
        | "remote-tbar-cancel" => parse_remote_t_bar(&command, arguments),
        "remote-ftb" => parse_remote_fade_to_black(arguments),
        "render" => {
            let path = required_path(&mut arguments, "project path")?;
            let output = required_path(&mut arguments, "output path")?;
            let mut width = 640;
            let mut height = 360;
            while let Some(option) = arguments.next() {
                match option.as_str() {
                    "--width" => {
                        width = number(&required(&mut arguments, "width")?, "width")?;
                    }
                    "--height" => {
                        height = number(&required(&mut arguments, "height")?, "height")?;
                    }
                    _ => return Err(ArgsError::UnknownOption(option)),
                }
            }
            Ok(Command::Render {
                path,
                output,
                width,
                height,
            })
        }
        "demo" => {
            let path = required_path(&mut arguments, "project path")?;
            let output = arguments.next().map(PathBuf::from);
            reject_extra(&mut arguments)?;
            Ok(Command::Demo { path, output })
        }
        "help" | "--help" | "-h" => Ok(Command::Help),
        _ => Err(ArgsError::UnknownCommand(command)),
    }
}

fn parse_new(mut arguments: impl Iterator<Item = String>) -> Result<Command, ArgsError> {
    let path = required_path(&mut arguments, "project path")?;
    let mut name = "FreeMix Show".to_owned();
    while let Some(option) = arguments.next() {
        match option.as_str() {
            "--name" => name = required(&mut arguments, "name")?,
            _ => return Err(ArgsError::UnknownOption(option)),
        }
    }
    Ok(Command::New { path, name })
}

fn parse_local_timed_transition(
    command: &str,
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, ArgsError> {
    let path = required_path(&mut arguments, "project path")?;
    let frames = number(&required(&mut arguments, "frames")?, "frames")?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(match command {
        "fade" => Command::Fade {
            path,
            frames,
            key,
            expected_revision,
        },
        "alpha-fade" => Command::AlphaFade {
            path,
            frames,
            key,
            expected_revision,
        },
        "slide" => Command::Slide {
            path,
            frames,
            key,
            expected_revision,
        },
        "wipe" => Command::Wipe {
            path,
            frames,
            key,
            expected_revision,
        },
        _ => unreachable!("caller only dispatches timed local transitions"),
    })
}

fn parse_local_t_bar(
    command: &str,
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, ArgsError> {
    let path = required_path(&mut arguments, "project path")?;
    let action = parse_t_bar_action(command, &mut arguments)?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(Command::TBar {
        path,
        action,
        key,
        expected_revision,
    })
}

fn parse_remote_t_bar(
    command: &str,
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, ArgsError> {
    let address = socket_address(&required(&mut arguments, "address")?)?;
    let action = parse_t_bar_action(command, &mut arguments)?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(Command::RemoteTBar {
        address,
        action,
        key,
        expected_revision,
    })
}

fn parse_local_fade_to_black(
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, ArgsError> {
    let path = required_path(&mut arguments, "project path")?;
    let target = fade_to_black_target(&required(&mut arguments, "FTB target")?)?;
    let frames = number(&required(&mut arguments, "frames")?, "frames")?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(Command::FadeToBlack {
        path,
        target,
        frames,
        key,
        expected_revision,
    })
}

fn parse_remote_fade_to_black(
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, ArgsError> {
    let address = socket_address(&required(&mut arguments, "address")?)?;
    let target = fade_to_black_target(&required(&mut arguments, "FTB target")?)?;
    let frames = number(&required(&mut arguments, "frames")?, "frames")?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(Command::RemoteFadeToBlack {
        address,
        target,
        frames,
        key,
        expected_revision,
    })
}

fn parse_t_bar_action(
    command: &str,
    arguments: &mut impl Iterator<Item = String>,
) -> Result<TBarAction, ArgsError> {
    if command.ends_with("-start") {
        let kind = manual_kind(&required(arguments, "transition kind")?)?;
        Ok(TBarAction::Start(kind))
    } else if command.ends_with("-position") {
        let position = basis_points(&required(arguments, "basis points")?)?;
        Ok(TBarAction::SetPosition(position))
    } else if command.ends_with("-commit") {
        Ok(TBarAction::Commit)
    } else {
        Ok(TBarAction::Cancel)
    }
}

fn parse_remote_status(mut arguments: impl Iterator<Item = String>) -> Result<Command, ArgsError> {
    let address = socket_address(&required(&mut arguments, "address")?)?;
    reject_extra(&mut arguments)?;
    Ok(Command::RemoteStatus { address })
}

fn parse_remote_preview(mut arguments: impl Iterator<Item = String>) -> Result<Command, ArgsError> {
    let address = socket_address(&required(&mut arguments, "address")?)?;
    let input = number(&required(&mut arguments, "input")?, "input")?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(Command::RemotePreview {
        address,
        input,
        key,
        expected_revision,
    })
}

fn parse_remote_cut(mut arguments: impl Iterator<Item = String>) -> Result<Command, ArgsError> {
    let address = socket_address(&required(&mut arguments, "address")?)?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(Command::RemoteCut {
        address,
        key,
        expected_revision,
    })
}

fn parse_remote_timed_transition(
    command: &str,
    mut arguments: impl Iterator<Item = String>,
) -> Result<Command, ArgsError> {
    let address = socket_address(&required(&mut arguments, "address")?)?;
    let frames = number(&required(&mut arguments, "frames")?, "frames")?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(match command {
        "remote-fade" => Command::RemoteFade {
            address,
            frames,
            key,
            expected_revision,
        },
        "remote-alpha-fade" => Command::RemoteAlphaFade {
            address,
            frames,
            key,
            expected_revision,
        },
        "remote-slide" => Command::RemoteSlide {
            address,
            frames,
            key,
            expected_revision,
        },
        "remote-wipe" => Command::RemoteWipe {
            address,
            frames,
            key,
            expected_revision,
        },
        _ => unreachable!("caller only dispatches timed remote transitions"),
    })
}

fn command_options(
    mut arguments: impl Iterator<Item = String>,
) -> Result<(Option<String>, Option<u64>), ArgsError> {
    let mut key = None;
    let mut expected_revision = None;
    while let Some(option) = arguments.next() {
        match option.as_str() {
            "--key" => {
                let value = required(&mut arguments, "idempotency key")?;
                if value.trim().is_empty() {
                    return Err(ArgsError::BlankValue("idempotency key"));
                }
                key = Some(value);
            }
            "--expect" => {
                expected_revision = Some(number(
                    &required(&mut arguments, "expected revision")?,
                    "expected revision",
                )?);
            }
            _ => return Err(ArgsError::UnknownOption(option)),
        }
    }
    Ok((key, expected_revision))
}

fn reject_extra(arguments: &mut impl Iterator<Item = String>) -> Result<(), ArgsError> {
    match arguments.next() {
        Some(argument) => Err(ArgsError::UnexpectedArgument(argument)),
        None => Ok(()),
    }
}

fn required(
    arguments: &mut impl Iterator<Item = String>,
    field: &'static str,
) -> Result<String, ArgsError> {
    arguments.next().ok_or(ArgsError::MissingValue(field))
}

fn required_path(
    arguments: &mut impl Iterator<Item = String>,
    field: &'static str,
) -> Result<PathBuf, ArgsError> {
    required(arguments, field).map(PathBuf::from)
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

fn socket_address(value: &str) -> Result<SocketAddr, ArgsError> {
    number(value, "address")
}

fn manual_kind(value: &str) -> Result<ManualTransitionKind, ArgsError> {
    match value {
        "fade" => Ok(ManualTransitionKind::Fade),
        "wipe" => Ok(ManualTransitionKind::Wipe),
        "alpha-fade" => Ok(ManualTransitionKind::AlphaFade),
        _ => Err(ArgsError::InvalidChoice {
            field: "transition kind",
            value: value.to_owned(),
        }),
    }
}

fn fade_to_black_target(value: &str) -> Result<FadeToBlackTarget, ArgsError> {
    match value {
        "live" => Ok(FadeToBlackTarget::Live),
        "black" => Ok(FadeToBlackTarget::Black),
        _ => Err(ArgsError::InvalidChoice {
            field: "FTB target",
            value: value.to_owned(),
        }),
    }
}

fn basis_points(value: &str) -> Result<u16, ArgsError> {
    let position = number(value, "basis points")?;
    if position <= 10_000 {
        Ok(position)
    } else {
        Err(ArgsError::OutOfRange {
            field: "basis points",
            minimum: 0,
            maximum: 10_000,
            value: position,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn parses_revisioned_fade() {
        assert_eq!(
            parse(strings(&[
                "fade",
                "show.freemix",
                "30",
                "--key",
                "take-1",
                "--expect",
                "4"
            ])),
            Ok(Command::Fade {
                path: "show.freemix".into(),
                frames: 30,
                key: Some("take-1".into()),
                expected_revision: Some(4),
            })
        );
    }

    #[test]
    fn parses_local_and_remote_wipe() {
        assert_eq!(
            parse(strings(&[
                "wipe",
                "show.freemix",
                "45",
                "--key",
                "wipe-1",
                "--expect",
                "4",
            ])),
            Ok(Command::Wipe {
                path: "show.freemix".into(),
                frames: 45,
                key: Some("wipe-1".into()),
                expected_revision: Some(4),
            })
        );
        assert_eq!(
            parse(strings(&[
                "remote-wipe",
                "127.0.0.1:9123",
                "12",
                "--key",
                "remote-wipe",
            ])),
            Ok(Command::RemoteWipe {
                address: "127.0.0.1:9123".parse().unwrap(),
                frames: 12,
                key: Some("remote-wipe".into()),
                expected_revision: None,
            })
        );
    }

    #[test]
    fn parses_local_and_remote_alpha_fade() {
        assert_eq!(
            parse(strings(&[
                "alpha-fade",
                "show.freemix",
                "45",
                "--key",
                "alpha-fade-1",
                "--expect",
                "4",
            ])),
            Ok(Command::AlphaFade {
                path: "show.freemix".into(),
                frames: 45,
                key: Some("alpha-fade-1".into()),
                expected_revision: Some(4),
            })
        );
        assert_eq!(
            parse(strings(&[
                "remote-alpha-fade",
                "127.0.0.1:9123",
                "12",
                "--key",
                "remote-alpha-fade",
            ])),
            Ok(Command::RemoteAlphaFade {
                address: "127.0.0.1:9123".parse().unwrap(),
                frames: 12,
                key: Some("remote-alpha-fade".into()),
                expected_revision: None,
            })
        );
    }

    #[test]
    fn parses_local_and_remote_slide() {
        assert_eq!(
            parse(strings(&[
                "slide",
                "show.freemix",
                "45",
                "--key",
                "slide-1",
                "--expect",
                "4",
            ])),
            Ok(Command::Slide {
                path: "show.freemix".into(),
                frames: 45,
                key: Some("slide-1".into()),
                expected_revision: Some(4),
            })
        );
        assert_eq!(
            parse(strings(&[
                "remote-slide",
                "127.0.0.1:9123",
                "12",
                "--key",
                "remote-slide",
            ])),
            Ok(Command::RemoteSlide {
                address: "127.0.0.1:9123".parse().unwrap(),
                frames: 12,
                key: Some("remote-slide".into()),
                expected_revision: None,
            })
        );
    }

    #[test]
    fn parses_local_and_remote_t_bar_commands_with_exact_endpoints() {
        assert_eq!(
            parse(strings(&[
                "tbar-start",
                "show.freemix",
                "wipe",
                "--key",
                "manual-start",
                "--expect",
                "4",
            ])),
            Ok(Command::TBar {
                path: "show.freemix".into(),
                action: TBarAction::Start(ManualTransitionKind::Wipe),
                key: Some("manual-start".into()),
                expected_revision: Some(4),
            })
        );
        assert_eq!(
            parse(strings(&["tbar-position", "show.freemix", "0"])),
            Ok(Command::TBar {
                path: "show.freemix".into(),
                action: TBarAction::SetPosition(0),
                key: None,
                expected_revision: None,
            })
        );
        assert_eq!(
            parse(strings(&[
                "remote-tbar-position",
                "127.0.0.1:9123",
                "10000",
                "--expect",
                "9",
            ])),
            Ok(Command::RemoteTBar {
                address: "127.0.0.1:9123".parse().unwrap(),
                action: TBarAction::SetPosition(10_000),
                key: None,
                expected_revision: Some(9),
            })
        );
        assert_eq!(
            parse(strings(&[
                "remote-tbar-start",
                "127.0.0.1:9123",
                "alpha-fade",
                "--key",
                "manual-alpha-start",
            ])),
            Ok(Command::RemoteTBar {
                address: "127.0.0.1:9123".parse().unwrap(),
                action: TBarAction::Start(ManualTransitionKind::AlphaFade),
                key: Some("manual-alpha-start".into()),
                expected_revision: None,
            })
        );
        assert!(matches!(
            parse(strings(&["tbar-commit", "show.freemix"])),
            Ok(Command::TBar {
                action: TBarAction::Commit,
                ..
            })
        ));
        assert!(matches!(
            parse(strings(&["remote-tbar-cancel", "127.0.0.1:9123"])),
            Ok(Command::RemoteTBar {
                action: TBarAction::Cancel,
                ..
            })
        ));
    }

    #[test]
    fn parses_local_and_remote_fade_to_black_targets() {
        assert_eq!(
            parse(strings(&[
                "ftb",
                "show.freemix",
                "black",
                "45",
                "--key",
                "blackout",
                "--expect",
                "7",
            ])),
            Ok(Command::FadeToBlack {
                path: "show.freemix".into(),
                target: FadeToBlackTarget::Black,
                frames: 45,
                key: Some("blackout".into()),
                expected_revision: Some(7),
            })
        );
        assert_eq!(
            parse(strings(&["remote-ftb", "127.0.0.1:9123", "live", "30",])),
            Ok(Command::RemoteFadeToBlack {
                address: "127.0.0.1:9123".parse().unwrap(),
                target: FadeToBlackTarget::Live,
                frames: 30,
                key: None,
                expected_revision: None,
            })
        );
        assert_eq!(
            parse(strings(&["ftb", "show.freemix", "toggle", "10"])),
            Err(ArgsError::InvalidChoice {
                field: "FTB target",
                value: "toggle".into(),
            })
        );
    }

    #[test]
    fn rejects_non_integer_unknown_and_out_of_range_t_bar_values() {
        assert!(matches!(
            parse(strings(&["tbar-position", "show.freemix", "62.50"])),
            Err(ArgsError::InvalidNumber {
                field: "basis points",
                ..
            })
        ));
        assert_eq!(
            parse(strings(&["tbar-position", "show.freemix", "10001"])),
            Err(ArgsError::OutOfRange {
                field: "basis points",
                minimum: 0,
                maximum: 10_000,
                value: 10_001,
            })
        );
        assert_eq!(
            parse(strings(&["tbar-start", "show.freemix", "slide"])),
            Err(ArgsError::InvalidChoice {
                field: "transition kind",
                value: "slide".into(),
            })
        );
    }

    #[test]
    fn rejects_unknown_option() {
        assert_eq!(
            parse(strings(&["cut", "show.freemix", "--wat"])),
            Err(ArgsError::UnknownOption("--wat".into()))
        );
    }

    #[test]
    fn status_rejects_extra_positional_argument() {
        assert_eq!(
            parse(strings(&["status", "show.freemix", "extra"])),
            Err(ArgsError::UnexpectedArgument("extra".into()))
        );
    }

    #[test]
    fn demo_rejects_more_than_one_output_path() {
        assert_eq!(
            parse(strings(&[
                "demo",
                "show.freemix",
                "output.ppm",
                "extra.ppm"
            ])),
            Err(ArgsError::UnexpectedArgument("extra.ppm".into()))
        );
    }

    #[test]
    fn rejects_blank_explicit_idempotency_key() {
        assert_eq!(
            parse(strings(&["cut", "show.freemix", "--key", "  \t"])),
            Err(ArgsError::BlankValue("idempotency key"))
        );
    }

    #[test]
    fn parses_remote_preview_options() {
        assert_eq!(
            parse(strings(&[
                "remote-preview",
                "127.0.0.1:9123",
                "2",
                "--key",
                "remote-preview",
                "--expect",
                "7",
            ])),
            Ok(Command::RemotePreview {
                address: "127.0.0.1:9123".parse().unwrap(),
                input: 2,
                key: Some("remote-preview".into()),
                expected_revision: Some(7),
            })
        );
    }
}
