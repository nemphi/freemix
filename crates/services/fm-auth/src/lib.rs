//! Dependency-free authentication and authorization primitives.

mod id;
mod pairing;
mod policy;

pub use id::{InvalidIdentity, SessionId, UserId};
pub use pairing::{
    InvalidPairingCode, MAX_PAIRING_LIFETIME_SECONDS, PairingCode, PairingCodeValidator,
    PairingConfigurationError, PairingError,
};
pub use policy::{
    AuthorizationDenial, CommandClass, DenialReason, Permission, Policy, Principal, PrincipalKind,
    Role,
};
