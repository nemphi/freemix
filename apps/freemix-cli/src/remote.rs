use std::{
    error::Error,
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpStream},
    num::NonZeroU128,
    time::{SystemTime, UNIX_EPOCH},
};

use fm_client::{Client, ClientConfig, Intake, Outbound};
use fm_protocol::{
    CURRENT_PROTOCOL_VERSION, CapabilityReportSummary, ClientHello, ClientType, CommandPayload,
    CommandResult, EventPayload, HandshakeOutcome, HandshakeResponse, Role, RuntimeLifecycleEvent,
    ServerHello, ServerIdentity, SnapshotReason, WireMessage, decode_line, encode_line,
};
use fm_types::ProjectId;

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
        remote.write(&WireMessage::ClientHello(ClientHello {
            versions: vec![CURRENT_PROTOCOL_VERSION],
            build: format!("freemix-cli-{}", env!("CARGO_PKG_VERSION")),
            client_type: ClientType::Cli,
            desired_role: Role::Operator,
            cached_cursor: None,
        }))?;

        let hello = match remote.read()? {
            WireMessage::ServerHello(hello) => hello,
            WireMessage::Error(error) => return Err(protocol_error(&error.error).into()),
            _ => return Err(RemoteFailure("expected server_hello during handshake".into()).into()),
        };
        if hello.resume {
            return Err(
                RemoteFailure("daemon selected resume without a client cursor".into()).into(),
            );
        }

        let project_id = project_id(&hello)?;
        let mut client = Client::new(ClientConfig::new(
            vec![CURRENT_PROTOCOL_VERSION],
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
        client.accept_handshake(adapt_hello(&hello, project_id))?;
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
                vec![CURRENT_PROTOCOL_VERSION],
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
            let realized = matches!(
                &runtime.event,
                RuntimeLifecycleEvent::Realized { domain } if domain == "switcher"
            );
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
            "project_id={} show={:?} revision={} frame=unavailable Program(desired={}, realized={}) Preview(desired={}, realized={})",
            self.project_id,
            state.show_name(),
            cursor.revision,
            switcher.desired.program,
            switcher.realized.program,
            switcher.desired.preview,
            switcher.realized.preview,
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

fn adapt_hello(hello: &ServerHello, project_id: ProjectId) -> HandshakeResponse {
    HandshakeResponse {
        negotiated: hello.negotiated,
        granted_role: hello.granted_role,
        permissions: hello.permissions.clone(),
        capabilities: CapabilityReportSummary {
            digest: hello.capabilities_digest.clone(),
            total: 0,
            available: 0,
            degraded: 0,
            unavailable: 0,
        },
        server: ServerIdentity {
            engine_id: hello.engine.engine_id.clone(),
            project_id: project_id.to_string(),
            state_epoch: hello.engine.state_epoch,
            log_id: hello.engine.log_id.clone(),
        },
        current_revision: hello.current_revision,
        outcome: HandshakeOutcome::Snapshot {
            reason: SnapshotReason::NoCursor,
        },
    }
}

fn project_id(hello: &ServerHello) -> RemoteResult<ProjectId> {
    let value = hello
        .engine
        .engine_id
        .strip_prefix("project-")
        .ok_or_else(|| RemoteFailure("daemon engine identity has no project ID".into()))?
        .parse::<u128>()?;
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
