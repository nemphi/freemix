//! Per-user-session capture broker state and bounded platform diagnostics.

pub mod args;
mod audio_inputs;
mod broker;
mod cameras;

pub use audio_inputs::{AudioDiagnosticError, audio_diagnostics, audio_smoke};
pub use broker::{
    BackoffPolicy, BackoffPolicyError, BrokerError, CaptureBroker, CaptureSource, ConnectionState,
    LogoutSummary, MediaKind, PairingError, PairingState, PermissionKind, PermissionState,
    PermissionStatus, PermissionTransition, PermissionUpdate, Publication, PublicationId,
    PublicationRegistry, ReconnectRecord, ReconnectTracker, RegistryError, SessionState,
    TimedMediaDescriptor,
};
pub use cameras::{CameraDiagnosticError, camera_diagnostics, camera_smoke};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const HELP: &str = "freemix-capture-node - per-user-session capture broker

Usage:
  freemix-capture-node cameras [--request-permission] [--helper <PATH>]
  freemix-capture-node camera-smoke --source-index <INDEX> [OPTIONS]
  freemix-capture-node audio-inputs [--request-permission] [--helper <PATH>]
  freemix-capture-node audio-smoke --stable-key <KEY> --sample-rate <HZ> --channels <COUNT> [OPTIONS]
  freemix-capture-node serve --session-id <ID> --endpoint <ADDRESS> [OPTIONS]
  freemix-capture-node help
  freemix-capture-node version

Camera options:
  --request-permission           Explicitly request camera access interactively
  --helper <PATH>                Packaged AVFoundation helper override

Camera smoke options:
  --source-index <INDEX>         Required index from the cameras report
  --format-index <INDEX>         Advertised format index (default: 0)
  --frames <COUNT>               Frames to acquire (default: 30, maximum: 300)
  --timeout-ms <MS>              Frame deadline (default: 10000, range: 1000-60000)
  --helper <PATH>                Packaged AVFoundation helper override

Audio input options:
  --request-permission           Explicitly request microphone access interactively
  --helper <PATH>                Packaged AVFoundation helper override

Audio smoke options:
  --stable-key <KEY>             Required exact key from the audio-inputs report
  --sample-rate <HZ>             Required exact advertised sample rate
  --channels <COUNT>             Required exact advertised channel count
  --blocks <COUNT>               Blocks to acquire (default: 100, maximum: 1000)
  --timeout-ms <MS>              Block deadline (default: 10000, range: 1000-60000)
  --helper <PATH>                Packaged AVFoundation helper override

Serve options:
  --session-id <ID>             Logged-in user-session identity
  --endpoint <ADDRESS>          Authenticated local IPC endpoint
  --max-publications <COUNT>    Publication limit (default: 32, maximum: 256)
  --initial-backoff-ms <MS>     Initial reconnect delay (default: 250)
  --max-backoff-ms <MS>         Maximum reconnect delay (default: 10000)

The built-in helper never prompts unless --request-permission is present on the
matching discovery command. Smoke commands never request permission and are
diagnostic-only. An overridden --helper is trusted application packaging. Serve
still models broker state and does not yet publish captured media or open IPC.";
