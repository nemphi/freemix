use std::collections::VecDeque;

use crate::{CameraId, ContinuousSource, PtzIntent};

/// A camera-targeted intent waiting for an adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedIntent {
    pub camera_id: CameraId,
    pub intent: PtzIntent,
}

impl QueuedIntent {
    #[must_use]
    pub const fn new(camera_id: CameraId, intent: PtzIntent) -> Self {
        Self { camera_id, intent }
    }

    fn continuous_key(&self) -> Option<(&CameraId, ContinuousSource)> {
        match self.intent {
            PtzIntent::MoveContinuous(movement) => Some((&self.camera_id, movement.source)),
            _ => None,
        }
    }
}

/// Result of adding an intent to a coalescing queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PushOutcome {
    Enqueued,
    Coalesced,
    /// An older continuous move was discarded for fresher input.
    ReplacedContinuous,
    /// All queue slots held discrete commands, so continuous input was dropped.
    DroppedContinuous,
}

/// Error returned when a discrete intent cannot be retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueError {
    ZeroCapacity,
    FullOfDiscreteIntents,
}

impl core::fmt::Display for QueueError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ZeroCapacity => formatter.write_str("queue capacity must be greater than zero"),
            Self::FullOfDiscreteIntents => {
                formatter.write_str("queue is full of discrete PTZ intents")
            }
        }
    }
}

impl std::error::Error for QueueError {}

/// Bounded queue optimized for high-rate joystick and mouse movement.
///
/// Adjacent continuous moves from the same camera and source coalesce. When
/// full, the oldest continuous move may be evicted, but accepted discrete
/// preset, stop, and home commands are never evicted or reordered.
#[derive(Clone, Debug)]
pub struct CoalescingQueue {
    capacity: usize,
    items: VecDeque<QueuedIntent>,
}

impl CoalescingQueue {
    /// Creates a queue with a fixed, nonzero capacity.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::ZeroCapacity`] when `capacity` is zero.
    pub fn new(capacity: usize) -> Result<Self, QueueError> {
        if capacity == 0 {
            return Err(QueueError::ZeroCapacity);
        }
        Ok(Self {
            capacity,
            items: VecDeque::with_capacity(capacity),
        })
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
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

    /// Adds an intent, coalescing or evicting only continuous movement.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::FullOfDiscreteIntents`] when a discrete intent
    /// cannot fit without discarding an earlier discrete intent.
    pub fn push(&mut self, queued: QueuedIntent) -> Result<PushOutcome, QueueError> {
        let should_coalesce = self
            .items
            .back()
            .zip(queued.continuous_key())
            .is_some_and(|(back, incoming_key)| back.continuous_key() == Some(incoming_key));
        if should_coalesce {
            if let Some(back) = self.items.back_mut() {
                *back = queued;
            }
            return Ok(PushOutcome::Coalesced);
        }

        if self.items.len() < self.capacity {
            self.items.push_back(queued);
            return Ok(PushOutcome::Enqueued);
        }

        if let Some(index) = self
            .items
            .iter()
            .position(|existing| existing.intent.is_continuous())
        {
            self.items.remove(index);
            self.items.push_back(queued);
            return Ok(PushOutcome::ReplacedContinuous);
        }

        if queued.intent.is_continuous() {
            Ok(PushOutcome::DroppedContinuous)
        } else {
            Err(QueueError::FullOfDiscreteIntents)
        }
    }

    pub fn pop(&mut self) -> Option<QueuedIntent> {
        self.items.pop_front()
    }

    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &QueuedIntent> {
        self.items.iter()
    }
}
