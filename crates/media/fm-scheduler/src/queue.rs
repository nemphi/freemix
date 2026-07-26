use core::fmt;
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueuePolicy {
    DropOldest,
    DropNewest,
    /// Reject the producer immediately; no thread is blocked.
    BlockProducer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueConfigError {
    ZeroCapacity,
    DuplicateInput,
}

impl fmt::Display for QueueConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ZeroCapacity => "queue capacity must be nonzero",
            Self::DuplicateInput => "input queue is already registered",
        })
    }
}

impl std::error::Error for QueueConfigError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueuePush<T> {
    Enqueued { dropped: Option<T> },
    DroppedNewest(T),
}

impl<T> QueuePush<T> {
    #[must_use]
    pub const fn dropped(&self) -> bool {
        matches!(
            self,
            Self::Enqueued { dropped: Some(_) } | Self::DroppedNewest(_)
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputQueueError<T> {
    UnknownInput(T),
    WouldBlock(T),
}

impl<T> fmt::Display for InputQueueError<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnknownInput(_) => "input queue is not registered",
            Self::WouldBlock(_) => "input queue is full; producer must retry",
        })
    }
}

impl<T: fmt::Debug> std::error::Error for InputQueueError<T> {}

#[derive(Clone, Debug)]
pub struct BoundedQueue<T> {
    capacity: usize,
    policy: QueuePolicy,
    items: VecDeque<T>,
}

impl<T> BoundedQueue<T> {
    /// Creates a bounded queue.
    ///
    /// # Errors
    ///
    /// Returns [`QueueConfigError::ZeroCapacity`] when `capacity` is zero.
    pub fn new(capacity: usize, policy: QueuePolicy) -> Result<Self, QueueConfigError> {
        if capacity == 0 {
            return Err(QueueConfigError::ZeroCapacity);
        }
        Ok(Self {
            capacity,
            policy,
            items: VecDeque::with_capacity(capacity),
        })
    }

    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Enqueues an item according to the configured overflow policy.
    ///
    /// # Errors
    ///
    /// Returns [`InputQueueError::WouldBlock`] with ownership of the item when
    /// a full `BlockProducer` queue rejects it.
    pub fn push(&mut self, item: T) -> Result<QueuePush<T>, InputQueueError<T>> {
        if self.items.len() < self.capacity {
            self.items.push_back(item);
            return Ok(QueuePush::Enqueued { dropped: None });
        }

        match self.policy {
            QueuePolicy::DropOldest => {
                let dropped = self.items.pop_front();
                self.items.push_back(item);
                Ok(QueuePush::Enqueued { dropped })
            }
            QueuePolicy::DropNewest => Ok(QueuePush::DroppedNewest(item)),
            QueuePolicy::BlockProducer => Err(InputQueueError::WouldBlock(item)),
        }
    }

    pub fn pop(&mut self) -> Option<T> {
        self.items.pop_front()
    }
}

#[derive(Clone, Debug)]
pub struct InputQueues<I, T> {
    queues: HashMap<I, BoundedQueue<T>>,
}

impl<I, T> Default for InputQueues<I, T> {
    fn default() -> Self {
        Self {
            queues: HashMap::new(),
        }
    }
}

impl<I: Eq + Hash, T> InputQueues<I, T> {
    /// Registers one independently bounded input queue.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero capacity or duplicate input identifier.
    pub fn register(
        &mut self,
        input: I,
        capacity: usize,
        policy: QueuePolicy,
    ) -> Result<(), QueueConfigError> {
        if self.queues.contains_key(&input) {
            return Err(QueueConfigError::DuplicateInput);
        }
        let queue = BoundedQueue::new(capacity, policy)?;
        self.queues.insert(input, queue);
        Ok(())
    }

    /// Pushes a frame into its input queue.
    ///
    /// # Errors
    ///
    /// Returns ownership of the frame when the input is unknown or a full
    /// `BlockProducer` queue rejects it.
    pub fn push(&mut self, input: &I, item: T) -> Result<QueuePush<T>, InputQueueError<T>> {
        let Some(queue) = self.queues.get_mut(input) else {
            return Err(InputQueueError::UnknownInput(item));
        };
        queue.push(item)
    }

    pub fn pop(&mut self, input: &I) -> Option<T> {
        self.queues.get_mut(input).and_then(BoundedQueue::pop)
    }

    #[must_use]
    pub fn len(&self, input: &I) -> Option<usize> {
        self.queues.get(input).map(BoundedQueue::len)
    }
}
