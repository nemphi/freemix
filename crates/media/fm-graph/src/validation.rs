use std::collections::{BTreeMap, BTreeSet};

use fm_capabilities::{CapabilityRegistry, CompatibilityReport};

use crate::{
    CyclePolicy, Edge, EditableGraph, Endpoint, InputPort, MediaKind, NodeId, PortDirection,
    PortId, QueueDepth,
};

/// Identifies which edge endpoint failed validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointRole {
    Source,
    Destination,
}

/// Complete deterministic graph validation result.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GraphValidation {
    pub issues: Vec<ValidationIssue>,
}

impl GraphValidation {
    #[must_use]
    pub fn evaluate(graph: &EditableGraph, capabilities: &CapabilityRegistry) -> Self {
        let mut issues = Vec::new();
        validate_edges(graph, &mut issues);
        validate_inputs(graph, &mut issues);
        validate_capabilities(graph, capabilities, &mut issues);
        if let Some(nodes) = find_cycle(graph) {
            issues.push(ValidationIssue::Cycle { nodes });
        }
        Self { issues }
    }

    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.issues.is_empty()
    }
}

/// One actionable graph validation failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValidationIssue {
    MissingNode {
        edge: usize,
        endpoint: EndpointRole,
        node: NodeId,
    },
    MissingPort {
        edge: usize,
        endpoint: EndpointRole,
        node: NodeId,
        port: PortId,
    },
    DirectionMismatch {
        edge: usize,
        endpoint: EndpointRole,
        node: NodeId,
        port: PortId,
        expected: PortDirection,
        actual: PortDirection,
    },
    MediaMismatch {
        edge: usize,
        from: MediaKind,
        to: MediaKind,
    },
    DuplicateConnection {
        first_edge: usize,
        duplicate_edge: usize,
        connection: Edge,
    },
    UnboundedQueue {
        edge: usize,
    },
    ZeroQueueDepth {
        edge: usize,
    },
    ZeroMemoryBudget {
        edge: usize,
    },
    InputCardinality {
        node: NodeId,
        port: PortId,
        minimum: usize,
        maximum: Option<usize>,
        actual: usize,
    },
    CapabilityMismatch {
        node: NodeId,
        report: CompatibilityReport,
    },
    Cycle {
        nodes: Vec<NodeId>,
    },
}

fn validate_edges(graph: &EditableGraph, issues: &mut Vec<ValidationIssue>) {
    let mut seen = BTreeMap::new();
    for (index, edge) in graph.edges.iter().enumerate() {
        if let Some(first) = seen.insert((&edge.from, &edge.to), index) {
            issues.push(ValidationIssue::DuplicateConnection {
                first_edge: first,
                duplicate_edge: index,
                connection: edge.clone(),
            });
        }

        match edge.queue_depth {
            QueueDepth::Unbounded => issues.push(ValidationIssue::UnboundedQueue { edge: index }),
            QueueDepth::Bounded(0) => issues.push(ValidationIssue::ZeroQueueDepth { edge: index }),
            QueueDepth::Bounded(_) => {}
        }
        if edge.memory_budget_bytes == 0 {
            issues.push(ValidationIssue::ZeroMemoryBudget { edge: index });
        }

        let source = output_kind(graph, &edge.from, index, EndpointRole::Source, issues);
        let destination = input_port(graph, &edge.to, index, EndpointRole::Destination, issues);
        if let (Some(from), Some(to)) = (source, destination)
            && from != &to.media_kind
        {
            issues.push(ValidationIssue::MediaMismatch {
                edge: index,
                from: from.clone(),
                to: to.media_kind.clone(),
            });
        }
    }
}

fn output_kind<'a>(
    graph: &'a EditableGraph,
    endpoint: &Endpoint,
    edge: usize,
    role: EndpointRole,
    issues: &mut Vec<ValidationIssue>,
) -> Option<&'a MediaKind> {
    let Some(node) = graph.nodes.get(&endpoint.node) else {
        issues.push(ValidationIssue::MissingNode {
            edge,
            endpoint: role,
            node: endpoint.node.clone(),
        });
        return None;
    };
    if let Some(port) = node.outputs.get(&endpoint.port) {
        Some(&port.media_kind)
    } else if node.inputs.contains_key(&endpoint.port) {
        issues.push(ValidationIssue::DirectionMismatch {
            edge,
            endpoint: role,
            node: endpoint.node.clone(),
            port: endpoint.port.clone(),
            expected: PortDirection::Output,
            actual: PortDirection::Input,
        });
        None
    } else {
        issues.push(ValidationIssue::MissingPort {
            edge,
            endpoint: role,
            node: endpoint.node.clone(),
            port: endpoint.port.clone(),
        });
        None
    }
}

fn input_port<'a>(
    graph: &'a EditableGraph,
    endpoint: &Endpoint,
    edge: usize,
    role: EndpointRole,
    issues: &mut Vec<ValidationIssue>,
) -> Option<&'a InputPort> {
    let Some(node) = graph.nodes.get(&endpoint.node) else {
        issues.push(ValidationIssue::MissingNode {
            edge,
            endpoint: role,
            node: endpoint.node.clone(),
        });
        return None;
    };
    if let Some(port) = node.inputs.get(&endpoint.port) {
        Some(port)
    } else if node.outputs.contains_key(&endpoint.port) {
        issues.push(ValidationIssue::DirectionMismatch {
            edge,
            endpoint: role,
            node: endpoint.node.clone(),
            port: endpoint.port.clone(),
            expected: PortDirection::Input,
            actual: PortDirection::Output,
        });
        None
    } else {
        issues.push(ValidationIssue::MissingPort {
            edge,
            endpoint: role,
            node: endpoint.node.clone(),
            port: endpoint.port.clone(),
        });
        None
    }
}

fn validate_inputs(graph: &EditableGraph, issues: &mut Vec<ValidationIssue>) {
    let mut connections = BTreeMap::<(&NodeId, &PortId), usize>::new();
    for edge in &graph.edges {
        if graph
            .nodes
            .get(&edge.to.node)
            .is_some_and(|node| node.inputs.contains_key(&edge.to.port))
        {
            *connections
                .entry((&edge.to.node, &edge.to.port))
                .or_default() += 1;
        }
    }

    for (node_id, node) in &graph.nodes {
        for (port_id, port) in &node.inputs {
            let actual = connections.get(&(node_id, port_id)).copied().unwrap_or(0);
            if !port.cardinality.accepts(actual) {
                issues.push(ValidationIssue::InputCardinality {
                    node: node_id.clone(),
                    port: port_id.clone(),
                    minimum: port.cardinality.minimum,
                    maximum: port.cardinality.maximum,
                    actual,
                });
            }
        }
    }
}

fn validate_capabilities(
    graph: &EditableGraph,
    capabilities: &CapabilityRegistry,
    issues: &mut Vec<ValidationIssue>,
) {
    for (node_id, node) in &graph.nodes {
        let report = CompatibilityReport::evaluate(capabilities, &node.capabilities);
        if !report.is_compatible() {
            issues.push(ValidationIssue::CapabilityMismatch {
                node: node_id.clone(),
                report,
            });
        }
    }
}

fn find_cycle(graph: &EditableGraph) -> Option<Vec<NodeId>> {
    let adjacency = adjacency(graph);
    let mut visited = BTreeSet::new();
    let mut active = BTreeSet::new();
    let mut path = Vec::new();
    for node in graph.nodes.keys() {
        if let Some(cycle) = visit(node, &adjacency, &mut visited, &mut active, &mut path) {
            return Some(cycle);
        }
    }
    None
}

fn adjacency(graph: &EditableGraph) -> BTreeMap<&NodeId, BTreeSet<&NodeId>> {
    let mut adjacency = graph
        .nodes
        .keys()
        .map(|node| (node, BTreeSet::new()))
        .collect::<BTreeMap<_, _>>();
    for edge in graph.dependency_edges() {
        if graph.nodes.contains_key(&edge.to.node)
            && graph
                .nodes
                .get(&edge.from.node)
                .is_some_and(|node| node.cycle_policy != CyclePolicy::BreaksCycle)
        {
            adjacency
                .entry(&edge.from.node)
                .or_default()
                .insert(&edge.to.node);
        }
    }
    adjacency
}

fn visit<'a>(
    node: &'a NodeId,
    adjacency: &BTreeMap<&'a NodeId, BTreeSet<&'a NodeId>>,
    visited: &mut BTreeSet<&'a NodeId>,
    active: &mut BTreeSet<&'a NodeId>,
    path: &mut Vec<&'a NodeId>,
) -> Option<Vec<NodeId>> {
    if active.contains(node) {
        let start = path
            .iter()
            .position(|candidate| *candidate == node)
            .unwrap_or(0);
        let mut cycle = path[start..]
            .iter()
            .map(|candidate| (*candidate).clone())
            .collect::<Vec<_>>();
        cycle.push(node.clone());
        return Some(cycle);
    }
    if !visited.insert(node) {
        return None;
    }

    active.insert(node);
    path.push(node);
    for next in adjacency.get(node).into_iter().flatten() {
        if let Some(cycle) = visit(next, adjacency, visited, active, path) {
            return Some(cycle);
        }
    }
    path.pop();
    active.remove(node);
    None
}
