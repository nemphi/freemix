use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GraphResourceId(NonZeroU64);

impl GraphResourceId {
    #[must_use]
    pub const fn new(value: NonZeroU64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> NonZeroU64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PassId(NonZeroU64);

impl PassId {
    #[must_use]
    pub const fn new(value: NonZeroU64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> NonZeroU64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceOrigin {
    /// Must be written by an ordered pass before its first read.
    Transient,
    /// Is initialized before graph execution.
    External,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphResource {
    pub id: GraphResourceId,
    pub label: String,
    pub origin: ResourceOrigin,
}

impl GraphResource {
    #[must_use]
    pub fn new(id: GraphResourceId, label: impl Into<String>, origin: ResourceOrigin) -> Self {
        Self {
            id,
            label: label.into(),
            origin,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderPassDescriptor {
    pub id: PassId,
    pub label: String,
    pub reads: Vec<GraphResourceId>,
    pub writes: Vec<GraphResourceId>,
    pub dependencies: Vec<PassId>,
}

impl RenderPassDescriptor {
    #[must_use]
    pub fn new(id: PassId, label: impl Into<String>) -> Self {
        Self {
            id,
            label: label.into(),
            reads: Vec::new(),
            writes: Vec::new(),
            dependencies: Vec::new(),
        }
    }

    #[must_use]
    pub fn reads(mut self, resource: GraphResourceId) -> Self {
        self.reads.push(resource);
        self
    }

    #[must_use]
    pub fn writes(mut self, resource: GraphResourceId) -> Self {
        self.writes.push(resource);
        self
    }

    #[must_use]
    pub fn depends_on(mut self, pass: PassId) -> Self {
        self.dependencies.push(pass);
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct RenderGraph {
    resources: BTreeMap<GraphResourceId, GraphResource>,
    passes: BTreeMap<PassId, RenderPassDescriptor>,
}

impl RenderGraph {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            resources: BTreeMap::new(),
            passes: BTreeMap::new(),
        }
    }

    /// Adds a graph resource.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError::DuplicateResource`] if its ID already exists.
    pub fn add_resource(&mut self, resource: GraphResource) -> Result<(), GraphError> {
        let id = resource.id;
        if self.resources.insert(id, resource).is_some() {
            return Err(GraphError::DuplicateResource(id));
        }
        Ok(())
    }

    /// Adds a render pass.
    ///
    /// # Errors
    ///
    /// Returns a duplicate-ID or duplicate-use contract violation.
    pub fn add_pass(&mut self, pass: RenderPassDescriptor) -> Result<(), GraphError> {
        let id = pass.id;
        check_unique(id, &pass.reads, ResourceAccess::Read)?;
        check_unique(id, &pass.writes, ResourceAccess::Write)?;
        let mut dependencies = BTreeSet::new();
        for dependency in &pass.dependencies {
            if !dependencies.insert(*dependency) {
                return Err(GraphError::DuplicateDependency {
                    pass: id,
                    dependency: *dependency,
                });
            }
        }
        if self.passes.insert(id, pass).is_some() {
            return Err(GraphError::DuplicatePass(id));
        }
        Ok(())
    }

    /// Validates references, topologically orders passes, and checks resource
    /// initialization in that execution order.
    ///
    /// # Errors
    ///
    /// Returns missing resource/pass, dependency cycle, or read-before-write
    /// errors.
    pub fn validate(&self) -> Result<ValidatedRenderGraph, GraphError> {
        for pass in self.passes.values() {
            for resource in pass.reads.iter().chain(&pass.writes) {
                if !self.resources.contains_key(resource) {
                    return Err(GraphError::UnknownResource {
                        pass: pass.id,
                        resource: *resource,
                    });
                }
            }
            for dependency in &pass.dependencies {
                if !self.passes.contains_key(dependency) {
                    return Err(GraphError::UnknownDependency {
                        pass: pass.id,
                        dependency: *dependency,
                    });
                }
            }
        }

        let order = self.topological_order()?;
        let mut initialized = self
            .resources
            .values()
            .filter_map(|resource| {
                (resource.origin == ResourceOrigin::External).then_some(resource.id)
            })
            .collect::<BTreeSet<_>>();
        for pass_id in &order {
            let pass = &self.passes[pass_id];
            for resource in &pass.reads {
                if !initialized.contains(resource) {
                    return Err(GraphError::ReadBeforeWrite {
                        pass: *pass_id,
                        resource: *resource,
                    });
                }
            }
            initialized.extend(pass.writes.iter().copied());
        }
        Ok(ValidatedRenderGraph { pass_order: order })
    }

    fn topological_order(&self) -> Result<Vec<PassId>, GraphError> {
        let mut remaining = self
            .passes
            .iter()
            .map(|(id, pass)| {
                (
                    *id,
                    pass.dependencies.iter().copied().collect::<BTreeSet<_>>(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut ready = remaining
            .iter()
            .filter_map(|(id, dependencies)| dependencies.is_empty().then_some(*id))
            .collect::<BTreeSet<_>>();
        let mut order = Vec::with_capacity(self.passes.len());
        while let Some(id) = ready.pop_first() {
            remaining.remove(&id);
            order.push(id);
            for (candidate, dependencies) in &mut remaining {
                dependencies.remove(&id);
                if dependencies.is_empty() {
                    ready.insert(*candidate);
                }
            }
        }
        if remaining.is_empty() {
            Ok(order)
        } else {
            Err(GraphError::Cycle {
                passes: remaining.into_keys().collect(),
            })
        }
    }
}

fn check_unique(
    pass: PassId,
    resources: &[GraphResourceId],
    access: ResourceAccess,
) -> Result<(), GraphError> {
    let mut found = BTreeSet::new();
    for resource in resources {
        if !found.insert(*resource) {
            return Err(GraphError::DuplicateResourceUse {
                pass,
                resource: *resource,
                access,
            });
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedRenderGraph {
    pass_order: Vec<PassId>,
}

impl ValidatedRenderGraph {
    #[must_use]
    pub fn pass_order(&self) -> &[PassId] {
        &self.pass_order
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceAccess {
    Read,
    Write,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GraphError {
    DuplicateResource(GraphResourceId),
    DuplicatePass(PassId),
    DuplicateResourceUse {
        pass: PassId,
        resource: GraphResourceId,
        access: ResourceAccess,
    },
    DuplicateDependency {
        pass: PassId,
        dependency: PassId,
    },
    UnknownResource {
        pass: PassId,
        resource: GraphResourceId,
    },
    UnknownDependency {
        pass: PassId,
        dependency: PassId,
    },
    ReadBeforeWrite {
        pass: PassId,
        resource: GraphResourceId,
    },
    Cycle {
        passes: Vec<PassId>,
    },
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "render graph validation failed: {self:?}")
    }
}

impl std::error::Error for GraphError {}
