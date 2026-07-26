use std::{env, process::ExitCode, time::Duration};

use freemix_studio::{Command, HELP, StudioRuntime, launch_native, parse_args};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

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
            let mut runtime = StudioRuntime::new(config)?;
            println!(
                "state={:?} address={}",
                runtime.lifecycle()?,
                runtime.address()
            );
            let connected = runtime.connect(CONNECT_TIMEOUT)?;
            println!("event={connected:?} state={:?}", runtime.lifecycle()?);
        }
    }
    Ok(())
}
