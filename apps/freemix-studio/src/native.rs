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
use fm_client::{
    ClientError, CommandStatus, CommandUncertainty, SessionEvent, SyncMode, TcpSessionError,
};
use fm_protocol::{
    ALPHA_FADE_PROTOCOL_VERSION, CommandPayload, CommandResult, DurableGap,
    FADE_TO_BLACK_PROTOCOL_VERSION, MANUAL_TRANSITION_PROTOCOL_VERSION, ProtocolVersion,
    WIPE_PROTOCOL_VERSION, WireInputId, WireMessage,
};
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
const TERMINAL_UNCERTAINTY_CAPACITY: usize = 8;
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
    // Never evicted during a worker lifetime; restart is the explicit clear operation.
    terminal_uncertainties: Vec<TerminalUncertaintyNotice>,
}

#[derive(Debug, Eq, PartialEq)]
struct TerminalUncertaintyNotice {
    command_id: String,
    received_command_id: String,
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
            terminal_uncertainties: Vec::new(),
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
        publish_deferred_runtime(runtime, publisher, &self.deferred_intents, self.error())
    }

    fn error(&self) -> Option<String> {
        combined_error(
            join_errors(self.visible_error.clone(), self.terminal_error()),
            self.deferred_rejections,
        )
    }

    fn terminal_error(&self) -> Option<String> {
        (!self.terminal_uncertainties.is_empty()).then(|| {
            let commands = self
                .terminal_uncertainties
                .iter()
                .map(|notice| {
                    format!(
                        "{:?} (server receipt {:?})",
                        notice.command_id, notice.received_command_id
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("Terminal command uncertainty remains after authoritative resync: {commands}")
        })
    }

    fn capture_pending_terminal_uncertainty(&mut self, runtime: &StudioRuntime) -> bool {
        let Some(command_id) = self.pending_command.as_ref() else {
            return false;
        };
        let Some(CommandStatus::TerminalUncertain(
            CommandUncertainty::IdempotencyReplayCollision {
                received_command_id,
            },
        )) = runtime
            .session()
            .client()
            .command(command_id)
            .map(|record| &record.status)
        else {
            return false;
        };
        if self
            .terminal_uncertainties
            .iter()
            .any(|notice| notice.command_id == *command_id)
        {
            return true;
        }
        if self.terminal_uncertainties.len() == TERMINAL_UNCERTAINTY_CAPACITY {
            self.visible_error = Some(format!(
                "Terminal uncertainty ledger reached capacity {TERMINAL_UNCERTAINTY_CAPACITY}; restart Studio to explicitly clear operator history"
            ));
            return false;
        }
        self.terminal_uncertainties.push(TerminalUncertaintyNotice {
            command_id: command_id.clone(),
            received_command_id: received_command_id.clone(),
        });
        true
    }
}

fn join_errors(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (Some(first), Some(second)) => Some(format!("{first}; {second}")),
        (Some(first), None) => Some(first),
        (None, second) => second,
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
    if !publish_recovery_runtime(&mut runtime, publisher, &recovery) {
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
            && let Some(intent) = pop_supported_deferred_intent(
                &mut recovery.deferred_intents,
                session_protocol(runtime.session()),
            )
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
                if recovery.active()
                    || !recovery.deferred_intents.is_empty()
                    || !intent_supported_by_session(runtime.session(), intent)
                {
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
    if recovery.terminal_uncertainties.len() == TERMINAL_UNCERTAINTY_CAPACITY {
        recovery.visible_error = Some(format!(
            "Command not sent: terminal uncertainty ledger reached capacity {TERMINAL_UNCERTAINTY_CAPACITY}; restart Studio to explicitly clear operator history"
        ));
        return publish_recovery_runtime(runtime, publisher, recovery);
    }
    let command_id = match begin_intent(runtime, intent, keys, publisher, recovery.error()) {
        Ok(command_id) => command_id,
        Err(error) => {
            recovery.visible_error = Some(error);
            return publish_runtime(runtime, publisher, recovery.error());
        }
    };
    recovery.pending_command = Some(command_id.clone());
    let persistent_error = recovery.terminal_error();
    let publication = CommandPublication {
        publisher,
        publish_updates: true,
        persistent_error: persistent_error.as_deref(),
    };
    let result = flush_worker(runtime, requests, recovery).and_then(|()| {
        consume_command_sequence(
            runtime,
            &command_id,
            publication,
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
            if recovery.capture_pending_terminal_uncertainty(runtime) {
                recovery.visible_error = None;
            }
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
    (recovery.error().is_none() && recovery.deferred_intents.is_empty())
        || publish_recovery_runtime(runtime, publisher, recovery)
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
                    && !publish_recovery_runtime(runtime, publisher, recovery)
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
        return publish_recovery_runtime(runtime, publisher, recovery);
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
        let terminal = runtime
            .session()
            .client()
            .command(&command_id)
            .is_some_and(|record| record.status.is_terminal());
        if terminal {
            recovery.pending_command = None;
        } else {
            let persistent_error = recovery.terminal_error();
            let publication = CommandPublication {
                publisher,
                publish_updates: !recovery.realization_uncertain,
                persistent_error: persistent_error.as_deref(),
            };
            let result = consume_command_sequence(
                runtime,
                &command_id,
                publication,
                requests,
                &mut recovery.deferred_intents,
                &mut recovery.deferred_rejections,
            );
            match result {
                Ok(()) => recovery.pending_command = None,
                Err(WorkerFailure::Shutdown) => return false,
                Err(error) => {
                    recovery.visible_error = Some(error.message().to_owned());
                    if recovery.capture_pending_terminal_uncertainty(runtime) {
                        recovery.visible_error = None;
                    }
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
    publication: CommandPublication<'_>,
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
            publication.rejection(
                runtime,
                format!("Command rejected ({code}): {message}"),
                *deferred_rejections,
            );
            return Ok(());
        }
    };
    publication.update(runtime, *deferred_rejections)?;
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
    publication.update(runtime, *deferred_rejections)?;

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
    publication.update(runtime, *deferred_rejections)?;
    Ok(())
}

#[derive(Clone, Copy)]
struct CommandPublication<'a> {
    publisher: &'a StatePublisher,
    publish_updates: bool,
    persistent_error: Option<&'a str>,
}

impl CommandPublication<'_> {
    fn update(
        self,
        runtime: &mut StudioRuntime,
        deferred_rejections: usize,
    ) -> Result<(), WorkerFailure> {
        if self.publish_updates
            && !publish_runtime(
                runtime,
                self.publisher,
                combined_error(
                    self.persistent_error.map(str::to_owned),
                    deferred_rejections,
                ),
            )
        {
            Err(WorkerFailure::Fatal("Studio UI disconnected".to_owned()))
        } else {
            Ok(())
        }
    }

    fn rejection(self, runtime: &mut StudioRuntime, error: String, deferred_rejections: usize) {
        if self.publish_updates {
            publish_runtime(
                runtime,
                self.publisher,
                combined_error(
                    join_errors(Some(error), self.persistent_error.map(str::to_owned)),
                    deferred_rejections,
                ),
            );
        }
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
            TcpSessionError::Disconnected { .. }
                | TcpSessionError::Cancelled { .. }
                | TcpSessionError::PendingCommandIncompatible { .. }
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
        StudioIntent::AlphaFade { duration_frames } => {
            CommandPayload::AlphaFade { duration_frames }
        }
        StudioIntent::Wipe { duration_frames } => CommandPayload::Wipe { duration_frames },
        StudioIntent::FadeToBlack {
            active,
            duration_frames,
        } => CommandPayload::FadeToBlack {
            active,
            duration_frames,
        },
        StudioIntent::StartManualTransition { kind } => {
            CommandPayload::StartManualTransition { kind }
        }
        StudioIntent::SetManualTransitionPosition { position } => {
            CommandPayload::SetManualTransitionPosition { position }
        }
        StudioIntent::CommitManualTransition => CommandPayload::CommitManualTransition,
        StudioIntent::CancelManualTransition => CommandPayload::CancelManualTransition,
    }
}

fn intent_supported_by_session(session: &fm_client::TcpSession, intent: StudioIntent) -> bool {
    intent_supported(intent, session_protocol(session))
}

fn session_protocol(session: &fm_client::TcpSession) -> Option<ProtocolVersion> {
    session.client().session().map(|session| session.protocol)
}

const fn intent_supported(intent: StudioIntent, protocol: Option<ProtocolVersion>) -> bool {
    match protocol {
        Some(protocol) => intent_payload(intent).is_supported_by(protocol),
        None => false,
    }
}

fn pop_supported_deferred_intent(
    deferred: &mut VecDeque<StudioIntent>,
    protocol: Option<ProtocolVersion>,
) -> Option<StudioIntent> {
    deferred
        .front()
        .copied()
        .filter(|intent| intent_supported(*intent, protocol))?;
    deferred.pop_front()
}

const fn lifecycle_status(lifecycle: LifecycleState) -> StudioConnectionStatus {
    match lifecycle {
        LifecycleState::LaunchingDaemon => StudioConnectionStatus::Launching,
        LifecycleState::Disconnected => StudioConnectionStatus::Disconnected,
        LifecycleState::Connecting => StudioConnectionStatus::Connecting,
        LifecycleState::Synchronizing => StudioConnectionStatus::Synchronizing,
        LifecycleState::Ready => StudioConnectionStatus::Ready,
        LifecycleState::Backoff(_) => StudioConnectionStatus::Backoff,
        LifecycleState::PendingIncompatible => StudioConnectionStatus::PendingIncompatible,
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
    let supports_wipe = client.session().is_some_and(|session| {
        session.protocol.major == WIPE_PROTOCOL_VERSION.major
            && session.protocol.minor >= WIPE_PROTOCOL_VERSION.minor
    });
    let supports_alpha_fade = client.session().is_some_and(|session| {
        session.protocol.major == ALPHA_FADE_PROTOCOL_VERSION.major
            && session.protocol.minor >= ALPHA_FADE_PROTOCOL_VERSION.minor
    });
    let supports_manual_transition = client.session().is_some_and(|session| {
        session.protocol.major == MANUAL_TRANSITION_PROTOCOL_VERSION.major
            && session.protocol.minor >= MANUAL_TRANSITION_PROTOCOL_VERSION.minor
    });
    let supports_fade_to_black = client.session().is_some_and(|session| {
        session.protocol.major == FADE_TO_BLACK_PROTOCOL_VERSION.major
            && session.protocol.minor >= FADE_TO_BLACK_PROTOCOL_VERSION.minor
    });
    let mut state = StudioUiState::new(connection_status)
        .with_switcher_permissions(can_select_preview, can_transition)
        .with_wipe_support(supports_wipe)
        .with_alpha_fade_support(supports_alpha_fade)
        .with_manual_transition_support(supports_manual_transition)
        .with_fade_to_black_support(supports_fade_to_black);
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
    deferred: &VecDeque<StudioIntent>,
    error: Option<String>,
) -> bool {
    let mut state = runtime_state(runtime, error);
    let incompatible = deferred
        .front()
        .copied()
        .filter(|intent| !intent_supported_by_session(runtime.session(), *intent));
    state.notice = Some(if let Some(intent) = incompatible {
        format!(
            "Blocked {} command(s) in operator FIFO; head {} requires protocol {}",
            deferred.len(),
            intent_label(intent),
            intent_payload(intent).minimum_protocol_version(),
        )
    } else {
        format!("Queued {} command(s) in operator FIFO", deferred.len())
    });
    publisher.publish(state)
}

const fn intent_label(intent: StudioIntent) -> &'static str {
    match intent {
        StudioIntent::SelectPreview(_) => "Select Preview",
        StudioIntent::Cut => "Cut",
        StudioIntent::Fade { .. } => "Fade",
        StudioIntent::AlphaFade { .. } => "AlphaFade",
        StudioIntent::Wipe { .. } => "Wipe",
        StudioIntent::FadeToBlack { active: true, .. } => "Fade to Black",
        StudioIntent::FadeToBlack { active: false, .. } => "Fade to Live",
        StudioIntent::StartManualTransition { .. } => "T-bar Start",
        StudioIntent::SetManualTransitionPosition { .. } => "T-bar Position",
        StudioIntent::CommitManualTransition => "T-bar Commit",
        StudioIntent::CancelManualTransition => "T-bar Cancel",
    }
}

fn publish_recovery_runtime(
    runtime: &mut StudioRuntime,
    publisher: &StatePublisher,
    recovery: &WorkerRecovery,
) -> bool {
    if recovery.deferred_intents.is_empty() {
        publish_runtime(runtime, publisher, recovery.error())
    } else {
        publish_deferred_runtime(
            runtime,
            publisher,
            &recovery.deferred_intents,
            recovery.error(),
        )
    }
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
        FadeToBlackPosition, FadeToBlackState, HandshakeOutcome, HandshakeResponse, LineDecoder,
        ManualTransitionKind, ManualTransitionPosition, ManualTransitionState,
        ManualTransitionStatus, ProtocolVersion, Role, RuntimeEventMessage, RuntimeLifecycleEvent,
        ServerIdentity, SnapshotMessage, SnapshotReason, WireMessage, encode_line,
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
        assert_eq!(
            lifecycle_status(LifecycleState::PendingIncompatible),
            StudioConnectionStatus::PendingIncompatible
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
        assert_eq!(
            intent_payload(StudioIntent::AlphaFade {
                duration_frames: u32::MAX,
            }),
            CommandPayload::AlphaFade {
                duration_frames: u32::MAX,
            }
        );
        assert_eq!(
            intent_payload(StudioIntent::Wipe {
                duration_frames: u32::MAX,
            }),
            CommandPayload::Wipe {
                duration_frames: u32::MAX,
            }
        );
        assert_eq!(
            intent_payload(StudioIntent::FadeToBlack {
                active: true,
                duration_frames: 45,
            }),
            CommandPayload::FadeToBlack {
                active: true,
                duration_frames: 45,
            }
        );
        assert_eq!(
            intent_payload(StudioIntent::StartManualTransition {
                kind: fm_protocol::ManualTransitionKind::Wipe,
            }),
            CommandPayload::StartManualTransition {
                kind: fm_protocol::ManualTransitionKind::Wipe,
            }
        );
        for position in [
            fm_protocol::ManualTransitionPosition::START,
            fm_protocol::ManualTransitionPosition::new(2_500).unwrap(),
            fm_protocol::ManualTransitionPosition::END,
        ] {
            assert_eq!(
                intent_payload(StudioIntent::SetManualTransitionPosition { position }),
                CommandPayload::SetManualTransitionPosition { position }
            );
        }
        assert_eq!(
            intent_payload(StudioIntent::CommitManualTransition),
            CommandPayload::CommitManualTransition
        );
        assert_eq!(
            intent_payload(StudioIntent::CancelManualTransition),
            CommandPayload::CancelManualTransition
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

    #[test]
    fn unsupported_head_wipe_blocks_preview_until_fifo_can_resume() {
        let preview = StudioIntent::SelectPreview(InputId::new(NonZeroU128::new(9).unwrap()));
        let wipe = StudioIntent::Wipe {
            duration_frames: 60,
        };
        let fade = StudioIntent::Fade {
            duration_frames: 30,
        };
        let mut deferred = VecDeque::from([wipe, preview, StudioIntent::Cut, fade]);

        assert_eq!(
            pop_supported_deferred_intent(&mut deferred, Some(ProtocolVersion::new(1, 2))),
            None
        );
        assert_eq!(
            deferred,
            VecDeque::from([wipe, preview, StudioIntent::Cut, fade])
        );
        assert_eq!(
            pop_supported_deferred_intent(&mut deferred, Some(WIPE_PROTOCOL_VERSION)),
            Some(wipe)
        );
        assert_eq!(
            pop_supported_deferred_intent(&mut deferred, Some(WIPE_PROTOCOL_VERSION)),
            Some(preview)
        );
        assert_eq!(
            pop_supported_deferred_intent(&mut deferred, Some(WIPE_PROTOCOL_VERSION)),
            Some(StudioIntent::Cut)
        );
        assert_eq!(
            pop_supported_deferred_intent(&mut deferred, Some(WIPE_PROTOCOL_VERSION)),
            Some(fade)
        );
    }

    #[test]
    fn unsupported_head_fade_to_black_blocks_fifo_until_protocol_1_5() {
        let fade_to_black = StudioIntent::FadeToBlack {
            active: true,
            duration_frames: 60,
        };
        let mut deferred = VecDeque::from([fade_to_black, StudioIntent::Cut]);

        assert_eq!(
            pop_supported_deferred_intent(&mut deferred, Some(MANUAL_TRANSITION_PROTOCOL_VERSION),),
            None
        );
        assert_eq!(deferred, VecDeque::from([fade_to_black, StudioIntent::Cut]));
        assert_eq!(
            pop_supported_deferred_intent(&mut deferred, Some(FADE_TO_BLACK_PROTOCOL_VERSION),),
            Some(fade_to_black)
        );
        assert_eq!(
            pop_supported_deferred_intent(&mut deferred, Some(FADE_TO_BLACK_PROTOCOL_VERSION),),
            Some(StudioIntent::Cut)
        );
    }

    #[test]
    fn unsupported_head_alpha_fade_blocks_fifo_until_protocol_1_6() {
        let alpha_fade = StudioIntent::AlphaFade {
            duration_frames: 60,
        };
        let mut deferred = VecDeque::from([alpha_fade, StudioIntent::Cut]);

        assert_eq!(
            pop_supported_deferred_intent(&mut deferred, Some(FADE_TO_BLACK_PROTOCOL_VERSION)),
            None
        );
        assert_eq!(deferred, VecDeque::from([alpha_fade, StudioIntent::Cut]));
        assert_eq!(
            pop_supported_deferred_intent(&mut deferred, Some(ALPHA_FADE_PROTOCOL_VERSION)),
            Some(alpha_fade)
        );
        assert_eq!(
            pop_supported_deferred_intent(&mut deferred, Some(ALPHA_FADE_PROTOCOL_VERSION)),
            Some(StudioIntent::Cut)
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

    fn manual_status(
        kind: ManualTransitionKind,
        interval_start: u16,
        position: u16,
    ) -> ManualTransitionStatus {
        ManualTransitionStatus::Active(ManualTransitionState {
            kind,
            from: wire_input(1),
            to: wire_input(2),
            interval_start: ManualTransitionPosition::new(interval_start).unwrap(),
            position: ManualTransitionPosition::new(position).unwrap(),
        })
    }

    fn manual_projection(negotiated: ProtocolVersion) -> Option<ManualTransitionStatus> {
        (negotiated.major == MANUAL_TRANSITION_PROTOCOL_VERSION.major
            && negotiated.minor >= MANUAL_TRANSITION_PROTOCOL_VERSION.minor)
            .then_some(ManualTransitionStatus::Inactive)
    }

    fn fade_to_black_projection(negotiated: ProtocolVersion) -> Option<FadeToBlackState> {
        (negotiated.major == FADE_TO_BLACK_PROTOCOL_VERSION.major
            && negotiated.minor >= FADE_TO_BLACK_PROTOCOL_VERSION.minor)
            .then_some(FadeToBlackState {
                target_active: false,
                position: FadeToBlackPosition::LIVE,
            })
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
        accept_worker_snapshot_version_at(
            listener,
            ProtocolVersion::new(1, 2),
            revision,
            desired_preview,
            realized_preview,
        )
    }

    fn accept_worker_snapshot_version_at(
        listener: &TcpListener,
        negotiated: ProtocolVersion,
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
            negotiated,
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
            desired_manual_transition: manual_projection(negotiated),
            realized_manual_transition: manual_projection(negotiated),
            desired_fade_to_black: fade_to_black_projection(negotiated),
            realized_fade_to_black: fade_to_black_projection(negotiated),
        }));
        peer
    }

    fn accept_worker_resume(listener: &TcpListener, current_revision: u64) -> (FakePeer, u64) {
        accept_worker_resume_version(listener, ProtocolVersion::new(1, 2), current_revision)
    }

    fn accept_worker_resume_version(
        listener: &TcpListener,
        negotiated: ProtocolVersion,
        current_revision: u64,
    ) -> (FakePeer, u64) {
        let mut peer = FakePeer::accept(listener);
        let WireMessage::HandshakeRequest(request) = peer.receive() else {
            panic!("expected reconnect handshake request");
        };
        let cursor = request.resume_cursor.expect("resume cursor");
        peer.send(&WireMessage::HandshakeResponse(HandshakeResponse {
            negotiated,
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

    fn accept_worker_reconnect_snapshot_version(
        listener: &TcpListener,
        negotiated: ProtocolVersion,
        revision: u64,
        desired: (u128, u128),
        realized: (u128, u128),
    ) -> FakePeer {
        let mut peer = FakePeer::accept(listener);
        let WireMessage::HandshakeRequest(request) = peer.receive() else {
            panic!("expected reconnect snapshot handshake request");
        };
        assert!(request.resume_cursor.is_some());
        peer.send(&WireMessage::HandshakeResponse(HandshakeResponse {
            negotiated,
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
                reason: SnapshotReason::HistoryUnavailable,
            },
        }));
        peer.send(&WireMessage::Snapshot(SnapshotMessage {
            engine: test_engine(),
            revision,
            show_name: "Worker test".to_owned(),
            inputs: vec![wire_input(1), wire_input(2), wire_input(3)],
            desired_program: wire_input(desired.0),
            desired_preview: wire_input(desired.1),
            realized_program: wire_input(realized.0),
            realized_preview: wire_input(realized.1),
            desired_manual_transition: manual_projection(negotiated),
            realized_manual_transition: manual_projection(negotiated),
            desired_fade_to_black: fade_to_black_projection(negotiated),
            realized_fade_to_black: fade_to_black_projection(negotiated),
        }));
        peer
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
                manual_transition: None,
                fade_to_black: None,
            },
        }));
        peer.send(&WireMessage::RuntimeEvent(RuntimeEventMessage {
            server: test_server(),
            revision: 5,
            generation: 1,
            sequence: 1,
            event: RuntimeLifecycleEvent::Realized {
                domain: "switcher".to_owned(),
                manual_transition: None,
                fade_to_black: None,
            },
        }));
    }

    fn serve_worker_automatic_transition(
        listener: &TcpListener,
        protocol: ProtocolVersion,
        payload: CommandPayload,
    ) {
        let mut peer = accept_worker_snapshot_version_at(listener, protocol, 4, 2, 2);
        let WireMessage::Command(command) = peer.receive() else {
            panic!("expected automatic transition command");
        };
        assert_eq!(command.protocol, protocol);
        assert_eq!(command.payload, payload);
        assert_eq!(command.expected_revision, Some(4));
        assert_eq!(command.deadline_ms, None);
        assert!(!command.idempotency_key.is_empty());
        peer.send(&WireMessage::CommandResult(CommandResult::Accepted {
            id: command.id,
            revision: 5,
            scheduled_frame: Some(9),
        }));
        peer.send(&WireMessage::Event(EventMessage {
            cursor: EventCursor {
                engine: test_engine(),
                revision: 5,
            },
            payload: EventPayload::DesiredSwitcher {
                program: wire_input(2),
                preview: wire_input(1),
                manual_transition: manual_projection(protocol),
                fade_to_black: fade_to_black_projection(protocol),
            },
        }));
        peer.send(&WireMessage::RuntimeEvent(RuntimeEventMessage {
            server: test_server(),
            revision: 5,
            generation: 1,
            sequence: 1,
            event: RuntimeLifecycleEvent::Realized {
                domain: "switcher".to_owned(),
                manual_transition: manual_projection(protocol),
                fade_to_black: fade_to_black_projection(protocol),
            },
        }));
    }

    fn serve_worker_fade_to_black(listener: &TcpListener) {
        let mut peer =
            accept_worker_snapshot_version_at(listener, FADE_TO_BLACK_PROTOCOL_VERSION, 4, 2, 2);
        for (revision, active, duration_frames, position) in [
            (5, true, 45, FadeToBlackPosition::BLACK),
            (6, false, 20, FadeToBlackPosition::LIVE),
        ] {
            let WireMessage::Command(command) = peer.receive() else {
                panic!("expected Fade-to-Black command");
            };
            assert_eq!(command.protocol, FADE_TO_BLACK_PROTOCOL_VERSION);
            assert_eq!(
                command.payload,
                CommandPayload::FadeToBlack {
                    active,
                    duration_frames,
                }
            );
            assert_eq!(command.expected_revision, Some(revision - 1));
            assert_eq!(command.deadline_ms, None);
            assert!(!command.idempotency_key.is_empty());
            peer.send(&WireMessage::CommandResult(CommandResult::Accepted {
                id: command.id,
                revision,
                scheduled_frame: Some(revision),
            }));
            let fade_to_black = FadeToBlackState {
                target_active: active,
                position,
            };
            peer.send(&WireMessage::Event(EventMessage {
                cursor: EventCursor {
                    engine: test_engine(),
                    revision,
                },
                payload: EventPayload::DesiredSwitcher {
                    program: wire_input(1),
                    preview: wire_input(2),
                    manual_transition: Some(ManualTransitionStatus::Inactive),
                    fade_to_black: Some(fade_to_black),
                },
            }));
            peer.send(&WireMessage::RuntimeEvent(RuntimeEventMessage {
                server: test_server(),
                revision,
                generation: 1,
                sequence: revision - 4,
                event: RuntimeLifecycleEvent::Realized {
                    domain: "switcher".to_owned(),
                    manual_transition: Some(ManualTransitionStatus::Inactive),
                    fade_to_black: Some(fade_to_black),
                },
            }));
        }
    }

    fn serve_worker_manual_t_bar(listener: &TcpListener) {
        let mut peer = accept_worker_snapshot_version_at(
            listener,
            MANUAL_TRANSITION_PROTOCOL_VERSION,
            4,
            2,
            2,
        );
        let steps = [
            (
                CommandPayload::StartManualTransition {
                    kind: ManualTransitionKind::Wipe,
                },
                Some((ManualTransitionKind::Wipe, 0, 0)),
                (1, 2),
            ),
            (
                CommandPayload::SetManualTransitionPosition {
                    position: ManualTransitionPosition::END,
                },
                Some((ManualTransitionKind::Wipe, 0, 10_000)),
                (1, 2),
            ),
            (
                CommandPayload::SetManualTransitionPosition {
                    position: ManualTransitionPosition::new(2_500).unwrap(),
                },
                Some((ManualTransitionKind::Wipe, 0, 2_500)),
                (1, 2),
            ),
            (CommandPayload::CancelManualTransition, None, (1, 2)),
            (
                CommandPayload::StartManualTransition {
                    kind: ManualTransitionKind::Fade,
                },
                Some((ManualTransitionKind::Fade, 0, 0)),
                (1, 2),
            ),
            (
                CommandPayload::SetManualTransitionPosition {
                    position: ManualTransitionPosition::END,
                },
                Some((ManualTransitionKind::Fade, 0, 10_000)),
                (1, 2),
            ),
            (CommandPayload::CommitManualTransition, None, (2, 1)),
        ];

        for (offset, (payload, active, routing)) in steps.into_iter().enumerate() {
            let WireMessage::Command(command) = peer.receive() else {
                panic!("expected manual T-bar command");
            };
            let revision = 5 + u64::try_from(offset).unwrap();
            assert_eq!(command.protocol, MANUAL_TRANSITION_PROTOCOL_VERSION);
            assert_eq!(command.expected_revision, Some(revision - 1));
            assert_eq!(command.payload, payload);
            peer.send(&WireMessage::CommandResult(CommandResult::Accepted {
                id: command.id,
                revision,
                scheduled_frame: Some(revision),
            }));
            let desired_manual_transition = active.map(|(kind, interval_start, position)| {
                manual_status(kind, interval_start, position)
            });
            peer.send(&WireMessage::Event(EventMessage {
                cursor: EventCursor {
                    engine: test_engine(),
                    revision,
                },
                payload: EventPayload::DesiredSwitcher {
                    program: wire_input(routing.0),
                    preview: wire_input(routing.1),
                    manual_transition: Some(
                        desired_manual_transition.unwrap_or(ManualTransitionStatus::Inactive),
                    ),
                    fade_to_black: None,
                },
            }));
            let realized_manual_transition =
                active.map(|(kind, _, position)| manual_status(kind, position, position));
            peer.send(&WireMessage::RuntimeEvent(RuntimeEventMessage {
                server: test_server(),
                revision,
                generation: revision,
                sequence: 1,
                event: RuntimeLifecycleEvent::Realized {
                    domain: "switcher".to_owned(),
                    manual_transition: Some(
                        realized_manual_transition.unwrap_or(ManualTransitionStatus::Inactive),
                    ),
                    fade_to_black: None,
                },
            }));
        }
    }

    fn serve_worker_receipt_collision(listener: &TcpListener) {
        let mut first = accept_worker_snapshot(listener);
        let WireMessage::Command(command) = first.receive() else {
            panic!("expected command before replay collision");
        };
        assert_eq!(
            command.payload,
            CommandPayload::SelectPreview {
                input: wire_input(3)
            }
        );
        first.send(&WireMessage::CommandResult(CommandResult::Accepted {
            id: "old-evicted-command".to_owned(),
            revision: 4,
            scheduled_frame: None,
        }));
        assert_eq!(first.stream.read(&mut [0_u8; 1]).unwrap(), 0);

        let mut second = accept_worker_snapshot_at(listener, 4, 2, 2);
        let WireMessage::Command(after_resync) = second.receive() else {
            panic!("expected explicit command after collision resync");
        };
        assert_eq!(
            after_resync.payload,
            CommandPayload::Cut,
            "terminally uncertain command was retried"
        );
        second.send(&WireMessage::CommandResult(CommandResult::Accepted {
            id: after_resync.id,
            revision: 5,
            scheduled_frame: None,
        }));
        second.send(&WireMessage::Event(EventMessage {
            cursor: EventCursor {
                engine: test_engine(),
                revision: 5,
            },
            payload: EventPayload::DesiredSwitcher {
                program: wire_input(2),
                preview: wire_input(1),
                manual_transition: None,
                fade_to_black: None,
            },
        }));
        second.send(&WireMessage::RuntimeEvent(RuntimeEventMessage {
            server: test_server(),
            revision: 5,
            generation: 1,
            sequence: 1,
            event: RuntimeLifecycleEvent::Realized {
                domain: "switcher".to_owned(),
                manual_transition: None,
                fade_to_black: None,
            },
        }));
    }

    fn serve_worker_deferred_wipe(listener: &TcpListener) {
        let mut first = accept_worker_snapshot_version_at(listener, WIPE_PROTOCOL_VERSION, 4, 2, 2);
        let WireMessage::Command(original) = first.receive() else {
            panic!("expected original Select Preview command");
        };
        drop(first);

        let (mut downgraded, revision) =
            accept_worker_resume_version(listener, ProtocolVersion::new(1, 2), 5);
        assert_eq!(revision, 4);
        downgraded.send(&WireMessage::Event(EventMessage {
            cursor: EventCursor {
                engine: test_engine(),
                revision: 5,
            },
            payload: EventPayload::DesiredSwitcher {
                program: wire_input(1),
                preview: wire_input(3),
                manual_transition: None,
                fade_to_black: None,
            },
        }));
        let WireMessage::Command(retried) = downgraded.receive() else {
            panic!("expected original command retry, not deferred Wipe");
        };
        assert_eq!(retried.protocol, ProtocolVersion::new(1, 2));
        assert_eq!(retried.id, original.id);
        assert_eq!(retried.idempotency_key, original.idempotency_key);
        assert_eq!(retried.payload, original.payload);
        downgraded.send(&WireMessage::CommandResult(CommandResult::Accepted {
            id: retried.id,
            revision: 5,
            scheduled_frame: None,
        }));
        assert_eq!(downgraded.stream.read(&mut [0_u8; 1]).unwrap(), 0);

        let mut compatible =
            accept_worker_snapshot_version_at(listener, WIPE_PROTOCOL_VERSION, 5, 3, 3);
        let WireMessage::Command(wipe) = compatible.receive() else {
            panic!("expected deferred Wipe command");
        };
        assert_eq!(wipe.protocol, WIPE_PROTOCOL_VERSION);
        assert_eq!(
            wipe.payload,
            CommandPayload::Wipe {
                duration_frames: 60
            }
        );
        compatible.send(&WireMessage::CommandResult(CommandResult::Accepted {
            id: wipe.id,
            revision: 6,
            scheduled_frame: None,
        }));
        compatible.send(&WireMessage::Event(EventMessage {
            cursor: EventCursor {
                engine: test_engine(),
                revision: 6,
            },
            payload: EventPayload::DesiredSwitcher {
                program: wire_input(3),
                preview: wire_input(1),
                manual_transition: None,
                fade_to_black: None,
            },
        }));
        compatible.send(&WireMessage::RuntimeEvent(RuntimeEventMessage {
            server: test_server(),
            revision: 6,
            generation: 1,
            sequence: 1,
            event: RuntimeLifecycleEvent::Realized {
                domain: "switcher".to_owned(),
                manual_transition: None,
                fade_to_black: None,
            },
        }));

        serve_deferred_preview(&mut compatible);
    }

    fn serve_deferred_preview(peer: &mut FakePeer) {
        let WireMessage::Command(preview) = peer.receive() else {
            panic!("expected Preview after deferred Wipe");
        };
        assert_eq!(
            preview.payload,
            CommandPayload::SelectPreview {
                input: wire_input(2)
            }
        );
        assert_eq!(preview.expected_revision, Some(6));
        peer.send(&WireMessage::CommandResult(CommandResult::Accepted {
            id: preview.id,
            revision: 7,
            scheduled_frame: None,
        }));
        peer.send(&WireMessage::Event(EventMessage {
            cursor: EventCursor {
                engine: test_engine(),
                revision: 7,
            },
            payload: EventPayload::DesiredSwitcher {
                program: wire_input(3),
                preview: wire_input(2),
                manual_transition: None,
                fade_to_black: None,
            },
        }));
        peer.send(&WireMessage::RuntimeEvent(RuntimeEventMessage {
            server: test_server(),
            revision: 7,
            generation: 1,
            sequence: 2,
            event: RuntimeLifecycleEvent::Realized {
                domain: "switcher".to_owned(),
                manual_transition: None,
                fade_to_black: None,
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
                manual_transition: None,
                fade_to_black: None,
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
                manual_transition: None,
                fade_to_black: None,
            },
        }));
        third.send(&WireMessage::RuntimeEvent(RuntimeEventMessage {
            server: test_server(),
            revision: 6,
            generation: 1,
            sequence: 1,
            event: RuntimeLifecycleEvent::Realized {
                domain: "switcher".to_owned(),
                manual_transition: None,
                fade_to_black: None,
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
    fn worker_wipe_flow_preserves_exact_envelope_result_event_and_runtime_order() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            serve_worker_automatic_transition(
                &listener,
                WIPE_PROTOCOL_VERSION,
                CommandPayload::Wipe {
                    duration_frames: 45,
                },
            );
        });
        let (requests, states, worker) = spawn_test_worker(address);
        wait_until_ready(&states);
        try_enqueue(
            &requests,
            WorkerRequest::Intent(StudioIntent::Wipe {
                duration_frames: 45,
            }),
        )
        .unwrap();

        let mut pending_seen = false;
        loop {
            let state = states.recv_timeout(Duration::from_secs(3)).unwrap();
            pending_seen |= state.pending_commands == 1;
            if state.pending_commands == 0
                && state.view.as_ref().is_some_and(|view| {
                    view.cursor.revision.get() == 5
                        && view.switcher.realized.program == wire_input(2).to_domain()
                        && view.switcher.realized.preview == wire_input(1).to_domain()
                })
            {
                assert!(state.transition_protocol.automatic.wipe);
                assert_eq!(state.error, None);
                break;
            }
        }
        assert!(pending_seen);
        try_enqueue(&requests, WorkerRequest::Shutdown).unwrap();
        worker.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn worker_alpha_fade_flow_preserves_protocol_duration_and_runtime_order() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            serve_worker_automatic_transition(
                &listener,
                ALPHA_FADE_PROTOCOL_VERSION,
                CommandPayload::AlphaFade {
                    duration_frames: 45,
                },
            );
        });
        let (requests, states, worker) = spawn_test_worker(address);
        wait_until_ready(&states);
        try_enqueue(
            &requests,
            WorkerRequest::Intent(StudioIntent::AlphaFade {
                duration_frames: 45,
            }),
        )
        .unwrap();

        let mut pending_seen = false;
        loop {
            let state = states.recv_timeout(Duration::from_secs(3)).unwrap();
            pending_seen |= state.pending_commands == 1;
            if state.pending_commands == 0
                && state.view.as_ref().is_some_and(|view| {
                    view.cursor.revision.get() == 5
                        && view.switcher.realized.program == wire_input(2).to_domain()
                        && view.switcher.realized.preview == wire_input(1).to_domain()
                })
            {
                assert!(state.transition_protocol.automatic.alpha_fade);
                assert_eq!(state.error, None);
                break;
            }
        }
        assert!(pending_seen);
        try_enqueue(&requests, WorkerRequest::Shutdown).unwrap();
        worker.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn worker_fade_to_black_flow_observes_black_and_live_realization() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || serve_worker_fade_to_black(&listener));
        let (requests, states, worker) = spawn_test_worker(address);
        let ready = loop {
            let state = states.recv_timeout(Duration::from_secs(3)).unwrap();
            if state.connection_status == StudioConnectionStatus::Ready {
                break state;
            }
        };
        assert!(ready.transition_protocol.fade_to_black);

        for (revision, active, duration_frames, position) in [
            (5, true, 45, FadeToBlackPosition::BLACK),
            (6, false, 20, FadeToBlackPosition::LIVE),
        ] {
            try_enqueue(
                &requests,
                WorkerRequest::Intent(StudioIntent::FadeToBlack {
                    active,
                    duration_frames,
                }),
            )
            .unwrap();
            loop {
                let state = states.recv_timeout(Duration::from_secs(3)).unwrap();
                assert_eq!(state.error, None, "FTB worker state: {state:?}");
                let Some(view) = state.view.as_ref() else {
                    continue;
                };
                if state.pending_commands == 0
                    && view.cursor.revision.get() == revision
                    && view.switcher.desired_fade_to_black
                        == (FadeToBlackState {
                            target_active: active,
                            position,
                        })
                    && view.switcher.realized_fade_to_black
                        == (FadeToBlackState {
                            target_active: active,
                            position,
                        })
                {
                    break;
                }
            }
        }

        try_enqueue(&requests, WorkerRequest::Shutdown).unwrap();
        worker.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn worker_manual_t_bar_flow_observes_hold_reverse_cancel_and_commit_from_model() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || serve_worker_manual_t_bar(&listener));
        let (requests, states, worker) = spawn_test_worker(address);
        let ready = loop {
            let state = states.recv_timeout(Duration::from_secs(3)).unwrap();
            if state.connection_status == StudioConnectionStatus::Ready {
                break state;
            }
        };
        assert!(ready.transition_protocol.manual);

        let steps = [
            (
                StudioIntent::StartManualTransition {
                    kind: ManualTransitionKind::Wipe,
                },
                5,
                Some((ManualTransitionKind::Wipe, 0)),
            ),
            (
                StudioIntent::SetManualTransitionPosition {
                    position: ManualTransitionPosition::END,
                },
                6,
                Some((ManualTransitionKind::Wipe, 10_000)),
            ),
            (
                StudioIntent::SetManualTransitionPosition {
                    position: ManualTransitionPosition::new(2_500).unwrap(),
                },
                7,
                Some((ManualTransitionKind::Wipe, 2_500)),
            ),
            (StudioIntent::CancelManualTransition, 8, None),
            (
                StudioIntent::StartManualTransition {
                    kind: ManualTransitionKind::Fade,
                },
                9,
                Some((ManualTransitionKind::Fade, 0)),
            ),
            (
                StudioIntent::SetManualTransitionPosition {
                    position: ManualTransitionPosition::END,
                },
                10,
                Some((ManualTransitionKind::Fade, 10_000)),
            ),
            (StudioIntent::CommitManualTransition, 11, None),
        ];

        for (intent, revision, expected) in steps {
            try_enqueue(&requests, WorkerRequest::Intent(intent)).unwrap();
            loop {
                let state = states.recv_timeout(Duration::from_secs(3)).unwrap();
                assert_eq!(state.error, None, "manual worker state: {state:?}");
                let Some(view) = state.view.as_ref() else {
                    continue;
                };
                if state.pending_commands != 0 || view.cursor.revision.get() != revision {
                    continue;
                }
                let desired_matches = match (view.switcher.desired_manual_transition, expected) {
                    (
                        fm_ui_model::ManualTransitionStatus::Active(active),
                        Some((kind, position)),
                    ) => active.kind == kind && active.position.basis_points() == position,
                    (fm_ui_model::ManualTransitionStatus::Inactive, None) => true,
                    _ => false,
                };
                let realized_matches = match (view.switcher.realized_manual_transition, expected) {
                    (
                        fm_ui_model::ManualTransitionStatus::Active(active),
                        Some((kind, position)),
                    ) => {
                        active.kind == kind
                            && active.interval_start.basis_points() == position
                            && active.position.basis_points() == position
                    }
                    (fm_ui_model::ManualTransitionStatus::Inactive, None) => true,
                    _ => false,
                };
                if !desired_matches || !realized_matches {
                    continue;
                }
                if revision == 11 {
                    assert_eq!(view.switcher.realized.program, wire_input(2).to_domain());
                    assert_eq!(view.switcher.realized.preview, wire_input(1).to_domain());
                }
                break;
            }
        }

        try_enqueue(&requests, WorkerRequest::Shutdown).unwrap();
        worker.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn runtime_state_hides_fade_to_black_after_protocol_1_5_downgrade() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut first = accept_worker_snapshot_version_at(
                &listener,
                FADE_TO_BLACK_PROTOCOL_VERSION,
                4,
                2,
                2,
            );
            assert_eq!(first.stream.read(&mut [0_u8; 1]).unwrap(), 0);
            accept_worker_reconnect_snapshot_version(
                &listener,
                MANUAL_TRANSITION_PROTOCOL_VERSION,
                4,
                (1, 2),
                (1, 2),
            );
        });
        let config = StudioConfig {
            connection: ConnectionConfig::Existing(ExistingConfig {
                address,
                expected_project_id: test_project_id(),
            }),
            client_id: "support-change".to_owned(),
            desired_role: Role::Operator,
            restart_policy: RestartPolicy::default(),
        };
        let mut runtime = StudioRuntime::new(config).unwrap();
        runtime.connect(CONNECT_TIMEOUT).unwrap();
        let current = runtime_state(&mut runtime, None);
        assert!(current.transition_protocol.automatic.wipe);
        assert!(current.transition_protocol.manual);
        assert!(current.transition_protocol.fade_to_black);
        assert!(current.can_transition);
        runtime.session_mut().disconnect().unwrap();
        runtime
            .reconnect(Duration::from_millis(250), CONNECT_TIMEOUT)
            .unwrap();
        let downgraded = runtime_state(&mut runtime, None);
        assert!(downgraded.transition_protocol.automatic.wipe);
        assert!(downgraded.transition_protocol.manual);
        assert!(!downgraded.transition_protocol.fade_to_black);
        assert!(downgraded.can_transition, "Cut/Fade permission was lost");
        server.join().unwrap();
    }

    #[test]
    fn runtime_state_hides_alpha_fade_after_protocol_1_6_downgrade() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut first =
                accept_worker_snapshot_version_at(&listener, ALPHA_FADE_PROTOCOL_VERSION, 4, 2, 2);
            assert_eq!(first.stream.read(&mut [0_u8; 1]).unwrap(), 0);
            accept_worker_reconnect_snapshot_version(
                &listener,
                FADE_TO_BLACK_PROTOCOL_VERSION,
                4,
                (1, 2),
                (1, 2),
            );
        });
        let config = StudioConfig {
            connection: ConnectionConfig::Existing(ExistingConfig {
                address,
                expected_project_id: test_project_id(),
            }),
            client_id: "alpha-support-change".to_owned(),
            desired_role: Role::Operator,
            restart_policy: RestartPolicy::default(),
        };
        let mut runtime = StudioRuntime::new(config).unwrap();
        runtime.connect(CONNECT_TIMEOUT).unwrap();
        let current = runtime_state(&mut runtime, None);
        assert!(current.transition_protocol.automatic.alpha_fade);
        assert!(current.transition_protocol.fade_to_black);
        assert!(current.can_transition);
        runtime.session_mut().disconnect().unwrap();
        runtime
            .reconnect(Duration::from_millis(250), CONNECT_TIMEOUT)
            .unwrap();
        let downgraded = runtime_state(&mut runtime, None);
        assert!(!downgraded.transition_protocol.automatic.alpha_fade);
        assert!(downgraded.transition_protocol.fade_to_black);
        assert!(downgraded.can_transition, "Cut/Fade permission was lost");
        server.join().unwrap();
    }

    #[test]
    fn runtime_state_keeps_transition_permission_separate_from_protocol_support() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut peer = FakePeer::accept(&listener);
            let WireMessage::HandshakeRequest(request) = peer.receive() else {
                panic!("expected snapshot handshake request");
            };
            assert_eq!(request.resume_cursor, None);
            peer.send(&WireMessage::HandshakeResponse(HandshakeResponse {
                negotiated: FADE_TO_BLACK_PROTOCOL_VERSION,
                granted_role: Role::Operator,
                permissions: vec!["view_status".to_owned()],
                capabilities: CapabilityReportSummary {
                    digest: "sha256:worker-test".to_owned(),
                    total: 1,
                    available: 1,
                    degraded: 0,
                    unavailable: 0,
                },
                server: test_server(),
                current_revision: 4,
                outcome: HandshakeOutcome::Snapshot {
                    reason: SnapshotReason::NoCursor,
                },
            }));
            peer.send(&WireMessage::Snapshot(SnapshotMessage {
                engine: test_engine(),
                revision: 4,
                show_name: "Permission test".to_owned(),
                inputs: vec![wire_input(1), wire_input(2)],
                desired_program: wire_input(1),
                desired_preview: wire_input(2),
                realized_program: wire_input(1),
                realized_preview: wire_input(2),
                desired_manual_transition: Some(ManualTransitionStatus::Inactive),
                realized_manual_transition: Some(ManualTransitionStatus::Inactive),
                desired_fade_to_black: fade_to_black_projection(FADE_TO_BLACK_PROTOCOL_VERSION),
                realized_fade_to_black: fade_to_black_projection(FADE_TO_BLACK_PROTOCOL_VERSION),
            }));
        });
        let config = StudioConfig {
            connection: ConnectionConfig::Existing(ExistingConfig {
                address,
                expected_project_id: test_project_id(),
            }),
            client_id: "permission-test".to_owned(),
            desired_role: Role::Operator,
            restart_policy: RestartPolicy::default(),
        };
        let mut runtime = StudioRuntime::new(config).unwrap();
        runtime.connect(CONNECT_TIMEOUT).unwrap();
        let state = runtime_state(&mut runtime, None);
        assert_eq!(state.connection_status, StudioConnectionStatus::Ready);
        assert!(state.transition_protocol.manual);
        assert!(state.transition_protocol.fade_to_black);
        assert!(!state.can_transition);
        server.join().unwrap();
    }

    #[test]
    fn deferred_wipe_waits_through_a_1_2_reconnect_then_runs_on_1_3() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || serve_worker_deferred_wipe(&listener));
        let (requests, states, worker) = spawn_test_worker(address);
        wait_until_ready(&states);
        try_enqueue(
            &requests,
            WorkerRequest::Intent(StudioIntent::SelectPreview(wire_input(3).to_domain())),
        )
        .unwrap();
        let mut deferred = false;
        let mut saw_pending_notice = false;
        loop {
            let state = states.recv_timeout(Duration::from_secs(3)).unwrap();
            if state.connection_status == StudioConnectionStatus::Backoff && !deferred {
                try_enqueue(
                    &requests,
                    WorkerRequest::Intent(StudioIntent::Wipe {
                        duration_frames: 60,
                    }),
                )
                .unwrap();
                try_enqueue(
                    &requests,
                    WorkerRequest::Intent(StudioIntent::SelectPreview(wire_input(2).to_domain())),
                )
                .unwrap();
                deferred = true;
            }
            saw_pending_notice |= state.notice.as_deref().is_some_and(|notice| {
                notice.contains("Blocked 2 command(s) in operator FIFO")
                    && notice.contains("head Wipe requires protocol 1.3")
            });
            if deferred
                && state.pending_commands == 0
                && state.view.as_ref().is_some_and(|view| {
                    view.cursor.revision.get() == 7
                        && view.switcher.realized.program == wire_input(3).to_domain()
                        && view.switcher.realized.preview == wire_input(2).to_domain()
                })
            {
                assert!(state.transition_protocol.automatic.wipe);
                assert!(saw_pending_notice);
                break;
            }
        }
        try_enqueue(&requests, WorkerRequest::Shutdown).unwrap();
        worker.join().unwrap();
        server.join().unwrap();
    }

    fn serve_unresolved_manual_downgrade(listener: &TcpListener) {
        let mut first = accept_worker_snapshot_version_at(
            listener,
            MANUAL_TRANSITION_PROTOCOL_VERSION,
            4,
            2,
            2,
        );
        let WireMessage::Command(original) = first.receive() else {
            panic!("expected original manual command");
        };
        assert!(matches!(
            original.payload,
            CommandPayload::StartManualTransition {
                kind: ManualTransitionKind::Fade
            }
        ));
        drop(first);

        let (mut downgraded, revision) =
            accept_worker_resume_version(listener, WIPE_PROTOCOL_VERSION, 4);
        assert_eq!(revision, 4);
        assert_eq!(
            downgraded.stream.read(&mut [0_u8; 1]).unwrap(),
            0,
            "protocol 1.4 manual head or later Cut reached protocol 1.3"
        );

        let mut compatible = accept_worker_reconnect_snapshot_version(
            listener,
            MANUAL_TRANSITION_PROTOCOL_VERSION,
            4,
            (1, 2),
            (1, 2),
        );
        let WireMessage::Command(retried) = compatible.receive() else {
            panic!("expected unresolved manual retry");
        };
        assert_eq!(retried, original);
        compatible.send(&WireMessage::CommandResult(CommandResult::Rejected {
            id: retried.id,
            code: "conflict".to_owned(),
            message: "manual transition was not applied".to_owned(),
            fields: Vec::new(),
            current_revision: 4,
            retryable: false,
        }));

        let WireMessage::Command(cut) = compatible.receive() else {
            panic!("expected deferred Cut after manual head resolved");
        };
        assert_eq!(cut.protocol, MANUAL_TRANSITION_PROTOCOL_VERSION);
        assert_eq!(cut.payload, CommandPayload::Cut);
        assert_eq!(cut.expected_revision, Some(4));
        assert_ne!(cut.idempotency_key, original.idempotency_key);
        compatible.send(&WireMessage::CommandResult(CommandResult::Accepted {
            id: cut.id,
            revision: 5,
            scheduled_frame: None,
        }));
        compatible.send(&WireMessage::Event(EventMessage {
            cursor: EventCursor {
                engine: test_engine(),
                revision: 5,
            },
            payload: EventPayload::DesiredSwitcher {
                program: wire_input(2),
                preview: wire_input(1),
                manual_transition: Some(ManualTransitionStatus::Inactive),
                fade_to_black: None,
            },
        }));
        compatible.send(&WireMessage::RuntimeEvent(RuntimeEventMessage {
            server: test_server(),
            revision: 5,
            generation: 1,
            sequence: 1,
            event: RuntimeLifecycleEvent::Realized {
                domain: "switcher".to_owned(),
                manual_transition: Some(ManualTransitionStatus::Inactive),
                fade_to_black: None,
            },
        }));
    }

    #[test]
    fn unresolved_manual_head_blocks_fifo_on_1_3_and_retries_only_on_1_4() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || serve_unresolved_manual_downgrade(&listener));

        let (requests, states, worker) = spawn_test_worker(address);
        wait_until_ready(&states);
        try_enqueue(
            &requests,
            WorkerRequest::Intent(StudioIntent::StartManualTransition {
                kind: ManualTransitionKind::Fade,
            }),
        )
        .unwrap();
        let mut saw_pending_incompatible = false;
        let mut queued_cut = false;
        loop {
            let state = states.recv_timeout(Duration::from_secs(3)).unwrap();
            if state.connection_status == StudioConnectionStatus::PendingIncompatible {
                saw_pending_incompatible |= state.error.as_deref().is_some_and(|error| {
                    error.contains("requires protocol 1.4") && error.contains("negotiated 1.3")
                });
                if !queued_cut {
                    try_enqueue(&requests, WorkerRequest::Intent(StudioIntent::Cut)).unwrap();
                    queued_cut = true;
                }
            }
            if state.pending_commands == 0
                && queued_cut
                && state.view.as_ref().is_some_and(|view| {
                    view.cursor.revision.get() == 5
                        && view.switcher.realized.program == wire_input(2).to_domain()
                })
            {
                assert!(saw_pending_incompatible);
                assert!(state.notice.is_none());
                assert!(state.transition_protocol.automatic.wipe);
                assert!(state.transition_protocol.manual);
                break;
            }
        }
        try_enqueue(&requests, WorkerRequest::Shutdown).unwrap();
        worker.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn terminal_collision_remains_visible_after_snapshot_and_successful_command() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || serve_worker_receipt_collision(&listener));
        let (requests, states, worker) = spawn_test_worker(address);
        wait_until_ready(&states);
        try_enqueue(
            &requests,
            WorkerRequest::Intent(StudioIntent::SelectPreview(wire_input(3).to_domain())),
        )
        .unwrap();

        let mut saw_recovery = false;
        let mut sent_after_resync = false;
        loop {
            let state = states.recv_timeout(Duration::from_secs(3)).unwrap();
            saw_recovery |= matches!(
                state.connection_status,
                StudioConnectionStatus::Backoff | StudioConnectionStatus::Synchronizing
            );
            let sticky = state.error.as_deref().is_some_and(|error| {
                error.contains("Terminal command uncertainty remains")
                    && error.contains("worker-test:1")
                    && error.contains("old-evicted-command")
            });
            if saw_recovery
                && sticky
                && state.connection_status == StudioConnectionStatus::Ready
                && state.pending_commands == 0
                && !sent_after_resync
            {
                try_enqueue(&requests, WorkerRequest::Intent(StudioIntent::Cut)).unwrap();
                sent_after_resync = true;
            }
            if sent_after_resync
                && sticky
                && state.pending_commands == 0
                && state.view.as_ref().is_some_and(|view| {
                    view.cursor.revision.get() == 5
                        && view.switcher.realized.program == wire_input(2).to_domain()
                })
            {
                break;
            }
        }
        assert!(saw_recovery);
        try_enqueue(&requests, WorkerRequest::Shutdown).unwrap();
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
                    manual_transition: None,
                    fade_to_black: None,
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
                    manual_transition: None,
                    fade_to_black: None,
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
                    manual_transition: None,
                    fade_to_black: None,
                },
            }));
            second.send(&WireMessage::RuntimeEvent(RuntimeEventMessage {
                server: test_server(),
                revision: 5,
                generation: 1,
                sequence: 1,
                event: RuntimeLifecycleEvent::Realized {
                    domain: "switcher".to_owned(),
                    manual_transition: None,
                    fade_to_black: None,
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
                    manual_transition: None,
                    fade_to_black: None,
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
