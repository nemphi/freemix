use std::{net::SocketAddr, path::PathBuf};

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
    Wipe {
        path: PathBuf,
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
    RemoteWipe {
        address: SocketAddr,
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
    InvalidNumber { field: &'static str, value: String },
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
            Self::InvalidNumber { field, value } => {
                write!(formatter, "invalid {field} value `{value}`")
            }
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
        "new" => {
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
        "fade" => {
            let path = required_path(&mut arguments, "project path")?;
            let frames = number(&required(&mut arguments, "frames")?, "frames")?;
            let (key, expected_revision) = command_options(arguments)?;
            Ok(Command::Fade {
                path,
                frames,
                key,
                expected_revision,
            })
        }
        "wipe" => {
            let path = required_path(&mut arguments, "project path")?;
            let frames = number(&required(&mut arguments, "frames")?, "frames")?;
            let (key, expected_revision) = command_options(arguments)?;
            Ok(Command::Wipe {
                path,
                frames,
                key,
                expected_revision,
            })
        }
        "remote-status" => parse_remote_status(arguments),
        "remote-preview" => parse_remote_preview(arguments),
        "remote-cut" => parse_remote_cut(arguments),
        "remote-fade" => parse_remote_fade(arguments),
        "remote-wipe" => parse_remote_wipe(arguments),
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

fn parse_remote_fade(mut arguments: impl Iterator<Item = String>) -> Result<Command, ArgsError> {
    let address = socket_address(&required(&mut arguments, "address")?)?;
    let frames = number(&required(&mut arguments, "frames")?, "frames")?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(Command::RemoteFade {
        address,
        frames,
        key,
        expected_revision,
    })
}

fn parse_remote_wipe(mut arguments: impl Iterator<Item = String>) -> Result<Command, ArgsError> {
    let address = socket_address(&required(&mut arguments, "address")?)?;
    let frames = number(&required(&mut arguments, "frames")?, "frames")?;
    let (key, expected_revision) = command_options(arguments)?;
    Ok(Command::RemoteWipe {
        address,
        frames,
        key,
        expected_revision,
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
