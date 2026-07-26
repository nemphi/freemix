use crate::{FrameNumber, PlanGeneration};
use core::fmt;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanError {
    StaleGeneration {
        current: PlanGeneration,
        requested: PlanGeneration,
    },
    BoundaryNotFuture {
        last: FrameNumber,
        requested: FrameNumber,
    },
    NonMonotonicBoundary {
        last: FrameNumber,
        requested: FrameNumber,
    },
}

impl fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleGeneration { current, requested } => {
                write!(
                    formatter,
                    "plan generation {requested} is not newer than {current}"
                )
            }
            Self::BoundaryNotFuture { last, requested } => {
                write!(formatter, "plan boundary {requested} is not after {last}")
            }
            Self::NonMonotonicBoundary { last, requested } => {
                write!(formatter, "frame boundary {requested} is not after {last}")
            }
        }
    }
}

impl std::error::Error for PlanError {}

#[derive(Clone, Debug)]
pub struct PendingPlan<P> {
    pub generation: PlanGeneration,
    pub target_frame: FrameNumber,
    pub plan: Arc<P>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlanActivation {
    pub previous_generation: PlanGeneration,
    pub generation: PlanGeneration,
    pub target_frame: FrameNumber,
    pub activated_frame: FrameNumber,
}

#[derive(Clone, Debug)]
pub struct PlanManager<P> {
    generation: PlanGeneration,
    current: Arc<P>,
    pending: Option<PendingPlan<P>>,
    last_boundary: Option<FrameNumber>,
}

impl<P> PlanManager<P> {
    #[must_use]
    pub fn new(generation: PlanGeneration, plan: Arc<P>) -> Self {
        Self {
            generation,
            current: plan,
            pending: None,
            last_boundary: None,
        }
    }

    pub(crate) fn restore(
        generation: PlanGeneration,
        plan: Arc<P>,
        last_boundary: Option<FrameNumber>,
    ) -> Self {
        Self {
            generation,
            current: plan,
            pending: None,
            last_boundary,
        }
    }

    #[must_use]
    pub const fn generation(&self) -> PlanGeneration {
        self.generation
    }

    #[must_use]
    pub fn current(&self) -> &Arc<P> {
        &self.current
    }

    #[must_use]
    pub const fn pending(&self) -> Option<&PendingPlan<P>> {
        self.pending.as_ref()
    }

    /// Schedules an immutable plan for a future frame boundary.
    ///
    /// A new pending plan supersedes and returns the previous pending plan.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale generation or a boundary that has already
    /// passed.
    pub fn schedule(
        &mut self,
        generation: PlanGeneration,
        target_frame: FrameNumber,
        plan: Arc<P>,
    ) -> Result<Option<PendingPlan<P>>, PlanError> {
        let newest = self
            .pending
            .as_ref()
            .map_or(self.generation, |pending| pending.generation);
        if generation <= newest {
            return Err(PlanError::StaleGeneration {
                current: newest,
                requested: generation,
            });
        }
        if let Some(last) = self.last_boundary
            && target_frame <= last
        {
            return Err(PlanError::BoundaryNotFuture {
                last,
                requested: target_frame,
            });
        }
        Ok(self.pending.replace(PendingPlan {
            generation,
            target_frame,
            plan,
        }))
    }

    pub fn cancel_pending(&mut self) -> Option<PendingPlan<P>> {
        self.pending.take()
    }

    /// Advances the scheduler to the next observed frame boundary.
    ///
    /// # Errors
    ///
    /// Returns [`PlanError::NonMonotonicBoundary`] when the boundary does not
    /// advance.
    pub fn advance_boundary(
        &mut self,
        frame: FrameNumber,
    ) -> Result<Option<PlanActivation>, PlanError> {
        if let Some(last) = self.last_boundary
            && frame <= last
        {
            return Err(PlanError::NonMonotonicBoundary {
                last,
                requested: frame,
            });
        }
        self.last_boundary = Some(frame);

        let should_activate = self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.target_frame <= frame);
        if !should_activate {
            return Ok(None);
        }

        let Some(pending) = self.pending.take() else {
            return Ok(None);
        };
        let activation = PlanActivation {
            previous_generation: self.generation,
            generation: pending.generation,
            target_frame: pending.target_frame,
            activated_frame: frame,
        };
        self.generation = pending.generation;
        self.current = pending.plan;
        Ok(Some(activation))
    }
}
