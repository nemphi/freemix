use std::{
    env, io,
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use fm_protocol::{MAX_LINE_BYTES, WireMessage, decode_line};
use tungstenite::{
    Error as WebSocketError, Message, accept_hdr_with_config,
    handshake::server::{ErrorResponse, Request, Response},
    http::{HeaderValue, StatusCode},
    protocol::WebSocketConfig,
};

use super::AppResult;

const PATH: &str = "/v1/control";
const TOKEN_MIN_BYTES: usize = 32;
const TOKEN_MAX_BYTES: usize = 256;
const SOCKET_HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(500);
const RELAY_POLL: Duration = Duration::from_millis(25);
const WEBSOCKET_READ_BUFFER: usize = 8 * 1024;
const WEBSOCKET_WRITE_BUFFER: usize = 8 * 1024;
const WEBSOCKET_MAX_WRITE_BUFFER: usize = 2 * MAX_LINE_BYTES;
const WEBSOCKET_MESSAGE_LIMIT: usize = MAX_LINE_BYTES;

const INBOUND_CAPACITY: usize = 8;
const OUTBOUND_CAPACITY: usize = 32;

pub(super) struct WebGateway {
    address: SocketAddr,
    events: Receiver<WebEvent>,
    cancel: Arc<AtomicBool>,
    accepting: Arc<AtomicBool>,
    listener_thread: Option<JoinHandle<()>>,
    worker_thread: Option<JoinHandle<()>>,
}

pub(super) enum WebEvent {
    Connected(WebConnection),
}

pub(super) struct WebConnection {
    pub(super) inbound: Receiver<WireMessage>,
    pub(super) outbound: SyncSender<Vec<u8>>,
    pub(super) acknowledgements: Receiver<()>,
    pub(super) cancel: Arc<AtomicBool>,
}

struct WebToken {
    bytes: [u8; TOKEN_MAX_BYTES],
    length: usize,
}

impl WebToken {
    fn from_environment() -> AppResult<Self> {
        let value = env::var("FREEMIXD_WEB_TOKEN")
            .map_err(|_| "FREEMIXD_WEB_TOKEN is required when --web-listen is enabled")?;
        let bytes = value.as_bytes();
        if !(TOKEN_MIN_BYTES..=TOKEN_MAX_BYTES).contains(&bytes.len())
            || !bytes.iter().all(u8::is_ascii_graphic)
        {
            return Err("FREEMIXD_WEB_TOKEN must be 32..=256 ASCII graphic bytes".into());
        }
        let mut token = [0; TOKEN_MAX_BYTES];
        token[..bytes.len()].copy_from_slice(bytes);
        Ok(Self {
            bytes: token,
            length: bytes.len(),
        })
    }

    fn matches(&self, authorization: Option<&HeaderValue>) -> bool {
        let presented = authorization
            .map(HeaderValue::as_bytes)
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

impl WebGateway {
    pub(super) fn bind(listener: TcpListener) -> AppResult<Self> {
        let address = listener.local_addr()?;
        let token = Arc::new(WebToken::from_environment()?);
        let (accepted_tx, accepted_rx) = mpsc::sync_channel(1);
        let (events_tx, events_rx) = mpsc::sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let accepting = Arc::new(AtomicBool::new(false));

        let listener_cancel = Arc::clone(&cancel);
        let listener_accepting = Arc::clone(&accepting);
        let listener_thread = thread::Builder::new()
            .name("freemixd-web-listener".into())
            .spawn(move || {
                listener_loop(listener, accepted_tx, listener_cancel, listener_accepting)
            })?;

        let worker_cancel = Arc::clone(&cancel);
        let worker_token = Arc::clone(&token);
        let worker_thread = match thread::Builder::new()
            .name("freemixd-web-worker".into())
            .spawn(move || worker_loop(accepted_rx, events_tx, worker_cancel, worker_token))
        {
            Ok(handle) => handle,
            Err(error) => {
                cancel.store(true, Ordering::Release);
                let _ = listener_thread.join();
                return Err(error.into());
            }
        };

        Ok(Self {
            address,
            events: events_rx,
            cancel,
            accepting,
            listener_thread: Some(listener_thread),
            worker_thread: Some(worker_thread),
        })
    }

    pub(super) fn address(&self) -> SocketAddr {
        self.address
    }

    pub(super) fn start_accepting(&self) {
        self.accepting.store(true, Ordering::Release);
    }

    pub(super) fn try_event(&self) -> Result<Option<WebEvent>, ()> {
        match self.events.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(()),
        }
    }

    pub(super) fn shutdown(mut self) -> AppResult<()> {
        self.cancel.store(true, Ordering::Release);
        let mut panic = None;
        for handle in [&mut self.listener_thread, &mut self.worker_thread] {
            if let Some(handle) = handle.take()
                && handle.join().is_err()
            {
                panic = Some("freemixd WebSocket gateway thread panicked");
            }
        }
        panic.map_or(Ok(()), |message| Err(message.into()))
    }
}

impl Drop for WebGateway {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(handle) = self.listener_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.worker_thread.take() {
            let _ = handle.join();
        }
    }
}

mod worker;
use worker::{listener_loop, worker_loop};
