use std::collections::BTreeSet;

use fm_capabilities::StableId;
use fm_frame::MemoryDomain;

use crate::TextureFormat;

/// Portable limits advertised by an adapter or enabled on a device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceLimits {
    pub max_texture_dimension_1d: u32,
    pub max_texture_dimension_2d: u32,
    pub max_texture_dimension_3d: u32,
    pub max_texture_array_layers: u32,
    pub max_buffer_size: u64,
}

impl DeviceLimits {
    #[must_use]
    pub const fn conservative() -> Self {
        Self {
            max_texture_dimension_1d: 8_192,
            max_texture_dimension_2d: 8_192,
            max_texture_dimension_3d: 2_048,
            max_texture_array_layers: 256,
            max_buffer_size: 256 * 1024 * 1024,
        }
    }

    fn supports(self, requested: Self) -> bool {
        requested.max_texture_dimension_1d <= self.max_texture_dimension_1d
            && requested.max_texture_dimension_2d <= self.max_texture_dimension_2d
            && requested.max_texture_dimension_3d <= self.max_texture_dimension_3d
            && requested.max_texture_array_layers <= self.max_texture_array_layers
            && requested.max_buffer_size <= self.max_buffer_size
    }
}

impl Default for DeviceLimits {
    fn default() -> Self {
        Self::conservative()
    }
}

/// Backend-neutral optional functionality.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DeviceFeature {
    TimestampQueries,
    ExternalResources,
    ExternalSynchronization,
    StorageTextures,
}

/// Immutable capabilities discovered for an adapter implementation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdapterProfile {
    pub id: StableId,
    pub name: String,
    pub limits: DeviceLimits,
    pub features: BTreeSet<DeviceFeature>,
    pub texture_formats: BTreeSet<TextureFormat>,
    pub memory_domains: Vec<MemoryDomain>,
}

impl AdapterProfile {
    /// Creates a profile after validating its portable invariants.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError`] for an empty name, no formats, duplicate memory
    /// domains, or zero limits.
    pub fn new(
        id: StableId,
        name: impl Into<String>,
        limits: DeviceLimits,
        texture_formats: impl IntoIterator<Item = TextureFormat>,
        memory_domains: impl IntoIterator<Item = MemoryDomain>,
    ) -> Result<Self, ProfileError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(ProfileError::EmptyName);
        }
        if limits.max_texture_dimension_1d == 0
            || limits.max_texture_dimension_2d == 0
            || limits.max_texture_dimension_3d == 0
            || limits.max_texture_array_layers == 0
            || limits.max_buffer_size == 0
        {
            return Err(ProfileError::ZeroLimit);
        }
        let texture_formats = texture_formats.into_iter().collect::<BTreeSet<_>>();
        if texture_formats.is_empty() {
            return Err(ProfileError::NoTextureFormats);
        }
        let mut domains = Vec::new();
        for domain in memory_domains {
            if domains.contains(&domain) {
                return Err(ProfileError::DuplicateMemoryDomain(domain));
            }
            domains.push(domain);
        }
        Ok(Self {
            id,
            name,
            limits,
            features: BTreeSet::new(),
            texture_formats,
            memory_domains: domains,
        })
    }
}

/// Capabilities and limits enabled for one logical device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceProfile {
    adapter: AdapterProfile,
    limits: DeviceLimits,
    features: BTreeSet<DeviceFeature>,
}

impl DeviceProfile {
    /// Selects limits and features from an adapter profile.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError`] when a requested limit or feature is not
    /// advertised by the adapter.
    pub fn new(
        adapter: AdapterProfile,
        limits: DeviceLimits,
        features: impl IntoIterator<Item = DeviceFeature>,
    ) -> Result<Self, ProfileError> {
        if !adapter.limits.supports(limits) {
            return Err(ProfileError::UnsupportedLimits);
        }
        let features = features.into_iter().collect::<BTreeSet<_>>();
        if let Some(feature) = features.difference(&adapter.features).next() {
            return Err(ProfileError::UnsupportedFeature(*feature));
        }
        Ok(Self {
            adapter,
            limits,
            features,
        })
    }

    #[must_use]
    pub const fn adapter(&self) -> &AdapterProfile {
        &self.adapter
    }

    #[must_use]
    pub const fn limits(&self) -> DeviceLimits {
        self.limits
    }

    #[must_use]
    pub fn features(&self) -> &BTreeSet<DeviceFeature> {
        &self.features
    }

    #[must_use]
    pub fn supports_feature(&self, feature: DeviceFeature) -> bool {
        self.features.contains(&feature)
    }

    #[must_use]
    pub fn supports_memory_domain(&self, domain: MemoryDomain) -> bool {
        self.adapter.memory_domains.contains(&domain)
    }

    #[must_use]
    pub fn supports_texture_format(&self, format: TextureFormat) -> bool {
        self.adapter.texture_formats.contains(&format)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProfileError {
    EmptyName,
    ZeroLimit,
    NoTextureFormats,
    DuplicateMemoryDomain(MemoryDomain),
    UnsupportedLimits,
    UnsupportedFeature(DeviceFeature),
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyName => formatter.write_str("adapter name is empty"),
            Self::ZeroLimit => formatter.write_str("adapter limits must be non-zero"),
            Self::NoTextureFormats => formatter.write_str("adapter supports no texture formats"),
            Self::DuplicateMemoryDomain(domain) => {
                write!(formatter, "memory domain {domain:?} is duplicated")
            }
            Self::UnsupportedLimits => formatter.write_str("device limits exceed adapter limits"),
            Self::UnsupportedFeature(feature) => {
                write!(formatter, "device feature {feature:?} is not supported")
            }
        }
    }
}

impl std::error::Error for ProfileError {}
