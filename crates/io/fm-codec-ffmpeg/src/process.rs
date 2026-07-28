use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::{Error, LimitKind, Tool, UnavailableReason};

#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
use std::path::{Path, PathBuf};

const POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Clone, Copy)]
pub(crate) struct RunRequest<'a> {
    pub executable: &'a OsStr,
    pub tool: Tool,
    pub args: &'a [OsString],
    pub env: &'a [(OsString, OsString)],
    pub timeout: Duration,
    pub kill_timeout: Duration,
    pub max_stdout: usize,
    pub max_stderr: usize,
    pub redactions: &'a [&'a str],
}

pub(crate) struct RunOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[cfg(test)]
#[derive(Clone)]
struct TestCommandShim {
    scenario: String,
    state: PathBuf,
}

#[cfg(test)]
thread_local! {
    static TEST_COMMAND_SHIM: RefCell<Option<TestCommandShim>> = const { RefCell::new(None) };
}

enum StreamMessage {
    Stdout(Result<Vec<u8>, DrainError>),
    Stderr(Result<Vec<u8>, DrainError>),
}

enum DrainError {
    Overflow,
    Io(io::ErrorKind),
}

/// Runs one direct child. Descendants that deliberately retain inherited pipes
/// are outside the isolation boundary; pipe completion remains timeout-bounded.
pub(crate) fn run(request: RunRequest<'_>) -> Result<RunOutput, Error> {
    let mut command = command_for(&request);
    let mut child = command
        .spawn()
        .map_err(|error| spawn_error(request.tool, &error))?;
    let stdout = child.stdout.take().ok_or(Error::ProcessIo {
        tool: request.tool,
        kind: io::ErrorKind::BrokenPipe,
    })?;
    let stderr = child.stderr.take().ok_or(Error::ProcessIo {
        tool: request.tool,
        kind: io::ErrorKind::BrokenPipe,
    })?;
    let (sender, receiver) = mpsc::channel();
    #[cfg(test)]
    let max_stdout = if TEST_COMMAND_SHIM.with(|slot| slot.borrow().is_some()) {
        request.max_stdout.saturating_add(4 * 1_024)
    } else {
        request.max_stdout
    };
    #[cfg(not(test))]
    let max_stdout = request.max_stdout;
    drain(stdout, max_stdout, sender.clone(), true);
    drain(stderr, request.max_stderr, sender, false);

    let deadline = Instant::now() + request.timeout;
    let mut status = None;
    let mut stdout = None;
    let mut stderr = None;
    loop {
        receive_streams(
            &receiver,
            &mut stdout,
            &mut stderr,
            request.tool,
            &mut child,
            request.kill_timeout,
        )?;
        if status.is_none() {
            status = child.try_wait().map_err(|error| Error::ProcessIo {
                tool: request.tool,
                kind: error.kind(),
            })?;
        }
        if let (Some(status), Some(stdout), Some(stderr)) = (status, stdout.take(), stderr.take()) {
            return finish(status, stdout, &stderr, &request);
        }
        if Instant::now() >= deadline {
            terminate(&mut child, request.kill_timeout);
            return Err(Error::ProcessTimedOut { tool: request.tool });
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn command_for(request: &RunRequest<'_>) -> Command {
    #[cfg(test)]
    if let Some(shim) = TEST_COMMAND_SHIM.with(|slot| slot.borrow().clone()) {
        let mut command = Command::new(request.executable);
        let args = request
            .args
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\u{1f}");
        command
            .args([
                "--ignored",
                "--exact",
                "process::tests::runner_helper",
                "--nocapture",
            ])
            .env("FM_RUNNER_HELPER", "audio-seek-stack")
            .env("FM_RUNNER_SCENARIO", shim.scenario)
            .env("FM_RUNNER_STATE", shim.state)
            .env("FM_RUNNER_TOOL", request.tool.to_string())
            .env("FM_RUNNER_ARGS", args)
            .env(
                "FM_RUNNER_TIMEOUT_NANOS",
                request.timeout.as_nanos().to_string(),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("LC_ALL", "C");
        return command;
    }

    let mut command = Command::new(request.executable);
    command
        .args(request.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("LC_ALL", "C");
    for (name, value) in request.env {
        command.env(name, value);
    }
    command
}

#[cfg(test)]
pub(crate) fn with_test_command_shim<T>(
    scenario: &str,
    state: &Path,
    operation: impl FnOnce() -> T,
) -> T {
    struct Reset;

    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_COMMAND_SHIM.with(|slot| {
                slot.replace(None);
            });
        }
    }

    TEST_COMMAND_SHIM.with(|slot| {
        assert!(
            slot.replace(Some(TestCommandShim {
                scenario: scenario.to_owned(),
                state: state.to_owned(),
            }))
            .is_none(),
            "test command shims cannot be nested"
        );
    });
    let reset = Reset;
    let result = operation();
    drop(reset);
    result
}

fn drain(
    mut stream: impl Read + Send + 'static,
    maximum: usize,
    sender: Sender<StreamMessage>,
    stdout: bool,
) {
    thread::spawn(move || {
        let result = drain_bounded(&mut stream, maximum);
        let message = if stdout {
            StreamMessage::Stdout(result)
        } else {
            StreamMessage::Stderr(result)
        };
        let _ = sender.send(message);
    });
}

fn drain_bounded(reader: &mut impl Read, maximum: usize) -> Result<Vec<u8>, DrainError> {
    let mut output = Vec::with_capacity(maximum.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| DrainError::Io(error.kind()))?;
        if count == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(count) > maximum {
            return Err(DrainError::Overflow);
        }
        output.extend_from_slice(&buffer[..count]);
    }
}

fn receive_streams(
    receiver: &Receiver<StreamMessage>,
    stdout: &mut Option<Vec<u8>>,
    stderr: &mut Option<Vec<u8>>,
    tool: Tool,
    child: &mut std::process::Child,
    kill_timeout: Duration,
) -> Result<(), Error> {
    while let Ok(message) = receiver.try_recv() {
        match message {
            StreamMessage::Stdout(Ok(bytes)) => *stdout = Some(bytes),
            StreamMessage::Stderr(Ok(bytes)) => *stderr = Some(bytes),
            StreamMessage::Stdout(Err(DrainError::Overflow)) => {
                terminate(child, kill_timeout);
                return Err(Error::ProcessOutputOverflow {
                    tool,
                    kind: LimitKind::Stdout,
                });
            }
            StreamMessage::Stderr(Err(DrainError::Overflow)) => {
                terminate(child, kill_timeout);
                return Err(Error::ProcessOutputOverflow {
                    tool,
                    kind: LimitKind::Stderr,
                });
            }
            StreamMessage::Stdout(Err(DrainError::Io(kind)))
            | StreamMessage::Stderr(Err(DrainError::Io(kind))) => {
                terminate(child, kill_timeout);
                return Err(Error::ProcessIo { tool, kind });
            }
        }
    }
    Ok(())
}

fn finish(
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: &[u8],
    request: &RunRequest<'_>,
) -> Result<RunOutput, Error> {
    if status.success() {
        #[cfg(test)]
        let stdout = if TEST_COMMAND_SHIM.with(|slot| slot.borrow().is_some()) {
            test_command_payload(stdout)
        } else {
            stdout
        };
        Ok(RunOutput {
            stdout,
            stderr: stderr.to_vec(),
        })
    } else {
        Err(Error::ProcessFailed {
            tool: request.tool,
            status: status.code(),
            stderr: sanitize_stderr(stderr, request.redactions, request.max_stderr),
        })
    }
}

#[cfg(test)]
fn test_command_payload(mut stdout: Vec<u8>) -> Vec<u8> {
    const PAYLOAD_MARKER: &[u8] = b"FM_TEST_PAYLOAD\0";
    let start = stdout
        .windows(PAYLOAD_MARKER.len())
        .position(|window| window == PAYLOAD_MARKER)
        .expect("test command shim payload marker")
        + PAYLOAD_MARKER.len();
    stdout.drain(..start);
    stdout
}

fn terminate(child: &mut std::process::Child, timeout: Duration) {
    let _ = child.kill();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => thread::sleep(POLL_INTERVAL),
        }
    }
}

fn spawn_error(tool: Tool, error: &io::Error) -> Error {
    match error.kind() {
        io::ErrorKind::NotFound => Error::ToolUnavailable {
            tool,
            reason: UnavailableReason::Missing,
        },
        io::ErrorKind::PermissionDenied => Error::ToolUnavailable {
            tool,
            reason: UnavailableReason::PermissionDenied,
        },
        kind => Error::ProcessIo { tool, kind },
    }
}

pub(crate) fn sanitize_stderr(bytes: &[u8], redactions: &[&str], maximum: usize) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    for secret in redactions.iter().filter(|secret| !secret.is_empty()) {
        text = text.replace(secret, "<input>");
    }
    text.retain(|character| character == '\n' || character == '\t' || !character.is_control());
    if text.len() > maximum {
        let mut end = maximum;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::time::Duration;

    use super::*;

    #[test]
    fn redaction_removes_paths_and_controls() {
        let result = sanitize_stderr(b"bad /private/movie.nut\0\n", &["/private/movie.nut"], 64);
        assert_eq!(result, "bad <input>");
    }

    #[test]
    fn runner_times_out_and_bounds_stdout() {
        let executable = std::env::current_exe().expect("test executable");
        let base_args = [
            OsString::from("--ignored"),
            OsString::from("--exact"),
            OsString::from("process::tests::runner_helper"),
            OsString::from("--nocapture"),
        ];
        let timeout_env = [(OsString::from("FM_RUNNER_HELPER"), OsString::from("sleep"))];
        let timeout = run(RunRequest {
            executable: executable.as_os_str(),
            tool: Tool::Ffmpeg,
            args: &base_args,
            env: &timeout_env,
            timeout: Duration::from_millis(50),
            kill_timeout: Duration::from_secs(1),
            max_stdout: 64,
            max_stderr: 1024,
            redactions: &[],
        });
        assert!(matches!(timeout, Err(Error::ProcessTimedOut { .. })));

        let overflow_env = [(
            OsString::from("FM_RUNNER_HELPER"),
            OsString::from("overflow"),
        )];
        let overflow = run(RunRequest {
            executable: executable.as_os_str(),
            tool: Tool::Ffmpeg,
            args: &base_args,
            env: &overflow_env,
            timeout: Duration::from_secs(2),
            kill_timeout: Duration::from_secs(1),
            max_stdout: 64,
            max_stderr: 1024,
            redactions: &[],
        });
        assert!(matches!(
            overflow,
            Err(Error::ProcessOutputOverflow {
                kind: LimitKind::Stdout,
                ..
            })
        ));
    }

    #[test]
    #[ignore = "subprocess helper"]
    fn runner_helper() {
        match std::env::var("FM_RUNNER_HELPER").as_deref() {
            Ok("sleep") => std::thread::sleep(Duration::from_secs(5)),
            Ok("overflow") => {
                std::io::stdout().write_all(&[b'x'; 4096]).unwrap();
                std::io::stdout().flush().unwrap();
            }
            Ok("audio-seek-stack") => audio_seek_stack_helper(),
            _ => {}
        }
    }

    fn audio_seek_stack_helper() {
        let scenario = std::env::var("FM_RUNNER_SCENARIO").unwrap();
        let state = PathBuf::from(std::env::var_os("FM_RUNNER_STATE").unwrap());
        let tool = std::env::var("FM_RUNNER_TOOL").unwrap();
        let args = std::env::var("FM_RUNNER_ARGS")
            .unwrap()
            .split('\u{1f}')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let timeout_nanos = std::env::var("FM_RUNNER_TIMEOUT_NANOS").unwrap();
        let previous = fs::read_to_string(&state).unwrap_or_default();
        let decode_attempt = previous
            .lines()
            .filter(|line| line.contains(" kind=decode "))
            .count();
        let kind = if args.iter().any(|argument| argument == "-show_format") {
            "probe"
        } else if args.iter().any(|argument| argument == "-show_frames") {
            "frames"
        } else {
            "decode"
        };
        let seek = args
            .windows(2)
            .find(|pair| pair[0] == "-ss")
            .map_or("none", |pair| pair[1].as_str());
        writeln!(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&state)
                .unwrap(),
            "tool={tool} kind={kind} timeout_nanos={timeout_nanos} seek={seek}"
        )
        .unwrap();

        match kind {
            "probe" => {
                std::io::stdout().write_all(b"FM_TEST_PAYLOAD\0").unwrap();
                print!(
                    r#"{{"format":{{"format_name":"nut","start_time":"0.000000"}},"streams":[{{"index":0,"codec_type":"audio","codec_name":"flac","time_base":"1/48000","sample_rate":"48000","channels":2,"channel_layout":"stereo","disposition":{{"default":1,"attached_pic":0}}}}]}}"#
                );
            }
            "frames" => {
                let packet_budget = args
                    .iter()
                    .find_map(|argument| argument.strip_prefix("%+#"))
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                let frames = (0..32)
                    .map(|index| {
                        format!(
                            r#"{{"stream_index":0,"best_effort_timestamp":{},"duration":1024,"nb_samples":1024}}"#,
                            index * 1_024
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                std::io::stdout().write_all(b"FM_TEST_PAYLOAD\0").unwrap();
                print!(
                    r#"{{"frames":[{frames}],"streams":[{{"index":0,"nb_read_packets":"{packet_budget}"}}]}}"#
                );
            }
            "decode" if scenario == "anchor-retry" => {
                assert_ne!(
                    seek, "none",
                    "bounded caller limits must select seek anchors"
                );
                std::io::stdout().write_all(b"FM_TEST_PAYLOAD\0").unwrap();
                std::io::stdout().write_all(&[0; 8_192]).unwrap();
                if decode_attempt == 0 {
                    std::thread::sleep(Duration::from_millis(75));
                    eprintln!("[Parsed_ashowinfo_0 @ test] n:0 pts:-1 rate:48000 nb_samples:1024");
                } else {
                    eprintln!(
                        "[Parsed_ashowinfo_0 @ test] n:0 pts:9216 rate:48000 nb_samples:1024"
                    );
                }
            }
            "decode" if scenario == "from-start-failure" && decode_attempt == 0 => {
                assert_eq!(seek, "none");
                eprintln!("from-start sentinel");
                std::process::exit(23);
            }
            "decode" if scenario == "from-start-failure" => {
                assert_eq!(seek, "none");
                std::io::stdout().write_all(b"FM_TEST_PAYLOAD\0").unwrap();
                std::io::stdout().write_all(&[0; 8_192]).unwrap();
            }
            _ => panic!("unexpected audio seek shim request"),
        }
        std::io::stdout().flush().unwrap();
        std::io::stderr().flush().unwrap();
        std::process::exit(0);
    }
}
