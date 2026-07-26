//! Portable editable media graphs and bounded execution plans.

mod id;
mod model;
mod plan;
mod validation;

pub use id::{InvalidGraphId, NodeId, PortId};
pub use model::{
    CyclePolicy, DuplicateNode, DuplicatePort, Edge, EditableGraph, Endpoint, InputCardinality,
    InputPort, InvalidCardinality, MediaKind, Node, OutputPort, OverflowPolicy, PortDirection,
    QueueDepth, ResourceCost,
};
pub use plan::{
    BudgetExceeded, CapabilityChoice, CompileError, ExecutionPlan, PlanReport, ResourceBudget,
    ResourceKind, ResourceUsage,
};
pub use validation::{EndpointRole, GraphValidation, ValidationIssue};
