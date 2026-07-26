use std::{collections::VecDeque, fmt, time::Duration};

use crate::DataValue;

/// Lifecycle state shared by polling and push sources.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceState {
    Stopped,
    Running,
}

/// A source value carrying a monotonic, source-local sequence number.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceEvent {
    pub sequence: u64,
    pub value: DataValue,
}

/// A lifecycle or deterministic adapter failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceError {
    AlreadyRunning,
    NotRunning,
    SequenceExhausted,
    Adapter(String),
}

impl fmt::Display for SourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyRunning => formatter.write_str("source is already running"),
            Self::NotRunning => formatter.write_str("source is not running"),
            Self::SequenceExhausted => formatter.write_str("source sequence is exhausted"),
            Self::Adapter(message) => write!(formatter, "adapter failed: {message}"),
        }
    }
}

impl std::error::Error for SourceError {}

/// Common lifecycle contract for all data sources.
pub trait DataSource {
    fn id(&self) -> &str;
    fn state(&self) -> SourceState;

    /// Starts delivery.
    ///
    /// # Errors
    ///
    /// Returns [`SourceError::AlreadyRunning`] when already started.
    fn start(&mut self) -> Result<(), SourceError>;

    /// Stops delivery without discarding queued values.
    ///
    /// # Errors
    ///
    /// Returns [`SourceError::NotRunning`] when already stopped.
    fn stop(&mut self) -> Result<(), SourceError>;
}

/// A source read at a caller-controlled interval.
pub trait PollingSource: DataSource {
    fn poll_interval(&self) -> Duration;

    /// Reads the next scripted polling result.
    ///
    /// # Errors
    ///
    /// Returns lifecycle, sequence, or adapter errors.
    fn poll(&mut self) -> Result<Option<SourceEvent>, SourceError>;
}

/// A source that queues externally supplied events for ordered consumption.
pub trait PushSource: DataSource {
    /// Consumes the oldest queued push event.
    ///
    /// # Errors
    ///
    /// Returns [`SourceError::NotRunning`] while stopped.
    fn next(&mut self) -> Result<Option<SourceEvent>, SourceError>;
}

/// Deterministic polling source backed by a FIFO script.
#[derive(Clone, Debug)]
pub struct FakePollingSource {
    id: String,
    interval: Duration,
    state: SourceState,
    sequence: u64,
    script: VecDeque<Result<DataValue, SourceError>>,
}

impl FakePollingSource {
    #[must_use]
    pub fn new(id: impl Into<String>, interval: Duration) -> Self {
        Self {
            id: id.into(),
            interval,
            state: SourceState::Stopped,
            sequence: 0,
            script: VecDeque::new(),
        }
    }

    pub fn enqueue(&mut self, value: DataValue) {
        self.script.push_back(Ok(value));
    }

    pub fn enqueue_error(&mut self, message: impl Into<String>) {
        self.script
            .push_back(Err(SourceError::Adapter(message.into())));
    }
}

impl DataSource for FakePollingSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn state(&self) -> SourceState {
        self.state
    }

    fn start(&mut self) -> Result<(), SourceError> {
        if self.state == SourceState::Running {
            return Err(SourceError::AlreadyRunning);
        }
        self.state = SourceState::Running;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SourceError> {
        if self.state == SourceState::Stopped {
            return Err(SourceError::NotRunning);
        }
        self.state = SourceState::Stopped;
        Ok(())
    }
}

impl PollingSource for FakePollingSource {
    fn poll_interval(&self) -> Duration {
        self.interval
    }

    fn poll(&mut self) -> Result<Option<SourceEvent>, SourceError> {
        if self.state != SourceState::Running {
            return Err(SourceError::NotRunning);
        }
        let Some(value) = self.script.pop_front() else {
            return Ok(None);
        };
        let value = value?;
        let sequence = self.sequence;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or(SourceError::SequenceExhausted)?;
        Ok(Some(SourceEvent { sequence, value }))
    }
}

/// Deterministic push source whose producer and consumer preserve FIFO order.
#[derive(Clone, Debug)]
pub struct FakePushSource {
    id: String,
    state: SourceState,
    sequence: u64,
    queue: VecDeque<SourceEvent>,
}

impl FakePushSource {
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            state: SourceState::Stopped,
            sequence: 0,
            queue: VecDeque::new(),
        }
    }

    /// Simulates an external producer emission while the source is running.
    ///
    /// # Errors
    ///
    /// Returns an error while stopped or when sequence numbers are exhausted.
    pub fn push(&mut self, value: DataValue) -> Result<u64, SourceError> {
        if self.state != SourceState::Running {
            return Err(SourceError::NotRunning);
        }
        let sequence = self.sequence;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or(SourceError::SequenceExhausted)?;
        self.queue.push_back(SourceEvent { sequence, value });
        Ok(sequence)
    }
}

impl DataSource for FakePushSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn state(&self) -> SourceState {
        self.state
    }

    fn start(&mut self) -> Result<(), SourceError> {
        if self.state == SourceState::Running {
            return Err(SourceError::AlreadyRunning);
        }
        self.state = SourceState::Running;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SourceError> {
        if self.state == SourceState::Stopped {
            return Err(SourceError::NotRunning);
        }
        self.state = SourceState::Stopped;
        Ok(())
    }
}

impl PushSource for FakePushSource {
    fn next(&mut self) -> Result<Option<SourceEvent>, SourceError> {
        if self.state != SourceState::Running {
            return Err(SourceError::NotRunning);
        }
        Ok(self.queue.pop_front())
    }
}
