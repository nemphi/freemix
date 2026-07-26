//! Transport-independent server sessions and resume negotiation.

mod config;
mod control;
mod server;
mod session;
mod status;

pub use config::{
    AuthenticationMode, ConfigError, RateLimit, ServerConfig, ServerMode, SessionLimits,
};
pub use control::{ControlPlane, InitialSync, SyncPayload};
pub use server::{HandshakeError, HandshakeOutcome, Server};
pub use session::{
    DisconnectReason, Heartbeat, HeartbeatState, Session, SessionAccounting, SessionError,
    SessionState,
};
pub use status::{HealthState, ReadinessState, ServiceStatus, StatusTransitionError};
