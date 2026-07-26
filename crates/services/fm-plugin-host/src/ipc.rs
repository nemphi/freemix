use std::{collections::VecDeque, error::Error, fmt};

/// One opaque message sent to an isolated child.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IpcMessage {
    pub id: u64,
    pub payload: Vec<u8>,
}

impl IpcMessage {
    pub fn new(id: u64, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            id,
            payload: payload.into(),
        }
    }
}

/// Queue bounds enforced before retaining message data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IpcLimits {
    pub max_messages: usize,
    pub max_message_bytes: usize,
}

impl IpcLimits {
    #[must_use]
    pub const fn new(max_messages: usize, max_message_bytes: usize) -> Self {
        Self {
            max_messages,
            max_message_bytes,
        }
    }

    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.max_messages > 0 && self.max_message_bytes > 0
    }
}

/// A strict FIFO queue which never grows beyond its configured limits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundedIpcQueue {
    limits: IpcLimits,
    messages: VecDeque<IpcMessage>,
}

impl BoundedIpcQueue {
    /// Creates an empty queue with fixed bounds.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::InvalidLimits`] for a zero bound.
    pub fn new(limits: IpcLimits) -> Result<Self, QueueError> {
        if !limits.is_valid() {
            return Err(QueueError::InvalidLimits);
        }
        Ok(Self {
            limits,
            messages: VecDeque::with_capacity(limits.max_messages),
        })
    }

    /// Retains one message if both queue bounds permit it.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::Full`] or [`QueueError::MessageTooLarge`] without
    /// modifying the queue.
    pub fn push(&mut self, message: IpcMessage) -> Result<(), QueueError> {
        if message.payload.len() > self.limits.max_message_bytes {
            return Err(QueueError::MessageTooLarge {
                size: message.payload.len(),
                maximum: self.limits.max_message_bytes,
            });
        }
        if self.messages.len() == self.limits.max_messages {
            return Err(QueueError::Full {
                capacity: self.limits.max_messages,
            });
        }
        self.messages.push_back(message);
        Ok(())
    }

    pub fn pop(&mut self) -> Option<IpcMessage> {
        self.messages.pop_front()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueueError {
    InvalidLimits,
    Full { capacity: usize },
    MessageTooLarge { size: usize, maximum: usize },
}

impl fmt::Display for QueueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => formatter.write_str("IPC queue limits must be nonzero"),
            Self::Full { capacity } => write!(formatter, "IPC queue capacity {capacity} reached"),
            Self::MessageTooLarge { size, maximum } => {
                write!(
                    formatter,
                    "IPC message size {size} exceeds maximum {maximum}"
                )
            }
        }
    }
}

impl Error for QueueError {}
