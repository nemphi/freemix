use std::fmt;
use std::io::{self, BufRead, Write};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const MIN_BLOCK_FRAMES: u32 = 16;
pub const MAX_BLOCK_FRAMES: u32 = 8_192;
pub const MIN_CHANNELS: u16 = 1;
pub const MAX_CHANNELS: u16 = 64;
pub const MIN_DEADLINE_US: u64 = 100;
pub const MAX_DEADLINE_US: u64 = 1_000_000;
pub const MISSED_DEADLINE_BYPASS_THRESHOLD: u32 = 3;

pub const HELP: &str = "freemix-dsp-host - dedicated FreeMix DSP child process skeleton

Usage: freemix-dsp-host [OPTIONS]

Options:
  --block-frames <FRAMES>  Audio block size (16..=8192) [default: 256]
  --channels <CHANNELS>    Audio channel count (1..=64) [default: 2]
  --deadline-us <MICROS>   Block deadline in microseconds (100..=1000000) [default: 5000]
  -h, --help               Print help
  -V, --version            Print version

Control protocol (one command per stdin line):
  ping
  status
  bypass on|off
  shutdown";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    pub block_frames: u32,
    pub channels: u16,
    pub deadline_us: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            block_frames: 256,
            channels: 2,
            deadline_us: 5_000,
        }
    }
}

impl Config {
    /// Builds a validated DSP host configuration.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when any value is outside its documented bounds.
    pub fn new(block_frames: u32, channels: u16, deadline_us: u64) -> Result<Self, ConfigError> {
        validate_range(
            "block frames",
            u64::from(block_frames),
            u64::from(MIN_BLOCK_FRAMES),
            u64::from(MAX_BLOCK_FRAMES),
        )?;
        validate_range(
            "channels",
            u64::from(channels),
            u64::from(MIN_CHANNELS),
            u64::from(MAX_CHANNELS),
        )?;
        validate_range("deadline", deadline_us, MIN_DEADLINE_US, MAX_DEADLINE_US)?;
        Ok(Self {
            block_frames,
            channels,
            deadline_us,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigError {
    field: &'static str,
    value: u64,
    min: u64,
    max: u64,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} value {} is outside {}..={}",
            self.field, self.value, self.min, self.max
        )
    }
}

impl std::error::Error for ConfigError {}

fn validate_range(field: &'static str, value: u64, min: u64, max: u64) -> Result<(), ConfigError> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(ConfigError {
            field,
            value,
            min,
            max,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliAction {
    Run(Config),
    Help,
    Version,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArgsError {
    MissingValue(&'static str),
    InvalidNumber { field: &'static str, value: String },
    InvalidConfig(ConfigError),
    UnknownArgument(String),
}

impl fmt::Display for ArgsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingValue(field) => write!(formatter, "missing value for {field}"),
            Self::InvalidNumber { field, value } => {
                write!(formatter, "invalid {field} value `{value}`")
            }
            Self::InvalidConfig(error) => error.fmt(formatter),
            Self::UnknownArgument(argument) => write!(formatter, "unknown argument `{argument}`"),
        }
    }
}

impl std::error::Error for ArgsError {}

/// Parses process arguments without the executable name.
///
/// # Errors
///
/// Returns [`ArgsError`] for unknown arguments, missing or non-numeric option
/// values, and values outside the [`Config`] bounds.
pub fn parse_args(arguments: impl IntoIterator<Item = String>) -> Result<CliAction, ArgsError> {
    let mut arguments = arguments.into_iter();
    let mut block_frames = Config::default().block_frames;
    let mut channels = Config::default().channels;
    let mut deadline_us = Config::default().deadline_us;

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(CliAction::Help),
            "-V" | "--version" => return Ok(CliAction::Version),
            "--block-frames" => {
                block_frames = parse_number(arguments.next(), "block frames", "--block-frames")?;
            }
            "--channels" => {
                channels = parse_number(arguments.next(), "channels", "--channels")?;
            }
            "--deadline-us" => {
                deadline_us = parse_number(arguments.next(), "deadline", "--deadline-us")?;
            }
            _ => return Err(ArgsError::UnknownArgument(argument)),
        }
    }

    Config::new(block_frames, channels, deadline_us)
        .map(CliAction::Run)
        .map_err(ArgsError::InvalidConfig)
}

fn parse_number<T: std::str::FromStr>(
    value: Option<String>,
    field: &'static str,
    option: &'static str,
) -> Result<T, ArgsError> {
    let value = value.ok_or(ArgsError::MissingValue(option))?;
    value
        .parse()
        .map_err(|_| ArgsError::InvalidNumber { field, value })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    Ping,
    Status,
    SetBypass(bool),
    Shutdown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandError {
    Empty,
    Unknown(String),
    UnexpectedArgument(String),
    MissingBypassState,
    InvalidBypassState(String),
}

impl fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("empty command"),
            Self::Unknown(command) => write!(formatter, "unknown command `{command}`"),
            Self::UnexpectedArgument(argument) => {
                write!(formatter, "unexpected argument `{argument}`")
            }
            Self::MissingBypassState => formatter.write_str("bypass requires `on` or `off`"),
            Self::InvalidBypassState(state) => {
                write!(
                    formatter,
                    "invalid bypass state `{state}`; expected `on` or `off`"
                )
            }
        }
    }
}

impl std::error::Error for CommandError {}

/// Parses one line from the child-process control channel.
///
/// # Errors
///
/// Returns [`CommandError`] when the line is empty, unknown, or malformed.
pub fn parse_command(line: &str) -> Result<Command, CommandError> {
    let mut parts = line.split_whitespace();
    let command = parts.next().ok_or(CommandError::Empty)?;
    let parsed = match command {
        "ping" => Command::Ping,
        "status" => Command::Status,
        "shutdown" => Command::Shutdown,
        "bypass" => {
            let state = parts.next().ok_or(CommandError::MissingBypassState)?;
            match state {
                "on" => Command::SetBypass(true),
                "off" => Command::SetBypass(false),
                _ => return Err(CommandError::InvalidBypassState(state.to_owned())),
            }
        }
        _ => return Err(CommandError::Unknown(command.to_owned())),
    };
    if let Some(argument) = parts.next() {
        return Err(CommandError::UnexpectedArgument(argument.to_owned()));
    }
    Ok(parsed)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lifecycle {
    Configured,
    Running,
    Stopping,
    Stopped,
}

impl Lifecycle {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BypassReason {
    Manual,
    MissedDeadlines,
}

impl BypassReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::MissedDeadlines => "missed-deadlines",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HeartbeatRecord {
    pub sequence: u64,
    pub lifecycle: Lifecycle,
    pub bypass_reason: Option<BypassReason>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeadlineRecord {
    pub sequence: u64,
    pub elapsed_us: u64,
    pub budget_us: u64,
    pub missed: bool,
    pub consecutive_misses: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatusRecord {
    pub lifecycle: Lifecycle,
    pub bypass_reason: Option<BypassReason>,
    pub heartbeat_count: u64,
    pub block_count: u64,
    pub missed_deadline_count: u64,
    pub consecutive_deadline_misses: u32,
    pub last_deadline: Option<DeadlineRecord>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Host {
    config: Config,
    lifecycle: Lifecycle,
    bypass_reason: Option<BypassReason>,
    heartbeat_count: u64,
    block_count: u64,
    missed_deadline_count: u64,
    consecutive_deadline_misses: u32,
    last_deadline: Option<DeadlineRecord>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LifecycleError {
    operation: &'static str,
    lifecycle: Lifecycle,
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "cannot {} while host is {}",
            self.operation,
            self.lifecycle.as_str()
        )
    }
}

impl std::error::Error for LifecycleError {}

impl Host {
    #[must_use]
    pub const fn new(config: Config) -> Self {
        Self {
            config,
            lifecycle: Lifecycle::Configured,
            bypass_reason: None,
            heartbeat_count: 0,
            block_count: 0,
            missed_deadline_count: 0,
            consecutive_deadline_misses: 0,
            last_deadline: None,
        }
    }

    /// Moves a configured host into its running state.
    ///
    /// # Errors
    ///
    /// Returns [`LifecycleError`] unless the host is configured.
    pub fn start(&mut self) -> Result<(), LifecycleError> {
        self.transition(Lifecycle::Configured, Lifecycle::Running, "start")
    }

    /// Moves a running host into its stopping state.
    ///
    /// # Errors
    ///
    /// Returns [`LifecycleError`] unless the host is running.
    pub fn begin_shutdown(&mut self) -> Result<(), LifecycleError> {
        self.transition(Lifecycle::Running, Lifecycle::Stopping, "begin shutdown")
    }

    /// Moves a stopping host into its stopped state.
    ///
    /// # Errors
    ///
    /// Returns [`LifecycleError`] unless the host is stopping.
    pub fn finish_shutdown(&mut self) -> Result<(), LifecycleError> {
        self.transition(Lifecycle::Stopping, Lifecycle::Stopped, "finish shutdown")
    }

    fn transition(
        &mut self,
        expected: Lifecycle,
        next: Lifecycle,
        operation: &'static str,
    ) -> Result<(), LifecycleError> {
        if self.lifecycle != expected {
            return Err(LifecycleError {
                operation,
                lifecycle: self.lifecycle,
            });
        }
        self.lifecycle = next;
        Ok(())
    }

    /// Enables or clears manual bypass.
    ///
    /// # Errors
    ///
    /// Returns [`LifecycleError`] unless the host is running.
    pub fn set_bypass(&mut self, enabled: bool) -> Result<(), LifecycleError> {
        self.require_running("change bypass")?;
        self.bypass_reason = enabled.then_some(BypassReason::Manual);
        Ok(())
    }

    /// Records a supervisor heartbeat.
    ///
    /// # Errors
    ///
    /// Returns [`LifecycleError`] unless the host is running.
    pub fn heartbeat(&mut self) -> Result<HeartbeatRecord, LifecycleError> {
        self.require_running("record heartbeat")?;
        self.heartbeat_count = self.heartbeat_count.saturating_add(1);
        Ok(HeartbeatRecord {
            sequence: self.heartbeat_count,
            lifecycle: self.lifecycle,
            bypass_reason: self.bypass_reason,
        })
    }

    /// Records one future processing loop's elapsed time against the budget.
    ///
    /// Three consecutive misses engage a sticky deadline bypass. No audio or
    /// plugin processing is performed by this method.
    ///
    /// # Errors
    ///
    /// Returns [`LifecycleError`] unless the host is running.
    pub fn record_deadline(&mut self, elapsed_us: u64) -> Result<DeadlineRecord, LifecycleError> {
        self.require_running("record deadline")?;
        self.block_count = self.block_count.saturating_add(1);
        let missed = elapsed_us > self.config.deadline_us;
        if missed {
            self.missed_deadline_count = self.missed_deadline_count.saturating_add(1);
            self.consecutive_deadline_misses = self.consecutive_deadline_misses.saturating_add(1);
            if self.consecutive_deadline_misses >= MISSED_DEADLINE_BYPASS_THRESHOLD
                && self.bypass_reason.is_none()
            {
                self.bypass_reason = Some(BypassReason::MissedDeadlines);
            }
        } else {
            self.consecutive_deadline_misses = 0;
        }
        let record = DeadlineRecord {
            sequence: self.block_count,
            elapsed_us,
            budget_us: self.config.deadline_us,
            missed,
            consecutive_misses: self.consecutive_deadline_misses,
        };
        self.last_deadline = Some(record);
        Ok(record)
    }

    fn require_running(&self, operation: &'static str) -> Result<(), LifecycleError> {
        if self.lifecycle == Lifecycle::Running {
            Ok(())
        } else {
            Err(LifecycleError {
                operation,
                lifecycle: self.lifecycle,
            })
        }
    }

    #[must_use]
    pub const fn config(&self) -> Config {
        self.config
    }

    #[must_use]
    pub const fn status(&self) -> StatusRecord {
        StatusRecord {
            lifecycle: self.lifecycle,
            bypass_reason: self.bypass_reason,
            heartbeat_count: self.heartbeat_count,
            block_count: self.block_count,
            missed_deadline_count: self.missed_deadline_count,
            consecutive_deadline_misses: self.consecutive_deadline_misses,
            last_deadline: self.last_deadline,
        }
    }
}

/// Runs the line-oriented child-process control loop until shutdown or EOF.
///
/// # Errors
///
/// Returns an I/O error if stdin cannot be read, stdout cannot be written or
/// flushed, or an internal lifecycle transition unexpectedly fails.
pub fn run_control_loop(
    config: Config,
    input: impl BufRead,
    mut output: impl Write,
) -> io::Result<()> {
    let mut host = Host::new(config);
    host.start().map_err(io::Error::other)?;
    writeln!(
        output,
        "ready version={} block_frames={} channels={} deadline_us={}",
        VERSION, config.block_frames, config.channels, config.deadline_us
    )?;
    output.flush()?;

    for line in input.lines() {
        let line = line?;
        let Ok(command) = parse_command(&line) else {
            writeln!(output, "error invalid-command")?;
            output.flush()?;
            continue;
        };
        match command {
            Command::Ping => {
                let heartbeat = host.heartbeat().map_err(io::Error::other)?;
                writeln!(output, "pong heartbeat={}", heartbeat.sequence)?;
            }
            Command::Status => write_status(&mut output, config, host.status())?,
            Command::SetBypass(enabled) => {
                host.set_bypass(enabled).map_err(io::Error::other)?;
                let reason = host.status().bypass_reason;
                writeln!(
                    output,
                    "ok bypass={} reason={}",
                    on_off(reason.is_some()),
                    reason.map_or("none", BypassReason::as_str)
                )?;
            }
            Command::Shutdown => {
                host.begin_shutdown().map_err(io::Error::other)?;
                writeln!(output, "ok shutdown lifecycle=stopping")?;
                output.flush()?;
                host.finish_shutdown().map_err(io::Error::other)?;
                return Ok(());
            }
        }
        output.flush()?;
    }

    host.begin_shutdown().map_err(io::Error::other)?;
    host.finish_shutdown().map_err(io::Error::other)?;
    Ok(())
}

fn write_status(output: &mut impl Write, config: Config, status: StatusRecord) -> io::Result<()> {
    writeln!(
        output,
        "status lifecycle={} bypass={} reason={} block_frames={} channels={} deadline_us={} heartbeats={} blocks={} missed_deadlines={} consecutive_misses={}",
        status.lifecycle.as_str(),
        on_off(status.bypass_reason.is_some()),
        status.bypass_reason.map_or("none", BypassReason::as_str),
        config.block_frames,
        config.channels,
        config.deadline_us,
        status.heartbeat_count,
        status.block_count,
        status.missed_deadline_count,
        status.consecutive_deadline_misses
    )
}

const fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running_host() -> Host {
        let mut host = Host::new(Config::default());
        host.start().unwrap();
        host
    }

    #[test]
    fn config_accepts_inclusive_bounds() {
        assert!(Config::new(MIN_BLOCK_FRAMES, MIN_CHANNELS, MIN_DEADLINE_US).is_ok());
        assert!(Config::new(MAX_BLOCK_FRAMES, MAX_CHANNELS, MAX_DEADLINE_US).is_ok());
    }

    #[test]
    fn config_rejects_values_outside_bounds() {
        assert!(Config::new(MIN_BLOCK_FRAMES - 1, 2, 5_000).is_err());
        assert!(Config::new(MAX_BLOCK_FRAMES + 1, 2, 5_000).is_err());
        assert!(Config::new(256, MIN_CHANNELS - 1, 5_000).is_err());
        assert!(Config::new(256, MAX_CHANNELS + 1, 5_000).is_err());
        assert!(Config::new(256, 2, MIN_DEADLINE_US - 1).is_err());
        assert!(Config::new(256, 2, MAX_DEADLINE_US + 1).is_err());
    }

    #[test]
    fn cli_configuration_is_validated() {
        let action = parse_args([
            "--block-frames".to_owned(),
            "512".to_owned(),
            "--channels".to_owned(),
            "8".to_owned(),
            "--deadline-us".to_owned(),
            "2000".to_owned(),
        ])
        .unwrap();
        assert_eq!(action, CliAction::Run(Config::new(512, 8, 2_000).unwrap()));
        assert!(parse_args(["--channels".to_owned(), "0".to_owned()]).is_err());
    }

    #[test]
    fn command_parser_accepts_control_commands() {
        assert_eq!(parse_command("ping"), Ok(Command::Ping));
        assert_eq!(parse_command(" status "), Ok(Command::Status));
        assert_eq!(parse_command("bypass on"), Ok(Command::SetBypass(true)));
        assert_eq!(parse_command("bypass off"), Ok(Command::SetBypass(false)));
        assert_eq!(parse_command("shutdown"), Ok(Command::Shutdown));
    }

    #[test]
    fn command_parser_rejects_malformed_commands() {
        assert_eq!(parse_command(""), Err(CommandError::Empty));
        assert_eq!(
            parse_command("bypass"),
            Err(CommandError::MissingBypassState)
        );
        assert!(matches!(
            parse_command("bypass maybe"),
            Err(CommandError::InvalidBypassState(_))
        ));
        assert!(matches!(
            parse_command("ping now"),
            Err(CommandError::UnexpectedArgument(_))
        ));
        assert!(matches!(
            parse_command("start"),
            Err(CommandError::Unknown(_))
        ));
    }

    #[test]
    fn consecutive_missed_deadlines_engage_sticky_bypass() {
        let mut host = running_host();
        for expected in 1..MISSED_DEADLINE_BYPASS_THRESHOLD {
            let record = host.record_deadline(5_001).unwrap();
            assert!(record.missed);
            assert_eq!(record.consecutive_misses, expected);
            assert_eq!(host.status().bypass_reason, None);
        }

        host.record_deadline(5_001).unwrap();
        assert_eq!(
            host.status().bypass_reason,
            Some(BypassReason::MissedDeadlines)
        );

        host.record_deadline(5_000).unwrap();
        assert_eq!(host.status().consecutive_deadline_misses, 0);
        assert_eq!(
            host.status().bypass_reason,
            Some(BypassReason::MissedDeadlines)
        );
        host.set_bypass(false).unwrap();
        assert_eq!(host.status().bypass_reason, None);
    }

    #[test]
    fn successful_deadline_breaks_a_miss_streak() {
        let mut host = running_host();
        host.record_deadline(5_001).unwrap();
        host.record_deadline(5_001).unwrap();
        host.record_deadline(5_000).unwrap();
        host.record_deadline(5_001).unwrap();
        assert_eq!(host.status().consecutive_deadline_misses, 1);
        assert_eq!(host.status().missed_deadline_count, 3);
        assert_eq!(host.status().bypass_reason, None);
    }

    #[test]
    fn lifecycle_requires_ordered_transitions() {
        let mut host = Host::new(Config::default());
        assert_eq!(host.status().lifecycle, Lifecycle::Configured);
        assert!(host.begin_shutdown().is_err());
        host.start().unwrap();
        assert_eq!(host.status().lifecycle, Lifecycle::Running);
        assert!(host.start().is_err());
        host.begin_shutdown().unwrap();
        assert_eq!(host.status().lifecycle, Lifecycle::Stopping);
        assert!(host.heartbeat().is_err());
        host.finish_shutdown().unwrap();
        assert_eq!(host.status().lifecycle, Lifecycle::Stopped);
    }

    #[test]
    fn control_loop_reports_heartbeat_status_bypass_and_shutdown() {
        let input = b"ping\nstatus\nbypass on\nstatus\nbypass off\nnope\nshutdown\nignored\n";
        let mut output = Vec::new();
        run_control_loop(Config::default(), &input[..], &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();

        assert!(output.starts_with("ready version="));
        assert!(output.contains("pong heartbeat=1\n"));
        assert!(output.contains("status lifecycle=running bypass=off"));
        assert!(output.contains("ok bypass=on reason=manual\n"));
        assert!(output.contains("status lifecycle=running bypass=on reason=manual"));
        assert!(output.contains("ok bypass=off reason=none\n"));
        assert!(output.contains("error invalid-command\n"));
        assert!(output.ends_with("ok shutdown lifecycle=stopping\n"));
        assert!(!output.contains("heartbeat=2"));
    }
}
