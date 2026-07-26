use fm_capabilities::{
    Capability, CapabilityKey, CapabilityRegistry, CapabilityRequirement, Health, Provider,
    ProviderVersion, StableId,
};
use fm_graph::{
    CompileError, CyclePolicy, Edge, EditableGraph, Endpoint, ExecutionPlan, InputCardinality,
    InputPort, MediaKind, Node, NodeId, OutputPort, OverflowPolicy, PortDirection, PortId,
    QueueDepth, ResourceBudget, ResourceCost, ResourceKind, ValidationIssue,
};

fn stable(value: &str) -> StableId {
    StableId::new(value).unwrap()
}

fn node_id(value: &str) -> NodeId {
    NodeId::new(value).unwrap()
}

fn port_id(value: &str) -> PortId {
    PortId::new(value).unwrap()
}

fn endpoint(node: &str, port: &str) -> Endpoint {
    Endpoint::new(node_id(node), port_id(port))
}

fn source(name: &str, media: MediaKind) -> Node {
    let mut node = Node::new(node_id(name), stable("source"));
    node.add_output(OutputPort::new(port_id("out"), media))
        .unwrap();
    node
}

fn sink(name: &str, media: MediaKind) -> Node {
    let mut node = Node::new(node_id(name), stable("sink"));
    node.add_input(InputPort::new(
        port_id("in"),
        media,
        InputCardinality::REQUIRED,
    ))
    .unwrap();
    node
}

fn edge(from: &str, to: &str) -> Edge {
    Edge::new(endpoint(from, "out"), endpoint(to, "in"))
}

fn ample_budget() -> ResourceBudget {
    ResourceBudget {
        nodes: 100,
        edges: 100,
        cpu_units: 100,
        gpu_bytes: 1_000_000,
        memory_bytes: 100_000_000,
        queue_frames: 100,
    }
}

#[test]
fn graph_ids_are_stable_and_validated() {
    assert_eq!(node_id("camera_1").as_str(), "camera_1");
    assert_eq!(port_id("key-fill").as_str(), "key-fill");
    assert!(NodeId::new("Camera").is_err());
    assert!(NodeId::new("1_camera").is_err());
    assert!(PortId::new("video.out").is_err());
}

#[test]
fn node_rejects_port_ids_reused_across_directions() {
    let mut node = source("camera", MediaKind::Video);
    let error = node
        .add_input(InputPort::new(
            port_id("out"),
            MediaKind::Video,
            InputCardinality::OPTIONAL,
        ))
        .unwrap_err();
    assert_eq!(error.node, node_id("camera"));
    assert_eq!(error.port, port_id("out"));
}

#[test]
fn validation_reports_missing_nodes_ports_and_wrong_directions() {
    let mut graph = EditableGraph::new();
    graph.add_node(source("camera", MediaKind::Video)).unwrap();
    graph.add_node(sink("program", MediaKind::Video)).unwrap();
    graph.connect(Edge::new(
        endpoint("missing", "out"),
        endpoint("program", "in"),
    ));
    graph.connect(Edge::new(
        endpoint("camera", "unknown"),
        endpoint("program", "in"),
    ));
    graph.connect(Edge::new(
        endpoint("camera", "out"),
        endpoint("program", "unknown"),
    ));
    graph.connect(Edge::new(
        endpoint("program", "in"),
        endpoint("camera", "out"),
    ));

    let validation = fm_graph::GraphValidation::evaluate(&graph, &CapabilityRegistry::new());
    assert!(validation.issues.iter().any(
        |issue| matches!(issue, ValidationIssue::MissingNode { node, .. } if node == &node_id("missing"))
    ));
    assert!(
        validation
            .issues
            .iter()
            .any(|issue| matches!(issue, ValidationIssue::MissingPort { .. }))
    );
    assert!(validation.issues.iter().any(|issue| matches!(
        issue,
        ValidationIssue::DirectionMismatch {
            expected: PortDirection::Output,
            ..
        }
    )));
    assert!(validation.issues.iter().any(|issue| matches!(
        issue,
        ValidationIssue::DirectionMismatch {
            expected: PortDirection::Input,
            ..
        }
    )));
}

#[test]
fn validation_reports_media_mismatch_duplicate_and_cardinality() {
    let mut graph = EditableGraph::new();
    graph.add_node(source("camera", MediaKind::Video)).unwrap();
    graph
        .add_node(source("microphone", MediaKind::Audio))
        .unwrap();
    graph.add_node(sink("program", MediaKind::Video)).unwrap();
    graph.add_node(sink("unused", MediaKind::Audio)).unwrap();
    graph.connect(edge("microphone", "program"));
    graph.connect(edge("camera", "program"));
    graph.connect(edge("camera", "program"));

    let validation = fm_graph::GraphValidation::evaluate(&graph, &CapabilityRegistry::new());
    assert!(
        validation
            .issues
            .iter()
            .any(|issue| matches!(issue, ValidationIssue::MediaMismatch { .. }))
    );
    assert!(
        validation
            .issues
            .iter()
            .any(|issue| matches!(issue, ValidationIssue::DuplicateConnection { .. }))
    );
    assert!(validation.issues.iter().any(|issue| matches!(
        issue,
        ValidationIssue::InputCardinality { node, actual: 3, .. } if node == &node_id("program")
    )));
    assert!(validation.issues.iter().any(|issue| matches!(
        issue,
        ValidationIssue::InputCardinality { node, actual: 0, .. } if node == &node_id("unused")
    )));
}

#[test]
fn validation_rejects_unbounded_and_zero_edge_allocations() {
    let mut graph = EditableGraph::new();
    for name in ["a", "b", "c"] {
        graph.add_node(source(name, MediaKind::Video)).unwrap();
    }
    for name in ["x", "y", "z"] {
        graph.add_node(sink(name, MediaKind::Video)).unwrap();
    }
    graph.connect(edge("a", "x").with_queue(QueueDepth::Unbounded, OverflowPolicy::Block, 1_024));
    graph.connect(edge("b", "y").with_queue(
        QueueDepth::Bounded(0),
        OverflowPolicy::DropNewest,
        1_024,
    ));
    graph.connect(edge("c", "z").with_queue(QueueDepth::Bounded(1), OverflowPolicy::DropOldest, 0));

    let validation = fm_graph::GraphValidation::evaluate(&graph, &CapabilityRegistry::new());
    assert!(matches!(
        validation.issues[0],
        ValidationIssue::UnboundedQueue { edge: 0 }
    ));
    assert!(matches!(
        validation.issues[1],
        ValidationIssue::ZeroQueueDepth { edge: 1 }
    ));
    assert!(matches!(
        validation.issues[2],
        ValidationIssue::ZeroMemoryBudget { edge: 2 }
    ));
}

#[test]
fn cycles_are_rejected_with_a_deterministic_path() {
    let mut first = Node::new(node_id("first"), stable("transform"));
    first
        .add_input(InputPort::new(
            port_id("in"),
            MediaKind::Video,
            InputCardinality::REQUIRED,
        ))
        .unwrap();
    first
        .add_output(OutputPort::new(port_id("out"), MediaKind::Video))
        .unwrap();
    let mut second = first.clone();
    second.id = node_id("second");

    let mut graph = EditableGraph::new();
    graph.add_node(second).unwrap();
    graph.add_node(first).unwrap();
    graph.connect(edge("first", "second"));
    graph.connect(edge("second", "first"));

    let validation = fm_graph::GraphValidation::evaluate(&graph, &CapabilityRegistry::new());
    assert!(validation.issues.iter().any(|issue| matches!(
        issue,
        ValidationIssue::Cycle { nodes }
            if nodes == &vec![node_id("first"), node_id("second"), node_id("first")]
    )));
}

#[test]
fn explicit_delay_breaks_feedback_dependency() {
    let mut processor = Node::new(node_id("processor"), stable("transform"));
    processor
        .add_input(InputPort::new(
            port_id("in"),
            MediaKind::Audio,
            InputCardinality::REQUIRED,
        ))
        .unwrap();
    processor
        .add_output(OutputPort::new(port_id("out"), MediaKind::Audio))
        .unwrap();
    let mut delay = processor.clone();
    delay.id = node_id("delay");
    delay.kind = stable("delay");
    delay.cycle_policy = CyclePolicy::BreaksCycle;

    let mut graph = EditableGraph::new();
    graph.add_node(processor).unwrap();
    graph.add_node(delay).unwrap();
    graph.connect(edge("processor", "delay"));
    graph.connect(edge("delay", "processor"));

    let plan = ExecutionPlan::compile(&graph, &CapabilityRegistry::new(), ample_budget()).unwrap();
    assert_eq!(
        plan.nodes()
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        ["processor", "delay"]
    );
}

#[test]
fn capability_failures_are_attached_to_the_responsible_node() {
    let capability_key = CapabilityKey::new("gpu.compositor.wgpu").unwrap();
    let mut compositor = source("compositor", MediaKind::Video);
    compositor
        .capabilities
        .push(CapabilityRequirement::new(capability_key.clone()));
    let mut graph = EditableGraph::new();
    graph.add_node(compositor).unwrap();

    let validation = fm_graph::GraphValidation::evaluate(&graph, &CapabilityRegistry::new());
    assert!(validation.issues.iter().any(|issue| matches!(
        issue,
        ValidationIssue::CapabilityMismatch { node, report }
            if node == &node_id("compositor") && !report.is_compatible()
    )));

    let provider = Provider::new(stable("wgpu"), ProviderVersion::new("1").unwrap());
    let mut unhealthy = Capability::new(capability_key, provider);
    unhealthy.health = Health::Unhealthy {
        reason: "device lost".into(),
    };
    let mut capabilities = CapabilityRegistry::new();
    capabilities.register(unhealthy).unwrap();
    let validation = fm_graph::GraphValidation::evaluate(&graph, &capabilities);
    assert!(
        validation
            .issues
            .iter()
            .any(|issue| matches!(issue, ValidationIssue::CapabilityMismatch { .. }))
    );
}

#[test]
fn plan_is_deterministically_topological_and_edges_are_sorted() {
    let mut graph = EditableGraph::new();
    graph.add_node(sink("z_sink", MediaKind::Video)).unwrap();
    graph
        .add_node(source("b_source", MediaKind::Video))
        .unwrap();
    graph
        .add_node(source("a_source", MediaKind::Audio))
        .unwrap();
    graph
        .add_node(sink("audio_sink", MediaKind::Audio))
        .unwrap();
    graph.connect(edge("b_source", "z_sink"));
    graph.connect(edge("a_source", "audio_sink"));

    let plan = ExecutionPlan::compile(&graph, &CapabilityRegistry::new(), ample_budget()).unwrap();
    assert_eq!(
        plan.nodes()
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        ["a_source", "audio_sink", "b_source", "z_sink"]
    );
    assert_eq!(plan.edges()[0], edge("a_source", "audio_sink"));
    assert_eq!(plan.edges()[1], edge("b_source", "z_sink"));
}

#[test]
fn compile_reports_every_exceeded_resource_dimension() {
    let mut costly = source("costly", MediaKind::Video);
    costly.resources = ResourceCost {
        cpu_units: 11,
        gpu_bytes: 1_024,
        queue_frames: 8,
    };
    let mut graph = EditableGraph::new();
    graph.add_node(costly).unwrap();

    let error = ExecutionPlan::compile(
        &graph,
        &CapabilityRegistry::new(),
        ResourceBudget {
            nodes: 0,
            edges: 0,
            cpu_units: 10,
            gpu_bytes: 512,
            memory_bytes: 0,
            queue_frames: 4,
        },
    )
    .unwrap_err();
    let CompileError::BudgetExceeded(exceeded) = error else {
        panic!("expected budget error");
    };
    assert_eq!(
        exceeded
            .iter()
            .map(|failure| failure.resource)
            .collect::<Vec<_>>(),
        [
            ResourceKind::Nodes,
            ResourceKind::CpuUnits,
            ResourceKind::GpuBytes,
            ResourceKind::QueueFrames
        ]
    );
}

#[test]
fn valid_plan_reports_exact_usage() {
    let mut camera = source("camera", MediaKind::Video);
    camera.resources = ResourceCost {
        cpu_units: 2,
        gpu_bytes: 256,
        queue_frames: 3,
    };
    let mut program = sink("program", MediaKind::Video);
    program.resources = ResourceCost {
        cpu_units: 4,
        gpu_bytes: 512,
        queue_frames: 2,
    };
    let mut graph = EditableGraph::new();
    graph.add_node(camera).unwrap();
    graph.add_node(program).unwrap();
    graph.connect(edge("camera", "program"));

    let plan = ExecutionPlan::compile(&graph, &CapabilityRegistry::new(), ample_budget()).unwrap();
    assert_eq!(plan.usage().nodes, 2);
    assert_eq!(plan.usage().edges, 1);
    assert_eq!(plan.usage().cpu_units, 6);
    assert_eq!(plan.usage().gpu_bytes, 768);
    assert_eq!(plan.usage().memory_bytes, Edge::DEFAULT_MEMORY_BUDGET_BYTES);
    assert_eq!(plan.usage().queue_frames, 6);
}

#[test]
fn edge_budgets_are_aggregated_and_enforced() {
    let mut graph = EditableGraph::new();
    graph.add_node(source("camera", MediaKind::Video)).unwrap();
    graph.add_node(sink("program", MediaKind::Video)).unwrap();
    graph.connect(edge("camera", "program").with_queue(
        QueueDepth::Bounded(7),
        OverflowPolicy::DropOldest,
        4_096,
    ));

    let error = ExecutionPlan::compile(
        &graph,
        &CapabilityRegistry::new(),
        ResourceBudget {
            nodes: 2,
            edges: 1,
            cpu_units: 0,
            gpu_bytes: 0,
            memory_bytes: 4_095,
            queue_frames: 6,
        },
    )
    .unwrap_err();
    let CompileError::BudgetExceeded(exceeded) = error else {
        panic!("expected budget error");
    };
    assert_eq!(
        exceeded
            .iter()
            .map(|failure| (failure.resource, failure.required))
            .collect::<Vec<_>>(),
        [
            (ResourceKind::MemoryBytes, 4_096),
            (ResourceKind::QueueFrames, 7),
        ]
    );
}

#[test]
fn plan_report_text_is_deterministic_and_complete() {
    let capability_key = CapabilityKey::new("capture.camera.raw").unwrap();
    let mut camera = source("camera", MediaKind::Video);
    camera
        .capabilities
        .push(CapabilityRequirement::new(capability_key.clone()));
    let mut graph = EditableGraph::new();
    graph.add_node(sink("program", MediaKind::Video)).unwrap();
    graph.add_node(camera).unwrap();
    graph.connect(edge("camera", "program").with_queue(
        QueueDepth::Bounded(3),
        OverflowPolicy::DropOldest,
        4_096,
    ));

    let mut capabilities = CapabilityRegistry::new();
    capabilities
        .register(Capability::new(
            capability_key,
            Provider::new(stable("builtin"), ProviderVersion::new("1").unwrap()),
        ))
        .unwrap();

    let (plan, report) =
        ExecutionPlan::compile_with_report(&graph, &capabilities, ample_budget()).unwrap();
    let expected = concat!(
        "Execution plan report\n",
        "Validation failures: none\n",
        "Node order (2):\n",
        "  0: camera (source)\n",
        "  1: program (sink)\n",
        "Capability choices (1):\n",
        "  camera: capture.camera.raw -> builtin@1\n",
        "Edges (1):\n",
        "  camera.out -> program.in: depth=3, overflow=drop-oldest, ",
        "memory_budget_bytes=4096\n",
        "Aggregate usage: nodes=2, edges=1, cpu_units=0, gpu_bytes=0, ",
        "memory_bytes=4096, queue_frames=3\n",
    );
    assert_eq!(report.as_str(), expected);
    assert_eq!(plan.report(), &report);
    assert!(report.validation_failures().is_empty());

    let (_, repeated) =
        ExecutionPlan::compile_with_report(&graph, &capabilities, ample_budget()).unwrap();
    assert_eq!(repeated.as_str(), expected);
}

#[test]
fn failed_compilation_returns_no_partial_plan_or_report() {
    let mut graph = EditableGraph::new();
    graph.add_node(sink("program", MediaKind::Video)).unwrap();

    let result =
        ExecutionPlan::compile_with_report(&graph, &CapabilityRegistry::new(), ample_budget());
    assert!(matches!(result, Err(CompileError::InvalidGraph(_))));

    let mut graph = EditableGraph::new();
    graph.add_node(source("camera", MediaKind::Video)).unwrap();
    let mut insufficient_budget = ample_budget();
    insufficient_budget.nodes = 0;
    let result =
        ExecutionPlan::compile_with_report(&graph, &CapabilityRegistry::new(), insufficient_budget);
    assert!(matches!(result, Err(CompileError::BudgetExceeded(_))));
}
