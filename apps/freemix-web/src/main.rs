#[cfg(not(target_arch = "wasm32"))]
fn main() {
    const HELP: &str = "\
FreeMix web control skeleton

Usage:
  freemix-web [--help]
  freemix-web --version

This Phase 1 binary defines the control model only; it does not start a browser or network runtime.";

    let mut arguments = std::env::args().skip(1);
    match (arguments.next().as_deref(), arguments.next()) {
        (None | Some("--help" | "-h" | "help"), None) => println!("{HELP}"),
        (Some("--version" | "-V" | "version"), None) => {
            println!("freemix-web {}", env!("CARGO_PKG_VERSION"));
        }
        _ => {
            eprintln!("invalid arguments\n\n{HELP}");
            std::process::exit(2);
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {}
