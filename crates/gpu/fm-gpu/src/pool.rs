use std::collections::BTreeMap;
use std::num::NonZeroU64;

use crate::{
    BufferDescriptor, DescriptorError, DeviceProfile, ResourceDescriptor, TextureDescriptor,
};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FenceValue(u64);

impl FenceValue {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LeaseId(NonZeroU64);

impl LeaseId {
    #[must_use]
    pub const fn get(self) -> NonZeroU64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ResourceId(NonZeroU64);

impl ResourceId {
    #[must_use]
    pub const fn get(self) -> NonZeroU64 {
        self.0
    }
}

/// Hard bounds for physical resources retained by a pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PoolBudget {
    pub max_resources: usize,
    pub max_bytes: u64,
}

impl PoolBudget {
    #[must_use]
    pub const fn new(max_resources: usize, max_bytes: u64) -> Self {
        Self {
            max_resources,
            max_bytes,
        }
    }
}

/// Cumulative counters and current pool occupancy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PoolTelemetry {
    pub allocated_resources: usize,
    pub active_leases: usize,
    pub allocated_bytes: u64,
    pub peak_allocated_bytes: u64,
    pub allocations: u64,
    pub reuses: u64,
    pub releases: u64,
    pub evictions: u64,
    pub denied_acquisitions: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PooledLease<D> {
    lease_id: LeaseId,
    resource_id: ResourceId,
    generation: u64,
    byte_size: u64,
    descriptor: D,
}

impl<D> PooledLease<D> {
    #[must_use]
    pub const fn lease_id(&self) -> LeaseId {
        self.lease_id
    }

    #[must_use]
    pub const fn resource_id(&self) -> ResourceId {
        self.resource_id
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub const fn byte_size(&self) -> u64 {
        self.byte_size
    }

    #[must_use]
    pub const fn descriptor(&self) -> &D {
        &self.descriptor
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResourceState {
    Leased(LeaseId),
    Available { retirement_fence: FenceValue },
}

#[derive(Clone, Debug)]
struct ResourceEntry<D> {
    descriptor: D,
    bytes: u64,
    state: ResourceState,
}

/// A deterministic pool that models allocation and retirement without owning
/// native GPU resources.
#[derive(Clone, Debug)]
pub struct ResourcePool<D> {
    profile: DeviceProfile,
    budget: PoolBudget,
    generation: u64,
    next_lease_id: u64,
    next_resource_id: u64,
    resources: BTreeMap<ResourceId, ResourceEntry<D>>,
    leases: BTreeMap<LeaseId, ResourceId>,
    telemetry: PoolTelemetry,
}

pub type TexturePool = ResourcePool<TextureDescriptor>;
pub type BufferPool = ResourcePool<BufferDescriptor>;

impl<D: ResourceDescriptor> ResourcePool<D> {
    /// Creates an empty first-generation pool.
    ///
    /// # Errors
    ///
    /// Returns [`PoolError::InvalidBudget`] if either bound is zero.
    pub fn new(profile: DeviceProfile, budget: PoolBudget) -> Result<Self, PoolError> {
        Self::with_generation(profile, budget, 1)
    }

    /// Creates an empty pool for a specific non-zero device generation.
    ///
    /// # Errors
    ///
    /// Returns an error for zero bounds or a zero generation.
    pub fn with_generation(
        profile: DeviceProfile,
        budget: PoolBudget,
        generation: u64,
    ) -> Result<Self, PoolError> {
        if budget.max_resources == 0 || budget.max_bytes == 0 {
            return Err(PoolError::InvalidBudget);
        }
        if generation == 0 {
            return Err(PoolError::InvalidGeneration);
        }
        Ok(Self {
            profile,
            budget,
            generation,
            next_lease_id: 1,
            next_resource_id: 1,
            resources: BTreeMap::new(),
            leases: BTreeMap::new(),
            telemetry: PoolTelemetry::default(),
        })
    }

    /// Acquires a new logical lease, reusing only resources whose retirement
    /// fences have completed.
    ///
    /// # Errors
    ///
    /// Returns descriptor, budget, or identifier exhaustion errors.
    pub fn acquire(
        &mut self,
        descriptor: D,
        completed_fence: FenceValue,
    ) -> Result<PooledLease<D>, PoolError> {
        if let Err(error) = descriptor.validate(&self.profile) {
            self.telemetry.denied_acquisitions += 1;
            return Err(PoolError::InvalidDescriptor(error));
        }
        let bytes = descriptor
            .byte_size()
            .map_err(PoolError::InvalidDescriptor)?;

        if let Some(resource_id) = self.resources.iter().find_map(|(id, entry)| {
            (entry.descriptor == descriptor
                && matches!(
                    entry.state,
                    ResourceState::Available { retirement_fence }
                        if retirement_fence <= completed_fence
                ))
            .then_some(*id)
        }) {
            self.telemetry.reuses += 1;
            return self.lease_existing(resource_id);
        }

        self.evict_until_fits(bytes, completed_fence);
        if self.resources.len() >= self.budget.max_resources
            || self.telemetry.allocated_bytes.saturating_add(bytes) > self.budget.max_bytes
        {
            self.telemetry.denied_acquisitions += 1;
            return Err(PoolError::BudgetExceeded {
                requested_bytes: bytes,
                available_bytes: self
                    .budget
                    .max_bytes
                    .saturating_sub(self.telemetry.allocated_bytes),
            });
        }

        let resource_id = ResourceId(next_non_zero(&mut self.next_resource_id)?);
        let lease_id = LeaseId(next_non_zero(&mut self.next_lease_id)?);
        self.resources.insert(
            resource_id,
            ResourceEntry {
                descriptor: descriptor.clone(),
                bytes,
                state: ResourceState::Leased(lease_id),
            },
        );
        self.leases.insert(lease_id, resource_id);
        self.telemetry.allocated_resources += 1;
        self.telemetry.active_leases += 1;
        self.telemetry.allocated_bytes += bytes;
        self.telemetry.peak_allocated_bytes = self
            .telemetry
            .peak_allocated_bytes
            .max(self.telemetry.allocated_bytes);
        self.telemetry.allocations += 1;
        Ok(PooledLease {
            lease_id,
            resource_id,
            generation: self.generation,
            byte_size: bytes,
            descriptor,
        })
    }

    /// Releases a lease and prevents reuse until `retirement_fence` completes.
    ///
    /// # Errors
    ///
    /// Returns [`PoolError::UnknownLease`] for a stale, foreign, or previously
    /// released lease ID.
    pub fn release(
        &mut self,
        lease_id: LeaseId,
        retirement_fence: FenceValue,
    ) -> Result<(), PoolError> {
        let resource_id = self
            .leases
            .remove(&lease_id)
            .ok_or(PoolError::UnknownLease(lease_id))?;
        let entry = self
            .resources
            .get_mut(&resource_id)
            .ok_or(PoolError::UnknownLease(lease_id))?;
        if entry.state != ResourceState::Leased(lease_id) {
            return Err(PoolError::UnknownLease(lease_id));
        }
        entry.state = ResourceState::Available { retirement_fence };
        self.telemetry.active_leases -= 1;
        self.telemetry.releases += 1;
        Ok(())
    }

    /// Drops all idle resources whose retirement fences have completed.
    pub fn trim(&mut self, completed_fence: FenceValue) {
        let removable = self
            .resources
            .iter()
            .filter_map(|(id, entry)| {
                matches!(
                    entry.state,
                    ResourceState::Available { retirement_fence }
                        if retirement_fence <= completed_fence
                )
                .then_some(*id)
            })
            .collect::<Vec<_>>();
        for id in removable {
            self.evict(id);
        }
    }

    /// Invalidates every pool ID and switches to a new device generation.
    /// Active leases become stale and need not be returned.
    ///
    /// # Errors
    ///
    /// Returns [`PoolError::InvalidGeneration`] unless the generation strictly
    /// increases and is non-zero.
    pub fn reset_generation(&mut self, generation: u64) -> Result<(), PoolError> {
        if generation == 0 || generation <= self.generation {
            return Err(PoolError::InvalidGeneration);
        }
        self.resources.clear();
        self.leases.clear();
        self.generation = generation;
        self.telemetry.allocated_resources = 0;
        self.telemetry.active_leases = 0;
        self.telemetry.allocated_bytes = 0;
        Ok(())
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub const fn budget(&self) -> PoolBudget {
        self.budget
    }

    #[must_use]
    pub const fn telemetry(&self) -> PoolTelemetry {
        self.telemetry
    }

    fn lease_existing(&mut self, resource_id: ResourceId) -> Result<PooledLease<D>, PoolError> {
        let lease_id = LeaseId(next_non_zero(&mut self.next_lease_id)?);
        let entry = self
            .resources
            .get_mut(&resource_id)
            .expect("selected resource exists");
        entry.state = ResourceState::Leased(lease_id);
        self.leases.insert(lease_id, resource_id);
        self.telemetry.active_leases += 1;
        Ok(PooledLease {
            lease_id,
            resource_id,
            generation: self.generation,
            byte_size: entry.bytes,
            descriptor: entry.descriptor.clone(),
        })
    }

    fn evict_until_fits(&mut self, requested_bytes: u64, completed_fence: FenceValue) {
        while self.resources.len() >= self.budget.max_resources
            || self
                .telemetry
                .allocated_bytes
                .saturating_add(requested_bytes)
                > self.budget.max_bytes
        {
            let candidate = self.resources.iter().find_map(|(id, entry)| {
                matches!(
                    entry.state,
                    ResourceState::Available { retirement_fence }
                        if retirement_fence <= completed_fence
                )
                .then_some(*id)
            });
            let Some(candidate) = candidate else {
                break;
            };
            self.evict(candidate);
        }
    }

    fn evict(&mut self, resource_id: ResourceId) {
        if let Some(entry) = self.resources.remove(&resource_id) {
            self.telemetry.allocated_resources -= 1;
            self.telemetry.allocated_bytes -= entry.bytes;
            self.telemetry.evictions += 1;
        }
    }
}

fn next_non_zero(counter: &mut u64) -> Result<NonZeroU64, PoolError> {
    let value = NonZeroU64::new(*counter).ok_or(PoolError::IdentifierExhausted)?;
    *counter = counter
        .checked_add(1)
        .ok_or(PoolError::IdentifierExhausted)?;
    Ok(value)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PoolError {
    InvalidBudget,
    InvalidGeneration,
    InvalidDescriptor(DescriptorError),
    BudgetExceeded {
        requested_bytes: u64,
        available_bytes: u64,
    },
    UnknownLease(LeaseId),
    IdentifierExhausted,
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidBudget => formatter.write_str("pool budget bounds must be non-zero"),
            Self::InvalidGeneration => formatter.write_str("pool generation is invalid"),
            Self::InvalidDescriptor(error) => error.fmt(formatter),
            Self::BudgetExceeded {
                requested_bytes,
                available_bytes,
            } => write!(
                formatter,
                "pool cannot fit {requested_bytes} bytes; {available_bytes} bytes are available"
            ),
            Self::UnknownLease(id) => write!(formatter, "lease {} is unknown", id.get()),
            Self::IdentifierExhausted => formatter.write_str("pool identifier space is exhausted"),
        }
    }
}

impl std::error::Error for PoolError {}
