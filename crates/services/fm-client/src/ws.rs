use std::io;
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use fm_protocol::{CodecError, MAX_LINE_BYTES, WireMessage, decode_line, encode_line};
use tungstenite::{
    client::{IntoClientRequest, client_with_config},
    handshake::HandshakeError,
    protocol::{Message, WebSocket, WebSocketConfig},
};

const READ_BUFFER_BYTES: usize = 8 * 1024;
const HANDSHAKE_WAIT: Duration = Duration::from_millis(1);

#[derive(Debug)]
pub(crate) struct WebSocketConnection {
    socket: WebSocket<TcpStream>,
}

impl WebSocketConnection {
    pub(crate) fn connect(
        address: SocketAddr,
        bearer_token: &str,
        timeout: Duration,
    ) -> Result<Self, &'static str> {
        validate(address, bearer_token)?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or("WebSocket connection failed")?;
        let stream = TcpStream::connect_timeout(&address, timeout)
            .map_err(|_| "WebSocket connection failed")?;
        stream
            .set_nodelay(true)
            .map_err(|_| "WebSocket connection failed")?;
        let mut request = format!("ws://{address}/v1/control")
            .into_client_request()
            .map_err(|_| "WebSocket handshake failed")?;
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {bearer_token}")
                .parse()
                .map_err(|_| "WebSocket handshake failed")?,
        );
        let mut config = WebSocketConfig::default();
        config.read_buffer_size = READ_BUFFER_BYTES;
        config.write_buffer_size = READ_BUFFER_BYTES;
        config.max_write_buffer_size = 2 * MAX_LINE_BYTES;
        config.max_message_size = Some(MAX_LINE_BYTES);
        config.max_frame_size = Some(MAX_LINE_BYTES);
        stream
            .set_nonblocking(true)
            .map_err(|_| "WebSocket connection failed")?;
        let _ = remaining(deadline).ok_or("WebSocket handshake failed")?;
        let mut handshake = client_with_config(request, stream, Some(config));
        let (socket, _) = loop {
            match handshake {
                Ok(result) => {
                    let _ = remaining(deadline).ok_or("WebSocket handshake failed")?;
                    break result;
                }
                Err(HandshakeError::Interrupted(mid)) => {
                    let wait = remaining(deadline).ok_or("WebSocket handshake failed")?;
                    std::thread::sleep(wait.min(HANDSHAKE_WAIT));
                    let _ = remaining(deadline).ok_or("WebSocket handshake failed")?;
                    handshake = mid.handshake();
                }
                Err(HandshakeError::Failure(_)) => return Err("WebSocket handshake failed"),
            }
        };
        socket
            .get_ref()
            .set_nonblocking(false)
            .and_then(|()| socket.get_ref().set_read_timeout(None))
            .and_then(|()| socket.get_ref().set_write_timeout(None))
            .map_err(|_| "WebSocket connection failed")?;
        Ok(Self { socket })
    }

    pub(crate) fn send(&mut self, message: &WireMessage) -> Result<(), &'static str> {
        let line = encode_line(message).map_err(|_| "WebSocket protocol encoding failed")?;
        self.socket
            .write(Message::text(line))
            .map_err(|_| "WebSocket write failed")
    }

    pub(crate) fn send_cancellable(
        &mut self,
        message: &WireMessage,
        interval: Duration,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<bool, &'static str> {
        let line = encode_line(message).map_err(|_| "WebSocket protocol encoding failed")?;
        let result = if cancelled() {
            Ok(false)
        } else {
            self.socket
                .get_mut()
                .set_write_timeout(Some(nonzero(interval)))
                .map_err(|_| "WebSocket write setup failed")?;
            // Do not retry: a partial WebSocket frame makes replay unsafe.
            self.socket
                .write(Message::text(line))
                .map(|_| true)
                .map_err(|_| "WebSocket write failed")
        };
        self.reset_write_timeout(result)
    }

    pub(crate) fn flush(&mut self) -> Result<(), &'static str> {
        self.socket.flush().map_err(|_| "WebSocket flush failed")
    }

    pub(crate) fn flush_cancellable(
        &mut self,
        interval: Duration,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<bool, &'static str> {
        let result = loop {
            if cancelled() {
                break Ok(false);
            }
            self.socket
                .get_mut()
                .set_write_timeout(Some(nonzero(interval)))
                .map_err(|_| "WebSocket flush setup failed")?;
            match self.socket.flush() {
                Ok(()) => break Ok(true),
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => break Err("WebSocket flush failed"),
            }
        };
        self.reset_write_timeout(result)
    }

    pub(crate) fn receive(&mut self) -> Result<Option<WireMessage>, WebSocketReceiveError> {
        self.socket
            .get_mut()
            .set_read_timeout(None)
            .and_then(|()| self.socket.get_mut().set_write_timeout(None))
            .map_err(|_| WebSocketReceiveError::Transport)?;
        let result = self.receive_inner(None).map(|status| status.message);
        self.reset_timeouts(result)
    }

    pub(crate) fn receive_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<ReceiveStatus, WebSocketReceiveError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(WebSocketReceiveError::Transport)?;
        let result = self.receive_inner(Some(deadline));
        self.reset_timeouts(result)
    }

    fn receive_inner(
        &mut self,
        deadline: Option<Instant>,
    ) -> Result<ReceiveStatus, WebSocketReceiveError> {
        loop {
            if let Some(deadline) = deadline {
                let Some(timeout) = remaining(deadline) else {
                    return Ok(ReceiveStatus::timed_out());
                };
                self.socket
                    .get_mut()
                    .set_read_timeout(Some(timeout))
                    .and_then(|()| self.socket.get_mut().set_write_timeout(Some(timeout)))
                    .map_err(|_| WebSocketReceiveError::Transport)?;
            }
            match self.socket.read() {
                Ok(Message::Text(text)) => {
                    return decode_line(text.as_ref())
                        .map(|message| ReceiveStatus {
                            message: Some(message),
                            timed_out: false,
                        })
                        .map_err(WebSocketReceiveError::Codec);
                }
                Ok(Message::Ping(_) | Message::Pong(_)) => continue,
                Ok(Message::Close(_)) => {
                    // tungstenite queued its automatic Close reply; flush it once.
                    if let Some(deadline) = deadline {
                        let Some(timeout) = remaining(deadline) else {
                            return Ok(ReceiveStatus {
                                message: None,
                                timed_out: false,
                            });
                        };
                        self.socket
                            .get_mut()
                            .set_write_timeout(Some(timeout))
                            .map_err(|_| WebSocketReceiveError::Transport)?;
                    }
                    self.socket
                        .flush()
                        .map_err(|_| WebSocketReceiveError::Transport)?;
                    return Ok(ReceiveStatus {
                        message: None,
                        timed_out: false,
                    });
                }
                Ok(Message::Binary(_) | Message::Frame(_)) => {
                    return Err(WebSocketReceiveError::Protocol);
                }
                Err(tungstenite::Error::Io(error))
                    if error.kind() == io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(ReceiveStatus {
                        message: None,
                        timed_out: false,
                    });
                }
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(ReceiveStatus::timed_out());
                }
                Err(_) => return Err(WebSocketReceiveError::Transport),
            }
        }
    }

    fn reset_write_timeout<T>(
        &mut self,
        result: Result<T, &'static str>,
    ) -> Result<T, &'static str> {
        let reset = self.socket.get_mut().set_write_timeout(None);
        match (result, reset) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(_)) => Err("WebSocket write setup failed"),
            (Ok(value), Ok(())) => Ok(value),
        }
    }

    fn reset_timeouts<T>(
        &mut self,
        result: Result<T, WebSocketReceiveError>,
    ) -> Result<T, WebSocketReceiveError> {
        let read = self.socket.get_mut().set_read_timeout(None);
        let write = self.socket.get_mut().set_write_timeout(None);
        match (result, read, write) {
            (Err(error), _, _) => Err(error),
            (Ok(_), Err(_), _) | (Ok(_), _, Err(_)) => Err(WebSocketReceiveError::Transport),
            (Ok(value), Ok(()), Ok(())) => Ok(value),
        }
    }

    pub(crate) fn shutdown(&mut self) {
        let _ = self.socket.get_ref().shutdown(Shutdown::Both);
    }
}

impl ReceiveStatus {
    fn timed_out() -> Self {
        Self {
            message: None,
            timed_out: true,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ReceiveStatus {
    pub(crate) message: Option<WireMessage>,
    pub(crate) timed_out: bool,
}

#[derive(Debug)]
pub(crate) enum WebSocketReceiveError {
    Codec(CodecError),
    Protocol,
    Transport,
}

pub(crate) fn validate(address: SocketAddr, token: &str) -> Result<(), &'static str> {
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err("WebSocket address must be loopback with a non-zero port");
    }
    if !(32..=256).contains(&token.len()) || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err("WebSocket bearer token must be 32-256 ASCII graphic bytes");
    }
    Ok(())
}

fn remaining(deadline: Instant) -> Option<Duration> {
    let duration = deadline.checked_duration_since(Instant::now())?;
    (!duration.is_zero()).then_some(duration)
}

fn nonzero(duration: Duration) -> Duration {
    if duration.is_zero() {
        Duration::from_millis(1)
    } else {
        duration
    }
}
