use std::collections::{BTreeMap, BTreeSet};

use crate::{CapabilityKey, LimitValue, StableId};

/// An adapter or runtime implementation that supplies a capability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Provider {
    pub id: StableId,
    pub version: ProviderVersion,
}

impl Provider {
    #[must_use]
    pub const fn new(id: StableId, version: ProviderVersion) -> Self {
        Self { id, version }
    }
}

/// An opaque, non-empty provider version.
///
/// Versions remain opaque because device firmware and platform SDK versions do
/// not consistently follow semantic versioning.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProviderVersion(String);

impl ProviderVersion {
    /// Creates an opaque provider version.
    ///
    /// # Errors
    ///
    /// Returns [`EmptyVersion`] when `value` contains no visible text.
    pub fn new(value: impl Into<String>) -> Result<Self, EmptyVersion> {
        let value = value.into();
        if value.trim().is_empty() {
            Err(EmptyVersion)
        } else {
            Ok(Self(value))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Returned when a provider version contains no visible text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmptyVersion;

impl std::fmt::Display for EmptyVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("provider version is empty")
    }
}

impl std::error::Error for EmptyVersion {}

/// Runtime health advertised by a capability provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Health {
    Healthy,
    Degraded { reason: String },
    Unhealthy { reason: String },
}

impl Health {
    #[must_use]
    pub const fn is_usable(&self) -> bool {
        !matches!(self, Self::Unhealthy { .. })
    }

    #[must_use]
    pub const fn is_healthy(&self) -> bool {
        matches!(self, Self::Healthy)
    }
}

/// The minimum health a project accepts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HealthRequirement {
    /// Healthy and degraded capabilities may be used.
    #[default]
    Usable,
    /// Only a fully healthy capability may be used.
    Healthy,
}

impl HealthRequirement {
    pub(crate) const fn matches(self, health: &Health) -> bool {
        match self {
            Self::Usable => health.is_usable(),
            Self::Healthy => health.is_healthy(),
        }
    }
}

/// A backend-neutral media or data format.
///
/// Required fields are matched as a subset of a supported descriptor, allowing
/// providers to attach additional detail without invalidating a project.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatDescriptor {
    pub kind: StableId,
    pub fields: BTreeMap<StableId, FormatValue>,
}

impl FormatDescriptor {
    #[must_use]
    pub const fn new(kind: StableId) -> Self {
        Self {
            kind,
            fields: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn with_field(mut self, name: StableId, value: impl Into<FormatValue>) -> Self {
        self.fields.insert(name, value.into());
        self
    }

    #[must_use]
    pub fn supports(&self, required: &Self) -> bool {
        self.kind == required.kind
            && required
                .fields
                .iter()
                .all(|(name, value)| self.fields.get(name) == Some(value))
    }
}

/// A discrete, backend-neutral format field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FormatValue {
    Boolean(bool),
    Signed(i64),
    Unsigned(u64),
    Text(String),
}

impl From<bool> for FormatValue {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

impl From<i64> for FormatValue {
    fn from(value: i64) -> Self {
        Self::Signed(value)
    }
}

impl From<u64> for FormatValue {
    fn from(value: u64) -> Self {
        Self::Unsigned(value)
    }
}

impl From<String> for FormatValue {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for FormatValue {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

/// A portable or native resource storage domain.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MemoryDomain(pub StableId);

/// A provider latency profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LatencyMode {
    pub id: StableId,
    pub nominal_microseconds: Option<u64>,
    pub maximum_microseconds: Option<u64>,
}

impl LatencyMode {
    #[must_use]
    pub const fn new(id: StableId) -> Self {
        Self {
            id,
            nominal_microseconds: None,
            maximum_microseconds: None,
        }
    }
}

/// Whether a capability can be opened concurrently.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExclusivityMode {
    Shared,
    Exclusive,
    Configurable,
}

impl ExclusivityMode {
    pub(crate) fn supports(self, required: Self) -> bool {
        self == required || matches!(self, Self::Configurable)
    }
}

/// Concurrency behavior and the optional resource scope it applies to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Exclusivity {
    pub mode: ExclusivityMode,
    pub scope: Option<StableId>,
}

impl Exclusivity {
    #[must_use]
    pub const fn new(mode: ExclusivityMode) -> Self {
        Self { mode, scope: None }
    }
}

/// One discovered capability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Capability {
    pub key: CapabilityKey,
    pub provider: Provider,
    pub health: Health,
    pub limits: BTreeMap<StableId, LimitValue>,
    pub formats: Vec<FormatDescriptor>,
    pub memory_domains: BTreeSet<MemoryDomain>,
    pub latency_modes: Vec<LatencyMode>,
    pub exclusivity: Exclusivity,
}

impl Capability {
    #[must_use]
    pub const fn new(key: CapabilityKey, provider: Provider) -> Self {
        Self {
            key,
            provider,
            health: Health::Healthy,
            limits: BTreeMap::new(),
            formats: Vec::new(),
            memory_domains: BTreeSet::new(),
            latency_modes: Vec::new(),
            exclusivity: Exclusivity::new(ExclusivityMode::Shared),
        }
    }
}
