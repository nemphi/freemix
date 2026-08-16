use std::{
    env, io,
    process::ExitCode,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use fm_client::{Intake, SessionEvent};
use fm_protocol::DiagnosticsResponse;
use freemix_studio::{Command, HELP, StudioRuntime, launch_native, parse_args};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DIAGNOSE_POLL_INTERVAL: Duration = Duration::from_millis(50);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("freemix-studio: {error}");
            eprintln!("Try 'freemix-studio --help' for usage.");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    match parse_args(env::args().skip(1))? {
        Command::Help => println!("{HELP}"),
        Command::Version => println!("freemix-studio {}", env!("CARGO_PKG_VERSION")),
        Command::Open(config) => launch_native(config)?,
        Command::Diagnose(config) => diagnose(config)?,
    }
    Ok(())
}

fn diagnose(config: freemix_studio::StudioConfig) -> Result<(), io::Error> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let mut runtime = StudioRuntime::new_cancellable(config, DIAGNOSE_POLL_INTERVAL, || {
        Instant::now() >= deadline
    })
    .map_err(diagnostic_failure)?;
    runtime
        .connect_cancellable(
            deadline.saturating_duration_since(Instant::now()),
            DIAGNOSE_POLL_INTERVAL,
            || Instant::now() >= deadline,
        )
        .map_err(diagnostic_failure)?;
    let sent_at_ms: u64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(diagnostic_failure)?
        .as_millis()
        .try_into()
        .map_err(diagnostic_failure)?;
    runtime
        .send_heartbeat_cancellable(sent_at_ms, DIAGNOSE_POLL_INTERVAL, || {
            Instant::now() >= deadline
        })
        .map_err(diagnostic_failure)?;
    loop {
        match runtime
            .receive_cancellable(DIAGNOSE_POLL_INTERVAL, || Instant::now() >= deadline)
            .map_err(diagnostic_failure)?
        {
            SessionEvent::HeartbeatAcknowledged { acknowledgement } => {
                if Instant::now() >= deadline {
                    return Err(diagnostic_failure("deadline exceeded").into());
                }
                println!(
                    "liveness=ok sequence={} received_at_ms={}",
                    acknowledgement.heartbeat_sequence, acknowledgement.received_at_ms
                );
                let request_id = format!(
                    "studio-diagnostics-{}-{}",
                    std::process::id(),
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_err(diagnostic_failure)?
                        .as_nanos()
                );
                let SessionEvent::DiagnosticsResponse { response } = runtime
                    .send_diagnostics_cancellable(request_id, DIAGNOSE_POLL_INTERVAL, || {
                        Instant::now() >= deadline
                    })
                    .map_err(diagnostic_failure)?
                else {
                    return Err(diagnostic_failure("unexpected diagnostics event").into());
                };
                println_diagnostics(&response);
                break;
            }
            SessionEvent::Event {
                intake: Intake::EventApplied,
                ..
            }
            | SessionEvent::RuntimeEvent {
                intake: Intake::RuntimeEventObserved,
                ..
            } => {}
            SessionEvent::Disconnected { .. } => {
                return Err(diagnostic_failure("EOF").into());
            }
            SessionEvent::ServerError(_) => {
                return Err(diagnostic_failure("server error").into());
            }
            _ => return Err(diagnostic_failure("unexpected session event").into()),
        }
    }
    Ok(())
}

fn println_diagnostics(response: &DiagnosticsResponse) {
    println!(
        "diagnostics=v1 engine_id={} state_epoch={} revision={} retained_oldest={} retained_newest={} subscribers={}/{} retained_limit={} subscriber_queue={}",
        sanitize_identity(&response.engine.engine_id),
        response.engine.state_epoch,
        response.current_revision,
        optional_number(response.oldest_retained_revision),
        optional_number(response.newest_retained_revision),
        response.subscriber_count,
        response.subscriber_limit,
        response.retained_events_limit,
        response.subscriber_queue_limit,
    );
}

fn optional_number(value: Option<u64>) -> String {
    value.map_or_else(|| "none".to_owned(), |value| value.to_string())
}

fn sanitize_identity(value: &str) -> String {
    let sanitized = value
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
        .take(96)
        .collect::<String>();
    if sanitized.is_empty() {
        "unknown".to_owned()
    } else {
        sanitized
    }
}

fn diagnostic_failure(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("diagnostic heartbeat failed: {error}"))
}
