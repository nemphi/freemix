//! Authoritative, transport-independent command control and event resumption.
//!
//! The retained log in this crate is bounded memory, not filesystem durability.
//! Newly accepted records are returned to the caller so a persistence layer can
//! compose durable storage without coupling it to a transport.

use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    error::Error,
    fmt,
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel},
    },
};

use fm_auth::{AuthorizationDenial, CommandClass, Policy, Principal};
use fm_command::{CommandReceipt, DurableEvent, IdempotencyKey, RejectionCode};
use fm_engine::{
    Engine, EngineAcceptance, EngineCommand, EngineCommandOutcome, EngineError, EngineEvent,
    EngineFadeToBlackState, EngineManualTransitionKind, EngineManualTransitionPosition,
    EngineManualTransitionState, EnginePrepareOutcome, EngineSnapshot, FrameResult,
    PreparedEngineExecution, SnapshotError,
};
use fm_protocol::{
    CommandMessage, CommandPayload, CommandResult, EngineIdentity, EventCursor, EventMessage,
    EventPayload, FadeToBlackPosition, FadeToBlackState, FieldIssue, ManualTransitionKind,
    ManualTransitionPosition, ManualTransitionState, ManualTransitionStatus, RuntimeEventMessage,
    RuntimeLifecycleEvent, ServerIdentity, SnapshotMessage,
    StingerAudioPolicy as ProtocolStingerAudioPolicy,
    StingerMissingMediaFallback as ProtocolStingerFallback, StingerReadiness, StingerStatus,
    WireInputId, WireMessage, WireStingerSlotId,
};
use fm_switcher::{
    MissingMediaFallback, StingerAudioPolicy, StingerDescriptor, StingerPreloadState, StingerSlotId,
};

/// Target-free authorization called before an engine command is constructed or validated.
pub trait AuthorizationHook {
    /// Authorizes one command category.
    ///
    /// # Errors
    ///
    /// Returns a safe, target-free denial when the principal lacks permission.
    fn authorize(
        &self,
        principal: &Principal,
        command: CommandClass,
    ) -> Result<(), AuthorizationDenial>;
}

impl AuthorizationHook for Policy {
    fn authorize(
        &self,
        principal: &Principal,
        command: CommandClass,
    ) -> Result<(), AuthorizationDenial> {
        Policy::authorize(self, principal, command)
    }
}

/// Independent bounds for resume history and live subscribers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlLimits {
    pub retained_events: usize,
    pub max_subscribers: usize,
    pub subscriber_queue: usize,
}

impl Default for ControlLimits {
    fn default() -> Self {
        Self {
            retained_events: 1_024,
            max_subscribers: 64,
            subscriber_queue: 64,
        }
    }
}

/// A point-in-time protocol snapshot retained for resume fallback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotRecord {
    pub cursor: EventCursor,
    pub snapshot: SnapshotMessage,
}

/// The complete result of a resume decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResumeDecision {
    /// Every revision after the supplied cursor is still contiguous and retained.
    Events(Vec<EventMessage>),
    /// Identity changed, the cursor is invalid, or required events were compacted.
    Snapshot(Box<SnapshotRecord>),
}

/// Wire output whose shape makes command-result-before-events ordering explicit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResultBeforeEvents {
    pub result: CommandResult,
    pub events: Vec<EventMessage>,
}

impl ResultBeforeEvents {
    #[must_use]
    pub fn into_wire_messages(self) -> Vec<WireMessage> {
        let mut messages = Vec::with_capacity(self.events.len() + 1);
        messages.push(WireMessage::CommandResult(self.result));
        messages.extend(self.events.into_iter().map(WireMessage::Event));
        messages
    }
}

/// Engine-owned records for a newly accepted command.
///
/// `Some(AcceptedOutcome)` means persistence work is required. Replayed
/// acceptances do not produce this value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedOutcome {
    pub idempotency_key: IdempotencyKey,
    pub receipt: CommandReceipt<EngineAcceptance>,
    pub events: Vec<DurableEvent<EngineEvent>>,
}

/// A command result plus optional persistence and subscriber side effects.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSubmission {
    pub output: ResultBeforeEvents,
    pub replayed: bool,
    pub accepted: Option<AcceptedOutcome>,
    pub subscriber_failures: Vec<SubscriberFailure>,
}

impl CommandSubmission {
    #[must_use]
    pub const fn is_accepted(&self) -> bool {
        matches!(&self.output.result, CommandResult::Accepted { .. })
    }
}

/// The result of preparing a control submission without changing live state.
pub enum PrepareSubmitOutcome<'a, A = Policy> {
    /// The idempotency key already has a durable engine receipt or authorization denial.
    Replayed(CommandSubmission),
    /// A new engine receipt or authorization denial is staged for explicit commit.
    Prepared(PreparedSubmission<'a, A>),
}

impl<'a, A> PrepareSubmitOutcome<'a, A> {
    #[must_use]
    pub fn prepared(self) -> Option<PreparedSubmission<'a, A>> {
        match self {
            Self::Prepared(prepared) => Some(prepared),
            Self::Replayed(_) => None,
        }
    }

    #[must_use]
    pub fn replayed(self) -> Option<CommandSubmission> {
        match self {
            Self::Prepared(_) => None,
            Self::Replayed(submission) => Some(submission),
        }
    }
}

enum PreparedSubmissionKind {
    Engine {
        execution: Box<PreparedEngineExecution>,
        command: EngineCommand,
    },
    AuthorizationDenial,
}

/// An isolated submission that exclusively borrows its control authority.
///
/// Dropping this value aborts the submission without changing the service.
pub struct PreparedSubmission<'a, A = Policy> {
    control: &'a mut ControlService<A>,
    idempotency_key: IdempotencyKey,
    submission: CommandSubmission,
    kind: PreparedSubmissionKind,
}

impl<A: AuthorizationHook> PreparedSubmission<'_, A> {
    /// Returns the prospective metadata. Subscriber failures remain empty until commit.
    #[must_use]
    pub const fn submission(&self) -> &CommandSubmission {
        &self.submission
    }

    /// Returns the prospective command result and durable protocol events.
    #[must_use]
    pub const fn output(&self) -> &ResultBeforeEvents {
        &self.submission.output
    }

    #[must_use]
    pub const fn idempotency_key(&self) -> &IdempotencyKey {
        &self.idempotency_key
    }

    /// Projects the post-commit engine by an exact number of deterministic ticks.
    ///
    /// # Errors
    ///
    /// Returns an engine tick error, or a snapshot error when the requested tick
    /// count does not reach an idle frame boundary.
    pub fn project(&self, ticks: u64) -> Result<EngineSnapshot, ControlError> {
        match &self.kind {
            PreparedSubmissionKind::Engine { execution, .. } => {
                execution.project(ticks).map_err(ControlError::from)
            }
            PreparedSubmissionKind::AuthorizationDenial => {
                project_engine(&self.control.engine, ticks).map_err(ControlError::from)
            }
        }
    }

    /// Atomically installs the staged decision and then applies control side effects.
    ///
    /// # Errors
    ///
    /// Returns an engine infrastructure error. The exclusive service borrow makes
    /// stale engine preparation impossible through the safe control API.
    pub fn commit(self) -> Result<CommandSubmission, ControlError> {
        let Self {
            control,
            idempotency_key,
            mut submission,
            kind,
        } = self;

        match kind {
            PreparedSubmissionKind::Engine { execution, command } => {
                let _ = control.engine.commit_execute(*execution)?;

                if submission.accepted.is_some() {
                    for event in &submission.output.events {
                        control.log.push_back(event.clone());
                    }
                    while control.log.len() > control.limits.retained_events {
                        control.log.pop_front();
                    }
                    control
                        .pending_runtime_actions
                        .push_back(PendingRuntimeAction {
                            revision: control.engine.revision().get(),
                            command,
                        });
                }
                control.snapshot = snapshot_record(&control.engine, &control.identity);

                let live_events = submission
                    .output
                    .events
                    .iter()
                    .cloned()
                    .map(LiveEvent::Durable)
                    .collect::<Vec<_>>();
                submission.subscriber_failures = control.publish(&live_events);
            }
            PreparedSubmissionKind::AuthorizationDenial => {
                control.remember_authorization_denial(
                    idempotency_key,
                    submission.output.result.clone(),
                );
            }
        }

        Ok(submission)
    }

    /// Consumes this preparation without changing the service.
    pub fn abort(self) {}
}

/// Output of exactly one simulated engine tick.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TickOutcome {
    pub frame: FrameResult,
    pub runtime_events: Vec<RuntimeEventMessage>,
    pub subscriber_failures: Vec<SubscriberFailure>,
}

/// Failure from ticking the control engine or realizing its rendered frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TickWithRealizerError<E> {
    /// The control tick failed before or after frame realization.
    Tick(ControlError),
    /// The frame realization callback failed after the engine advanced.
    Realization(E),
}

impl<E> TickWithRealizerError<E> {
    #[must_use]
    pub const fn tick_error(&self) -> Option<&ControlError> {
        match self {
            Self::Tick(error) => Some(error),
            Self::Realization(_) => None,
        }
    }

    #[must_use]
    pub const fn realization_error(&self) -> Option<&E> {
        match self {
            Self::Tick(_) => None,
            Self::Realization(error) => Some(error),
        }
    }

    #[must_use]
    pub fn into_tick_error(self) -> Option<ControlError> {
        match self {
            Self::Tick(error) => Some(error),
            Self::Realization(_) => None,
        }
    }

    #[must_use]
    pub fn into_realization_error(self) -> Option<E> {
        match self {
            Self::Tick(_) => None,
            Self::Realization(error) => Some(error),
        }
    }
}

impl<E: fmt::Display> fmt::Display for TickWithRealizerError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tick(error) => write!(formatter, "control tick failed: {error}"),
            Self::Realization(error) => write!(formatter, "frame realization failed: {error}"),
        }
    }
}

impl<E: Error + 'static> Error for TickWithRealizerError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Tick(error) => Some(error),
            Self::Realization(error) => Some(error),
        }
    }
}

/// A live subscription item with an explicit durability class.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LiveEvent {
    Durable(EventMessage),
    Runtime(RuntimeEventMessage),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SubscriberId(u64);

impl SubscriberId {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriberFailureReason {
    SlowClient,
    Disconnected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscriberFailure {
    pub subscriber: SubscriberId,
    pub reason: SubscriberFailureReason,
}

/// The receiving half of a bounded live-event subscription.
#[derive(Debug)]
pub struct Subscription {
    id: SubscriberId,
    receiver: Receiver<LiveEvent>,
    failure: Arc<Mutex<Option<SubscriberFailureReason>>>,
}

impl Subscription {
    #[must_use]
    pub const fn id(&self) -> SubscriberId {
        self.id
    }

    /// Receives a queued event without blocking.
    ///
    /// # Errors
    ///
    /// Returns `Empty` when no event is ready or `Disconnected` after removal.
    pub fn try_recv(&self) -> Result<LiveEvent, TryRecvError> {
        self.receiver.try_recv()
    }

    #[must_use]
    pub fn failure(&self) -> Option<SubscriberFailureReason> {
        match self.failure.lock() {
            Ok(failure) => *failure,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscribeError {
    LimitReached,
    IdentifierExhausted,
}

impl fmt::Display for SubscribeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::LimitReached => "subscriber limit reached",
            Self::IdentifierExhausted => "subscriber identifier space exhausted",
        })
    }
}

impl Error for SubscribeError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlError {
    Engine(EngineError),
    ServerIdentityMismatch,
    RuntimeGenerationOutOfOrder,
    RuntimeActionMismatch,
    RuntimeSequenceExhausted,
}

impl fmt::Display for ControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Engine(error) => error.fmt(formatter),
            Self::ServerIdentityMismatch => {
                formatter.write_str("runtime server identity does not match the control engine")
            }
            Self::RuntimeGenerationOutOfOrder => {
                formatter.write_str("runtime generation did not advance monotonically")
            }
            Self::RuntimeActionMismatch => {
                formatter.write_str("runtime generation advanced without a pending action")
            }
            Self::RuntimeSequenceExhausted => {
                formatter.write_str("runtime event sequence exhausted")
            }
        }
    }
}

impl Error for ControlError {}

impl From<EngineError> for ControlError {
    fn from(value: EngineError) -> Self {
        Self::Engine(value)
    }
}

/// Capability-neutral operational state for diagnostic adapters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlDiagnostics {
    pub engine: EngineIdentity,
    pub current_revision: u64,
    pub oldest_retained_revision: Option<u64>,
    pub newest_retained_revision: Option<u64>,
    pub subscriber_count: usize,
    pub limits: ControlLimits,
}

struct Subscriber {
    sender: SyncSender<LiveEvent>,
    failure: Arc<Mutex<Option<SubscriberFailureReason>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingRuntimeAction {
    revision: u64,
    command: EngineCommand,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ActiveTransition {
    revision: u64,
    generation: u64,
}

/// Single-writer Phase 1 control service.
pub struct ControlService<A = Policy> {
    engine: Engine,
    authorizer: A,
    identity: EngineIdentity,
    limits: ControlLimits,
    authorization_denials: HashMap<IdempotencyKey, CommandResult>,
    authorization_denial_order: VecDeque<IdempotencyKey>,
    log: VecDeque<EventMessage>,
    snapshot: SnapshotRecord,
    subscribers: HashMap<SubscriberId, Subscriber>,
    next_subscriber_id: u64,
    pending_runtime_actions: VecDeque<PendingRuntimeAction>,
    active_transition: Option<ActiveTransition>,
    active_fade_to_black: Option<ActiveTransition>,
    runtime_sequence_generation: u64,
    runtime_sequence: u64,
}

impl<A: AuthorizationHook> ControlService<A> {
    #[must_use]
    pub fn new(
        engine: Engine,
        authorizer: A,
        engine_id: impl Into<String>,
        log_id: impl Into<String>,
        limits: ControlLimits,
    ) -> Self {
        let runtime_sequence_generation = engine.runtime_generation().get();
        let identity = EngineIdentity {
            engine_id: engine_id.into(),
            state_epoch: engine.state_epoch().get(),
            log_id: log_id.into(),
        };
        let snapshot = snapshot_record(&engine, &identity);
        Self {
            engine,
            authorizer,
            identity,
            limits,
            authorization_denials: HashMap::new(),
            authorization_denial_order: VecDeque::with_capacity(limits.retained_events),
            log: VecDeque::with_capacity(limits.retained_events),
            snapshot,
            subscribers: HashMap::new(),
            next_subscriber_id: 1,
            pending_runtime_actions: VecDeque::new(),
            active_transition: None,
            active_fade_to_black: None,
            runtime_sequence_generation,
            runtime_sequence: 0,
        }
    }

    #[must_use]
    pub const fn identity(&self) -> &EngineIdentity {
        &self.identity
    }

    #[must_use]
    pub const fn snapshot(&self) -> &SnapshotRecord {
        &self.snapshot
    }

    /// Returns the engine's next deterministic frame deadline without ticking.
    ///
    /// # Errors
    ///
    /// Returns an engine pacing error when the next deadline is not representable.
    pub fn next_frame_deadline(&self) -> Result<fm_clock::ClockTime, ControlError> {
        self.engine.next_frame_deadline().map_err(Into::into)
    }

    /// Projects the next deterministic frame without advancing authority state.
    ///
    /// This is intended for bounded media preparation that must complete before
    /// the authoritative tick is realized.
    ///
    /// # Errors
    ///
    /// Returns an engine tick error when the projected frame cannot advance.
    pub fn project_next_frame(&self) -> Result<FrameResult, ControlError> {
        self.engine.clone().tick().map_err(Into::into)
    }

    /// Captures the current engine only when it is at an idle frame boundary.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::WorkInFlight`] while an engine action or
    /// transition is pending, including an unpublished control realization.
    pub fn idle_engine_snapshot(&self) -> Result<EngineSnapshot, SnapshotError> {
        if !self.pending_runtime_actions.is_empty()
            || self.active_transition.is_some()
            || self.active_fade_to_black.is_some()
        {
            return Err(SnapshotError::WorkInFlight);
        }
        self.engine.snapshot()
    }

    #[must_use]
    pub fn diagnostics(&self) -> ControlDiagnostics {
        ControlDiagnostics {
            engine: self.identity.clone(),
            current_revision: self.engine.revision().get(),
            oldest_retained_revision: self.log.front().map(|event| event.cursor.revision),
            newest_retained_revision: self.log.back().map(|event| event.cursor.revision),
            subscriber_count: self.subscribers.len(),
            limits: self.limits,
        }
    }

    /// Creates a bounded live-event subscription.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured subscriber limit or identifier
    /// space is exhausted.
    pub fn subscribe(&mut self) -> Result<Subscription, SubscribeError> {
        if self.subscribers.len() >= self.limits.max_subscribers {
            return Err(SubscribeError::LimitReached);
        }
        let id = SubscriberId(self.next_subscriber_id);
        self.next_subscriber_id = self
            .next_subscriber_id
            .checked_add(1)
            .ok_or(SubscribeError::IdentifierExhausted)?;
        let (sender, receiver) = sync_channel(self.limits.subscriber_queue);
        let failure = Arc::new(Mutex::new(None));
        self.subscribers.insert(
            id,
            Subscriber {
                sender,
                failure: Arc::clone(&failure),
            },
        );
        Ok(Subscription {
            id,
            receiver,
            failure,
        })
    }

    /// Returns retained events only when identity and every following revision match.
    #[must_use]
    pub fn resume(&self, cursor: &EventCursor) -> ResumeDecision {
        let current_revision = self.engine.revision().get();
        if cursor.engine != self.identity || cursor.revision > current_revision {
            return ResumeDecision::Snapshot(Box::new(self.snapshot.clone()));
        }
        if cursor.revision == current_revision {
            return ResumeDecision::Events(Vec::new());
        }

        let events: Vec<_> = self
            .log
            .iter()
            .filter(|event| event.cursor.revision > cursor.revision)
            .cloned()
            .collect();
        let mut expected = cursor.revision.checked_add(1);
        let contiguous = !events.is_empty()
            && events.iter().all(|event| {
                let matches = expected == Some(event.cursor.revision);
                expected = event.cursor.revision.checked_add(1);
                matches
            })
            && events.last().map(|event| event.cursor.revision) == Some(current_revision);
        if contiguous {
            ResumeDecision::Events(events)
        } else {
            ResumeDecision::Snapshot(Box::new(self.snapshot.clone()))
        }
    }

    /// Prepares an authorized protocol command without changing live control state.
    ///
    /// A cached target-free authorization denial is replayed first. Otherwise,
    /// authorization sees only the target-free command class and runs before
    /// conversion and detailed validation. Engine execution remains the
    /// authority for other idempotency, deadline, revision, and domain behavior.
    ///
    /// # Errors
    ///
    /// Returns an engine infrastructure error. Command rejections and authorization
    /// denials are staged as normal [`CommandResult::Rejected`] values.
    pub fn prepare_submit<'a>(
        &'a mut self,
        principal: &Principal,
        message: CommandMessage,
        now_millis: u64,
    ) -> Result<PrepareSubmitOutcome<'a, A>, ControlError> {
        let idempotency_key = IdempotencyKey::new(message.idempotency_key.clone());
        if let Some(result) = self.authorization_denials.get(&idempotency_key) {
            return Ok(PrepareSubmitOutcome::Replayed(CommandSubmission {
                output: ResultBeforeEvents {
                    result: result.clone(),
                    events: Vec::new(),
                },
                replayed: true,
                accepted: None,
                subscriber_failures: Vec::new(),
            }));
        }

        if let Err(denial) = self
            .authorizer
            .authorize(principal, command_class(message.payload))
        {
            let result = CommandResult::Rejected {
                id: message.id,
                code: RejectionCode::PermissionDenied.as_str().to_owned(),
                message: denial.to_string(),
                fields: Vec::new(),
                current_revision: self.engine.revision().get(),
                retryable: false,
            };
            return Ok(PrepareSubmitOutcome::Prepared(PreparedSubmission {
                control: self,
                idempotency_key,
                kind: PreparedSubmissionKind::AuthorizationDenial,
                submission: CommandSubmission {
                    output: ResultBeforeEvents {
                        result,
                        events: Vec::new(),
                    },
                    replayed: false,
                    accepted: None,
                    subscriber_failures: Vec::new(),
                },
            }));
        }

        let command = engine_command(message.payload);
        match self
            .engine
            .prepare_execute(message.domain_envelope(command), now_millis)?
        {
            EnginePrepareOutcome::Replayed(outcome) => Ok(PrepareSubmitOutcome::Replayed(
                engine_submission(&self.identity, idempotency_key, &self.engine, &outcome),
            )),
            EnginePrepareOutcome::Prepared(execution) => {
                let submission = engine_submission(
                    &self.identity,
                    idempotency_key.clone(),
                    execution.staged_engine(),
                    execution.outcome(),
                );
                Ok(PrepareSubmitOutcome::Prepared(PreparedSubmission {
                    control: self,
                    idempotency_key,
                    submission,
                    kind: PreparedSubmissionKind::Engine { execution, command },
                }))
            }
        }
    }

    /// Authorizes, prepares, and immediately commits a protocol command.
    ///
    /// # Errors
    ///
    /// Returns an engine infrastructure error. Command rejections are normal
    /// [`CommandResult::Rejected`] values.
    pub fn submit(
        &mut self,
        principal: &Principal,
        message: CommandMessage,
        now_millis: u64,
    ) -> Result<CommandSubmission, ControlError> {
        match self.prepare_submit(principal, message, now_millis)? {
            PrepareSubmitOutcome::Replayed(submission) => Ok(submission),
            PrepareSubmitOutcome::Prepared(prepared) => prepared.commit(),
        }
    }

    fn remember_authorization_denial(
        &mut self,
        idempotency_key: IdempotencyKey,
        result: CommandResult,
    ) {
        if self.limits.retained_events == 0 {
            return;
        }
        while self.authorization_denials.len() >= self.limits.retained_events {
            let Some(expired) = self.authorization_denial_order.pop_front() else {
                break;
            };
            self.authorization_denials.remove(&expired);
        }
        self.authorization_denial_order
            .push_back(idempotency_key.clone());
        self.authorization_denials.insert(idempotency_key, result);
    }

    /// Advances exactly one frame at the engine's deterministic simulated deadline.
    ///
    /// # Errors
    ///
    /// Returns an engine tick error without advancing an additional frame.
    pub fn tick(&mut self, server: &ServerIdentity) -> Result<TickOutcome, ControlError> {
        match self.tick_with_realizer(server, |_| Ok::<(), Infallible>(())) {
            Ok(outcome) => Ok(outcome),
            Err(TickWithRealizerError::Tick(error)) => Err(error),
            Err(TickWithRealizerError::Realization(error)) => match error {},
        }
    }

    /// Advances one shutdown-only frame without publishing a runtime-realized
    /// event for media that was deliberately not rendered.
    ///
    /// This exists only to settle already accepted work to an idle checkpoint
    /// after the process has committed to exit.
    ///
    /// # Errors
    ///
    /// Returns an engine tick error without advancing an additional frame.
    pub fn tick_for_shutdown(
        &mut self,
        server: &ServerIdentity,
    ) -> Result<TickOutcome, ControlError> {
        match self.tick_with_realizer_inner(server, |_| Ok::<(), Infallible>(()), false) {
            Ok(outcome) => Ok(outcome),
            Err(TickWithRealizerError::Tick(error)) => Err(error),
            Err(TickWithRealizerError::Realization(error)) => match error {},
        }
    }

    /// Advances one engine frame, realizes it, then publishes its control effects.
    ///
    /// The realizer receives the exact [`FrameResult`] produced by the engine.
    /// Pending runtime actions, realized lifecycle events, snapshots, and live
    /// subscribers are updated only after the callback succeeds.
    ///
    /// # Errors
    ///
    /// Returns [`TickWithRealizerError::Tick`] for a control tick failure. If the
    /// callback fails, returns [`TickWithRealizerError::Realization`] without
    /// publishing or changing control-side realization state. The engine has
    /// already advanced in that case, so the failed frame cannot be retried;
    /// callers must treat realization failure as fatal to the tick loop.
    pub fn tick_with_realizer<E>(
        &mut self,
        server: &ServerIdentity,
        realizer: impl FnOnce(&FrameResult) -> Result<(), E>,
    ) -> Result<TickOutcome, TickWithRealizerError<E>> {
        self.tick_with_realizer_inner(server, realizer, true)
    }

    fn tick_with_realizer_inner<E>(
        &mut self,
        server: &ServerIdentity,
        realizer: impl FnOnce(&FrameResult) -> Result<(), E>,
        publish_runtime_realization: bool,
    ) -> Result<TickOutcome, TickWithRealizerError<E>> {
        if server.engine_id != self.identity.engine_id
            || server.state_epoch != self.identity.state_epoch
            || server.log_id != self.identity.log_id
        {
            return Err(TickWithRealizerError::Tick(
                ControlError::ServerIdentityMismatch,
            ));
        }

        let previous_generation = self.engine.runtime_generation().get();
        let frame = self
            .engine
            .tick()
            .map_err(ControlError::from)
            .map_err(TickWithRealizerError::Tick)?;
        realizer(&frame).map_err(TickWithRealizerError::Realization)?;
        let current_generation = frame.runtime_generation.get();
        let applied_actions = current_generation
            .checked_sub(previous_generation)
            .ok_or(ControlError::RuntimeGenerationOutOfOrder)
            .map_err(TickWithRealizerError::Tick)?;
        let mut runtime_events = Vec::new();
        for offset in 1..=applied_actions {
            let pending = self
                .pending_runtime_actions
                .pop_front()
                .ok_or(ControlError::RuntimeActionMismatch)
                .map_err(TickWithRealizerError::Tick)?;
            let generation = previous_generation
                .checked_add(offset)
                .ok_or(ControlError::RuntimeGenerationOutOfOrder)
                .map_err(TickWithRealizerError::Tick)?;
            if is_program_transition(pending.command) {
                self.active_transition = Some(ActiveTransition {
                    revision: pending.revision,
                    generation,
                });
            } else if matches!(pending.command, EngineCommand::FadeToBlack { .. }) {
                if let Some(superseded) = self.active_fade_to_black.replace(ActiveTransition {
                    revision: pending.revision,
                    generation,
                }) && publish_runtime_realization
                {
                    runtime_events.push(
                        self.runtime_superseded(
                            server,
                            superseded.revision,
                            generation,
                            pending.revision,
                        )
                        .map_err(TickWithRealizerError::Tick)?,
                    );
                }
            } else if publish_runtime_realization {
                runtime_events.push(
                    self.runtime_realized(server, pending.revision, generation)
                        .map_err(TickWithRealizerError::Tick)?,
                );
            }
        }

        let completed_transition = (self.engine.realized_switcher().transition().is_none()
            && self.engine.realized_switcher().program()
                == self.engine.show().desired_switcher().program()
            && self.engine.realized_switcher().preview()
                == self.engine.show().desired_switcher().preview())
        .then(|| self.active_transition.take())
        .flatten();
        let completed_fade_to_black =
            (!self.engine.realized_switcher().fade_to_black_is_automatic()
                && self.engine.realized_fade_to_black() == self.engine.desired_fade_to_black())
            .then(|| self.active_fade_to_black.take())
            .flatten();
        let mut completions = [completed_transition, completed_fade_to_black]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        completions.sort_by_key(|operation| operation.generation);
        if publish_runtime_realization {
            for operation in completions {
                runtime_events.push(
                    self.runtime_realized(server, operation.revision, current_generation)
                        .map_err(TickWithRealizerError::Tick)?,
                );
            }
        }

        self.snapshot = snapshot_record(&self.engine, &self.identity);
        let live_events = runtime_events
            .iter()
            .cloned()
            .map(LiveEvent::Runtime)
            .collect::<Vec<_>>();
        let subscriber_failures = self.publish(&live_events);
        Ok(TickOutcome {
            frame,
            runtime_events,
            subscriber_failures,
        })
    }

    fn runtime_realized(
        &mut self,
        server: &ServerIdentity,
        revision: u64,
        generation: u64,
    ) -> Result<RuntimeEventMessage, ControlError> {
        let event = RuntimeLifecycleEvent::Realized {
            domain: "switcher".to_owned(),
            manual_transition: Some(protocol_manual_status(
                self.engine.realized_manual_transition(),
            )),
            fade_to_black: Some(protocol_fade_to_black_state(
                self.engine.realized_fade_to_black(),
            )),
        };
        self.runtime_event(server, revision, generation, event)
    }

    fn runtime_superseded(
        &mut self,
        server: &ServerIdentity,
        revision: u64,
        generation: u64,
        by_revision: u64,
    ) -> Result<RuntimeEventMessage, ControlError> {
        self.runtime_event(
            server,
            revision,
            generation,
            RuntimeLifecycleEvent::Superseded { by_revision },
        )
    }

    fn runtime_event(
        &mut self,
        server: &ServerIdentity,
        revision: u64,
        generation: u64,
        event: RuntimeLifecycleEvent,
    ) -> Result<RuntimeEventMessage, ControlError> {
        if generation != self.runtime_sequence_generation {
            if generation < self.runtime_sequence_generation {
                return Err(ControlError::RuntimeGenerationOutOfOrder);
            }
            self.runtime_sequence_generation = generation;
            self.runtime_sequence = 0;
        }
        self.runtime_sequence = self
            .runtime_sequence
            .checked_add(1)
            .ok_or(ControlError::RuntimeSequenceExhausted)?;
        Ok(RuntimeEventMessage {
            server: server.clone(),
            revision,
            generation,
            sequence: self.runtime_sequence,
            event,
        })
    }

    fn publish(&mut self, events: &[LiveEvent]) -> Vec<SubscriberFailure> {
        let mut failures = Vec::new();
        for event in events {
            self.subscribers.retain(|id, subscriber| {
                let failure = match subscriber.sender.try_send(event.clone()) {
                    Ok(()) => return true,
                    Err(TrySendError::Full(_)) => SubscriberFailureReason::SlowClient,
                    Err(TrySendError::Disconnected(_)) => SubscriberFailureReason::Disconnected,
                };
                match subscriber.failure.lock() {
                    Ok(mut status) => *status = Some(failure),
                    Err(poisoned) => *poisoned.into_inner() = Some(failure),
                }
                failures.push(SubscriberFailure {
                    subscriber: *id,
                    reason: failure,
                });
                false
            });
        }
        failures
    }
}

const fn command_class(payload: CommandPayload) -> CommandClass {
    match payload {
        CommandPayload::SelectPreview { .. } => CommandClass::SelectPreview,
        CommandPayload::Cut
        | CommandPayload::Fade { .. }
        | CommandPayload::AlphaFade { .. }
        | CommandPayload::Slide { .. }
        | CommandPayload::Zoom { .. }
        | CommandPayload::Stinger { .. }
        | CommandPayload::ConfigureStinger { .. }
        | CommandPayload::RemoveStinger { .. }
        | CommandPayload::Wipe { .. }
        | CommandPayload::FadeToBlack { .. }
        | CommandPayload::StartManualTransition { .. }
        | CommandPayload::SetManualTransitionPosition { .. }
        | CommandPayload::CommitManualTransition
        | CommandPayload::CancelManualTransition => CommandClass::Transition,
    }
}

fn engine_command(payload: CommandPayload) -> EngineCommand {
    match payload {
        CommandPayload::SelectPreview { input } => EngineCommand::SelectPreview(input.to_domain()),
        CommandPayload::Cut => EngineCommand::Cut,
        CommandPayload::Fade { duration_frames } => EngineCommand::Fade { duration_frames },
        CommandPayload::AlphaFade { duration_frames } => {
            EngineCommand::AlphaFade { duration_frames }
        }
        CommandPayload::Slide { duration_frames } => EngineCommand::Slide { duration_frames },
        CommandPayload::Zoom { duration_frames } => EngineCommand::Zoom { duration_frames },
        CommandPayload::Stinger {
            slot,
            duration_frames,
        } => EngineCommand::Stinger {
            slot: fm_switcher::StingerSlotId::new(slot.number())
                .expect("wire Stinger slots are bounded"),
            duration_frames,
        },
        CommandPayload::ConfigureStinger {
            slot,
            media_input,
            preload,
            cut_point_frames,
            audio_policy,
            missing_media_fallback,
        } => EngineCommand::ConfigureStinger {
            slot: StingerSlotId::new(slot.number()).expect("wire Stinger slots are bounded"),
            descriptor: StingerDescriptor::new(
                media_input.to_domain(),
                preload,
                cut_point_frames,
                match audio_policy {
                    ProtocolStingerAudioPolicy::Muted => StingerAudioPolicy::Muted,
                    ProtocolStingerAudioPolicy::StingerOnly => StingerAudioPolicy::StingerOnly,
                    ProtocolStingerAudioPolicy::MixWithProgram => {
                        StingerAudioPolicy::MixWithProgram
                    }
                },
                match missing_media_fallback {
                    ProtocolStingerFallback::Cut => MissingMediaFallback::Cut,
                    ProtocolStingerFallback::Fade => MissingMediaFallback::Fade,
                    ProtocolStingerFallback::KeepProgram => MissingMediaFallback::KeepProgram,
                },
            ),
        },
        CommandPayload::RemoveStinger { slot } => EngineCommand::RemoveStinger {
            slot: StingerSlotId::new(slot.number()).expect("wire Stinger slots are bounded"),
        },
        CommandPayload::Wipe { duration_frames } => EngineCommand::Wipe { duration_frames },
        CommandPayload::FadeToBlack {
            active,
            duration_frames,
        } => EngineCommand::FadeToBlack {
            active,
            duration_frames,
        },
        CommandPayload::StartManualTransition { kind } => EngineCommand::StartManualTransition {
            kind: match kind {
                ManualTransitionKind::Fade => EngineManualTransitionKind::Fade,
                ManualTransitionKind::Wipe => EngineManualTransitionKind::Wipe,
                ManualTransitionKind::AlphaFade => EngineManualTransitionKind::AlphaFade,
            },
        },
        CommandPayload::SetManualTransitionPosition { position } => {
            EngineCommand::SetManualTransitionPosition {
                position: engine_manual_position(position),
            }
        }
        CommandPayload::CommitManualTransition => EngineCommand::CommitManualTransition,
        CommandPayload::CancelManualTransition => EngineCommand::CancelManualTransition,
    }
}

const fn is_program_transition(command: EngineCommand) -> bool {
    matches!(
        command,
        EngineCommand::Fade { .. }
            | EngineCommand::AlphaFade { .. }
            | EngineCommand::Slide { .. }
            | EngineCommand::Zoom { .. }
            | EngineCommand::Stinger { .. }
            | EngineCommand::Wipe { .. }
    )
}

fn engine_manual_position(
    position: fm_protocol::ManualTransitionPosition,
) -> EngineManualTransitionPosition {
    EngineManualTransitionPosition::new(position.basis_points())
        .expect("protocol manual position is bounded")
}

fn command_result(receipt: &CommandReceipt<EngineAcceptance>) -> CommandResult {
    match receipt {
        CommandReceipt::Accepted {
            command_id,
            acceptance,
        } => CommandResult::Accepted {
            id: command_id.as_str().to_owned(),
            revision: acceptance.revision.get(),
            scheduled_frame: Some(acceptance.result.target_frame.get()),
        },
        CommandReceipt::Rejected {
            command_id,
            rejection,
        } => CommandResult::Rejected {
            id: command_id.as_str().to_owned(),
            code: rejection.rejection.code.as_str().to_owned(),
            message: rejection.rejection.message.clone(),
            fields: rejection
                .rejection
                .fields
                .iter()
                .map(|field| FieldIssue {
                    field: field.field.clone(),
                    code: field.code.clone(),
                    message: field.message.clone(),
                })
                .collect(),
            current_revision: rejection.current_revision.get(),
            retryable: rejection.rejection.retryable,
        },
    }
}

fn engine_submission(
    identity: &EngineIdentity,
    idempotency_key: IdempotencyKey,
    engine: &Engine,
    outcome: &EngineCommandOutcome,
) -> CommandSubmission {
    let result = command_result(&outcome.receipt);
    let events = if !outcome.replayed && outcome.receipt.accepted().is_some() {
        let stinger_slots_changed = outcome.events.iter().any(|event| {
            matches!(
                event.payload,
                EngineEvent::DesiredSwitcherChanged(
                    EngineCommand::ConfigureStinger { .. } | EngineCommand::RemoveStinger { .. }
                )
            )
        });
        let program = WireInputId::from_domain(engine.show().desired_switcher().program());
        let preview = WireInputId::from_domain(engine.show().desired_switcher().preview());
        let manual_transition = Some(protocol_manual_status(engine.desired_manual_transition()));
        let fade_to_black = Some(protocol_fade_to_black_state(engine.desired_fade_to_black()));
        vec![EventMessage {
            cursor: EventCursor {
                engine: identity.clone(),
                revision: engine.revision().get(),
            },
            payload: if stinger_slots_changed {
                EventPayload::StingerSlotsChanged {
                    program,
                    preview,
                    manual_transition,
                    fade_to_black,
                    stingers: protocol_desired_stingers(engine),
                }
            } else {
                EventPayload::DesiredSwitcher {
                    program,
                    preview,
                    manual_transition,
                    fade_to_black,
                }
            },
        }]
    } else {
        Vec::new()
    };
    let accepted = if !outcome.replayed && outcome.receipt.accepted().is_some() {
        Some(AcceptedOutcome {
            idempotency_key,
            receipt: outcome.receipt.clone(),
            events: outcome.events.clone(),
        })
    } else {
        None
    };
    CommandSubmission {
        output: ResultBeforeEvents { result, events },
        replayed: outcome.replayed,
        accepted,
        subscriber_failures: Vec::new(),
    }
}

fn project_engine(engine: &Engine, ticks: u64) -> Result<EngineSnapshot, EngineError> {
    let mut projected = engine.clone();
    for _ in 0..ticks {
        projected.tick()?;
    }
    projected.snapshot().map_err(EngineError::from)
}

fn snapshot_record(engine: &Engine, identity: &EngineIdentity) -> SnapshotRecord {
    let snapshot = SnapshotMessage {
        engine: identity.clone(),
        revision: engine.revision().get(),
        show_name: engine.show().name().to_owned(),
        inputs: engine
            .show()
            .inputs()
            .iter()
            .copied()
            .map(WireInputId::from_domain)
            .collect(),
        desired_program: WireInputId::from_domain(engine.show().desired_switcher().program()),
        desired_preview: WireInputId::from_domain(engine.show().desired_switcher().preview()),
        realized_program: WireInputId::from_domain(engine.realized_switcher().program()),
        realized_preview: WireInputId::from_domain(engine.realized_switcher().preview()),
        desired_manual_transition: Some(protocol_manual_status(engine.desired_manual_transition())),
        realized_manual_transition: Some(protocol_manual_status(
            engine.realized_manual_transition(),
        )),
        desired_fade_to_black: Some(protocol_fade_to_black_state(engine.desired_fade_to_black())),
        realized_fade_to_black: Some(protocol_fade_to_black_state(
            engine.realized_fade_to_black(),
        )),
        stingers: Some(protocol_stingers(engine)),
    };
    SnapshotRecord {
        cursor: EventCursor {
            engine: identity.clone(),
            revision: snapshot.revision,
        },
        snapshot,
    }
}

fn protocol_stingers(engine: &Engine) -> Vec<StingerStatus> {
    engine
        .show()
        .desired_switcher()
        .stingers()
        .iter()
        .enumerate()
        .filter_map(|(index, desired)| {
            let descriptor = desired.descriptor()?;
            let slot = StingerSlotId::from_index(index).expect("Stinger array index is bounded");
            let realized = engine.realized_switcher().stinger(slot);
            Some(protocol_stinger_status(
                slot,
                descriptor,
                if realized.descriptor() == Some(descriptor) {
                    realized.preload_state()
                } else {
                    StingerPreloadState::NotRequested
                },
            ))
        })
        .collect()
}

fn protocol_stinger_status(
    slot: StingerSlotId,
    descriptor: &StingerDescriptor,
    readiness: StingerPreloadState,
) -> StingerStatus {
    StingerStatus {
        slot: WireStingerSlotId::new(slot.number()).expect("domain Stinger slot is bounded"),
        media_input: WireInputId::from_domain(descriptor.media_input),
        preload: descriptor.preload,
        cut_point_frames: descriptor.cut_point_frames,
        audio_policy: match descriptor.audio_policy {
            StingerAudioPolicy::Muted => ProtocolStingerAudioPolicy::Muted,
            StingerAudioPolicy::StingerOnly => ProtocolStingerAudioPolicy::StingerOnly,
            StingerAudioPolicy::MixWithProgram => ProtocolStingerAudioPolicy::MixWithProgram,
        },
        missing_media_fallback: match descriptor.missing_media_fallback {
            MissingMediaFallback::Cut => ProtocolStingerFallback::Cut,
            MissingMediaFallback::Fade => ProtocolStingerFallback::Fade,
            MissingMediaFallback::KeepProgram => ProtocolStingerFallback::KeepProgram,
        },
        readiness: match readiness {
            StingerPreloadState::NotRequested => StingerReadiness::NotRequested,
            StingerPreloadState::Ready => StingerReadiness::Ready,
            StingerPreloadState::Missing => StingerReadiness::Missing,
        },
    }
}

fn protocol_desired_stingers(engine: &Engine) -> Vec<StingerStatus> {
    engine
        .show()
        .desired_switcher()
        .stingers()
        .iter()
        .enumerate()
        .filter_map(|(index, desired)| {
            let descriptor = desired.descriptor()?;
            let slot = StingerSlotId::from_index(index).expect("Stinger array index is bounded");
            Some(protocol_stinger_status(
                slot,
                descriptor,
                desired.preload_state(),
            ))
        })
        .collect()
}

fn protocol_manual_status(state: Option<EngineManualTransitionState>) -> ManualTransitionStatus {
    let Some(state) = state else {
        return ManualTransitionStatus::Inactive;
    };
    let kind = match state.kind {
        EngineManualTransitionKind::Fade => ManualTransitionKind::Fade,
        EngineManualTransitionKind::Wipe => ManualTransitionKind::Wipe,
        EngineManualTransitionKind::AlphaFade => ManualTransitionKind::AlphaFade,
    };
    ManualTransitionStatus::Active(ManualTransitionState {
        kind,
        from: WireInputId::from_domain(state.from),
        to: WireInputId::from_domain(state.to),
        interval_start: ManualTransitionPosition::new(state.interval_start.basis_points())
            .expect("engine manual transition positions are bounded"),
        position: ManualTransitionPosition::new(state.position.basis_points())
            .expect("engine manual transition positions are bounded"),
    })
}

fn protocol_fade_to_black_state(state: EngineFadeToBlackState) -> FadeToBlackState {
    FadeToBlackState {
        target_active: state.active,
        position: FadeToBlackPosition::new(
            u16::try_from(state.position.numerator())
                .expect("engine Fade-to-Black positions use a u16 denominator"),
        ),
    }
}

#[cfg(test)]
mod tests;
