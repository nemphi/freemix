use super::*;

pub(super) fn listener_loop(
    listener: TcpListener,
    accepted: SyncSender<TcpStream>,
    cancel: Arc<AtomicBool>,
    accepting: Arc<AtomicBool>,
) {
    while !cancel.load(Ordering::Acquire) {
        if !accepting.load(Ordering::Acquire) {
            thread::sleep(RELAY_POLL);
            continue;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if accepted.try_send(stream).is_err() {
                    // One active WebSocket is the transport bound. Drop excess peers.
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) =>
            {
                thread::sleep(RELAY_POLL)
            }
            Err(_) => break,
        }
    }
}

pub(super) fn worker_loop(
    accepted: Receiver<TcpStream>,
    events: SyncSender<WebEvent>,
    cancel: Arc<AtomicBool>,
    token: Arc<WebToken>,
) {
    while !cancel.load(Ordering::Acquire) {
        let stream = match accepted.recv_timeout(RELAY_POLL) {
            Ok(stream) => stream,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if let Err(error) = stream.set_read_timeout(Some(SOCKET_HANDSHAKE_TIMEOUT)) {
            let _ = stream.shutdown(Shutdown::Both);
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            continue;
        }
        if stream
            .set_write_timeout(Some(SOCKET_HANDSHAKE_TIMEOUT))
            .is_err()
        {
            let _ = stream.shutdown(Shutdown::Both);
            continue;
        }
        let config = WebSocketConfig::default()
            .read_buffer_size(WEBSOCKET_READ_BUFFER)
            .write_buffer_size(WEBSOCKET_WRITE_BUFFER)
            .max_write_buffer_size(WEBSOCKET_MAX_WRITE_BUFFER)
            .max_message_size(Some(WEBSOCKET_MESSAGE_LIMIT))
            .max_frame_size(Some(WEBSOCKET_MESSAGE_LIMIT))
            .accept_unmasked_frames(false);
        let callback_token = Arc::clone(&token);
        let websocket = accept_hdr_with_config(
            stream,
            move |request: &Request, response: Response| {
                if request.uri().path_and_query().map(|value| value.as_str()) != Some(PATH) {
                    return Err(http_error(StatusCode::NOT_FOUND));
                }
                if !callback_token.matches(request.headers().get("Authorization")) {
                    return Err(http_error(StatusCode::UNAUTHORIZED));
                }
                Ok(response)
            },
            Some(config),
        );
        let Ok(mut websocket) = websocket else {
            continue;
        };
        if websocket
            .get_ref()
            .set_read_timeout(Some(RELAY_POLL))
            .is_err()
            || websocket
                .get_ref()
                .set_write_timeout(Some(Duration::from_millis(250)))
                .is_err()
        {
            let _ = websocket.close(None);
            continue;
        }

        let (inbound_tx, inbound_rx) = mpsc::sync_channel(INBOUND_CAPACITY);
        let (outbound_tx, outbound_rx) = mpsc::sync_channel(OUTBOUND_CAPACITY);
        let (acknowledgements_tx, acknowledgements_rx) = mpsc::sync_channel(OUTBOUND_CAPACITY);
        let peer_cancel = Arc::new(AtomicBool::new(false));
        let connection = WebConnection {
            inbound: inbound_rx,
            outbound: outbound_tx,
            acknowledgements: acknowledgements_rx,
            cancel: Arc::clone(&peer_cancel),
        };
        if events.try_send(WebEvent::Connected(connection)).is_err() {
            let _ = websocket.close(None);
            continue;
        }
        relay(
            &mut websocket,
            inbound_tx,
            outbound_rx,
            acknowledgements_tx,
            peer_cancel,
            &cancel,
        );
    }
}

fn relay(
    websocket: &mut tungstenite::WebSocket<TcpStream>,
    inbound: SyncSender<WireMessage>,
    outbound: Receiver<Vec<u8>>,
    acknowledgements: SyncSender<()>,
    peer_cancel: Arc<AtomicBool>,
    gateway_cancel: &AtomicBool,
) {
    while !gateway_cancel.load(Ordering::Acquire) && !peer_cancel.load(Ordering::Acquire) {
        loop {
            match outbound.try_recv() {
                Ok(bytes) => {
                    let Ok(text) = String::from_utf8(bytes) else {
                        return;
                    };
                    if websocket.send(Message::text(text)).is_err() {
                        return;
                    }
                    if acknowledgements.try_send(()).is_err() {
                        return;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        match websocket.read() {
            Ok(Message::Text(text)) => {
                let line = text.as_str();
                let Ok(message) = decode_line(line) else {
                    return;
                };
                if inbound.try_send(message).is_err() {
                    return;
                }
            }
            Ok(Message::Binary(_)) => return,
            Ok(Message::Ping(_) | Message::Pong(_)) => {}
            Ok(Message::Close(_)) => {
                if websocket.flush().is_err() {
                    return;
                }
                return;
            }
            Ok(Message::Frame(_)) => return,
            Err(WebSocketError::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) =>
            {
                thread::sleep(RELAY_POLL)
            }
            Err(_) => return,
        }
    }
    let _ = websocket.close(None);
}

fn http_error(status: StatusCode) -> ErrorResponse {
    Response::builder()
        .status(status)
        .body(None)
        .expect("static WebSocket HTTP error response is valid")
}
