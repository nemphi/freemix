use crate::{
    ActionError, ActionId, ActionQueue, FrameDeadline, FrameNumber, FramePacer, InputQueueError,
    InputQueues, PendingPlan, PlanActivation, PlanError, PlanGeneration, PlanManager,
    QueueConfigError, QueuePolicy, QueuePush, ScheduledAction, SchedulerTelemetry,
};
use core::fmt;
use fm_types::FrameRate;
use std::hash::Hash;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TickError {
    TooEarly { now_ns: u64, deadline_ns: u64 },
    Pacing(crate::PacingError),
    Plan(PlanError),
}

impl fmt::Display for TickError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooEarly {
                now_ns,
                deadline_ns,
            } => write!(
                formatter,
                "tick at {now_ns}ns precedes deadline {deadline_ns}ns"
            ),
            Self::Pacing(error) => error.fmt(formatter),
            Self::Plan(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for TickError {}

impl From<crate::PacingError> for TickError {
    fn from(value: crate::PacingError) -> Self {
        Self::Pacing(value)
    }
}

impl From<PlanError> for TickError {
    fn from(value: PlanError) -> Self {
        Self::Plan(value)
    }
}

#[derive(Clone, Debug)]
pub struct FrameTick<A, P> {
    pub deadline: FrameDeadline,
    pub late: bool,
    pub actions: Vec<ScheduledAction<A>>,
    pub plan_generation: PlanGeneration,
    pub plan: Arc<P>,
    pub activation: Option<PlanActivation>,
}

#[derive(Clone, Debug)]
pub struct FrameScheduler<I, F, A, P> {
    pacer: FramePacer,
    inputs: InputQueues<I, F>,
    actions: ActionQueue<A>,
    plans: PlanManager<P>,
    telemetry: SchedulerTelemetry,
}

impl<I, F, A, P> FrameScheduler<I, F, A, P> {
    #[must_use]
    pub fn new(
        frame_rate: FrameRate,
        origin_ns: u64,
        plan_generation: PlanGeneration,
        plan: Arc<P>,
    ) -> Self {
        Self {
            pacer: FramePacer::new(frame_rate, origin_ns),
            inputs: InputQueues::default(),
            actions: ActionQueue::default(),
            plans: PlanManager::new(plan_generation, plan),
            telemetry: SchedulerTelemetry::default(),
        }
    }

    /// Restores an idle scheduler whose next frame is `next_frame`.
    ///
    /// The current plan and generation are preserved. Transient input queues,
    /// scheduled actions, pending plans, and telemetry start empty.
    ///
    /// # Errors
    ///
    /// Returns an error when the cursor cannot produce and consume its exact
    /// next deadline.
    pub fn restore(
        frame_rate: FrameRate,
        origin_ns: u64,
        next_frame: FrameNumber,
        plan_generation: PlanGeneration,
        plan: Arc<P>,
    ) -> Result<Self, crate::PacingError> {
        let pacer = FramePacer::restore(frame_rate, origin_ns, next_frame)?;
        let last_boundary = next_frame.get().checked_sub(1).map(FrameNumber::new);
        Ok(Self {
            pacer,
            inputs: InputQueues::default(),
            actions: ActionQueue::default(),
            plans: PlanManager::restore(plan_generation, plan, last_boundary),
            telemetry: SchedulerTelemetry::default(),
        })
    }

    #[must_use]
    pub const fn pacer(&self) -> &FramePacer {
        &self.pacer
    }

    #[must_use]
    pub const fn telemetry(&self) -> SchedulerTelemetry {
        self.telemetry
    }

    #[must_use]
    pub const fn plan_generation(&self) -> PlanGeneration {
        self.plans.generation()
    }

    /// Schedules an action for a frame.
    ///
    /// # Errors
    ///
    /// Returns an error when the action sequence is exhausted.
    pub fn schedule_action(
        &mut self,
        target_frame: FrameNumber,
        action: A,
    ) -> Result<ActionId, ActionError> {
        self.actions.schedule(target_frame, action)
    }

    pub fn cancel_action(&mut self, id: ActionId) -> Option<ScheduledAction<A>> {
        self.actions.cancel(id)
    }

    /// Replaces an existing scheduled action.
    ///
    /// # Errors
    ///
    /// Returns an error when the old action is unknown or the sequence is
    /// exhausted.
    pub fn supersede_action(
        &mut self,
        superseded: ActionId,
        target_frame: FrameNumber,
        action: A,
    ) -> Result<(ActionId, ScheduledAction<A>), ActionError> {
        self.actions.supersede(superseded, target_frame, action)
    }

    /// Schedules a plan generation for activation at a frame boundary.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale generation or elapsed boundary.
    pub fn schedule_plan(
        &mut self,
        generation: PlanGeneration,
        target_frame: FrameNumber,
        plan: Arc<P>,
    ) -> Result<Option<PendingPlan<P>>, PlanError> {
        self.plans.schedule(generation, target_frame, plan)
    }

    pub fn cancel_pending_plan(&mut self) -> Option<PendingPlan<P>> {
        self.plans.cancel_pending()
    }

    /// Realizes the next frame when its deadline has arrived.
    ///
    /// # Errors
    ///
    /// Returns [`TickError::TooEarly`] before the deadline, or a typed pacing
    /// or plan-boundary error if internal counters cannot advance.
    pub fn tick(&mut self, now_ns: u64) -> Result<FrameTick<A, P>, TickError> {
        let deadline = self.pacer.next_deadline()?;
        if now_ns < deadline.at_ns {
            return Err(TickError::TooEarly {
                now_ns,
                deadline_ns: deadline.at_ns,
            });
        }

        let deadline = self.pacer.advance()?;
        let activation = self.plans.advance_boundary(deadline.frame)?;
        let actions = self.actions.drain_due(deadline.frame);
        let late = now_ns > deadline.at_ns;
        if late {
            self.telemetry.record_late();
        }
        self.telemetry.record_realized();

        Ok(FrameTick {
            deadline,
            late,
            actions,
            plan_generation: self.plans.generation(),
            plan: Arc::clone(self.plans.current()),
            activation,
        })
    }
}

impl<I: Eq + Hash, F, A, P> FrameScheduler<I, F, A, P> {
    /// Registers a bounded queue for an input.
    ///
    /// # Errors
    ///
    /// Returns an error for zero capacity or duplicate input.
    pub fn register_input(
        &mut self,
        input: I,
        capacity: usize,
        policy: QueuePolicy,
    ) -> Result<(), QueueConfigError> {
        self.inputs.register(input, capacity, policy)
    }

    /// Enqueues a frame and updates drop telemetry.
    ///
    /// # Errors
    ///
    /// Returns ownership of the frame for an unknown input or rejected
    /// `BlockProducer` push.
    pub fn push_frame(&mut self, input: &I, frame: F) -> Result<QueuePush<F>, InputQueueError<F>> {
        let outcome = self.inputs.push(input, frame)?;
        if outcome.dropped() {
            self.telemetry.record_dropped();
        }
        Ok(outcome)
    }

    pub fn pop_frame(&mut self, input: &I) -> Option<F> {
        self.inputs.pop(input)
    }

    #[must_use]
    pub fn input_depth(&self, input: &I) -> Option<usize> {
        self.inputs.len(input)
    }
}
