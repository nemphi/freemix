use std::{
    error::Error,
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpStream},
    num::NonZeroU128,
    time::{SystemTime, UNIX_EPOCH},
};

use fm_client::{Client, ClientConfig, Intake, Outbound};
use fm_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientType, CommandPayload, CommandResult, EventPayload,
    FadeToBlackState, HandshakeOutcome, HandshakeRequest, Role, RuntimeLifecycleEvent,
    ServerIdentity, WireMessage, decode_line, encode_line,
};
use fm_types::ProjectId;
use fm_ui_model::ManualTransitionStatus;

type RemoteResult<T> = Result<T, Box<dyn Error>>;

pub fn status(address: SocketAddr) -> RemoteResult<()> {
    let remote = Remote::connect(address)?;
    remote.print_status()
}

pub fn execute(
    address: SocketAddr,
    payload: CommandPayload,
    key: Option<String>,
    expected_revision: Option<u64>,
) -> RemoteResult<()> {
    let mut remote = Remote::connect(address)?;
    let key = match key {
        Some(key) => key,
        None => implicit_key(payload)?,
    };
    remote.execute(payload, key, expected_revision)
}

struct Remote {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    client: Client,
    project_id: ProjectId,
}

impl Remote {
    fn connect(address: SocketAddr) -> RemoteResult<Self> {
        if !address.ip().is_loopback() {
            return Err(RemoteFailure(format!(
                "development mode requires a loopback address, got {}",
                address.ip()
            ))
            .into());
        }

        let stream = TcpStream::connect(address)?;
        stream.set_nodelay(true)?;
        let mut remote = Self::uninitialized(stream)?;
        remote.write(&WireMessage::HandshakeRequest(HandshakeRequest {
            protocol: CURRENT_PROTOCOL_VERSION,
            build: format!("freemix-cli-{}", env!("CARGO_PKG_VERSION")),
            client_type: ClientType::Cli,
            desired_role: Role::Operator,
            resume_cursor: None,
        }))?;

        let response = match remote.read()? {
            WireMessage::HandshakeResponse(response) => response,
            WireMessage::Error(error) => return Err(protocol_error(&error.error).into()),
            _ => {
                return Err(
                    RemoteFailure("expected handshake_response during handshake".into()).into(),
                );
            }
        };
        match &response.outcome {
            HandshakeOutcome::Snapshot { .. } => {}
            HandshakeOutcome::Resume { .. } => {
                return Err(
                    RemoteFailure("daemon selected resume without a client cursor".into()).into(),
                );
            }
            HandshakeOutcome::Rejected { error } => return Err(protocol_error(error).into()),
        }

        let project_id = project_id(&response.server)?;
        let mut client = Client::new(ClientConfig::new(
            env!("CARGO_PKG_VERSION"),
            ClientType::Cli,
            Role::Operator,
            unique_client_id()?,
            project_id,
        ))?;
        client.start_connect()?;
        let request = client.transport_connected()?;
        if request.resume_cursor.is_some() {
            return Err(
                RemoteFailure("fresh client unexpectedly supplied a resume cursor".into()).into(),
            );
        }
        client.accept_handshake(response)?;
        match remote.read()? {
            WireMessage::Snapshot(snapshot) => {
                client.intake(WireMessage::Snapshot(snapshot))?;
            }
            WireMessage::Error(error) => return Err(protocol_error(&error.error).into()),
            _ => {
                return Err(
                    RemoteFailure("expected initial snapshot after handshake".into()).into(),
                );
            }
        }

        remote.client = client;
        remote.project_id = project_id;
        Ok(remote)
    }

    fn uninitialized(stream: TcpStream) -> RemoteResult<Self> {
        let placeholder = ProjectId::new(NonZeroU128::MIN);
        Ok(Self {
            writer: stream.try_clone()?,
            reader: BufReader::new(stream),
            client: Client::new(ClientConfig::new(
                env!("CARGO_PKG_VERSION"),
                ClientType::Cli,
                Role::Operator,
                "uninitialized",
                placeholder,
            ))?,
            project_id: placeholder,
        })
    }

    fn execute(
        &mut self,
        payload: CommandPayload,
        key: String,
        expected_revision: Option<u64>,
    ) -> RemoteResult<()> {
        let queued = self
            .client
            .queue_command(payload, key, expected_revision, None)?;
        let Some(Outbound::Command(command)) = self.client.pop_outbound() else {
            return Err(
                RemoteFailure("client did not queue the command for transport".into()).into(),
            );
        };
        debug_assert_eq!(command, queued);
        self.write(&WireMessage::Command(command.clone()))?;

        let result = match self.read()? {
            WireMessage::CommandResult(result) => result,
            WireMessage::Error(error) => return Err(protocol_error(&error.error).into()),
            WireMessage::Event(_) | WireMessage::RuntimeEvent(_) => {
                return Err(RemoteFailure(
                    "daemon sent a durable/runtime event before the command result".into(),
                )
                .into());
            }
            _ => return Err(RemoteFailure("expected command_result from daemon".into()).into()),
        };
        let replayed = result_id(&result) != command.id;
        if !replayed {
            let intake = self
                .client
                .intake(WireMessage::CommandResult(result.clone()))?;
            if intake != Intake::ResultReconciled {
                return Err(
                    RemoteFailure("client did not reconcile the command result".into()).into(),
                );
            }
        }

        match result {
            CommandResult::Rejected { code, message, .. } => {
                Err(RemoteFailure(format!("{code}: {message}")).into())
            }
            CommandResult::Accepted { revision, .. } => {
                if !replayed {
                    self.read_command_events(revision)?;
                }
                self.print_status()
            }
        }
    }

    fn read_command_events(&mut self, revision: u64) -> RemoteResult<()> {
        let event = match self.read()? {
            WireMessage::Event(event) => event,
            WireMessage::Error(error) => return Err(protocol_error(&error.error).into()),
            _ => {
                return Err(
                    RemoteFailure("expected durable command event from daemon".into()).into(),
                );
            }
        };
        if event.cursor.revision != revision {
            return Err(RemoteFailure(format!(
                "command event revision {} does not match result revision {revision}",
                event.cursor.revision
            ))
            .into());
        }
        if !matches!(event.payload, EventPayload::DesiredSwitcher { .. }) {
            return Err(RemoteFailure("command event was not desired_switcher".into()).into());
        }
        self.client.intake(WireMessage::Event(event))?;

        loop {
            let runtime = match self.read()? {
                WireMessage::RuntimeEvent(event) => event,
                WireMessage::Error(error) => return Err(protocol_error(&error.error).into()),
                _ => {
                    return Err(
                        RemoteFailure("expected runtime command event from daemon".into()).into(),
                    );
                }
            };
            if runtime.revision != revision {
                return Err(RemoteFailure(format!(
                    "runtime event revision {} does not match result revision {revision}",
                    runtime.revision
                ))
                .into());
            }
            let realized = matches!(&runtime.event, RuntimeLifecycleEvent::Realized { .. });
            self.client.intake(WireMessage::RuntimeEvent(runtime))?;
            if realized {
                break;
            }
        }
        Ok(())
    }

    fn print_status(&self) -> RemoteResult<()> {
        let state = self
            .client
            .model()
            .state()
            .ok_or_else(|| RemoteFailure("remote project state is unavailable".into()))?;
        let cursor = self
            .client
            .model()
            .reconnect_cursor()
            .ok_or_else(|| RemoteFailure("remote project cursor is unavailable".into()))?;
        let switcher = state.switcher();
        println!(
            "project_id={} show={:?} revision={} frame=unavailable Program(desired={}, realized={}) Preview(desired={}, realized={}) TBar(desired={}, realized={}) FTB(desired={}, realized={}) AudioStrips={} Overlays(desired={}, realized={})",
            self.project_id,
            state.show_name(),
            cursor.revision,
            switcher.desired.program,
            switcher.realized.program,
            switcher.desired.preview,
            switcher.realized.preview,
            format_manual_transition(switcher.desired_manual_transition),
            format_manual_transition(switcher.realized_manual_transition),
            format_fade_to_black(switcher.desired_fade_to_black),
            format_fade_to_black(switcher.realized_fade_to_black),
            format_input_audio_strips(state),
            format_overlays(state.desired_overlays()),
            format_overlays(state.realized_overlays()),
        );
        Ok(())
    }

    fn write(&mut self, message: &WireMessage) -> RemoteResult<()> {
        let line = encode_line(message)?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()?;
        Ok(())
    }

    fn read(&mut self) -> RemoteResult<WireMessage> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Err(RemoteFailure("daemon closed the TCP connection".into()).into());
        }
        Ok(decode_line(&line)?)
    }
}

fn format_input_audio_strips(state: &fm_ui_model::ProjectState) -> String {
    format!(
        "[{}]",
        state
            .input_audio_strips()
            .iter()
            .map(|status| {
                format!(
                    "{}:{:?}:{}:{}:{}:{}:{}:{}",
                    status.input,
                    state.input_name(status.input).unwrap_or(""),
                    status.gain_millidb,
                    status.balance_basis_points,
                    status.muted,
                    status.soloed,
                    status.follow_video,
                    status.delay_samples
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn format_fade_to_black(state: FadeToBlackState) -> String {
    format!(
        "{}@{}/{}",
        if state.target_active { "black" } else { "live" },
        state.position.numerator(),
        fm_protocol::FadeToBlackPosition::DENOMINATOR,
    )
}

fn format_manual_transition(status: ManualTransitionStatus) -> String {
    match status {
        ManualTransitionStatus::Inactive => "inactive".to_owned(),
        ManualTransitionStatus::Active(state) => format!(
            "{}:{}->{}@{}",
            match state.kind {
                fm_protocol::ManualTransitionKind::Fade => "fade",
                fm_protocol::ManualTransitionKind::Wipe => "wipe",
                fm_protocol::ManualTransitionKind::AlphaFade => "alpha_fade",
            },
            state.from,
            state.to,
            state.position.basis_points(),
        ),
    }
}

fn format_overlays(overlays: &[fm_ui_model::OverlayStatus]) -> String {
    format!(
        "[{}]",
        overlays
            .iter()
            .map(|overlay| format!(
                "{}:{}:{}:opacity={}:{}@{}:{}:{}:queue=[{}]",
                overlay.channel,
                overlay
                    .source
                    .map_or_else(|| "none".to_owned(), |source| source.to_string()),
                if overlay.active { "on" } else { "off" },
                overlay.opacity,
                match overlay.transition {
                    fm_protocol::OverlayTransitionKind::Cut => "cut",
                    fm_protocol::OverlayTransitionKind::Fade => "fade",
                },
                overlay.duration_frames,
                match overlay.position {
                    fm_protocol::OverlayPositionPreset::FullFrame => "full-frame",
                    fm_protocol::OverlayPositionPreset::TopLeft => "top-left",
                    fm_protocol::OverlayPositionPreset::TopRight => "top-right",
                    fm_protocol::OverlayPositionPreset::BottomLeft => "bottom-left",
                    fm_protocol::OverlayPositionPreset::BottomRight => "bottom-right",
                },
                match overlay.border {
                    fm_protocol::OverlayBorderPreset::None => "none",
                    fm_protocol::OverlayBorderPreset::ThinWhite => "thin-white",
                    fm_protocol::OverlayBorderPreset::ThickWhite => "thick-white",
                },
                overlay
                    .queued_sources
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("+"),
            ))
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn project_id(server: &ServerIdentity) -> RemoteResult<ProjectId> {
    let value = server.project_id.parse::<u128>()?;
    NonZeroU128::new(value)
        .map(ProjectId::new)
        .ok_or_else(|| RemoteFailure("daemon project ID is zero".into()).into())
}

fn protocol_error(error: &fm_protocol::StructuredError) -> RemoteFailure {
    RemoteFailure(format!("{}: {}", error.code, error.message))
}

fn result_id(result: &CommandResult) -> &str {
    match result {
        CommandResult::Accepted { id, .. } | CommandResult::Rejected { id, .. } => id,
    }
}

fn implicit_key(payload: CommandPayload) -> RemoteResult<String> {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(format!(
        "remote-cli:{timestamp:032x}:{:08x}:{payload:?}",
        std::process::id()
    ))
}

fn unique_client_id() -> RemoteResult<String> {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(format!(
        "freemix-cli-{}-{timestamp:032x}",
        std::process::id()
    ))
}

#[derive(Debug)]
struct RemoteFailure(String);

impl core::fmt::Display for RemoteFailure {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for RemoteFailure {}
