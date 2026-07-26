use std::{collections::BTreeMap, error::Error, fmt, path::PathBuf};

use fm_capabilities::{Capability, StableId};
use fm_plugin_api::{
    AbiVersion, AbiVersionRange, CapabilitySet, PluginId, PluginManifest as ApiPluginManifest,
    StatusCode, negotiate_abi,
};

/// Hard limits assigned to one isolated plugin instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceBudget {
    pub memory_bytes: u64,
    pub fuel: u64,
    pub deadline_ms: u64,
}

impl ResourceBudget {
    #[must_use]
    pub const fn new(memory_bytes: u64, fuel: u64, deadline_ms: u64) -> Self {
        Self {
            memory_bytes,
            fuel,
            deadline_ms,
        }
    }

    #[must_use]
    pub const fn fits_within(self, maximum: Self) -> bool {
        self.memory_bytes > 0
            && self.fuel > 0
            && self.deadline_ms > 0
            && self.memory_bytes <= maximum.memory_bytes
            && self.fuel <= maximum.fuel
            && self.deadline_ms <= maximum.deadline_ms
    }

    fn first_violation(self, maximum: Self) -> Option<BudgetLimit> {
        for (resource, requested, allowed) in [
            ("memory", self.memory_bytes, maximum.memory_bytes),
            ("fuel", self.fuel, maximum.fuel),
            ("deadline", self.deadline_ms, maximum.deadline_ms),
        ] {
            if requested == 0 || requested > allowed {
                return Some(BudgetLimit {
                    resource,
                    requested,
                    allowed,
                });
            }
        }
        None
    }
}

/// Host metadata around the portable plugin API manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginManifest {
    pub api: ApiPluginManifest,
    /// Stable provider name used by the capability registry.
    pub provider_id: StableId,
    pub budget: ResourceBudget,
    /// Capabilities supplied by the plugin once its child reports ready.
    pub capabilities: Vec<Capability>,
}

impl PluginManifest {
    #[must_use]
    pub const fn plugin_id(&self) -> PluginId {
        self.api.plugin_id
    }
}

/// Opaque signature bytes supplied by a discovery source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Signature(Vec<u8>);

impl Signature {
    /// Creates non-empty signature bytes.
    ///
    /// # Errors
    ///
    /// Returns [`EmptySignature`] when `bytes` is empty.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, EmptySignature> {
        let bytes = bytes.into();
        if bytes.is_empty() {
            Err(EmptySignature)
        } else {
            Ok(Self(bytes))
        }
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmptySignature;

impl fmt::Display for EmptySignature {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("plugin signature is empty")
    }
}

impl Error for EmptySignature {}

/// A filesystem artifact and its sidecar metadata. The artifact is not loaded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginArtifact {
    pub path: PathBuf,
    pub manifest: PluginManifest,
    pub signature: Signature,
}

impl PluginArtifact {
    pub fn new(path: impl Into<PathBuf>, manifest: PluginManifest, signature: Signature) -> Self {
        Self {
            path: path.into(),
            manifest,
            signature,
        }
    }
}

/// Trust-policy hook used to authenticate an artifact and its manifest.
pub trait SignatureVerifier {
    /// Authenticates the artifact and its sidecar metadata.
    ///
    /// # Errors
    ///
    /// Returns the trust policy's rejection reason.
    fn verify(&self, artifact: &PluginArtifact) -> Result<(), SignatureError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignatureError {
    pub reason: String,
}

impl SignatureError {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl fmt::Display for SignatureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl Error for SignatureError {}

/// Host policy applied before an artifact can enter the catalog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiscoveryPolicy {
    pub host_abi: AbiVersionRange,
    /// Maximum access the host is willing to grant. This remains default-deny.
    pub granted_capabilities: CapabilitySet,
    pub maximum_budget: ResourceBudget,
}

/// An accepted catalog ordered by the portable 128-bit plugin identifier.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Catalog {
    plugins: BTreeMap<(u64, u64), PluginArtifact>,
}

impl Catalog {
    #[must_use]
    pub fn get(&self, id: &PluginId) -> Option<&PluginArtifact> {
        self.plugins.get(&plugin_key(*id))
    }

    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (PluginId, &PluginArtifact)> {
        self.plugins
            .iter()
            .map(|(&(high, low), artifact)| (PluginId::new(high, low), artifact))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Validates portable API metadata without executing or loading artifacts.
    pub fn discover(
        artifacts: impl IntoIterator<Item = PluginArtifact>,
        policy: DiscoveryPolicy,
        verifier: &impl SignatureVerifier,
    ) -> DiscoveryReport {
        let artifacts: Vec<_> = artifacts.into_iter().collect();
        let mut counts = BTreeMap::new();
        for artifact in &artifacts {
            *counts
                .entry(plugin_key(artifact.manifest.plugin_id()))
                .or_insert(0_usize) += 1;
        }

        let mut catalog = Self::default();
        let mut rejections = Vec::new();
        for artifact in artifacts {
            let id = artifact.manifest.plugin_id();
            let key = plugin_key(id);
            let reason = validate_artifact(&artifact, &policy, verifier, counts[&key]);
            if let Some(reason) = reason {
                rejections.push(DiscoveryRejection {
                    id,
                    path: artifact.path,
                    reason,
                });
            } else {
                catalog.plugins.insert(key, artifact);
            }
        }
        rejections.sort_by(|left, right| {
            (plugin_key(left.id), &left.path).cmp(&(plugin_key(right.id), &right.path))
        });
        DiscoveryReport {
            catalog,
            rejections,
        }
    }
}

const fn plugin_key(id: PluginId) -> (u64, u64) {
    (id.high, id.low)
}

fn validate_artifact(
    artifact: &PluginArtifact,
    policy: &DiscoveryPolicy,
    verifier: &impl SignatureVerifier,
    duplicate_count: usize,
) -> Option<DiscoveryRejectionReason> {
    if duplicate_count > 1 {
        return Some(DiscoveryRejectionReason::DuplicateId);
    }
    if let Err(status) = artifact.manifest.api.validate() {
        return Some(DiscoveryRejectionReason::InvalidManifest(status));
    }
    let selected_abi = match negotiate_abi(policy.host_abi, artifact.manifest.api.abi) {
        Ok(version) => version,
        Err(status) => return Some(DiscoveryRejectionReason::UnsupportedApi(status)),
    };
    if let Err(status) = artifact
        .manifest
        .api
        .validate_compatible(policy.host_abi, &policy.granted_capabilities)
    {
        return Some(DiscoveryRejectionReason::CapabilityDenied(status));
    }
    if let Some(limit) = artifact
        .manifest
        .budget
        .first_violation(policy.maximum_budget)
    {
        return Some(DiscoveryRejectionReason::BudgetExceeded(limit));
    }
    let version = artifact.manifest.api.plugin_version;
    let version = format!("{}.{}.{}", version.major, version.minor, version.patch);
    for capability in &artifact.manifest.capabilities {
        if capability.provider.id != artifact.manifest.provider_id {
            return Some(DiscoveryRejectionReason::CapabilityProviderMismatch {
                capability: capability.key.to_string(),
            });
        }
        if capability.provider.version.as_str() != version {
            return Some(DiscoveryRejectionReason::CapabilityVersionMismatch {
                capability: capability.key.to_string(),
            });
        }
    }
    verifier
        .verify(artifact)
        .err()
        .map(|error| DiscoveryRejectionReason::InvalidSignature {
            selected_abi,
            reason: error.reason,
        })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryReport {
    pub catalog: Catalog,
    pub rejections: Vec<DiscoveryRejection>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryRejection {
    pub id: PluginId,
    pub path: PathBuf,
    pub reason: DiscoveryRejectionReason,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BudgetLimit {
    pub resource: &'static str,
    pub requested: u64,
    pub allowed: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiscoveryRejectionReason {
    DuplicateId,
    InvalidManifest(StatusCode),
    InvalidSignature {
        selected_abi: AbiVersion,
        reason: String,
    },
    UnsupportedApi(StatusCode),
    CapabilityDenied(StatusCode),
    BudgetExceeded(BudgetLimit),
    CapabilityProviderMismatch {
        capability: String,
    },
    CapabilityVersionMismatch {
        capability: String,
    },
}
