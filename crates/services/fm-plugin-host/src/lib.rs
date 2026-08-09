//! Plugin discovery, isolation policy, IPC, and crash handling.
//!
//! This crate supervises already-isolated child processes. It deliberately does
//! not load native libraries or provide a WebAssembly runtime.

mod discovery;
mod ipc;
mod supervisor;

pub use discovery::{
    BudgetLimit, Catalog, DiscoveryPolicy, DiscoveryRejection, DiscoveryRejectionReason,
    DiscoveryReport, EmptySignature, PluginArtifact, PluginManifest, ResourceBudget, Signature,
    SignatureError, SignatureVerifier,
};
pub use fm_capabilities::{Capability, CapabilityRegistry};
pub use fm_plugin_api::{AbiVersion, CapabilityGrant, CapabilitySet, PluginId, StatusCode};
pub use ipc::{BoundedIpcQueue, IpcLimits, IpcMessage, QueueError};
pub use supervisor::{
    AuditEvent, AuditRecord, BudgetResource, ChildController, ChildError, ChildEvent, ChildId,
    Failure, InstanceState, PluginStatus, ResourceUsage, Supervisor, SupervisorError,
    SupervisorPolicy,
};

#[cfg(test)]
mod tests;
