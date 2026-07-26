use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Write as _},
};

use fm_capabilities::{CapabilityKey, CapabilityRegistry, ProviderVersion, StableId};

use crate::{
    Edge, EditableGraph, GraphValidation, Node, NodeId, OverflowPolicy, QueueDepth, ValidationIssue,
};

/// Hard limits declared before an execution plan is built.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceBudget {
    pub nodes: u64,
    pub edges: u64,
    pub cpu_units: u64,
    pub gpu_bytes: u64,
    pub memory_bytes: u64,
    pub queue_frames: u64,
}

/// Resources consumed by one immutable execution plan.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResourceUsage {
    pub nodes: u64,
    pub edges: u64,
    pub cpu_units: u64,
    pub gpu_bytes: u64,
    pub memory_bytes: u64,
    pub queue_frames: u64,
}

impl ResourceUsage {
    fn measure(graph: &EditableGraph) -> Self {
        let mut usage = Self {
            nodes: u64::try_from(graph.nodes.len()).unwrap_or(u64::MAX),
            edges: u64::try_from(graph.edges.len()).unwrap_or(u64::MAX),
            ..Self::default()
        };
        for node in graph.nodes.values() {
            usage.cpu_units = usage.cpu_units.saturating_add(node.resources.cpu_units);
            usage.gpu_bytes = usage.gpu_bytes.saturating_add(node.resources.gpu_bytes);
            usage.queue_frames = usage
                .queue_frames
                .saturating_add(node.resources.queue_frames);
        }
        for edge in &graph.edges {
            if let QueueDepth::Bounded(depth) = edge.queue_depth {
                usage.queue_frames = usage.queue_frames.saturating_add(depth);
            }
            usage.memory_bytes = usage.memory_bytes.saturating_add(edge.memory_budget_bytes);
        }
        usage
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceKind {
    Nodes,
    Edges,
    CpuUnits,
    GpuBytes,
    MemoryBytes,
    QueueFrames,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BudgetExceeded {
    pub resource: ResourceKind,
    pub required: u64,
    pub available: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompileError {
    InvalidGraph(GraphValidation),
    BudgetExceeded(Vec<BudgetExceeded>),
}

/// The concrete provider selected for one node capability requirement.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CapabilityChoice {
    pub node: NodeId,
    pub capability: CapabilityKey,
    pub provider: StableId,
    pub provider_version: ProviderVersion,
}

/// Deterministic human-readable decisions associated with an execution plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanReport {
    text: String,
    node_order: Vec<NodeId>,
    capability_choices: Vec<CapabilityChoice>,
    edges: Vec<Edge>,
    usage: ResourceUsage,
    validation_failures: Vec<ValidationIssue>,
}

impl PlanReport {
    fn build(
        nodes: &[Node],
        edges: &[Edge],
        capabilities: &CapabilityRegistry,
        usage: ResourceUsage,
    ) -> Self {
        let node_order = nodes.iter().map(|node| node.id.clone()).collect::<Vec<_>>();
        let mut capability_choices = nodes
            .iter()
            .flat_map(|node| {
                node.capabilities.iter().filter_map(|requirement| {
                    capabilities
                        .get(&requirement.key)
                        .map(|capability| CapabilityChoice {
                            node: node.id.clone(),
                            capability: requirement.key.clone(),
                            provider: capability.provider.id.clone(),
                            provider_version: capability.provider.version.clone(),
                        })
                })
            })
            .collect::<Vec<_>>();
        capability_choices.sort();
        let validation_failures = Vec::new();

        let mut text = String::from("Execution plan report\n");
        text.push_str("Validation failures: none\n");
        writeln!(&mut text, "Node order ({}):", nodes.len())
            .expect("writing to a string cannot fail");
        for (index, node) in nodes.iter().enumerate() {
            writeln!(&mut text, "  {index}: {} ({})", node.id, node.kind)
                .expect("writing to a string cannot fail");
        }
        writeln!(
            &mut text,
            "Capability choices ({}):",
            capability_choices.len()
        )
        .expect("writing to a string cannot fail");
        for choice in &capability_choices {
            writeln!(
                &mut text,
                "  {}: {} -> {}@{}",
                choice.node,
                choice.capability,
                choice.provider,
                choice.provider_version.as_str()
            )
            .expect("writing to a string cannot fail");
        }
        writeln!(&mut text, "Edges ({}):", edges.len()).expect("writing to a string cannot fail");
        for edge in edges {
            let depth = match edge.queue_depth {
                QueueDepth::Bounded(depth) => depth.to_string(),
                QueueDepth::Unbounded => "unbounded".to_owned(),
            };
            writeln!(
                &mut text,
                "  {}.{} -> {}.{}: depth={depth}, overflow={}, memory_budget_bytes={}",
                edge.from.node,
                edge.from.port,
                edge.to.node,
                edge.to.port,
                overflow_policy_name(edge.overflow_policy),
                edge.memory_budget_bytes
            )
            .expect("writing to a string cannot fail");
        }
        writeln!(
            &mut text,
            "Aggregate usage: nodes={}, edges={}, cpu_units={}, gpu_bytes={}, memory_bytes={}, queue_frames={}",
            usage.nodes,
            usage.edges,
            usage.cpu_units,
            usage.gpu_bytes,
            usage.memory_bytes,
            usage.queue_frames
        )
        .expect("writing to a string cannot fail");

        Self {
            text,
            node_order,
            capability_choices,
            edges: edges.to_vec(),
            usage,
            validation_failures,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn node_order(&self) -> &[NodeId] {
        &self.node_order
    }

    #[must_use]
    pub fn capability_choices(&self) -> &[CapabilityChoice] {
        &self.capability_choices
    }

    #[must_use]
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    #[must_use]
    pub const fn usage(&self) -> ResourceUsage {
        self.usage
    }

    #[must_use]
    pub fn validation_failures(&self) -> &[ValidationIssue] {
        &self.validation_failures
    }
}

impl fmt::Display for PlanReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.text)
    }
}

/// A validated, topologically ordered, immutable execution plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionPlan {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    usage: ResourceUsage,
    report: PlanReport,
}

impl ExecutionPlan {
    /// Validates and compiles an editable graph against capabilities and budget.
    ///
    /// # Errors
    ///
    /// Returns [`CompileError::InvalidGraph`] for structural or capability
    /// failures, or [`CompileError::BudgetExceeded`] for resource overruns.
    pub fn compile(
        graph: &EditableGraph,
        capabilities: &CapabilityRegistry,
        budget: ResourceBudget,
    ) -> Result<Self, CompileError> {
        Self::compile_with_report(graph, capabilities, budget).map(|(plan, _report)| plan)
    }

    /// Compiles a plan and its decision report as one atomic result.
    ///
    /// # Errors
    ///
    /// Returns no partial plan or report when validation or budgeting fails.
    pub fn compile_with_report(
        graph: &EditableGraph,
        capabilities: &CapabilityRegistry,
        budget: ResourceBudget,
    ) -> Result<(Self, PlanReport), CompileError> {
        let validation = GraphValidation::evaluate(graph, capabilities);
        if !validation.is_valid() {
            return Err(CompileError::InvalidGraph(validation));
        }

        let usage = ResourceUsage::measure(graph);
        let exceeded = budget.exceeded_by(usage);
        if !exceeded.is_empty() {
            return Err(CompileError::BudgetExceeded(exceeded));
        }

        let order = topological_order(graph);
        let nodes = order
            .into_iter()
            .map(|id| graph.nodes[&id].clone())
            .collect::<Vec<_>>();
        let edges = graph.unique_edges().into_iter().collect::<Vec<_>>();
        let report = PlanReport::build(&nodes, &edges, capabilities, usage);
        let plan = Self {
            nodes,
            edges,
            usage,
            report: report.clone(),
        };
        Ok((plan, report))
    }

    #[must_use]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    #[must_use]
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    #[must_use]
    pub const fn usage(&self) -> ResourceUsage {
        self.usage
    }

    #[must_use]
    pub const fn report(&self) -> &PlanReport {
        &self.report
    }
}

impl ResourceBudget {
    fn exceeded_by(self, usage: ResourceUsage) -> Vec<BudgetExceeded> {
        [
            (ResourceKind::Nodes, usage.nodes, self.nodes),
            (ResourceKind::Edges, usage.edges, self.edges),
            (ResourceKind::CpuUnits, usage.cpu_units, self.cpu_units),
            (ResourceKind::GpuBytes, usage.gpu_bytes, self.gpu_bytes),
            (
                ResourceKind::MemoryBytes,
                usage.memory_bytes,
                self.memory_bytes,
            ),
            (
                ResourceKind::QueueFrames,
                usage.queue_frames,
                self.queue_frames,
            ),
        ]
        .into_iter()
        .filter_map(|(resource, required, available)| {
            (required > available).then_some(BudgetExceeded {
                resource,
                required,
                available,
            })
        })
        .collect()
    }
}

const fn overflow_policy_name(policy: OverflowPolicy) -> &'static str {
    match policy {
        OverflowPolicy::Block => "block",
        OverflowPolicy::DropNewest => "drop-newest",
        OverflowPolicy::DropOldest => "drop-oldest",
    }
}

fn topological_order(graph: &EditableGraph) -> Vec<NodeId> {
    let mut indegree = graph
        .nodes
        .keys()
        .cloned()
        .map(|node| (node, 0_u64))
        .collect::<BTreeMap<_, _>>();
    let mut outgoing = BTreeMap::<NodeId, BTreeSet<NodeId>>::new();
    for edge in graph.dependency_edges() {
        if outgoing
            .entry(edge.from.node.clone())
            .or_default()
            .insert(edge.to.node.clone())
        {
            *indegree.entry(edge.to.node.clone()).or_default() += 1;
        }
    }

    let mut ready = indegree
        .iter()
        .filter_map(|(node, degree)| (*degree == 0).then_some(node.clone()))
        .collect::<BTreeSet<_>>();
    let mut order = Vec::with_capacity(graph.nodes.len());
    while let Some(node) = ready.pop_first() {
        order.push(node.clone());
        for target in outgoing.get(&node).into_iter().flatten() {
            let degree = indegree.get_mut(target).expect("validated target node");
            *degree -= 1;
            if *degree == 0 {
                ready.insert(target.clone());
            }
        }
    }
    order
}
