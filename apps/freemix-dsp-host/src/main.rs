use std::io;
use std::process::ExitCode;

use freemix_dsp_host::{CliAction, HELP, VERSION, parse_args, run_control_loop};

fn main() -> ExitCode {
    let action = match parse_args(std::env::args().skip(1)) {
        Ok(action) => action,
        Err(error) => {
            eprintln!("error: {error}");
            eprintln!("run `freemix-dsp-host --help` for usage");
            return ExitCode::from(2);
        }
    };

    match action {
        CliAction::Help => println!("{HELP}"),
        CliAction::Version => println!("freemix-dsp-host {VERSION}"),
        CliAction::Run(config) => {
            let stdin = io::stdin();
            let stdout = io::stdout();
            if let Err(error) = run_control_loop(config, stdin.lock(), stdout.lock()) {
                eprintln!("error: control channel failed: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}
