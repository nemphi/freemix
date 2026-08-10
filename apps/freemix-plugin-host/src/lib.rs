use std::collections::BTreeSet;
use std::fmt;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::str::FromStr;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_CONTROL_LINE_BYTES: usize = 4 * 1024;

pub const HELP: &str = "\
Restricted child process for FreeMix plugins (Phase 1 skeleton)

Usage: freemix-plugin-host --mode <native|wasm> [OPTIONS]

Options:
  -h, --help                     Print help
  -V, --version                  Print version
      --mode <native|wasm>       Select the plugin execution mode
      --allow-fs-read <PATH>     Allow filesystem reads at PATH (repeatable)
      --allow-fs-write <PATH>    Allow filesystem writes at PATH (repeatable)
      --allow-network <ENDPOINT> Allow connecting to ENDPOINT (repeatable)
      --allow-clock <KIND>       Allow monotonic or wall clock access (repeatable)
      --allow-command <NAME>     Allow submitting command NAME (repeatable)
      --max-memory-bytes <N>     Memory limit (default: 268435456)
      --max-fuel <N>             Wasm fuel limit (default: 10000000)
      --deadline-ms <N>          Operation deadline (default: 1000)

Control protocol on stdin/stdout: ping, status, shutdown (one command per line).
No plugins are loaded by this Phase 1 host.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostMode {
    Native,
    Wasm,
}

impl fmt::Display for HostMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Native => "native",
            Self::Wasm => "wasm",
        })
    }
}

impl FromStr for HostMode {
    type Err = ParseModeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "native" => Ok(Self::Native),
            "wasm" => Ok(Self::Wasm),
            _ => Err(ParseModeError(value.to_owned())),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseModeError(String);

impl fmt::Display for ParseModeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid mode `{}`; expected native or wasm",
            self.0
        )
    }
}

impl std::error::Error for ParseModeError {}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum FilesystemAccess {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ClockAccess {
    Monotonic,
    Wall,
}

impl FromStr for ClockAccess {
    type Err = ParseClockError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "monotonic" => Ok(Self::Monotonic),
            "wall" => Ok(Self::Wall),
            _ => Err(ParseClockError(value.to_owned())),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseClockError(String);

impl fmt::Display for ParseClockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid clock capability `{}`; expected monotonic or wall",
            self.0
        )
    }
}

impl std::error::Error for ParseClockError {}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Capability {
    Filesystem {
        access: FilesystemAccess,
        path: PathBuf,
    },
    Network {
        endpoint: String,
    },
    Clock {
        access: ClockAccess,
    },
    Command {
        name: String,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CapabilityManifest {
    grants: BTreeSet<Capability>,
}

impl CapabilityManifest {
    #[must_use]
    pub fn deny_all() -> Self {
        Self::default()
    }

    pub fn grant(&mut self, capability: Capability) {
        self.grants.insert(capability);
    }

    #[must_use]
    pub fn allows(&self, capability: &Capability) -> bool {
        self.grants.contains(capability)
    }

    /// Checks that a capability was explicitly granted.
    ///
    /// # Errors
    ///
    /// Returns [`CapabilityDenied`] when the manifest does not contain the grant.
    pub fn require(&self, capability: &Capability) -> Result<(), CapabilityDenied> {
        if self.allows(capability) {
            Ok(())
        } else {
            Err(CapabilityDenied(capability.clone()))
        }
    }

    pub fn grants(&self) -> impl Iterator<Item = &Capability> {
        self.grants.iter()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityDenied(pub Capability);

impl fmt::Display for CapabilityDenied {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "capability denied: {:?}", self.0)
    }
}

impl std::error::Error for CapabilityDenied {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceLimits {
    pub max_memory_bytes: u64,
    pub max_fuel: u64,
    pub deadline_ms: u64,
}

impl ResourceLimits {
    pub const MAX_MEMORY_BYTES: u64 = 16 * 1024 * 1024 * 1024;
    pub const MAX_FUEL: u64 = 1_000_000_000_000;
    pub const MAX_DEADLINE_MS: u64 = 60_000;

    /// Validates that every limit is nonzero and within the host policy maximum.
    ///
    /// # Errors
    ///
    /// Returns [`LimitsError`] for the first invalid field.
    pub fn validate(self) -> Result<Self, LimitsError> {
        validate_limit(
            "max-memory-bytes",
            self.max_memory_bytes,
            Self::MAX_MEMORY_BYTES,
        )?;
        validate_limit("max-fuel", self.max_fuel, Self::MAX_FUEL)?;
        validate_limit("deadline-ms", self.deadline_ms, Self::MAX_DEADLINE_MS)?;
        Ok(self)
    }
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_memory_bytes: 256 * 1024 * 1024,
            max_fuel: 10_000_000,
            deadline_ms: 1_000,
        }
    }
}

fn validate_limit(field: &'static str, value: u64, maximum: u64) -> Result<(), LimitsError> {
    if value == 0 {
        Err(LimitsError::Zero { field })
    } else if value > maximum {
        Err(LimitsError::AboveMaximum {
            field,
            value,
            maximum,
        })
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LimitsError {
    Zero {
        field: &'static str,
    },
    AboveMaximum {
        field: &'static str,
        value: u64,
        maximum: u64,
    },
}

impl fmt::Display for LimitsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero { field } => write!(formatter, "{field} must be greater than zero"),
            Self::AboveMaximum {
                field,
                value,
                maximum,
            } => write!(formatter, "{field} value {value} exceeds maximum {maximum}"),
        }
    }
}

impl std::error::Error for LimitsError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostConfig {
    pub mode: HostMode,
    pub capabilities: CapabilityManifest,
    pub limits: ResourceLimits,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    Help,
    Version,
    Run(HostConfig),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArgsError {
    MissingMode,
    MissingValue(String),
    InvalidValue {
        option: String,
        value: String,
        reason: String,
    },
    UnknownOption(String),
    UnexpectedArgument(String),
    DuplicateOption(String),
    InvalidLimits(LimitsError),
}

impl fmt::Display for ArgsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingMode => formatter.write_str("--mode is required"),
            Self::MissingValue(option) => write!(formatter, "missing value for {option}"),
            Self::InvalidValue {
                option,
                value,
                reason,
            } => write!(formatter, "invalid value `{value}` for {option}: {reason}"),
            Self::UnknownOption(option) => write!(formatter, "unknown option `{option}`"),
            Self::UnexpectedArgument(argument) => {
                write!(formatter, "unexpected argument `{argument}`")
            }
            Self::DuplicateOption(option) => write!(formatter, "duplicate option `{option}`"),
            Self::InvalidLimits(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ArgsError {}

/// Parses host options without reading process-global state.
///
/// # Errors
///
/// Returns [`ArgsError`] when options are missing, malformed, duplicated, or invalid.
pub fn parse_args(arguments: impl IntoIterator<Item = String>) -> Result<Action, ArgsError> {
    let mut arguments = arguments.into_iter().peekable();
    if let Some(first) = arguments.peek() {
        if first == "-h" || first == "--help" {
            arguments.next();
            reject_extra(arguments)?;
            return Ok(Action::Help);
        }
        if first == "-V" || first == "--version" {
            arguments.next();
            reject_extra(arguments)?;
            return Ok(Action::Version);
        }
    }

    let mut mode = None;
    let mut capabilities = CapabilityManifest::default();
    let mut limits = ResourceLimits::default();
    let mut memory_set = false;
    let mut fuel_set = false;
    let mut deadline_set = false;

    while let Some(option) = arguments.next() {
        match option.as_str() {
            "--mode" => {
                set_once(&mut mode, parse_value(&mut arguments, &option)?, &option)?;
            }
            "--allow-fs-read" => capabilities.grant(Capability::Filesystem {
                access: FilesystemAccess::Read,
                path: path_value(&mut arguments, &option)?,
            }),
            "--allow-fs-write" => capabilities.grant(Capability::Filesystem {
                access: FilesystemAccess::Write,
                path: path_value(&mut arguments, &option)?,
            }),
            "--allow-network" => capabilities.grant(Capability::Network {
                endpoint: nonblank_value(&mut arguments, &option)?,
            }),
            "--allow-clock" => capabilities.grant(Capability::Clock {
                access: parse_value(&mut arguments, &option)?,
            }),
            "--allow-command" => capabilities.grant(Capability::Command {
                name: nonblank_value(&mut arguments, &option)?,
            }),
            "--max-memory-bytes" => {
                set_limit(
                    &mut limits.max_memory_bytes,
                    &mut memory_set,
                    &mut arguments,
                    &option,
                )?;
            }
            "--max-fuel" => {
                set_limit(&mut limits.max_fuel, &mut fuel_set, &mut arguments, &option)?;
            }
            "--deadline-ms" => {
                set_limit(
                    &mut limits.deadline_ms,
                    &mut deadline_set,
                    &mut arguments,
                    &option,
                )?;
            }
            "-h" | "--help" | "-V" | "--version" => {
                return Err(ArgsError::UnexpectedArgument(option));
            }
            _ if option.starts_with('-') => return Err(ArgsError::UnknownOption(option)),
            _ => return Err(ArgsError::UnexpectedArgument(option)),
        }
    }

    let mode = mode.ok_or(ArgsError::MissingMode)?;
    let limits = limits.validate().map_err(ArgsError::InvalidLimits)?;
    Ok(Action::Run(HostConfig {
        mode,
        capabilities,
        limits,
    }))
}

fn reject_extra(mut arguments: impl Iterator<Item = String>) -> Result<(), ArgsError> {
    if let Some(argument) = arguments.next() {
        Err(ArgsError::UnexpectedArgument(argument))
    } else {
        Ok(())
    }
}

fn set_once<T>(slot: &mut Option<T>, value: T, option: &str) -> Result<(), ArgsError> {
    if slot.replace(value).is_some() {
        Err(ArgsError::DuplicateOption(option.to_owned()))
    } else {
        Ok(())
    }
}

fn set_limit(
    destination: &mut u64,
    was_set: &mut bool,
    arguments: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<(), ArgsError> {
    if *was_set {
        return Err(ArgsError::DuplicateOption(option.to_owned()));
    }
    *destination = parse_value(arguments, option)?;
    *was_set = true;
    Ok(())
}

fn parse_value<T>(
    arguments: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<T, ArgsError>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    let value = required_value(arguments, option)?;
    value
        .parse()
        .map_err(|error: T::Err| ArgsError::InvalidValue {
            option: option.to_owned(),
            value,
            reason: error.to_string(),
        })
}

fn path_value(
    arguments: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<PathBuf, ArgsError> {
    Ok(PathBuf::from(nonblank_value(arguments, option)?))
}

fn nonblank_value(
    arguments: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<String, ArgsError> {
    let value = required_value(arguments, option)?;
    if value.is_empty() {
        Err(ArgsError::InvalidValue {
            option: option.to_owned(),
            value,
            reason: "value must not be empty".to_owned(),
        })
    } else {
        Ok(value)
    }
}

fn required_value(
    arguments: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<String, ArgsError> {
    arguments
        .next()
        .ok_or_else(|| ArgsError::MissingValue(option.to_owned()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleState {
    Starting,
    Ready,
    ShuttingDown,
    Stopped,
    Failed,
}

impl fmt::Display for LifecycleState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::ShuttingDown => "shutting-down",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Lifecycle {
    state: LifecycleState,
}

impl Lifecycle {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: LifecycleState::Starting,
        }
    }

    #[must_use]
    pub const fn state(&self) -> LifecycleState {
        self.state
    }

    /// Moves to an allowed next lifecycle state.
    ///
    /// # Errors
    ///
    /// Returns [`LifecycleError`] when the requested transition is not allowed.
    pub fn transition(&mut self, next: LifecycleState) -> Result<(), LifecycleError> {
        let valid = matches!(
            (self.state, next),
            (
                LifecycleState::Starting,
                LifecycleState::Ready | LifecycleState::Failed
            ) | (
                LifecycleState::Ready,
                LifecycleState::ShuttingDown | LifecycleState::Failed
            ) | (
                LifecycleState::ShuttingDown,
                LifecycleState::Stopped | LifecycleState::Failed
            )
        );
        if !valid {
            return Err(LifecycleError {
                from: self.state,
                to: next,
            });
        }
        self.state = next;
        Ok(())
    }
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleError {
    pub from: LifecycleState,
    pub to: LifecycleState,
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid lifecycle transition from {} to {}",
            self.from, self.to
        )
    }
}

impl std::error::Error for LifecycleError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlCommand {
    Ping,
    Shutdown,
    Status,
}

impl FromStr for ControlCommand {
    type Err = ControlParseError;

    fn from_str(line: &str) -> Result<Self, Self::Err> {
        match line {
            "ping" => Ok(Self::Ping),
            "shutdown" => Ok(Self::Shutdown),
            "status" => Ok(Self::Status),
            "" => Err(ControlParseError::Empty),
            _ => Err(ControlParseError::Unknown(line.to_owned())),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlParseError {
    Empty,
    Unknown(String),
}

impl fmt::Display for ControlParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("empty control command"),
            Self::Unknown(command) => write!(formatter, "unknown control command `{command}`"),
        }
    }
}

impl std::error::Error for ControlParseError {}

/// Runs the newline-delimited control protocol until shutdown or end of input.
///
/// # Errors
///
/// Returns [`RunError`] if stream I/O fails or the supplied lifecycle cannot transition.
pub fn run_control_loop(
    mut input: impl BufRead,
    mut output: impl Write,
    lifecycle: &mut Lifecycle,
) -> Result<(), RunError> {
    lifecycle
        .transition(LifecycleState::Ready)
        .map_err(RunError::Lifecycle)?;

    let mut line = Vec::with_capacity(MAX_CONTROL_LINE_BYTES);
    loop {
        if !read_control_line(&mut input, &mut line)? {
            lifecycle
                .transition(LifecycleState::ShuttingDown)
                .map_err(RunError::Lifecycle)?;
            lifecycle
                .transition(LifecycleState::Stopped)
                .map_err(RunError::Lifecycle)?;
            return Ok(());
        }

        let command_line = std::str::from_utf8(&line)
            .map_err(|error| RunError::Io(io::Error::new(io::ErrorKind::InvalidData, error)))?
            .trim_end_matches(['\r', '\n']);
        match command_line.parse::<ControlCommand>() {
            Ok(ControlCommand::Ping) => writeln!(output, "pong").map_err(RunError::Io)?,
            Ok(ControlCommand::Status) => {
                writeln!(output, "status {}", lifecycle.state()).map_err(RunError::Io)?;
            }
            Ok(ControlCommand::Shutdown) => {
                lifecycle
                    .transition(LifecycleState::ShuttingDown)
                    .map_err(RunError::Lifecycle)?;
                writeln!(output, "shutting-down").map_err(RunError::Io)?;
                output.flush().map_err(RunError::Io)?;
                lifecycle
                    .transition(LifecycleState::Stopped)
                    .map_err(RunError::Lifecycle)?;
                return Ok(());
            }
            Err(error) => writeln!(output, "error {error}").map_err(RunError::Io)?,
        }
        output.flush().map_err(RunError::Io)?;
    }
}

fn read_control_line(input: &mut impl BufRead, line: &mut Vec<u8>) -> Result<bool, RunError> {
    line.clear();
    loop {
        let (length, complete) = {
            let buffer = input.fill_buf().map_err(RunError::Io)?;
            if buffer.is_empty() {
                return Ok(!line.is_empty());
            }
            let newline = buffer.iter().position(|&byte| byte == b'\n');
            let length = newline.map_or(buffer.len(), |index| index + 1);
            if line.len() + length > MAX_CONTROL_LINE_BYTES {
                return Err(RunError::ControlLineTooLong);
            }
            line.extend_from_slice(&buffer[..length]);
            (length, newline.is_some())
        };
        input.consume(length);
        if complete {
            return Ok(true);
        }
    }
}

#[derive(Debug)]
pub enum RunError {
    Io(io::Error),
    Lifecycle(LifecycleError),
    ControlLineTooLong,
}

impl fmt::Display for RunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Lifecycle(error) => error.fmt(formatter),
            Self::ControlLineTooLong => formatter.write_str("control line exceeds 4096 bytes"),
        }
    }
}

impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Lifecycle(error) => Some(error),
            Self::ControlLineTooLong => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn parses_help_and_version() {
        assert_eq!(parse_args(strings(&["--help"])), Ok(Action::Help));
        assert_eq!(parse_args(strings(&["-V"])), Ok(Action::Version));
    }

    #[test]
    fn parses_modes_and_defaults() {
        let Action::Run(native) = parse_args(strings(&["--mode", "native"])).unwrap() else {
            panic!("expected run action");
        };
        assert_eq!(native.mode, HostMode::Native);
        assert_eq!(native.limits, ResourceLimits::default());
        assert_eq!(native.capabilities.grants().count(), 0);

        let Action::Run(wasm) = parse_args(strings(&["--mode", "wasm"])).unwrap() else {
            panic!("expected run action");
        };
        assert_eq!(wasm.mode, HostMode::Wasm);
    }

    #[test]
    fn rejects_missing_or_invalid_mode_and_unknown_options() {
        assert_eq!(parse_args(Vec::new()), Err(ArgsError::MissingMode));
        assert!(matches!(
            parse_args(strings(&["--mode", "other"])),
            Err(ArgsError::InvalidValue { option, .. }) if option == "--mode"
        ));
        assert_eq!(
            parse_args(strings(&["--wat"])),
            Err(ArgsError::UnknownOption("--wat".to_owned()))
        );
    }

    #[test]
    fn parses_explicit_capability_grants() {
        let Action::Run(config) = parse_args(strings(&[
            "--mode",
            "wasm",
            "--allow-fs-read",
            "/media",
            "--allow-fs-write",
            "/cache",
            "--allow-network",
            "api.example.test:443",
            "--allow-clock",
            "monotonic",
            "--allow-command",
            "switch.preview",
        ]))
        .unwrap() else {
            panic!("expected run action");
        };

        assert!(config.capabilities.allows(&Capability::Filesystem {
            access: FilesystemAccess::Read,
            path: PathBuf::from("/media"),
        }));
        assert!(config.capabilities.allows(&Capability::Filesystem {
            access: FilesystemAccess::Write,
            path: PathBuf::from("/cache"),
        }));
        assert!(config.capabilities.allows(&Capability::Network {
            endpoint: "api.example.test:443".to_owned(),
        }));
        assert!(config.capabilities.allows(&Capability::Clock {
            access: ClockAccess::Monotonic,
        }));
        assert!(config.capabilities.allows(&Capability::Command {
            name: "switch.preview".to_owned(),
        }));
    }

    #[test]
    fn capabilities_are_default_deny_and_exact() {
        let mut manifest = CapabilityManifest::default();
        let read = Capability::Filesystem {
            access: FilesystemAccess::Read,
            path: PathBuf::from("/media"),
        };
        let write = Capability::Filesystem {
            access: FilesystemAccess::Write,
            path: PathBuf::from("/media"),
        };

        assert_eq!(manifest.require(&read), Err(CapabilityDenied(read.clone())));
        manifest.grant(read.clone());
        assert!(manifest.require(&read).is_ok());
        assert_eq!(manifest.require(&write), Err(CapabilityDenied(write)));
    }

    #[test]
    fn validates_resource_limits() {
        assert!(ResourceLimits::default().validate().is_ok());
        assert_eq!(
            ResourceLimits {
                max_memory_bytes: 0,
                ..ResourceLimits::default()
            }
            .validate(),
            Err(LimitsError::Zero {
                field: "max-memory-bytes"
            })
        );
        assert!(matches!(
            ResourceLimits {
                deadline_ms: ResourceLimits::MAX_DEADLINE_MS + 1,
                ..ResourceLimits::default()
            }
            .validate(),
            Err(LimitsError::AboveMaximum {
                field: "deadline-ms",
                ..
            })
        ));
    }

    #[test]
    fn validates_limits_from_arguments() {
        let Action::Run(config) = parse_args(strings(&[
            "--mode",
            "wasm",
            "--max-memory-bytes",
            "1024",
            "--max-fuel",
            "2000",
            "--deadline-ms",
            "30",
        ]))
        .unwrap() else {
            panic!("expected run action");
        };
        assert_eq!(config.limits.max_memory_bytes, 1024);
        assert_eq!(config.limits.max_fuel, 2000);
        assert_eq!(config.limits.deadline_ms, 30);
        assert!(matches!(
            parse_args(strings(&["--mode", "native", "--max-fuel", "0"])),
            Err(ArgsError::InvalidLimits(LimitsError::Zero {
                field: "max-fuel"
            }))
        ));
    }

    #[test]
    fn lifecycle_accepts_only_ordered_transitions() {
        let mut lifecycle = Lifecycle::new();
        assert_eq!(lifecycle.state(), LifecycleState::Starting);
        lifecycle.transition(LifecycleState::Ready).unwrap();
        lifecycle.transition(LifecycleState::ShuttingDown).unwrap();
        lifecycle.transition(LifecycleState::Stopped).unwrap();
        assert_eq!(lifecycle.state(), LifecycleState::Stopped);
        assert!(matches!(
            lifecycle.transition(LifecycleState::Ready),
            Err(LifecycleError {
                from: LifecycleState::Stopped,
                to: LifecycleState::Ready
            })
        ));
    }

    #[test]
    fn lifecycle_can_fail_before_stopping() {
        for initial in [LifecycleState::Starting, LifecycleState::Ready] {
            let mut lifecycle = Lifecycle::new();
            if initial == LifecycleState::Ready {
                lifecycle.transition(LifecycleState::Ready).unwrap();
            }
            lifecycle.transition(LifecycleState::Failed).unwrap();
            assert_eq!(lifecycle.state(), LifecycleState::Failed);
        }
    }

    #[test]
    fn parses_only_exact_control_commands() {
        assert_eq!("ping".parse(), Ok(ControlCommand::Ping));
        assert_eq!("shutdown".parse(), Ok(ControlCommand::Shutdown));
        assert_eq!("status".parse(), Ok(ControlCommand::Status));
        assert_eq!("".parse::<ControlCommand>(), Err(ControlParseError::Empty));
        assert!(matches!(
            " ping".parse::<ControlCommand>(),
            Err(ControlParseError::Unknown(command)) if command == " ping"
        ));
    }

    #[test]
    fn control_loop_responds_and_shuts_down() {
        let input = Cursor::new(b"ping\nstatus\nnope\nshutdown\nignored\n");
        let mut output = Vec::new();
        let mut lifecycle = Lifecycle::new();

        run_control_loop(input, &mut output, &mut lifecycle).unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "pong\nstatus ready\nerror unknown control command `nope`\nshutting-down\n"
        );
        assert_eq!(lifecycle.state(), LifecycleState::Stopped);
    }

    #[test]
    fn control_loop_rejects_oversized_line() {
        let mut accepted = b"shutdown".to_vec();
        accepted.resize(MAX_CONTROL_LINE_BYTES - 1, b'\r');
        accepted.push(b'\n');
        let mut accepted_output = Vec::new();
        let mut accepted_lifecycle = Lifecycle::new();
        run_control_loop(
            BufReader::with_capacity(64, Cursor::new(accepted)),
            &mut accepted_output,
            &mut accepted_lifecycle,
        )
        .unwrap();
        assert_eq!(accepted_output, b"shutting-down\n");

        let input = BufReader::with_capacity(
            64,
            Cursor::new(vec![b'x'; MAX_CONTROL_LINE_BYTES + 1]),
        );
        let mut output = Vec::new();
        let mut lifecycle = Lifecycle::new();

        assert!(matches!(
            run_control_loop(input, &mut output, &mut lifecycle),
            Err(RunError::ControlLineTooLong)
        ));
        assert!(output.is_empty());
    }

    #[test]
    fn end_of_input_stops_cleanly() {
        let mut lifecycle = Lifecycle::new();
        run_control_loop(Cursor::new([]), Vec::new(), &mut lifecycle).unwrap();
        assert_eq!(lifecycle.state(), LifecycleState::Stopped);
    }
}
