use std::num::{NonZeroU64, NonZeroU128};

use fm_frame::{
    BridgeId, MemoryDomain, ReleaseOwner, ReleaseOwnerId, ReleaseOwnership,
    ResourceId as ExternalResourceId, ResourceLease,
};
use fm_gpu::{
    BufferDescriptor, BufferUsage, ContractShaderValidator, DeviceError, DeviceState, FenceValue,
    GraphError, GraphResource, GraphResourceId, PassId, PoolBudget, PoolError, RecoveryError,
    RenderGraph, RenderPassDescriptor, ResourceOrigin, ShaderDescriptor, ShaderLanguage,
    ShaderSource, ShaderStage, ShaderValidationLevel, ShaderValidator, SimulatedBackend,
    TextureDescriptor, TextureFormat, TextureUsage,
};

fn resource_id(value: u64) -> GraphResourceId {
    GraphResourceId::new(NonZeroU64::new(value).unwrap())
}

fn pass_id(value: u64) -> PassId {
    PassId::new(NonZeroU64::new(value).unwrap())
}

#[test]
fn texture_pool_reuses_with_new_lease_only_after_fence() {
    let device = SimulatedBackend::deterministic()
        .request_default_device()
        .unwrap();
    let descriptor = TextureDescriptor::two_dimensional(
        4,
        4,
        TextureFormat::Rgba8Unorm,
        TextureUsage::SAMPLED | TextureUsage::RENDER_ATTACHMENT,
    );
    let mut pool = device.texture_pool(PoolBudget::new(1, 64)).unwrap();

    let first = pool.acquire(descriptor.clone(), FenceValue::ZERO).unwrap();
    pool.release(first.lease_id(), FenceValue::new(5)).unwrap();
    assert!(matches!(
        pool.acquire(descriptor.clone(), FenceValue::new(4)),
        Err(PoolError::BudgetExceeded { .. })
    ));

    let second = pool.acquire(descriptor, FenceValue::new(5)).unwrap();
    assert_eq!(first.resource_id(), second.resource_id());
    assert_ne!(first.lease_id(), second.lease_id());
    assert_eq!(pool.telemetry().allocations, 1);
    assert_eq!(pool.telemetry().reuses, 1);
    assert_eq!(pool.telemetry().denied_acquisitions, 1);
}

#[test]
fn buffer_pool_enforces_byte_budget_and_reports_telemetry() {
    let device = SimulatedBackend::deterministic()
        .request_default_device()
        .unwrap();
    let mut pool = device.buffer_pool(PoolBudget::new(4, 16)).unwrap();
    let first = pool
        .acquire(
            BufferDescriptor::new(12, BufferUsage::STORAGE),
            FenceValue::ZERO,
        )
        .unwrap();

    assert!(matches!(
        pool.acquire(
            BufferDescriptor::new(8, BufferUsage::VERTEX),
            FenceValue::ZERO
        ),
        Err(PoolError::BudgetExceeded {
            requested_bytes: 8,
            available_bytes: 4
        })
    ));
    assert_eq!(pool.telemetry().allocated_bytes, 12);
    assert_eq!(pool.telemetry().peak_allocated_bytes, 12);

    pool.release(first.lease_id(), FenceValue::new(2)).unwrap();
    let replacement = pool
        .acquire(
            BufferDescriptor::new(8, BufferUsage::VERTEX),
            FenceValue::new(2),
        )
        .unwrap();
    assert_ne!(first.resource_id(), replacement.resource_id());
    assert_eq!(pool.telemetry().allocated_bytes, 8);
    assert_eq!(pool.telemetry().evictions, 1);
}

#[test]
fn graph_orders_dependencies_and_rejects_cycles() {
    let texture = resource_id(1);
    let producer = pass_id(2);
    let consumer = pass_id(1);
    let mut graph = RenderGraph::new();
    graph
        .add_resource(GraphResource::new(
            texture,
            "transient",
            ResourceOrigin::Transient,
        ))
        .unwrap();
    graph
        .add_pass(
            RenderPassDescriptor::new(consumer, "consume")
                .reads(texture)
                .depends_on(producer),
        )
        .unwrap();
    graph
        .add_pass(RenderPassDescriptor::new(producer, "produce").writes(texture))
        .unwrap();
    assert_eq!(
        graph.validate().unwrap().pass_order(),
        &[producer, consumer]
    );

    let mut cyclic = RenderGraph::new();
    cyclic
        .add_pass(RenderPassDescriptor::new(pass_id(1), "a").depends_on(pass_id(2)))
        .unwrap();
    cyclic
        .add_pass(RenderPassDescriptor::new(pass_id(2), "b").depends_on(pass_id(1)))
        .unwrap();
    assert!(matches!(
        cyclic.validate(),
        Err(GraphError::Cycle { passes }) if passes == vec![pass_id(1), pass_id(2)]
    ));
}

#[test]
fn graph_checks_resource_existence_and_read_before_write() {
    let missing = resource_id(9);
    let mut graph = RenderGraph::new();
    graph
        .add_pass(RenderPassDescriptor::new(pass_id(1), "missing").reads(missing))
        .unwrap();
    assert!(matches!(
        graph.validate(),
        Err(GraphError::UnknownResource { resource, .. }) if resource == missing
    ));

    let transient = resource_id(1);
    let mut graph = RenderGraph::new();
    graph
        .add_resource(GraphResource::new(
            transient,
            "transient",
            ResourceOrigin::Transient,
        ))
        .unwrap();
    graph
        .add_pass(RenderPassDescriptor::new(pass_id(1), "read").reads(transient))
        .unwrap();
    assert!(matches!(
        graph.validate(),
        Err(GraphError::ReadBeforeWrite { resource, .. }) if resource == transient
    ));
}

#[test]
fn shader_validator_records_contract_only_metadata() {
    let device = SimulatedBackend::deterministic()
        .request_default_device()
        .unwrap();
    let descriptor = ShaderDescriptor::new(
        "copy",
        ShaderStage::Compute,
        ShaderLanguage::Wgsl,
        "main",
        ShaderSource::Text("@compute @workgroup_size(1) fn main() {}".to_owned()),
    );
    let first = ContractShaderValidator
        .validate(descriptor.clone(), device.profile())
        .unwrap();
    let second = ContractShaderValidator
        .validate(descriptor, device.profile())
        .unwrap();
    assert_eq!(first.metadata().level, ShaderValidationLevel::ContractOnly);
    assert_eq!(
        first.metadata().source_fingerprint,
        second.metadata().source_fingerprint
    );

    let invalid = ShaderDescriptor::new(
        "binary mismatch",
        ShaderStage::Compute,
        ShaderLanguage::Wgsl,
        "main",
        ShaderSource::Binary(vec![0, 0, 0, 0]),
    );
    assert!(
        ContractShaderValidator
            .validate(invalid, device.profile())
            .is_err()
    );
}

#[test]
fn device_loss_has_one_recovery_attempt_and_changes_generation() {
    let mut device = SimulatedBackend::deterministic()
        .request_default_device()
        .unwrap();
    assert_eq!(device.generation(), 1);
    device.mark_lost("test loss").unwrap();
    assert!(matches!(device.state(), DeviceState::Lost { .. }));
    device.begin_recovery().unwrap();
    assert!(matches!(device.state(), DeviceState::Recovering { .. }));
    device.finish_recovery(true).unwrap();
    assert_eq!(device.state(), &DeviceState::Active);
    assert_eq!(device.generation(), 2);
    assert_eq!(
        device.check_generation(1),
        Err(DeviceError::StaleGeneration {
            expected: 2,
            actual: 1
        })
    );

    device.mark_lost("terminal loss").unwrap();
    device.begin_recovery().unwrap();
    device.finish_recovery(false).unwrap();
    assert!(matches!(device.state(), DeviceState::Failed { .. }));
    assert_eq!(
        device.begin_recovery(),
        Err(RecoveryError::AttemptAlreadyUsed)
    );
}

#[test]
fn external_lease_rejects_incompatible_memory_domain() {
    let device = SimulatedBackend::deterministic()
        .request_default_device()
        .unwrap();
    let lease = ResourceLease::new(
        BridgeId::new(NonZeroU128::new(1).unwrap()),
        ExternalResourceId::new(NonZeroU128::new(2).unwrap()),
        MemoryDomain::Metal,
        None,
        None,
        ReleaseOwner::new(
            ReleaseOwnerId::new(NonZeroU128::new(3).unwrap()),
            ReleaseOwnership::OwnerReclaims,
        ),
    )
    .unwrap();

    assert!(matches!(
        device.check_external_lease(&lease),
        Err(fm_gpu::ExternalLeaseError::IncompatibleMemoryDomain {
            actual: MemoryDomain::Metal,
            supported
        }) if supported == vec![MemoryDomain::Cpu, MemoryDomain::Shared]
    ));
}
