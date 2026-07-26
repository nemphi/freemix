use std::env;
use std::io;
use std::process::ExitCode;

use freemix_plugin_host::{Action, HELP, Lifecycle, VERSION, parse_args, run_control_loop};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("freemix-plugin-host: {error}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    match parse_args(env::args().skip(1))? {
        Action::Help => println!("{HELP}"),
        Action::Version => println!("freemix-plugin-host {VERSION}"),
        Action::Run(_config) => {
            let stdin = io::stdin();
            let stdout = io::stdout();
            let mut lifecycle = Lifecycle::new();
            run_control_loop(stdin.lock(), stdout.lock(), &mut lifecycle)?;
        }
    }
    Ok(())
}
