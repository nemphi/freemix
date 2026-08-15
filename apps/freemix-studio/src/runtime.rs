use std::{error::Error, fmt, net::SocketAddr, time::Duration};

use fm_client::{
    Client, ConnectionState, ReconnectBackoff, SessionEvent, TcpSession, TcpSessionError,
};
use fm_protocol::{CommandMessage, CommandPayload, HeartbeatMessage};

use crate::{
    ConnectionConfig, DaemonSupervisor, ReadinessRecord, StudioConfig, SupervisorError,
    SupervisorState, native_client_config,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleState {
    LaunchingDaemon,
    DaemonExited { code: Option<i32> },
    DaemonFailed,
    RestartLimitReached,
    Disconnected,
    Connecting,
    Synchronizing,
    Ready,
    Backoff(ReconnectBackoff),
    ResyncRequired,
    ProtocolMismatch,
}

#[derive(Debug)]
pub enum StudioError {
    Supervisor(SupervisorError),
    Client(fm_client::ClientError),
    Session(TcpSessionError),
    BackoffNotElapsed {
        required: Duration,
        supplied: Duration,
    },
    NoSupervisedDaemon,
}

impl fmt::Display for StudioError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Supervisor(error) => error.fmt(formatter),
            Self::Client(error) => error.fmt(formatter),
            Self::Session(error) => error.fmt(formatter),
            Self::BackoffNotElapsed { required, supplied } => write!(
                formatter,
                "reconnect backoff not elapsed: required {required:?}, supplied {supplied:?}"
            ),
            Self::NoSupervisedDaemon => {
                formatter.write_str("runtime does not own a supervised daemon")
            }
        }
    }
}

impl Error for StudioError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Supervisor(error) => Some(error),
            Self::Client(error) => Some(error),
            Self::Session(error) => Some(error),
            Self::BackoffNotElapsed { .. } | Self::NoSupervisedDaemon => None,
        }
    }
}

impl From<SupervisorError> for StudioError {
    fn from(value: SupervisorError) -> Self {
        Self::Supervisor(value)
    }
}

impl From<fm_client::ClientError> for StudioError {
    fn from(value: fm_client::ClientError) -> Self {
        Self::Client(value)
    }
}

impl From<TcpSessionError> for StudioError {
    fn from(value: TcpSessionError) -> Self {
        Self::Session(value)
    }
}

/// A synchronous Studio control runtime. All clocks and waits are caller-owned.
#[derive(Debug)]
pub struct StudioRuntime {
    supervisor: Option<DaemonSupervisor>,
    address: SocketAddr,
    session: TcpSession,
}

impl StudioRuntime {
    /// Creates a disconnected runtime, launching a supervised daemon when requested.
    ///
    /// # Errors
    ///
    /// Returns an error for daemon startup/readiness or invalid client configuration.
    pub fn new(config: StudioConfig) -> Result<Self, StudioError> {
        Self::new_cancellable(config, Duration::from_millis(50), || false)
    }

    /// Creates a disconnected runtime with cancellable supervised readiness.
    ///
    /// # Errors
    ///
    /// Returns startup, readiness, cancellation, or client configuration errors.
    pub fn new_cancellable(
        config: StudioConfig,
        poll_interval: Duration,
        cancelled: impl FnMut() -> bool,
    ) -> Result<Self, StudioError> {
        let (supervisor, address, project_id) = match config.connection {
            ConnectionConfig::Supervised(supervised) => {
                let supervisor = DaemonSupervisor::launch_cancellable(
                    supervised,
                    config.restart_policy,
                    poll_interval,
                    cancelled,
                )?;
                let readiness = supervisor
                    .readiness()
                    .ok_or(StudioError::Supervisor(SupervisorError::MissingReadiness))?;
                (Some(supervisor), readiness.address, readiness.project_id)
            }
            ConnectionConfig::Existing(existing) => {
                (None, existing.address, existing.expected_project_id)
            }
        };
        let client = Client::new(native_client_config(
            config.desired_role,
            config.client_id,
            project_id,
        ))?;
        Ok(Self {
            supervisor,
            address,
            session: TcpSession::new(client),
        })
    }

    #[must_use]
    pub const fn session(&self) -> &TcpSession {
        &self.session
    }

    #[must_use]
    pub const fn session_mut(&mut self) -> &mut TcpSession {
        &mut self.session
    }

    #[must_use]
    pub const fn supervisor(&self) -> Option<&DaemonSupervisor> {
        self.supervisor.as_ref()
    }

    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    /// Derives display lifecycle directly from process and protocol state.
    ///
    /// # Errors
    ///
    /// Returns an operating-system error while refreshing supervised process state.
    pub fn lifecycle(&mut self) -> Result<LifecycleState, StudioError> {
        if let Some(supervisor) = &mut self.supervisor {
            match supervisor.poll()? {
                SupervisorState::Launching => return Ok(LifecycleState::LaunchingDaemon),
                SupervisorState::Exited { code } => {
                    return Ok(LifecycleState::DaemonExited { code });
                }
                SupervisorState::Failed => return Ok(LifecycleState::DaemonFailed),
                SupervisorState::RestartLimitReached => {
                    return Ok(LifecycleState::RestartLimitReached);
                }
                SupervisorState::Ready(_) => {}
            }
        }
        Ok(match self.session.client().state() {
            ConnectionState::Disconnected => LifecycleState::Disconnected,
            ConnectionState::Connecting | ConnectionState::AwaitingHandshake => {
                LifecycleState::Connecting
            }
            ConnectionState::Synchronizing { .. } => LifecycleState::Synchronizing,
            ConnectionState::Ready => LifecycleState::Ready,
            ConnectionState::Backoff(backoff) => LifecycleState::Backoff(*backoff),
            ConnectionState::ResyncRequired { .. } => LifecycleState::ResyncRequired,
            ConnectionState::ProtocolMismatch { .. } => LifecycleState::ProtocolMismatch,
        })
    }

    /// Connects and synchronizes by snapshot or resume using the supplied timeout.
    ///
    /// # Errors
    ///
    /// Returns a transport, handshake, synchronization, or supervisor error.
    pub fn connect(&mut self, connect_timeout: Duration) -> Result<SessionEvent, StudioError> {
        if let Some(supervisor) = &mut self.supervisor {
            supervisor.poll()?;
        }
        Ok(self.session.connect(self.address(), connect_timeout)?)
    }

    /// Connects and synchronizes while polling a caller-owned cancellation source.
    ///
    /// # Errors
    ///
    /// Returns a transport, handshake, synchronization, cancellation, or supervisor error.
    pub fn connect_cancellable(
        &mut self,
        connect_timeout: Duration,
        poll_interval: Duration,
        cancelled: impl FnMut() -> bool,
    ) -> Result<SessionEvent, StudioError> {
        if let Some(supervisor) = &mut self.supervisor {
            supervisor.poll()?;
        }
        Ok(self.session.connect_cancellable(
            self.address(),
            connect_timeout,
            poll_interval,
            cancelled,
        )?)
    }

    /// Reconnects only after the caller reports that the client-selected backoff elapsed.
    /// If the owned daemon has exited, one bounded restart is performed first.
    ///
    /// # Errors
    ///
    /// Rejects insufficient elapsed backoff and propagates restart/session errors.
    pub fn reconnect(
        &mut self,
        elapsed_backoff: Duration,
        connect_timeout: Duration,
    ) -> Result<SessionEvent, StudioError> {
        if let Some(backoff) = self.session.reconnect_backoff() {
            let required = Duration::from_millis(backoff.delay_ms);
            if elapsed_backoff < required {
                return Err(StudioError::BackoffNotElapsed {
                    required,
                    supplied: elapsed_backoff,
                });
            }
        }
        if let Some(supervisor) = &mut self.supervisor
            && matches!(supervisor.poll()?, SupervisorState::Exited { .. })
        {
            self.address = supervisor.restart()?.address;
        }
        self.connect(connect_timeout)
    }

    /// Reconnects after backoff while polling a caller-owned cancellation source.
    ///
    /// # Errors
    ///
    /// Rejects insufficient elapsed backoff and propagates restart/session errors.
    pub fn reconnect_cancellable(
        &mut self,
        elapsed_backoff: Duration,
        connect_timeout: Duration,
        poll_interval: Duration,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<SessionEvent, StudioError> {
        if let Some(backoff) = self.session.reconnect_backoff() {
            let required = Duration::from_millis(backoff.delay_ms);
            if elapsed_backoff < required {
                return Err(StudioError::BackoffNotElapsed {
                    required,
                    supplied: elapsed_backoff,
                });
            }
        }
        if let Some(supervisor) = &mut self.supervisor
            && matches!(supervisor.poll()?, SupervisorState::Exited { .. })
        {
            self.address = supervisor
                .restart_cancellable(poll_interval, &mut cancelled)?
                .address;
        }
        self.connect_cancellable(connect_timeout, poll_interval, cancelled)
    }

    /// Explicitly restarts the owned daemon after disconnecting the session.
    /// The caller decides when this bounded restart is appropriate.
    ///
    /// # Errors
    ///
    /// Returns an error for existing mode, exhausted restart policy, or process/readiness failure.
    pub fn restart_daemon(&mut self) -> Result<ReadinessRecord, StudioError> {
        self.session.disconnect();
        let readiness = self
            .supervisor
            .as_mut()
            .ok_or(StudioError::NoSupervisedDaemon)?
            .restart()?;
        self.address = readiness.address;
        Ok(readiness)
    }

    /// Queues a durable command in the bounded client queue.
    ///
    /// # Errors
    ///
    /// Propagates client state, capacity, and idempotency errors.
    pub fn queue_command(
        &mut self,
        payload: CommandPayload,
        idempotency_key: impl Into<String>,
        expected_revision: Option<u64>,
        deadline_ms: Option<u64>,
    ) -> Result<CommandMessage, StudioError> {
        Ok(self
            .session
            .queue_command(payload, idempotency_key, expected_revision, deadline_ms)?)
    }

    /// Sends all currently queued commands and heartbeats.
    ///
    /// # Errors
    ///
    /// Propagates transport and codec errors.
    pub fn flush(&mut self) -> Result<usize, StudioError> {
        Ok(self.session.flush()?)
    }

    /// Sends queued records while polling writes for cancellation.
    ///
    /// # Errors
    ///
    /// Propagates transport, codec, and cancellation errors.
    pub fn flush_cancellable(
        &mut self,
        poll_interval: Duration,
        cancelled: impl FnMut() -> bool,
    ) -> Result<usize, StudioError> {
        Ok(self.session.flush_cancellable(poll_interval, cancelled)?)
    }

    /// Queues and immediately flushes a heartbeat using caller-supplied time.
    ///
    /// # Errors
    ///
    /// Propagates client, transport, and codec errors.
    pub fn send_heartbeat(&mut self, sent_at_ms: u64) -> Result<HeartbeatMessage, StudioError> {
        let heartbeat = self.session.queue_heartbeat(sent_at_ms)?;
        self.session.flush()?;
        Ok(heartbeat)
    }

    /// Queues a heartbeat and flushes it with cancellable writes.
    ///
    /// # Errors
    ///
    /// Propagates client, transport, codec, and cancellation errors.
    pub fn send_heartbeat_cancellable(
        &mut self,
        sent_at_ms: u64,
        poll_interval: Duration,
        cancelled: impl FnMut() -> bool,
    ) -> Result<HeartbeatMessage, StudioError> {
        let heartbeat = self.session.queue_heartbeat(sent_at_ms)?;
        self.session.flush_cancellable(poll_interval, cancelled)?;
        Ok(heartbeat)
    }

    /// Blocks for one durable event, runtime event, command result, server error, or EOF.
    ///
    /// # Errors
    ///
    /// Propagates transport, codec, and client intake errors.
    pub fn receive(&mut self) -> Result<SessionEvent, StudioError> {
        Ok(self.session.receive()?)
    }

    /// Waits for one session event while polling a caller-owned cancellation source.
    ///
    /// # Errors
    ///
    /// Propagates transport, codec, client intake, and cancellation errors.
    pub fn receive_cancellable(
        &mut self,
        poll_interval: Duration,
        cancelled: impl FnMut() -> bool,
    ) -> Result<SessionEvent, StudioError> {
        Ok(self.session.receive_cancellable(poll_interval, cancelled)?)
    }

    /// Sends one diagnostics request and waits for its validated response.
    pub fn send_diagnostics_cancellable(
        &mut self,
        request_id: impl Into<String>,
        poll_interval: Duration,
        cancelled: impl FnMut() -> bool,
    ) -> Result<SessionEvent, StudioError> {
        Ok(self
            .session
            .send_diagnostics_cancellable(request_id, poll_interval, cancelled)?)
    }
}
