use crate::{ExchangeError, ProtocolLimits};
use core::fmt;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PluginId(String);

impl PluginId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for PluginId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for PluginId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CapabilityId(String);

impl CapabilityId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for CapabilityId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for CapabilityId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for CapabilityId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ApiVersion {
    pub major: u16,
    pub minor: u16,
}

impl ApiVersion {
    #[must_use]
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StateVersion(u32);

impl StateVersion {
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl From<u32> for StateVersion {
    fn from(value: u32) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for StateVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginManifest {
    pub id: PluginId,
    pub api_version: ApiVersion,
    pub state_version: StateVersion,
    pub requested_capabilities: Vec<CapabilityId>,
}

impl PluginManifest {
    #[must_use]
    pub fn new(
        id: impl Into<PluginId>,
        api_version: ApiVersion,
        state_version: StateVersion,
    ) -> Self {
        Self {
            id: id.into(),
            api_version,
            state_version,
            requested_capabilities: Vec::new(),
        }
    }

    #[must_use]
    pub fn requesting(mut self, capability: impl Into<CapabilityId>) -> Self {
        self.requested_capabilities.push(capability.into());
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginState {
    Discovered,
    Validated,
    Loaded,
    Started,
    Stopped,
    Quarantined,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilityDecision {
    Granted,
    Denied,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QuarantineReason {
    Policy(String),
    DeadlineMissed { deadline_millis: u64 },
    Crashed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateSnapshot {
    plugin_id: PluginId,
    version: StateVersion,
    data: Vec<u8>,
}

impl StateSnapshot {
    /// Creates a bounded opaque plugin-state snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`ExchangeError`] if the identifier or state exceeds the
    /// configured protocol limits.
    pub fn new(
        plugin_id: impl Into<PluginId>,
        version: StateVersion,
        data: impl Into<Vec<u8>>,
        limits: &ProtocolLimits,
    ) -> Result<Self, ExchangeError> {
        limits.validate()?;
        let plugin_id = plugin_id.into();
        limits.validate_identifier("plugin_id", plugin_id.as_str())?;
        let data = data.into();
        ProtocolLimits::validate_payload("snapshot", data.len(), limits.max_snapshot_bytes)?;
        Ok(Self {
            plugin_id,
            version,
            data,
        })
    }

    #[must_use]
    pub const fn plugin_id(&self) -> &PluginId {
        &self.plugin_id
    }

    #[must_use]
    pub const fn version(&self) -> StateVersion {
        self.version
    }

    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CrashReport {
    plugin_id: PluginId,
    occurred_at_millis: u64,
    summary: String,
    details: Vec<u8>,
}

impl CrashReport {
    /// Creates a crash report after applying identifier and report bounds.
    ///
    /// # Errors
    ///
    /// Returns [`ExchangeError`] when a configured bound is exceeded.
    pub fn new(
        plugin_id: impl Into<PluginId>,
        occurred_at_millis: u64,
        summary: impl Into<String>,
        details: impl Into<Vec<u8>>,
        limits: &ProtocolLimits,
    ) -> Result<Self, ExchangeError> {
        limits.validate()?;
        let plugin_id = plugin_id.into();
        limits.validate_identifier("plugin_id", plugin_id.as_str())?;
        let summary = summary.into();
        limits.validate_identifier("crash summary", &summary)?;
        let details = details.into();
        ProtocolLimits::validate_payload(
            "crash report",
            details.len(),
            limits.max_crash_report_bytes,
        )?;
        Ok(Self {
            plugin_id,
            occurred_at_millis,
            summary,
            details,
        })
    }

    #[must_use]
    pub const fn plugin_id(&self) -> &PluginId {
        &self.plugin_id
    }

    #[must_use]
    pub const fn occurred_at_millis(&self) -> u64 {
        self.occurred_at_millis
    }

    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    #[must_use]
    pub fn details(&self) -> &[u8] {
        &self.details
    }
}
