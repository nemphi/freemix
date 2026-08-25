use std::{
    collections::VecDeque,
    io::Read,
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

use fm_auth::Principal;
use fm_control::{LiveEvent, Subscription};
use fm_persistence::StoredProject;
use fm_protocol::{
    AudioMetersMessage, ClientType, ErrorMessage, HandshakeOutcome as ProtocolHandshakeOutcome,
    HandshakeResponse, HeartbeatAcknowledgementMessage, LineDecoder, ServerIdentity,
    StreamStatusMessage, StructuredError, WireMessage, encode_line,
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
use crate::journal::DurableStore;
use crate::latest_record::LatestRecord;
use crate::web::{WebEvent, WebGateway};

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

#[allow(clippy::struct_excessive_bools)]
struct Outbound {
    write: OutboundWrite,
    accounting: Accounting,
    handshake_response: bool,
    command_result: bool,
    accounted: bool,
    channel_sent: bool,
}

enum OutboundWrite {
    Raw(PendingWrite),
    Web(Vec<u8>),
}

struct WebTransport {
    inbound: Receiver<WireMessage>,
    outbound: SyncSender<Vec<u8>>,
    acknowledgements: Receiver<()>,
    cancel: Arc<AtomicBool>,
}

impl Drop for WebTransport {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

enum Transport {
    Raw(TcpStream),
    Web(WebTransport),
}

impl Transport {
    fn is_web(&self) -> bool {
        matches!(self, Self::Web(_))
    }
}

struct Peer {
    id: u64,
    transport: Transport,
    principal: Principal,
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
    audio_meters: bool,
    latest_meter: LatestRecord,
    stream_status: bool,
    latest_stream_status: LatestRecord,
}

impl Peer {
    fn new(
        id: u64,
        transport: Transport,
        principal: Principal,
        handshake_timeout: Duration,
    ) -> Option<Self> {
        let handshake_deadline = Instant::now().checked_add(handshake_timeout)?;
        Some(Self {
            id,
            transport,
            principal,
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
            audio_meters: false,
            latest_meter: LatestRecord::default(),
            stream_status: false,
            latest_stream_status: LatestRecord::default(),
        })
    }

    fn queue(&mut self, message: &WireMessage, accounting: Accounting) -> Result<(), ()> {
        if self.outbound.len() >= OUTBOUND_CAPACITY {
            return Err(());
        }
        let bytes = encode_line(message).map_err(|_| ())?.into_bytes();
        let byte_len = bytes.len();
        let (write, accounted) = match &self.transport {
            Transport::Raw(_) => (OutboundWrite::Raw(PendingWrite::new(bytes)), true),
            Transport::Web(_) => (OutboundWrite::Web(bytes), false),
        };
        if matches!(accounting, Accounting::Session) && accounted {
            self.session
                .as_mut()
                .ok_or(())?
                .queue_outbound(byte_len, now_millis().map_err(|_| ())?)
                .map_err(|_| ())?;
        }
        self.outbound.push_back(Outbound {
            write,
            accounting,
            handshake_response: matches!(message, WireMessage::HandshakeResponse(_)),
            command_result: matches!(message, WireMessage::CommandResult(_)),
            accounted,
            channel_sent: false,
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

    fn replace_meter(&mut self, bytes: Vec<u8>) {
        if self.audio_meters && self.phase == Phase::Active {
            self.latest_meter.replace(bytes);
        }
    }

    /// Coalesces the latest stream status independently of the meter slot so
    /// neither lossy record can evict the other.
    fn replace_stream_status(&mut self, bytes: Vec<u8>) {
        if self.stream_status && self.phase == Phase::Active {
            self.latest_stream_status.replace(bytes);
        }
    }

    fn replace_outbound_for_shutdown(&mut self) {
        let retained = usize::from(self.outbound.front().is_some_and(|record| {
            record.channel_sent
                || matches!(&record.write, OutboundWrite::Raw(write) if write.started())
        }));
        while self.outbound.len() > retained {
            self.discard_outbound(self.outbound.len() - 1);
        }
    }

    fn preserve_outbound_for_shutdown(&mut self) {
        if self.outbound.len() < OUTBOUND_CAPACITY {
            return;
        }
        let Some(index) = self
            .outbound
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, record)| {
                (!record.command_result
                    && !record.channel_sent
                    && !matches!(
                        &record.write,
                        OutboundWrite::Raw(write) if write.started()
                    ))
                .then_some(index)
            })
        else {
            return;
        };
        self.discard_outbound(index);
    }

    fn discard_outbound(&mut self, index: usize) {
        if let Some(record) = self.outbound.remove(index)
            && record.accounted
            && matches!(record.accounting, Accounting::Session)
        {
            let _ = self
                .session
                .as_mut()
                .and_then(|session| session.discard_outbound(index).ok());
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
pub(super) fn run(
    listener: &TcpListener,
    server: &Server<ControlHandle>,
    control: &SharedControl,
    store: &dyn DurableStore,
    durable: &mut StoredProject,
    principal: &Principal,
    native: Option<&mut NativeDaemon>,
    authority: &ServerIdentity,
    process_shutdown: &ProcessShutdown,
    once: bool,
    web: Option<&WebGateway>,
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
        process_shutdown,
    };
    let mut native = native;

    loop {
        if let Some(reason) = requested_daemon_shutdown(native.as_deref(), Some(process_shutdown)) {
            return Ok(shutdown_for_reason(reason, &mut peers, control));
        }

        if let Some(web) = web {
            loop {
                let Ok(Some(event)) = web.try_event() else {
                    break;
                };
                match event {
                    WebEvent::Connected(connection)
                        if !peers.iter().any(|peer| peer.transport.is_web()) =>
                    {
                        let web_principal = Principal::authenticated(
                            fm_auth::UserId::new("web-token").expect("stable web user is valid"),
                            fm_auth::SessionId::new(format!("web-{next_peer_id}"))
                                .expect("scheduler web session id is valid"),
                            [fm_auth::Role::Admin],
                        );
                        let transport = Transport::Web(WebTransport {
                            inbound: connection.inbound,
                            outbound: connection.outbound,
                            acknowledgements: connection.acknowledgements,
                            cancel: connection.cancel,
                        });
                        let Some(peer) = Peer::new(
                            next_peer_id,
                            transport,
                            web_principal,
                            handshake_timeout.min(Duration::from_millis(500)),
                        ) else {
                            close_all(&mut peers, control);
                            return Err("handshake deadline exceeds Instant range".into());
                        };
                        next_peer_id = next_peer_id.wrapping_add(1);
                        peers.push(peer);
                    }
                    WebEvent::Connected(connection) => {
                        connection.cancel.store(true, Ordering::Release);
                    }
                }
            }
        }

        if once_peer.is_none() && (!once || peers.is_empty()) && raw_peer_count(&peers) < peer_limit
        {
            match listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_ok() && stream.set_nodelay(true).is_ok() {
                        let Some(peer) = Peer::new(
                            next_peer_id,
                            Transport::Raw(stream),
                            principal.clone(),
                            handshake_timeout,
                        ) else {
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
            if !close[index]
                && let Some(session) = peer.session.as_mut()
            {
                close[index] = !super::session_heartbeat_active(session).unwrap_or(false);
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
                    shutdown_peers(&mut peers, control, &ShutdownQueue::Replace);
                    return Err(error);
                }
            }
        }
        if let Some(reason) = requested_daemon_shutdown(native.as_deref(), Some(process_shutdown)) {
            return Ok(shutdown_for_reason(reason, &mut peers, control));
        }
        if let Some(native) = native.as_deref_mut() {
            if let Err(error) = native.tick_if_due(&mut control.borrow_mut(), authority) {
                shutdown_peers(&mut peers, control, &ShutdownQueue::Replace);
                return Err(error);
            }
            if let Some(meters) = native.take_audio_meters() {
                publish_audio_meters(&mut peers, meters);
            }
            if let Some(status) = native.take_stream_status() {
                publish_stream_status(&mut peers, status);
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
            && !peers.iter().any(|peer| peer.id == id)
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
    if let Transport::Web(web) = &mut peer.transport {
        let budget = INBOUND_CAPACITY.saturating_sub(peer.inbound.len());
        for _ in 0..budget {
            match web.inbound.try_recv() {
                Ok(message) => peer.inbound.push_back(message),
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => return true,
            }
        }
        return false;
    }
    let mut chunk = [0_u8; 8 * 1024];
    let Transport::Raw(stream) = &mut peer.transport else {
        unreachable!("web transport returned above")
    };
    match stream.read(&mut chunk) {
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

fn raw_peer_count(peers: &[Peer]) -> usize {
    peers
        .iter()
        .filter(|peer| matches!(peer.transport, Transport::Raw(_)))
        .count()
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
    store: &'a dyn DurableStore,
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
                    &peer.principal,
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
            &peer.principal,
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
        peer.audio_meters = !peer.transport.is_web() && request.client_type == ClientType::Studio;
        peer.stream_status = peer.audio_meters;
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
    if peer.transport.is_web() {
        let Some(record) = peer.outbound.front_mut() else {
            return WriteOutcome::Pending;
        };
        let Transport::Web(web) = &mut peer.transport else {
            unreachable!("web transport checked above")
        };
        if record.channel_sent {
            match web.acknowledgements.try_recv() {
                Ok(()) => return finish_outbound(peer),
                Err(TryRecvError::Empty) => return WriteOutcome::Pending,
                Err(TryRecvError::Disconnected) => return WriteOutcome::Failed,
            }
        }
        let OutboundWrite::Web(bytes) = &record.write else {
            unreachable!("web peers only queue web writes")
        };
        if matches!(record.accounting, Accounting::Session) && !record.accounted {
            let Ok(now) = now_millis() else {
                return WriteOutcome::Failed;
            };
            if peer
                .session
                .as_mut()
                .expect("session accounting has a session")
                .queue_outbound(bytes.len(), now)
                .is_err()
            {
                return WriteOutcome::Failed;
            }
            record.accounted = true;
        }
        match web.outbound.try_send(bytes.clone()) {
            Ok(()) => {
                record.channel_sent = true;
                WriteOutcome::Pending
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => WriteOutcome::Failed,
        }
    } else {
        // Continue any in-flight lossy record before touching the queue.
        if peer.latest_meter.started() {
            return write_meter(peer);
        }
        if peer.latest_stream_status.started() {
            return write_stream_status(peer);
        }
        let Some(record) = peer.outbound.front_mut() else {
            let outcome = write_meter(peer);
            if matches!(outcome, WriteOutcome::Failed) {
                return outcome;
            }
            return write_stream_status(peer);
        };
        let Transport::Raw(stream) = &mut peer.transport else {
            unreachable!("raw transport checked above")
        };
        let complete = match &mut record.write {
            OutboundWrite::Raw(write) => write.write_once(stream),
            OutboundWrite::Web(_) => unreachable!("raw peers only queue raw writes"),
        };
        match complete {
            Ok(true) => finish_outbound(peer),
            Ok(false) => WriteOutcome::Pending,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) =>
            {
                WriteOutcome::Pending
            }
            Err(_) => WriteOutcome::Failed,
        }
    }
}

fn finish_outbound(peer: &mut Peer) -> WriteOutcome {
    let record = peer.outbound.pop_front().expect("written record exists");
    if record.accounted
        && matches!(record.accounting, Accounting::Session)
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

fn write_meter(peer: &mut Peer) -> WriteOutcome {
    write_lossy(peer, LossySlot::Meter)
}

fn write_stream_status(peer: &mut Peer) -> WriteOutcome {
    write_lossy(peer, LossySlot::StreamStatus)
}

/// Which independent lossy side-channel slot to drain.
#[derive(Clone, Copy)]
enum LossySlot {
    Meter,
    StreamStatus,
}

fn write_lossy(peer: &mut Peer, slot: LossySlot) -> WriteOutcome {
    let (record, transport) = match slot {
        LossySlot::Meter => (&mut peer.latest_meter, &mut peer.transport),
        LossySlot::StreamStatus => (&mut peer.latest_stream_status, &mut peer.transport),
    };
    let Transport::Raw(stream) = transport else {
        return WriteOutcome::Failed;
    };
    match record.write_once(stream) {
        Ok(()) => WriteOutcome::Pending,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
            ) =>
        {
            WriteOutcome::Pending
        }
        Err(_) => WriteOutcome::Failed,
    }
}

fn publish_audio_meters(peers: &mut [Peer], meters: AudioMetersMessage) {
    let Ok(bytes) = encode_line(&WireMessage::AudioMeters(meters)).map(String::into_bytes) else {
        return;
    };
    for peer in peers {
        peer.replace_meter(bytes.clone());
    }
}

fn publish_stream_status(peers: &mut [Peer], status: StreamStatusMessage) {
    let Ok(bytes) = encode_line(&WireMessage::StreamStatus(status)).map(String::into_bytes) else {
        return;
    };
    for peer in peers {
        peer.replace_stream_status(bytes.clone());
    }
}

/// Drops lossy records that never reached the wire so shutdown cannot flush
/// stale telemetry after its goodbye; in-flight records keep draining.
fn discard_unstarted_lossy_records(peer: &mut Peer) {
    peer.latest_meter.discard_unstarted();
    peer.latest_stream_status.discard_unstarted();
}

enum ShutdownQueue {
    Replace,
    PreserveCommandResult,
}

fn shutdown_peers(peers: &mut Vec<Peer>, control: &SharedControl, queue: &ShutdownQueue) {
    for index in (0..peers.len()).rev() {
        if peers[index].session.is_none() || !peers[index].handshake_written {
            close_peer(peers.swap_remove(index), control);
        }
    }
    if peers.is_empty() {
        return;
    }
    for peer in peers.iter_mut() {
        discard_unstarted_lossy_records(peer);
        if matches!(queue, ShutdownQueue::Replace) {
            peer.replace_outbound_for_shutdown();
        } else {
            peer.preserve_outbound_for_shutdown();
        }
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
            let outcome = write_peer(&mut peers[index]);
            let complete = if peers[index].transport.is_web() {
                matches!(outcome, WriteOutcome::Failed) || peers[index].outbound.is_empty()
            } else {
                !matches!(outcome, WriteOutcome::Pending) || peers[index].outbound.is_empty()
            };
            if complete {
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
        DaemonShutdownReason::ProcessSignal => {
            let queue = if peers
                .iter()
                .any(|peer| peer.outbound.iter().any(|record| record.command_result))
            {
                ShutdownQueue::PreserveCommandResult
            } else {
                ShutdownQueue::Replace
            };
            shutdown_peers(peers, control, &queue);
        }
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

#[cfg(test)]
mod tests {
    use std::num::NonZeroU128;
    use std::sync::mpsc;

    use fm_auth::{Role, SessionId, UserId};
    use fm_protocol::{
        AudioMeterChannel, StreamRealizedState, StreamStatusMessage, StreamStatusSample,
        WireStreamTargetId,
    };

    use super::*;

    fn identity() -> ServerIdentity {
        ServerIdentity {
            engine_id: "engine".to_owned(),
            project_id: "project".to_owned(),
            state_epoch: 1,
            log_id: "log".to_owned(),
        }
    }

    fn meters_message() -> AudioMetersMessage {
        AudioMetersMessage {
            server: identity(),
            sequence: 7,
            frame: 9,
            start_sample: 100,
            end_sample: 200,
            master: vec![AudioMeterChannel {
                peak_millionths: 500_000,
                rms_millionths: 250_000,
            }],
            inputs: Vec::new(),
        }
    }

    fn status_message() -> StreamStatusMessage {
        StreamStatusMessage {
            server: identity(),
            sequence: 1,
            samples: vec![StreamStatusSample {
                target: WireStreamTargetId::new(NonZeroU128::new(5).unwrap()),
                realized: StreamRealizedState::Live,
                connected: true,
                muxed_bytes: 2_048,
                enqueued_pairs: 12,
                dropped_pairs: 1,
                failure: None,
            }],
        }
    }

    /// An eligible active raw peer plus the far end of its transport.
    fn raw_peer() -> (Peer, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        stream.set_nonblocking(true).unwrap();
        let principal = Principal::authenticated(
            UserId::new("user").unwrap(),
            SessionId::new("session").unwrap(),
            [Role::Admin],
        );
        let peer = Peer::new(0, Transport::Raw(stream), principal, Duration::from_secs(5)).unwrap();
        (peer, client)
    }

    fn active_eligible_peer() -> (Peer, TcpStream) {
        let (mut peer, client) = raw_peer();
        peer.phase = Phase::Active;
        peer.audio_meters = true;
        peer.stream_status = true;
        (peer, client)
    }

    fn read_exact(client: &mut TcpStream, length: usize) -> Vec<u8> {
        let mut buffer = vec![0_u8; length];
        client.read_exact(&mut buffer).unwrap();
        buffer
    }

    #[test]
    fn stream_status_and_meter_records_coexist_and_drain_independently() {
        let (mut peer, mut client) = active_eligible_peer();

        publish_audio_meters(std::slice::from_mut(&mut peer), meters_message());
        publish_stream_status(std::slice::from_mut(&mut peer), status_message());
        // Both slots hold their own encoded record; neither evicted the other.
        assert!(!peer.latest_meter.is_empty());
        assert!(!peer.latest_stream_status.is_empty());

        // A newer stream status replaces only the stream status slot.
        publish_stream_status(std::slice::from_mut(&mut peer), status_message());
        assert!(!peer.latest_meter.is_empty());
        assert!(!peer.latest_stream_status.is_empty());

        let expected_meters = encode_line(&WireMessage::AudioMeters(meters_message()))
            .unwrap()
            .into_bytes();
        let expected_status = encode_line(&WireMessage::StreamStatus(status_message()))
            .unwrap()
            .into_bytes();

        while !peer.latest_meter.is_empty() || !peer.latest_stream_status.is_empty() {
            assert!(!matches!(write_peer(&mut peer), WriteOutcome::Failed));
        }
        assert_eq!(
            read_exact(&mut client, expected_meters.len()),
            expected_meters
        );
        assert_eq!(
            read_exact(&mut client, expected_status.len()),
            expected_status
        );

        // The drained line is a well-formed stream_status record.
        let decoded = LineDecoder::new().push(&expected_status).unwrap().remove(0);
        assert!(matches!(decoded, WireMessage::StreamStatus(_)));
    }

    #[test]
    fn ineligible_peers_never_store_or_write_stream_status() {
        // A raw peer that did not opt in keeps both lossy slots empty.
        let (mut peer, mut client) = raw_peer();
        peer.phase = Phase::Active;
        peer.audio_meters = false;
        peer.stream_status = false;
        publish_stream_status(std::slice::from_mut(&mut peer), status_message());
        assert!(peer.latest_stream_status.is_empty());
        write_peer(&mut peer);
        client.set_nonblocking(true).unwrap();
        let mut probe = [0_u8; 16];
        assert!(matches!(
            client.read(&mut probe),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));

        // Web transports are excluded from the lossy side channel entirely.
        let (inbound_sender, inbound) = mpsc::channel();
        let (outbound_sender, _outbound) = mpsc::sync_channel(8);
        let (_acknowledgement_sender, acknowledgements) = mpsc::channel();
        drop(inbound_sender);
        let web_peer = Peer::new(
            1,
            Transport::Web(WebTransport {
                inbound,
                outbound: outbound_sender,
                acknowledgements,
                cancel: Arc::new(AtomicBool::new(false)),
            }),
            Principal::authenticated(
                UserId::new("user").unwrap(),
                SessionId::new("session").unwrap(),
                [Role::Admin],
            ),
            Duration::from_secs(5),
        )
        .unwrap();
        let mut web_peer = web_peer;
        web_peer.phase = Phase::Active;
        // Eligibility is derived once at handshake, exactly as in `handshake`.
        let client_type = ClientType::Studio;
        web_peer.audio_meters = !web_peer.transport.is_web() && client_type == ClientType::Studio;
        web_peer.stream_status = web_peer.audio_meters;
        assert!(
            !web_peer.stream_status,
            "web peers are not lossy-record eligible"
        );
        publish_stream_status(std::slice::from_mut(&mut web_peer), status_message());
        assert!(web_peer.latest_stream_status.is_empty());
    }

    #[test]
    fn unstarted_lossy_records_are_discarded_for_shutdown_together() {
        let (mut peer, _client) = active_eligible_peer();
        publish_audio_meters(std::slice::from_mut(&mut peer), meters_message());
        publish_stream_status(std::slice::from_mut(&mut peer), status_message());
        assert!(!peer.latest_meter.is_empty());
        assert!(!peer.latest_stream_status.is_empty());

        discard_unstarted_lossy_records(&mut peer);
        assert!(peer.latest_meter.is_empty());
        assert!(peer.latest_stream_status.is_empty());
    }
}
