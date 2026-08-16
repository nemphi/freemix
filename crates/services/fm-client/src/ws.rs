use std::io;
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use fm_protocol::{CodecError, MAX_LINE_BYTES, WireMessage, decode_line, encode_line};
use tungstenite::{
    client::client_with_config,
    handshake::HandshakeError,
    http::Request,
    protocol::{Message, WebSocket, WebSocketConfig},
};

const READ_BUFFER_BYTES: usize = 8 * 1024;

#[derive(Debug)]
pub(crate) struct WebSocketConnection {
    socket: WebSocket<TcpStream>,
}

impl WebSocketConnection {
    pub(crate) fn connect(
        address: SocketAddr,
        bearer_token: &str,
        connect_timeout: Duration,
    ) -> Result<Self, &'static str> {
        validate(address, bearer_token)?;
        let stream = TcpStream::connect_timeout(&address, connect_timeout)
            .map_err(|_| "WebSocket connection failed")?;
        stream
            .set_nodelay(true)
            .map_err(|_| "WebSocket connection failed")?;
        stream
            .set_read_timeout(Some(connect_timeout))
            .and_then(|()| stream.set_write_timeout(Some(connect_timeout)))
            .map_err(|_| "WebSocket connection failed")?;
        let request = Request::builder()
            .uri(format!("ws://{address}/v1/control"))
            .header("Authorization", format!("Bearer {bearer_token}"))
            .body(())
            .map_err(|_| "WebSocket handshake failed")?;
        let mut config = WebSocketConfig::default();
        config.read_buffer_size = READ_BUFFER_BYTES;
        config.write_buffer_size = READ_BUFFER_BYTES;
        config.max_write_buffer_size = 2 * MAX_LINE_BYTES;
        config.max_message_size = Some(MAX_LINE_BYTES);
        config.max_frame_size = Some(MAX_LINE_BYTES);
        let (socket, _) = client_with_config(request, stream, Some(config))
            .map_err(|_: HandshakeError<_>| "WebSocket handshake failed")?;
        socket
            .get_ref()
            .set_read_timeout(None)
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

    pub(crate) fn flush(&mut self) -> Result<(), &'static str> {
        self.socket.flush().map_err(|_| "WebSocket flush failed")
    }

    pub(crate) fn receive(&mut self) -> Result<Option<WireMessage>, WebSocketReceiveError> {
        loop {
            match self.socket.read() {
                Ok(Message::Text(text)) => {
                    return decode_line(text.as_ref())
                        .map(Some)
                        .map_err(WebSocketReceiveError::Codec);
                }
                Ok(Message::Ping(_) | Message::Pong(_)) => continue,
                Ok(Message::Close(frame)) => {
                    let _ = self.socket.send(Message::Close(frame));
                    let _ = self.socket.flush();
                    return Ok(None);
                }
                Ok(Message::Binary(_) | Message::Frame(_)) => {
                    return Err(WebSocketReceiveError::Protocol);
                }
                Err(tungstenite::Error::Io(error))
                    if error.kind() == io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(None);
                }
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err(WebSocketReceiveError::TimedOut);
                }
                Err(_) => return Err(WebSocketReceiveError::Transport),
            }
        }
    }

    pub(crate) fn receive_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<ReceiveStatus, WebSocketReceiveError> {
        self.socket
            .get_mut()
            .set_read_timeout(Some(timeout))
            .map_err(|_| WebSocketReceiveError::Transport)?;
        let result = match self.receive() {
            Ok(message) => Ok(ReceiveStatus {
                message,
                timed_out: false,
            }),
            Err(WebSocketReceiveError::Transport)
                if self.socket.get_mut().set_read_timeout(None).is_ok() =>
            {
                Err(WebSocketReceiveError::Transport)
            }
            Err(error) => Err(error),
        };
        let reset = self.socket.get_mut().set_read_timeout(None);
        match (result, reset) {
            (Err(WebSocketReceiveError::TimedOut), _) => Ok(ReceiveStatus {
                message: None,
                timed_out: true,
            }),
            (Err(error), _) => Err(error),
            (Ok(_status), Err(_)) => Err(WebSocketReceiveError::Transport),
            (Ok(status), Ok(())) => Ok(status),
        }
    }

    pub(crate) fn shutdown(&mut self) {
        let _ = self.socket.close(None);
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
    TimedOut,
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
