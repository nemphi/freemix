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
use fm_protocol::{CommandPayload, CommandResult, DurableGap, WireInputId, WireMessage};
use fm_ui_egui::{StudioConnectionStatus, StudioIntent, StudioShell, StudioUiState};

use crate::{LifecycleState, StudioConfig, StudioRuntime};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(not(test))]
const PEER_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const PEER_WAIT_TIMEOUT: Duration = Duration::from_millis(200);
const IO_POLL_INTERVAL: Duration = Duration::from_millis(25);
#[cfg(not(test))]
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
#[cfg(test)]
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(25);
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
                if recovery.active() || !recovery.deferred_intents.is_empty() {
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
    let lifecycle = runtime.lifecycle();
    if let Ok(LifecycleState::DaemonExited { code }) = &lifecycle {
        recovery.realization_uncertain = runtime.session().client().model().view().is_some();
        let _ = runtime.session_mut().disconnect();
        recovery.reconnect_wait = ReconnectWait::from_runtime(runtime);
        recovery.visible_error = Some(format!(
            "Supervised daemon exited with code {code:?}; reconnecting after bounded backoff"
        ));
        if !publish_recovery_runtime(runtime, publisher, recovery) {
            return false;
        }
    }
    if matches!(lifecycle, Ok(LifecycleState::Ready)) {
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let wait_started = Instant::now();
        let mut shutdown = false;
        let heartbeat = runtime.send_heartbeat_cancellable(elapsed_ms, IO_POLL_INTERVAL, || {
            shutdown = cancellation_requested(
                requests,
                &mut recovery.deferred_intents,
                &mut recovery.deferred_rejections,
            );
            shutdown || wait_started.elapsed() >= PEER_WAIT_TIMEOUT
        });
        let result = heartbeat
            .map_err(|error| worker_error("Heartbeat failed", &error))
            .and_then(|heartbeat| {
                let event = runtime
                    .receive_cancellable(IO_POLL_INTERVAL, || {
                        shutdown = cancellation_requested(
                            requests,
                            &mut recovery.deferred_intents,
                            &mut recovery.deferred_rejections,
                        );
                        shutdown || wait_started.elapsed() >= PEER_WAIT_TIMEOUT
                    })
                    .map_err(|error| worker_error("Heartbeat acknowledgement failed", &error))?;
                match event {
                    SessionEvent::HeartbeatAcknowledged { acknowledgement }
                        if acknowledgement.heartbeat_sequence == heartbeat.sequence =>
                    {
                        Ok(())
                    }
                    other => Err(unexpected_failure("heartbeat acknowledgement", &other)),
                }
            });
        if shutdown {
            return false;
        }
        if let Err(error) = result {
            recovery.visible_error = Some(error.message().to_owned());
            if error.is_recoverable() {
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
                | TcpSessionError::Client(ClientError::InvalidHeartbeatAcknowledgement(_))
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
        StudioIntent::SetInputAudioStrip {
            input,
            gain_millidb,
            balance_basis_points,
            muted,
            soloed,
            follow_video,
            delay_samples,
        } => CommandPayload::SetInputAudioStrip {
            input: WireInputId::from_domain(input),
            gain_millidb,
            balance_basis_points,
            muted,
            soloed,
            follow_video,
            delay_samples,
        },
        StudioIntent::SelectPreview(input) => CommandPayload::SelectPreview {
            input: WireInputId::from_domain(input),
        },
        StudioIntent::Cut => CommandPayload::Cut,
        StudioIntent::Fade { duration_frames } => CommandPayload::Fade { duration_frames },
        StudioIntent::AlphaFade { duration_frames } => {
            CommandPayload::AlphaFade { duration_frames }
        }
        StudioIntent::Slide { duration_frames } => CommandPayload::Slide { duration_frames },
        StudioIntent::Zoom { duration_frames } => CommandPayload::Zoom { duration_frames },
        StudioIntent::Stinger {
            slot,
            duration_frames,
        } => CommandPayload::Stinger {
            slot,
            duration_frames,
        },
        StudioIntent::TakeOverlay { channel, source } => CommandPayload::TakeOverlay {
            channel,
            source: WireInputId::from_domain(source),
        },
        StudioIntent::OverlayOff { channel } => CommandPayload::OverlayOff { channel },
        StudioIntent::ConfigureOverlayTransition {
            channel,
            transition,
            duration_frames,
        } => CommandPayload::ConfigureOverlayTransition {
            channel,
            transition,
            duration_frames,
        },
        StudioIntent::ConfigureOverlayAppearance {
            channel,
            position,
            border,
        } => CommandPayload::ConfigureOverlayAppearance {
            channel,
            position,
            border,
        },
        StudioIntent::QueueOverlay { channel, source } => CommandPayload::QueueOverlay {
            channel,
            source: WireInputId::from_domain(source),
        },
        StudioIntent::TakeNextOverlay { channel } => CommandPayload::TakeNextOverlay { channel },
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

const fn lifecycle_status(lifecycle: LifecycleState) -> StudioConnectionStatus {
    match lifecycle {
        LifecycleState::LaunchingDaemon => StudioConnectionStatus::Launching,
        LifecycleState::Disconnected => StudioConnectionStatus::Disconnected,
        LifecycleState::Connecting => StudioConnectionStatus::Connecting,
        LifecycleState::Synchronizing => StudioConnectionStatus::Synchronizing,
        LifecycleState::Ready => StudioConnectionStatus::Ready,
        LifecycleState::Backoff(_) => StudioConnectionStatus::Backoff,
        LifecycleState::ProtocolMismatch => StudioConnectionStatus::ProtocolMismatch,
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
    let can_control_audio =
        permissions.is_some_and(|values| values.iter().any(|value| value == "control_audio"));
    let mut state = StudioUiState::new(connection_status)
        .with_switcher_permissions(can_select_preview, can_transition)
        .with_audio_permission(can_control_audio);
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
    state.notice = Some(format!(
        "Queued {} command(s) in operator FIFO",
        deferred.len()
    ));
    publisher.publish(state)
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
        io::{BufRead, BufReader, Write},
        net::{TcpListener, TcpStream},
        sync::mpsc::{Receiver, sync_channel},
    };

    use core::num::NonZeroU128;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::{fs, path::PathBuf, process::Command};

    use fm_protocol::{
        CURRENT_PROTOCOL_VERSION, CapabilityReportSummary, EngineIdentity, FadeToBlackPosition,
        FadeToBlackState, HandshakeOutcome, HandshakeResponse, InputAudioStripStatus, InputStatus,
        HeartbeatAcknowledgementMessage, ManualTransitionStatus, OverlayStatus, Role,
        ServerIdentity, SnapshotMessage, SnapshotReason, WireMessage, decode_line, encode_line,
    };
    use fm_types::InputId;

    #[cfg(unix)]
    use crate::SupervisedConfig;
    use crate::{ConnectionConfig, RestartPolicy};

    use super::*;

    const HEARTBEAT_TEST_TIMEOUT: Duration = Duration::from_secs(3);

    struct HeartbeatPeer {
        reader: BufReader<TcpStream>,
        writer: TcpStream,
    }

    impl HeartbeatPeer {
        fn accept(listener: &TcpListener) -> Self {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(HEARTBEAT_TEST_TIMEOUT))
                .unwrap();
            Self {
                reader: BufReader::new(stream.try_clone().unwrap()),
                writer: stream,
            }
        }

        fn receive(&mut self) -> WireMessage {
            let mut line = String::new();
            assert_ne!(self.reader.read_line(&mut line).unwrap(), 0);
            decode_line(&line).unwrap()
        }

        fn handshake_request(&mut self) -> fm_protocol::HandshakeRequest {
            let WireMessage::HandshakeRequest(request) = self.receive() else {
                panic!("expected handshake request");
            };
            request
        }

        fn send(&mut self, message: &WireMessage) {
            self.writer
                .write_all(encode_line(message).unwrap().as_bytes())
                .unwrap();
        }

        fn acknowledge_heartbeats_until_eof(&mut self) {
            loop {
                let mut line = String::new();
                if self.reader.read_line(&mut line).unwrap() == 0 {
                    return;
                }
                let WireMessage::Heartbeat(heartbeat) = decode_line(&line).unwrap() else {
                    panic!("expected heartbeat");
                };
                self.send(&WireMessage::HeartbeatAcknowledgement(
                    HeartbeatAcknowledgementMessage {
                        server: heartbeat.server,
                        heartbeat_sequence: heartbeat.sequence,
                        received_at_ms: heartbeat.sent_at_ms,
                    },
                ));
            }
        }
    }

    fn heartbeat_handshake(outcome: HandshakeOutcome) -> HandshakeResponse {
        HandshakeResponse {
            protocol: CURRENT_PROTOCOL_VERSION,
            granted_role: Role::Operator,
            permissions: Vec::new(),
            capabilities: CapabilityReportSummary {
                digest: String::new(),
                total: 0,
                available: 0,
                degraded: 0,
                unavailable: 0,
            },
            server: ServerIdentity {
                engine_id: "e".into(),
                project_id: "1".into(),
                state_epoch: 1,
                log_id: "l".into(),
            },
            current_revision: 4,
            outcome,
        }
    }

    fn heartbeat_snapshot() -> SnapshotMessage {
        let input = WireInputId::new(NonZeroU128::new(2).unwrap());
        SnapshotMessage {
            engine: EngineIdentity {
                engine_id: "e".into(),
                state_epoch: 1,
                log_id: "l".into(),
            },
            revision: 4,
            show_name: String::new(),
            inputs: vec![InputStatus {
                input,
                name: "i".into(),
            }],
            input_audio_strips: vec![InputAudioStripStatus {
                input,
                gain_millidb: 0,
                balance_basis_points: 0,
                muted: false,
                soloed: false,
                follow_video: false,
                delay_samples: 0,
            }],
            desired_program: input,
            desired_preview: input,
            realized_program: input,
            realized_preview: input,
            desired_manual_transition: ManualTransitionStatus::Inactive,
            realized_manual_transition: ManualTransitionStatus::Inactive,
            desired_fade_to_black: FadeToBlackState {
                target_active: false,
                position: FadeToBlackPosition::LIVE,
            },
            realized_fade_to_black: FadeToBlackState {
                target_active: false,
                position: FadeToBlackPosition::LIVE,
            },
            stingers: Vec::new(),
            desired_overlays: OverlayStatus::empty_channels(),
            realized_overlays: OverlayStatus::empty_channels(),
        }
    }

    fn serve_worker_recovery(listener: TcpListener) {
        let mut first = HeartbeatPeer::accept(&listener);
        first.handshake_request();
        first.send(&WireMessage::HandshakeResponse(heartbeat_handshake(
            HandshakeOutcome::Snapshot {
                reason: SnapshotReason::NoCursor,
            },
        )));
        first.send(&WireMessage::Snapshot(heartbeat_snapshot()));
        first.acknowledge_heartbeats_until_eof();

        let mut resume = HeartbeatPeer::accept(&listener);
        let cursor = resume
            .handshake_request()
            .resume_cursor
            .expect("resume cursor");
        resume.send(&WireMessage::HandshakeResponse(heartbeat_handshake(
            HandshakeOutcome::Resume { cursor },
        )));
        resume.acknowledge_heartbeats_until_eof();

        let mut snapshot = HeartbeatPeer::accept(&listener);
        assert_eq!(snapshot.handshake_request().resume_cursor, None);
        snapshot.send(&WireMessage::HandshakeResponse(heartbeat_handshake(
            HandshakeOutcome::Snapshot {
                reason: SnapshotReason::HistoryUnavailable,
            },
        )));
        snapshot.send(&WireMessage::Snapshot(heartbeat_snapshot()));
        snapshot.acknowledge_heartbeats_until_eof();
    }

    #[cfg(unix)]
    struct TestDirectory(PathBuf);

    #[cfg(unix)]
    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "freemix-studio-idle-exit-{}-{}",
                std::process::id(),
                worker_nonce()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    #[cfg(unix)]
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    fn idle_exit_helper(directory: &TestDirectory, address: std::net::SocketAddr) -> PathBuf {
        let path = directory.path("freemixd-test-helper");
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$2.pid\"\nprintf '%s\\n' \"$$\" >> \"$2.launches\"\nprintf 'FREEMIXD_READY\\tv=1\\taddress={address}\\tproject_id=1\\n'\nIFS= read -r hold\n"
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    fn receive_connection_state(
        updates: &Receiver<StudioUiState>,
        expected: StudioConnectionStatus,
    ) {
        loop {
            let state = updates.recv_timeout(HEARTBEAT_TEST_TIMEOUT).unwrap();
            if state.connection_status == expected {
                return;
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn studio_worker_recovers_after_idle_supervised_daemon_exit() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || serve_worker_recovery(listener));
        let directory = TestDirectory::new();
        let project_bundle = directory.path("show.freemix");
        let daemon_executable = idle_exit_helper(&directory, address);
        let (request_sender, request_receiver) = sync_channel(REQUEST_CAPACITY);
        let (state_sender, state_receiver) = sync_channel(STATE_CAPACITY);
        let worker = thread::spawn(move || {
            run_worker(
                StudioConfig {
                    connection: ConnectionConfig::Supervised(SupervisedConfig {
                        project_bundle,
                        daemon_executable,
                        listen: "127.0.0.1:0".parse().unwrap(),
                    }),
                    client_id: "idle-exit-worker-test".to_owned(),
                    desired_role: Role::Operator,
                    restart_policy: RestartPolicy {
                        maximum_restarts: 1,
                    },
                },
                &request_receiver,
                &StatePublisher {
                    sender: state_sender,
                    repaint_context: egui::Context::default(),
                },
            );
        });

        receive_connection_state(&state_receiver, StudioConnectionStatus::Ready);
        let pid_path = directory.path("show.freemix.pid");
        let pid = fs::read_to_string(pid_path).unwrap();
        assert!(
            Command::new("/bin/kill")
                .args(["-TERM", pid.trim()])
                .status()
                .unwrap()
                .success()
        );
        receive_connection_state(&state_receiver, StudioConnectionStatus::Failed);
        receive_connection_state(&state_receiver, StudioConnectionStatus::Ready);
        assert_eq!(
            fs::read_to_string(directory.path("show.freemix.launches"))
                .unwrap()
                .lines()
                .count(),
            2
        );

        request_sender.send(WorkerRequest::Shutdown).unwrap();
        worker.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn input_audio_strip_intent_maps_to_the_exact_wire_command() {
        let input = InputId::new(NonZeroU128::new(7).unwrap());
        assert_eq!(
            intent_payload(StudioIntent::SetInputAudioStrip {
                input,
                gain_millidb: -6_000,
                balance_basis_points: 2_500,
                muted: true,
                soloed: true,
                follow_video: false,
                delay_samples: 2_400,
            }),
            CommandPayload::SetInputAudioStrip {
                input: WireInputId::from_domain(input),
                gain_millidb: -6_000,
                balance_basis_points: 2_500,
                muted: true,
                soloed: true,
                follow_video: false,
                delay_samples: 2_400,
            }
        );
    }
}
