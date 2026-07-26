use fm_capabilities::StableId;
use fm_frame::{MemoryDomain, ResourceLease};

use crate::{
    AdapterProfile, BufferPool, DeviceFeature, DeviceLimits, DeviceProfile, PoolBudget, PoolError,
    ProfileError, TextureFormat, TexturePool,
};

/// Deterministic backend for exercising contracts. It does not create or model
/// a native GPU API object.
#[derive(Clone, Debug)]
pub struct SimulatedBackend {
    adapter: AdapterProfile,
}

impl SimulatedBackend {
    #[must_use]
    pub const fn new(adapter: AdapterProfile) -> Self {
        Self { adapter }
    }

    /// Returns a stable, conservative simulated adapter profile.
    ///
    /// # Panics
    ///
    /// Panics only if the crate's static adapter ID or profile constants cease
    /// to satisfy their own validation rules.
    #[must_use]
    pub fn deterministic() -> Self {
        let id = StableId::new("gpu.simulated").expect("static adapter ID is valid");
        let mut adapter = AdapterProfile::new(
            id,
            "Deterministic simulated GPU",
            DeviceLimits::conservative(),
            [
                TextureFormat::R8Unorm,
                TextureFormat::Rg8Unorm,
                TextureFormat::Rgba8Unorm,
                TextureFormat::Bgra8Unorm,
                TextureFormat::Rgba16Float,
                TextureFormat::Rgba32Float,
                TextureFormat::Depth32Float,
            ],
            [MemoryDomain::Cpu, MemoryDomain::Shared],
        )
        .expect("static simulated profile is valid");
        adapter.features.extend([
            DeviceFeature::TimestampQueries,
            DeviceFeature::ExternalResources,
            DeviceFeature::ExternalSynchronization,
            DeviceFeature::StorageTextures,
        ]);
        Self::new(adapter)
    }

    #[must_use]
    pub const fn adapter_profile(&self) -> &AdapterProfile {
        &self.adapter
    }

    /// Creates a simulated logical device with selected capabilities.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError`] when requested capabilities exceed the adapter.
    pub fn request_device(
        &self,
        limits: DeviceLimits,
        features: impl IntoIterator<Item = DeviceFeature>,
    ) -> Result<SimulatedDevice, ProfileError> {
        let profile = DeviceProfile::new(self.adapter.clone(), limits, features)?;
        Ok(SimulatedDevice {
            profile,
            state: DeviceState::Active,
            generation: 1,
            recovery_attempted: false,
        })
    }

    /// Creates a device with all capabilities from this adapter enabled.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError`] if the adapter profile is internally
    /// inconsistent.
    pub fn request_default_device(&self) -> Result<SimulatedDevice, ProfileError> {
        self.request_device(self.adapter.limits, self.adapter.features.iter().copied())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceState {
    Active,
    Lost { reason: String },
    Recovering { reason: String },
    Failed { reason: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryPolicy {
    /// Exactly one recreation attempt is permitted for each device loss.
    OneAttempt,
}

/// Stateful deterministic device-loss simulator with one attempt per loss.
#[derive(Clone, Debug)]
pub struct SimulatedDevice {
    profile: DeviceProfile,
    state: DeviceState,
    generation: u64,
    recovery_attempted: bool,
}

impl SimulatedDevice {
    #[must_use]
    pub const fn profile(&self) -> &DeviceProfile {
        &self.profile
    }

    #[must_use]
    pub const fn state(&self) -> &DeviceState {
        &self.state
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub const fn recovery_policy(&self) -> RecoveryPolicy {
        RecoveryPolicy::OneAttempt
    }

    /// Marks an active device lost. A successful recovery attempt can follow.
    ///
    /// # Errors
    ///
    /// Returns [`DeviceError::NotActive`] for any non-active state.
    pub fn mark_lost(&mut self, reason: impl Into<String>) -> Result<(), DeviceError> {
        if self.state != DeviceState::Active {
            return Err(DeviceError::NotActive);
        }
        self.state = DeviceState::Lost {
            reason: reason.into(),
        };
        self.recovery_attempted = false;
        Ok(())
    }

    /// Consumes the sole recovery attempt for the current loss.
    ///
    /// # Errors
    ///
    /// Returns an error unless the device is lost and no attempt was made.
    pub fn begin_recovery(&mut self) -> Result<(), RecoveryError> {
        if self.recovery_attempted {
            return Err(RecoveryError::AttemptAlreadyUsed);
        }
        let DeviceState::Lost { reason } = &self.state else {
            return Err(RecoveryError::NotLost);
        };
        self.recovery_attempted = true;
        self.state = DeviceState::Recovering {
            reason: reason.clone(),
        };
        Ok(())
    }

    /// Deterministically completes the in-progress attempt.
    ///
    /// A successful recreation increments the generation; failure is terminal.
    ///
    /// # Errors
    ///
    /// Returns an error when no recovery is in progress or the generation is
    /// exhausted.
    pub fn finish_recovery(&mut self, succeeds: bool) -> Result<(), RecoveryError> {
        let DeviceState::Recovering { reason } = &self.state else {
            return Err(RecoveryError::NotRecovering);
        };
        if succeeds {
            self.generation = self
                .generation
                .checked_add(1)
                .ok_or(RecoveryError::GenerationExhausted)?;
            self.state = DeviceState::Active;
        } else {
            self.state = DeviceState::Failed {
                reason: reason.clone(),
            };
        }
        Ok(())
    }

    /// Checks whether a resource generation belongs to this active device.
    ///
    /// # Errors
    ///
    /// Returns a state or stale-generation error.
    pub fn check_generation(&self, generation: u64) -> Result<(), DeviceError> {
        self.require_active()?;
        if generation != self.generation {
            return Err(DeviceError::StaleGeneration {
                expected: self.generation,
                actual: generation,
            });
        }
        Ok(())
    }

    /// Creates a generation-bound texture pool.
    ///
    /// # Errors
    ///
    /// Returns a device state or pool budget error.
    pub fn texture_pool(&self, budget: PoolBudget) -> Result<TexturePool, DeviceError> {
        self.require_active()?;
        TexturePool::with_generation(self.profile.clone(), budget, self.generation)
            .map_err(DeviceError::Pool)
    }

    /// Creates a generation-bound buffer pool.
    ///
    /// # Errors
    ///
    /// Returns a device state or pool budget error.
    pub fn buffer_pool(&self, budget: PoolBudget) -> Result<BufferPool, DeviceError> {
        self.require_active()?;
        BufferPool::with_generation(self.profile.clone(), budget, self.generation)
            .map_err(DeviceError::Pool)
    }

    /// Checks type-erased external lease metadata against enabled capabilities.
    /// No native resource import is attempted or implied.
    ///
    /// # Errors
    ///
    /// Returns a device-state, feature, memory-domain, or synchronization
    /// incompatibility.
    pub fn check_external_lease(&self, lease: &ResourceLease) -> Result<(), ExternalLeaseError> {
        if self.state != DeviceState::Active {
            return Err(ExternalLeaseError::DeviceNotActive);
        }
        if !self
            .profile
            .supports_feature(DeviceFeature::ExternalResources)
        {
            return Err(ExternalLeaseError::ExternalResourcesDisabled);
        }
        if !self.profile.supports_memory_domain(lease.memory_domain()) {
            return Err(ExternalLeaseError::IncompatibleMemoryDomain {
                actual: lease.memory_domain(),
                supported: self.profile.adapter().memory_domains.clone(),
            });
        }
        if (lease.ready_token().is_some() || lease.release_token().is_some())
            && !self
                .profile
                .supports_feature(DeviceFeature::ExternalSynchronization)
        {
            return Err(ExternalLeaseError::ExternalSynchronizationDisabled);
        }
        Ok(())
    }

    fn require_active(&self) -> Result<(), DeviceError> {
        if self.state == DeviceState::Active {
            Ok(())
        } else {
            Err(DeviceError::NotActive)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryError {
    NotLost,
    AttemptAlreadyUsed,
    NotRecovering,
    GenerationExhausted,
}

impl std::fmt::Display for RecoveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "device recovery failed: {self:?}")
    }
}

impl std::error::Error for RecoveryError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceError {
    NotActive,
    StaleGeneration { expected: u64, actual: u64 },
    Pool(PoolError),
}

impl std::fmt::Display for DeviceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "simulated device error: {self:?}")
    }
}

impl std::error::Error for DeviceError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExternalLeaseError {
    DeviceNotActive,
    ExternalResourcesDisabled,
    IncompatibleMemoryDomain {
        actual: MemoryDomain,
        supported: Vec<MemoryDomain>,
    },
    ExternalSynchronizationDisabled,
}

impl std::fmt::Display for ExternalLeaseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "external resource lease is incompatible: {self:?}"
        )
    }
}

impl std::error::Error for ExternalLeaseError {}
