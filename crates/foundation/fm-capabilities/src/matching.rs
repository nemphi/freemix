use std::collections::{BTreeMap, BTreeSet};

use crate::{
    Capability, CapabilityKey, CapabilityRegistry, ExclusivityMode, FormatDescriptor, Health,
    HealthRequirement, LimitConstraint, LimitValue, MemoryDomain, ProviderVersion, StableId,
    ValueKind,
};

/// An optional exact provider constraint.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProviderRequirement {
    pub id: Option<StableId>,
    pub version: Option<ProviderVersion>,
}

/// Project-side requirements for one capability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityRequirement {
    pub key: CapabilityKey,
    pub health: HealthRequirement,
    pub provider: ProviderRequirement,
    pub limits: BTreeMap<StableId, LimitConstraint>,
    /// Every listed format must be supported by at least one advertised format.
    pub formats: Vec<FormatDescriptor>,
    /// Every listed memory domain must be advertised.
    pub memory_domains: BTreeSet<MemoryDomain>,
    /// At least one listed latency mode must be advertised. Empty accepts any.
    pub latency_modes: BTreeSet<StableId>,
    /// A required concurrency mode. Configurable providers satisfy either mode.
    pub exclusivity: Option<ExclusivityMode>,
}

impl CapabilityRequirement {
    #[must_use]
    pub const fn new(key: CapabilityKey) -> Self {
        Self {
            key,
            health: HealthRequirement::Usable,
            provider: ProviderRequirement {
                id: None,
                version: None,
            },
            limits: BTreeMap::new(),
            formats: Vec::new(),
            memory_domains: BTreeSet::new(),
            latency_modes: BTreeSet::new(),
            exclusivity: None,
        }
    }
}

/// Full project compatibility result, preserving project requirement order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompatibilityReport {
    pub requirements: Vec<RequirementReport>,
}

impl CompatibilityReport {
    #[must_use]
    pub fn evaluate(registry: &CapabilityRegistry, requirements: &[CapabilityRequirement]) -> Self {
        Self {
            requirements: requirements
                .iter()
                .map(|requirement| RequirementReport::evaluate(registry, requirement))
                .collect(),
        }
    }

    #[must_use]
    pub fn is_compatible(&self) -> bool {
        self.requirements.iter().all(RequirementReport::is_met)
    }

    pub fn issues(&self) -> impl Iterator<Item = &CompatibilityIssue> {
        self.requirements
            .iter()
            .flat_map(|requirement| requirement.issues.iter())
    }
}

/// Match result for one project requirement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequirementReport {
    pub key: CapabilityKey,
    pub issues: Vec<CompatibilityIssue>,
}

impl RequirementReport {
    #[must_use]
    pub fn is_met(&self) -> bool {
        self.issues.is_empty()
    }

    fn evaluate(registry: &CapabilityRegistry, requirement: &CapabilityRequirement) -> Self {
        let Some(capability) = registry.get(&requirement.key) else {
            return Self {
                key: requirement.key.clone(),
                issues: vec![CompatibilityIssue::MissingCapability],
            };
        };

        let mut issues = Vec::new();
        if !requirement.health.matches(&capability.health) {
            issues.push(CompatibilityIssue::Unhealthy {
                required: requirement.health,
                actual: capability.health.clone(),
            });
        }
        if requirement
            .provider
            .id
            .as_ref()
            .is_some_and(|id| id != &capability.provider.id)
            || requirement
                .provider
                .version
                .as_ref()
                .is_some_and(|version| version != &capability.provider.version)
        {
            issues.push(CompatibilityIssue::ProviderMismatch {
                required: requirement.provider.clone(),
                actual_id: capability.provider.id.clone(),
                actual_version: capability.provider.version.clone(),
            });
        }

        append_limit_issues(&mut issues, capability, requirement);
        append_format_issues(&mut issues, capability, requirement);
        append_resource_issues(&mut issues, capability, requirement);

        Self {
            key: requirement.key.clone(),
            issues,
        }
    }
}

fn append_limit_issues(
    issues: &mut Vec<CompatibilityIssue>,
    capability: &Capability,
    requirement: &CapabilityRequirement,
) {
    for (name, constraint) in &requirement.limits {
        let mismatch = match capability.limits.get(name) {
            None => Some(LimitMismatch {
                name: name.clone(),
                required: constraint.clone(),
                actual: None,
                kind: LimitMismatchKind::Missing,
            }),
            Some(actual) => match constraint.matches(actual) {
                Some(true) => None,
                Some(false) => Some(LimitMismatch {
                    name: name.clone(),
                    required: constraint.clone(),
                    actual: Some(actual.clone()),
                    kind: LimitMismatchKind::Unsatisfied,
                }),
                None => Some(LimitMismatch {
                    name: name.clone(),
                    required: constraint.clone(),
                    actual: Some(actual.clone()),
                    kind: LimitMismatchKind::Incomparable {
                        required: constraint.value.kind(),
                        actual: actual.kind(),
                    },
                }),
            },
        };
        if let Some(mismatch) = mismatch {
            issues.push(CompatibilityIssue::LimitMismatch(mismatch));
        }
    }
}

fn append_format_issues(
    issues: &mut Vec<CompatibilityIssue>,
    capability: &Capability,
    requirement: &CapabilityRequirement,
) {
    for required in &requirement.formats {
        if !capability
            .formats
            .iter()
            .any(|supported| supported.supports(required))
        {
            issues.push(CompatibilityIssue::FormatMismatch(FormatMismatch {
                required: required.clone(),
                supported: capability.formats.clone(),
            }));
        }
    }
}

fn append_resource_issues(
    issues: &mut Vec<CompatibilityIssue>,
    capability: &Capability,
    requirement: &CapabilityRequirement,
) {
    let missing_domains = requirement
        .memory_domains
        .difference(&capability.memory_domains)
        .cloned()
        .collect::<BTreeSet<_>>();
    if !missing_domains.is_empty() {
        issues.push(CompatibilityIssue::MemoryDomainMismatch(
            MemoryDomainMismatch {
                missing: missing_domains,
                supported: capability.memory_domains.clone(),
            },
        ));
    }

    if !requirement.latency_modes.is_empty() {
        let supported = capability
            .latency_modes
            .iter()
            .map(|mode| mode.id.clone())
            .collect::<BTreeSet<_>>();
        if requirement.latency_modes.is_disjoint(&supported) {
            issues.push(CompatibilityIssue::LatencyMismatch(LatencyMismatch {
                accepted: requirement.latency_modes.clone(),
                supported,
            }));
        }
    }

    if let Some(required) = requirement.exclusivity
        && !capability.exclusivity.mode.supports(required)
    {
        issues.push(CompatibilityIssue::ExclusivityMismatch {
            required,
            actual: capability.exclusivity.mode,
        });
    }
}

/// One structured reason a requirement cannot be activated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompatibilityIssue {
    MissingCapability,
    Unhealthy {
        required: HealthRequirement,
        actual: Health,
    },
    ProviderMismatch {
        required: ProviderRequirement,
        actual_id: StableId,
        actual_version: ProviderVersion,
    },
    LimitMismatch(LimitMismatch),
    FormatMismatch(FormatMismatch),
    MemoryDomainMismatch(MemoryDomainMismatch),
    LatencyMismatch(LatencyMismatch),
    ExclusivityMismatch {
        required: ExclusivityMode,
        actual: ExclusivityMode,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LimitMismatch {
    pub name: StableId,
    pub required: LimitConstraint,
    pub actual: Option<LimitValue>,
    pub kind: LimitMismatchKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LimitMismatchKind {
    Missing,
    Unsatisfied,
    Incomparable {
        required: ValueKind,
        actual: ValueKind,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatMismatch {
    pub required: FormatDescriptor,
    pub supported: Vec<FormatDescriptor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryDomainMismatch {
    pub missing: BTreeSet<MemoryDomain>,
    pub supported: BTreeSet<MemoryDomain>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LatencyMismatch {
    pub accepted: BTreeSet<StableId>,
    pub supported: BTreeSet<StableId>,
}
