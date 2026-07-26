use core::fmt;
use std::collections::VecDeque;

use fm_frame::{AudioBlock, CpuVideoFrame};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverflowPolicy {
    DropOldest,
    DropNewest,
    Reject,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SinkConfigError {
    ZeroCapacity,
}

impl fmt::Display for SinkConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("collecting sink capacity must be nonzero")
    }
}

impl std::error::Error for SinkConfigError {}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SinkTelemetry {
    received: u64,
    accepted: u64,
    dropped_oldest: u64,
    dropped_newest: u64,
    rejected: u64,
    high_watermark: usize,
}

impl SinkTelemetry {
    #[must_use]
    pub const fn received(self) -> u64 {
        self.received
    }

    #[must_use]
    pub const fn accepted(self) -> u64 {
        self.accepted
    }

    #[must_use]
    pub const fn dropped_oldest(self) -> u64 {
        self.dropped_oldest
    }

    #[must_use]
    pub const fn dropped_newest(self) -> u64 {
        self.dropped_newest
    }

    #[must_use]
    pub const fn rejected(self) -> u64 {
        self.rejected
    }

    #[must_use]
    pub const fn high_watermark(self) -> usize {
        self.high_watermark
    }

    #[must_use]
    pub const fn dropped(self) -> u64 {
        self.dropped_oldest + self.dropped_newest
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CollectOutcome<T> {
    Collected,
    DroppedOldest(T),
    DroppedNewest(T),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CollectError<T> {
    Full(T),
}

impl<T> fmt::Display for CollectError<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("collecting sink is full")
    }
}

impl<T: fmt::Debug> std::error::Error for CollectError<T> {}

#[derive(Clone, Debug)]
pub struct CollectingSink<T> {
    capacity: usize,
    policy: OverflowPolicy,
    items: VecDeque<T>,
    telemetry: SinkTelemetry,
}

impl<T> CollectingSink<T> {
    /// Creates an in-memory sink with a hard item bound.
    ///
    /// # Errors
    ///
    /// Returns [`SinkConfigError::ZeroCapacity`] when `capacity` is zero.
    pub fn new(capacity: usize, policy: OverflowPolicy) -> Result<Self, SinkConfigError> {
        if capacity == 0 {
            return Err(SinkConfigError::ZeroCapacity);
        }
        Ok(Self {
            capacity,
            policy,
            items: VecDeque::with_capacity(capacity),
            telemetry: SinkTelemetry::default(),
        })
    }

    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    #[must_use]
    pub const fn policy(&self) -> OverflowPolicy {
        self.policy
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[must_use]
    pub const fn telemetry(&self) -> SinkTelemetry {
        self.telemetry
    }

    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &T> {
        self.items.iter()
    }

    /// Collects one item according to the configured overflow policy.
    ///
    /// # Errors
    ///
    /// Returns ownership of `item` when a full sink uses [`OverflowPolicy::Reject`].
    pub fn collect(&mut self, item: T) -> Result<CollectOutcome<T>, CollectError<T>> {
        self.telemetry.received = self.telemetry.received.saturating_add(1);
        if self.items.len() < self.capacity {
            self.items.push_back(item);
            self.record_accept();
            return Ok(CollectOutcome::Collected);
        }

        match self.policy {
            OverflowPolicy::DropOldest => {
                let Some(dropped) = self.items.pop_front() else {
                    self.items.push_back(item);
                    self.record_accept();
                    return Ok(CollectOutcome::Collected);
                };
                self.items.push_back(item);
                self.telemetry.dropped_oldest = self.telemetry.dropped_oldest.saturating_add(1);
                self.record_accept();
                Ok(CollectOutcome::DroppedOldest(dropped))
            }
            OverflowPolicy::DropNewest => {
                self.telemetry.dropped_newest = self.telemetry.dropped_newest.saturating_add(1);
                Ok(CollectOutcome::DroppedNewest(item))
            }
            OverflowPolicy::Reject => {
                self.telemetry.rejected = self.telemetry.rejected.saturating_add(1);
                Err(CollectError::Full(item))
            }
        }
    }

    pub fn pop_front(&mut self) -> Option<T> {
        self.items.pop_front()
    }

    pub fn drain(&mut self) -> impl Iterator<Item = T> + '_ {
        self.items.drain(..)
    }

    /// Clears collected items and telemetry while preserving configuration.
    pub fn reset(&mut self) {
        self.items.clear();
        self.telemetry = SinkTelemetry::default();
    }

    fn record_accept(&mut self) {
        self.telemetry.accepted = self.telemetry.accepted.saturating_add(1);
        self.telemetry.high_watermark = self.telemetry.high_watermark.max(self.items.len());
    }
}

pub type CollectingVideoSink = CollectingSink<CpuVideoFrame>;
pub type CollectingAudioSink = CollectingSink<AudioBlock>;
