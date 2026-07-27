use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use fm_protocol::{
    CodecError, CommandPayload, CommandResult, DurableGap, ErrorMessage, EventMessage,
    HandshakeOutcome, LineDecoder, RuntimeEventMessage, WireMessage, encode_line,
};

use crate::{Client, ClientError, CommandStatus, Intake, Outbound, ReconnectBackoff, SyncMode};

const READ_BUFFER_BYTES: usize = 8 * 1024;

#[derive(Debug)]
struct ReceiveStatus {
    message: Option<WireMessage>,
    timed_out: bool,
}

/// An error from the raw newline-delimited TCP connection.
#[derive(Debug)]
pub enum TcpConnectionError {
    Io(io::Error),
    Codec(CodecError),
}

impl fmt::Display for TcpConnectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Codec(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for TcpConnectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Codec(error) => Some(error),
        }
    }
}

impl From<io::Error> for TcpConnectionError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<CodecError> for TcpConnectionError {
    fn from(value: CodecError) -> Self {
        Self::Codec(value)
    }
}

/// A synchronous raw-TCP connection with separate socket handles for reads and
/// writes. Decoding is bounded by `fm_protocol`'s line and batch limits.
#[derive(Debug)]
pub struct TcpConnection {
    reader: TcpStream,
    writer: TcpStream,
    decoder: LineDecoder,
    decoded: VecDeque<WireMessage>,
}

impl TcpConnection {
    /// Connects to one concrete address within the supplied timeout.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the connection or socket setup fails.
    pub fn connect(
        address: SocketAddr,
        connect_timeout: Duration,
    ) -> Result<Self, TcpConnectionError> {
        let reader = TcpStream::connect_timeout(&address, connect_timeout)?;
        reader.set_nodelay(true)?;
        let writer = reader.try_clone()?;
        Ok(Self {
            reader,
            writer,
            decoder: LineDecoder::new(),
            decoded: VecDeque::new(),
        })
    }

    /// Returns the connected peer address.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the peer address is unavailable.
    pub fn peer_addr(&self) -> Result<SocketAddr, TcpConnectionError> {
        Ok(self.reader.peer_addr()?)
    }

    /// Writes one current protocol record. This never emits a legacy hello.
    ///
    /// # Errors
    ///
    /// Returns a codec error for an unencodable record or an I/O error when
    /// writing fails.
    pub fn send(&mut self, message: &WireMessage) -> Result<(), TcpConnectionError> {
        let encoded = encode_line(message)?;
        self.writer.write_all(encoded.as_bytes())?;
        Ok(())
    }

    /// Flushes records previously written with [`Self::send`].
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the socket cannot be flushed.
    pub fn flush(&mut self) -> Result<(), TcpConnectionError> {
        self.writer.flush()?;
        Ok(())
    }

    /// Reads one decoded message, or `None` for a clean EOF.
    ///
    /// # Errors
    ///
    /// Returns an I/O error for a socket failure or a codec error for an
    /// invalid, oversized, or incomplete protocol record.
    pub fn receive(&mut self) -> Result<Option<WireMessage>, TcpConnectionError> {
        self.reader.set_read_timeout(None)?;
        let status = self.receive_until(None)?;
        debug_assert!(!status.timed_out, "blocking receive cannot time out");
        Ok(status.message)
    }

    fn receive_timeout(&mut self, timeout: Duration) -> Result<ReceiveStatus, TcpConnectionError> {
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "TCP receive timeout is too large",
            )
        })?;
        let result = self.receive_until(Some(deadline));
        let reset = self.reader.set_read_timeout(None);
        match (result, reset) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error.into()),
            (Ok(status), Ok(())) => Ok(status),
        }
    }

    fn receive_until(
        &mut self,
        deadline: Option<Instant>,
    ) -> Result<ReceiveStatus, TcpConnectionError> {
        loop {
            if let Some(message) = self.decoded.pop_front() {
                return Ok(ReceiveStatus {
                    message: Some(message),
                    timed_out: false,
                });
            }

            if let Some(deadline) = deadline {
                let timeout = deadline.saturating_duration_since(Instant::now());
                if timeout.is_zero() {
                    return Ok(ReceiveStatus {
                        message: None,
                        timed_out: true,
                    });
                }
                self.reader.set_read_timeout(Some(timeout))?;
            }

            let mut buffer = [0_u8; READ_BUFFER_BYTES];
            let read = match self.reader.read(&mut buffer) {
                Ok(read) => read,
                Err(error)
                    if deadline.is_some()
                        && matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) =>
                {
                    return Ok(ReceiveStatus {
                        message: None,
                        timed_out: true,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            };
            if read == 0 {
                let decoder = std::mem::take(&mut self.decoder);
                decoder.finish()?;
                return Ok(ReceiveStatus {
                    message: None,
                    timed_out: false,
                });
            }
            self.decoded.extend(self.decoder.push(&buffer[..read])?);
        }
    }

    fn shutdown(&self) {
        let _ = self.reader.shutdown(Shutdown::Both);
    }
}

/// Why an established or attempted TCP connection ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisconnectCause {
    Eof,
    Io(io::ErrorKind),
}

/// A state-bearing record received from an established session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEvent {
    Connected {
        mode: SyncMode,
    },
    Event {
        event: EventMessage,
        intake: Intake,
    },
    RuntimeEvent {
        event: RuntimeEventMessage,
        intake: Intake,
    },
    CommandResult {
        result: CommandResult,
        intake: Intake,
    },
    DurableGap {
        gap: DurableGap,
    },
    ServerError(ErrorMessage),
    Disconnected {
        cause: DisconnectCause,
        backoff: ReconnectBackoff,
    },
}

/// Session setup, protocol, or transport failure.
#[derive(Debug)]
pub enum TcpSessionError {
    Client(ClientError),
    Codec(CodecError),
    Disconnected {
        cause: DisconnectCause,
        backoff: ReconnectBackoff,
    },
    ExpectedHandshake,
    UnexpectedMessage,
    NotConnected,
    AlreadyConnected,
    Cancelled {
        backoff: ReconnectBackoff,
    },
}

impl fmt::Display for TcpSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(error) => error.fmt(formatter),
            Self::Codec(error) => error.fmt(formatter),
            Self::Disconnected { cause, backoff } => write!(
                formatter,
                "TCP session disconnected ({cause:?}); reconnect attempt {} after {} ms",
                backoff.attempt, backoff.delay_ms
            ),
            Self::ExpectedHandshake => {
                formatter.write_str("first server record must be handshake_response")
            }
            Self::UnexpectedMessage => formatter.write_str("unexpected TCP session message"),
            Self::NotConnected => formatter.write_str("TCP session is not connected"),
            Self::AlreadyConnected => formatter.write_str("TCP session is already connected"),
            Self::Cancelled { backoff } => write!(
                formatter,
                "TCP session wait cancelled; reconnect attempt {} after {} ms",
                backoff.attempt, backoff.delay_ms
            ),
        }
    }
}

impl std::error::Error for TcpSessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Client(error) => Some(error),
            Self::Codec(error) => Some(error),
            Self::Disconnected { .. }
            | Self::ExpectedHandshake
            | Self::UnexpectedMessage
            | Self::NotConnected
            | Self::AlreadyConnected
            | Self::Cancelled { .. } => None,
        }
    }
}

impl From<ClientError> for TcpSessionError {
    fn from(value: ClientError) -> Self {
        Self::Client(value)
    }
}

/// A bounded synchronous freemixd session. It owns the transport-independent
/// client and only exists when the opt-in `std-tcp` feature is enabled.
#[derive(Debug)]
pub struct TcpSession {
    client: Client,
    connection: Option<TcpConnection>,
    sent_commands: VecDeque<String>,
}

impl TcpSession {
    #[must_use]
    pub const fn new(client: Client) -> Self {
        Self {
            client,
            connection: None,
            sent_commands: VecDeque::new(),
        }
    }

    #[must_use]
    pub const fn client(&self) -> &Client {
        &self.client
    }

    #[must_use]
    pub const fn client_mut(&mut self) -> &mut Client {
        &mut self.client
    }

    #[must_use]
    pub const fn connection(&self) -> Option<&TcpConnection> {
        self.connection.as_ref()
    }

    #[must_use]
    pub fn in_flight_len(&self) -> usize {
        self.sent_commands.len()
    }

    #[must_use]
    pub fn reconnect_backoff(&self) -> Option<ReconnectBackoff> {
        match self.client.state() {
            crate::ConnectionState::Backoff(backoff) => Some(*backoff),
            _ => None,
        }
    }

    #[must_use]
    pub fn into_client(self) -> Client {
        self.client
    }

    /// Connects, sends `HandshakeRequest`, and consumes snapshot or resume
    /// records until the owned client reaches `Ready`.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid client state, connection failure, malformed
    /// protocol data, an invalid handshake, or failed synchronization.
    pub fn connect(
        &mut self,
        address: SocketAddr,
        connect_timeout: Duration,
    ) -> Result<SessionEvent, TcpSessionError> {
        self.connect_with(address, connect_timeout, Self::read_wire)
    }

    /// Connects and synchronizes while polling for caller-requested cancellation.
    /// A cancelled wait closes the transport and enters reconnect backoff. Partial
    /// framed reads remain intact between polls until completion or cancellation.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::connect`], or
    /// [`TcpSessionError::Cancelled`] when `cancelled` returns `true`.
    pub fn connect_cancellable(
        &mut self,
        address: SocketAddr,
        connect_timeout: Duration,
        poll_interval: Duration,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<SessionEvent, TcpSessionError> {
        self.connect_with(address, connect_timeout, |session| {
            session.read_wire_cancellable(poll_interval, &mut cancelled)
        })
    }

    fn connect_with(
        &mut self,
        address: SocketAddr,
        connect_timeout: Duration,
        mut read: impl FnMut(&mut Self) -> Result<Option<WireMessage>, TcpSessionError>,
    ) -> Result<SessionEvent, TcpSessionError> {
        if self.connection.is_some() {
            return Err(TcpSessionError::AlreadyConnected);
        }
        self.client.start_connect()?;
        let connection = match TcpConnection::connect(address, connect_timeout) {
            Ok(connection) => connection,
            Err(TcpConnectionError::Io(error)) => {
                return Err(self.disconnected(DisconnectCause::Io(error.kind())));
            }
            Err(TcpConnectionError::Codec(error)) => return Err(TcpSessionError::Codec(error)),
        };
        self.connection = Some(connection);

        let request = self.client.transport_connected()?;
        self.send_wire(&WireMessage::HandshakeRequest(request))?;
        self.flush_wire()?;

        let Some(first) = read(self)? else {
            return Err(self.disconnected(DisconnectCause::Eof));
        };
        let WireMessage::HandshakeResponse(response) = first else {
            self.transition_disconnect();
            return Err(TcpSessionError::ExpectedHandshake);
        };
        let mode = match response.outcome {
            HandshakeOutcome::Snapshot { .. } | HandshakeOutcome::Rejected { .. } => {
                SyncMode::Snapshot
            }
            HandshakeOutcome::Resume { .. } => SyncMode::Resume,
        };
        if let Err(error) = self.client.intake(WireMessage::HandshakeResponse(response)) {
            self.transition_disconnect();
            return Err(TcpSessionError::Client(error));
        }

        while self.client.state() != &crate::ConnectionState::Ready {
            let Some(message) = read(self)? else {
                return Err(self.disconnected(DisconnectCause::Eof));
            };
            if let Err(error) = self.client.intake(message) {
                self.transition_disconnect();
                return Err(TcpSessionError::Client(error));
            }
        }

        self.retry_unresolved()?;
        Ok(SessionEvent::Connected { mode })
    }

    /// Queues a command in the owned transport-independent client.
    ///
    /// # Errors
    ///
    /// Returns an error when the client is not ready or its bounded queue,
    /// command ID space, or idempotency constraints reject the command.
    pub fn queue_command(
        &mut self,
        payload: CommandPayload,
        idempotency_key: impl Into<String>,
        expected_revision: Option<u64>,
        deadline_ms: Option<u64>,
    ) -> Result<fm_protocol::CommandMessage, TcpSessionError> {
        Ok(self
            .client
            .queue_command(payload, idempotency_key, expected_revision, deadline_ms)?)
    }

    /// Queues a heartbeat carrying the client's latest durable cursor.
    ///
    /// # Errors
    ///
    /// Returns an error when there is no negotiated session, the queue is
    /// full, or the heartbeat sequence is exhausted.
    pub fn queue_heartbeat(
        &mut self,
        sent_at_ms: u64,
    ) -> Result<fm_protocol::HeartbeatMessage, TcpSessionError> {
        Ok(self.client.queue_heartbeat(sent_at_ms)?)
    }

    /// Flushes queued commands and heartbeats in order. Commands are retained
    /// as in-flight before their first write so uncertain writes can be retried.
    ///
    /// # Errors
    ///
    /// Returns an error when disconnected or when encoding, writing, or
    /// flushing a record fails.
    pub fn flush(&mut self) -> Result<usize, TcpSessionError> {
        if self.connection.is_none() {
            return Err(TcpSessionError::NotConnected);
        }

        let mut flushed = 0;
        while self.sent_commands.len() < self.client.outbound_capacity() {
            let Some(outbound) = self.client.pop_outbound() else {
                break;
            };
            let message = match outbound {
                Outbound::Command(command) => {
                    if matches!(
                        self.client
                            .command(&command.id)
                            .map(|record| &record.status),
                        Some(CommandStatus::Completed(_))
                    ) {
                        continue;
                    }
                    if !self.sent_commands.iter().any(|id| id == &command.id) {
                        self.sent_commands.push_back(command.id.clone());
                    }
                    WireMessage::Command(command)
                }
                Outbound::Heartbeat(heartbeat) => WireMessage::Heartbeat(heartbeat),
            };
            self.send_wire(&message)?;
            flushed += 1;
        }
        self.flush_wire()?;
        Ok(flushed)
    }

    /// Blocks for the next typed post-handshake session event.
    ///
    /// # Errors
    ///
    /// Returns an error when disconnected, decoding or I/O fails, the server
    /// sends an unexpected record, or the owned client rejects a record.
    pub fn receive(&mut self) -> Result<SessionEvent, TcpSessionError> {
        let message = self.read_wire()?;
        self.handle_received(message)
    }

    /// Waits for one post-handshake event while polling for cancellation.
    /// Cancellation closes the current transport and enters reconnect backoff.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::receive`], or
    /// [`TcpSessionError::Cancelled`] when `cancelled` returns `true`.
    pub fn receive_cancellable(
        &mut self,
        poll_interval: Duration,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<SessionEvent, TcpSessionError> {
        let message = self.read_wire_cancellable(poll_interval, &mut cancelled)?;
        self.handle_received(message)
    }

    fn handle_received(
        &mut self,
        message: Option<WireMessage>,
    ) -> Result<SessionEvent, TcpSessionError> {
        let Some(message) = message else {
            let backoff = self.transition_disconnect();
            return Ok(SessionEvent::Disconnected {
                cause: DisconnectCause::Eof,
                backoff,
            });
        };

        match message {
            WireMessage::Event(event) => {
                let intake = self.client.intake(WireMessage::Event(event.clone()))?;
                Ok(SessionEvent::Event { event, intake })
            }
            WireMessage::RuntimeEvent(event) => {
                let intake = self
                    .client
                    .intake(WireMessage::RuntimeEvent(event.clone()))?;
                Ok(SessionEvent::RuntimeEvent { event, intake })
            }
            WireMessage::CommandResult(result) => {
                let intake = self
                    .client
                    .intake(WireMessage::CommandResult(result.clone()))?;
                let id = result_id(&result);
                self.sent_commands.retain(|sent| sent != id);
                Ok(SessionEvent::CommandResult { result, intake })
            }
            WireMessage::DurableGap(gap) => {
                let result = self.client.intake(WireMessage::DurableGap(gap.clone()));
                if let Err(error) = result {
                    self.transition_disconnect();
                    if matches!(error, ClientError::ResyncRequired { .. }) {
                        return Ok(SessionEvent::DurableGap { gap });
                    }
                    return Err(TcpSessionError::Client(error));
                }
                Ok(SessionEvent::DurableGap { gap })
            }
            WireMessage::Error(error) => Ok(SessionEvent::ServerError(error)),
            WireMessage::ClientHello(_)
            | WireMessage::ServerHello(_)
            | WireMessage::Command(_)
            | WireMessage::Snapshot(_)
            | WireMessage::HandshakeRequest(_)
            | WireMessage::HandshakeResponse(_)
            | WireMessage::DurableEventBatch(_)
            | WireMessage::Heartbeat(_)
            | WireMessage::CapabilityReport(_) => {
                self.transition_disconnect();
                Err(TcpSessionError::UnexpectedMessage)
            }
        }
    }

    /// Closes an established session and enters backoff once.
    pub fn disconnect(&mut self) -> Option<ReconnectBackoff> {
        self.connection.as_ref()?.shutdown();
        Some(self.transition_disconnect())
    }

    fn retry_unresolved(&mut self) -> Result<(), TcpSessionError> {
        let ids = self.sent_commands.clone();
        for id in ids {
            let Some(record) = self.client.command(&id) else {
                self.sent_commands.retain(|sent| sent != &id);
                continue;
            };
            if matches!(record.status, CommandStatus::Completed(_)) {
                self.sent_commands.retain(|sent| sent != &id);
                continue;
            }
            let message = WireMessage::Command(record.command.clone());
            self.send_wire(&message)?;
        }
        self.flush_wire()
    }

    fn send_wire(&mut self, message: &WireMessage) -> Result<(), TcpSessionError> {
        let result = self
            .connection
            .as_mut()
            .ok_or(TcpSessionError::NotConnected)?
            .send(message);
        match result {
            Ok(()) => Ok(()),
            Err(TcpConnectionError::Io(error)) => {
                Err(self.disconnected(DisconnectCause::Io(error.kind())))
            }
            Err(TcpConnectionError::Codec(error)) => Err(TcpSessionError::Codec(error)),
        }
    }

    fn flush_wire(&mut self) -> Result<(), TcpSessionError> {
        let result = self
            .connection
            .as_mut()
            .ok_or(TcpSessionError::NotConnected)?
            .flush();
        match result {
            Ok(()) => Ok(()),
            Err(TcpConnectionError::Io(error)) => {
                Err(self.disconnected(DisconnectCause::Io(error.kind())))
            }
            Err(TcpConnectionError::Codec(error)) => Err(TcpSessionError::Codec(error)),
        }
    }

    fn read_wire(&mut self) -> Result<Option<WireMessage>, TcpSessionError> {
        let result = self
            .connection
            .as_mut()
            .ok_or(TcpSessionError::NotConnected)?
            .receive();
        match result {
            Ok(message) => Ok(message),
            Err(TcpConnectionError::Io(error)) => {
                Err(self.disconnected(DisconnectCause::Io(error.kind())))
            }
            Err(TcpConnectionError::Codec(error)) => {
                self.transition_disconnect();
                Err(TcpSessionError::Codec(error))
            }
        }
    }

    fn read_wire_cancellable(
        &mut self,
        poll_interval: Duration,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<Option<WireMessage>, TcpSessionError> {
        loop {
            if cancelled() {
                let backoff = self.transition_disconnect();
                return Err(TcpSessionError::Cancelled { backoff });
            }
            let result = self
                .connection
                .as_mut()
                .ok_or(TcpSessionError::NotConnected)?
                .receive_timeout(poll_interval);
            match result {
                Ok(ReceiveStatus {
                    message,
                    timed_out: false,
                }) => return Ok(message),
                Ok(ReceiveStatus {
                    timed_out: true, ..
                }) => {}
                Err(TcpConnectionError::Io(error)) => {
                    return Err(self.disconnected(DisconnectCause::Io(error.kind())));
                }
                Err(TcpConnectionError::Codec(error)) => {
                    self.transition_disconnect();
                    return Err(TcpSessionError::Codec(error));
                }
            }
        }
    }

    fn disconnected(&mut self, cause: DisconnectCause) -> TcpSessionError {
        let backoff = self.transition_disconnect();
        TcpSessionError::Disconnected { cause, backoff }
    }

    fn transition_disconnect(&mut self) -> ReconnectBackoff {
        self.connection.take();
        self.client.transport_disconnected()
    }
}

fn result_id(result: &CommandResult) -> &str {
    match result {
        CommandResult::Accepted { id, .. } | CommandResult::Rejected { id, .. } => id,
    }
}
