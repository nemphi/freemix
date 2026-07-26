use core::{fmt, num::NonZeroU128};

use fm_types::MemoryDomain;

macro_rules! stable_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(NonZeroU128);

        impl $name {
            #[must_use]
            pub const fn new(value: NonZeroU128) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn get(self) -> NonZeroU128 {
                self.0
            }
        }

        impl From<NonZeroU128> for $name {
            fn from(value: NonZeroU128) -> Self {
                Self::new(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

stable_id!(BridgeId);
stable_id!(ResourceId);
stable_id!(SynchronizationId);
stable_id!(ReleaseOwnerId);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SynchronizationToken {
    id: SynchronizationId,
    value: u64,
}

impl SynchronizationToken {
    #[must_use]
    pub const fn new(id: SynchronizationId, value: u64) -> Self {
        Self { id, value }
    }

    #[must_use]
    pub const fn id(self) -> SynchronizationId {
        self.id
    }

    #[must_use]
    pub const fn value(self) -> u64 {
        self.value
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReleaseOwnership {
    OwnerReclaims,
    LeaseHolderSignals,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ReleaseOwner {
    id: ReleaseOwnerId,
    ownership: ReleaseOwnership,
}

impl ReleaseOwner {
    #[must_use]
    pub const fn new(id: ReleaseOwnerId, ownership: ReleaseOwnership) -> Self {
        Self { id, ownership }
    }

    #[must_use]
    pub const fn id(self) -> ReleaseOwnerId {
        self.id
    }

    #[must_use]
    pub const fn ownership(self) -> ReleaseOwnership {
        self.ownership
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct ResourceLease {
    bridge_id: BridgeId,
    resource_id: ResourceId,
    memory_domain: MemoryDomain,
    ready: Option<SynchronizationToken>,
    release: Option<SynchronizationToken>,
    release_owner: ReleaseOwner,
}

impl ResourceLease {
    /// Creates a type-erased resource lease without exposing a native handle.
    ///
    /// # Errors
    ///
    /// Returns an error when holder-owned release has no signal token, or when
    /// tokens on one timeline put release before readiness.
    pub fn new(
        bridge_id: BridgeId,
        resource_id: ResourceId,
        memory_domain: MemoryDomain,
        ready: Option<SynchronizationToken>,
        release: Option<SynchronizationToken>,
        release_owner: ReleaseOwner,
    ) -> Result<Self, LeaseError> {
        if release_owner.ownership == ReleaseOwnership::LeaseHolderSignals && release.is_none() {
            return Err(LeaseError::MissingReleaseToken);
        }
        if let (Some(ready), Some(release)) = (ready, release)
            && ready.id == release.id
            && release.value < ready.value
        {
            return Err(LeaseError::ReleaseBeforeReady {
                ready: ready.value,
                release: release.value,
            });
        }
        Ok(Self {
            bridge_id,
            resource_id,
            memory_domain,
            ready,
            release,
            release_owner,
        })
    }

    #[must_use]
    pub const fn bridge_id(&self) -> BridgeId {
        self.bridge_id
    }

    #[must_use]
    pub const fn resource_id(&self) -> ResourceId {
        self.resource_id
    }

    #[must_use]
    pub const fn memory_domain(&self) -> MemoryDomain {
        self.memory_domain
    }

    #[must_use]
    pub const fn ready_token(&self) -> Option<SynchronizationToken> {
        self.ready
    }

    #[must_use]
    pub const fn release_token(&self) -> Option<SynchronizationToken> {
        self.release
    }

    #[must_use]
    pub const fn release_owner(&self) -> ReleaseOwner {
        self.release_owner
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeaseError {
    MissingReleaseToken,
    ReleaseBeforeReady { ready: u64, release: u64 },
}

impl fmt::Display for LeaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingReleaseToken => {
                formatter.write_str("lease holder release requires a synchronization token")
            }
            Self::ReleaseBeforeReady { ready, release } => write!(
                formatter,
                "release token value {release} precedes ready token value {ready}"
            ),
        }
    }
}

impl std::error::Error for LeaseError {}
