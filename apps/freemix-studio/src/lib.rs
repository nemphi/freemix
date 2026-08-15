//! Native Studio application, TCP runtime, and optional daemon supervision.

mod args;
mod native;
mod osc;
mod runtime;
mod supervisor;

pub use args::{
    ArgsError, Command, ConnectionConfig, ExistingConfig, HELP, StudioConfig, SupervisedConfig,
    parse_args,
};
pub use native::launch_native;
pub use runtime::{LifecycleState, StudioError, StudioRuntime};
pub use supervisor::{
    DaemonSupervisor, ReadinessParseError, ReadinessRecord, RestartPolicy, SupervisorError,
    SupervisorState,
};

use fm_client::ClientConfig;
use fm_protocol::{ClientType, Role};
use fm_types::ProjectId;

pub const DEFAULT_DAEMON: &str = "freemixd";
pub const DEFAULT_LISTEN: &str = "127.0.0.1:0";

/// Constructs protocol client settings for a native Studio session.
#[must_use]
pub fn native_client_config(
    desired_role: Role,
    client_id: impl Into<String>,
    project_id: ProjectId,
) -> ClientConfig {
    ClientConfig::new(
        concat!("freemix-studio ", env!("CARGO_PKG_VERSION")),
        ClientType::Studio,
        desired_role,
        client_id,
        project_id,
    )
}
