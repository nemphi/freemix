use std::collections::HashMap;
use std::sync::Arc;

use fm_clock::{Clock, ClockDomainId, ClockTime, ManualClock};
use fm_command::{
    ApplyOutcome, CommandEnvelope, CommandReceipt, CommandState, EventSequence, IdempotencyKey,
    Mutation, Rejection, RejectionCode, Revision, RuntimeGeneration, StateEpoch,
};
use fm_scheduler::{FrameNumber, FrameScheduler, PlanGeneration};
use fm_switcher::{
    FadeToBlackFrame, FadeToBlackPosition, ProgramFrame, SwitcherCommand, SwitcherError,
    SwitcherEvent, SwitcherState, TBarPosition, TransitionKind,
};
use fm_types::{FrameRate, InputId};

use crate::{EngineError, ShowState, SnapshotError};

type RuntimeScheduler = FrameScheduler<(), (), EngineCommand, ()>;
const MAX_TRANSITION_DURATION_FRAMES: u32 = 3_600;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineManualTransitionKind {
    Fade,
    Wipe,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct EngineManualTransitionPosition(TBarPosition);

impl EngineManualTransitionPosition {
    pub const MAX: u16 = TBarPosition::MAX;
    pub const START: Self = Self(TBarPosition::START);
    pub const END: Self = Self(TBarPosition::END);

    #[must_use]
    pub const fn new(basis_points: u16) -> Option<Self> {
        match TBarPosition::new(basis_points) {
            Some(position) => Some(Self(position)),
            None => None,
        }
    }

    #[must_use]
    pub const fn basis_points(self) -> u16 {
        self.0.basis_points()
    }

    const fn domain(self) -> TBarPosition {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineManualTransitionState {
    pub kind: EngineManualTransitionKind,
    pub from: InputId,
    pub to: InputId,
    pub interval_start: EngineManualTransitionPosition,
    pub position: EngineManualTransitionPosition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineFadeToBlackState {
    pub active: bool,
    pub position: FadeToBlackPosition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineCommand {
    SelectPreview(InputId),
    Cut,
    Fade {
        duration_frames: u32,
    },
    Wipe {
        duration_frames: u32,
    },
    FadeToBlack {
        active: bool,
        duration_frames: u32,
    },
    StartManualTransition {
        kind: EngineManualTransitionKind,
    },
    SetManualTransitionPosition {
        position: EngineManualTransitionPosition,
    },
    CommitManualTransition,
    CancelManualTransition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineAcceptance {
    pub target_frame: FrameNumber,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineEvent {
    DesiredSwitcherChanged(EngineCommand),
}

pub type EngineCommandOutcome = ApplyOutcome<EngineAcceptance, EngineEvent>;

/// The result of preparing an engine command without changing the live engine.
#[derive(Debug)]
pub enum EnginePrepareOutcome {
    /// The idempotency key was already durable, so there is nothing to commit.
    Replayed(EngineCommandOutcome),
    /// A new accepted or rejected receipt is staged and ready to commit.
    Prepared(Box<PreparedEngineExecution>),
}

impl EnginePrepareOutcome {
    #[must_use]
    pub fn prepared(self) -> Option<PreparedEngineExecution> {
        match self {
            Self::Prepared(prepared) => Some(*prepared),
            Self::Replayed(_) => None,
        }
    }

    #[must_use]
    pub fn replayed(self) -> Option<EngineCommandOutcome> {
        match self {
            Self::Prepared(_) => None,
            Self::Replayed(outcome) => Some(outcome),
        }
    }
}

/// An isolated post-command engine ready for projection or atomic installation.
#[derive(Debug)]
pub struct PreparedEngineExecution {
    staged: Engine,
    outcome: EngineCommandOutcome,
    base: EnginePreparationBase,
}

impl PreparedEngineExecution {
    #[must_use]
    pub const fn outcome(&self) -> &EngineCommandOutcome {
        &self.outcome
    }

    /// Returns the staged post-command engine without exposing mutable access.
    #[must_use]
    pub const fn staged_engine(&self) -> &Engine {
        &self.staged
    }

    /// Projects the staged engine by an exact number of deterministic ticks.
    ///
    /// The returned snapshot includes the projected runtime state and receipt.
    ///
    /// # Errors
    ///
    /// Returns a tick error if projection cannot advance, or
    /// [`SnapshotError::WorkInFlight`] if `ticks` does not reach an idle frame
    /// boundary.
    pub fn project(&self, ticks: u64) -> Result<EngineSnapshot, EngineError> {
        let mut projected = self.staged.clone();
        for _ in 0..ticks {
            projected.tick()?;
        }
        projected.snapshot().map_err(EngineError::from)
    }

    /// Consumes this preparation without changing its originating engine.
    pub fn abort(self) {}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EnginePreparationBase {
    state_epoch: StateEpoch,
    revision: Revision,
    event_sequence: EventSequence,
    durable_receipts: usize,
    frame_cursor: FrameNumber,
    clock_time: ClockTime,
    runtime_generation: RuntimeGeneration,
}

/// Persisted engine coordinates needed to resume an idle runtime exactly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineRestoreState {
    pub state_epoch: StateEpoch,
    pub revision: Revision,
    pub event_sequence: EventSequence,
    pub runtime_generation: RuntimeGeneration,
    pub clock_time: ClockTime,
    pub frame_cursor: FrameNumber,
    pub receipts: Vec<(IdempotencyKey, CommandReceipt<EngineAcceptance>)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameResult {
    pub frame: FrameNumber,
    pub deadline: ClockTime,
    pub program: ProgramFrame,
    pub fade_to_black: FadeToBlackFrame,
    pub events: Vec<SwitcherEvent>,
    pub revision: Revision,
    pub runtime_generation: RuntimeGeneration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineSnapshot {
    show: ShowState,
    realized_switcher: SwitcherState,
    state_epoch: StateEpoch,
    revision: Revision,
    event_sequence: EventSequence,
    runtime_generation: RuntimeGeneration,
    frame_rate: FrameRate,
    clock_domain: ClockDomainId,
    clock_time: ClockTime,
    frames_rendered: u64,
    receipts: Vec<(IdempotencyKey, CommandReceipt<EngineAcceptance>)>,
}

impl EngineSnapshot {
    #[must_use]
    pub const fn show(&self) -> &ShowState {
        &self.show
    }

    #[must_use]
    pub const fn realized_switcher(&self) -> &SwitcherState {
        &self.realized_switcher
    }

    #[must_use]
    pub fn desired_manual_transition(&self) -> Option<EngineManualTransitionState> {
        self.show()
            .desired_switcher()
            .t_bar()
            .map(engine_manual_state)
    }

    #[must_use]
    pub const fn desired_fade_to_black(&self) -> EngineFadeToBlackState {
        engine_fade_to_black_state(self.show.desired_switcher())
    }

    #[must_use]
    pub const fn realized_fade_to_black(&self) -> EngineFadeToBlackState {
        engine_fade_to_black_state(&self.realized_switcher)
    }

    #[must_use]
    pub fn realized_manual_transition(&self) -> Option<EngineManualTransitionState> {
        self.realized_switcher.t_bar().map(engine_manual_state)
    }

    #[must_use]
    pub const fn state_epoch(&self) -> StateEpoch {
        self.state_epoch
    }

    #[must_use]
    pub const fn revision(&self) -> Revision {
        self.revision
    }

    #[must_use]
    pub const fn event_sequence(&self) -> EventSequence {
        self.event_sequence
    }

    #[must_use]
    pub const fn runtime_generation(&self) -> RuntimeGeneration {
        self.runtime_generation
    }

    #[must_use]
    pub const fn frame_rate(&self) -> FrameRate {
        self.frame_rate
    }

    #[must_use]
    pub const fn clock_domain(&self) -> ClockDomainId {
        self.clock_domain
    }

    #[must_use]
    pub const fn clock_time(&self) -> ClockTime {
        self.clock_time
    }

    #[must_use]
    pub const fn frames_rendered(&self) -> u64 {
        self.frames_rendered
    }

    #[must_use]
    pub fn receipts(&self) -> &[(IdempotencyKey, CommandReceipt<EngineAcceptance>)] {
        &self.receipts
    }
}

#[derive(Clone, Debug)]
pub struct Engine {
    commands: CommandState<ShowState, EngineAcceptance>,
    receipt_history: HashMap<IdempotencyKey, CommandReceipt<EngineAcceptance>>,
    realized_switcher: SwitcherState,
    scheduler: RuntimeScheduler,
    clock: ManualClock,
    frame_rate: FrameRate,
    runtime_generation: RuntimeGeneration,
    pending_actions: usize,
    transition_in_flight: Option<TransitionKind>,
}

impl Engine {
    #[must_use]
    pub fn new(show: ShowState, frame_rate: FrameRate, clock_domain: ClockDomainId) -> Self {
        let realized_switcher = show.desired_switcher().clone();
        Self {
            commands: CommandState::new(show, StateEpoch::new(1)),
            receipt_history: HashMap::new(),
            realized_switcher,
            scheduler: RuntimeScheduler::new(frame_rate, 0, PlanGeneration::new(0), Arc::new(())),
            clock: ManualClock::new(clock_domain, ClockTime::ZERO),
            frame_rate,
            runtime_generation: RuntimeGeneration::default(),
            pending_actions: 0,
            transition_in_flight: None,
        }
    }

    #[must_use]
    pub const fn show(&self) -> &ShowState {
        self.commands.state()
    }

    #[must_use]
    pub const fn realized_switcher(&self) -> &SwitcherState {
        &self.realized_switcher
    }

    #[must_use]
    pub fn desired_manual_transition(&self) -> Option<EngineManualTransitionState> {
        self.show()
            .desired_switcher()
            .t_bar()
            .map(engine_manual_state)
    }

    #[must_use]
    pub fn realized_manual_transition(&self) -> Option<EngineManualTransitionState> {
        self.realized_switcher.t_bar().map(engine_manual_state)
    }

    #[must_use]
    pub const fn desired_fade_to_black(&self) -> EngineFadeToBlackState {
        engine_fade_to_black_state(self.show().desired_switcher())
    }

    #[must_use]
    pub const fn realized_fade_to_black(&self) -> EngineFadeToBlackState {
        engine_fade_to_black_state(&self.realized_switcher)
    }

    #[must_use]
    pub const fn revision(&self) -> Revision {
        self.commands.revision()
    }

    #[must_use]
    pub const fn state_epoch(&self) -> StateEpoch {
        self.commands.state_epoch()
    }

    #[must_use]
    pub const fn event_sequence(&self) -> EventSequence {
        self.commands.event_sequence()
    }

    #[must_use]
    pub const fn runtime_generation(&self) -> RuntimeGeneration {
        self.runtime_generation
    }

    #[must_use]
    pub const fn frame_cursor(&self) -> FrameNumber {
        self.scheduler.pacer().next_frame()
    }

    #[must_use]
    pub fn clock_time(&self) -> ClockTime {
        self.clock.now()
    }

    /// Returns the next deterministic frame deadline without advancing state.
    ///
    /// # Errors
    ///
    /// Returns a pacing error when the next deadline exceeds the media clock.
    pub fn next_frame_deadline(&self) -> Result<ClockTime, EngineError> {
        self.scheduler
            .pacer()
            .next_deadline()
            .map(|deadline| ClockTime::from_nanos(deadline.at_ns))
            .map_err(|error| EngineError::Tick(fm_scheduler::TickError::Pacing(error)))
    }

    /// Prepares a durable command and its runtime action on an isolated clone.
    ///
    /// Duplicate idempotency keys return their durable receipt without creating
    /// a staged execution.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Schedule`] if the staged runtime action sequence
    /// is exhausted.
    pub fn prepare_execute(
        &self,
        envelope: CommandEnvelope<EngineCommand>,
        now_millis: u64,
    ) -> Result<EnginePrepareOutcome, EngineError> {
        if let Some(receipt) = self.commands.receipt(&envelope.idempotency_key) {
            return Ok(EnginePrepareOutcome::Replayed(EngineCommandOutcome {
                receipt: receipt.clone(),
                events: Vec::new(),
                replayed: true,
            }));
        }

        let base = self.preparation_base();
        let mut staged = self.clone();
        let outcome = staged.execute_immediate(envelope, now_millis)?;
        Ok(EnginePrepareOutcome::Prepared(Box::new(
            PreparedEngineExecution {
                staged,
                outcome,
                base,
            },
        )))
    }

    /// Atomically installs a prepared engine if its live base is still current.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::StalePreparation`] if durable authority, receipt
    /// history, or the runtime cursor changed after preparation.
    pub fn commit_execute(
        &mut self,
        prepared: PreparedEngineExecution,
    ) -> Result<EngineCommandOutcome, EngineError> {
        if self.preparation_base() != prepared.base {
            return Err(EngineError::StalePreparation);
        }

        let PreparedEngineExecution {
            staged, outcome, ..
        } = prepared;
        *self = staged;
        Ok(outcome)
    }

    /// Accepts a durable command and schedules its runtime action.
    ///
    /// Duplicate idempotency keys replay the original receipt without adding
    /// another runtime action.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Schedule`] if the runtime action sequence is
    /// exhausted.
    pub fn execute(
        &mut self,
        envelope: CommandEnvelope<EngineCommand>,
        now_millis: u64,
    ) -> Result<EngineCommandOutcome, EngineError> {
        match self.prepare_execute(envelope, now_millis)? {
            EnginePrepareOutcome::Replayed(outcome) => Ok(outcome),
            EnginePrepareOutcome::Prepared(prepared) => self.commit_execute(*prepared),
        }
    }

    fn execute_immediate(
        &mut self,
        envelope: CommandEnvelope<EngineCommand>,
        now_millis: u64,
    ) -> Result<EngineCommandOutcome, EngineError> {
        let key = envelope.idempotency_key.clone();
        let command = envelope.command;
        let target_frame = self.scheduler.pacer().next_frame();
        let already_seen = self.commands.receipt(&key).is_some();
        let scheduled = if already_seen {
            None
        } else {
            Some(
                self.scheduler
                    .schedule_action(target_frame, command)
                    .map_err(EngineError::Schedule)?,
            )
        };
        let runtime_busy = self.transition_in_flight;
        let outcome = self
            .commands
            .apply(envelope, now_millis, move |_, command| {
                if let Some(kind) = runtime_busy
                    && !matches!(command, EngineCommand::FadeToBlack { .. })
                {
                    let name = match kind {
                        TransitionKind::Fade => "fade",
                        TransitionKind::Wipe => "wipe",
                        _ => "timed",
                    };
                    return Err(Rejection::new(
                        RejectionCode::Conflict,
                        format!("a {name} transition is already in flight"),
                    ));
                }
                Ok(EngineMutation {
                    command,
                    target_frame,
                })
            });

        if !outcome.replayed {
            self.receipt_history.insert(key, outcome.receipt.clone());
        }
        if outcome.receipt.accepted().is_some() && !outcome.replayed {
            self.pending_actions += 1;
            if let Some(kind) = match command {
                EngineCommand::Fade { .. } => Some(TransitionKind::Fade),
                EngineCommand::Wipe { .. } => Some(TransitionKind::Wipe),
                EngineCommand::SelectPreview(_)
                | EngineCommand::Cut
                | EngineCommand::FadeToBlack { .. }
                | EngineCommand::StartManualTransition { .. }
                | EngineCommand::SetManualTransitionPosition { .. }
                | EngineCommand::CommitManualTransition
                | EngineCommand::CancelManualTransition => None,
            } {
                self.transition_in_flight = Some(kind);
            }
        } else if let Some(action) = scheduled {
            self.scheduler.cancel_action(action);
        }
        Ok(outcome)
    }

    fn preparation_base(&self) -> EnginePreparationBase {
        EnginePreparationBase {
            state_epoch: self.commands.state_epoch(),
            revision: self.commands.revision(),
            event_sequence: self.commands.event_sequence(),
            durable_receipts: self.receipt_history.len(),
            frame_cursor: self.scheduler.pacer().next_frame(),
            clock_time: self.clock.now(),
            runtime_generation: self.runtime_generation,
        }
    }

    /// Renders the next frame at its exact simulated deadline.
    ///
    /// # Errors
    ///
    /// Returns a typed error if the clock, scheduler, switcher, or runtime
    /// generation cannot advance.
    pub fn tick(&mut self) -> Result<FrameResult, EngineError> {
        let deadline = self
            .scheduler
            .pacer()
            .next_deadline()
            .map_err(|error| EngineError::Tick(fm_scheduler::TickError::Pacing(error)))?;
        self.clock.set(ClockTime::from_nanos(deadline.at_ns))?;
        let tick = self.scheduler.tick(deadline.at_ns)?;
        self.pending_actions = self.pending_actions.saturating_sub(tick.actions.len());

        let mut events = Vec::new();
        for scheduled in tick.actions {
            let command_events = apply_runtime(&mut self.realized_switcher, scheduled.action)?;
            events.extend(command_events);
            self.runtime_generation = self.runtime_generation.checked_next()?;
        }

        let program = self.realized_switcher.program_frame();
        let fade_to_black = self.realized_switcher.fade_to_black_frame();
        if let Some(event) = self.realized_switcher.advance_frame() {
            if matches!(event, SwitcherEvent::ProgramChanged { .. }) {
                self.transition_in_flight = None;
            }
            events.push(event);
        }

        Ok(FrameResult {
            frame: tick.deadline.frame,
            deadline: ClockTime::from_nanos(tick.deadline.at_ns),
            program,
            fade_to_black,
            events,
            revision: self.commands.revision(),
            runtime_generation: self.runtime_generation,
        })
    }

    /// Captures durable and realized state at an idle frame boundary.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::WorkInFlight`] while an action or transition
    /// is pending.
    pub fn snapshot(&self) -> Result<EngineSnapshot, SnapshotError> {
        if self.pending_actions != 0
            || self.transition_in_flight.is_some()
            || self.realized_switcher.transition().is_some()
            || self
                .commands
                .state()
                .desired_switcher()
                .fade_to_black_is_automatic()
            || self.realized_switcher.fade_to_black_is_automatic()
        {
            return Err(SnapshotError::WorkInFlight);
        }
        let mut receipts: Vec<_> = self
            .receipt_history
            .iter()
            .map(|(key, receipt)| (key.clone(), receipt.clone()))
            .collect();
        receipts.sort_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));
        Ok(EngineSnapshot {
            show: self.commands.state().clone(),
            realized_switcher: self.realized_switcher.clone(),
            state_epoch: self.commands.state_epoch(),
            revision: self.commands.revision(),
            event_sequence: self.commands.event_sequence(),
            runtime_generation: self.runtime_generation,
            frame_rate: self.frame_rate,
            clock_domain: self.clock.domain(),
            clock_time: self.clock.now(),
            frames_rendered: self.scheduler.pacer().next_frame().get(),
            receipts,
        })
    }

    /// Restores an idle snapshot, including durable and runtime counters.
    ///
    /// # Errors
    ///
    /// Returns an error if the saved scheduler cursor cannot be reconstructed.
    pub fn restore(snapshot: EngineSnapshot) -> Result<Self, EngineError> {
        let restore_state = EngineRestoreState {
            state_epoch: snapshot.state_epoch,
            revision: snapshot.revision,
            event_sequence: snapshot.event_sequence,
            runtime_generation: snapshot.runtime_generation,
            clock_time: snapshot.clock_time,
            frame_cursor: FrameNumber::new(snapshot.frames_rendered),
            receipts: snapshot.receipts,
        };
        Self::restore_persisted(
            snapshot.show,
            snapshot.realized_switcher,
            snapshot.frame_rate,
            snapshot.clock_domain,
            restore_state,
        )
        .map_err(EngineError::from)
    }

    /// Restores durable and idle runtime state supplied by persistence.
    ///
    /// # Errors
    ///
    /// Returns a [`SnapshotError`] if the persisted state is not a consistent
    /// idle frame boundary or its frame cursor cannot be reconstructed.
    pub fn restore_persisted(
        show: ShowState,
        realized_switcher: SwitcherState,
        frame_rate: FrameRate,
        clock_domain: ClockDomainId,
        restore_state: EngineRestoreState,
    ) -> Result<Self, SnapshotError> {
        validate_idle_restore(&show, &realized_switcher, &restore_state, frame_rate)?;
        let scheduler = RuntimeScheduler::restore(
            frame_rate,
            0,
            restore_state.frame_cursor,
            PlanGeneration::new(0),
            Arc::new(()),
        )
        .map_err(|_| SnapshotError::InvalidFrameCounter)?;
        let receipt_history = restore_state.receipts.iter().cloned().collect();
        Ok(Self {
            commands: CommandState::restore_with_receipts(
                show,
                restore_state.state_epoch,
                restore_state.revision,
                restore_state.event_sequence,
                restore_state.receipts,
            ),
            receipt_history,
            realized_switcher,
            scheduler,
            clock: ManualClock::new(clock_domain, restore_state.clock_time),
            frame_rate,
            runtime_generation: restore_state.runtime_generation,
            pending_actions: 0,
            transition_in_flight: None,
        })
    }
}

fn validate_idle_restore(
    show: &ShowState,
    realized_switcher: &SwitcherState,
    restore_state: &EngineRestoreState,
    frame_rate: FrameRate,
) -> Result<(), SnapshotError> {
    if show.desired_switcher().transition().is_some() || realized_switcher.transition().is_some() {
        return Err(SnapshotError::WorkInFlight);
    }
    if show.desired_switcher().fade_to_black_is_automatic()
        || realized_switcher.fade_to_black_is_automatic()
    {
        return Err(SnapshotError::WorkInFlight);
    }
    if show.inputs() != realized_switcher.inputs() {
        return Err(SnapshotError::IncompatibleSwitcher);
    }
    if show.desired_switcher().program() != realized_switcher.program()
        || show.desired_switcher().preview() != realized_switcher.preview()
        || show.desired_switcher().fade_to_black() != realized_switcher.fade_to_black()
        || show.desired_switcher().fade_to_black_position()
            != realized_switcher.fade_to_black_position()
    {
        return Err(SnapshotError::MismatchedSwitcherRouting);
    }
    match (show.desired_switcher().t_bar(), realized_switcher.t_bar()) {
        (None, None) => {}
        (Some(desired), Some(realized))
            if matches!(desired.kind(), TransitionKind::Fade | TransitionKind::Wipe)
                && matches!(realized.kind(), TransitionKind::Fade | TransitionKind::Wipe)
                && desired.kind() == realized.kind()
                && desired.from() == realized.from()
                && desired.to() == realized.to()
                && desired.interval_start() == TBarPosition::START
                && desired.position() == realized.position()
                && realized.interval_start() == realized.position() => {}
        _ => return Err(SnapshotError::MismatchedManualTransition),
    }

    let cursor = restore_state.frame_cursor.get();
    let pacer = fm_scheduler::FramePacer::new(frame_rate, 0);
    let expected_clock_ns = if cursor == 0 {
        0
    } else {
        pacer
            .deadline_for(FrameNumber::new(cursor - 1))
            .map_err(|_| SnapshotError::InvalidFrameCounter)?
            .at_ns
    };
    if restore_state.clock_time.as_nanos() != expected_clock_ns {
        return Err(SnapshotError::ClockTimeMismatch {
            expected_ns: expected_clock_ns,
            actual_ns: restore_state.clock_time.as_nanos(),
        });
    }

    let mut accepted_commands = 0_u64;
    for (_, receipt) in &restore_state.receipts {
        if let Some(accepted) = receipt.accepted() {
            accepted_commands = accepted_commands
                .checked_add(1)
                .ok_or(SnapshotError::InvalidFrameCounter)?;
            if accepted.result.target_frame >= restore_state.frame_cursor {
                return Err(SnapshotError::UnrealizedAcceptedReceipt {
                    target_frame: accepted.result.target_frame.get(),
                    frame_cursor: cursor,
                });
            }
        }
    }

    if restore_state.revision.get() != accepted_commands
        || restore_state.event_sequence.get() != accepted_commands
        || restore_state.runtime_generation.get() != accepted_commands
    {
        return Err(SnapshotError::CounterMismatch {
            accepted_commands,
            revision: restore_state.revision.get(),
            event_sequence: restore_state.event_sequence.get(),
            runtime_generation: restore_state.runtime_generation.get(),
        });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct EngineMutation {
    command: EngineCommand,
    target_frame: FrameNumber,
}

impl Mutation<ShowState, EngineEvent, EngineAcceptance> for EngineMutation {
    fn apply(
        self,
        state: &mut ShowState,
        events: &mut Vec<EngineEvent>,
    ) -> Result<EngineAcceptance, Rejection> {
        let switcher_command = match self.command {
            EngineCommand::SelectPreview(input) => SwitcherCommand::SelectPreview(input),
            EngineCommand::Cut => SwitcherCommand::Cut,
            EngineCommand::Fade { duration_frames } => {
                if duration_frames == 0 {
                    return Err(Rejection::new(
                        RejectionCode::InvalidCommand,
                        "fade duration must be nonzero",
                    ));
                }
                if duration_frames > MAX_TRANSITION_DURATION_FRAMES {
                    return Err(Rejection::new(
                        RejectionCode::InvalidCommand,
                        format!(
                            "fade duration must not exceed {MAX_TRANSITION_DURATION_FRAMES} frames"
                        ),
                    ));
                }
                SwitcherCommand::Cut
            }
            EngineCommand::Wipe { duration_frames } => {
                if duration_frames == 0 {
                    return Err(Rejection::new(
                        RejectionCode::InvalidCommand,
                        "wipe duration must be nonzero",
                    ));
                }
                if duration_frames > MAX_TRANSITION_DURATION_FRAMES {
                    return Err(Rejection::new(
                        RejectionCode::InvalidCommand,
                        format!(
                            "wipe duration must not exceed {MAX_TRANSITION_DURATION_FRAMES} frames"
                        ),
                    ));
                }
                SwitcherCommand::Cut
            }
            EngineCommand::FadeToBlack {
                active,
                duration_frames,
            } => {
                validate_fade_to_black_duration(duration_frames)?;
                let _ = state.desired_switcher_mut().set_fade_to_black(active);
                events.push(EngineEvent::DesiredSwitcherChanged(self.command));
                return Ok(EngineAcceptance {
                    target_frame: self.target_frame,
                });
            }
            EngineCommand::StartManualTransition { kind } => SwitcherCommand::StartTBar {
                kind: manual_transition_kind(kind),
            },
            EngineCommand::SetManualTransitionPosition { position } => {
                SwitcherCommand::SetTBarPosition(position.domain())
            }
            EngineCommand::CommitManualTransition => SwitcherCommand::CommitTBar,
            EngineCommand::CancelManualTransition => SwitcherCommand::CancelTBar,
        };
        state
            .desired_switcher_mut()
            .apply(switcher_command)
            .map_err(switcher_rejection)?;
        events.push(EngineEvent::DesiredSwitcherChanged(self.command));
        Ok(EngineAcceptance {
            target_frame: self.target_frame,
        })
    }
}

fn apply_runtime(
    switcher: &mut SwitcherState,
    command: EngineCommand,
) -> Result<Vec<SwitcherEvent>, SwitcherError> {
    if let EngineCommand::FadeToBlack {
        active,
        duration_frames,
    } = command
    {
        return Ok(switcher
            .request_fade_to_black(active, duration_frames)
            .expect("accepted engine FTB durations are validated"));
    }
    switcher.apply(match command {
        EngineCommand::SelectPreview(input) => SwitcherCommand::SelectPreview(input),
        EngineCommand::Cut => SwitcherCommand::Cut,
        EngineCommand::Fade { duration_frames } => SwitcherCommand::Transition {
            kind: TransitionKind::Fade,
            duration_frames,
        },
        EngineCommand::Wipe { duration_frames } => SwitcherCommand::Wipe { duration_frames },
        EngineCommand::FadeToBlack { .. } => {
            unreachable!("Fade-to-Black commands return before switcher command mapping")
        }
        EngineCommand::StartManualTransition { kind } => SwitcherCommand::StartTBar {
            kind: manual_transition_kind(kind),
        },
        EngineCommand::SetManualTransitionPosition { position } => {
            SwitcherCommand::SetTBarPosition(position.domain())
        }
        EngineCommand::CommitManualTransition => SwitcherCommand::CommitTBar,
        EngineCommand::CancelManualTransition => SwitcherCommand::CancelTBar,
    })
}

const fn engine_fade_to_black_state(switcher: &SwitcherState) -> EngineFadeToBlackState {
    EngineFadeToBlackState {
        active: switcher.fade_to_black(),
        position: switcher.fade_to_black_position(),
    }
}

fn validate_fade_to_black_duration(duration_frames: u32) -> Result<(), Rejection> {
    if duration_frames == 0 {
        Err(Rejection::new(
            RejectionCode::InvalidCommand,
            "Fade-to-Black duration must be nonzero",
        ))
    } else if duration_frames > fm_switcher::MAX_FADE_TO_BLACK_DURATION_FRAMES {
        Err(Rejection::new(
            RejectionCode::InvalidCommand,
            format!(
                "Fade-to-Black duration must not exceed {} frames",
                fm_switcher::MAX_FADE_TO_BLACK_DURATION_FRAMES
            ),
        ))
    } else {
        Ok(())
    }
}

const fn manual_transition_kind(kind: EngineManualTransitionKind) -> TransitionKind {
    match kind {
        EngineManualTransitionKind::Fade => TransitionKind::Fade,
        EngineManualTransitionKind::Wipe => TransitionKind::Wipe,
    }
}

fn engine_manual_state(state: fm_switcher::TBarState) -> EngineManualTransitionState {
    EngineManualTransitionState {
        kind: match state.kind() {
            TransitionKind::Fade => EngineManualTransitionKind::Fade,
            TransitionKind::Wipe => EngineManualTransitionKind::Wipe,
            _ => unreachable!("engine manual transitions are fade or wipe"),
        },
        from: state.from(),
        to: state.to(),
        interval_start: EngineManualTransitionPosition::new(state.interval_start().basis_points())
            .expect("switcher manual transition positions are bounded"),
        position: EngineManualTransitionPosition::new(state.position().basis_points())
            .expect("switcher manual transition positions are bounded"),
    }
}

fn switcher_rejection(error: SwitcherError) -> Rejection {
    let code = match error {
        SwitcherError::UnknownInput(_) => RejectionCode::NotFound,
        SwitcherError::TransitionInProgress => RejectionCode::Conflict,
        SwitcherError::UnsupportedManualTransitionKind
        | SwitcherError::InvalidManualTransitionRoute
        | SwitcherError::ZeroDuration => RejectionCode::InvalidCommand,
    };
    Rejection::new(code, error.to_string())
}
