use std::{
    fs,
    num::NonZeroU128,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use fm_model::{
    AudioBus, BusSend, CURRENT_SCHEMA_VERSION, CropRect, Input, InputAudioStripState,
    InputBalanceBasisPoints, InputDelaySamples, InputGainMilliDb, InputKind, Layer, LayerGeometry,
    MainMix, Output, Project, ProjectSettings, RectMask, RestartPolicy, Rgba8, Rotation, Scene,
    SimulatedAudio, SimulatedInput, SimulatedVideo, SolidColor, SourceRef, StartupPolicy,
    StingerAudioPolicy, StingerConfig, StingerMissingMediaFallback, StingerSlotNumber,
    StreamEndpoint, StreamKey, StreamProtocol, StreamTarget, StreamTargetId,
};
use fm_persistence::{ProjectPosition, ProjectStore, RuntimeRouting, StoreError, StoredProject};
use fm_types::{
    AudioFormat, BusId, Channel, ChannelLayout, ChromaLocation, ColorMetadata, ColorPrimaries,
    FrameRate, InputId, MatrixCoefficients, OutputId, PixelFormat, ProjectId, SampleFormat,
    SampleRate, ScanMode, SceneId, SignalRange, TransferFunction, VideoDimensions, VideoFormat,
};

static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fm-persistence-v3-{}-{sequence}-{name}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn store(&self, name: &str) -> ProjectStore {
        ProjectStore::new(self.0.join(format!("{name}.freemix"))).unwrap()
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn nz(value: u128) -> NonZeroU128 {
    NonZeroU128::new(value).unwrap()
}
fn project_id(value: u128) -> ProjectId {
    ProjectId::new(nz(value))
}
fn input_id(value: u128) -> InputId {
    InputId::new(nz(value))
}
fn scene_id(value: u128) -> SceneId {
    SceneId::new(nz(value))
}
fn bus_id(value: u128) -> BusId {
    BusId::new(nz(value))
}
fn output_id(value: u128) -> OutputId {
    OutputId::new(nz(value))
}
fn stream_target_id(value: u128) -> StreamTargetId {
    StreamTargetId::new(nz(value))
}

/// The rich fixture's stream key. Nothing but `project.json` may contain it.
const RICH_STREAM_KEY: &str = "live-2f8c41d9-secret";

fn rich_settings() -> ProjectSettings {
    let frame_rate = FrameRate::new(24_000, 1_001).unwrap();
    ProjectSettings {
        frame_rate,
        video: VideoFormat {
            dimensions: VideoDimensions::new(3_840, 2_160).unwrap(),
            frame_rate,
            pixel_format: PixelFormat::P010,
            scan: ScanMode::InterlacedTopFieldFirst,
            color: ColorMetadata {
                primaries: ColorPrimaries::Bt2020,
                transfer: TransferFunction::Pq,
                matrix: MatrixCoefficients::Bt2020NonConstant,
                range: SignalRange::Full,
                chroma_location: ChromaLocation::TopLeft,
            },
        },
        audio: AudioFormat {
            sample_rate: SampleRate::new(96_000).unwrap(),
            sample_format: SampleFormat::I24,
            channels: ChannelLayout::new(vec![
                Channel::Left,
                Channel::Right,
                Channel::Center,
                Channel::LeftSurround,
                Channel::RightSurround,
            ])
            .unwrap(),
        },
    }
}

fn rich_inputs(high: u128) -> [Input; 7] {
    [
        Input {
            id: input_id(high),
            name: "Color".into(),
            kind: InputKind::Color,
            required_capabilities: vec!["gpu.color.fill".into()],
        },
        Input {
            id: input_id(high + 1),
            name: "Media".into(),
            kind: InputKind::Media {
                asset_uri: "asset://opening.mov".into(),
            },
            required_capabilities: vec!["codec.video.hevc".into()],
        },
        Input {
            id: input_id(high + 2),
            name: "Device".into(),
            kind: InputKind::Device {
                stable_key: "decklink:1".into(),
            },
            required_capabilities: vec!["capture.device.sdi".into()],
        },
        Input {
            id: input_id(high + 3),
            name: "Network".into(),
            kind: InputKind::Network {
                endpoint: "srt://example.test:9000".into(),
            },
            required_capabilities: vec!["network.input.srt".into()],
        },
        Input {
            id: input_id(high + 4),
            name: "Solid silence".into(),
            kind: InputKind::Simulated(SimulatedInput::new(
                SimulatedVideo::Solid(SolidColor::new(1, 2, 3, 4)),
                SimulatedAudio::Silence,
            )),
            required_capabilities: Vec::new(),
        },
        Input {
            id: input_id(high + 5),
            name: "Bars tone".into(),
            kind: InputKind::Simulated(SimulatedInput::new(
                SimulatedVideo::Bars,
                SimulatedAudio::Sine {
                    frequency_hz: 1_234,
                },
            )),
            required_capabilities: Vec::new(),
        },
        Input {
            id: input_id(high + 6),
            name: "Scene route".into(),
            kind: InputKind::Scene {
                scene_id: scene_id(high + 11),
                audio_source: Some(input_id(high + 5)),
            },
            required_capabilities: vec!["scene.composite.basic".into()],
        },
    ]
}

fn rich_stream_target(high: u128) -> StreamTarget {
    StreamTarget::new(
        stream_target_id(high + 40),
        "Main ingest".into(),
        StreamProtocol::Rtmps,
        StreamEndpoint::parse("ingest.example.test:443/live").unwrap(),
        StreamKey::parse(RICH_STREAM_KEY).unwrap(),
        output_id(high + 30),
    )
    .unwrap()
    .with_backup_endpoint(Some(
        StreamEndpoint::parse("backup.example.test/live/eu").unwrap(),
    ))
    .unwrap()
    .with_startup(StartupPolicy::ReconcileDesiredState)
}

fn rich_project() -> Project {
    let high = u128::from(u64::MAX) + 101;
    let mut project = Project::new(
        project_id(u128::MAX - 1),
        "Rich production",
        rich_settings(),
    )
    .with_main_mix(MainMix::new(input_id(high), input_id(high + 1)))
    .with_restart_policy(RestartPolicy::OnFailure { max_attempts: 7 });
    for input in rich_inputs(high) {
        project.add_input(input);
    }
    project.add_stinger(StingerConfig::new(
        StingerSlotNumber::new(1).unwrap(),
        input_id(high + 1),
        true,
        17,
        StingerAudioPolicy::StingerOnly,
        StingerMissingMediaFallback::Fade,
    ));
    project.add_stinger(StingerConfig::new(
        StingerSlotNumber::new(8).unwrap(),
        input_id(high + 5),
        false,
        0,
        StingerAudioPolicy::MixWithProgram,
        StingerMissingMediaFallback::KeepProgram,
    ));

    project.add_scene(Scene {
        id: scene_id(high + 10),
        name: "Base".into(),
        background: Rgba8::new(8, 4, 2, 16),
        layers: vec![Layer {
            name: "Camera".into(),
            source: SourceRef::Input(input_id(high + 2)),
            enabled: true,
            geometry: LayerGeometry::new(-20, 30, 1920, 1080, Rotation::Deg90),
            crop: Some(CropRect::new(10, 20, 1000, 700)),
            mask: Some(RectMask::new(11, 12, 500, 300).inverted(true)),
            opacity: 200,
            z_order: -7,
        }],
    });
    project.add_scene(Scene {
        id: scene_id(high + 11),
        name: "Composite".into(),
        background: Rgba8::OPAQUE_BLACK,
        layers: vec![
            Layer {
                name: "Base scene".into(),
                source: SourceRef::Scene(scene_id(high + 10)),
                enabled: true,
                geometry: LayerGeometry::new(0, 0, 3840, 2160, Rotation::Deg0),
                crop: None,
                mask: None,
                opacity: u8::MAX,
                z_order: 3,
            },
            Layer {
                name: "Overlay".into(),
                source: SourceRef::Input(input_id(high + 4)),
                enabled: false,
                geometry: LayerGeometry::new(100, -200, 640, 360, Rotation::Deg270),
                crop: Some(CropRect::new(1, 2, 3, 4)),
                mask: Some(RectMask::new(1, 1, 2, 3)),
                opacity: 127,
                z_order: 3,
            },
        ],
    });
    project.add_audio_bus(AudioBus {
        id: bus_id(high + 20),
        name: "Program".into(),
        sends: vec![BusSend {
            destination: bus_id(high + 21),
        }],
    });
    project.add_audio_bus(AudioBus {
        id: bus_id(high + 21),
        name: "Monitor".into(),
        sends: Vec::new(),
    });
    project.add_output(Output {
        id: output_id(high + 30),
        name: "Primary".into(),
        video_source: scene_id(high + 11),
        audio_source: bus_id(high + 20),
        startup: StartupPolicy::ReconcileDesiredState,
        required_capabilities: vec!["output.network.srt".into()],
    });
    project.add_stream_target(rich_stream_target(high));
    project
}

fn stored_rich_project() -> StoredProject {
    let high = u128::from(u64::MAX) + 101;
    StoredProject::from_project(
        rich_project(),
        RuntimeRouting {
            desired_program_id: Some(input_id(high)),
            realized_program_id: Some(input_id(high + 1)),
            desired_preview_id: Some(input_id(high + 1)),
            realized_preview_id: Some(input_id(high)),
        },
        ProjectPosition {
            revision: 8,
            state_epoch: 3,
            event_sequence: 13,
            frames_rendered: 800,
            runtime_generation: 2,
            clock_time_nanos: 99_000,
        },
        Vec::new(),
    )
    .unwrap()
}

#[test]
fn input_audio_strips_round_trip_exactly_and_reject_malformed_values() {
    let temp = TestDirectory::new("input-audio-strips");
    let store = temp.store("show");
    let mut project = rich_project();
    let input = project.inputs()[1].id;
    let state = InputAudioStripState {
        gain: InputGainMilliDb::new(-12_345).unwrap(),
        balance: InputBalanceBasisPoints::new(2_500).unwrap(),
        delay_samples: InputDelaySamples::new(1_200).unwrap(),
        muted: true,
        soloed: true,
        follow_video: false,
    };
    assert!(project.set_input_audio_strip(input, state));
    let stored = StoredProject::from_project(
        project,
        RuntimeRouting::default(),
        ProjectPosition::default(),
        Vec::new(),
    )
    .unwrap();

    store.save(&stored).unwrap();
    let loaded = store.load().unwrap();
    assert_eq!(loaded.project().input_audio_strip(input), Some(state));
    let encoded = fs::read_to_string(store.manifest_path()).unwrap();
    assert!(
        encoded.contains(
            "\"gain_milli_db\": -12345, \"balance_basis_points\": 2500, \"delay_samples\": 1200, \"muted\": true, \"soloed\": true, \"follow_video\": false"
        )
    );

    let strip_line = encoded
        .lines()
        .find(|line| line.contains("\"gain_milli_db\": -12345"))
        .unwrap();
    for invalid in [
        encoded.replacen("\"gain_milli_db\": -12345", "\"gain_milli_db\": 24001", 1),
        encoded.replacen(
            "\"gain_milli_db\": -12345",
            "\"gain_milli_db\": \"-12345\"",
            1,
        ),
        encoded.replacen(
            "\"balance_basis_points\": 2500",
            "\"balance_basis_points\": 10001",
            1,
        ),
        encoded.replacen(
            "\"balance_basis_points\": 2500",
            "\"balance_basis_points\": \"2500\"",
            1,
        ),
        encoded.replacen("\"delay_samples\": 1200", "\"delay_samples\": 48001", 1),
        encoded.replacen("\"delay_samples\": 1200", "\"delay_samples\": \"1200\"", 1),
        encoded.replacen("\"soloed\": true", "\"soloed\": \"true\"", 1),
        encoded.replacen(
            &format!("\"input\": {input}, \"gain_milli_db\": -12345"),
            "\"input\": 999, \"gain_milli_db\": -12345",
            1,
        ),
        encoded.replacen(strip_line, &format!("{strip_line}\n{strip_line}"), 1),
    ] {
        fs::write(store.manifest_path(), invalid).unwrap();
        assert!(matches!(
            store.load(),
            Err(StoreError::MalformedManifest { .. })
        ));
    }
}

#[test]
fn current_project_round_trip_preserves_formats_graph_capabilities_and_u128_ids() {
    let temp = TestDirectory::new("rich-round-trip");
    let store = temp.store("show");
    let expected = stored_rich_project();

    store.save(&expected).unwrap();
    let loaded = store.load().unwrap();

    assert_eq!(loaded, expected);
    assert_eq!(loaded.project().id().get().get(), u128::MAX - 1);
    assert_eq!(
        loaded.project().inputs()[4].kind,
        expected.project().inputs()[4].kind
    );
    assert_eq!(
        loaded.project().inputs()[5].kind,
        expected.project().inputs()[5].kind
    );
    assert_eq!(loaded.project().settings(), &rich_settings());
    assert_eq!(loaded.runtime_routing(), expected.runtime_routing());
    assert_eq!(
        loaded.project().scenes()[0].layers[0].mask,
        Some(RectMask::new(11, 12, 500, 300).inverted(true))
    );
    assert_eq!(
        loaded.project().scenes()[1].layers[1].mask,
        Some(RectMask::new(1, 1, 2, 3))
    );
    assert!(matches!(
        loaded.project().inputs()[6].kind,
        InputKind::Scene {
            scene_id: id,
            audio_source: Some(audio)
        } if id.get().get() > u128::from(u64::MAX)
            && audio.get().get() > u128::from(u64::MAX)
    ));
}

#[test]
fn stream_destination_round_trips_at_the_current_schema_and_keeps_its_key_off_every_other_surface()
{
    let temp = TestDirectory::new("stream-destination");
    let store = temp.store("show");
    let expected = stored_rich_project();

    store.save(&expected).unwrap();
    let loaded = store.load().unwrap();

    assert_eq!(loaded, expected);
    assert_eq!(loaded.project().schema_version(), CURRENT_SCHEMA_VERSION);
    let target = &loaded.project().stream_targets()[0];
    assert_eq!(target.name(), "Main ingest");
    assert_eq!(target.protocol(), StreamProtocol::Rtmps);
    assert_eq!(target.endpoint().as_str(), "ingest.example.test:443/live");
    assert_eq!(
        target.backup_endpoint().map(StreamEndpoint::as_str),
        Some("backup.example.test/live/eu")
    );
    assert_eq!(target.key().expose_secret(), RICH_STREAM_KEY);
    assert_eq!(target.startup(), StartupPolicy::ReconcileDesiredState);
    assert_eq!(
        target.expose_url(),
        format!("rtmps://ingest.example.test:443/live/{RICH_STREAM_KEY}")
    );

    // The manifest is the one place the key is allowed to appear: the bundle
    // is plaintext by design and must be protected like the credential it now
    // holds. Nothing derived from the loaded project may repeat it.
    let encoded = fs::read_to_string(store.manifest_path()).unwrap();
    assert!(encoded.contains(&format!("\"key\": \"{RICH_STREAM_KEY}\"")));
    for rendered in [
        format!("{target:?}"),
        format!("{target}"),
        format!("{:?}", loaded.project()),
        target.redacted_url(),
        target.redacted_backup_url().unwrap(),
    ] {
        assert!(
            !rendered.contains(RICH_STREAM_KEY),
            "stream key leaked into `{rendered}`"
        );
    }
    assert_eq!(
        target.redacted_url(),
        "rtmps://ingest.example.test:443/live/****"
    );
}

#[test]
fn strict_stream_destination_parser_rejects_missing_wrong_typed_and_out_of_contract_fields() {
    let temp = TestDirectory::new("strict-stream-destination");
    let store = temp.store("show");
    store.save(&stored_rich_project()).unwrap();
    let valid = fs::read_to_string(store.manifest_path()).unwrap();

    for malformed in [
        // Missing, wrong-typed, unknown and duplicated fields.
        valid.replacen("\"protocol\": \"rtmps\",\n        ", "", 1),
        valid.replacen("\"protocol\": \"rtmps\"", "\"protocol\": 443", 1),
        valid.replacen(
            "\"backup_endpoint\":",
            "\"future_field\": 1,\n        \"backup_endpoint\":",
            1,
        ),
        valid.replacen(
            &format!("\"key\": \"{RICH_STREAM_KEY}\""),
            &format!("\"key\": \"{RICH_STREAM_KEY}\", \"key\": \"{RICH_STREAM_KEY}\""),
            1,
        ),
        valid.replacen("\"output\": ", "\"output\": 0, \"ignored\": ", 1),
        // Values outside the destination contract the sink enforces.
        valid.replacen("\"protocol\": \"rtmps\"", "\"protocol\": \"srt\"", 1),
        valid.replacen(
            "\"endpoint\": \"ingest.example.test:443/live\"",
            "\"endpoint\": \"rtmps://ingest.example.test/live\"",
            1,
        ),
        valid.replacen(
            "\"endpoint\": \"ingest.example.test:443/live\"",
            "\"endpoint\": \"operator:hunter2@ingest.example.test/live\"",
            1,
        ),
        valid.replacen(
            "\"endpoint\": \"ingest.example.test:443/live\"",
            "\"endpoint\": \"ingest.example.test\"",
            1,
        ),
        valid.replacen(
            "\"backup_endpoint\": \"backup.example.test/live/eu\"",
            "\"backup_endpoint\": \"backup.example.test/live/\"",
            1,
        ),
        valid.replacen(
            &format!("\"key\": \"{RICH_STREAM_KEY}\""),
            "\"key\": \"ab\"",
            1,
        ),
        valid.replacen(
            &format!("\"key\": \"{RICH_STREAM_KEY}\""),
            "\"key\": \"has/slash\"",
            1,
        ),
        valid.replacen(
            "\"startup\": \"reconcile_desired_state\",\n        \"output\"",
            "\"startup\": \"running\",\n        \"output\"",
            1,
        ),
    ] {
        fs::write(store.manifest_path(), &malformed).unwrap();
        let error = store.load().unwrap_err();
        assert!(
            matches!(error, StoreError::MalformedManifest { .. }),
            "expected a malformed manifest, got {error:?}"
        );
        assert!(
            !error.to_string().contains(RICH_STREAM_KEY),
            "stream key leaked into `{error}`"
        );
    }

    // A destination whose output does not exist is a project-level failure,
    // not a syntax one.
    let missing_output = u128::from(u64::MAX) + 999;
    let current_output = u128::from(u64::MAX) + 131;
    fs::write(
        store.manifest_path(),
        valid.replacen(
            &format!("\"output\": {current_output}"),
            &format!("\"output\": {missing_output}"),
            1,
        ),
    )
    .unwrap();
    assert!(matches!(store.load(), Err(StoreError::Validation(_))));
}

#[test]
fn display_p3_bt709_round_trips_in_current_schema() {
    let temp = TestDirectory::new("bt709-transfer");
    let store = temp.store("show");
    let mut settings = rich_settings();
    settings.video.color.primaries = ColorPrimaries::DisplayP3;
    settings.video.color.transfer = TransferFunction::Bt709;
    let expected = StoredProject::from_project(
        Project::new(project_id(77), "BT.709 transfer", settings),
        RuntimeRouting::default(),
        ProjectPosition::default(),
        Vec::new(),
    )
    .unwrap();

    store.save(&expected).unwrap();
    let loaded = store.load().unwrap();

    assert_eq!(
        loaded.project().settings().video.color.transfer,
        TransferFunction::Bt709
    );
    assert_eq!(
        loaded.project().settings().video.color.primaries,
        ColorPrimaries::DisplayP3
    );
}

#[test]
fn current_encoding_is_deterministic_for_the_complete_model() {
    let temp = TestDirectory::new("deterministic-rich");
    let first = temp.store("first");
    let second = temp.store("second");
    let project = stored_rich_project();
    first.save(&project).unwrap();
    second.save(&project).unwrap();
    assert_eq!(
        fs::read(first.manifest_path()).unwrap(),
        fs::read(second.manifest_path()).unwrap()
    );
}

#[test]
fn malformed_enum_format_and_reference_are_rejected() {
    let temp = TestDirectory::new("malformed-domain");
    let store = temp.store("show");
    store.save(&stored_rich_project()).unwrap();
    let valid = fs::read_to_string(store.manifest_path()).unwrap();

    fs::write(
        store.manifest_path(),
        valid.replacen("\"p010\"", "\"future_pixel\"", 1),
    )
    .unwrap();
    assert!(matches!(
        store.load(),
        Err(StoreError::MalformedManifest { .. })
    ));

    fs::write(
        store.manifest_path(),
        valid.replacen("\"width\": 3840", "\"width\": 0", 1),
    )
    .unwrap();
    assert!(matches!(
        store.load(),
        Err(StoreError::MalformedManifest { .. })
    ));

    let missing_scene = u128::from(u64::MAX) + 999;
    let current_scene = u128::from(u64::MAX) + 112;
    fs::write(
        store.manifest_path(),
        valid.replacen(
            &format!("\"video_source\": {current_scene}"),
            &format!("\"video_source\": {missing_scene}"),
            1,
        ),
    )
    .unwrap();
    assert!(matches!(store.load(), Err(StoreError::Validation(_))));
}

#[test]
fn strict_composition_parser_rejects_unknown_values_ranges_and_fields() {
    let temp = TestDirectory::new("strict-composition");
    let store = temp.store("show");
    store.save(&stored_rich_project()).unwrap();
    let valid = fs::read_to_string(store.manifest_path()).unwrap();

    for malformed in [
        valid.replacen("\"rotation\": \"deg90\"", "\"rotation\": \"deg45\"", 1),
        valid.replacen(
            "\"translation_x\": -20",
            "\"translation_x\": -2147483649",
            1,
        ),
        valid.replacen(
            "\"rotation\": \"deg90\"",
            "\"rotation\": \"deg90\", \"future\": 1",
            1,
        ),
    ] {
        fs::write(store.manifest_path(), malformed).unwrap();
        assert!(matches!(
            store.load(),
            Err(StoreError::MalformedManifest { .. })
        ));
    }

    fs::write(
        store.manifest_path(),
        valid.replacen(
            "\"red\": 8, \"green\": 4, \"blue\": 2, \"alpha\": 16",
            "\"red\": 17, \"green\": 4, \"blue\": 2, \"alpha\": 16",
            1,
        ),
    )
    .unwrap();
    assert!(matches!(store.load(), Err(StoreError::Validation(_))));
}

#[test]
fn strict_rect_mask_parser_rejects_malformed_and_out_of_bounds_values() {
    let temp = TestDirectory::new("strict-mask");
    let store = temp.store("show");
    store.save(&stored_rich_project()).unwrap();
    let valid = fs::read_to_string(store.manifest_path()).unwrap();

    for malformed in [
        valid.replacen("\"invert\": true", "\"invert\": 1", 1),
        valid.replacen("\"invert\": true", "\"invert\": true, \"feather\": 1", 1),
        valid.replacen("\"width\": 500", "\"width\": -1", 1),
    ] {
        fs::write(store.manifest_path(), malformed).unwrap();
        assert!(matches!(
            store.load(),
            Err(StoreError::MalformedManifest { .. })
        ));
    }

    for invalid in [
        valid.replacen("\"width\": 500", "\"width\": 0", 1),
        valid.replacen("\"width\": 500", "\"width\": 1000", 1),
        valid.replacen("\"x\": 11", "\"x\": 4294967295", 1),
    ] {
        fs::write(store.manifest_path(), invalid).unwrap();
        assert!(matches!(store.load(), Err(StoreError::Validation(_))));
    }
}
