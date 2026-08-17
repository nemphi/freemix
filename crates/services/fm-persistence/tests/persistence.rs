use std::{
    fs,
    num::NonZeroU128,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use fm_model::{Input, InputKind, MainMix, Project, ProjectSettings};
use fm_persistence::{
    CURRENT_SCHEMA_VERSION, FadeToBlackState, IdempotencyReceipt, JournalError,
    MAX_JOURNAL_RECORD_BYTES, MAX_MANIFEST_BYTES, ManualTransitionKind, ManualTransitionState,
    MutationBatch, ProjectPosition, ProjectStore, ProjectValidationError, ReceiptOutcome,
    ReferenceField, RuntimeFadeToBlack, RuntimeManualTransitions, RuntimeOverlayChannel,
    RuntimeOverlays, RuntimeRouting, StoreError, StoredProject,
};
use fm_types::{
    AudioFormat, ChannelLayout, ColorMetadata, FrameRate, InputId, PixelFormat, ProjectId,
    SampleFormat, SampleRate, ScanMode, VideoDimensions, VideoFormat,
};

static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fm-persistence-{}-{sequence}-{name}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn project_path(&self, name: &str) -> PathBuf {
        self.0.join(format!("{name}.freemix"))
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn input_id(value: u128) -> InputId {
    InputId::new(NonZeroU128::new(value).unwrap())
}

fn project(name: &str, revision: u64) -> StoredProject {
    project_with_receipts(
        name,
        ProjectPosition {
            revision,
            state_epoch: 7,
            event_sequence: 41,
            frames_rendered: 240,
            runtime_generation: 3,
            clock_time_nanos: 8_008_000_000,
        },
        vec![
            IdempotencyReceipt::rejected(
                "operator:002",
                "command-002",
                revision,
                "revision_conflict",
                "expected revision changed",
                true,
            ),
            IdempotencyReceipt::accepted("operator:001", "command-001", revision, 241),
        ],
    )
}

fn project_with_receipts(
    name: &str,
    position: ProjectPosition,
    receipts: Vec<IdempotencyReceipt>,
) -> StoredProject {
    try_project_with_receipts(name, position, receipts).unwrap()
}

fn try_project_with_receipts(
    name: &str,
    position: ProjectPosition,
    receipts: Vec<IdempotencyReceipt>,
) -> Result<StoredProject, ProjectValidationError> {
    try_project_with_overlays(name, position, receipts, RuntimeOverlays::default())
}

fn try_project_with_overlays(
    name: &str,
    position: ProjectPosition,
    receipts: Vec<IdempotencyReceipt>,
    overlays: RuntimeOverlays,
) -> Result<StoredProject, ProjectValidationError> {
    let frame_rate = FrameRate::new(60_000, 1_001).unwrap();
    let settings = ProjectSettings {
        frame_rate,
        video: VideoFormat {
            dimensions: VideoDimensions::new(1_920, 1_080).unwrap(),
            frame_rate,
            pixel_format: PixelFormat::Nv12,
            scan: ScanMode::Progressive,
            color: ColorMetadata::default(),
        },
        audio: AudioFormat {
            sample_rate: SampleRate::new(48_000).unwrap(),
            sample_format: SampleFormat::F32,
            channels: ChannelLayout::stereo(),
        },
    };
    let program = input_id(1);
    let preview = input_id(2);
    let mut domain = Project::new(
        ProjectId::new(NonZeroU128::new(9_001).unwrap()),
        name,
        settings,
    );
    for (id, label) in [(program, "Program"), (preview, "Preview")] {
        domain.add_input(Input {
            id,
            name: label.to_owned(),
            kind: InputKind::Color,
            required_capabilities: Vec::new(),
        });
    }
    domain.set_main_mix(MainMix::new(program, preview));
    StoredProject::from_project_with_complete_runtime_state(
        domain,
        RuntimeRouting {
            desired_program_id: Some(program),
            realized_program_id: Some(program),
            desired_preview_id: Some(preview),
            realized_preview_id: Some(preview),
        },
        RuntimeManualTransitions::default(),
        RuntimeFadeToBlack::default(),
        overlays,
        position,
        receipts,
    )
}

fn write_manifest(project_root: &Path, source: &str) {
    fs::create_dir_all(project_root).unwrap();
    fs::write(project_root.join("project.json"), source).unwrap();
}

#[test]
fn round_trip_preserves_typed_project() {
    let temp = TestDirectory::new("round-trip");
    let store = ProjectStore::new(temp.project_path("show")).unwrap();
    let expected = project("Friday Show", 11);

    store.save(&expected).unwrap();

    assert_eq!(store.load().unwrap(), expected);
    assert!(store.root().ends_with("show.freemix"));
    assert_eq!(expected.project().id().get().get(), 9_001);
    assert_eq!(expected.position().frames_rendered, 240);
    assert_eq!(expected.position().runtime_generation, 3);
    assert_eq!(expected.position().clock_time_nanos, 8_008_000_000);
    assert_eq!(expected.idempotency_receipts()[0].key(), "operator:001");
    assert!(matches!(
        expected.idempotency_receipts()[0].outcome(),
        ReceiptOutcome::Accepted {
            revision: 11,
            target_frame: 241
        }
    ));
    assert!(matches!(
        expected.idempotency_receipts()[1].outcome(),
        ReceiptOutcome::Rejected {
            current_revision: 11,
            code,
            message,
            retryable: true
        } if code == "revision_conflict" && message == "expected revision changed"
    ));
}

#[test]
fn round_trip_preserves_complete_overlay_state() {
    let temp = TestDirectory::new("overlay-round-trip");
    let root = temp.project_path("show");
    let mut overlays = RuntimeOverlays::default();
    overlays.desired[2] = RuntimeOverlayChannel {
        source: Some(input_id(2)),
        active: true,
        transition: fm_persistence::RuntimeOverlayTransition::Fade,
        duration_frames: 18,
        position: fm_persistence::RuntimeOverlayPosition::BottomRight,
        border: fm_persistence::RuntimeOverlayBorder::ThinWhite,
        queued_sources: vec![input_id(1), input_id(2)],
        included_outputs: Vec::new(),
    };
    overlays.realized = overlays.desired.clone();
    let stored = try_project_with_overlays(
        "Overlay show",
        ProjectPosition::default(),
        Vec::new(),
        overlays.clone(),
    )
    .unwrap();

    let store = ProjectStore::new(root).unwrap();
    store.save(&stored).unwrap();
    assert_eq!(store.load().unwrap().runtime_overlays(), &overlays);
}

#[test]
fn round_trip_preserves_exact_desired_and_realized_manual_transition_state() {
    let temp = TestDirectory::new("manual-transition");
    let store = ProjectStore::new(temp.project_path("show")).unwrap();
    let base = project("Manual", 11);
    let routing = base.runtime_routing();
    let desired = ManualTransitionState::new(
        ManualTransitionKind::Slide,
        routing.desired_program_id.unwrap(),
        routing.desired_preview_id.unwrap(),
        0,
        6_250,
    )
    .unwrap();
    let realized = ManualTransitionState::new(
        ManualTransitionKind::Slide,
        routing.realized_program_id.unwrap(),
        routing.realized_preview_id.unwrap(),
        6_250,
        6_250,
    )
    .unwrap();
    let expected = StoredProject::from_project_with_manual_transitions(
        base.project().clone(),
        routing,
        RuntimeManualTransitions {
            desired: Some(desired),
            realized: Some(realized),
        },
        base.position(),
        base.idempotency_receipts().to_vec(),
    )
    .unwrap();

    store.save(&expected).unwrap();

    assert_eq!(store.load().unwrap(), expected);
    let encoded = fs::read_to_string(store.manifest_path()).unwrap();
    assert!(encoded.contains("\"kind\": \"slide\""));
    assert!(encoded.contains("\"interval_start_basis_points\": 6250"));
    assert!(encoded.contains("\"position_basis_points\": 6250"));
}

#[test]
fn round_trip_preserves_settled_fade_to_black_state() {
    let temp = TestDirectory::new("fade-to-black");
    let store = ProjectStore::new(temp.project_path("show")).unwrap();
    let base = project("Fade to black", 11);
    let expected = StoredProject::from_project_with_runtime_state(
        base.project().clone(),
        base.runtime_routing(),
        base.runtime_manual_transitions(),
        RuntimeFadeToBlack {
            desired: FadeToBlackState::BLACK,
            realized: FadeToBlackState::BLACK,
        },
        base.position(),
        base.idempotency_receipts().to_vec(),
    )
    .unwrap();

    store.save(&expected).unwrap();

    assert_eq!(store.load().unwrap(), expected);
    let encoded = fs::read_to_string(store.manifest_path()).unwrap();
    assert!(encoded.contains("\"target_active\": true, \"position_numerator\": 65535"));
}

#[test]
fn manifests_reject_unsettled_or_divergent_fade_to_black_checkpoints() {
    let base = project("Fade to black validation", 11);
    let build = |fade_to_black| {
        StoredProject::from_project_with_runtime_state(
            base.project().clone(),
            base.runtime_routing(),
            base.runtime_manual_transitions(),
            fade_to_black,
            base.position(),
            base.idempotency_receipts().to_vec(),
        )
    };

    assert_eq!(
        build(RuntimeFadeToBlack {
            desired: FadeToBlackState::new(true, 40_000),
            realized: FadeToBlackState::BLACK,
        }),
        Err(ProjectValidationError::UnsettledFadeToBlack)
    );
    assert_eq!(
        build(RuntimeFadeToBlack {
            desired: FadeToBlackState::BLACK,
            realized: FadeToBlackState::LIVE,
        }),
        Err(ProjectValidationError::FadeToBlackCheckpointMismatch)
    );
}

#[test]
fn manifests_reject_unsettled_manual_transition_intervals() {
    let temp = TestDirectory::new("manual-transition-intervals");
    let valid_root = temp.project_path("valid");
    let valid_store = ProjectStore::new(&valid_root).unwrap();
    let base = project("Manual intervals", 11);
    let routing = base.runtime_routing();
    let valid = StoredProject::from_project_with_manual_transitions(
        base.project().clone(),
        routing,
        RuntimeManualTransitions {
            desired: ManualTransitionState::new(
                ManualTransitionKind::Fade,
                routing.desired_program_id.unwrap(),
                routing.desired_preview_id.unwrap(),
                0,
                6_250,
            ),
            realized: ManualTransitionState::new(
                ManualTransitionKind::Fade,
                routing.realized_program_id.unwrap(),
                routing.realized_preview_id.unwrap(),
                6_250,
                6_250,
            ),
        },
        base.position(),
        base.idempotency_receipts().to_vec(),
    )
    .unwrap();
    valid_store.save(&valid).unwrap();
    let manifest = fs::read_to_string(valid_store.manifest_path()).unwrap();

    let desired_root = temp.project_path("desired");
    write_manifest(
        &desired_root,
        &manifest.replacen(
            "\"interval_start_basis_points\": 0, \"position_basis_points\": 6250",
            "\"interval_start_basis_points\": 2500, \"position_basis_points\": 6250",
            1,
        ),
    );
    assert!(matches!(
        ProjectStore::new(desired_root).unwrap().load(),
        Err(StoreError::Validation(
            ProjectValidationError::InvalidDesiredManualTransitionInterval
        ))
    ));

    let realized_root = temp.project_path("realized");
    write_manifest(
        &realized_root,
        &manifest.replacen(
            "\"interval_start_basis_points\": 6250, \"position_basis_points\": 6250",
            "\"interval_start_basis_points\": 2500, \"position_basis_points\": 6250",
            1,
        ),
    );
    assert!(matches!(
        ProjectStore::new(realized_root).unwrap().load(),
        Err(StoreError::Validation(
            ProjectValidationError::InvalidRealizedManualTransitionInterval
        ))
    ));
}

#[test]
fn json_escaping_round_trips_quotes_slashes_controls_and_unicode() {
    let temp = TestDirectory::new("escaping");
    let store = ProjectStore::new(temp.project_path("escape")).unwrap();
    let expected = project("A \"quote\" \\\n\t\u{1} café 🎬", 1);

    store.save(&expected).unwrap();

    let encoded = fs::read_to_string(store.manifest_path()).unwrap();
    assert!(encoded.contains("\\\"quote\\\""));
    assert!(encoded.contains("\\\\"));
    assert!(encoded.contains("\\n\\t\\u0001"));
    assert!(encoded.contains("café 🎬"));
    assert_eq!(store.load().unwrap(), expected);
}

#[test]
fn deterministic_encoding_is_identical_across_saves() {
    let temp = TestDirectory::new("deterministic");
    let first = ProjectStore::new(temp.project_path("first")).unwrap();
    let second = ProjectStore::new(temp.project_path("second")).unwrap();
    let project = project("Stable", 5);

    first.save(&project).unwrap();
    second.save(&project).unwrap();

    assert_eq!(
        fs::read(first.manifest_path()).unwrap(),
        fs::read(second.manifest_path()).unwrap()
    );
}

#[test]
fn malformed_and_truncated_manifests_never_return_a_project() {
    let temp = TestDirectory::new("malformed");
    let malformed_root = temp.project_path("malformed");
    write_manifest(&malformed_root, "{\"schema_version\": -1}");
    let malformed = ProjectStore::new(malformed_root)
        .unwrap()
        .load()
        .unwrap_err();
    assert!(matches!(malformed, StoreError::MalformedManifest { .. }));

    let truncated_root = temp.project_path("truncated");
    write_manifest(
        &truncated_root,
        &format!("{{\"schema_version\":{CURRENT_SCHEMA_VERSION},\"show_name\":\"unfinished"),
    );
    let truncated = ProjectStore::new(truncated_root)
        .unwrap()
        .load()
        .unwrap_err();
    assert!(matches!(truncated, StoreError::MalformedManifest { .. }));
}

#[test]
fn manifest_just_over_size_limit_is_rejected() {
    let temp = TestDirectory::new("oversized");
    let root = temp.project_path("show");
    fs::create_dir_all(&root).unwrap();
    fs::File::create(root.join("project.json"))
        .unwrap()
        .set_len(MAX_MANIFEST_BYTES + 1)
        .unwrap();

    let error = ProjectStore::new(root).unwrap().load().unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "manifest is {} bytes, exceeding the {MAX_MANIFEST_BYTES}-byte maximum",
            MAX_MANIFEST_BYTES + 1
        )
    );
    assert!(matches!(
        error,
        StoreError::ManifestTooLarge {
            size,
            maximum: MAX_MANIFEST_BYTES
        } if size == MAX_MANIFEST_BYTES + 1
    ));
}

#[test]
fn non_current_schema_is_reported_before_missing_current_fields() {
    let temp = TestDirectory::new("non-current-schema");
    let root = temp.project_path("show");
    let non_current = CURRENT_SCHEMA_VERSION.checked_add(1).unwrap();
    write_manifest(&root, &format!(r#"{{"schema_version":{non_current}}}"#));

    assert!(matches!(
        ProjectStore::new(root).unwrap().load(),
        Err(StoreError::Validation(
            ProjectValidationError::UnsupportedSchema {
                found,
                supported: CURRENT_SCHEMA_VERSION
            }
        )) if found == non_current
    ));
}

#[test]
fn strict_parser_rejects_unknown_duplicate_and_wrong_typed_fields() {
    let temp = TestDirectory::new("strict");
    for (name, manifest) in [
        (
            "unknown",
            format!("{{\"schema_version\":{CURRENT_SCHEMA_VERSION},\"unknown\":true}}"),
        ),
        (
            "duplicate",
            format!(
                "{{\"schema_version\":{CURRENT_SCHEMA_VERSION},\"schema_version\":{CURRENT_SCHEMA_VERSION}}}"
            ),
        ),
        (
            "wrong-type",
            format!("{{\"schema_version\":\"{CURRENT_SCHEMA_VERSION}\"}}"),
        ),
        (
            "object-trailing-comma",
            format!("{{\"schema_version\":{CURRENT_SCHEMA_VERSION},}}"),
        ),
        (
            "array-trailing-comma",
            format!(
                r#"{{
              "schema_version": {CURRENT_SCHEMA_VERSION},
              "project_id": 1,
              "show_name": "Trailing",
              "input_ids": [1,],
              "desired_program_id": null,
              "realized_program_id": null,
              "desired_preview_id": null,
              "realized_preview_id": null,
              "revision": 0,
              "state_epoch": 0,
              "event_sequence": 0
            }}"#
            ),
        ),
    ] {
        let root = temp.project_path(name);
        write_manifest(&root, &manifest);
        assert!(matches!(
            ProjectStore::new(root).unwrap().load(),
            Err(StoreError::MalformedManifest { .. })
        ));
    }
}

#[test]
fn zero_project_id_is_rejected_during_typed_parsing() {
    let temp = TestDirectory::new("zero");
    let root = temp.project_path("show");
    let store = ProjectStore::new(&root).unwrap();
    store.save(&project("Zero", 0)).unwrap();
    let manifest = fs::read_to_string(store.manifest_path()).unwrap();
    write_manifest(&root, &manifest.replacen("\"id\": 9001", "\"id\": 0", 1));

    assert!(matches!(
        ProjectStore::new(root).unwrap().load(),
        Err(StoreError::MalformedManifest { .. })
    ));
}

#[test]
fn zero_input_ids_are_rejected_during_typed_parsing() {
    let temp = TestDirectory::new("zero-input");
    let root = temp.project_path("show");
    let store = ProjectStore::new(&root).unwrap();
    store.save(&project("Zero input", 0)).unwrap();
    let manifest = fs::read_to_string(store.manifest_path()).unwrap();
    write_manifest(&root, &manifest.replacen("\"id\": 1", "\"id\": 0", 1));

    assert!(matches!(
        ProjectStore::new(root).unwrap().load(),
        Err(StoreError::MalformedManifest { .. })
    ));
}

#[test]
fn load_rejects_references_to_missing_inputs() {
    let temp = TestDirectory::new("missing-reference");
    let root = temp.project_path("show");
    let store = ProjectStore::new(&root).unwrap();
    store.save(&project("Missing", 0)).unwrap();
    let manifest = fs::read_to_string(store.manifest_path()).unwrap();
    write_manifest(
        &root,
        &manifest.replacen("\"desired_program_id\": 1", "\"desired_program_id\": 99", 1),
    );

    let error = ProjectStore::new(root).unwrap().load().unwrap_err();
    assert!(matches!(
        error,
        StoreError::Validation(fm_persistence::ProjectValidationError::MissingInputReference {
            field: ReferenceField::DesiredProgram,
            id: missing
        }) if missing.get().get() == 99
    ));
}

#[test]
fn replacing_existing_save_is_complete_and_leaves_no_temp_file() {
    let temp = TestDirectory::new("replace");
    let store = ProjectStore::new(temp.project_path("show")).unwrap();
    store.save(&project("Old", 1)).unwrap();
    store.save(&project("New", 2)).unwrap();

    let loaded = store.load().unwrap();
    assert_eq!(loaded.show_name(), "New");
    assert_eq!(loaded.position().revision, 2);
    assert_eq!(
        fs::read_dir(store.root()).unwrap().count(),
        1,
        "only project.json should remain"
    );
}

#[test]
fn invalid_bundle_path_is_rejected_without_creating_it() {
    let temp = TestDirectory::new("root");
    let invalid = temp.0.join("show");

    assert!(matches!(
        ProjectStore::new(&invalid),
        Err(StoreError::InvalidRoot { .. })
    ));
    assert!(!invalid.exists());
}

#[test]
fn receipt_metadata_and_revision_invariants_are_validated() {
    let base = |receipts| {
        try_project_with_receipts(
            "Validation",
            ProjectPosition {
                revision: 4,
                ..ProjectPosition::default()
            },
            receipts,
        )
    };

    assert_eq!(
        base(vec![IdempotencyReceipt::accepted(" ", "command", 4, 1)]),
        Err(ProjectValidationError::EmptyIdempotencyKey)
    );
    assert_eq!(
        base(vec![IdempotencyReceipt::accepted("key", " ", 4, 1)]),
        Err(ProjectValidationError::EmptyCommandId {
            key: "key".to_owned()
        })
    );
    assert_eq!(
        base(vec![
            IdempotencyReceipt::accepted("key", "one", 3, 1),
            IdempotencyReceipt::rejected("key", "two", 4, "conflict", "no", false),
        ]),
        Err(ProjectValidationError::DuplicateIdempotencyKey {
            key: "key".to_owned()
        })
    );
    assert_eq!(
        base(vec![IdempotencyReceipt::rejected(
            "key", "command", 4, " ", "no", false,
        )]),
        Err(ProjectValidationError::EmptyRejectionCode {
            key: "key".to_owned()
        })
    );
    assert_eq!(
        base(vec![IdempotencyReceipt::accepted("key", "command", 5, 1)]),
        Err(ProjectValidationError::ReceiptRevisionAhead {
            key: "key".to_owned(),
            receipt_revision: 5,
            project_revision: 4,
        })
    );
}

#[test]
fn strict_parser_rejects_receipt_variant_fields() {
    let temp = TestDirectory::new("receipt-variant");
    let root = temp.project_path("show");
    let store = ProjectStore::new(&root).unwrap();
    let strict = project_with_receipts(
        "Strict receipt",
        ProjectPosition {
            revision: 1,
            ..ProjectPosition::default()
        },
        vec![IdempotencyReceipt::accepted("key", "command", 1, 1)],
    );
    store.save(&strict).unwrap();
    let manifest = fs::read_to_string(store.manifest_path()).unwrap();
    write_manifest(
        &root,
        &manifest.replacen(
            "\"target_frame\": 1",
            "\"target_frame\": 1,\n        \"retryable\": false",
            1,
        ),
    );

    assert!(matches!(
        ProjectStore::new(root).unwrap().load(),
        Err(StoreError::MalformedManifest { .. })
    ));
}

#[test]
fn journal_appends_are_durable_and_replay_in_sequence_order() {
    let temp = TestDirectory::new("journal-durable");
    let root = temp.project_path("show");
    let store = ProjectStore::new(&root).unwrap();
    store.save(&project("Journal", 10)).unwrap();
    store
        .append_batch(&MutationBatch::new(1, 10, 11, b"first".to_vec()))
        .unwrap();
    store
        .append_batch(&MutationBatch::new(2, 11, 12, b"second".to_vec()))
        .unwrap();

    // A fresh handle reopens the database from disk: the appends were durable
    // before each call returned, and nothing else is stored in the bundle.
    let reopened = ProjectStore::new(&root).unwrap();
    let scan = reopened.scan_journal().unwrap();
    assert_eq!(scan.checkpoint_sequence(), 0);
    assert_eq!(scan.checkpoint_revision(), 10);
    let sequences: Vec<u64> = scan.batches().iter().map(MutationBatch::sequence).collect();
    assert_eq!(sequences, vec![1, 2]);
    assert_eq!(scan.batches()[0].payload(), b"first");
    assert_eq!(scan.batches()[1].payload(), b"second");
    assert!(store.journal_database_path().is_file());
    let mut entries: Vec<String> = fs::read_dir(store.root())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        vec!["journal".to_owned(), "project.json".to_owned()]
    );
}

#[test]
fn journal_refuses_sequence_and_revision_gaps_without_recording_them() {
    let temp = TestDirectory::new("journal-gaps");
    let store = ProjectStore::new(temp.project_path("show")).unwrap();
    store.save(&project("Journal", 6)).unwrap();

    assert!(matches!(
        store.append_batch(&MutationBatch::new(2, 6, 7, Vec::new())),
        Err(StoreError::Journal(JournalError::SequenceGap {
            expected: 1,
            found: 2
        }))
    ));
    assert!(matches!(
        store.append_batch(&MutationBatch::new(1, 5, 7, Vec::new())),
        Err(StoreError::Journal(JournalError::RevisionGap { .. }))
    ));
    assert!(matches!(
        store.append_batch(&MutationBatch::new(1, 6, 8, Vec::new())),
        Err(StoreError::Journal(JournalError::RevisionGap { .. }))
    ));
    // Every refusal rolled back: the journal is still empty.
    assert!(store.scan_journal().unwrap().batches().is_empty());

    store
        .append_batch(&MutationBatch::new(1, 6, 7, Vec::new()))
        .unwrap();
    assert!(matches!(
        store.append_batch(&MutationBatch::new(1, 7, 8, Vec::new())),
        Err(StoreError::Journal(JournalError::SequenceGap {
            expected: 2,
            found: 1
        }))
    ));
    assert_eq!(store.scan_journal().unwrap().batches().len(), 1);
}

#[test]
fn journal_refuses_oversized_records_before_creating_the_journal() {
    let temp = TestDirectory::new("journal-oversize");
    let store = ProjectStore::new(temp.project_path("show")).unwrap();
    store.save(&project("Journal", 0)).unwrap();
    let payload = vec![0_u8; usize::try_from(MAX_JOURNAL_RECORD_BYTES).unwrap()];

    let error = store
        .append_batch(&MutationBatch::new(1, 0, 1, payload))
        .unwrap_err();
    assert!(matches!(
        error,
        StoreError::Journal(JournalError::RecordTooLarge {
            maximum: MAX_JOURNAL_RECORD_BYTES,
            ..
        })
    ));
    assert!(!store.journal_path().exists());

    // A large payload that fits the bound still round-trips byte for byte.
    let large = vec![0xa5_u8; usize::try_from(MAX_JOURNAL_RECORD_BYTES).unwrap() / 2];
    store
        .append_batch(&MutationBatch::new(1, 0, 1, large.clone()))
        .unwrap();
    assert_eq!(store.scan_journal().unwrap().batches()[0].payload(), large);
}

#[test]
fn compaction_saves_the_manifest_then_atomically_advances_the_checkpoint() {
    let temp = TestDirectory::new("journal-compact");
    let store = ProjectStore::new(temp.project_path("show")).unwrap();
    store.save(&project("Before", 20)).unwrap();
    store
        .append_batch(&MutationBatch::new(1, 20, 21, b"one".to_vec()))
        .unwrap();
    store
        .append_batch(&MutationBatch::new(2, 21, 22, b"two".to_vec()))
        .unwrap();

    let report = store
        .checkpoint_and_compact(&project("Checkpoint", 21), 1)
        .unwrap();

    assert_eq!(report.applied_through_sequence(), 1);
    assert_eq!(report.removed_records(), 1);
    assert_eq!(store.load().unwrap().show_name(), "Checkpoint");
    let scan = store.scan_journal().unwrap();
    assert_eq!(scan.checkpoint_sequence(), 1);
    assert_eq!(scan.checkpoint_revision(), 21);
    assert_eq!(scan.batches().len(), 1);
    assert_eq!(scan.batches()[0].sequence(), 2);
    assert_eq!(scan.batches()[0].payload(), b"two");
}

#[test]
fn compaction_refuses_a_mismatched_or_unknown_checkpoint_without_saving() {
    let temp = TestDirectory::new("journal-compact-refuse");
    let store = ProjectStore::new(temp.project_path("show")).unwrap();
    store.save(&project("Before", 20)).unwrap();
    store
        .append_batch(&MutationBatch::new(1, 20, 21, b"one".to_vec()))
        .unwrap();

    assert!(matches!(
        store.checkpoint_and_compact(&project("Wrong", 22), 1),
        Err(StoreError::Journal(JournalError::CheckpointRevision {
            sequence: 1,
            expected: 21,
            found: 22
        }))
    ));
    assert!(matches!(
        store.checkpoint_and_compact(&project("Unknown", 21), 9),
        Err(StoreError::Journal(JournalError::UnknownCheckpoint(9)))
    ));

    // Neither refusal saved the manifest or moved the checkpoint.
    assert_eq!(store.load().unwrap().show_name(), "Before");
    let scan = store.scan_journal().unwrap();
    assert_eq!(scan.checkpoint_sequence(), 0);
    assert_eq!(scan.batches().len(), 1);
}

#[test]
fn a_damaged_journal_database_is_a_typed_error_and_leaves_the_manifest_readable() {
    let temp = TestDirectory::new("journal-damaged");
    let store = ProjectStore::new(temp.project_path("show")).unwrap();
    store.save(&project("Journal", 3)).unwrap();
    store
        .append_batch(&MutationBatch::new(1, 3, 4, b"payload".to_vec()))
        .unwrap();

    // Overwrite the database header with garbage, as a torn write would.
    let database = store.journal_database_path();
    let mut bytes = fs::read(&database).unwrap();
    for byte in bytes.iter_mut().take(64) {
        *byte ^= 0xff;
    }
    fs::write(&database, bytes).unwrap();

    let error = store.scan_journal().unwrap_err();
    assert!(
        matches!(
            error,
            StoreError::Journal(
                JournalError::CorruptDatabase { .. } | JournalError::Database { .. }
            )
        ),
        "expected a typed journal database error, got {error:?}"
    );
    // The last consistent state stays readable through the manifest.
    assert_eq!(store.load().unwrap().position().revision, 3);
}
