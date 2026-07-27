use std::{
    error::Error,
    fmt,
    io::{self, BufRead, BufReader},
    net::SocketAddr,
    num::NonZeroU128,
    process::{Child, ChildStdout, Command, ExitStatus, Stdio},
    str::FromStr,
    thread::{self, JoinHandle},
    time::Duration,
};

use fm_types::ProjectId;

use crate::SupervisedConfig;

const READY_PREFIX: &str = "FREEMIXD_READY";
const READY_VERSION: u8 = 1;
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartPolicy {
    pub maximum_restarts: u8,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            maximum_restarts: 3,
        }
    }
}

/// The versioned record emitted after freemixd has bound its TCP listener.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadinessRecord {
    pub version: u8,
    pub address: SocketAddr,
    pub project_id: ProjectId,
}

impl FromStr for ReadinessRecord {
    type Err = ReadinessParseError;

    fn from_str(line: &str) -> Result<Self, Self::Err> {
        let mut fields = line.trim_end_matches(['\r', '\n']).split('\t');
        if fields.next() != Some(READY_PREFIX) {
            return Err(ReadinessParseError);
        }
        let version = fields
            .next()
            .and_then(|field| field.strip_prefix("v="))
            .and_then(|value| value.parse().ok())
            .ok_or(ReadinessParseError)?;
        let address = fields
            .next()
            .and_then(|field| field.strip_prefix("address="))
            .and_then(|value| value.parse().ok())
            .ok_or(ReadinessParseError)?;
        let project_id = fields
            .next()
            .and_then(|field| field.strip_prefix("project_id="))
            .and_then(|value| value.parse::<NonZeroU128>().ok())
            .map(ProjectId::new)
            .ok_or(ReadinessParseError)?;
        if fields.next().is_some() || version != READY_VERSION {
            return Err(ReadinessParseError);
        }
        Ok(Self {
            version,
            address,
            project_id,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadinessParseError;

impl fmt::Display for ReadinessParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid freemixd readiness record")
    }
}

impl Error for ReadinessParseError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorState {
    Launching,
    Ready(ReadinessRecord),
    Exited { code: Option<i32> },
    Failed,
    RestartLimitReached,
}

#[derive(Debug)]
pub enum SupervisorError {
    NonLoopbackConfigured(SocketAddr),
    Spawn(io::Error),
    MissingStdout,
    MissingReadiness,
    ReadinessCancelled,
    ReadinessIo(io::Error),
    InvalidReadiness(ReadinessParseError),
    ExitedBeforeReady {
        status: ExitStatus,
    },
    NonLoopbackReadiness(SocketAddr),
    ProjectIdentityChanged {
        expected: ProjectId,
        received: ProjectId,
    },
    RestartLimitReached {
        maximum_restarts: u8,
    },
    Process(io::Error),
}

impl fmt::Display for SupervisorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonLoopbackConfigured(address) => write!(
                formatter,
                "supervised listen address must be loopback, got {address}"
            ),
            Self::Spawn(error) => write!(formatter, "failed to launch freemixd: {error}"),
            Self::MissingStdout => formatter.write_str("launched freemixd has no captured stdout"),
            Self::MissingReadiness => {
                formatter.write_str("freemixd supervisor has no readiness record")
            }
            Self::ReadinessCancelled => formatter.write_str("freemixd readiness wait cancelled"),
            Self::ReadinessIo(error) => {
                write!(formatter, "failed to read freemixd readiness: {error}")
            }
            Self::InvalidReadiness(error) => error.fmt(formatter),
            Self::ExitedBeforeReady { status } => {
                write!(formatter, "freemixd exited before readiness with {status}")
            }
            Self::NonLoopbackReadiness(address) => write!(
                formatter,
                "freemixd announced non-loopback address {address}"
            ),
            Self::ProjectIdentityChanged { expected, received } => write!(
                formatter,
                "freemixd project identity changed from {expected} to {received}"
            ),
            Self::RestartLimitReached { maximum_restarts } => write!(
                formatter,
                "freemixd restart limit of {maximum_restarts} reached"
            ),
            Self::Process(error) => write!(formatter, "failed to manage freemixd process: {error}"),
        }
    }
}

impl Error for SupervisorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn(error) | Self::ReadinessIo(error) | Self::Process(error) => Some(error),
            Self::InvalidReadiness(error) => Some(error),
            _ => None,
        }
    }
}

/// Owns a freemixd child and its readiness stdout for the Studio lifetime.
#[derive(Debug)]
pub struct DaemonSupervisor {
    config: SupervisedConfig,
    restart_policy: RestartPolicy,
    child: Option<Child>,
    stdout: Option<BufReader<ChildStdout>>,
    stable_project_id: Option<ProjectId>,
    restarts: u8,
    state: SupervisorState,
}

impl DaemonSupervisor {
    /// Launches exactly `freemixd serve <project-path> --listen <addr>` and waits
    /// for its stdout readiness record.
    ///
    /// # Errors
    ///
    /// Rejects invalid configuration, process failures, malformed readiness,
    /// non-loopback announcements, and unstable project identity.
    pub fn launch(
        config: SupervisedConfig,
        restart_policy: RestartPolicy,
    ) -> Result<Self, SupervisorError> {
        Self::launch_cancellable(config, restart_policy, READINESS_POLL_INTERVAL, || false)
    }

    /// Launches freemixd and polls its readiness wait for cancellation.
    /// Cancellation terminates and reaps the child before returning.
    ///
    /// # Errors
    ///
    /// Returns [`SupervisorError::ReadinessCancelled`] when `cancelled`
    /// returns `true`, in addition to the errors from [`Self::launch`].
    pub fn launch_cancellable(
        config: SupervisedConfig,
        restart_policy: RestartPolicy,
        poll_interval: Duration,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<Self, SupervisorError> {
        if !config.listen.ip().is_loopback() {
            return Err(SupervisorError::NonLoopbackConfigured(config.listen));
        }
        let mut supervisor = Self {
            config,
            restart_policy,
            child: None,
            stdout: None,
            stable_project_id: None,
            restarts: 0,
            state: SupervisorState::Launching,
        };
        supervisor.spawn_cancellable(poll_interval, &mut cancelled)?;
        Ok(supervisor)
    }

    #[must_use]
    pub const fn state(&self) -> SupervisorState {
        self.state
    }

    #[must_use]
    pub const fn readiness(&self) -> Option<ReadinessRecord> {
        match self.state {
            SupervisorState::Ready(readiness) => Some(readiness),
            _ => None,
        }
    }

    #[must_use]
    pub const fn restart_count(&self) -> u8 {
        self.restarts
    }

    /// Non-blockingly refreshes child exit state. `try_wait` also reaps an exited child.
    ///
    /// # Errors
    ///
    /// Returns an operating-system process error.
    pub fn poll(&mut self) -> Result<SupervisorState, SupervisorError> {
        if let Some(child) = &mut self.child
            && let Some(status) = child.try_wait().map_err(SupervisorError::Process)?
        {
            self.child.take();
            self.stdout.take();
            self.state = SupervisorState::Exited {
                code: status.code(),
            };
        }
        Ok(self.state)
    }

    /// Waits for the current child to exit and reaps it. This is useful when an
    /// external event already establishes that shutdown is expected.
    ///
    /// # Errors
    ///
    /// Returns an operating-system process error.
    pub fn wait_for_exit(&mut self) -> Result<SupervisorState, SupervisorError> {
        let Some(mut child) = self.child.take() else {
            return Ok(self.state);
        };
        let status = child.wait().map_err(SupervisorError::Process)?;
        self.stdout.take();
        self.state = SupervisorState::Exited {
            code: status.code(),
        };
        Ok(self.state)
    }

    /// Terminates and reaps the current child, then performs one bounded restart.
    /// No delay is imposed; the caller owns restart timing.
    ///
    /// # Errors
    ///
    /// Returns an error after the configured restart budget or on process/readiness failure.
    pub fn restart(&mut self) -> Result<ReadinessRecord, SupervisorError> {
        self.restart_cancellable(READINESS_POLL_INTERVAL, || false)
    }

    /// Terminates the current child and performs one restart with cancellable readiness.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::restart`], including readiness cancellation.
    pub fn restart_cancellable(
        &mut self,
        poll_interval: Duration,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<ReadinessRecord, SupervisorError> {
        if self.restarts >= self.restart_policy.maximum_restarts {
            self.terminate_and_reap()?;
            self.state = SupervisorState::RestartLimitReached;
            return Err(SupervisorError::RestartLimitReached {
                maximum_restarts: self.restart_policy.maximum_restarts,
            });
        }
        self.terminate_and_reap()?;
        self.restarts += 1;
        self.spawn_cancellable(poll_interval, &mut cancelled)
            .inspect_err(|_| self.state = SupervisorState::Failed)
    }

    /// Stops and reaps the child without consuming the supervisor.
    ///
    /// # Errors
    ///
    /// Returns an operating-system process error.
    pub fn shutdown(&mut self) -> Result<(), SupervisorError> {
        self.terminate_and_reap()?;
        self.state = SupervisorState::Exited { code: None };
        Ok(())
    }

    fn spawn_cancellable(
        &mut self,
        poll_interval: Duration,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<ReadinessRecord, SupervisorError> {
        self.state = SupervisorState::Launching;
        if cancelled() {
            return Err(SupervisorError::ReadinessCancelled);
        }
        let mut child = Command::new(&self.config.daemon_executable)
            .arg("serve")
            .arg(&self.config.project_bundle)
            .arg("--listen")
            .arg(self.config.listen.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(SupervisorError::Spawn)?;
        let Some(stdout) = child.stdout.take() else {
            reap_local_child(&mut child);
            return Err(SupervisorError::MissingStdout);
        };
        let reader = match spawn_readiness_reader(stdout) {
            Ok(reader) => reader,
            Err(error) => {
                reap_local_child(&mut child);
                return Err(SupervisorError::ReadinessIo(error));
            }
        };
        while !reader.is_finished() {
            if cancelled() {
                reap_local_child(&mut child);
                drop(join_readiness_reader(reader)?);
                return Err(SupervisorError::ReadinessCancelled);
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    drop(join_readiness_reader(reader)?);
                    return Err(SupervisorError::ExitedBeforeReady { status });
                }
                Ok(None) => {}
                Err(error) => {
                    reap_local_child(&mut child);
                    drop(join_readiness_reader(reader)?);
                    return Err(SupervisorError::Process(error));
                }
            }
            thread::sleep(poll_interval);
        }
        let read = match join_readiness_reader(reader) {
            Ok(read) => read,
            Err(error) => {
                reap_local_child(&mut child);
                return Err(error);
            }
        };
        let (stdout, line, bytes) = match read {
            (stdout, line, Ok(bytes)) => (stdout, line, bytes),
            (_, _, Err(error)) => {
                reap_local_child(&mut child);
                return Err(SupervisorError::ReadinessIo(error));
            }
        };
        if bytes == 0 {
            return match child.try_wait() {
                Ok(Some(status)) => Err(SupervisorError::ExitedBeforeReady { status }),
                Ok(None) => {
                    reap_local_child(&mut child);
                    Err(SupervisorError::MissingReadiness)
                }
                Err(error) => {
                    reap_local_child(&mut child);
                    Err(SupervisorError::Process(error))
                }
            };
        }
        let readiness = match line.parse::<ReadinessRecord>() {
            Ok(readiness) => readiness,
            Err(error) => {
                reap_local_child(&mut child);
                return Err(SupervisorError::InvalidReadiness(error));
            }
        };
        if !readiness.address.ip().is_loopback() {
            reap_local_child(&mut child);
            return Err(SupervisorError::NonLoopbackReadiness(readiness.address));
        }
        if let Some(expected) = self.stable_project_id
            && readiness.project_id != expected
        {
            reap_local_child(&mut child);
            return Err(SupervisorError::ProjectIdentityChanged {
                expected,
                received: readiness.project_id,
            });
        }

        self.stable_project_id = Some(readiness.project_id);
        self.child = Some(child);
        self.stdout = Some(stdout);
        self.state = SupervisorState::Ready(readiness);
        Ok(readiness)
    }

    fn terminate_and_reap(&mut self) -> Result<(), SupervisorError> {
        self.stdout.take();
        if let Some(mut child) = self.child.take()
            && child
                .try_wait()
                .map_err(SupervisorError::Process)?
                .is_none()
        {
            let kill = child.kill();
            let wait = child.wait();
            kill.map_err(SupervisorError::Process)?;
            wait.map_err(SupervisorError::Process)?;
        }
        Ok(())
    }
}

type ReadinessReader = JoinHandle<(BufReader<ChildStdout>, String, io::Result<usize>)>;

fn spawn_readiness_reader(stdout: ChildStdout) -> io::Result<ReadinessReader> {
    thread::Builder::new()
        .name("freemixd-readiness".to_owned())
        .spawn(move || {
            let mut stdout = BufReader::new(stdout);
            let mut line = String::new();
            let result = stdout.read_line(&mut line);
            (stdout, line, result)
        })
}

fn join_readiness_reader(
    reader: ReadinessReader,
) -> Result<(BufReader<ChildStdout>, String, io::Result<usize>), SupervisorError> {
    reader.join().map_err(|_| {
        SupervisorError::ReadinessIo(io::Error::other("freemixd readiness reader panicked"))
    })
}

impl Drop for DaemonSupervisor {
    fn drop(&mut self) {
        let _ = self.terminate_and_reap();
    }
}

fn reap_local_child(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
}
