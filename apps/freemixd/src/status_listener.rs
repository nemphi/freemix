//! Bounded loopback status listener for operators and process supervisors.
//!
//! The listener owns a fixed set of threads and never touches engine, control,
//! or render state. It answers probes from [`DaemonStatus`], an atomic snapshot
//! the control loop publishes as it runs, so a probe can never block or slow
//! the scheduler.
//!
//! Every bound is fixed at compile time: at most [`CONNECTION_WORKERS`] request
//! threads plus one queued socket each, [`MAX_REQUEST_BYTES`] of request head,
//! [`MAX_REQUEST_HEADERS`] headers, and a response no larger than the capped
//! support bundle. Each socket phase carries its own deadline, which is what
//! bounds how long [`StatusListener::shutdown`] can take to join.

use std::{
    env,
    fmt::Write as _,
    io::{ErrorKind, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use fm_observability::{
    Category, ComponentHealth, EventField, EventLog, HealthCheck, HealthRegistry, Metric,
    MetricStore, Severity, SupportBundle,
};
use fm_server::{HealthState, ReadinessState, ServiceStatus};

use super::AppResult;

const TOKEN_ENVIRONMENT: &str = "FREEMIXD_STATUS_TOKEN";
const TOKEN_MIN_BYTES: usize = 32;
const TOKEN_MAX_BYTES: usize = 256;

const HEALTH_PATH: &str = "/healthz";
const READY_PATH: &str = "/readyz";
const BUNDLE_PATH: &str = "/v1/support-bundle";

/// Request threads. One queued socket each caps the listener at four sockets.
const CONNECTION_WORKERS: usize = 2;
const WORKER_QUEUE_CAPACITY: usize = 1;
const MAX_REQUEST_BYTES: usize = 4 * 1024;
const MAX_REQUEST_HEADERS: usize = 32;
const MAX_DRAIN_BYTES: usize = 16 * 1024;
const BUNDLE_MAX_BYTES: usize = 32 * 1024;
const EVENT_CAPACITY: usize = 8;
const METRIC_CAPACITY: usize = 8;

const ACCEPT_POLL: Duration = Duration::from_millis(25);
const SOCKET_READ_TIMEOUT: Duration = Duration::from_millis(250);
const SOCKET_WRITE_TIMEOUT: Duration = Duration::from_millis(250);
const REQUEST_DEADLINE: Duration = Duration::from_millis(500);
const RESPONSE_DEADLINE: Duration = Duration::from_millis(500);
const DRAIN_DEADLINE: Duration = Duration::from_millis(100);

/// How long the control loop may go silent before liveness fails.
const CONTROL_STALL_LIMIT_MILLIS: u64 = 10_000;

const READINESS_STARTING: u8 = 0;
const READINESS_READY: u8 = 1;
const READINESS_DRAINING: u8 = 2;
const READINESS_UNHEALTHY: u8 = 3;
const HEALTH_HEALTHY: u8 = 0;
const HEALTH_UNHEALTHY: u8 = 1;
const UNSET_MILLIS: u64 = u64::MAX;

/// Lock-free daemon state shared with the status listener threads.
pub(super) struct DaemonStatus {
    started: Instant,
    beat_millis: AtomicU64,
    readiness: AtomicU8,
    health: AtomicU8,
    ready_millis: AtomicU64,
    draining_millis: AtomicU64,
    served: AtomicU64,
    rejected: AtomicU64,
    in_flight: AtomicUsize,
}

impl DaemonStatus {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            started: Instant::now(),
            beat_millis: AtomicU64::new(0),
            readiness: AtomicU8::new(READINESS_STARTING),
            health: AtomicU8::new(HEALTH_HEALTHY),
            ready_millis: AtomicU64::new(UNSET_MILLIS),
            draining_millis: AtomicU64::new(UNSET_MILLIS),
            served: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            in_flight: AtomicUsize::new(0),
        })
    }

    fn uptime_millis(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Records that the control loop completed another pass. One clock read and
    /// one relaxed-ordering store; the loop calls this several times per pass.
    pub(super) fn beat(&self) {
        self.beat_millis
            .store(self.uptime_millis(), Ordering::Release);
    }

    /// Publishes the authoritative `fm-server` readiness and health states.
    pub(super) fn publish(&self, status: ServiceStatus) {
        let readiness = match status.readiness() {
            ReadinessState::Starting => READINESS_STARTING,
            ReadinessState::Ready => READINESS_READY,
            ReadinessState::Draining => READINESS_DRAINING,
            ReadinessState::Unhealthy => READINESS_UNHEALTHY,
        };
        self.readiness.store(readiness, Ordering::Release);
        self.health.store(
            match status.health() {
                HealthState::Healthy => HEALTH_HEALTHY,
                HealthState::Unhealthy => HEALTH_UNHEALTHY,
            },
            Ordering::Release,
        );
        if readiness == READINESS_READY {
            self.stamp(&self.ready_millis);
        }
    }

    /// Moves a ready daemon to draining on the first cooperative stop request.
    pub(super) fn begin_draining(&self) {
        if self
            .readiness
            .compare_exchange(
                READINESS_READY,
                READINESS_DRAINING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.stamp(&self.draining_millis);
        }
    }

    fn stamp(&self, slot: &AtomicU64) {
        let _ = slot.compare_exchange(
            UNSET_MILLIS,
            self.uptime_millis(),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn snapshot(&self) -> StatusSnapshot {
        let uptime_millis = self.uptime_millis();
        let beat_millis = self.beat_millis.load(Ordering::Acquire);
        StatusSnapshot {
            uptime_millis,
            beat_millis,
            stalled: uptime_millis.saturating_sub(beat_millis) > CONTROL_STALL_LIMIT_MILLIS,
            readiness: match self.readiness.load(Ordering::Acquire) {
                READINESS_READY => ReadinessState::Ready,
                READINESS_DRAINING => ReadinessState::Draining,
                READINESS_UNHEALTHY => ReadinessState::Unhealthy,
                _ => ReadinessState::Starting,
            },
            health: if self.health.load(Ordering::Acquire) == HEALTH_UNHEALTHY {
                HealthState::Unhealthy
            } else {
                HealthState::Healthy
            },
            ready_millis: unset_to_none(self.ready_millis.load(Ordering::Acquire)),
            draining_millis: unset_to_none(self.draining_millis.load(Ordering::Acquire)),
            served: self.served.load(Ordering::Acquire),
            rejected: self.rejected.load(Ordering::Acquire),
            in_flight: self.in_flight.load(Ordering::Acquire),
        }
    }
}

const fn unset_to_none(millis: u64) -> Option<u64> {
    if millis == UNSET_MILLIS {
        None
    } else {
        Some(millis)
    }
}

struct StatusSnapshot {
    uptime_millis: u64,
    beat_millis: u64,
    stalled: bool,
    readiness: ReadinessState,
    health: HealthState,
    ready_millis: Option<u64>,
    draining_millis: Option<u64>,
    served: u64,
    rejected: u64,
    in_flight: usize,
}

impl StatusSnapshot {
    fn live(&self) -> bool {
        !self.stalled && self.health == HealthState::Healthy
    }

    fn ready(&self) -> bool {
        self.live() && self.readiness == ReadinessState::Ready
    }
}

/// The bearer token guarding the support bundle, mirroring the web gateway.
struct StatusToken {
    bytes: [u8; TOKEN_MAX_BYTES],
    length: usize,
}

impl StatusToken {
    fn from_environment() -> AppResult<Self> {
        let value = env::var(TOKEN_ENVIRONMENT).map_err(|_| {
            format!("{TOKEN_ENVIRONMENT} is required when --status-listen is enabled")
        })?;
        let bytes = value.as_bytes();
        if !(TOKEN_MIN_BYTES..=TOKEN_MAX_BYTES).contains(&bytes.len())
            || !bytes.iter().all(u8::is_ascii_graphic)
        {
            return Err(format!(
                "{TOKEN_ENVIRONMENT} must be {TOKEN_MIN_BYTES}..={TOKEN_MAX_BYTES} ASCII graphic bytes"
            )
            .into());
        }
        let mut token = [0; TOKEN_MAX_BYTES];
        token[..bytes.len()].copy_from_slice(bytes);
        Ok(Self {
            bytes: token,
            length: bytes.len(),
        })
    }

    fn matches(&self, authorization: Option<&[u8]>) -> bool {
        let presented = authorization
            .and_then(|value| value.strip_prefix(b"Bearer "))
            .unwrap_or_default();
        let mut candidate = [0; TOKEN_MAX_BYTES];
        let copied = presented.len().min(TOKEN_MAX_BYTES);
        candidate[..copied].copy_from_slice(&presented[..copied]);
        let mut difference = self.length ^ presented.len();
        for (expected, actual) in self.bytes.iter().zip(candidate) {
            difference |= usize::from(*expected ^ actual);
        }
        difference == 0
    }
}

/// A bounded HTTP/1.1 listener serving liveness, readiness, and support state.
pub(super) struct StatusListener {
    address: SocketAddr,
    cancel: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl StatusListener {
    /// Binds the loopback listener and starts its fixed thread set.
    ///
    /// # Errors
    ///
    /// Fails when the address is not loopback, the bearer token environment is
    /// missing or malformed, the socket cannot be bound, or a thread cannot be
    /// spawned. Threads already started are stopped before returning.
    pub(super) fn bind(address: SocketAddr, status: &Arc<DaemonStatus>) -> AppResult<Self> {
        if !address.ip().is_loopback() {
            return Err("--status-listen must use a loopback address".into());
        }
        let token = Arc::new(StatusToken::from_environment()?);
        let listener = TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        let bound = listener.local_addr()?;
        status.beat();

        let cancel = Arc::new(AtomicBool::new(false));
        let mut senders = Vec::with_capacity(CONNECTION_WORKERS);
        let mut threads = Vec::with_capacity(CONNECTION_WORKERS + 1);
        for index in 0..CONNECTION_WORKERS {
            let (sender, receiver) = mpsc::sync_channel(WORKER_QUEUE_CAPACITY);
            let worker_cancel = Arc::clone(&cancel);
            let worker_status = Arc::clone(status);
            let worker_token = Arc::clone(&token);
            match thread::Builder::new()
                .name(format!("freemixd-status-{index}"))
                .spawn(move || {
                    worker_loop(&receiver, &worker_cancel, &worker_status, &worker_token);
                }) {
                Ok(handle) => {
                    senders.push(sender);
                    threads.push(handle);
                }
                Err(error) => return Err(stop_threads(&cancel, threads, error)),
            }
        }

        let accept_cancel = Arc::clone(&cancel);
        let accept_status = Arc::clone(status);
        match thread::Builder::new()
            .name("freemixd-status-accept".into())
            .spawn(move || accept_loop(&listener, &senders, &accept_cancel, &accept_status))
        {
            Ok(handle) => threads.insert(0, handle),
            Err(error) => return Err(stop_threads(&cancel, threads, error)),
        }
        Ok(Self {
            address: bound,
            cancel,
            threads,
        })
    }

    pub(super) fn address(&self) -> SocketAddr {
        self.address
    }

    /// Stops accepting and joins every listener thread.
    ///
    /// The join is bounded by construction: accept and queue waits poll at
    /// [`ACCEPT_POLL`], and an in-flight request cannot outlive its request,
    /// response, and drain deadlines.
    ///
    /// # Errors
    ///
    /// Returns an error if a listener thread panicked.
    pub(super) fn shutdown(mut self) -> AppResult<()> {
        if self.stop() {
            return Err("freemixd status listener thread panicked".into());
        }
        Ok(())
    }

    fn stop(&mut self) -> bool {
        self.cancel.store(true, Ordering::Release);
        let mut panicked = false;
        for handle in std::mem::take(&mut self.threads) {
            panicked |= handle.join().is_err();
        }
        panicked
    }
}

impl Drop for StatusListener {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn stop_threads(
    cancel: &AtomicBool,
    threads: Vec<JoinHandle<()>>,
    error: std::io::Error,
) -> Box<dyn std::error::Error> {
    cancel.store(true, Ordering::Release);
    for handle in threads {
        let _ = handle.join();
    }
    error.into()
}

fn accept_loop(
    listener: &TcpListener,
    senders: &[SyncSender<TcpStream>],
    cancel: &AtomicBool,
    status: &DaemonStatus,
) {
    while !cancel.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => admit(stream, senders, status),
            Err(error)
                if matches!(error.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) =>
            {
                thread::sleep(ACCEPT_POLL);
            }
            Err(_) => break,
        }
    }
}

/// Hands the socket to the first free worker, or rejects it outright.
fn admit(stream: TcpStream, senders: &[SyncSender<TcpStream>], status: &DaemonStatus) {
    let mut pending = stream;
    for sender in senders {
        match sender.try_send(pending) {
            // Either the worker owns the socket now, or the worker is gone and
            // the socket closes with the value this arm drops.
            Ok(()) | Err(TrySendError::Disconnected(_)) => return,
            Err(TrySendError::Full(returned)) => pending = returned,
        }
    }
    status.rejected.fetch_add(1, Ordering::AcqRel);
    if prepare(&mut pending).is_ok() {
        let response = Response::busy();
        write_response(&mut pending, &response);
    }
    let _ = pending.shutdown(Shutdown::Both);
}

fn worker_loop(
    accepted: &Receiver<TcpStream>,
    cancel: &AtomicBool,
    status: &DaemonStatus,
    token: &StatusToken,
) {
    while !cancel.load(Ordering::Acquire) {
        match accepted.recv_timeout(ACCEPT_POLL) {
            Ok(mut stream) => {
                status.in_flight.fetch_add(1, Ordering::AcqRel);
                serve_connection(&mut stream, status, token);
                status.in_flight.fetch_sub(1, Ordering::AcqRel);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn serve_connection(stream: &mut TcpStream, status: &DaemonStatus, token: &StatusToken) {
    if prepare(stream).is_err() {
        let _ = stream.shutdown(Shutdown::Both);
        return;
    }
    let mut head = Vec::with_capacity(1024);
    let response = match read_head(stream, &mut head) {
        Ok(()) => route(&head, status, token),
        Err(response) => response,
    };
    status.served.fetch_add(1, Ordering::AcqRel);
    write_response(stream, &response);
    drain(stream);
    let _ = stream.shutdown(Shutdown::Both);
}

fn prepare(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(SOCKET_READ_TIMEOUT))?;
    stream.set_write_timeout(Some(SOCKET_WRITE_TIMEOUT))
}

/// Reads the request head, capping total bytes and total time.
fn read_head(stream: &mut TcpStream, head: &mut Vec<u8>) -> Result<(), Response> {
    let deadline = Instant::now() + REQUEST_DEADLINE;
    let mut chunk = [0_u8; 1024];
    loop {
        if let Some(end) = head.windows(4).position(|window| window == b"\r\n\r\n") {
            head.truncate(end);
            return Ok(());
        }
        if head.len() >= MAX_REQUEST_BYTES {
            return Err(Response::request_too_large());
        }
        if Instant::now() >= deadline {
            return Err(Response::request_timeout());
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err(Response::bad_request()),
            Ok(read) => {
                let room = MAX_REQUEST_BYTES - head.len();
                head.extend_from_slice(&chunk[..read.min(room)]);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err(Response::request_timeout());
            }
            Err(_) => return Err(Response::bad_request()),
        }
    }
}

/// Consumes any request remainder so the close is graceful instead of a reset.
fn drain(stream: &mut TcpStream) {
    let deadline = Instant::now() + DRAIN_DEADLINE;
    let mut chunk = [0_u8; 1024];
    let mut drained = 0;
    while drained < MAX_DRAIN_BYTES && Instant::now() < deadline {
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(read) => drained += read,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

fn write_response(stream: &mut TcpStream, response: &Response) {
    let mut head = String::with_capacity(256);
    let _ = write!(
        head,
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n",
        response.status,
        response.reason,
        response.body.len()
    );
    if response.authenticate {
        head.push_str("WWW-Authenticate: Bearer\r\n");
    }
    head.push_str("\r\n");
    let deadline = Instant::now() + RESPONSE_DEADLINE;
    if write_bounded(stream, head.as_bytes(), deadline) {
        let _ = write_bounded(stream, response.body.as_bytes(), deadline);
    }
    let _ = stream.flush();
}

fn write_bounded(stream: &mut TcpStream, bytes: &[u8], deadline: Instant) -> bool {
    let mut offset = 0;
    while offset < bytes.len() {
        if Instant::now() >= deadline {
            return false;
        }
        match stream.write(&bytes[offset..]) {
            Ok(0) => return false,
            Ok(written) => offset += written,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    true
}

struct RequestHead<'a> {
    method: &'a str,
    path: &'a str,
    authorization: Option<&'a [u8]>,
}

impl<'a> RequestHead<'a> {
    fn parse(head: &'a [u8]) -> Result<Self, Response> {
        let Ok(text) = str::from_utf8(head) else {
            return Err(Response::bad_request());
        };
        let mut lines = text.split("\r\n");
        let Some(request_line) = lines.next() else {
            return Err(Response::bad_request());
        };
        let mut parts = request_line.split(' ');
        let (Some(method), Some(target), Some(version), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(Response::bad_request());
        };
        if !matches!(version, "HTTP/1.1" | "HTTP/1.0") {
            return Err(Response::bad_request());
        }
        let path = target.split(['?', '#']).next().unwrap_or(target);

        let mut authorization = None;
        let mut headers = 0_usize;
        for line in lines.filter(|line| !line.is_empty()) {
            headers += 1;
            if headers > MAX_REQUEST_HEADERS {
                return Err(Response::too_many_headers());
            }
            let Some((name, value)) = line.split_once(':') else {
                return Err(Response::bad_request());
            };
            let value = value.trim();
            if name.eq_ignore_ascii_case("authorization") {
                if authorization.is_some() {
                    return Err(Response::bad_request());
                }
                authorization = Some(value.as_bytes());
            } else if name.eq_ignore_ascii_case("transfer-encoding")
                || (name.eq_ignore_ascii_case("content-length") && value != "0")
            {
                // The listener never reads a request body.
                return Err(Response::bad_request());
            }
        }
        Ok(Self {
            method,
            path,
            authorization,
        })
    }
}

fn route(head: &[u8], status: &DaemonStatus, token: &StatusToken) -> Response {
    let request = match RequestHead::parse(head) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let known = matches!(request.path, HEALTH_PATH | READY_PATH | BUNDLE_PATH);
    if known && request.method != "GET" {
        return Response::method_not_allowed();
    }
    let snapshot = status.snapshot();
    match request.path {
        HEALTH_PATH => health_response(&snapshot),
        READY_PATH => ready_response(&snapshot),
        BUNDLE_PATH if token.matches(request.authorization) => bundle_response(&snapshot),
        BUNDLE_PATH => Response::unauthorized(),
        _ => Response::not_found(),
    }
}

fn health_response(snapshot: &StatusSnapshot) -> Response {
    let state = if snapshot.health == HealthState::Unhealthy {
        "unhealthy"
    } else if snapshot.stalled {
        "stalled"
    } else {
        "live"
    };
    let body = format!(
        "FREEMIXD_STATUS\tv=1\tcheck=healthz\tstatus={state}\tuptime_ms={}\tlast_beat_ms={}\n",
        snapshot.uptime_millis, snapshot.beat_millis
    );
    if snapshot.live() {
        Response::ok(body)
    } else {
        Response::unavailable(body)
    }
}

fn ready_response(snapshot: &StatusSnapshot) -> Response {
    let ready = snapshot.ready();
    let body = format!(
        "FREEMIXD_STATUS\tv=1\tcheck=readyz\tstatus={}\treadiness={}\thealth={}\tliveness={}\tuptime_ms={}\n",
        if ready { "ready" } else { "not-ready" },
        snapshot.readiness.as_str(),
        snapshot.health.as_str(),
        if snapshot.stalled { "stalled" } else { "live" },
        snapshot.uptime_millis
    );
    if ready {
        Response::ok(body)
    } else {
        Response::unavailable(body)
    }
}

/// Renders the redacted support bundle from the snapshot alone.
///
/// Nothing derived from paths, hostnames, device names, addresses, or tokens is
/// ever placed into the bundle inputs.
fn bundle_response(snapshot: &StatusSnapshot) -> Response {
    let mut health = HealthRegistry::new();
    health.update(
        HealthCheck::new(
            "control-loop",
            !snapshot.stalled,
            snapshot.ready(),
            component_health(snapshot),
        )
        .with_detail(snapshot.readiness.as_str()),
    );
    health.update(HealthCheck::new(
        "status-listener",
        true,
        true,
        ComponentHealth::Healthy,
    ));

    let mut metrics = MetricStore::new(METRIC_CAPACITY);
    let _ = metrics.set_gauge(
        Metric::QueueDepth,
        snapshot.uptime_millis,
        approximate(u64::try_from(snapshot.in_flight).unwrap_or(u64::MAX)),
    );
    let _ = metrics.increment_counter(
        Metric::DroppedItems,
        snapshot.uptime_millis,
        approximate(snapshot.rejected),
    );

    let mut events = EventLog::new(EVENT_CAPACITY);
    if let Some(at) = snapshot.ready_millis {
        let _ = events.record(
            at,
            Severity::Info,
            Category::Runtime,
            "control plane became ready",
            [],
        );
    }
    if let Some(at) = snapshot.draining_millis {
        let _ = events.record(
            at,
            Severity::Warning,
            Category::Runtime,
            "cooperative shutdown requested",
            [],
        );
    }
    let _ = events.record(
        snapshot.uptime_millis,
        Severity::Info,
        Category::Runtime,
        "status snapshot exported",
        [
            EventField::new("readiness", snapshot.readiness.as_str()),
            EventField::new("served", snapshot.served),
            EventField::new("rejected", snapshot.rejected),
        ],
    );

    // `CapabilityRegistry` is only reachable through `fm-observability`, so the
    // empty registry is built by inference rather than by name.
    #[allow(clippy::default_trait_access)]
    let capabilities = Default::default();
    let export =
        SupportBundle::new(&events, &metrics, &health, &capabilities).export(BUNDLE_MAX_BYTES);
    let mut body = format!(
        "FREEMIXD_STATUS\tv=1\tcheck=support-bundle\tstatus=ok\tbytes={}\tuncapped_bytes={}\ttruncated={}\n",
        export.text.len(),
        export.uncapped_bytes,
        export.truncated
    );
    body.push_str(&export.text);
    Response::ok(body)
}

const fn component_health(snapshot: &StatusSnapshot) -> ComponentHealth {
    if snapshot.stalled || matches!(snapshot.health, HealthState::Unhealthy) {
        ComponentHealth::Unhealthy
    } else if matches!(snapshot.readiness, ReadinessState::Ready) {
        ComponentHealth::Healthy
    } else {
        ComponentHealth::Degraded
    }
}

/// Widens a bounded counter for metric storage without a precision-losing cast.
fn approximate(value: u64) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

struct Response {
    status: u16,
    reason: &'static str,
    authenticate: bool,
    body: String,
}

impl Response {
    fn new(status: u16, reason: &'static str, body: String) -> Self {
        Self {
            status,
            reason,
            authenticate: false,
            body,
        }
    }

    fn ok(body: String) -> Self {
        Self::new(200, "OK", body)
    }

    fn unavailable(body: String) -> Self {
        Self::new(503, "Service Unavailable", body)
    }

    fn bad_request() -> Self {
        Self::new(
            400,
            "Bad Request",
            "FREEMIXD_STATUS\tv=1\tcheck=request\tstatus=malformed\n".into(),
        )
    }

    fn unauthorized() -> Self {
        Self {
            authenticate: true,
            ..Self::new(
                401,
                "Unauthorized",
                "FREEMIXD_STATUS\tv=1\tcheck=support-bundle\tstatus=unauthorized\n".into(),
            )
        }
    }

    fn not_found() -> Self {
        Self::new(
            404,
            "Not Found",
            "FREEMIXD_STATUS\tv=1\tcheck=request\tstatus=unknown-route\n".into(),
        )
    }

    fn method_not_allowed() -> Self {
        Self::new(
            405,
            "Method Not Allowed",
            "FREEMIXD_STATUS\tv=1\tcheck=request\tstatus=method-not-allowed\n".into(),
        )
    }

    fn request_timeout() -> Self {
        Self::new(
            408,
            "Request Timeout",
            "FREEMIXD_STATUS\tv=1\tcheck=request\tstatus=timeout\n".into(),
        )
    }

    fn request_too_large() -> Self {
        Self::new(
            413,
            "Content Too Large",
            format!(
                "FREEMIXD_STATUS\tv=1\tcheck=request\tstatus=too-large\tlimit_bytes={MAX_REQUEST_BYTES}\n"
            ),
        )
    }

    fn too_many_headers() -> Self {
        Self::new(
            431,
            "Request Header Fields Too Large",
            format!(
                "FREEMIXD_STATUS\tv=1\tcheck=request\tstatus=too-many-headers\tlimit={MAX_REQUEST_HEADERS}\n"
            ),
        )
    }

    fn busy() -> Self {
        Self::new(
            503,
            "Service Unavailable",
            format!(
                "FREEMIXD_STATUS\tv=1\tcheck=admission\tstatus=busy\tlimit={CONNECTION_WORKERS}\n"
            ),
        )
    }
}
