use std::process::ExitCode;

use freemix_capture_node::{
    HELP, VERSION,
    args::{self, Command},
    audio_diagnostics, audio_smoke, camera_diagnostics, camera_smoke,
};

fn main() -> ExitCode {
    let command = match args::parse(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("error: {error}");
            eprintln!("run `freemix-capture-node help` for usage");
            return ExitCode::from(2);
        }
    };

    match command {
        Command::Help => println!("{HELP}"),
        Command::Version => println!("freemix-capture-node {VERSION}"),
        Command::Cameras(config) => match camera_diagnostics(&config) {
            Ok(report) => print!("{report}"),
            Err(error) => {
                eprintln!("error: {error}");
                return ExitCode::FAILURE;
            }
        },
        Command::CameraSmoke(config) => match camera_smoke(&config) {
            Ok(report) => println!("{report}"),
            Err(error) => {
                eprintln!("error: {error}");
                return ExitCode::FAILURE;
            }
        },
        Command::AudioInputs(config) => match audio_diagnostics(&config) {
            Ok(report) => print!("{report}"),
            Err(error) => {
                eprintln!("error: {error}");
                return ExitCode::FAILURE;
            }
        },
        Command::AudioSmoke(config) => match audio_smoke(&config) {
            Ok(report) => println!("{report}"),
            Err(error) => {
                eprintln!("error: {error}");
                return ExitCode::FAILURE;
            }
        },
        Command::Serve(config) => println!(
            "capture broker configured for session `{}` at `{}` (publication limit {})",
            config.session_id, config.endpoint, config.max_publications
        ),
    }
    ExitCode::SUCCESS
}
