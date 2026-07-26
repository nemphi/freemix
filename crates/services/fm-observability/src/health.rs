use std::collections::BTreeMap;

use crate::Redactor;

/// Component condition independent of liveness and readiness gates.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ComponentHealth {
    Healthy,
    Degraded,
    Unhealthy,
}

impl ComponentHealth {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Unhealthy => "unhealthy",
        }
    }
}

/// Latest health observation for one named component.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HealthCheck {
    pub component: String,
    pub live: bool,
    pub ready: bool,
    pub health: ComponentHealth,
    pub detail: Option<String>,
}

impl HealthCheck {
    #[must_use]
    pub fn new(
        component: impl Into<String>,
        live: bool,
        ready: bool,
        health: ComponentHealth,
    ) -> Self {
        Self {
            component: component.into(),
            live,
            ready,
            health,
            detail: None,
        }
    }

    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// Process-level aggregation of all registered checks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HealthAggregate {
    pub live: bool,
    pub ready: bool,
    pub degraded: bool,
    pub health: ComponentHealth,
    pub failing_components: Vec<String>,
}

/// Explicitly owned latest-value registry, deterministically ordered by name.
#[derive(Clone, Debug, Default)]
pub struct HealthRegistry {
    checks: BTreeMap<String, HealthCheck>,
    redactor: Redactor,
}

impl HealthRegistry {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            checks: BTreeMap::new(),
            redactor: Redactor,
        }
    }

    #[must_use]
    pub const fn with_redactor(redactor: Redactor) -> Self {
        Self {
            checks: BTreeMap::new(),
            redactor,
        }
    }

    /// Inserts or replaces a component's redacted latest observation.
    pub fn update(&mut self, mut check: HealthCheck) -> Option<HealthCheck> {
        check.component = self.redactor.redact(&check.component);
        check.detail = check
            .detail
            .as_deref()
            .map(|detail| self.redactor.redact(detail));
        self.checks.insert(check.component.clone(), check)
    }

    pub fn remove(&mut self, component: &str) -> Option<HealthCheck> {
        self.checks.remove(component)
    }

    #[must_use]
    pub fn get(&self, component: &str) -> Option<&HealthCheck> {
        self.checks.get(component)
    }

    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &HealthCheck> {
        self.checks.values()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.checks.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.checks.is_empty()
    }

    #[must_use]
    pub fn aggregate(&self) -> HealthAggregate {
        let live = self.checks.values().all(|check| check.live);
        let has_unhealthy = self
            .checks
            .values()
            .any(|check| check.health == ComponentHealth::Unhealthy);
        let ready = live && !has_unhealthy && self.checks.values().all(|check| check.ready);
        let has_degraded = self
            .checks
            .values()
            .any(|check| check.health == ComponentHealth::Degraded);
        let health = if !live || has_unhealthy {
            ComponentHealth::Unhealthy
        } else if !ready || has_degraded {
            ComponentHealth::Degraded
        } else {
            ComponentHealth::Healthy
        };
        let failing_components = self
            .checks
            .values()
            .filter(|check| !check.live || !check.ready || check.health != ComponentHealth::Healthy)
            .map(|check| check.component.clone())
            .collect();
        HealthAggregate {
            live,
            ready,
            degraded: health == ComponentHealth::Degraded,
            health,
            failing_components,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregation_tracks_readiness_degradation_and_liveness() {
        let mut registry = HealthRegistry::new();
        registry.update(HealthCheck::new(
            "engine",
            true,
            true,
            ComponentHealth::Healthy,
        ));
        assert_eq!(registry.aggregate().health, ComponentHealth::Healthy);

        registry.update(HealthCheck::new(
            "output",
            true,
            false,
            ComponentHealth::Healthy,
        ));
        let not_ready = registry.aggregate();
        assert!(not_ready.live);
        assert!(!not_ready.ready);
        assert!(not_ready.degraded);

        registry.update(HealthCheck::new(
            "output",
            false,
            false,
            ComponentHealth::Unhealthy,
        ));
        let dead = registry.aggregate();
        assert!(!dead.live);
        assert!(!dead.ready);
        assert!(!dead.degraded);
        assert_eq!(dead.health, ComponentHealth::Unhealthy);
    }
}
