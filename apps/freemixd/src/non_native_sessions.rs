use std::{
    collections::VecDeque,
    io::Read,
    net::{TcpListener, TcpStream},
    sync::mpsc::TryRecvError,
    thread,
    time::{Duration, Instant},
};

use fm_auth::Principal;
use fm_control::{LiveEvent, Subscription};
use fm_persistence::{ProjectStore, StoredProject};
use fm_protocol::{
    ErrorMessage, HandshakeOutcome as ProtocolHandshakeOutcome, HandshakeResponse,
    HeartbeatAcknowledgementMessage, LineDecoder, ServerIdentity, StructuredError, WireMessage,
    encode_line,
};
use fm_server::{Server, Session, SyncPayload};

use super::{
    AppResult, CLIENT_READ_POLL_INTERVAL, CLIENT_WRITE_TIMEOUT, CommandDelivery, ControlHandle,
    DaemonShutdownReason, NativeDaemon, PendingWrite, ProcessShutdown, SharedControl,
    current_handshake, diagnostics_response, error_message, execute_session_command,
    handshake_code, handshake_response, is_client_session_termination, now_millis,
    reconciled_handshake_outcome, record_heartbeat, rejected_handshake_response,
    requested_daemon_shutdown, server_identity, shutdown_message, structured_session_error,
};

const MAX_PEERS: usize = 2;
const INBOUND_CAPACITY: usize = 8;
const OUTBOUND_CAPACITY: usize = 32;
const LIVE_EVENTS_PER_PASS: usize = 8;

#[derive(Clone, Copy, Eq, PartialEq)]
enum Phase {
    AwaitHandshake,
    Syncing,
    Active,
    Closing,
}

#[derive(Clone, Copy)]
enum Accounting {
    Raw,
    Session,
}

struct Outbound {
    write: PendingWrite,
    accounting: Accounting,
    handshake_response: bool,
}

struct Peer {
    id: u64,
    stream: TcpStream,
    decoder: LineDecoder,
    inbound: VecDeque<WireMessage>,
    handshake_deadline: Instant,
    phase: Phase,
    handshake_written: bool,
    session: Option<Session>,
    identity: Option<ServerIdentity>,
    subscription: Option<Subscription>,
    initial_sync: VecDeque<WireMessage>,
    outbound: VecDeque<Outbound>,
}

impl Peer {
    fn new(id: u64, stream: TcpStream, handshake_timeout: Duration) -> Option<Self> {
        let handshake_deadline = Instant::now().checked_add(handshake_timeout)?;
        Some(Self {
            id,
            stream,
            decoder: LineDecoder::new(),
            inbound: VecDeque::new(),
            handshake_deadline,
            phase: Phase::AwaitHandshake,
            handshake_written: false,
            session: None,
            identity: None,
            subscription: None,
            initial_sync: VecDeque::new(),
            outbound: VecDeque::new(),
        })
    }

    fn queue(&mut self, message: &WireMessage, accounting: Accounting) -> Result<(), ()> {
        if self.outbound.len() >= OUTBOUND_CAPACITY {
            return Err(());
        }
        let bytes = encode_line(message).map_err(|_| ())?.into_bytes();
        if matches!(accounting, Accounting::Session) {
            self.session
                .as_mut()
                .ok_or(())?
                .queue_outbound(bytes.len(), now_millis().map_err(|_| ())?)
                .map_err(|_| ())?;
        }
        self.outbound.push_back(Outbound {
            write: PendingWrite::new(bytes),
            accounting,
            handshake_response: matches!(message, WireMessage::HandshakeResponse(_)),
        });
        Ok(())
    }

    fn feed_initial_sync(&mut self) -> Result<(), ()> {
        while self.outbound.len() < OUTBOUND_CAPACITY {
            let Some(message) = self.initial_sync.pop_front() else {
                break;
            };
            if self.queue(&message, Accounting::Session).is_err() {
                return Err(());
            }
        }
        Ok(())
    }

    fn close_after(&mut self, message: &WireMessage) -> Result<(), ()> {
        self.queue(message, Accounting::Raw)?;
        self.phase = Phase::Closing;
        Ok(())
    }

    fn clear_outbound_accounting(&mut self) {
        while let Some(record) = self.outbound.pop_front() {
            if matches!(record.accounting, Accounting::Session) {
                let _ = self
                    .session
                    .as_mut()
                    .and_then(|session| session.outbound_delivered().ok());
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    listener: TcpListener,
    server: &Server<ControlHandle>,
    control: &SharedControl,
    store: &ProjectStore,
    durable: &mut StoredProject,
    principal: &Principal,
    native: Option<&mut NativeDaemon>,
    authority: &ServerIdentity,
    process_shutdown: &ProcessShutdown,
    once: bool,
) -> AppResult<DaemonShutdownReason> {
    let handshake_timeout =
        Duration::from_millis(server.config().session_limits.heartbeat_timeout_ms);
    let peer_limit = if native.is_some() { 1 } else { MAX_PEERS };
    let mut peers = Vec::with_capacity(peer_limit);
    let mut next_peer_id = 0_u64;
    let mut once_peer = None;
    let runtime = Runtime {
        server,
        control,
        store,
        principal,
        process_shutdown,
    };
    let mut native = native;

    loop {
        if let Some(reason) = requested_daemon_shutdown(native.as_deref(), Some(process_shutdown)) {
            return Ok(shutdown_for_reason(reason, &mut peers, control));
        }

        if once_peer.is_none() && (!once || peers.is_empty()) && peers.len() < peer_limit {
            match listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_ok() && stream.set_nodelay(true).is_ok() {
                        let Some(peer) = Peer::new(next_peer_id, stream, handshake_timeout) else {
                            close_all(&mut peers, control);
                            return Err("handshake deadline exceeds Instant range".into());
                        };
                        next_peer_id = next_peer_id.wrapping_add(1);
                        peers.push(peer);
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                    ) => {}
                Err(error) => {
                    close_all(&mut peers, control);
                    return Err(error.into());
                }
            }
        }

        let mut close = vec![false; peers.len()];
        for (index, peer) in peers.iter_mut().enumerate() {
            close[index] = read_peer(peer);
            if !close[index] && peer.phase == Phase::AwaitHandshake {
                close[index] = Instant::now() >= peer.handshake_deadline;
            }
            if !close[index] && peer.session.is_some() {
                close[index] =
                    !super::session_heartbeat_active(peer.session.as_mut().expect("checked above"))
                        .unwrap_or(false);
            }
            if !close[index]
                && peer
                    .subscription
                    .as_ref()
                    .and_then(Subscription::failure)
                    .is_some()
            {
                close[index] = true;
            }
        }

        let mut live_budget = vec![LIVE_EVENTS_PER_PASS; peers.len()];
        for index in 0..peers.len() {
            if close[index] {
                continue;
            }
            let message = match peers[index].phase {
                Phase::AwaitHandshake | Phase::Active => peers[index].inbound.pop_front(),
                Phase::Syncing | Phase::Closing => None,
            };
            let Some(message) = message else {
                continue;
            };
            match runtime.dispatch(&mut peers[index], message, durable, native.as_deref_mut()) {
                Ok(()) => {}
                Err(DispatchError::Peer) => close[index] = true,
                Err(DispatchError::Daemon(error)) => {
                    shutdown_peers(&mut peers, control);
                    return Err(error);
                }
            }
        }
        if let Some(native) = native.as_deref_mut() {
            if let Err(error) = native.tick_if_due(&mut control.borrow_mut(), authority) {
                shutdown_peers(&mut peers, control);
                return Err(error);
            }
        }
        if let Some(reason) = requested_daemon_shutdown(native.as_deref(), Some(process_shutdown)) {
            return Ok(shutdown_for_reason(reason, &mut peers, control));
        }
        drain_live(&mut peers, &mut close, &mut live_budget);

        for (index, peer) in peers.iter_mut().enumerate() {
            if close[index] {
                continue;
            }
            if peer.phase == Phase::Syncing && peer.feed_initial_sync().is_err() {
                close[index] = true;
                continue;
            }
            match write_peer(peer) {
                WriteOutcome::Pending => {}
                WriteOutcome::HandshakeResponseWritten => {
                    if once && once_peer.is_none() {
                        once_peer = Some(peer.id);
                    }
                }
                WriteOutcome::Failed => close[index] = true,
            }
            if peer.phase == Phase::Syncing
                && peer.initial_sync.is_empty()
                && peer.outbound.is_empty()
            {
                peer.phase = Phase::Active;
            }
            if peer.phase == Phase::Closing && peer.outbound.is_empty() {
                close[index] = true;
            }
        }

        for index in (0..peers.len()).rev() {
            if close[index] {
                close_peer(peers.swap_remove(index), control);
            }
        }

        if let Some(id) = once_peer
            && peers
                .iter()
                .find(|peer| peer.id == id)
                .is_none_or(|peer| peer.phase == Phase::Active)
        {
            close_all(&mut peers, control);
            return Ok(DaemonShutdownReason::Once);
        }
        if let Some(reason) = requested_daemon_shutdown(native.as_deref(), Some(process_shutdown)) {
            return Ok(shutdown_for_reason(reason, &mut peers, control));
        }
        thread::sleep(CLIENT_READ_POLL_INTERVAL);
    }
}

fn read_peer(peer: &mut Peer) -> bool {
    let mut chunk = [0_u8; 8 * 1024];
    match peer.stream.read(&mut chunk) {
        Ok(0) => {
            let decoder = std::mem::replace(&mut peer.decoder, LineDecoder::new());
            let _ = decoder.finish();
            true
        }
        Ok(read) => match peer.decoder.push(&chunk[..read]) {
            Ok(messages)
                if messages.len() <= INBOUND_CAPACITY.saturating_sub(peer.inbound.len()) =>
            {
                peer.inbound.extend(messages);
                false
            }
            Ok(_) | Err(_) => true,
        },
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
            ) =>
        {
            false
        }
        Err(_) => true,
    }
}

enum DispatchError {
    Peer,
    Daemon(Box<dyn std::error::Error>),
}

impl From<()> for DispatchError {
    fn from((): ()) -> Self {
        Self::Peer
    }
}

struct Runtime<'a> {
    server: &'a Server<ControlHandle>,
    control: &'a SharedControl,
    store: &'a ProjectStore,
    principal: &'a Principal,
    process_shutdown: &'a ProcessShutdown,
}

impl Runtime<'_> {
    fn dispatch(
        &self,
        peer: &mut Peer,
        message: WireMessage,
        durable: &mut StoredProject,
        native: Option<&mut NativeDaemon>,
    ) -> Result<(), DispatchError> {
        if peer.phase == Phase::AwaitHandshake {
            return self.handshake(peer, message, durable);
        }
        match message {
            WireMessage::Command(command) => {
                let delivery = execute_session_command(
                    peer.session.as_mut().expect("active peers have sessions"),
                    self.control,
                    self.store,
                    durable,
                    self.principal,
                    peer.identity
                        .as_ref()
                        .expect("active peers have identities"),
                    &command,
                    native,
                    Some(self.process_shutdown),
                )
                .map_err(|error| {
                    if is_client_session_termination(error.as_ref()) {
                        DispatchError::Peer
                    } else {
                        DispatchError::Daemon(error)
                    }
                })?;
                let CommandDelivery { result, .. } = delivery;
                peer.queue(&WireMessage::CommandResult(result), Accounting::Session)?;
                Ok(())
            }
            WireMessage::Heartbeat(heartbeat) => {
                let identity = peer
                    .identity
                    .as_ref()
                    .expect("active peers have identities");
                let message = match record_heartbeat(
                    peer.session.as_mut().expect("active peers have sessions"),
                    self.control,
                    identity,
                    &heartbeat,
                ) {
                    Ok(received_at_ms) => {
                        WireMessage::HeartbeatAcknowledgement(HeartbeatAcknowledgementMessage {
                            server: identity.clone(),
                            heartbeat_sequence: heartbeat.sequence,
                            received_at_ms,
                        })
                    }
                    Err(message) => error_message("invalid_heartbeat", &message),
                };
                peer.queue(&message, Accounting::Session)?;
                Ok(())
            }
            WireMessage::DiagnosticsRequest(request) => {
                let encoded_bytes = encode_line(&WireMessage::DiagnosticsRequest(request.clone()))
                    .map_err(|_| ())?
                    .len();
                let session = peer.session.as_mut().expect("active peers have sessions");
                let response = match session.admit_diagnostics(
                    &request,
                    encoded_bytes,
                    now_millis().map_err(|_| ())?,
                ) {
                    Ok(()) => WireMessage::DiagnosticsResponse(
                        diagnostics_response(self.control, request).map_err(|_| ())?,
                    ),
                    Err(error) => WireMessage::Error(ErrorMessage {
                        request_id: Some(request.request_id),
                        current_revision: Some(
                            self.control.borrow().diagnostics().current_revision,
                        ),
                        error: structured_session_error(&error),
                    }),
                };
                peer.queue(&response, Accounting::Session)?;
                Ok(())
            }
            _ => {
                peer.queue(
                    &error_message(
                        "unexpected_message",
                        "only command and heartbeat messages are accepted after the handshake",
                    ),
                    Accounting::Session,
                )?;
                Ok(())
            }
        }
    }

    fn handshake(
        &self,
        peer: &mut Peer,
        message: WireMessage,
        durable: &StoredProject,
    ) -> Result<(), DispatchError> {
        let WireMessage::HandshakeRequest(request) = message else {
            peer.close_after(&error_message(
                "handshake_required",
                "first message must be handshake_request",
            ))?;
            return Ok(());
        };
        let project_id = durable.project().id();
        let (hello, outcome) = current_handshake(&request, self.control, project_id);
        let handshake = match self.server.handshake(
            &hello,
            self.principal,
            now_millis().map_err(DispatchError::Daemon)?,
        ) {
            Ok(handshake) => handshake,
            Err(error) => {
                let response = rejected_handshake_response(
                    self.server,
                    self.control,
                    project_id,
                    &hello,
                    handshake_code(&error),
                    &error.to_string(),
                );
                peer.close_after(&WireMessage::HandshakeResponse(response))?;
                return Ok(());
            }
        };

        let subscription = match self.control.borrow_mut().subscribe() {
            Ok(subscription) => subscription,
            Err(error) => {
                let mut response = rejected_handshake_response(
                    self.server,
                    self.control,
                    project_id,
                    &hello,
                    "unavailable",
                    &error.to_string(),
                );
                make_retryable(&mut response);
                peer.close_after(&WireMessage::HandshakeResponse(response))?;
                return Ok(());
            }
        };

        let identity = server_identity(&handshake.server_hello, project_id);
        let response = handshake_response(
            &handshake.server_hello,
            identity.clone(),
            reconciled_handshake_outcome(outcome, &handshake.sync),
        );
        match handshake.sync {
            SyncPayload::Snapshot(snapshot) => peer
                .initial_sync
                .push_back(WireMessage::Snapshot(*snapshot)),
            SyncPayload::Resume(events) => peer
                .initial_sync
                .extend(events.into_iter().map(WireMessage::Event)),
        }
        let sync_limit = self
            .control
            .borrow()
            .diagnostics()
            .limits
            .retained_events
            .max(1);
        if peer.initial_sync.len() > sync_limit {
            self.control.borrow_mut().unsubscribe(subscription.id());
            return Err(DispatchError::Peer);
        }
        peer.session = Some(handshake.session);
        peer.identity = Some(identity);
        peer.subscription = Some(subscription);
        if peer
            .queue(
                &WireMessage::HandshakeResponse(response),
                Accounting::Session,
            )
            .is_err()
        {
            let subscription = peer.subscription.take().expect("set above");
            self.control.borrow_mut().unsubscribe(subscription.id());
            return Err(DispatchError::Peer);
        }
        peer.phase = Phase::Syncing;
        Ok(())
    }
}

fn make_retryable(response: &mut HandshakeResponse) {
    if let ProtocolHandshakeOutcome::Rejected {
        error: StructuredError { retryable, .. },
    } = &mut response.outcome
    {
        *retryable = true;
    }
}

fn drain_live(peers: &mut [Peer], close: &mut [bool], budgets: &mut [usize]) {
    for (index, peer) in peers.iter_mut().enumerate() {
        if close[index] || peer.phase != Phase::Active {
            continue;
        }
        while budgets[index] > 0 && peer.outbound.len() < OUTBOUND_CAPACITY {
            let event = match peer
                .subscription
                .as_ref()
                .expect("active peers have subscriptions")
                .try_recv()
            {
                Ok(event) => event,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    close[index] = true;
                    break;
                }
            };
            let message = match event {
                LiveEvent::Durable(event) => WireMessage::Event(event),
                LiveEvent::Runtime(event) => WireMessage::RuntimeEvent(event),
            };
            if peer.queue(&message, Accounting::Session).is_err() {
                close[index] = true;
                break;
            }
            budgets[index] -= 1;
        }
    }
}

enum WriteOutcome {
    Pending,
    HandshakeResponseWritten,
    Failed,
}

fn write_peer(peer: &mut Peer) -> WriteOutcome {
    let Some(record) = peer.outbound.front_mut() else {
        return WriteOutcome::Pending;
    };
    let complete = match record.write.write_once(&mut peer.stream) {
        Ok(complete) => complete,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
            ) =>
        {
            return WriteOutcome::Pending;
        }
        Err(_) => return WriteOutcome::Failed,
    };
    if !complete {
        return WriteOutcome::Pending;
    }
    let record = peer.outbound.pop_front().expect("written record exists");
    if matches!(record.accounting, Accounting::Session)
        && peer
            .session
            .as_mut()
            .expect("accounted records have sessions")
            .outbound_delivered()
            .is_err()
    {
        return WriteOutcome::Failed;
    }
    if record.handshake_response {
        peer.handshake_written = true;
        WriteOutcome::HandshakeResponseWritten
    } else {
        WriteOutcome::Pending
    }
}

fn shutdown_peers(peers: &mut Vec<Peer>, control: &SharedControl) {
    for index in (0..peers.len()).rev() {
        if peers[index].session.is_none() || !peers[index].handshake_written {
            close_peer(peers.swap_remove(index), control);
        }
    }
    if peers.is_empty() {
        return;
    }
    for peer in peers.iter_mut() {
        peer.clear_outbound_accounting();
        peer.initial_sync.clear();
        peer.inbound.clear();
        let notice = shutdown_message();
        if peer.queue(&notice, Accounting::Session).is_err() {
            let _ = peer.queue(&notice, Accounting::Raw);
        }
        peer.phase = Phase::Closing;
    }

    let deadline = Instant::now() + CLIENT_WRITE_TIMEOUT;
    while !peers.is_empty() && Instant::now() < deadline {
        for index in (0..peers.len()).rev() {
            if !matches!(write_peer(&mut peers[index]), WriteOutcome::Pending)
                || peers[index].outbound.is_empty()
            {
                close_peer(peers.swap_remove(index), control);
            }
        }
        if !peers.is_empty() {
            thread::sleep(CLIENT_READ_POLL_INTERVAL);
        }
    }
    close_all(peers, control);
}

fn close_all(peers: &mut Vec<Peer>, control: &SharedControl) {
    while let Some(peer) = peers.pop() {
        close_peer(peer, control);
    }
}

fn shutdown_for_reason(
    reason: DaemonShutdownReason,
    peers: &mut Vec<Peer>,
    control: &SharedControl,
) -> DaemonShutdownReason {
    match reason {
        DaemonShutdownReason::ProcessSignal => shutdown_peers(peers, control),
        DaemonShutdownReason::ProgramSurface => close_all(peers, control),
        DaemonShutdownReason::Once => unreachable!("once is handled below"),
    }
    reason
}

fn close_peer(mut peer: Peer, control: &SharedControl) {
    if let Some(subscription) = peer.subscription.take() {
        control.borrow_mut().unsubscribe(subscription.id());
    }
}
