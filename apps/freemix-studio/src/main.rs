use std::{
    env, io,
    process::ExitCode,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use fm_client::{Intake, SessionEvent};
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
        Command::Diagnose(config) => {
            let deadline = Instant::now() + CONNECT_TIMEOUT;
            let mut runtime =
                StudioRuntime::new_cancellable(config, DIAGNOSE_POLL_INTERVAL, || {
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
        }
    }
    Ok(())
}

fn diagnostic_failure(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("diagnostic heartbeat failed: {error}"))
}
