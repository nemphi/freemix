use std::{fmt, net::SocketAddr, num::NonZeroU128, path::PathBuf};

use fm_protocol::Role;
use fm_types::ProjectId;

use crate::{DEFAULT_DAEMON, RestartPolicy};

pub const HELP: &str = "\
FreeMix Studio native control application

Usage:
  freemix-studio --project <SHOW.FREEMIX> [OPTIONS] [--diagnose]
  freemix-studio --connect <ADDR> --project-id <ID> [OPTIONS] [--diagnose]
  freemix-studio --help
  freemix-studio --version

Connection options:
  --project <PATH>       Launch and supervise a daemon for this project bundle
  --daemon <PATH>        Daemon executable [default: freemixd]
  --listen <ADDR>        Supervised daemon listen address [default: 127.0.0.1:0]
  --connect <ADDR>       Connect to an existing daemon
  --project-id <ID>      Expected full nonzero u128 project ID in existing mode

Client options:
  --client-id <ID>       Protocol client identity [default: freemix-studio]
  --role <ROLE>          viewer, graphics, audio, replay, operator, or admin
                         [default: operator; viewer with --diagnose]
  --max-restarts <COUNT> Maximum supervised daemon restarts [default: 3]
  --osc-listen <ADDR>    Receive OSC control on a loopback address and nonzero port
  --diagnose             Run the one-shot TCP connection diagnostic instead of Studio
  -h, --help             Print help
  -V, --version          Print version";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    Help,
    Version,
    Open(StudioConfig),
    Diagnose(StudioConfig),
}

/// Settings common to both runtime ownership modes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StudioConfig {
    pub connection: ConnectionConfig,
    pub client_id: String,
    pub desired_role: Role,
    pub restart_policy: RestartPolicy,
    pub osc_listen: Option<SocketAddr>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConnectionConfig {
    Supervised(SupervisedConfig),
    Existing(ExistingConfig),
}

/// A Studio-owned daemon. The project identity comes from daemon readiness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SupervisedConfig {
    pub project_bundle: PathBuf,
    pub daemon_executable: PathBuf,
    pub listen: SocketAddr,
}

impl SupervisedConfig {
    #[must_use]
    pub fn new(project_bundle: impl Into<PathBuf>) -> Self {
        Self {
            project_bundle: project_bundle.into(),
            daemon_executable: PathBuf::from(DEFAULT_DAEMON),
            listen: default_listen(),
        }
    }
}

/// An independently-owned daemon with an identity that must match its handshake.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExistingConfig {
    pub address: SocketAddr,
    pub expected_project_id: ProjectId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArgsError {
    MissingValue(&'static str),
    MissingRequired(&'static str),
    DuplicateOption(&'static str),
    BlankValue(&'static str),
    InvalidAddress { option: &'static str, value: String },
    InvalidProjectId(String),
    InvalidRestartCount(String),
    InvalidRole(String),
    ConflictingModes,
    SupervisedOnly(&'static str),
    ExistingOnly(&'static str),
    NonLoopbackListen(SocketAddr),
    InvalidOscListen(SocketAddr),
    OscUnavailableInDiagnose,
    UnknownArgument(String),
}

impl fmt::Display for ArgsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingValue(option) => write!(formatter, "missing value for {option}"),
            Self::MissingRequired(option) => {
                write!(formatter, "required option {option} is missing")
            }
            Self::DuplicateOption(option) => {
                write!(formatter, "option {option} was provided more than once")
            }
            Self::BlankValue(option) => write!(formatter, "value for {option} must not be blank"),
            Self::InvalidAddress { option, value } => {
                write!(formatter, "invalid socket address `{value}` for {option}")
            }
            Self::InvalidProjectId(value) => {
                write!(formatter, "project ID `{value}` must be a nonzero u128")
            }
            Self::InvalidRestartCount(value) => {
                write!(formatter, "restart count `{value}` must be a u8")
            }
            Self::InvalidRole(role) => write!(formatter, "unknown role `{role}`"),
            Self::ConflictingModes => {
                formatter.write_str("--project and --connect cannot be used together")
            }
            Self::SupervisedOnly(option) => {
                write!(formatter, "option {option} is only valid with --project")
            }
            Self::ExistingOnly(option) => {
                write!(formatter, "option {option} is only valid with --connect")
            }
            Self::NonLoopbackListen(address) => write!(
                formatter,
                "supervised listen address must be loopback, got {address}"
            ),
            Self::InvalidOscListen(address) => write!(
                formatter,
                "OSC listen address must be loopback with a nonzero port, got {address}"
            ),
            Self::OscUnavailableInDiagnose => {
                formatter.write_str("--osc-listen cannot be used with --diagnose")
            }
            Self::UnknownArgument(argument) => write!(formatter, "unknown argument `{argument}`"),
        }
    }
}

impl std::error::Error for ArgsError {}

/// Parses process arguments after the executable name.
///
/// # Errors
///
/// Returns an error for unknown, duplicate, conflicting, missing, or invalid options.
pub fn parse_args(arguments: impl IntoIterator<Item = String>) -> Result<Command, ArgsError> {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    if arguments.as_slice() == ["--help"] || arguments.as_slice() == ["-h"] {
        return Ok(Command::Help);
    }
    if arguments.as_slice() == ["--version"] || arguments.as_slice() == ["-V"] {
        return Ok(Command::Version);
    }

    let mut project = None;
    let mut daemon = None;
    let mut listen = None;
    let mut connect = None;
    let mut project_id = None;
    let mut client_id = None;
    let mut role = None;
    let mut maximum_restarts = None;
    let mut osc_listen = None;
    let mut diagnose = None;
    let mut arguments = arguments.into_iter();
    while let Some(option) = arguments.next() {
        match option.as_str() {
            "--project" => set_once(
                &mut project,
                PathBuf::from(nonblank(&mut arguments, "--project")?),
                "--project",
            )?,
            "--daemon" => set_once(
                &mut daemon,
                PathBuf::from(nonblank(&mut arguments, "--daemon")?),
                "--daemon",
            )?,
            "--listen" => {
                let value = required(&mut arguments, "--listen")?;
                let address = socket_address("--listen", &value)?;
                set_once(&mut listen, address, "--listen")?;
            }
            "--connect" => {
                let value = required(&mut arguments, "--connect")?;
                let address = socket_address("--connect", &value)?;
                set_once(&mut connect, address, "--connect")?;
            }
            "--project-id" => {
                let value = required(&mut arguments, "--project-id")?;
                set_once(&mut project_id, parse_project_id(&value)?, "--project-id")?;
            }
            "--client-id" => set_once(
                &mut client_id,
                nonblank(&mut arguments, "--client-id")?,
                "--client-id",
            )?,
            "--role" => {
                let value = required(&mut arguments, "--role")?;
                set_once(&mut role, parse_role(&value)?, "--role")?;
            }
            "--max-restarts" => {
                let value = required(&mut arguments, "--max-restarts")?;
                let count = value
                    .parse()
                    .map_err(|_| ArgsError::InvalidRestartCount(value))?;
                set_once(&mut maximum_restarts, count, "--max-restarts")?;
            }
            "--osc-listen" => {
                let value = required(&mut arguments, "--osc-listen")?;
                let address = socket_address("--osc-listen", &value)?;
                set_once(&mut osc_listen, address, "--osc-listen")?;
            }
            "--diagnose" => set_once(&mut diagnose, (), "--diagnose")?,
            _ => return Err(ArgsError::UnknownArgument(option)),
        }
    }

    let connection = connection_config(project, daemon, listen, connect, project_id)?;
    if diagnose.is_some() && osc_listen.is_some() {
        return Err(ArgsError::OscUnavailableInDiagnose);
    }
    if let Some(address) = osc_listen
        && (!address.ip().is_loopback() || address.port() == 0)
    {
        return Err(ArgsError::InvalidOscListen(address));
    }

    let config = StudioConfig {
        connection,
        client_id: client_id.unwrap_or_else(|| "freemix-studio".to_owned()),
        desired_role: role.unwrap_or(if diagnose.is_some() {
            Role::Viewer
        } else {
            Role::Operator
        }),
        restart_policy: RestartPolicy {
            maximum_restarts: maximum_restarts.unwrap_or(3),
        },
        osc_listen,
    };
    Ok(if diagnose.is_some() {
        Command::Diagnose(config)
    } else {
        Command::Open(config)
    })
}

fn connection_config(
    project: Option<PathBuf>,
    daemon: Option<PathBuf>,
    listen: Option<SocketAddr>,
    connect: Option<SocketAddr>,
    project_id: Option<ProjectId>,
) -> Result<ConnectionConfig, ArgsError> {
    match (project, connect) {
        (Some(_), Some(_)) => Err(ArgsError::ConflictingModes),
        (Some(project_bundle), None) => {
            if project_id.is_some() {
                return Err(ArgsError::ExistingOnly("--project-id"));
            }
            let listen = listen.unwrap_or_else(default_listen);
            if !listen.ip().is_loopback() {
                return Err(ArgsError::NonLoopbackListen(listen));
            }
            Ok(ConnectionConfig::Supervised(SupervisedConfig {
                project_bundle,
                daemon_executable: daemon.unwrap_or_else(|| PathBuf::from(DEFAULT_DAEMON)),
                listen,
            }))
        }
        (None, Some(address)) => {
            if daemon.is_some() {
                return Err(ArgsError::SupervisedOnly("--daemon"));
            }
            if listen.is_some() {
                return Err(ArgsError::SupervisedOnly("--listen"));
            }
            Ok(ConnectionConfig::Existing(ExistingConfig {
                address,
                expected_project_id: project_id
                    .ok_or(ArgsError::MissingRequired("--project-id"))?,
            }))
        }
        (None, None) => Err(ArgsError::MissingRequired("--project or --connect")),
    }
}

fn required(
    arguments: &mut impl Iterator<Item = String>,
    option: &'static str,
) -> Result<String, ArgsError> {
    arguments.next().ok_or(ArgsError::MissingValue(option))
}

fn nonblank(
    arguments: &mut impl Iterator<Item = String>,
    option: &'static str,
) -> Result<String, ArgsError> {
    let value = required(arguments, option)?;
    if value.trim().is_empty() {
        Err(ArgsError::BlankValue(option))
    } else {
        Ok(value)
    }
}

fn set_once<T>(slot: &mut Option<T>, value: T, option: &'static str) -> Result<(), ArgsError> {
    if slot.replace(value).is_some() {
        Err(ArgsError::DuplicateOption(option))
    } else {
        Ok(())
    }
}

fn socket_address(option: &'static str, value: &str) -> Result<SocketAddr, ArgsError> {
    value.parse().map_err(|_| ArgsError::InvalidAddress {
        option,
        value: value.to_owned(),
    })
}

fn parse_project_id(value: &str) -> Result<ProjectId, ArgsError> {
    value
        .parse::<NonZeroU128>()
        .map(ProjectId::new)
        .map_err(|_| ArgsError::InvalidProjectId(value.to_owned()))
}

fn parse_role(value: &str) -> Result<Role, ArgsError> {
    match value {
        "viewer" => Ok(Role::Viewer),
        "graphics" => Ok(Role::Graphics),
        "audio" => Ok(Role::Audio),
        "replay" => Ok(Role::Replay),
        "operator" => Ok(Role::Operator),
        "admin" => Ok(Role::Admin),
        _ => Err(ArgsError::InvalidRole(value.to_owned())),
    }
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}
