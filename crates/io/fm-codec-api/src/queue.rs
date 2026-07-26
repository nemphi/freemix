use std::collections::VecDeque;
use std::fmt;
use std::num::NonZeroUsize;

/// A validated item limit for adapter-owned queues.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct QueueCapacity(NonZeroUsize);

impl QueueCapacity {
    pub const MAX: usize = 65_536;

    /// Creates a nonzero, bounded queue capacity.
    ///
    /// # Errors
    ///
    /// Returns a typed error when `value` is zero or exceeds [`Self::MAX`].
    pub const fn new(value: usize) -> Result<Self, QueueCapacityError> {
        if value == 0 {
            return Err(QueueCapacityError::Zero);
        }
        if value > Self::MAX {
            return Err(QueueCapacityError::TooLarge {
                actual: value,
                maximum: Self::MAX,
            });
        }
        match NonZeroUsize::new(value) {
            Some(value) => Ok(Self(value)),
            None => Err(QueueCapacityError::Zero),
        }
    }

    #[must_use]
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueCapacityError {
    Zero,
    TooLarge { actual: usize, maximum: usize },
}

impl fmt::Display for QueueCapacityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero => formatter.write_str("queue capacity must be nonzero"),
            Self::TooLarge { actual, maximum } => {
                write!(formatter, "queue capacity {actual} exceeds {maximum}")
            }
        }
    }
}

impl std::error::Error for QueueCapacityError {}

/// A FIFO whose allocation and item count are constrained by configuration.
#[derive(Clone, Debug)]
pub struct BoundedQueue<T> {
    capacity: QueueCapacity,
    items: VecDeque<T>,
}

impl<T> BoundedQueue<T> {
    #[must_use]
    pub fn new(capacity: QueueCapacity) -> Self {
        Self {
            capacity,
            items: VecDeque::with_capacity(capacity.get()),
        }
    }

    /// Pushes one item without evicting an older item.
    ///
    /// # Errors
    ///
    /// Returns [`QueueFull`] with ownership of `item` when the queue is full.
    pub fn push(&mut self, item: T) -> Result<(), QueueFull<T>> {
        if self.items.len() == self.capacity.get() {
            Err(QueueFull(item))
        } else {
            self.items.push_back(item);
            Ok(())
        }
    }

    pub fn pop(&mut self) -> Option<T> {
        self.items.pop_front()
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    #[must_use]
    pub const fn capacity(&self) -> QueueCapacity {
        self.capacity
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn remaining(&self) -> usize {
        self.capacity.get() - self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[must_use]
    pub fn is_full(&self) -> bool {
        self.items.len() == self.capacity.get()
    }
}

/// A failed queue insertion that retains the unaccepted item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueFull<T>(T);

impl<T> QueueFull<T> {
    #[must_use]
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> fmt::Display for QueueFull<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bounded queue is full")
    }
}

impl<T: fmt::Debug> std::error::Error for QueueFull<T> {}
