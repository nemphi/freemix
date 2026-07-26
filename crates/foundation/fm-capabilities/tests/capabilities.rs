use std::collections::BTreeSet;

use fm_capabilities::{
    Capability, CapabilityKey, CapabilityRegistry, CapabilityRequirement, CompatibilityIssue,
    CompatibilityReport, ExclusivityMode, FormatDescriptor, Health, HealthRequirement, InvalidKey,
    LatencyMode, LimitComparison, LimitConstraint, LimitMismatchKind, LimitValue, MemoryDomain,
    Provider, ProviderVersion, QuantityUnit, StableId,
};

fn id(value: &str) -> StableId {
    StableId::new(value).unwrap()
}

fn key(value: &str) -> CapabilityKey {
    CapabilityKey::new(value).unwrap()
}

fn version(value: &str) -> ProviderVersion {
    ProviderVersion::new(value).unwrap()
}

fn provider(name: &str, provider_version: &str) -> Provider {
    Provider::new(id(name), version(provider_version))
}

fn video_format(width: u64, pixel_format: &str) -> FormatDescriptor {
    FormatDescriptor::new(id("video.raw"))
        .with_field(id("width"), width)
        .with_field(id("pixel_format"), pixel_format)
}

#[test]
fn capability_keys_are_hierarchical_and_stable() {
    for valid in [
        "gpu.compositor.wgpu",
        "gpu.interop.dmabuf",
        "codec.h264.decode.nvdec",
        "output.decklink.key_fill",
        "network.ndi.v6.receive",
        "clock.ptp",
    ] {
        let parsed: CapabilityKey = valid.parse().unwrap();
        assert_eq!(parsed.as_str(), valid);
        assert_eq!(parsed.to_string(), valid);
    }

    assert_eq!(
        CapabilityKey::new("clock").unwrap_err(),
        InvalidKey::NotHierarchical
    );
    assert_eq!(
        CapabilityKey::new("gpu..wgpu").unwrap_err(),
        InvalidKey::EmptySegment { segment: 1 }
    );
    assert!(matches!(
        CapabilityKey::new("GPU.wgpu"),
        Err(InvalidKey::InvalidSegmentStart {
            segment: 0,
            found: 'G'
        })
    ));
    assert!(matches!(
        CapabilityKey::new("gpu.wg@pu"),
        Err(InvalidKey::InvalidCharacter {
            segment: 1,
            found: '@'
        })
    ));
}

#[test]
fn stable_descriptor_ids_may_be_flat_but_remain_validated() {
    assert_eq!(id("dmabuf").as_str(), "dmabuf");
    assert!(StableId::new("10_bit").is_err());
    assert!(StableId::new("").is_err());
}

#[test]
fn provider_versions_are_opaque_but_nonempty() {
    assert_eq!(version("Driver 551.86").as_str(), "Driver 551.86");
    assert!(ProviderVersion::new(" \t").is_err());
}

#[test]
fn format_matching_uses_required_fields_as_a_subset() {
    let supported = video_format(1920, "nv12")
        .with_field(id("height"), 1080_u64)
        .with_field(id("full_range"), false);
    assert!(supported.supports(&video_format(1920, "nv12")));
    assert!(!supported.supports(&video_format(3840, "nv12")));
    assert!(!supported.supports(&FormatDescriptor::new(id("video.encoded"))));
}

#[test]
fn typed_limit_comparison_rejects_different_types_and_units() {
    let at_least_four = LimitConstraint::new(LimitComparison::AtLeast, LimitValue::Unsigned(4));
    assert!(matches!(
        at_least_four,
        LimitConstraint {
            comparison: LimitComparison::AtLeast,
            ..
        }
    ));

    let fps = QuantityUnit::from_id(id("fps_milli"));
    let hz = QuantityUnit::from_id(id("hertz"));
    let required = LimitConstraint::new(
        LimitComparison::AtMost,
        LimitValue::Quantity {
            value: 16_000,
            unit: fps.clone(),
        },
    );
    let mut capability = Capability::new(key("capture.camera.raw"), provider("sim", "1"));
    capability.limits.insert(
        id("latency"),
        LimitValue::Quantity {
            value: 10_000,
            unit: fps,
        },
    );
    capability.limits.insert(
        id("wrong_unit"),
        LimitValue::Quantity {
            value: 10_000,
            unit: hz,
        },
    );

    let mut registry = CapabilityRegistry::new();
    registry.register(capability).unwrap();
    let mut requirement = CapabilityRequirement::new(key("capture.camera.raw"));
    requirement.limits.insert(id("latency"), required.clone());
    requirement.limits.insert(id("wrong_unit"), required);
    requirement.limits.insert(id("wrong_type"), at_least_four);

    let report = CompatibilityReport::evaluate(&registry, &[requirement]);
    assert_eq!(report.issues().count(), 2);
    assert!(report.issues().all(|issue| matches!(
        issue,
        CompatibilityIssue::LimitMismatch(mismatch)
            if matches!(mismatch.kind, LimitMismatchKind::Missing | LimitMismatchKind::Incomparable { .. })
    )));
}

#[test]
fn registry_is_key_ordered_and_rejects_duplicates_without_replacement() {
    let mut registry = CapabilityRegistry::new();
    registry
        .register(Capability::new(
            key("network.ndi.receive"),
            provider("ndi", "6"),
        ))
        .unwrap();
    registry
        .register(Capability::new(
            key("codec.h264.decode"),
            provider("software", "1"),
        ))
        .unwrap();

    let duplicate = registry
        .register(Capability::new(
            key("network.ndi.receive"),
            provider("other", "2"),
        ))
        .unwrap_err();
    assert_eq!(duplicate.registered_provider.id, id("ndi"));
    assert_eq!(duplicate.rejected_provider.id, id("other"));
    assert_eq!(
        registry
            .iter()
            .map(|(capability_key, _)| capability_key.as_str())
            .collect::<Vec<_>>(),
        ["codec.h264.decode", "network.ndi.receive"]
    );
    assert_eq!(
        registry
            .get(&key("network.ndi.receive"))
            .unwrap()
            .provider
            .id,
        id("ndi")
    );
}

#[test]
fn complete_project_requirement_matches() {
    let mut capability =
        Capability::new(key("codec.h264.encode.nvenc"), provider("nvidia", "551.86"));
    capability
        .limits
        .insert(id("max_width"), LimitValue::Unsigned(3840));
    capability.formats.push(video_format(1920, "nv12"));
    capability
        .memory_domains
        .insert(MemoryDomain(id("gpu.d3d12")));
    capability
        .latency_modes
        .push(LatencyMode::new(id("low_latency")));
    capability.exclusivity.mode = ExclusivityMode::Shared;

    let mut registry = CapabilityRegistry::new();
    registry.register(capability).unwrap();

    let mut requirement = CapabilityRequirement::new(key("codec.h264.encode.nvenc"));
    requirement.provider.id = Some(id("nvidia"));
    requirement.provider.version = Some(version("551.86"));
    requirement.limits.insert(
        id("max_width"),
        LimitConstraint::new(LimitComparison::AtLeast, LimitValue::Unsigned(1920)),
    );
    requirement.formats.push(video_format(1920, "nv12"));
    requirement
        .memory_domains
        .insert(MemoryDomain(id("gpu.d3d12")));
    requirement.latency_modes.insert(id("low_latency"));
    requirement.exclusivity = Some(ExclusivityMode::Shared);

    let report = CompatibilityReport::evaluate(&registry, &[requirement]);
    assert!(report.is_compatible());
    assert_eq!(report.issues().count(), 0);
}

#[test]
fn report_distinguishes_core_incompatibility_categories() {
    let mut unhealthy = Capability::new(key("capture.decklink.sdi"), provider("decklink", "12"));
    unhealthy.health = Health::Unhealthy {
        reason: "device disconnected".into(),
    };
    unhealthy
        .limits
        .insert(id("max_channels"), LimitValue::Unsigned(2));
    unhealthy.formats.push(video_format(1280, "uyvy"));

    let mut registry = CapabilityRegistry::new();
    registry.register(unhealthy).unwrap();

    let mut present = CapabilityRequirement::new(key("capture.decklink.sdi"));
    present.limits.insert(
        id("max_channels"),
        LimitConstraint::new(LimitComparison::AtLeast, LimitValue::Unsigned(8)),
    );
    present.limits.insert(
        id("max_width"),
        LimitConstraint::new(LimitComparison::AtLeast, LimitValue::Unsigned(1920)),
    );
    present.formats.push(video_format(1920, "v210"));
    let missing = CapabilityRequirement::new(key("clock.ptp"));

    let report = CompatibilityReport::evaluate(&registry, &[present, missing]);
    assert!(!report.is_compatible());
    assert!(matches!(
        report.requirements[0].issues[0],
        CompatibilityIssue::Unhealthy { .. }
    ));
    assert!(report.requirements[0].issues.iter().any(|issue| matches!(
        issue,
        CompatibilityIssue::LimitMismatch(mismatch)
            if mismatch.kind == LimitMismatchKind::Unsatisfied
    )));
    assert!(report.requirements[0].issues.iter().any(|issue| matches!(
        issue,
        CompatibilityIssue::LimitMismatch(mismatch)
            if mismatch.kind == LimitMismatchKind::Missing
    )));
    assert!(
        report.requirements[0]
            .issues
            .iter()
            .any(|issue| matches!(issue, CompatibilityIssue::FormatMismatch(_)))
    );
    assert_eq!(
        report.requirements[1].issues,
        [CompatibilityIssue::MissingCapability]
    );
}

#[test]
fn degraded_health_is_usable_unless_project_requires_healthy() {
    let mut capability = Capability::new(key("network.srt.receive"), provider("builtin", "1"));
    capability.health = Health::Degraded {
        reason: "high packet loss".into(),
    };
    let mut registry = CapabilityRegistry::new();
    registry.register(capability).unwrap();

    let usable = CapabilityRequirement::new(key("network.srt.receive"));
    let mut healthy = usable.clone();
    healthy.health = HealthRequirement::Healthy;

    assert!(CompatibilityReport::evaluate(&registry, &[usable]).is_compatible());
    assert!(!CompatibilityReport::evaluate(&registry, &[healthy]).is_compatible());
}

#[test]
fn domain_latency_and_exclusivity_mismatches_are_structured() {
    let mut capability = Capability::new(key("output.decklink.sdi"), provider("decklink", "12"));
    capability
        .memory_domains
        .insert(MemoryDomain(id("cpu.planar")));
    capability
        .latency_modes
        .push(LatencyMode::new(id("normal")));
    capability.exclusivity.mode = ExclusivityMode::Exclusive;
    let mut registry = CapabilityRegistry::new();
    registry.register(capability).unwrap();

    let mut requirement = CapabilityRequirement::new(key("output.decklink.sdi"));
    requirement.memory_domains = BTreeSet::from([MemoryDomain(id("gpu.dmabuf"))]);
    requirement.latency_modes = BTreeSet::from([id("low_latency")]);
    requirement.exclusivity = Some(ExclusivityMode::Shared);

    let report = CompatibilityReport::evaluate(&registry, &[requirement]);
    assert!(
        report
            .issues()
            .any(|issue| matches!(issue, CompatibilityIssue::MemoryDomainMismatch(_)))
    );
    assert!(
        report
            .issues()
            .any(|issue| matches!(issue, CompatibilityIssue::LatencyMismatch(_)))
    );
    assert!(
        report
            .issues()
            .any(|issue| matches!(issue, CompatibilityIssue::ExclusivityMismatch { .. }))
    );
}

#[test]
fn configurable_exclusivity_can_satisfy_a_shared_requirement() {
    let mut capability = Capability::new(key("audio.device.output"), provider("builtin", "1"));
    capability.exclusivity.mode = ExclusivityMode::Configurable;
    let mut registry = CapabilityRegistry::new();
    registry.register(capability).unwrap();

    let mut requirement = CapabilityRequirement::new(key("audio.device.output"));
    requirement.exclusivity = Some(ExclusivityMode::Shared);

    assert!(CompatibilityReport::evaluate(&registry, &[requirement]).is_compatible());
}
