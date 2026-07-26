use std::{collections::BTreeMap, error::Error, fmt};

use crate::{Capability, CapabilityKey, Provider};

/// A deterministic capability registry ordered by stable key.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CapabilityRegistry {
    capabilities: BTreeMap<CapabilityKey, Capability>,
}

impl CapabilityRegistry {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            capabilities: BTreeMap::new(),
        }
    }

    /// Registers a capability, rejecting an existing key without replacing it.
    ///
    /// # Errors
    ///
    /// Returns [`DuplicateCapability`] if the stable key is already present.
    pub fn register(&mut self, capability: Capability) -> Result<(), DuplicateCapability> {
        if let Some(existing) = self.capabilities.get(&capability.key) {
            return Err(DuplicateCapability {
                key: capability.key,
                registered_provider: existing.provider.clone(),
                rejected_provider: capability.provider,
            });
        }
        self.capabilities.insert(capability.key.clone(), capability);
        Ok(())
    }

    #[must_use]
    pub fn get(&self, key: &CapabilityKey) -> Option<&Capability> {
        self.capabilities.get(key)
    }

    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&CapabilityKey, &Capability)> {
        self.capabilities.iter()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.capabilities.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
    }
}

/// A rejected duplicate registration. The existing record remains unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicateCapability {
    pub key: CapabilityKey,
    pub registered_provider: Provider,
    pub rejected_provider: Provider,
}

impl fmt::Display for DuplicateCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "capability `{}` is already registered by `{}`",
            self.key, self.registered_provider.id
        )
    }
}

impl Error for DuplicateCapability {}
