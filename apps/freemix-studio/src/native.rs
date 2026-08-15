use std::{
    collections::VecDeque,
    fmt,
    sync::{
        Arc, Mutex, PoisonError,
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
    CommandPayload, CommandResult, DurableGap, OverlayBorderPreset, OverlayPositionPreset,
    OverlayTransitionKind, WireInputId, WireMessage,
};
use fm_ui_egui::{
    ExternalStudioAction, InputAudioStripUpdate, StudioConnectionStatus, StudioIntent, StudioShell,
    StudioUiState, external_intent,
};
use fm_ui_model::ClientView;

use crate::osc::{OscAction, OscReceiver};
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
const STATE_NOTIFICATION_CAPACITY: usize = 1;
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

fn coalesce_adjacent_intent(tail: &mut StudioIntent, incoming: StudioIntent) -> bool {
    match (tail, incoming) {
        (
            StudioIntent::SetManualTransitionPosition { position: current },
            StudioIntent::SetManualTransitionPosition { position },
        ) => {
            *current = position;
            true
        }
        (
            StudioIntent::SetInputAudioStrip {
                input: current_input,
                update: current_update,
            },
            StudioIntent::SetInputAudioStrip { input, update },
        ) if *current_input == input => {
            current_update.gain_millidb = update.gain_millidb.or(current_update.gain_millidb);
            current_update.balance_basis_points = update
                .balance_basis_points
                .or(current_update.balance_basis_points);
            current_update.muted = update.muted.or(current_update.muted);
            current_update.soloed = update.soloed.or(current_update.soloed);
            current_update.follow_video = update.follow_video.or(current_update.follow_video);
            current_update.delay_samples = update.delay_samples.or(current_update.delay_samples);
            true
        }
        _ => false,
    }
}

struct PendingIntents {
    intents: VecDeque<StudioIntent>,
}

impl PendingIntents {
    fn new() -> Self {
        Self {
            intents: VecDeque::with_capacity(REQUEST_CAPACITY),
        }
    }

    fn submit(
        &mut self,
        sender: &SyncSender<WorkerRequest>,
        intent: StudioIntent,
    ) -> Result<(), EnqueueError> {
        if self.intents.is_empty() {
            match try_enqueue(sender, WorkerRequest::Intent(intent)) {
                Ok(()) => return Ok(()),
                Err(EnqueueError::Disconnected) => return Err(EnqueueError::Disconnected),
                Err(EnqueueError::Full) => {}
            }
        }
        self.push(intent)
    }

    fn flush(&mut self, sender: &SyncSender<WorkerRequest>) -> Result<(), EnqueueError> {
        while let Some(intent) = self.intents.front().copied() {
            match try_enqueue(sender, WorkerRequest::Intent(intent)) {
                Ok(()) => {
                    self.intents.pop_front();
                }
                Err(EnqueueError::Full) => return Ok(()),
                Err(EnqueueError::Disconnected) => return Err(EnqueueError::Disconnected),
            }
        }
        Ok(())
    }

    fn push(&mut self, intent: StudioIntent) -> Result<(), EnqueueError> {
        if let Some(tail) = self.intents.back_mut()
            && coalesce_adjacent_intent(tail, intent)
        {
            return Ok(());
        }
        if self.intents.len() == REQUEST_CAPACITY {
            return Err(EnqueueError::Full);
        }
        self.intents.push_back(intent);
        Ok(())
    }

    fn clear(&mut self) {
        self.intents.clear();
    }
}

struct StudioApp {
    shell: StudioShell,
    state: StudioUiState,
    requests: Option<SyncSender<WorkerRequest>>,
    pending_intents: PendingIntents,
    updates: Option<StateUpdates>,
    worker: Option<JoinHandle<()>>,
    shutdown_sent: bool,
    osc: Option<OscReceiver>,
    osc_rejected: u64,
    osc_notice: Option<String>,
}

impl StudioApp {
    fn new(
        config: StudioConfig,
        creation_context: &eframe::CreationContext<'_>,
    ) -> Result<Self, std::io::Error> {
        let osc = config.osc_listen.map(OscReceiver::bind).transpose()?;
        let (request_sender, request_receiver) = sync_channel(REQUEST_CAPACITY);
        let repaint_context = creation_context.egui_ctx.clone();
        let (publisher, updates) = state_mailbox(repaint_context);
        let worker = thread::Builder::new()
            .name("freemix-studio-worker".to_owned())
            .spawn(move || {
                run_worker(config, &request_receiver, &publisher);
            })?;
        Ok(Self {
            shell: StudioShell::default(),
            state: StudioUiState::new(StudioConnectionStatus::Launching),
            requests: Some(request_sender),
            pending_intents: PendingIntents::new(),
            updates: Some(updates),
            worker: Some(worker),
            shutdown_sent: false,
            osc,
            osc_rejected: 0,
            osc_notice: None,
        })
    }

    fn report_enqueue_error(&mut self, error: EnqueueError) {
        self.state.error = Some(error.to_string());
    }

    fn enqueue_intent(&mut self, intent: StudioIntent) {
        let result = self
            .requests
            .as_ref()
            .map_or(Err(EnqueueError::Disconnected), |sender| {
                self.pending_intents.submit(sender, intent)
            });
        if let Err(error) = result {
            self.handle_enqueue_error(error);
        }
    }

    fn flush_pending_intents(&mut self) {
        let result = self
            .requests
            .as_ref()
            .map_or(Err(EnqueueError::Disconnected), |sender| {
                self.pending_intents.flush(sender)
            });
        if let Err(error) = result {
            self.handle_enqueue_error(error);
        }
    }

    fn handle_enqueue_error(&mut self, error: EnqueueError) {
        if error == EnqueueError::Disconnected {
            self.pending_intents.clear();
            self.requests.take();
        }
        self.report_enqueue_error(error);
    }

    fn send_shutdown(&mut self) {
        if !self.shutdown_sent {
            if let Some(requests) = &self.requests {
                let _ = try_enqueue(requests, WorkerRequest::Shutdown);
            }
            self.shutdown_sent = true;
        }
    }

    fn drain_osc(&mut self) {
        for _ in 0..8 {
            let Some(action) = self.osc.as_ref().and_then(|osc| osc.try_recv().ok()) else {
                break;
            };
            let action = match action {
                OscAction::SelectPreview(number) => ExternalStudioAction::SelectPreview(number),
                OscAction::Cut => ExternalStudioAction::Cut,
                OscAction::Fade => ExternalStudioAction::Fade,
                OscAction::FadeToBlack { active } => ExternalStudioAction::FadeToBlack { active },
            };
            let intent = external_intent(
                &self.state,
                action,
                self.shell.transition_duration_frames(),
                self.shell.fade_to_black_duration_frames(),
            );
            if let Some(intent) = intent {
                self.enqueue_intent(intent);
            } else {
                self.osc_rejected = self.osc_rejected.saturating_add(1);
            }
        }
        if let Some(osc) = &self.osc {
            let counters = osc.counters();
            let rejected = counters.rejected.saturating_add(self.osc_rejected);
            if counters.failed > 0 {
                self.osc_notice = Some("OSC receive failed".to_owned());
            } else if counters.malformed > 0 || rejected > 0 || counters.overflow > 0 {
                self.osc_notice = Some(format!(
                    "OSC malformed={} rejected={} overflow={}",
                    counters.malformed, rejected, counters.overflow
                ));
            }
            self.state.external_notice = self.osc_notice.clone();
        }
    }
}

impl eframe::App for StudioApp {
    fn logic(&mut self, _context: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(updates) = &self.updates {
            if let Some(state) = updates.try_recv() {
                self.state = state;
            }
        }
        self.drain_osc();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.flush_pending_intents();
        for intent in self.shell.draw(ui, &self.state) {
            self.enqueue_intent(intent);
        }
    }

    fn on_exit(&mut self) {
        self.send_shutdown();
        if let Some(osc) = self.osc.take() {
            let _ = osc.shutdown();
        }
    }
}

impl Drop for StudioApp {
    fn drop(&mut self) {
        self.send_shutdown();
        self.requests.take();
        if let Some(osc) = self.osc.take() {
            let _ = osc.shutdown();
        }
        self.updates.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct StatePublisher {
    latest: Arc<Mutex<Option<StudioUiState>>>,
    sender: SyncSender<()>,
    repaint_context: egui::Context,
}

struct StateUpdates {
    latest: Arc<Mutex<Option<StudioUiState>>>,
    receiver: Receiver<()>,
}

fn state_mailbox(repaint_context: egui::Context) -> (StatePublisher, StateUpdates) {
    let latest = Arc::new(Mutex::new(None));
    let (sender, receiver) = sync_channel(STATE_NOTIFICATION_CAPACITY);
    (
        StatePublisher {
            latest: Arc::clone(&latest),
            sender,
            repaint_context,
        },
        StateUpdates { latest, receiver },
    )
}

impl StatePublisher {
    fn publish(&self, state: StudioUiState) -> bool {
        *self.latest.lock().unwrap_or_else(PoisonError::into_inner) = Some(state);
        match self.sender.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => {
                self.repaint_context.request_repaint();
                true
            }
            Err(TrySendError::Disconnected(())) => false,
        }
    }
}

impl StateUpdates {
    fn try_recv(&self) -> Option<StudioUiState> {
        self.receiver.try_recv().ok()?;
        self.latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
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
        if let Some(tail) = self.deferred_intents.back_mut()
            && coalesce_adjacent_intent(tail, intent)
        {
            return publish_deferred_runtime(
                runtime,
                publisher,
                &self.deferred_intents,
                self.error(),
            );
        }
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
            if let Some(tail) = deferred_intents.back_mut()
                && coalesce_adjacent_intent(tail, intent)
            {
                return false;
            }
            if deferred_intents.len() == DEFERRED_INTENT_CAPACITY {
                *deferred_rejections = deferred_rejections.saturating_add(1);
            } else {
                deferred_intents.push_back(intent);
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
    let view = runtime.session().client().model().view();
    let payload = intent_payload(intent, view.as_ref())?;
    let expected_revision = view
        .as_ref()
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

fn resolve_input_audio_strip(
    view: Option<&ClientView>,
    input: fm_types::InputId,
    update: InputAudioStripUpdate,
) -> Result<CommandPayload, String> {
    let view =
        view.ok_or_else(|| "Cannot edit audio before project state is synchronized".to_owned())?;
    let strip = view
        .input_audio_strips
        .iter()
        .find(|strip| strip.input == input)
        .ok_or_else(|| "Cannot edit audio: input is no longer available".to_owned())?;
    Ok(CommandPayload::SetInputAudioStrip {
        input: WireInputId::from_domain(input),
        gain_millidb: update.gain_millidb.unwrap_or(strip.gain_millidb),
        balance_basis_points: update
            .balance_basis_points
            .unwrap_or(strip.balance_basis_points),
        muted: update.muted.unwrap_or(strip.muted),
        soloed: update.soloed.unwrap_or(strip.soloed),
        follow_video: update.follow_video.unwrap_or(strip.follow_video),
        delay_samples: update.delay_samples.unwrap_or(strip.delay_samples),
    })
}

fn desired_overlay<'view>(
    view: Option<&'view ClientView>,
    channel: fm_protocol::WireOverlayChannelId,
) -> Result<&'view fm_ui_model::OverlayStatus, String> {
    let view =
        view.ok_or_else(|| "Cannot edit overlay before project state is synchronized".to_owned())?;
    let overlay = view
        .desired_overlays
        .iter()
        .find(|overlay| overlay.channel == channel.number())
        .ok_or_else(|| "Cannot edit overlay: channel is no longer available".to_owned())?;
    Ok(overlay)
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

fn intent_payload(
    intent: StudioIntent,
    view: Option<&ClientView>,
) -> Result<CommandPayload, String> {
    let payload = match intent {
        StudioIntent::SetInputAudioStrip { input, update } => {
            return resolve_input_audio_strip(view, input, update);
        }
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
        StudioIntent::ToggleOverlayTransition {
            channel,
            duration_frames,
        } => {
            let overlay = desired_overlay(view, channel)?;
            CommandPayload::ConfigureOverlayTransition {
                channel,
                transition: match overlay.transition {
                    OverlayTransitionKind::Cut => OverlayTransitionKind::Fade,
                    OverlayTransitionKind::Fade => OverlayTransitionKind::Cut,
                },
                duration_frames,
            }
        }
        StudioIntent::CycleOverlayPosition { channel } => {
            let overlay = desired_overlay(view, channel)?;
            CommandPayload::ConfigureOverlayAppearance {
                channel,
                position: match overlay.position {
                    OverlayPositionPreset::FullFrame => OverlayPositionPreset::TopLeft,
                    OverlayPositionPreset::TopLeft => OverlayPositionPreset::TopRight,
                    OverlayPositionPreset::TopRight => OverlayPositionPreset::BottomLeft,
                    OverlayPositionPreset::BottomLeft => OverlayPositionPreset::BottomRight,
                    OverlayPositionPreset::BottomRight => OverlayPositionPreset::FullFrame,
                },
                border: overlay.border,
            }
        }
        StudioIntent::CycleOverlayBorder { channel } => {
            let overlay = desired_overlay(view, channel)?;
            CommandPayload::ConfigureOverlayAppearance {
                channel,
                position: overlay.position,
                border: match overlay.border {
                    OverlayBorderPreset::None => OverlayBorderPreset::ThinWhite,
                    OverlayBorderPreset::ThinWhite => OverlayBorderPreset::ThickWhite,
                    OverlayBorderPreset::ThickWhite => OverlayBorderPreset::None,
                },
            }
        }
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
        StudioIntent::ToggleFadeToBlack { duration_frames } => {
            let view = view.ok_or_else(|| "Project state is not synchronized".to_owned())?;
            CommandPayload::FadeToBlack {
                active: !view.switcher.desired_fade_to_black.target_active,
                duration_frames,
            }
        }
        StudioIntent::StartManualTransition { kind } => {
            CommandPayload::StartManualTransition { kind }
        }
        StudioIntent::SetManualTransitionPosition { position } => {
            CommandPayload::SetManualTransitionPosition { position }
        }
        StudioIntent::CommitManualTransition => CommandPayload::CommitManualTransition,
        StudioIntent::CancelManualTransition => CommandPayload::CancelManualTransition,
    };
    Ok(payload)
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
        net::{TcpListener, TcpStream, UdpSocket},
        sync::mpsc::sync_channel,
        thread::sleep,
    };

    use core::num::NonZeroU128;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::{fs, path::PathBuf, process::Command};

    use fm_protocol::{
        CURRENT_PROTOCOL_VERSION, CapabilityReportSummary, EngineIdentity, FadeToBlackPosition,
        FadeToBlackState, HandshakeOutcome, HandshakeResponse, HeartbeatAcknowledgementMessage,
        InputAudioStripStatus, InputStatus, ManualTransitionPosition, ManualTransitionStatus,
        OverlayBorderPreset, OverlayPositionPreset, OverlayStatus, Role, ServerIdentity,
        SnapshotMessage, SnapshotReason, WireMessage, decode_line, encode_line,
    };
    use fm_types::ProjectId;
    use fm_ui_model::{ClientModel, DurableChange, DurableProjectEvent, ProjectSnapshot};

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

    fn receive_connection_state(updates: &StateUpdates, expected: StudioConnectionStatus) {
        loop {
            updates
                .receiver
                .recv_timeout(HEARTBEAT_TEST_TIMEOUT)
                .unwrap();
            let state = updates
                .latest
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
                .expect("notification has a current state");
            if state.connection_status == expected {
                return;
            }
        }
    }

    #[test]
    fn state_mailbox_keeps_latest_state_when_notification_is_full() {
        let (publisher, updates) = state_mailbox(egui::Context::default());
        assert!(publisher.publish(StudioUiState::new(StudioConnectionStatus::Launching)));
        assert!(publisher.publish(StudioUiState::new(StudioConnectionStatus::Ready)));
        assert_eq!(
            updates.try_recv().unwrap().connection_status,
            StudioConnectionStatus::Ready
        );
        drop(updates);
        assert!(!publisher.publish(StudioUiState::new(StudioConnectionStatus::Failed)));
    }

    fn manual_position(basis_points: u16) -> StudioIntent {
        StudioIntent::SetManualTransitionPosition {
            position: ManualTransitionPosition::new(basis_points).unwrap(),
        }
    }

    #[test]
    fn pending_intents_coalesce_manual_positions_after_outer_queue_full() {
        let (sender, receiver) = sync_channel(1);
        sender
            .send(WorkerRequest::Intent(StudioIntent::Cut))
            .unwrap();
        let mut pending = PendingIntents::new();
        for position in [1_000, 2_000, 3_000] {
            assert_eq!(pending.submit(&sender, manual_position(position)), Ok(()));
        }

        assert_eq!(
            receiver.try_recv().unwrap(),
            WorkerRequest::Intent(StudioIntent::Cut)
        );
        assert_eq!(pending.flush(&sender), Ok(()));
        assert_eq!(
            receiver.try_recv().unwrap(),
            WorkerRequest::Intent(manual_position(3_000))
        );
    }

    #[test]
    fn pending_intents_coalesce_adjacent_audio_updates_without_crossing_boundaries() {
        let input = WireInputId::new(NonZeroU128::new(2).unwrap()).to_domain();
        let other_input = WireInputId::new(NonZeroU128::new(3).unwrap()).to_domain();
        let mut pending = PendingIntents::new();
        let initial = StudioIntent::SetInputAudioStrip {
            input,
            update: InputAudioStripUpdate {
                muted: Some(false),
                soloed: Some(false),
                follow_video: Some(false),
                ..InputAudioStripUpdate::default()
            },
        };
        assert_eq!(pending.push(initial), Ok(()));
        for value in 1..=REQUEST_CAPACITY {
            assert_eq!(
                pending.push(StudioIntent::SetInputAudioStrip {
                    input,
                    update: InputAudioStripUpdate {
                        gain_millidb: Some(value as i32),
                        balance_basis_points: Some(-(value as i32)),
                        delay_samples: Some(0),
                        ..InputAudioStripUpdate::default()
                    },
                }),
                Ok(())
            );
        }
        let coalesced = StudioIntent::SetInputAudioStrip {
            input,
            update: InputAudioStripUpdate {
                gain_millidb: Some(REQUEST_CAPACITY as i32),
                balance_basis_points: Some(-(REQUEST_CAPACITY as i32)),
                muted: Some(false),
                soloed: Some(false),
                follow_video: Some(false),
                delay_samples: Some(0),
            },
        };
        assert_eq!(pending.intents, VecDeque::from([coalesced]));

        let other = StudioIntent::SetInputAudioStrip {
            input: other_input,
            update: InputAudioStripUpdate {
                muted: Some(true),
                ..InputAudioStripUpdate::default()
            },
        };
        assert_eq!(pending.push(other), Ok(()));
        assert_eq!(pending.push(StudioIntent::Cut), Ok(()));
        assert_eq!(pending.push(other), Ok(()));
        assert_eq!(
            pending.intents,
            VecDeque::from([coalesced, other, StudioIntent::Cut, other])
        );
    }

    #[test]
    fn pending_intents_keep_manual_position_before_commit_after_retry() {
        let (sender, receiver) = sync_channel(1);
        sender
            .send(WorkerRequest::Intent(StudioIntent::Cut))
            .unwrap();
        let mut pending = PendingIntents::new();
        assert_eq!(pending.submit(&sender, manual_position(3_000)), Ok(()));
        assert_eq!(
            pending.submit(&sender, StudioIntent::CommitManualTransition),
            Ok(())
        );

        assert_eq!(
            receiver.try_recv().unwrap(),
            WorkerRequest::Intent(StudioIntent::Cut)
        );
        assert_eq!(pending.flush(&sender), Ok(()));
        assert_eq!(
            receiver.try_recv().unwrap(),
            WorkerRequest::Intent(manual_position(3_000))
        );
        assert_eq!(pending.flush(&sender), Ok(()));
        assert_eq!(
            receiver.try_recv().unwrap(),
            WorkerRequest::Intent(StudioIntent::CommitManualTransition)
        );
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
        let (publisher, updates) = state_mailbox(egui::Context::default());
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
                    osc_listen: None,
                },
                &request_receiver,
                &publisher,
            );
        });

        receive_connection_state(&updates, StudioConnectionStatus::Ready);
        let pid_path = directory.path("show.freemix.pid");
        let pid = fs::read_to_string(pid_path).unwrap();
        assert!(
            Command::new("/bin/kill")
                .args(["-TERM", pid.trim()])
                .status()
                .unwrap()
                .success()
        );
        receive_connection_state(&updates, StudioConnectionStatus::Ready);
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
    fn queued_audio_edits_merge_with_the_latest_confirmed_strip() {
        let project = ProjectId::new(NonZeroU128::new(7).unwrap());
        let snapshot = heartbeat_snapshot();
        let input = snapshot.inputs[0].input.to_domain();
        let mut model = ClientModel::new(project);
        model
            .install_snapshot(ProjectSnapshot::from_protocol(project, snapshot))
            .unwrap();
        let confirmed = model.view().unwrap();
        assert_eq!(
            resolve_input_audio_strip(
                Some(&confirmed),
                input,
                InputAudioStripUpdate {
                    muted: Some(true),
                    ..InputAudioStripUpdate::default()
                },
            ),
            Ok(CommandPayload::SetInputAudioStrip {
                input: WireInputId::from_domain(input),
                gain_millidb: 0,
                balance_basis_points: 0,
                muted: true,
                soloed: false,
                follow_video: false,
                delay_samples: 0,
            })
        );
        let mut input_audio_strips = confirmed.input_audio_strips.clone();
        input_audio_strips[0].muted = true;
        let mut cursor = confirmed.cursor.clone();
        cursor.revision = cursor.revision.checked_next().unwrap();
        model
            .apply_event(DurableProjectEvent {
                cursor,
                change: DurableChange::DesiredSwitcher {
                    selection: confirmed.switcher.desired,
                    manual_transition: confirmed.switcher.desired_manual_transition,
                    fade_to_black: confirmed.switcher.desired_fade_to_black,
                    overlays: confirmed.desired_overlays.clone(),
                    input_audio_strips,
                },
            })
            .unwrap();
        let confirmed = model.view().unwrap();
        assert_eq!(
            resolve_input_audio_strip(
                Some(&confirmed),
                input,
                InputAudioStripUpdate {
                    soloed: Some(true),
                    ..InputAudioStripUpdate::default()
                },
            ),
            Ok(CommandPayload::SetInputAudioStrip {
                input: WireInputId::from_domain(input),
                gain_millidb: 0,
                balance_basis_points: 0,
                muted: true,
                soloed: true,
                follow_video: false,
                delay_samples: 0,
            })
        );
    }

    fn apply_overlay_change(
        model: &mut ClientModel,
        view: &ClientView,
        change: impl FnOnce(&mut fm_ui_model::OverlayStatus),
    ) {
        let mut overlays = view.desired_overlays.clone();
        change(&mut overlays[0]);
        let mut cursor = view.cursor.clone();
        cursor.revision = cursor.revision.checked_next().unwrap();
        model
            .apply_event(DurableProjectEvent {
                cursor,
                change: DurableChange::DesiredSwitcher {
                    selection: view.switcher.desired,
                    manual_transition: view.switcher.desired_manual_transition,
                    fade_to_black: view.switcher.desired_fade_to_black,
                    overlays,
                    input_audio_strips: view.input_audio_strips.clone(),
                },
            })
            .unwrap();
    }

    fn assert_overlay_payload(view: &ClientView, intent: StudioIntent, expected: CommandPayload) {
        assert_eq!(intent_payload(intent, Some(view)), Ok(expected));
    }

    #[test]
    fn osc_ingress_obeys_limits_gates_and_worker_order() {
        let address = UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let receiver = OscReceiver::bind(address).unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let queued = [
            "/freemix/switcher/preview/1",
            "/freemix/switcher/cut",
            "/freemix/switcher/fade",
            "/freemix/switcher/preview/1",
            "/freemix/switcher/cut",
            "/freemix/switcher/fade",
            "/freemix/switcher/preview/1",
            "/freemix/switcher/cut",
        ]
        .iter()
        .flat_map(|address| [*address, *address])
        .collect::<Vec<_>>();
        for address in queued.iter().chain(std::iter::once(&queued[0])) {
            sender
                .send_to(&osc_message(address), receiver.local_addr())
                .unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while receiver.counters().overflow == 0 && Instant::now() < deadline {
            sleep(Duration::from_millis(5));
        }
        assert_eq!(receiver.counters().overflow, 1);

        let project = ProjectId::new(NonZeroU128::new(7).unwrap());
        let mut model = ClientModel::new(project);
        model
            .install_snapshot(ProjectSnapshot::from_protocol(
                project,
                heartbeat_snapshot(),
            ))
            .unwrap();
        let view = model.view().unwrap();
        let ready = StudioUiState::new(StudioConnectionStatus::Ready)
            .with_view(view.clone())
            .with_switcher_permissions(true, true);
        assert_eq!(
            external_intent(&ready, ExternalStudioAction::SelectPreview(1), 42, 84),
            Some(StudioIntent::SelectPreview(view.inputs[0]))
        );
        assert_eq!(
            external_intent(
                &ready,
                ExternalStudioAction::FadeToBlack { active: true },
                42,
                84
            ),
            Some(StudioIntent::FadeToBlack {
                active: true,
                duration_frames: 84
            })
        );
        let mut black = ready.clone();
        black
            .view
            .as_mut()
            .unwrap()
            .switcher
            .desired_fade_to_black
            .target_active = true;
        assert_eq!(
            external_intent(
                &black,
                ExternalStudioAction::FadeToBlack { active: false },
                42,
                84
            ),
            Some(StudioIntent::FadeToBlack {
                active: false,
                duration_frames: 84
            })
        );
        assert_eq!(
            external_intent(&ready, ExternalStudioAction::Fade, 42, 84),
            Some(StudioIntent::Fade {
                duration_frames: 42
            })
        );
        let mut manual = ready.clone();
        manual
            .view
            .as_mut()
            .unwrap()
            .switcher
            .desired_manual_transition =
            fm_ui_model::ManualTransitionStatus::Active(fm_ui_model::ActiveManualTransition {
                kind: fm_protocol::ManualTransitionKind::Fade,
                from: view.switcher.desired.preview,
                to: view.switcher.desired.program,
                interval_start: ManualTransitionPosition::START,
                position: ManualTransitionPosition::START,
            });
        assert!(external_intent(&manual, ExternalStudioAction::Cut, 42, 84).is_none());
        manual
            .view
            .as_mut()
            .unwrap()
            .switcher
            .desired_manual_transition = fm_ui_model::ManualTransitionStatus::Inactive;
        manual
            .view
            .as_mut()
            .unwrap()
            .switcher
            .realized_manual_transition =
            fm_ui_model::ManualTransitionStatus::Active(fm_ui_model::ActiveManualTransition {
                kind: fm_protocol::ManualTransitionKind::Fade,
                from: view.switcher.realized.preview,
                to: view.switcher.realized.program,
                interval_start: ManualTransitionPosition::START,
                position: ManualTransitionPosition::START,
            });
        assert!(external_intent(&manual, ExternalStudioAction::Cut, 42, 84).is_none());
        let denied = StudioUiState::new(StudioConnectionStatus::Synchronizing)
            .with_view(view.clone())
            .with_switcher_permissions(true, true);
        assert!(external_intent(&denied, ExternalStudioAction::Cut, 42, 84).is_none());

        let (tx, rx) = sync_channel(REQUEST_CAPACITY);
        let mut app = StudioApp {
            shell: StudioShell::default(),
            state: ready,
            requests: Some(tx),
            pending_intents: PendingIntents::new(),
            updates: None,
            worker: None,
            shutdown_sent: false,
            osc: Some(receiver),
            osc_rejected: 0,
            osc_notice: None,
        };
        app.shell.set_transition_duration_frames(42);
        app.shell.set_fade_to_black_duration_frames(84);
        app.drain_osc();
        assert_eq!(app.pending_intents.intents.len(), 0);
        app.drain_osc();
        let order = rx
            .try_iter()
            .map(|request| match request {
                WorkerRequest::Intent(intent) => intent,
                WorkerRequest::Shutdown => panic!("unexpected shutdown"),
            })
            .collect::<Vec<_>>();
        let expected = [
            StudioIntent::SelectPreview(view.inputs[0]),
            StudioIntent::SelectPreview(view.inputs[0]),
            StudioIntent::Cut,
            StudioIntent::Cut,
            StudioIntent::Fade {
                duration_frames: 42,
            },
            StudioIntent::Fade {
                duration_frames: 42,
            },
            StudioIntent::SelectPreview(view.inputs[0]),
            StudioIntent::SelectPreview(view.inputs[0]),
            StudioIntent::Cut,
            StudioIntent::Cut,
            StudioIntent::Fade {
                duration_frames: 42,
            },
            StudioIntent::Fade {
                duration_frames: 42,
            },
            StudioIntent::SelectPreview(view.inputs[0]),
            StudioIntent::SelectPreview(view.inputs[0]),
            StudioIntent::Cut,
            StudioIntent::Cut,
        ];
        assert_eq!(order, expected);
    }

    fn osc_message(address: &str) -> Vec<u8> {
        let mut message = Vec::new();
        for value in [address, ","] {
            message.extend_from_slice(value.as_bytes());
            message.push(0);
            while !message.len().is_multiple_of(4) {
                message.push(0);
            }
        }
        message
    }

    #[test]
    fn fade_to_black_shortcut_resolves_each_press_from_latest_confirmed_desired_target() {
        let project = ProjectId::new(NonZeroU128::new(7).unwrap());
        let mut model = ClientModel::new(project);
        model
            .install_snapshot(ProjectSnapshot::from_protocol(
                project,
                heartbeat_snapshot(),
            ))
            .unwrap();
        let confirmed = model.view().unwrap();
        let intent = StudioIntent::ToggleFadeToBlack {
            duration_frames: 84,
        };
        let assert_target = |view: &ClientView, active| {
            assert_eq!(
                intent_payload(intent, Some(view)),
                Ok(CommandPayload::FadeToBlack {
                    active,
                    duration_frames: 84,
                })
            );
        };
        assert_target(&confirmed, true);

        let mut cursor = confirmed.cursor.clone();
        cursor.revision = cursor.revision.checked_next().unwrap();
        model
            .apply_event(DurableProjectEvent {
                cursor,
                change: DurableChange::DesiredSwitcher {
                    selection: confirmed.switcher.desired,
                    manual_transition: confirmed.switcher.desired_manual_transition,
                    fade_to_black: FadeToBlackState {
                        target_active: true,
                        position: FadeToBlackPosition::LIVE,
                    },
                    overlays: confirmed.desired_overlays.clone(),
                    input_audio_strips: confirmed.input_audio_strips.clone(),
                },
            })
            .unwrap();
        let confirmed = model.view().unwrap();
        assert_target(&confirmed, false);
    }

    #[test]
    fn queued_overlay_actions_resolve_from_the_latest_confirmed_channel() {
        let project = ProjectId::new(NonZeroU128::new(7).unwrap());
        let snapshot = heartbeat_snapshot();
        let channel = snapshot.desired_overlays[0].channel;
        let mut model = ClientModel::new(project);
        model
            .install_snapshot(ProjectSnapshot::from_protocol(project, snapshot))
            .unwrap();
        let confirmed = model.view().unwrap();
        assert_overlay_payload(
            &confirmed,
            StudioIntent::ToggleOverlayTransition {
                channel,
                duration_frames: 42,
            },
            CommandPayload::ConfigureOverlayTransition {
                channel,
                transition: OverlayTransitionKind::Fade,
                duration_frames: 42,
            },
        );
        apply_overlay_change(&mut model, &confirmed, |overlay| {
            overlay.transition = OverlayTransitionKind::Fade;
        });
        let confirmed = model.view().unwrap();
        assert_overlay_payload(
            &confirmed,
            StudioIntent::ToggleOverlayTransition {
                channel,
                duration_frames: 42,
            },
            CommandPayload::ConfigureOverlayTransition {
                channel,
                transition: OverlayTransitionKind::Cut,
                duration_frames: 42,
            },
        );
        apply_overlay_change(&mut model, &confirmed, |overlay| {
            overlay.transition = OverlayTransitionKind::Cut;
        });
        let confirmed = model.view().unwrap();
        assert_overlay_payload(
            &confirmed,
            StudioIntent::CycleOverlayPosition { channel },
            CommandPayload::ConfigureOverlayAppearance {
                channel,
                position: OverlayPositionPreset::TopLeft,
                border: OverlayBorderPreset::None,
            },
        );
        apply_overlay_change(&mut model, &confirmed, |overlay| {
            overlay.position = OverlayPositionPreset::TopLeft;
        });
        let confirmed = model.view().unwrap();
        assert_overlay_payload(
            &confirmed,
            StudioIntent::CycleOverlayPosition { channel },
            CommandPayload::ConfigureOverlayAppearance {
                channel,
                position: OverlayPositionPreset::TopRight,
                border: OverlayBorderPreset::None,
            },
        );
        apply_overlay_change(&mut model, &confirmed, |overlay| {
            overlay.position = OverlayPositionPreset::TopRight;
        });
        let confirmed = model.view().unwrap();
        assert_overlay_payload(
            &confirmed,
            StudioIntent::CycleOverlayBorder { channel },
            CommandPayload::ConfigureOverlayAppearance {
                channel,
                position: OverlayPositionPreset::TopRight,
                border: OverlayBorderPreset::ThinWhite,
            },
        );
        apply_overlay_change(&mut model, &confirmed, |overlay| {
            overlay.border = OverlayBorderPreset::ThinWhite;
        });
        let confirmed = model.view().unwrap();
        assert_overlay_payload(
            &confirmed,
            StudioIntent::CycleOverlayBorder { channel },
            CommandPayload::ConfigureOverlayAppearance {
                channel,
                position: OverlayPositionPreset::TopRight,
                border: OverlayBorderPreset::ThickWhite,
            },
        );
    }
}
