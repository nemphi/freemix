use std::num::NonZeroU128;

use fm_model::{
    AudioBus, BusSend, CURRENT_SCHEMA_VERSION, EntityRef, Input, InputKind, Layer, MainMix,
    MigrationInput, OLDEST_SUPPORTED_SCHEMA_VERSION, Output, OutputFormat, Project,
    ProjectSettings, RestartPolicy, SUPPORTED_SCHEMA_VERSIONS, Scene, SchemaVersion,
    SimulatedAudio, SimulatedInput, SimulatedVideo, SolidColor, SourceRef, StartupPolicy,
    ValidationErrorKind,
};
use fm_types::{
    AudioFormat, BusId, ChannelLayout, ColorMetadata, FrameRate, InputId, OutputId, PixelFormat,
    ProjectId, SampleFormat, SampleRate, ScanMode, SceneId, VideoDimensions, VideoFormat,
};

fn project_id(value: u128) -> ProjectId {
    ProjectId::new(NonZeroU128::new(value).unwrap())
}

fn input_id(value: u128) -> InputId {
    InputId::new(NonZeroU128::new(value).unwrap())
}

fn scene_id(value: u128) -> SceneId {
    SceneId::new(NonZeroU128::new(value).unwrap())
}

fn bus_id(value: u128) -> BusId {
    BusId::new(NonZeroU128::new(value).unwrap())
}

fn output_id(value: u128) -> OutputId {
    OutputId::new(NonZeroU128::new(value).unwrap())
}

fn settings() -> ProjectSettings {
    let frame_rate = FrameRate::new(60_000, 1_001).unwrap();
    ProjectSettings {
        frame_rate,
        video: VideoFormat {
            dimensions: VideoDimensions::new(1920, 1080).unwrap(),
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
    }
}

fn valid_project() -> Project {
    let mut project = Project::new(project_id(1), "Main show", settings());
    project.add_input(Input {
        id: input_id(1),
        name: "Camera".into(),
        kind: InputKind::Device {
            stable_key: "camera-1".into(),
        },
        required_capabilities: vec!["capture.camera.raw".into()],
    });
    project.add_scene(Scene {
        id: scene_id(1),
        name: "Wide".into(),
        layers: vec![Layer {
            name: "Camera".into(),
            source: SourceRef::Input(input_id(1)),
            enabled: true,
        }],
    });
    project.add_audio_bus(AudioBus {
        id: bus_id(1),
        name: "Master".into(),
        sends: Vec::new(),
    });
    project.add_output(Output {
        id: output_id(1),
        name: "Program".into(),
        video_source: scene_id(1),
        audio_source: bus_id(1),
        startup: StartupPolicy::Stopped,
        required_capabilities: vec!["gpu.compositor.wgpu".into()],
    });
    project
}

fn simulated_project() -> Project {
    let output_format = OutputFormat {
        video: settings().video,
        audio: settings().audio,
    };
    let mut project = Project::new(
        project_id(2),
        "Simulated show",
        ProjectSettings::new(FrameRate::new(60_000, 1_001).unwrap(), output_format),
    )
    .with_main_mix(MainMix::new(input_id(1), input_id(2)))
    .with_restart_policy(RestartPolicy::OnFailure { max_attempts: 3 });
    project.add_input(Input {
        id: input_id(1),
        name: "Red".into(),
        kind: InputKind::Simulated(SimulatedInput::new(
            SimulatedVideo::Solid(SolidColor::new(255, 0, 0, 255)),
            SimulatedAudio::Silence,
        )),
        required_capabilities: Vec::new(),
    });
    project.add_input(Input {
        id: input_id(2),
        name: "Bars and tone".into(),
        kind: InputKind::Simulated(SimulatedInput::new(
            SimulatedVideo::Bars,
            SimulatedAudio::Sine {
                frequency_hz: 1_000,
            },
        )),
        required_capabilities: Vec::new(),
    });
    project
}

#[test]
fn coherent_project_is_valid() {
    assert_eq!(valid_project().validate(), Ok(()));
}

#[test]
fn complete_simulated_production_is_valid() {
    let project = simulated_project();

    assert_eq!(project.validate(), Ok(()));
    assert_eq!(
        project.main_mix(),
        Some(MainMix::new(input_id(1), input_id(2)))
    );
    assert_eq!(
        project.restart_policy(),
        RestartPolicy::OnFailure { max_attempts: 3 }
    );
    assert_eq!(
        project.settings().output_format().video.frame_rate,
        project.settings().frame_rate
    );
}

#[test]
fn bad_routing_generator_rate_and_format_are_reported() {
    let frame_rate = FrameRate::new(241, 1).unwrap();
    let mismatched_video_rate = FrameRate::new(60, 1).unwrap();
    let mut project = Project::new(
        project_id(3),
        "Broken simulation",
        ProjectSettings {
            frame_rate,
            video: VideoFormat {
                dimensions: VideoDimensions::new(8_193, 1080).unwrap(),
                frame_rate: mismatched_video_rate,
                pixel_format: PixelFormat::Rgba8,
                scan: ScanMode::Progressive,
                color: ColorMetadata::default(),
            },
            audio: AudioFormat {
                sample_rate: SampleRate::new(1_000).unwrap(),
                sample_format: SampleFormat::F32,
                channels: ChannelLayout::stereo(),
            },
        },
    )
    .with_main_mix(MainMix::new(input_id(1), input_id(99)))
    .with_restart_policy(RestartPolicy::OnFailure { max_attempts: 0 });
    project.add_input(Input {
        id: input_id(1),
        name: "Bad tone".into(),
        kind: InputKind::Simulated(SimulatedInput::new(
            SimulatedVideo::Bars,
            SimulatedAudio::Sine { frequency_hz: 0 },
        )),
        required_capabilities: Vec::new(),
    });

    let errors = project.validate().unwrap_err();
    for field in [
        "settings.frame_rate",
        "settings.video.frame_rate",
        "settings.video.dimensions",
        "settings.audio.sample_rate",
        "restart_policy.max_attempts",
        "kind.simulated.audio.frequency_hz",
        "main_mix.desired_preview",
    ] {
        assert!(
            errors.iter().any(|error| error.field == field),
            "missing validation error for {field}"
        );
    }
}

#[test]
fn duplicate_routes_are_rejected() {
    let mut project = simulated_project();
    project.set_main_mix(MainMix::new(input_id(1), input_id(1)));
    project.add_audio_bus(AudioBus {
        id: bus_id(1),
        name: "Master".into(),
        sends: Vec::new(),
    });
    project.add_audio_bus(AudioBus {
        id: bus_id(2),
        name: "Aux".into(),
        sends: vec![
            BusSend {
                destination: bus_id(1),
            },
            BusSend {
                destination: bus_id(1),
            },
        ],
    });

    let errors = project.validate().unwrap_err();
    assert_eq!(
        errors
            .iter()
            .filter(|error| error.kind == ValidationErrorKind::DuplicateReference)
            .count(),
        2
    );
}

#[test]
fn schema_support_window_is_current_plus_previous_two() {
    assert_eq!(CURRENT_SCHEMA_VERSION, SchemaVersion::new(3));
    assert_eq!(OLDEST_SUPPORTED_SCHEMA_VERSION, SchemaVersion::new(1));
    assert_eq!(
        SUPPORTED_SCHEMA_VERSIONS,
        [
            SchemaVersion::new(3),
            SchemaVersion::new(2),
            SchemaVersion::new(1)
        ]
    );
    assert!(CURRENT_SCHEMA_VERSION.is_supported());
    assert!(!CURRENT_SCHEMA_VERSION.requires_migration());
    assert!(SchemaVersion::new(1).requires_migration());
    assert!(!SchemaVersion::new(0).is_supported());
    assert!(!SchemaVersion::new(4).is_supported());

    let input = MigrationInput::new(SchemaVersion::new(2), ("format-neutral", 7_u8));
    assert_eq!(input.schema_version(), SchemaVersion::new(2));
    assert_eq!(input.into_representation(), ("format-neutral", 7));
}

#[test]
fn dangling_references_and_malformed_capabilities_are_reported_together() {
    let mut project = valid_project();
    project.add_output(Output {
        id: output_id(2),
        name: "Broken".into(),
        video_source: scene_id(99),
        audio_source: bus_id(99),
        startup: StartupPolicy::Stopped,
        required_capabilities: vec!["GPU invalid".into()],
    });

    let errors = project.validate().unwrap_err();
    assert!(errors.iter().any(|error| {
        error.kind == ValidationErrorKind::MissingReference(EntityRef::Scene(scene_id(99)))
    }));
    assert!(errors.iter().any(|error| {
        error.kind == ValidationErrorKind::MissingReference(EntityRef::AudioBus(bus_id(99)))
    }));
    assert!(
        errors
            .iter()
            .any(|error| error.kind == ValidationErrorKind::MalformedCapabilityKey)
    );
}

#[test]
fn identifiers_and_names_must_be_unique_per_collection() {
    let mut project = valid_project();
    project.add_input(Input {
        id: input_id(1),
        name: "camera".into(),
        kind: InputKind::Color,
        required_capabilities: Vec::new(),
    });

    let errors = project.validate().unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| error.kind == ValidationErrorKind::DuplicateId)
    );
    assert!(
        errors
            .iter()
            .any(|error| error.kind == ValidationErrorKind::DuplicateName)
    );
}

#[test]
fn nested_scene_cycles_are_rejected() {
    let mut project = valid_project();
    project.add_scene(Scene {
        id: scene_id(2),
        name: "Nested A".into(),
        layers: vec![Layer {
            name: "B".into(),
            source: SourceRef::Scene(scene_id(3)),
            enabled: true,
        }],
    });
    project.add_scene(Scene {
        id: scene_id(3),
        name: "Nested B".into(),
        layers: vec![Layer {
            name: "A".into(),
            source: SourceRef::Scene(scene_id(2)),
            enabled: true,
        }],
    });

    let errors = project.validate().unwrap_err();
    assert_eq!(
        errors
            .iter()
            .filter(|error| error.kind == ValidationErrorKind::Cycle)
            .count(),
        2
    );
}

#[test]
fn audio_bus_cycles_are_rejected() {
    let mut project = valid_project();
    project.add_audio_bus(AudioBus {
        id: bus_id(2),
        name: "Aux A".into(),
        sends: vec![BusSend {
            destination: bus_id(3),
        }],
    });
    project.add_audio_bus(AudioBus {
        id: bus_id(3),
        name: "Aux B".into(),
        sends: vec![BusSend {
            destination: bus_id(2),
        }],
    });

    let errors = project.validate().unwrap_err();
    assert_eq!(
        errors
            .iter()
            .filter(|error| error.kind == ValidationErrorKind::Cycle)
            .count(),
        2
    );
}
