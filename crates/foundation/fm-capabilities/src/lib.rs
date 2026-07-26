//! Portable capability discovery and project compatibility matching.
//!
//! This crate intentionally owns no backend-specific types. Adapters translate
//! their native formats and resource domains into these small descriptors.

mod descriptor;
mod key;
mod matching;
mod registry;
mod value;

pub use descriptor::{
    Capability, EmptyVersion, Exclusivity, ExclusivityMode, FormatDescriptor, FormatValue, Health,
    HealthRequirement, LatencyMode, MemoryDomain, Provider, ProviderVersion,
};
pub use key::{CapabilityKey, InvalidKey, StableId};
pub use matching::{
    CapabilityRequirement, CompatibilityIssue, CompatibilityReport, FormatMismatch,
    LatencyMismatch, LimitMismatch, LimitMismatchKind, MemoryDomainMismatch, ProviderRequirement,
    RequirementReport,
};
pub use registry::{CapabilityRegistry, DuplicateCapability};
pub use value::{LimitComparison, LimitConstraint, LimitValue, QuantityUnit, ValueKind};

/// Explicit name for a discovered capability record.
pub type CapabilityRecord = Capability;
