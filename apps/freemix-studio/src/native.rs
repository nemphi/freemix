use std::{
    collections::VecDeque,
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use eframe::egui;
use fm_client::{ClientError, CommandStatus, SessionEvent, SyncMode, TcpSessionError};
use fm_protocol::{CommandPayload, CommandResult, DurableGap, WireInputId, WireMessage};
use fm_ui_egui::{StudioConnectionStatus, StudioIntent, StudioShell, StudioUiState};

use crate::{LifecycleState, StudioConfig, StudioRuntime};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(not(test))]
const PEER_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const PEER_WAIT_TIMEOUT: Duration = Duration::from_millis(200);
const IO_POLL_INTERVAL: Duration = Duration::from_millis(25);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const REQUEST_CAPACITY: usize = 16;
const DEFERRED_INTENT_CAPACITY: usize = 16;
const STATE_CAPACITY: usize = 16;
const MAX_COMMAND_RECORDS: usize = 8;
static NEXT_WORKER_NONCE: AtomicU64 = AtomicU64::new(1);

/// Opens the cross-platform native Studio window.
///
/// # Errors
///
/// Returns an error when the native event loop or renderer cannot start.
pub fn launch_native(config: StudioConfig) -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([760.0, 560.0]),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "FreeMix Studio",
        options,
        Box::new(move |creation_context| Ok(Box::new(StudioApp::new(config, creation_context)?))),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkerRequest {
    Intent(StudioIntent),
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnqueueError {
    Full,
    Disconnected,
}

impl fmt::Display for EnqueueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => formatter.write_str("Studio command queue is full"),
            Self::Disconnected => formatter.write_str("Studio worker is disconnected"),
        }
    }
}

fn try_enqueue(
    sender: &SyncSender<WorkerRequest>,
    request: WorkerRequest,
) -> Result<(), EnqueueError> {
    sender.try_send(request).map_err(|error| match error {
        TrySendError::Full(_) => EnqueueError::Full,
        TrySendError::Disconnected(_) => EnqueueError::Disconnected,
    })
}

struct StudioApp {
    shell: StudioShell,
    state: StudioUiState,
    requests: Option<SyncSender<WorkerRequest>>,
    updates: Option<Receiver<StudioUiState>>,
    worker: Option<JoinHandle<()>>,
    shutdown_sent: bool,
}

impl StudioApp {
    fn new(
        config: StudioConfig,
        creation_context: &eframe::CreationContext<'_>,
    ) -> Result<Self, std::io::Error> {
        let (request_sender, request_receiver) = sync_channel(REQUEST_CAPACITY);
        let (state_sender, state_receiver) = sync_channel(STATE_CAPACITY);
        let repaint_context = creation_context.egui_ctx.clone();
        let worker = thread::Builder::new()
            .name("freemix-studio-worker".to_owned())
            .spawn(move || {
                let publisher = StatePublisher {
                    sender: state_sender,
                    repaint_context,
                };
                run_worker(config, &request_receiver, &publisher);
            })?;
        Ok(Self {
            shell: StudioShell::default(),
            state: StudioUiState::new(StudioConnectionStatus::Launching),
            requests: Some(request_sender),
            updates: Some(state_receiver),
            worker: Some(worker),
            shutdown_sent: false,
        })
    }

    fn report_enqueue_error(&mut self, error: EnqueueError) {
        self.state.error = Some(error.to_string());
    }

    fn send_shutdown(&mut self) {
        if !self.shutdown_sent {
            if let Some(requests) = &self.requests {
                let _ = try_enqueue(requests, WorkerRequest::Shutdown);
            }
            self.shutdown_sent = true;
        }
    }
}

impl eframe::App for StudioApp {
    fn logic(&mut self, _context: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(updates) = &self.updates {
            while let Ok(state) = updates.try_recv() {
                self.state = state;
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        for intent in self.shell.draw(ui, &self.state) {
            if let Some(requests) = &self.requests
                && let Err(error) = try_enqueue(requests, WorkerRequest::Intent(intent))
            {
                self.report_enqueue_error(error);
            }
        }
    }

    fn on_exit(&mut self) {
        self.send_shutdown();
    }
}

impl Drop for StudioApp {
    fn drop(&mut self) {
        self.send_shutdown();
        self.requests.take();
        self.updates.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct StatePublisher {
    sender: SyncSender<StudioUiState>,
    repaint_context: egui::Context,
}

impl StatePublisher {
    fn publish(&self, state: StudioUiState) -> bool {
        if self.sender.send(state).is_err() {
            return false;
        }
        self.repaint_context.request_repaint();
        true
    }
}

#[derive(Debug)]
struct IdempotencyKeys {
    nonce: String,
    next: u64,
}

impl IdempotencyKeys {
    fn new(nonce: impl Into<String>) -> Self {
        Self {
            nonce: nonce.into(),
            next: 0,
        }
    }

    fn next(&mut self) -> Option<String> {
        self.next = self.next.checked_add(1)?;
        Some(format!("{}:{}", self.nonce, self.next))
    }
}

fn worker_nonce() -> String {
    let started_at_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let sequence = NEXT_WORKER_NONCE.fetch_add(1, Ordering::Relaxed);
    format!(
        "freemix-studio-{}-{started_at_nanos}-{sequence}",
        std::process::id()
    )
}

#[derive(Clone, Copy, Debug)]
struct ReconnectWait {
    started: Instant,
    delay: Duration,
}

impl ReconnectWait {
    fn from_runtime(runtime: &StudioRuntime) -> Option<Self> {
        runtime.session().reconnect_backoff().map(|backoff| Self {
            started: Instant::now(),
            delay: Duration::from_millis(backoff.delay_ms),
        })
    }

    fn remaining(self) -> Duration {
        self.delay.saturating_sub(self.started.elapsed())
    }
}

#[derive(Debug)]
enum WorkerFailure {
    Transport(String),
    Resync(String),
    Fatal(String),
    Shutdown,
}

impl WorkerFailure {
    fn message(&self) -> &str {
        match self {
            Self::Transport(message) | Self::Resync(message) | Self::Fatal(message) => message,
            Self::Shutdown => "Studio worker shut down",
        }
    }

    const fn is_recoverable(&self) -> bool {
        matches!(self, Self::Transport(_) | Self::Resync(_))
    }

    const fn invalidates_realization(&self) -> bool {
        self.is_recoverable()
    }
}

#[derive(Debug)]
struct WorkerRecovery {
    pending_command: Option<String>,
    deferred_intents: VecDeque<StudioIntent>,
    reconnect_wait: Option<ReconnectWait>,
    visible_error: Option<String>,
    realization_uncertain: bool,
    deferred_rejections: usize,
}

impl WorkerRecovery {
    fn new(visible_error: Option<String>, reconnect_wait: Option<ReconnectWait>) -> Self {
        Self {
            pending_command: None,
            deferred_intents: VecDeque::new(),
            reconnect_wait,
            visible_error,
            realization_uncertain: false,
            deferred_rejections: 0,
        }
    }

    fn active(&self) -> bool {
        self.reconnect_wait.is_some() || self.pending_command.is_some()
    }

    fn defer_intent(
        &mut self,
        runtime: &mut StudioRuntime,
        intent: StudioIntent,
        publisher: &StatePublisher,
    ) -> bool {
        if self.deferred_intents.len() == DEFERRED_INTENT_CAPACITY {
            self.deferred_rejections = self.deferred_rejections.saturating_add(1);
            return publish_runtime(runtime, publisher, self.error());
        }
        self.deferred_intents.push_back(intent);
        publish_deferred_runtime(
            runtime,
            publisher,
            self.deferred_intents.len(),
            self.error(),
        )
    }

    fn error(&self) -> Option<String> {
        combined_error(self.visible_error.clone(), self.deferred_rejections)
    }
}

fn combined_error(transient: Option<String>, deferred_rejections: usize) -> Option<String> {
    let overflow = (deferred_rejections > 0).then(|| {
        format!(
            "Rejected {deferred_rejections} command(s): Studio deferred command queue reached capacity {DEFERRED_INTENT_CAPACITY}"
        )
    });
    match (transient, overflow) {
        (Some(transient), Some(overflow)) => Some(format!("{transient}; {overflow}")),
        (Some(transient), None) => Some(transient),
        (None, overflow) => overflow,
    }
}

fn cancellation_requested(
    requests: &Receiver<WorkerRequest>,
    deferred_intents: &mut VecDeque<StudioIntent>,
    deferred_rejections: &mut usize,
) -> bool {
    match requests.try_recv() {
        Ok(WorkerRequest::Shutdown) | Err(TryRecvError::Disconnected) => true,
        Ok(WorkerRequest::Intent(intent)) => {
            if deferred_intents.len() < DEFERRED_INTENT_CAPACITY {
                deferred_intents.push_back(intent);
            } else {
                *deferred_rejections = deferred_rejections.saturating_add(1);
            }
            false
        }
        Err(TryRecvError::Empty) => false,
    }
}

fn connect_worker(
    runtime: &mut StudioRuntime,
    requests: &Receiver<WorkerRequest>,
    deferred_intents: &mut VecDeque<StudioIntent>,
    deferred_rejections: &mut usize,
) -> Option<Result<SessionEvent, crate::StudioError>> {
    let started = Instant::now();
    let mut shutdown = false;
    let result = runtime.connect_cancellable(CONNECT_TIMEOUT, IO_POLL_INTERVAL, || {
        shutdown = cancellation_requested(requests, deferred_intents, deferred_rejections);
        shutdown || started.elapsed() >= PEER_WAIT_TIMEOUT
    });
    (!shutdown).then_some(result)
}

fn reconnect_worker(
    runtime: &mut StudioRuntime,
    elapsed_backoff: Duration,
    requests: &Receiver<WorkerRequest>,
    deferred_intents: &mut VecDeque<StudioIntent>,
    deferred_rejections: &mut usize,
) -> Option<Result<SessionEvent, crate::StudioError>> {
    let started = Instant::now();
    let mut shutdown = false;
    let result =
        runtime.reconnect_cancellable(elapsed_backoff, CONNECT_TIMEOUT, IO_POLL_INTERVAL, || {
            shutdown = cancellation_requested(requests, deferred_intents, deferred_rejections);
            shutdown || started.elapsed() >= PEER_WAIT_TIMEOUT
        });
    (!shutdown).then_some(result)
}

fn start_worker_runtime(
    config: StudioConfig,
    requests: &Receiver<WorkerRequest>,
    deferred_intents: &mut VecDeque<StudioIntent>,
    deferred_rejections: &mut usize,
) -> Option<Result<StudioRuntime, crate::StudioError>> {
    let started = Instant::now();
    let mut shutdown = false;
    let result = StudioRuntime::new_cancellable(config, IO_POLL_INTERVAL, || {
        shutdown = cancellation_requested(requests, deferred_intents, deferred_rejections);
        shutdown || started.elapsed() >= PEER_WAIT_TIMEOUT
    });
    (!shutdown).then_some(result)
}

fn startup_failure_message(error: &crate::StudioError, deferred_rejections: usize) -> String {
    combined_error(
        Some(format!("Studio startup failed: {error}")),
        deferred_rejections,
    )
    .expect("startup failure always has an error")
}

fn initialize_worker(
    config: StudioConfig,
    requests: &Receiver<WorkerRequest>,
    publisher: &StatePublisher,
) -> Option<(StudioRuntime, WorkerRecovery)> {
    if !publisher.publish(StudioUiState::new(StudioConnectionStatus::Launching)) {
        return None;
    }
    let mut deferred_intents = VecDeque::new();
    let mut deferred_rejections = 0;
    let startup_result = start_worker_runtime(
        config,
        requests,
        &mut deferred_intents,
        &mut deferred_rejections,
    )?;
    let mut runtime = match startup_result {
        Ok(runtime) => runtime,
        Err(error) => {
            let mut state = StudioUiState::new(StudioConnectionStatus::Failed);
            state.error = Some(startup_failure_message(&error, deferred_rejections));
            publisher.publish(state);
            return None;
        }
    };
    if !publish_runtime(
        &mut runtime,
        publisher,
        combined_error(None, deferred_rejections),
    ) {
        return None;
    }
    let connect_result = connect_worker(
        &mut runtime,
        requests,
        &mut deferred_intents,
        &mut deferred_rejections,
    )?;
    let (visible_error, reconnect_wait) = match connect_result {
        Ok(_) => (None, None),
        Err(error) => {
            let reconnect_wait = is_recoverable_failure(&error)
                .then(|| ReconnectWait::from_runtime(&runtime))
                .flatten();
            (Some(format!("Connection failed: {error}")), reconnect_wait)
        }
    };
    let mut recovery = WorkerRecovery::new(visible_error, reconnect_wait);
    recovery.deferred_intents = deferred_intents;
    recovery.deferred_rejections = deferred_rejections;
    if !publish_runtime(&mut runtime, publisher, recovery.error()) {
        return None;
    }
    Some((runtime, recovery))
}

fn run_worker(
    config: StudioConfig,
    requests: &Receiver<WorkerRequest>,
    publisher: &StatePublisher,
) {
    let Some((mut runtime, mut recovery)) = initialize_worker(config, requests, publisher) else {
        return;
    };
    let started = Instant::now();
    let mut next_heartbeat = Instant::now() + HEARTBEAT_INTERVAL;
    let mut keys = IdempotencyKeys::new(worker_nonce());

    // Idle receive is unnecessary: the current single-client daemon has no
    // unsolicited broadcasts. Active waits below remain cancellable.
    loop {
        if !recovery.active()
            && let Some(intent) = recovery.deferred_intents.pop_front()
        {
            if !handle_worker_intent(
                &mut runtime,
                intent,
                &mut keys,
                requests,
                publisher,
                &mut recovery,
            ) {
                break;
            }
            continue;
        }
        let now = Instant::now();
        let timeout = recovery.reconnect_wait.map_or_else(
            || next_heartbeat.saturating_duration_since(now),
            ReconnectWait::remaining,
        );
        match requests.recv_timeout(timeout) {
            Ok(WorkerRequest::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(WorkerRequest::Intent(intent)) => {
                if recovery.active() {
                    if !recovery.defer_intent(&mut runtime, intent, publisher) {
                        break;
                    }
                    continue;
                }
                if !handle_worker_intent(
                    &mut runtime,
                    intent,
                    &mut keys,
                    requests,
                    publisher,
                    &mut recovery,
                ) {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if !handle_worker_timeout(
                    &mut runtime,
                    requests,
                    publisher,
                    &mut recovery,
                    &mut next_heartbeat,
                    started,
                ) {
                    break;
                }
            }
        }
    }
}

fn handle_worker_intent(
    runtime: &mut StudioRuntime,
    intent: StudioIntent,
    keys: &mut IdempotencyKeys,
    requests: &Receiver<WorkerRequest>,
    publisher: &StatePublisher,
    recovery: &mut WorkerRecovery,
) -> bool {
    let command_id = match begin_intent(runtime, intent, keys, publisher, recovery.error()) {
        Ok(command_id) => command_id,
        Err(error) => {
            recovery.visible_error = Some(error);
            return publish_runtime(runtime, publisher, recovery.error());
        }
    };
    recovery.pending_command = Some(command_id.clone());
    let result = flush_worker(runtime, requests, recovery).and_then(|()| {
        consume_command_sequence(
            runtime,
            &command_id,
            publisher,
            true,
            requests,
            &mut recovery.deferred_intents,
            &mut recovery.deferred_rejections,
        )
    });
    match result {
        Ok(()) => {
            recovery.pending_command = None;
            recovery.visible_error = None;
        }
        Err(WorkerFailure::Shutdown) => return false,
        Err(error) => {
            recovery.visible_error = Some(error.message().to_owned());
            if error.invalidates_realization() {
                recovery.realization_uncertain = true;
            }
            recovery.reconnect_wait = if error.is_recoverable() {
                ReconnectWait::from_runtime(runtime)
            } else {
                recovery.pending_command = None;
                None
            };
        }
    }
    recovery.error().is_none() || publish_runtime(runtime, publisher, recovery.error())
}

fn flush_worker(
    runtime: &mut StudioRuntime,
    requests: &Receiver<WorkerRequest>,
    recovery: &mut WorkerRecovery,
) -> Result<(), WorkerFailure> {
    let started = Instant::now();
    let mut shutdown = false;
    let result = runtime.flush_cancellable(IO_POLL_INTERVAL, || {
        shutdown = cancellation_requested(
            requests,
            &mut recovery.deferred_intents,
            &mut recovery.deferred_rejections,
        );
        shutdown || started.elapsed() >= PEER_WAIT_TIMEOUT
    });
    if shutdown {
        Err(WorkerFailure::Shutdown)
    } else {
        result
            .map(|_| ())
            .map_err(|error| worker_error("Could not send command", &error))
    }
}

fn handle_worker_timeout(
    runtime: &mut StudioRuntime,
    requests: &Receiver<WorkerRequest>,
    publisher: &StatePublisher,
    recovery: &mut WorkerRecovery,
    next_heartbeat: &mut Instant,
    started: Instant,
) -> bool {
    if let Some(wait) = recovery.reconnect_wait {
        if !wait.remaining().is_zero() {
            return true;
        }
        let Some(reconnect_result) = reconnect_worker(
            runtime,
            wait.started.elapsed(),
            requests,
            &mut recovery.deferred_intents,
            &mut recovery.deferred_rejections,
        ) else {
            return false;
        };
        match reconnect_result {
            Ok(SessionEvent::Connected { mode }) => {
                recovery.reconnect_wait = None;
                recovery.visible_error = None;
                *next_heartbeat = Instant::now() + HEARTBEAT_INTERVAL;
                if mode == SyncMode::Snapshot {
                    recovery.realization_uncertain = false;
                }
                if !recovery.realization_uncertain
                    && !publish_runtime(runtime, publisher, recovery.error())
                {
                    return false;
                }

                if !resume_pending_command(runtime, requests, publisher, recovery) {
                    return false;
                }
            }
            Ok(other) => {
                recovery.visible_error = Some(format!("Unexpected reconnect result: {other:?}"));
                recovery.pending_command = None;
                recovery.reconnect_wait = None;
            }
            Err(error) => {
                recovery.visible_error = Some(format!("Reconnect failed: {error}"));
                if is_resync_failure(&error) {
                    recovery.realization_uncertain = true;
                }
                recovery.reconnect_wait = if is_recoverable_failure(&error) {
                    ReconnectWait::from_runtime(runtime)
                } else {
                    recovery.pending_command = None;
                    None
                };
            }
        }
        return publish_runtime(runtime, publisher, recovery.error());
    }

    handle_heartbeat_timeout(
        runtime,
        requests,
        publisher,
        recovery,
        next_heartbeat,
        started,
    )
}

fn resume_pending_command(
    runtime: &mut StudioRuntime,
    requests: &Receiver<WorkerRequest>,
    publisher: &StatePublisher,
    recovery: &mut WorkerRecovery,
) -> bool {
    if let Some(command_id) = recovery.pending_command.clone() {
        let completed = runtime
            .session()
            .client()
            .command(&command_id)
            .is_some_and(|record| matches!(record.status, CommandStatus::Completed(_)));
        let result = if completed {
            Ok(())
        } else {
            consume_command_sequence(
                runtime,
                &command_id,
                publisher,
                !recovery.realization_uncertain,
                requests,
                &mut recovery.deferred_intents,
                &mut recovery.deferred_rejections,
            )
        };
        match result {
            Ok(()) => recovery.pending_command = None,
            Err(WorkerFailure::Shutdown) => return false,
            Err(error) => {
                recovery.visible_error = Some(error.message().to_owned());
                if error.invalidates_realization() {
                    recovery.realization_uncertain = true;
                }
                recovery.reconnect_wait = if error.is_recoverable() {
                    ReconnectWait::from_runtime(runtime)
                } else {
                    recovery.pending_command = None;
                    None
                };
            }
        }
    }
    if recovery.reconnect_wait.is_none() && recovery.realization_uncertain {
        match require_snapshot_reconnect(runtime) {
            Ok(wait) => {
                recovery.reconnect_wait = Some(wait);
                recovery.visible_error = Some(
                    "Runtime realization changed during reconnect; requesting snapshot".to_owned(),
                );
            }
            Err(error) => {
                recovery.visible_error = Some(error.message().to_owned());
                recovery.pending_command = None;
            }
        }
    }
    true
}

fn handle_heartbeat_timeout(
    runtime: &mut StudioRuntime,
    requests: &Receiver<WorkerRequest>,
    publisher: &StatePublisher,
    recovery: &mut WorkerRecovery,
    next_heartbeat: &mut Instant,
    started: Instant,
) -> bool {
    if matches!(runtime.lifecycle(), Ok(LifecycleState::Ready)) {
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let wait_started = Instant::now();
        let mut shutdown = false;
        let result = runtime.send_heartbeat_cancellable(elapsed_ms, IO_POLL_INTERVAL, || {
            shutdown = cancellation_requested(
                requests,
                &mut recovery.deferred_intents,
                &mut recovery.deferred_rejections,
            );
            shutdown || wait_started.elapsed() >= PEER_WAIT_TIMEOUT
        });
        if shutdown {
            return false;
        }
        if let Err(error) = result {
            recovery.visible_error = Some(format!("Heartbeat failed: {error}"));
            if is_recoverable_failure(&error) {
                recovery.realization_uncertain =
                    runtime.session().client().model().view().is_some();
                recovery.reconnect_wait = ReconnectWait::from_runtime(runtime);
            }
            if !publish_runtime(runtime, publisher, recovery.error()) {
                return false;
            }
        }
    }
    *next_heartbeat = Instant::now() + HEARTBEAT_INTERVAL;
    true
}

fn begin_intent(
    runtime: &mut StudioRuntime,
    intent: StudioIntent,
    keys: &mut IdempotencyKeys,
    publisher: &StatePublisher,
    persistent_error: Option<String>,
) -> Result<String, String> {
    let payload = intent_payload(intent);
    let expected_revision = runtime
        .session()
        .client()
        .model()
        .view()
        .map(|view| view.cursor.revision.get())
        .ok_or_else(|| "Cannot send a command before project state is synchronized".to_owned())?;
    let key = keys
        .next()
        .ok_or_else(|| "Studio idempotency key space is exhausted".to_owned())?;
    let command = runtime
        .queue_command(payload, key, Some(expected_revision), None)
        .map_err(|error| format!("Could not queue command: {error}"))?;

    // This publication exposes SelectPreview's optimistic desired state before I/O.
    if !publish_runtime(runtime, publisher, persistent_error) {
        return Err("Studio UI disconnected".to_owned());
    }
    Ok(command.id)
}

fn consume_command_sequence(
    runtime: &mut StudioRuntime,
    command_id: &str,
    publisher: &StatePublisher,
    publish_updates: bool,
    requests: &Receiver<WorkerRequest>,
    deferred_intents: &mut VecDeque<StudioIntent>,
    deferred_rejections: &mut usize,
) -> Result<(), WorkerFailure> {
    let deadline = Instant::now()
        .checked_add(PEER_WAIT_TIMEOUT)
        .expect("peer wait timeout must fit in Instant");
    let mut consumed = 0;
    count_record(&mut consumed)?;
    let result = match receive_command_event(
        runtime,
        requests,
        deferred_intents,
        deferred_rejections,
        deadline,
    )? {
        SessionEvent::CommandResult { result, .. } => result,
        other => return Err(unexpected_failure("command result", &other)),
    };
    if result_id(&result) != command_id {
        return Err(WorkerFailure::Fatal(format!(
            "Unexpected command result ID {:?}; expected {command_id:?}",
            result_id(&result)
        )));
    }
    let accepted_revision = match &result {
        CommandResult::Accepted { revision, .. } => *revision,
        CommandResult::Rejected { code, message, .. } => {
            if publish_updates {
                publish_runtime(
                    runtime,
                    publisher,
                    combined_error(
                        Some(format!("Command rejected ({code}): {message}")),
                        *deferred_rejections,
                    ),
                );
            }
            return Ok(());
        }
    };
    publish_command_update(runtime, publisher, publish_updates, *deferred_rejections)?;
    if runtime
        .session()
        .client()
        .last_applied_cursor()
        .is_some_and(|cursor| cursor.revision >= accepted_revision)
    {
        return Ok(());
    }

    count_record(&mut consumed)?;
    match receive_command_event(
        runtime,
        requests,
        deferred_intents,
        deferred_rejections,
        deadline,
    )? {
        SessionEvent::Event { event, .. } if event.cursor.revision == accepted_revision => {}
        SessionEvent::Event { event, .. } => {
            return Err(WorkerFailure::Fatal(format!(
                "Unexpected durable event revision {}; expected {accepted_revision}",
                event.cursor.revision
            )));
        }
        other => return Err(unexpected_failure("durable event", &other)),
    }
    publish_command_update(runtime, publisher, publish_updates, *deferred_rejections)?;

    count_record(&mut consumed)?;
    match receive_command_event(
        runtime,
        requests,
        deferred_intents,
        deferred_rejections,
        deadline,
    )? {
        SessionEvent::RuntimeEvent { event, .. } if event.revision == accepted_revision => {}
        SessionEvent::RuntimeEvent { event, .. } => {
            return Err(WorkerFailure::Fatal(format!(
                "Unexpected runtime event revision {}; expected {accepted_revision}",
                event.revision
            )));
        }
        other => return Err(unexpected_failure("runtime event", &other)),
    }
    publish_command_update(runtime, publisher, publish_updates, *deferred_rejections)?;
    Ok(())
}

fn publish_command_update(
    runtime: &mut StudioRuntime,
    publisher: &StatePublisher,
    publish: bool,
    deferred_rejections: usize,
) -> Result<(), WorkerFailure> {
    if publish
        && !publish_runtime(
            runtime,
            publisher,
            combined_error(None, deferred_rejections),
        )
    {
        Err(WorkerFailure::Fatal("Studio UI disconnected".to_owned()))
    } else {
        Ok(())
    }
}

fn count_record(consumed: &mut usize) -> Result<(), WorkerFailure> {
    *consumed = consumed.saturating_add(1);
    if *consumed > MAX_COMMAND_RECORDS {
        Err(WorkerFailure::Fatal(
            "Command response exceeded the record limit".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn receive_command_event(
    runtime: &mut StudioRuntime,
    requests: &Receiver<WorkerRequest>,
    deferred_intents: &mut VecDeque<StudioIntent>,
    deferred_rejections: &mut usize,
    deadline: Instant,
) -> Result<SessionEvent, WorkerFailure> {
    let mut shutdown = false;
    let result = runtime.receive_cancellable(IO_POLL_INTERVAL, || {
        shutdown = cancellation_requested(requests, deferred_intents, deferred_rejections);
        shutdown || Instant::now() >= deadline
    });
    if shutdown {
        Err(WorkerFailure::Shutdown)
    } else {
        result.map_err(|error| worker_error("Command response failed", &error))
    }
}

fn require_snapshot_reconnect(runtime: &mut StudioRuntime) -> Result<ReconnectWait, WorkerFailure> {
    let client = runtime.session().client();
    let server = client
        .session()
        .map(|session| session.server.clone())
        .ok_or_else(|| {
            WorkerFailure::Fatal("Cannot request snapshot without a session".to_owned())
        })?;
    let revision = client
        .last_applied_cursor()
        .map_or(0, |cursor| cursor.revision);
    // Reuse the client's snapshot-only gap transition so unresolved command
    // records survive the extra reconnect needed to refresh runtime state.
    let gap = DurableGap {
        server,
        requested_after_revision: revision.saturating_sub(2),
        available_from_revision: revision,
        current_revision: revision,
    };
    match runtime
        .session_mut()
        .client_mut()
        .intake(WireMessage::DurableGap(gap))
    {
        Err(ClientError::ResyncRequired { .. }) => {}
        Err(error) => {
            return Err(WorkerFailure::Fatal(format!(
                "Could not request authoritative snapshot: {error}"
            )));
        }
        Ok(intake) => {
            return Err(WorkerFailure::Fatal(format!(
                "Snapshot request unexpectedly succeeded: {intake:?}"
            )));
        }
    }
    runtime.session_mut().disconnect().ok_or_else(|| {
        WorkerFailure::Fatal("Cannot reconnect for snapshot without a transport".to_owned())
    })?;
    ReconnectWait::from_runtime(runtime)
        .ok_or_else(|| WorkerFailure::Fatal("Snapshot reconnect did not enter backoff".to_owned()))
}

fn worker_error(context: &str, error: &crate::StudioError) -> WorkerFailure {
    let message = format!("{context}: {error}");
    if is_resync_failure(error) {
        WorkerFailure::Resync(message)
    } else if is_transport_failure(error) {
        WorkerFailure::Transport(message)
    } else {
        WorkerFailure::Fatal(message)
    }
}

const fn is_transport_failure(error: &crate::StudioError) -> bool {
    matches!(
        error,
        crate::StudioError::Session(
            TcpSessionError::Disconnected { .. } | TcpSessionError::Cancelled { .. }
        )
    )
}

const fn is_resync_failure(error: &crate::StudioError) -> bool {
    matches!(
        error,
        crate::StudioError::Client(ClientError::ResyncRequired { .. })
            | crate::StudioError::Session(
                TcpSessionError::ResyncRequired(_)
                    | TcpSessionError::Client(ClientError::ResyncRequired { .. })
            )
    )
}

const fn is_recoverable_failure(error: &crate::StudioError) -> bool {
    is_transport_failure(error) || is_resync_failure(error)
}

fn unexpected_failure(expected: &str, event: &SessionEvent) -> WorkerFailure {
    let message = match event {
        SessionEvent::ServerError(error) => format!(
            "Server error while awaiting {expected}: {}: {}",
            error.error.code, error.error.message
        ),
        SessionEvent::DurableGap { .. } => {
            return WorkerFailure::Resync(format!(
                "Durable event gap while awaiting {expected}; requesting snapshot"
            ));
        }
        SessionEvent::Disconnected { cause, .. } => {
            return WorkerFailure::Transport(format!(
                "Disconnected while awaiting {expected}: {cause:?}"
            ));
        }
        _ => format!("Unexpected response while awaiting {expected}: {event:?}"),
    };
    WorkerFailure::Fatal(message)
}

fn result_id(result: &CommandResult) -> &str {
    match result {
        CommandResult::Accepted { id, .. } | CommandResult::Rejected { id, .. } => id,
    }
}

const fn intent_payload(intent: StudioIntent) -> CommandPayload {
    match intent {
        StudioIntent::SelectPreview(input) => CommandPayload::SelectPreview {
            input: WireInputId::from_domain(input),
        },
        StudioIntent::Cut => CommandPayload::Cut,
        StudioIntent::Fade { duration_frames } => CommandPayload::Fade { duration_frames },
    }
}

const fn lifecycle_status(lifecycle: LifecycleState) -> StudioConnectionStatus {
    match lifecycle {
        LifecycleState::LaunchingDaemon => StudioConnectionStatus::Launching,
        LifecycleState::Disconnected => StudioConnectionStatus::Disconnected,
        LifecycleState::Connecting => StudioConnectionStatus::Connecting,
        LifecycleState::Synchronizing => StudioConnectionStatus::Synchronizing,
        LifecycleState::Ready => StudioConnectionStatus::Ready,
        LifecycleState::Backoff(_) => StudioConnectionStatus::Backoff,
        LifecycleState::Incompatible => StudioConnectionStatus::Incompatible,
        LifecycleState::DaemonExited { .. }
        | LifecycleState::DaemonFailed
        | LifecycleState::RestartLimitReached
        | LifecycleState::ResyncRequired => StudioConnectionStatus::Failed,
    }
}

fn runtime_state(runtime: &mut StudioRuntime, error: Option<String>) -> StudioUiState {
    let (connection_status, lifecycle_error) = match runtime.lifecycle() {
        Ok(lifecycle) => (lifecycle_status(lifecycle), None),
        Err(error) => (
            StudioConnectionStatus::Failed,
            Some(format!("Lifecycle check failed: {error}")),
        ),
    };
    let client = runtime.session().client();
    let permissions = client
        .session()
        .map(|session| session.permissions.as_slice());
    let (can_select_preview, can_transition) = switcher_permissions(permissions);
    let mut state = StudioUiState::new(connection_status)
        .with_switcher_permissions(can_select_preview, can_transition);
    if connection_status == StudioConnectionStatus::Ready {
        state.view = client.model().view();
    }
    state.pending_commands = client.model().pending_commands().len();
    state.error = error.or(lifecycle_error);
    state
}

fn switcher_permissions(permissions: Option<&[String]>) -> (bool, bool) {
    (
        permissions.is_some_and(|values| values.iter().any(|value| value == "select_preview")),
        permissions.is_some_and(|values| values.iter().any(|value| value == "transition")),
    )
}

fn publish_runtime(
    runtime: &mut StudioRuntime,
    publisher: &StatePublisher,
    error: Option<String>,
) -> bool {
    publisher.publish(runtime_state(runtime, error))
}

fn publish_deferred_runtime(
    runtime: &mut StudioRuntime,
    publisher: &StatePublisher,
    deferred: usize,
    error: Option<String>,
) -> bool {
    let mut state = runtime_state(runtime, error);
    state.notice = Some(format!(
        "Queued {deferred} command(s) until Studio reconnects"
    ));
    publisher.publish(state)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        num::NonZeroU128,
    };

    #[cfg(unix)]
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        process::{Command as ProcessCommand, Stdio},
    };

    use fm_client::ReconnectBackoff;
    use fm_protocol::{
        CapabilityReportSummary, EngineIdentity, EventCursor, EventMessage, EventPayload,
        HandshakeOutcome, HandshakeResponse, LineDecoder, ProtocolVersion, Role,
        RuntimeEventMessage, RuntimeLifecycleEvent, ServerIdentity, SnapshotMessage,
        SnapshotReason, WireMessage, encode_line,
    };
    use fm_types::{InputId, ProjectId};

    #[cfg(unix)]
    use crate::SupervisedConfig;
    use crate::{ConnectionConfig, ExistingConfig, RestartPolicy};

    use super::*;

    #[test]
    fn lifecycle_mapping_is_explicit() {
        assert_eq!(
            lifecycle_status(LifecycleState::LaunchingDaemon),
            StudioConnectionStatus::Launching
        );
        assert_eq!(
            lifecycle_status(LifecycleState::Connecting),
            StudioConnectionStatus::Connecting
        );
        assert_eq!(
            lifecycle_status(LifecycleState::Synchronizing),
            StudioConnectionStatus::Synchronizing
        );
        assert_eq!(
            lifecycle_status(LifecycleState::Ready),
            StudioConnectionStatus::Ready
        );
        assert_eq!(
            lifecycle_status(LifecycleState::Backoff(ReconnectBackoff {
                attempt: 1,
                delay_ms: 250,
            })),
            StudioConnectionStatus::Backoff
        );
        assert_eq!(
            lifecycle_status(LifecycleState::Disconnected),
            StudioConnectionStatus::Disconnected
        );
        assert_eq!(
            lifecycle_status(LifecycleState::Incompatible),
            StudioConnectionStatus::Incompatible
        );
        for lifecycle in [
            LifecycleState::DaemonExited { code: Some(1) },
            LifecycleState::DaemonFailed,
            LifecycleState::RestartLimitReached,
            LifecycleState::ResyncRequired,
        ] {
            assert_eq!(lifecycle_status(lifecycle), StudioConnectionStatus::Failed);
        }
    }

    #[test]
    fn intents_preserve_full_width_ids_and_fade_frames() {
        let input = InputId::new(NonZeroU128::new(u128::MAX).unwrap());
        assert_eq!(
            intent_payload(StudioIntent::SelectPreview(input)),
            CommandPayload::SelectPreview {
                input: WireInputId::new(NonZeroU128::new(u128::MAX).unwrap())
            }
        );
        assert_eq!(intent_payload(StudioIntent::Cut), CommandPayload::Cut);
        assert_eq!(
            intent_payload(StudioIntent::Fade {
                duration_frames: u32::MAX,
            }),
            CommandPayload::Fade {
                duration_frames: u32::MAX,
            }
        );
    }

    #[test]
    fn idempotency_keys_are_nonempty_unique_and_monotonic() {
        let mut keys = IdempotencyKeys::new("freemix-studio-test-session");
        let first = keys.next().unwrap();
        let second = keys.next().unwrap();
        assert_eq!(first, "freemix-studio-test-session:1");
        assert_eq!(second, "freemix-studio-test-session:2");
        assert!(!first.is_empty());
        assert_ne!(first, second);
    }

    #[test]
    fn worker_nonces_are_distinct_within_one_process() {
        assert_ne!(worker_nonce(), worker_nonce());
    }

    #[test]
    fn state_permissions_map_independently() {
        let permissions = vec!["select_preview".to_owned(), "view_status".to_owned()];
        assert_eq!(switcher_permissions(Some(&permissions)), (true, false));
        let permissions = vec!["transition".to_owned()];
        assert_eq!(switcher_permissions(Some(&permissions)), (false, true));
        assert_eq!(switcher_permissions(None), (false, false));
    }

    #[test]
    fn request_send_is_bounded_and_nonblocking() {
        let (sender, receiver) = sync_channel(1);
        assert_eq!(try_enqueue(&sender, WorkerRequest::Shutdown), Ok(()));
        assert_eq!(
            try_enqueue(&sender, WorkerRequest::Shutdown),
            Err(EnqueueError::Full)
        );
        drop(receiver);
        assert_eq!(
            try_enqueue(&sender, WorkerRequest::Shutdown),
            Err(EnqueueError::Disconnected)
        );
    }

    #[test]
    fn active_wait_deferred_overflow_remains_observable() {
        let (sender, receiver) = sync_channel(1);
        let mut deferred = VecDeque::from(vec![StudioIntent::Cut; DEFERRED_INTENT_CAPACITY]);
        let mut rejected = 0;
        try_enqueue(&sender, WorkerRequest::Intent(StudioIntent::Cut)).unwrap();

        assert!(!cancellation_requested(
            &receiver,
            &mut deferred,
            &mut rejected,
        ));
        assert_eq!(deferred.len(), DEFERRED_INTENT_CAPACITY);
        assert_eq!(rejected, 1);

        let mut recovery = WorkerRecovery::new(None, None);
        recovery.deferred_rejections = rejected;
        assert_eq!(
            recovery.error().as_deref(),
            Some("Rejected 1 command(s): Studio deferred command queue reached capacity 16")
        );
        recovery.visible_error = Some("temporary reconnect failure".to_owned());
        recovery.visible_error = None;
        assert!(
            recovery.error().is_some(),
            "success erased overflow rejection"
        );
    }

    struct FakePeer {
        stream: TcpStream,
        decoder: LineDecoder,
        pending: VecDeque<WireMessage>,
    }

    impl FakePeer {
        fn accept(listener: &TcpListener) -> Self {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            Self {
                stream,
                decoder: LineDecoder::new(),
                pending: VecDeque::new(),
            }
        }

        fn receive(&mut self) -> WireMessage {
            loop {
                if let Some(message) = self.pending.pop_front() {
                    return message;
                }
                let mut buffer = [0_u8; 4096];
                let read = self.stream.read(&mut buffer).unwrap();
                assert_ne!(read, 0, "unexpected worker EOF");
                self.pending
                    .extend(self.decoder.push(&buffer[..read]).unwrap());
            }
        }

        fn send(&mut self, message: &WireMessage) {
            self.stream
                .write_all(encode_line(message).unwrap().as_bytes())
                .unwrap();
            self.stream.flush().unwrap();
        }
    }

    fn wire_input(value: u128) -> WireInputId {
        WireInputId::new(NonZeroU128::new(value).unwrap())
    }

    fn test_project_id() -> ProjectId {
        ProjectId::new(NonZeroU128::new(91).unwrap())
    }

    fn test_server() -> ServerIdentity {
        ServerIdentity {
            engine_id: "worker-engine".to_owned(),
            project_id: test_project_id().to_string(),
            state_epoch: 1,
            log_id: "worker-log".to_owned(),
        }
    }

    fn test_engine() -> EngineIdentity {
        EngineIdentity {
            engine_id: "worker-engine".to_owned(),
            state_epoch: 1,
            log_id: "worker-log".to_owned(),
        }
    }

    fn accept_worker_snapshot(listener: &TcpListener) -> FakePeer {
        accept_worker_snapshot_at(listener, 4, 2, 2)
    }

    fn accept_worker_snapshot_at(
        listener: &TcpListener,
        revision: u64,
        desired_preview: u128,
        realized_preview: u128,
    ) -> FakePeer {
        let mut peer = FakePeer::accept(listener);
        let WireMessage::HandshakeRequest(request) = peer.receive() else {
            panic!("expected snapshot handshake request");
        };
        assert_eq!(request.resume_cursor, None);
        peer.send(&WireMessage::HandshakeResponse(HandshakeResponse {
            negotiated: ProtocolVersion::new(1, 2),
            granted_role: Role::Operator,
            permissions: vec!["select_preview".to_owned(), "transition".to_owned()],
            capabilities: CapabilityReportSummary {
                digest: "sha256:worker-test".to_owned(),
                total: 1,
                available: 1,
                degraded: 0,
                unavailable: 0,
            },
            server: test_server(),
            current_revision: revision,
            outcome: HandshakeOutcome::Snapshot {
                reason: SnapshotReason::NoCursor,
            },
        }));
        peer.send(&WireMessage::Snapshot(SnapshotMessage {
            engine: test_engine(),
            revision,
            show_name: "Worker test".to_owned(),
            inputs: vec![wire_input(1), wire_input(2), wire_input(3)],
            desired_program: wire_input(1),
            desired_preview: wire_input(desired_preview),
            realized_program: wire_input(1),
            realized_preview: wire_input(realized_preview),
        }));
        peer
    }

    fn accept_worker_resume(listener: &TcpListener, current_revision: u64) -> (FakePeer, u64) {
        let mut peer = FakePeer::accept(listener);
        let WireMessage::HandshakeRequest(request) = peer.receive() else {
            panic!("expected reconnect handshake request");
        };
        let cursor = request.resume_cursor.expect("resume cursor");
        peer.send(&WireMessage::HandshakeResponse(HandshakeResponse {
            negotiated: ProtocolVersion::new(1, 2),
            granted_role: Role::Operator,
            permissions: vec!["select_preview".to_owned(), "transition".to_owned()],
            capabilities: CapabilityReportSummary {
                digest: "sha256:worker-test".to_owned(),
                total: 1,
                available: 1,
                degraded: 0,
                unavailable: 0,
            },
            server: test_server(),
            current_revision,
            outcome: HandshakeOutcome::Resume {
                cursor: cursor.clone(),
            },
        }));
        (peer, cursor.revision)
    }

    fn spawn_test_worker(
        address: std::net::SocketAddr,
    ) -> (
        SyncSender<WorkerRequest>,
        Receiver<StudioUiState>,
        JoinHandle<()>,
    ) {
        spawn_test_worker_config(StudioConfig {
            connection: ConnectionConfig::Existing(ExistingConfig {
                address,
                expected_project_id: test_project_id(),
            }),
            client_id: "worker-test".to_owned(),
            desired_role: Role::Operator,
            restart_policy: RestartPolicy::default(),
        })
    }

    fn spawn_test_worker_config(
        config: StudioConfig,
    ) -> (
        SyncSender<WorkerRequest>,
        Receiver<StudioUiState>,
        JoinHandle<()>,
    ) {
        let (request_sender, request_receiver) = sync_channel(REQUEST_CAPACITY);
        let (state_sender, state_receiver) = sync_channel(STATE_CAPACITY);
        let worker = thread::spawn(move || {
            let publisher = StatePublisher {
                sender: state_sender,
                repaint_context: egui::Context::default(),
            };
            run_worker(config, &request_receiver, &publisher);
        });
        (request_sender, state_receiver, worker)
    }

    fn wait_until_ready(states: &Receiver<StudioUiState>) {
        loop {
            let state = states.recv_timeout(Duration::from_secs(3)).unwrap();
            if state.connection_status == StudioConnectionStatus::Ready {
                return;
            }
        }
    }

    fn serve_worker_select_preview(listener: &TcpListener) {
        let mut peer = accept_worker_snapshot(listener);

        let WireMessage::Command(command) = peer.receive() else {
            panic!("expected Select Preview command");
        };
        assert_eq!(
            command.payload,
            CommandPayload::SelectPreview {
                input: wire_input(3)
            }
        );
        assert_eq!(command.expected_revision, Some(4));
        assert_eq!(command.deadline_ms, None);
        assert!(!command.idempotency_key.is_empty());
        peer.send(&WireMessage::CommandResult(CommandResult::Accepted {
            id: command.id,
            revision: 5,
            scheduled_frame: None,
        }));
        peer.send(&WireMessage::Event(EventMessage {
            cursor: EventCursor {
                engine: test_engine(),
                revision: 5,
            },
            payload: EventPayload::DesiredSwitcher {
                program: wire_input(1),
                preview: wire_input(3),
            },
        }));
        peer.send(&WireMessage::RuntimeEvent(RuntimeEventMessage {
            server: test_server(),
            revision: 5,
            generation: 1,
            sequence: 1,
            event: RuntimeLifecycleEvent::Realized {
                domain: "switcher".to_owned(),
            },
        }));
    }

    fn serve_worker_reconnect_snapshot_and_deferred(listener: &TcpListener) {
        let mut first = accept_worker_snapshot(listener);
        let WireMessage::Command(original) = first.receive() else {
            panic!("expected original command");
        };
        drop(first);

        let (mut second, resumed_revision) = accept_worker_resume(listener, 5);
        assert_eq!(resumed_revision, 4);
        second.send(&WireMessage::Event(EventMessage {
            cursor: EventCursor {
                engine: test_engine(),
                revision: 5,
            },
            payload: EventPayload::DesiredSwitcher {
                program: wire_input(1),
                preview: wire_input(3),
            },
        }));
        let WireMessage::Command(retried) = second.receive() else {
            panic!("expected retried command");
        };
        assert_eq!(retried, original, "reconnect changed the command envelope");
        second.send(&WireMessage::CommandResult(CommandResult::Accepted {
            id: retried.id,
            revision: 5,
            scheduled_frame: None,
        }));

        drop(second);
        let mut third = accept_worker_snapshot_at(listener, 5, 3, 3);
        let WireMessage::Command(deferred) = third.receive() else {
            panic!("expected deferred command");
        };
        assert_eq!(deferred.payload, CommandPayload::Cut);
        assert_eq!(deferred.expected_revision, Some(5));
        assert_ne!(deferred.idempotency_key, original.idempotency_key);
        third.send(&WireMessage::CommandResult(CommandResult::Accepted {
            id: deferred.id,
            revision: 6,
            scheduled_frame: None,
        }));
        third.send(&WireMessage::Event(EventMessage {
            cursor: EventCursor {
                engine: test_engine(),
                revision: 6,
            },
            payload: EventPayload::DesiredSwitcher {
                program: wire_input(3),
                preview: wire_input(1),
            },
        }));
        third.send(&WireMessage::RuntimeEvent(RuntimeEventMessage {
            server: test_server(),
            revision: 6,
            generation: 1,
            sequence: 1,
            event: RuntimeLifecycleEvent::Realized {
                domain: "switcher".to_owned(),
            },
        }));
    }

    #[test]
    fn worker_select_preview_flow_is_exact_and_shutdown_is_idle() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || serve_worker_select_preview(&listener));

        let (request_sender, state_receiver, worker) = spawn_test_worker(address);

        wait_until_ready(&state_receiver);
        let target = InputId::new(NonZeroU128::new(3).unwrap());
        try_enqueue(
            &request_sender,
            WorkerRequest::Intent(StudioIntent::SelectPreview(target)),
        )
        .unwrap();

        let mut optimistic_seen = false;
        let final_state = loop {
            let state = state_receiver.recv_timeout(Duration::from_secs(3)).unwrap();
            optimistic_seen |= state.pending_commands == 1
                && state.view.as_ref().is_some_and(|view| {
                    view.cursor.revision.get() == 4 && view.switcher.desired.preview == target
                });
            if state.pending_commands == 0
                && state.view.as_ref().is_some_and(|view| {
                    view.cursor.revision.get() == 5
                        && view.switcher.desired.preview == target
                        && view.switcher.realized.preview == target
                })
            {
                break state;
            }
        };
        assert!(optimistic_seen, "optimistic state was not published first");
        assert_eq!(final_state.error, None);
        try_enqueue(&request_sender, WorkerRequest::Shutdown).unwrap();
        for _ in 0..100 {
            if worker.is_finished() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            worker.is_finished(),
            "idle worker did not shut down promptly"
        );
        worker.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn worker_reconnects_then_snapshots_realization_and_runs_deferred_command() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || serve_worker_reconnect_snapshot_and_deferred(&listener));

        let (requests, states, worker) = spawn_test_worker(address);
        wait_until_ready(&states);
        let target = InputId::new(NonZeroU128::new(3).unwrap());
        try_enqueue(
            &requests,
            WorkerRequest::Intent(StudioIntent::SelectPreview(target)),
        )
        .unwrap();

        let mut saw_backoff = false;
        let mut queued_during_backoff = false;
        let mut saw_deferred_notice = false;
        loop {
            let state = states.recv_timeout(Duration::from_secs(3)).unwrap();
            saw_backoff |= state.connection_status == StudioConnectionStatus::Backoff;
            if saw_backoff && !queued_during_backoff {
                assert_eq!(state.view, None, "stale realization remained visible");
                try_enqueue(&requests, WorkerRequest::Intent(StudioIntent::Cut)).unwrap();
                queued_during_backoff = true;
            }
            saw_deferred_notice |= state
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("Queued 1 command"));
            if saw_backoff
                && state.connection_status == StudioConnectionStatus::Ready
                && state.pending_commands == 0
                && state.view.as_ref().is_some_and(|view| {
                    view.cursor.revision.get() == 6
                        && view.switcher.desired.program == target
                        && view.switcher.realized.program == target
                })
            {
                assert_eq!(state.error, None);
                assert!(saw_deferred_notice);
                break;
            }
        }

        try_enqueue(&requests, WorkerRequest::Shutdown).unwrap();
        worker.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn worker_recovers_durable_gap_with_snapshot() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut first = accept_worker_snapshot(&listener);
            let WireMessage::Command(command) = first.receive() else {
                panic!("expected command before durable gap");
            };
            first.send(&WireMessage::CommandResult(CommandResult::Accepted {
                id: command.id,
                revision: 5,
                scheduled_frame: None,
            }));
            first.send(&WireMessage::DurableGap(DurableGap {
                server: test_server(),
                requested_after_revision: 4,
                available_from_revision: 6,
                current_revision: 6,
            }));
            drop(first);

            accept_worker_snapshot_at(&listener, 6, 3, 3);
        });

        let (requests, states, worker) = spawn_test_worker(address);
        wait_until_ready(&states);
        let target = InputId::new(NonZeroU128::new(3).unwrap());
        try_enqueue(
            &requests,
            WorkerRequest::Intent(StudioIntent::SelectPreview(target)),
        )
        .unwrap();

        let mut saw_backoff = false;
        loop {
            let state = states.recv_timeout(Duration::from_secs(3)).unwrap();
            saw_backoff |= state.connection_status == StudioConnectionStatus::Backoff;
            if saw_backoff
                && state.connection_status == StudioConnectionStatus::Ready
                && state.pending_commands == 0
                && state.view.as_ref().is_some_and(|view| {
                    view.cursor.revision.get() == 6
                        && view.switcher.desired.preview == target
                        && view.switcher.realized.preview == target
                })
            {
                assert_eq!(state.error, None);
                break;
            }
        }

        try_enqueue(&requests, WorkerRequest::Shutdown).unwrap();
        worker.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn worker_recovers_event_gap_with_forced_snapshot() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut first = accept_worker_snapshot(&listener);
            let WireMessage::Command(command) = first.receive() else {
                panic!("expected command before event gap")
            };
            first.send(&WireMessage::CommandResult(CommandResult::Accepted {
                id: command.id,
                revision: 5,
                scheduled_frame: None,
            }));
            first.send(&WireMessage::Event(EventMessage {
                cursor: EventCursor {
                    engine: test_engine(),
                    revision: 6,
                },
                payload: EventPayload::DesiredSwitcher {
                    program: wire_input(1),
                    preview: wire_input(3),
                },
            }));
            assert_eq!(first.stream.read(&mut [0_u8; 1]).unwrap(), 0);
            accept_worker_snapshot_at(&listener, 6, 3, 3);
        });

        let (requests, states, worker) = spawn_test_worker(address);
        wait_until_ready(&states);
        let target = InputId::new(NonZeroU128::new(3).unwrap());
        try_enqueue(
            &requests,
            WorkerRequest::Intent(StudioIntent::SelectPreview(target)),
        )
        .unwrap();

        let mut saw_backoff = false;
        loop {
            let state = states.recv_timeout(Duration::from_secs(3)).unwrap();
            saw_backoff |= state.connection_status == StudioConnectionStatus::Backoff;
            if saw_backoff
                && state.connection_status == StudioConnectionStatus::Ready
                && state.pending_commands == 0
                && state.view.as_ref().is_some_and(|view| {
                    view.cursor.revision.get() == 6
                        && view.switcher.desired.preview == target
                        && view.switcher.realized.preview == target
                })
            {
                assert_eq!(state.error, None);
                break;
            }
        }
        try_enqueue(&requests, WorkerRequest::Shutdown).unwrap();
        worker.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn worker_recovers_model_error_with_forced_snapshot() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut first = accept_worker_snapshot(&listener);
            let WireMessage::Command(original) = first.receive() else {
                panic!("expected command before model error")
            };
            first.send(&WireMessage::Event(EventMessage {
                cursor: EventCursor {
                    engine: test_engine(),
                    revision: 5,
                },
                payload: EventPayload::DesiredSwitcher {
                    program: wire_input(1),
                    preview: wire_input(99),
                },
            }));
            assert_eq!(first.stream.read(&mut [0_u8; 1]).unwrap(), 0);

            let mut second = accept_worker_snapshot_at(&listener, 5, 3, 3);
            let WireMessage::Command(retried) = second.receive() else {
                panic!("expected unresolved retry after model error")
            };
            assert_eq!(retried, original);
            second.send(&WireMessage::CommandResult(CommandResult::Rejected {
                id: retried.id,
                code: "already_applied".to_owned(),
                message: "snapshot is authoritative".to_owned(),
                fields: Vec::new(),
                current_revision: 5,
                retryable: false,
            }));
        });

        let (requests, states, worker) = spawn_test_worker(address);
        wait_until_ready(&states);
        let target = InputId::new(NonZeroU128::new(3).unwrap());
        try_enqueue(
            &requests,
            WorkerRequest::Intent(StudioIntent::SelectPreview(target)),
        )
        .unwrap();

        let mut saw_backoff = false;
        loop {
            let state = states.recv_timeout(Duration::from_secs(3)).unwrap();
            saw_backoff |= state.connection_status == StudioConnectionStatus::Backoff;
            if saw_backoff
                && state.connection_status == StudioConnectionStatus::Ready
                && state.pending_commands == 0
                && state.view.as_ref().is_some_and(|view| {
                    view.cursor.revision.get() == 5
                        && view.switcher.desired.preview == target
                        && view.switcher.realized.preview == target
                })
            {
                break;
            }
        }
        try_enqueue(&requests, WorkerRequest::Shutdown).unwrap();
        worker.join().unwrap();
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn native_app_drop_joins_worker_and_reaps_daemon_silent_during_readiness() {
        let base = std::env::temp_dir().join(format!("{}-silent-readiness", worker_nonce()));
        let executable = base.with_extension("sh");
        let project = base.with_extension("freemix");
        let pid_path = PathBuf::from(format!("{}.pid", project.display()));
        let descendant_pid_path = PathBuf::from(format!("{}.descendant.pid", project.display()));
        fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$2.pid\"\nsleep 30 &\nprintf '%s\\n' \"$!\" > \"$2.descendant.pid\"\nIFS= read -r hold\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();

        let config = StudioConfig {
            connection: ConnectionConfig::Supervised(SupervisedConfig {
                project_bundle: project,
                daemon_executable: executable.clone(),
                listen: "127.0.0.1:0".parse().unwrap(),
            }),
            client_id: "silent-readiness".to_owned(),
            desired_role: Role::Operator,
            restart_policy: RestartPolicy::default(),
        };
        let (requests, states, worker) = spawn_test_worker_config(config);
        let app = StudioApp {
            shell: StudioShell::default(),
            state: StudioUiState::new(StudioConnectionStatus::Launching),
            requests: Some(requests),
            updates: Some(states),
            worker: Some(worker),
            shutdown_sent: false,
        };
        let wait_started = Instant::now();
        while !pid_path.exists() || !descendant_pid_path.exists() {
            assert!(wait_started.elapsed() < Duration::from_secs(3));
            thread::sleep(Duration::from_millis(10));
        }
        let pid = fs::read_to_string(&pid_path).unwrap();
        let descendant_pid = fs::read_to_string(&descendant_pid_path).unwrap();

        let shutdown_started = Instant::now();
        drop(app);
        assert!(
            shutdown_started.elapsed() < Duration::from_millis(250),
            "worker did not join a cancelled readiness wait"
        );
        assert!(
            !ProcessCommand::new("/bin/kill")
                .args(["-0", pid.trim()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success(),
            "supervised daemon was not reaped before worker join"
        );
        assert!(
            !ProcessCommand::new("/bin/kill")
                .args(["-0", descendant_pid.trim()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success(),
            "readiness stdout descendant survived cancellation"
        );
        let _ = fs::remove_file(executable);
        let _ = fs::remove_file(pid_path);
        let _ = fs::remove_file(descendant_pid_path);
    }

    #[test]
    fn worker_shutdown_cancels_a_silent_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (handshake_tx, handshake_rx) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let mut peer = FakePeer::accept(&listener);
            assert!(matches!(peer.receive(), WireMessage::HandshakeRequest(_)));
            handshake_tx.send(()).unwrap();
            assert_eq!(peer.stream.read(&mut [0_u8; 1]).unwrap(), 0);
        });

        let (requests, _states, worker) = spawn_test_worker(address);
        handshake_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let shutdown_started = Instant::now();
        try_enqueue(&requests, WorkerRequest::Shutdown).unwrap();
        worker.join().unwrap();
        assert!(
            shutdown_started.elapsed() < Duration::from_millis(250),
            "worker remained blocked on a silent handshake"
        );
        server.join().unwrap();
    }

    #[test]
    fn worker_reconnects_after_a_silent_handshake_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut silent = FakePeer::accept(&listener);
            assert!(matches!(silent.receive(), WireMessage::HandshakeRequest(_)));
            assert_eq!(silent.stream.read(&mut [0_u8; 1]).unwrap(), 0);
            accept_worker_snapshot(&listener);
        });

        let (requests, states, worker) = spawn_test_worker(address);
        let mut saw_backoff = false;
        loop {
            let state = states.recv_timeout(Duration::from_secs(3)).unwrap();
            saw_backoff |= state.connection_status == StudioConnectionStatus::Backoff;
            if saw_backoff && state.connection_status == StudioConnectionStatus::Ready {
                break;
            }
        }
        try_enqueue(&requests, WorkerRequest::Shutdown).unwrap();
        worker.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn worker_reconnects_after_a_silent_command_response_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut first = accept_worker_snapshot(&listener);
            let WireMessage::Command(original) = first.receive() else {
                panic!("expected original command")
            };
            assert_eq!(first.stream.read(&mut [0_u8; 1]).unwrap(), 0);

            let (mut second, resumed_revision) = accept_worker_resume(&listener, 4);
            assert_eq!(resumed_revision, 4);
            let WireMessage::Command(retried) = second.receive() else {
                panic!("expected retried command")
            };
            assert_eq!(retried, original);
            second.send(&WireMessage::CommandResult(CommandResult::Accepted {
                id: retried.id,
                revision: 5,
                scheduled_frame: None,
            }));
            second.send(&WireMessage::Event(EventMessage {
                cursor: EventCursor {
                    engine: test_engine(),
                    revision: 5,
                },
                payload: EventPayload::DesiredSwitcher {
                    program: wire_input(1),
                    preview: wire_input(3),
                },
            }));
            second.send(&WireMessage::RuntimeEvent(RuntimeEventMessage {
                server: test_server(),
                revision: 5,
                generation: 1,
                sequence: 1,
                event: RuntimeLifecycleEvent::Realized {
                    domain: "switcher".to_owned(),
                },
            }));
            assert_eq!(second.stream.read(&mut [0_u8; 1]).unwrap(), 0);
            accept_worker_snapshot_at(&listener, 5, 3, 3);
        });

        let (requests, states, worker) = spawn_test_worker(address);
        wait_until_ready(&states);
        let target = InputId::new(NonZeroU128::new(3).unwrap());
        try_enqueue(
            &requests,
            WorkerRequest::Intent(StudioIntent::SelectPreview(target)),
        )
        .unwrap();

        let mut saw_backoff = false;
        loop {
            let state = states.recv_timeout(Duration::from_secs(3)).unwrap();
            saw_backoff |= state.connection_status == StudioConnectionStatus::Backoff;
            if saw_backoff
                && state.connection_status == StudioConnectionStatus::Ready
                && state.pending_commands == 0
                && state.view.as_ref().is_some_and(|view| {
                    view.cursor.revision.get() == 5
                        && view.switcher.desired.preview == target
                        && view.switcher.realized.preview == target
                })
            {
                break;
            }
        }
        try_enqueue(&requests, WorkerRequest::Shutdown).unwrap();
        worker.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn command_response_uses_one_aggregate_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (command_tx, command_rx) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let mut peer = accept_worker_snapshot(&listener);
            let WireMessage::Command(command) = peer.receive() else {
                panic!("expected command")
            };
            command_tx.send(()).unwrap();
            thread::sleep(Duration::from_millis(150));
            peer.send(&WireMessage::CommandResult(CommandResult::Accepted {
                id: command.id,
                revision: 5,
                scheduled_frame: None,
            }));
            thread::sleep(Duration::from_millis(150));
            let event = encode_line(&WireMessage::Event(EventMessage {
                cursor: EventCursor {
                    engine: test_engine(),
                    revision: 5,
                },
                payload: EventPayload::DesiredSwitcher {
                    program: wire_input(1),
                    preview: wire_input(3),
                },
            }))
            .unwrap();
            let _ = peer.stream.write_all(event.as_bytes());
            let _ = peer.stream.flush();
            let _ = peer.stream.read(&mut [0_u8; 1]);
        });

        let (requests, states, worker) = spawn_test_worker(address);
        wait_until_ready(&states);
        try_enqueue(
            &requests,
            WorkerRequest::Intent(StudioIntent::SelectPreview(InputId::new(
                NonZeroU128::new(3).unwrap(),
            ))),
        )
        .unwrap();
        command_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let response_started = Instant::now();
        loop {
            let state = states.recv_timeout(Duration::from_secs(3)).unwrap();
            if state.connection_status == StudioConnectionStatus::Backoff {
                break;
            }
        }
        assert!(
            response_started.elapsed() < Duration::from_millis(400),
            "each command response record received a fresh timeout"
        );
        try_enqueue(&requests, WorkerRequest::Shutdown).unwrap();
        worker.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn worker_shutdown_interrupts_reconnect_backoff() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut first = accept_worker_snapshot(&listener);
            let WireMessage::Command(original) = first.receive() else {
                panic!("expected original command");
            };
            drop(first);

            let (mut second, resumed_revision) = accept_worker_resume(&listener, 4);
            assert_eq!(resumed_revision, 4);
            let WireMessage::Command(retried) = second.receive() else {
                panic!("expected retried command");
            };
            assert_eq!(retried, original);
        });

        let (requests, states, worker) = spawn_test_worker(address);
        wait_until_ready(&states);
        try_enqueue(
            &requests,
            WorkerRequest::Intent(StudioIntent::SelectPreview(InputId::new(
                NonZeroU128::new(3).unwrap(),
            ))),
        )
        .unwrap();

        let mut backoffs = 0;
        while backoffs < 2 {
            let state = states.recv_timeout(Duration::from_secs(3)).unwrap();
            if state.connection_status == StudioConnectionStatus::Backoff {
                backoffs += 1;
            }
        }
        let shutdown_started = Instant::now();
        try_enqueue(&requests, WorkerRequest::Shutdown).unwrap();
        worker.join().unwrap();
        assert!(
            shutdown_started.elapsed() < Duration::from_millis(250),
            "worker waited for the 500 ms reconnect delay during shutdown"
        );
        server.join().unwrap();
    }
}
