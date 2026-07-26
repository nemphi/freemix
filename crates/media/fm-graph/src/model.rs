use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
};

use fm_capabilities::{CapabilityRequirement, StableId};

use crate::{NodeId, PortId};

/// The payload class flowing through a port.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum MediaKind {
    Video,
    Audio,
    TimedData,
    ControlData,
    Health,
    Custom(StableId),
}

/// The number of connections accepted by an input port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputCardinality {
    pub minimum: usize,
    pub maximum: Option<usize>,
}

impl InputCardinality {
    pub const REQUIRED: Self = Self {
        minimum: 1,
        maximum: Some(1),
    };
    pub const OPTIONAL: Self = Self {
        minimum: 0,
        maximum: Some(1),
    };
    pub const MANY: Self = Self {
        minimum: 0,
        maximum: None,
    };

    /// Creates a custom input cardinality.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidCardinality`] when `maximum` is below `minimum`.
    pub const fn new(minimum: usize, maximum: Option<usize>) -> Result<Self, InvalidCardinality> {
        if matches!(maximum, Some(maximum) if maximum < minimum) {
            Err(InvalidCardinality { minimum, maximum })
        } else {
            Ok(Self { minimum, maximum })
        }
    }

    pub(crate) fn accepts(self, actual: usize) -> bool {
        actual >= self.minimum && self.maximum.is_none_or(|maximum| actual <= maximum)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidCardinality {
    pub minimum: usize,
    pub maximum: Option<usize>,
}

impl fmt::Display for InvalidCardinality {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("input maximum is below its minimum")
    }
}

impl Error for InvalidCardinality {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputPort {
    pub id: PortId,
    pub media_kind: MediaKind,
    pub cardinality: InputCardinality,
}

impl InputPort {
    #[must_use]
    pub const fn new(id: PortId, media_kind: MediaKind, cardinality: InputCardinality) -> Self {
        Self {
            id,
            media_kind,
            cardinality,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputPort {
    pub id: PortId,
    pub media_kind: MediaKind,
}

impl OutputPort {
    #[must_use]
    pub const fn new(id: PortId, media_kind: MediaKind) -> Self {
        Self { id, media_kind }
    }
}

/// Whether a node introduces a temporal boundary that permits feedback.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CyclePolicy {
    #[default]
    Reject,
    /// Outputs represent a prior tick and do not create a same-tick dependency.
    BreaksCycle,
}

/// Per-node bounded execution resources.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResourceCost {
    pub cpu_units: u64,
    pub gpu_bytes: u64,
    pub queue_frames: u64,
}

/// An editable processing node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Node {
    pub id: NodeId,
    pub kind: StableId,
    pub inputs: BTreeMap<PortId, InputPort>,
    pub outputs: BTreeMap<PortId, OutputPort>,
    pub capabilities: Vec<CapabilityRequirement>,
    pub resources: ResourceCost,
    pub cycle_policy: CyclePolicy,
}

impl Node {
    #[must_use]
    pub const fn new(id: NodeId, kind: StableId) -> Self {
        Self {
            id,
            kind,
            inputs: BTreeMap::new(),
            outputs: BTreeMap::new(),
            capabilities: Vec::new(),
            resources: ResourceCost {
                cpu_units: 0,
                gpu_bytes: 0,
                queue_frames: 0,
            },
            cycle_policy: CyclePolicy::Reject,
        }
    }

    /// Adds an input, rejecting a port ID used in either direction.
    ///
    /// # Errors
    ///
    /// Returns [`DuplicatePort`] when the port ID is already present.
    pub fn add_input(&mut self, port: InputPort) -> Result<(), DuplicatePort> {
        self.ensure_new_port(&port.id)?;
        self.inputs.insert(port.id.clone(), port);
        Ok(())
    }

    /// Adds an output, rejecting a port ID used in either direction.
    ///
    /// # Errors
    ///
    /// Returns [`DuplicatePort`] when the port ID is already present.
    pub fn add_output(&mut self, port: OutputPort) -> Result<(), DuplicatePort> {
        self.ensure_new_port(&port.id)?;
        self.outputs.insert(port.id.clone(), port);
        Ok(())
    }

    fn ensure_new_port(&self, port: &PortId) -> Result<(), DuplicatePort> {
        if self.inputs.contains_key(port) || self.outputs.contains_key(port) {
            Err(DuplicatePort {
                node: self.id.clone(),
                port: port.clone(),
            })
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicatePort {
    pub node: NodeId,
    pub port: PortId,
}

impl fmt::Display for DuplicatePort {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "node `{}` already contains port `{}`",
            self.node, self.port
        )
    }
}

impl Error for DuplicatePort {}

/// One side of a directed graph edge.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Endpoint {
    pub node: NodeId,
    pub port: PortId,
}

impl Endpoint {
    #[must_use]
    pub const fn new(node: NodeId, port: PortId) -> Self {
        Self { node, port }
    }
}

/// A directed output-to-input connection.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Edge {
    pub from: Endpoint,
    pub to: Endpoint,
    pub queue_depth: QueueDepth,
    pub overflow_policy: OverflowPolicy,
    pub memory_budget_bytes: u64,
}

impl Edge {
    pub const DEFAULT_MEMORY_BUDGET_BYTES: u64 = 1_048_576;

    #[must_use]
    pub const fn new(from: Endpoint, to: Endpoint) -> Self {
        Self {
            from,
            to,
            queue_depth: QueueDepth::Bounded(1),
            overflow_policy: OverflowPolicy::Block,
            memory_budget_bytes: Self::DEFAULT_MEMORY_BUDGET_BYTES,
        }
    }

    /// Declares the bounded queue allocation used by this connection.
    #[must_use]
    pub const fn with_queue(
        mut self,
        queue_depth: QueueDepth,
        overflow_policy: OverflowPolicy,
        memory_budget_bytes: u64,
    ) -> Self {
        self.queue_depth = queue_depth;
        self.overflow_policy = overflow_policy;
        self.memory_budget_bytes = memory_budget_bytes;
        self
    }
}

/// The maximum number of media items retained by an edge queue.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum QueueDepth {
    Bounded(u64),
    /// Representable in editable graphs so validation can reject it explicitly.
    Unbounded,
}

impl Default for QueueDepth {
    fn default() -> Self {
        Self::Bounded(1)
    }
}

/// Behavior when a bounded edge queue reaches its declared depth.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum OverflowPolicy {
    /// Apply backpressure until the consumer releases capacity.
    #[default]
    Block,
    DropNewest,
    DropOldest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PortDirection {
    Input,
    Output,
}

/// Mutable graph definition. Validation is deliberately deferred to compilation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EditableGraph {
    pub(crate) nodes: BTreeMap<NodeId, Node>,
    pub(crate) edges: Vec<Edge>,
}

impl EditableGraph {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            nodes: BTreeMap::new(),
            edges: Vec::new(),
        }
    }

    /// Adds a node without replacing an existing stable ID.
    ///
    /// # Errors
    ///
    /// Returns [`DuplicateNode`] when the node ID is already present.
    pub fn add_node(&mut self, node: Node) -> Result<(), DuplicateNode> {
        if self.nodes.contains_key(&node.id) {
            return Err(DuplicateNode { node: node.id });
        }
        self.nodes.insert(node.id.clone(), node);
        Ok(())
    }

    /// Adds a raw edge. Endpoint and duplicate checks occur during validation.
    pub fn connect(&mut self, edge: Edge) {
        self.edges.push(edge);
    }

    #[must_use]
    pub fn node(&self, id: &NodeId) -> Option<&Node> {
        self.nodes.get(id)
    }

    #[must_use]
    pub fn nodes(&self) -> impl ExactSizeIterator<Item = (&NodeId, &Node)> {
        self.nodes.iter()
    }

    #[must_use]
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    pub(crate) fn dependency_edges(&self) -> impl Iterator<Item = &Edge> {
        self.edges.iter().filter(|edge| {
            self.nodes
                .get(&edge.from.node)
                .is_none_or(|node| node.cycle_policy != CyclePolicy::BreaksCycle)
        })
    }

    pub(crate) fn unique_edges(&self) -> BTreeSet<Edge> {
        self.edges.iter().cloned().collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicateNode {
    pub node: NodeId,
}

impl fmt::Display for DuplicateNode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "graph already contains node `{}`", self.node)
    }
}

impl Error for DuplicateNode {}
