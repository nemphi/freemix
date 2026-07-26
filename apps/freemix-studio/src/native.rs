use std::{
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use eframe::egui;
use fm_client::SessionEvent;
use fm_protocol::{CommandPayload, CommandResult, WireInputId};
use fm_ui_egui::{StudioConnectionStatus, StudioIntent, StudioShell, StudioUiState};

use crate::{LifecycleState, StudioConfig, StudioRuntime};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const REQUEST_CAPACITY: usize = 16;
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
    requests: SyncSender<WorkerRequest>,
    updates: Receiver<StudioUiState>,
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
            requests: request_sender,
            updates: state_receiver,
            worker: Some(worker),
            shutdown_sent: false,
        })
    }

    fn report_enqueue_error(&mut self, error: EnqueueError) {
        self.state.error = Some(error.to_string());
    }

    fn send_shutdown(&mut self) {
        if !self.shutdown_sent {
            let _ = try_enqueue(&self.requests, WorkerRequest::Shutdown);
            self.shutdown_sent = true;
        }
    }
}

impl eframe::App for StudioApp {
    fn logic(&mut self, _context: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(state) = self.updates.try_recv() {
            self.state = state;
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        for intent in self.shell.draw(ui, &self.state) {
            if let Err(error) = try_enqueue(&self.requests, WorkerRequest::Intent(intent)) {
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
        // Dropping the handle detaches: the render thread never waits on socket work.
        self.worker.take();
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

fn run_worker(
    config: StudioConfig,
    requests: &Receiver<WorkerRequest>,
    publisher: &StatePublisher,
) {
    if !publisher.publish(StudioUiState::new(StudioConnectionStatus::Launching)) {
        return;
    }
    let mut runtime = match StudioRuntime::new(config) {
        Ok(runtime) => runtime,
        Err(error) => {
            let mut state = StudioUiState::new(StudioConnectionStatus::Failed);
            state.error = Some(format!("Studio startup failed: {error}"));
            publisher.publish(state);
            return;
        }
    };
    if !publish_runtime(&mut runtime, publisher, None) {
        return;
    }
    let mut visible_error = match runtime.connect(CONNECT_TIMEOUT) {
        Ok(_) => None,
        Err(error) => Some(format!("Connection failed: {error}")),
    };
    if !publish_runtime(&mut runtime, publisher, visible_error.clone()) {
        return;
    }

    let started = Instant::now();
    let mut next_heartbeat = Instant::now() + HEARTBEAT_INTERVAL;
    let mut keys = IdempotencyKeys::new(worker_nonce());

    // Idle receive is intentionally forbidden: the current single-client daemon
    // has no unsolicited broadcasts and TCP receive has no cancellation timeout.
    loop {
        let now = Instant::now();
        let timeout = next_heartbeat.saturating_duration_since(now);
        match requests.recv_timeout(timeout) {
            Ok(WorkerRequest::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(WorkerRequest::Intent(intent)) => {
                visible_error = process_intent(&mut runtime, intent, &mut keys, publisher).err();
                if visible_error.is_some()
                    && !publish_runtime(&mut runtime, publisher, visible_error.clone())
                {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if matches!(runtime.lifecycle(), Ok(LifecycleState::Ready)) {
                    let elapsed_ms =
                        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    if let Err(error) = runtime.send_heartbeat(elapsed_ms) {
                        visible_error = Some(format!("Heartbeat failed: {error}"));
                        if !publish_runtime(&mut runtime, publisher, visible_error.clone()) {
                            break;
                        }
                    }
                }
                next_heartbeat = Instant::now() + HEARTBEAT_INTERVAL;
            }
        }
    }
}

fn process_intent(
    runtime: &mut StudioRuntime,
    intent: StudioIntent,
    keys: &mut IdempotencyKeys,
    publisher: &StatePublisher,
) -> Result<(), String> {
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
    if !publish_runtime(runtime, publisher, None) {
        return Err("Studio UI disconnected".to_owned());
    }
    runtime
        .flush()
        .map_err(|error| format!("Could not send command: {error}"))?;

    consume_command_sequence(runtime, &command.id, publisher)
}

fn consume_command_sequence(
    runtime: &mut StudioRuntime,
    command_id: &str,
    publisher: &StatePublisher,
) -> Result<(), String> {
    let mut consumed = 0;
    count_record(&mut consumed)?;
    let result = match runtime.receive().map_err(|error| transport_error(&error))? {
        SessionEvent::CommandResult { result, .. } => result,
        other => return Err(unexpected("command result", &other)),
    };
    if result_id(&result) != command_id {
        return Err(format!(
            "Unexpected command result ID {:?}; expected {command_id:?}",
            result_id(&result)
        ));
    }
    let accepted_revision = match &result {
        CommandResult::Accepted { revision, .. } => *revision,
        CommandResult::Rejected { code, message, .. } => {
            publish_runtime(
                runtime,
                publisher,
                Some(format!("Command rejected ({code}): {message}")),
            );
            return Ok(());
        }
    };
    if !publish_runtime(runtime, publisher, None) {
        return Err("Studio UI disconnected".to_owned());
    }

    count_record(&mut consumed)?;
    match runtime.receive().map_err(|error| transport_error(&error))? {
        SessionEvent::Event { event, .. } if event.cursor.revision == accepted_revision => {}
        SessionEvent::Event { event, .. } => {
            return Err(format!(
                "Unexpected durable event revision {}; expected {accepted_revision}",
                event.cursor.revision
            ));
        }
        other => return Err(unexpected("durable event", &other)),
    }
    if !publish_runtime(runtime, publisher, None) {
        return Err("Studio UI disconnected".to_owned());
    }

    count_record(&mut consumed)?;
    match runtime.receive().map_err(|error| transport_error(&error))? {
        SessionEvent::RuntimeEvent { event, .. } if event.revision == accepted_revision => {}
        SessionEvent::RuntimeEvent { event, .. } => {
            return Err(format!(
                "Unexpected runtime event revision {}; expected {accepted_revision}",
                event.revision
            ));
        }
        other => return Err(unexpected("runtime event", &other)),
    }
    if !publish_runtime(runtime, publisher, None) {
        return Err("Studio UI disconnected".to_owned());
    }
    Ok(())
}

fn count_record(consumed: &mut usize) -> Result<(), String> {
    *consumed = consumed.saturating_add(1);
    if *consumed > MAX_COMMAND_RECORDS {
        Err("Command response exceeded the record limit".to_owned())
    } else {
        Ok(())
    }
}

fn transport_error(error: &crate::StudioError) -> String {
    format!("Command response failed: {error}")
}

fn unexpected(expected: &str, event: &SessionEvent) -> String {
    match event {
        SessionEvent::ServerError(error) => format!(
            "Server error while awaiting {expected}: {}: {}",
            error.error.code, error.error.message
        ),
        SessionEvent::DurableGap { .. } => {
            format!("Durable event gap while awaiting {expected}")
        }
        SessionEvent::Disconnected { cause, .. } => {
            format!("Disconnected while awaiting {expected}: {cause:?}")
        }
        _ => format!("Unexpected response while awaiting {expected}: {event:?}"),
    }
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
    state.view = client.model().view();
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

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        num::NonZeroU128,
    };

    use fm_client::ReconnectBackoff;
    use fm_protocol::{
        CapabilityReportSummary, EngineIdentity, EventCursor, EventMessage, EventPayload,
        HandshakeOutcome, HandshakeResponse, LineDecoder, ProtocolVersion, Role,
        RuntimeEventMessage, RuntimeLifecycleEvent, ServerIdentity, SnapshotMessage,
        SnapshotReason, WireMessage, encode_line,
    };
    use fm_types::{InputId, ProjectId};

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

    fn serve_worker_select_preview(listener: &TcpListener) {
        let mut peer = FakePeer::accept(listener);
        assert!(matches!(peer.receive(), WireMessage::HandshakeRequest(_)));
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
            current_revision: 4,
            outcome: HandshakeOutcome::Snapshot {
                reason: SnapshotReason::NoCursor,
            },
        }));
        peer.send(&WireMessage::Snapshot(SnapshotMessage {
            engine: test_engine(),
            revision: 4,
            show_name: "Worker test".to_owned(),
            inputs: vec![wire_input(1), wire_input(2), wire_input(3)],
            desired_program: wire_input(1),
            desired_preview: wire_input(2),
            realized_program: wire_input(1),
            realized_preview: wire_input(2),
        }));

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

    #[test]
    fn worker_select_preview_flow_is_exact_and_shutdown_is_idle() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || serve_worker_select_preview(&listener));

        let (request_sender, request_receiver) = sync_channel(REQUEST_CAPACITY);
        let (state_sender, state_receiver) = sync_channel(STATE_CAPACITY);
        let worker = thread::spawn(move || {
            let publisher = StatePublisher {
                sender: state_sender,
                repaint_context: egui::Context::default(),
            };
            run_worker(
                StudioConfig {
                    connection: ConnectionConfig::Existing(ExistingConfig {
                        address,
                        expected_project_id: test_project_id(),
                    }),
                    client_id: "worker-test".to_owned(),
                    desired_role: Role::Operator,
                    restart_policy: RestartPolicy::default(),
                },
                &request_receiver,
                &publisher,
            );
        });

        loop {
            let state = state_receiver.recv_timeout(Duration::from_secs(3)).unwrap();
            if state.connection_status == StudioConnectionStatus::Ready {
                assert!(state.can_select_preview);
                assert!(state.can_transition);
                break;
            }
        }
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
}
