use std::num::NonZeroU128;

use fm_model::{
    AddInputError, AddSceneInputError, AddSceneLayerError, AudioBus, BusSend,
    CURRENT_SCHEMA_VERSION, CropRect, DuplicateSceneInputError, EntityRef, Input,
    InputAudioStripState, InputBalanceBasisPoints, InputDelaySamples, InputGainMilliDb, InputKind,
    Layer, LayerGeometry, MainMix, Output, OutputFormat, Project, ProjectSettings, RectMask,
    RemoveAudioBusError, RemoveInputError, RemoveOutputError, RemoveSceneError, RenameSceneError,
    RestartPolicy, Rgba8, Rotation, Scene, SceneLayerError, SetStingerError, SimulatedAudio,
    SimulatedInput, SimulatedVideo, SolidColor, SourceRef, StartupPolicy, StingerAudioPolicy,
    StingerConfig, StingerMissingMediaFallback, StingerSlotNumber, ValidationErrorKind,
};
use fm_types::{
    AudioFormat, BusId, ChannelLayout, ColorMetadata, FrameRate, InputId, MAX_INPUT_NAME_BYTES,
    OutputId, PixelFormat, ProjectId, SampleFormat, SampleRate, ScanMode, SceneId, VideoDimensions,
    VideoFormat,
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

fn identity_geometry() -> LayerGeometry {
    LayerGeometry::new(0, 0, 1920, 1080, Rotation::Deg0)
}

fn layer(name: &str, source: SourceRef) -> Layer {
    Layer {
        name: name.into(),
        source,
        enabled: true,
        geometry: identity_geometry(),
        crop: None,
        mask: None,
        opacity: u8::MAX,
        z_order: 0,
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
        background: Rgba8::OPAQUE_BLACK,
        layers: vec![layer("Camera", SourceRef::Input(input_id(1)))],
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

#[test]
fn scene_layer_crop_checked_validation_is_atomic_and_mask_aware() {
    let scene = scene_id(1);
    let mut project = valid_project();
    project
        .set_scene_layer_crop(scene, 0, Some(CropRect::new(0, 0, 1920, 1080)))
        .unwrap();
    project
        .set_scene_layer_mask(scene, 0, Some(RectMask::new(10, 20, 100, 100)))
        .unwrap();
    let before = project.clone();

    for (crop, error) in [
        (
            CropRect::new(0, 0, 0, 10),
            SceneLayerError::InvalidCrop { scene, index: 0 },
        ),
        (
            CropRect::new(1900, 0, 21, 10),
            SceneLayerError::InvalidCrop { scene, index: 0 },
        ),
        (
            CropRect::new(u32::MAX, 0, 1, 10),
            SceneLayerError::InvalidCrop { scene, index: 0 },
        ),
        (
            CropRect::new(0, 0, 20, 20),
            SceneLayerError::CropWouldInvalidateMask { scene, index: 0 },
        ),
    ] {
        assert_eq!(
            project.set_scene_layer_crop(scene, 0, Some(crop)),
            Err(error)
        );
        assert_eq!(project, before);
        assert_eq!(project.scenes()[0].layers, before.scenes()[0].layers);
    }

    project
        .set_scene_layer_crop(scene, 0, Some(CropRect::new(100, 100, 640, 480)))
        .unwrap();
    assert_eq!(
        project.scenes()[0].layers[0].crop,
        Some(CropRect::new(100, 100, 640, 480))
    );
    project.set_scene_layer_crop(scene, 0, None).unwrap();
    assert_eq!(project.scenes()[0].layers[0].crop, None);
}

#[test]
fn scene_layer_mask_checked_validation_is_atomic_and_crop_aware() {
    let scene = scene_id(1);
    let mut project = valid_project();
    let before = project.clone();
    for (mask, error) in [
        (
            RectMask::new(0, 0, 0, 1),
            SceneLayerError::InvalidMask { scene, index: 0 },
        ),
        (
            RectMask::new(1_900, 0, 21, 1),
            SceneLayerError::InvalidMask { scene, index: 0 },
        ),
        (
            RectMask::new(u32::MAX, 0, 1, 1),
            SceneLayerError::InvalidMask { scene, index: 0 },
        ),
    ] {
        assert_eq!(
            project.set_scene_layer_mask(scene, 0, Some(mask)),
            Err(error)
        );
        assert_eq!(project, before);
    }
    project
        .set_scene_layer_crop(scene, 0, Some(CropRect::new(100, 100, 640, 480)))
        .unwrap();
    let before_crop_failure = project.clone();
    assert_eq!(
        project.set_scene_layer_mask(scene, 0, Some(RectMask::new(639, 0, 2, 1))),
        Err(SceneLayerError::InvalidMask { scene, index: 0 })
    );
    assert_eq!(project, before_crop_failure);
    project
        .set_scene_layer_mask(scene, 0, Some(RectMask::new(0, 0, 640, 480)))
        .unwrap();
    project.set_scene_layer_mask(scene, 0, None).unwrap();
    assert_eq!(project.scenes()[0].layers[0].mask, None);
}

#[test]
fn scene_layer_geometry_checked_validation_is_atomic() {
    let scene = scene_id(1);
    let mut project = valid_project();
    let before = project.clone();
    for (width, height) in [
        (0, 100),
        (100, 0),
        (ProjectSettings::MAX_VIDEO_WIDTH + 1, 100),
        (100, ProjectSettings::MAX_VIDEO_HEIGHT + 1),
    ] {
        let geometry = LayerGeometry::new(1, 2, width, height, Rotation::Deg90);
        assert_eq!(
            project.set_scene_layer_geometry(scene, 0, geometry),
            Err(SceneLayerError::InvalidGeometry { scene, index: 0 })
        );
        assert_eq!(project, before);
    }

    let geometry = LayerGeometry::new(-12, 34, 640, 480, Rotation::Deg270);
    project
        .set_scene_layer_geometry(scene, 0, geometry)
        .unwrap();
    assert_eq!(project.scenes()[0].layers[0].geometry, geometry);
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
fn add_scene_layer_rejects_missing_sources_and_cycles_without_mutation() {
    let mut project = valid_project();
    let before = project.clone();
    assert_eq!(
        project.add_layer_to_scene(scene_id(1), layer(" \t", SourceRef::Input(input_id(99)))),
        Err(AddSceneLayerError::EmptyName)
    );
    assert_eq!(project, before);

    for (source, error) in [
        (
            SourceRef::Input(input_id(99)),
            AddSceneLayerError::MissingInput(input_id(99)),
        ),
        (
            SourceRef::Scene(scene_id(99)),
            AddSceneLayerError::MissingScene(scene_id(99)),
        ),
        (
            SourceRef::Scene(scene_id(1)),
            AddSceneLayerError::SourceCycle,
        ),
    ] {
        let before = project.clone();
        assert_eq!(
            project.add_layer_to_scene(scene_id(1), layer("rejected", source)),
            Err(error)
        );
        assert_eq!(project, before);
    }

    project.add_scene(Scene {
        id: scene_id(2),
        name: "Nested".into(),
        background: Rgba8::OPAQUE_BLACK,
        layers: vec![layer("back", SourceRef::Scene(scene_id(1)))],
    });
    project.add_input(Input {
        id: input_id(2),
        name: "Nested input".into(),
        kind: InputKind::Scene {
            scene_id: scene_id(2),
            audio_source: None,
        },
        required_capabilities: Vec::new(),
    });
    let before = project.clone();
    assert_eq!(
        project.add_layer_to_scene(scene_id(1), layer("cross", SourceRef::Input(input_id(2)))),
        Err(AddSceneLayerError::SourceCycle)
    );
    assert_eq!(project, before);

    let appended = layer("accepted", SourceRef::Input(input_id(1)));
    project
        .add_layer_to_scene(scene_id(1), appended.clone())
        .unwrap();
    assert_eq!(project.scenes()[0].layers.last(), Some(&appended));
}

#[test]
fn rename_scene_preserves_exact_text_and_rejects_invalid_names_without_mutation() {
    let mut project = Project::new(project_id(80), "Rename", settings());
    project.add_scene(Scene {
        id: scene_id(1),
        name: "Wide".into(),
        background: Rgba8::OPAQUE_BLACK,
        layers: Vec::new(),
    });
    project.add_scene(Scene {
        id: scene_id(2),
        name: "Close".into(),
        background: Rgba8::OPAQUE_BLACK,
        layers: Vec::new(),
    });
    project
        .rename_scene(scene_id(1), "  Exact scene  ".into())
        .unwrap();
    assert_eq!(project.scenes()[0].name, "  Exact scene  ");

    for (scene, name, error) in [
        (
            scene_id(99),
            "Unused",
            RenameSceneError::UnknownScene(scene_id(99)),
        ),
        (scene_id(1), " \t ", RenameSceneError::EmptyName),
        (scene_id(1), "close", RenameSceneError::DuplicateName),
    ] {
        let before = project.clone();
        assert_eq!(project.rename_scene(scene, name.into()), Err(error));
        assert_eq!(project, before);
    }
}

#[test]
fn duplicate_scene_input_is_atomic_and_preserves_layer_fields() {
    use DuplicateSceneInputError::*;

    let mut project = Project::new(project_id(81), "Duplicate", settings());
    project.add_input(Input {
        id: input_id(1),
        name: "Camera".into(),
        kind: InputKind::Color,
        required_capabilities: vec!["capture.camera".into()],
    });
    let mut source_layer = layer("Inset", SourceRef::Input(input_id(1)));
    source_layer.enabled = false;
    source_layer.geometry = LayerGeometry::new(10, 20, 640, 360, Rotation::Deg90);
    source_layer.crop = Some(CropRect::new(100, 50, 300, 200));
    source_layer.mask = Some(RectMask::new(20, 30, 100, 80).inverted(true));
    source_layer.opacity = 200;
    source_layer.z_order = -3;
    project.add_scene(Scene {
        id: scene_id(1),
        name: "Source".into(),
        background: Rgba8::new(8, 4, 2, 16),
        layers: vec![source_layer],
    });
    project
        .duplicate_scene_input_checked(
            scene_id(1),
            scene_id(2),
            "Shared".into(),
            input_id(2),
            "Shared".into(),
        )
        .unwrap();
    let mut expected_scene = project.scenes()[0].clone();
    expected_scene.id = scene_id(2);
    expected_scene.name = "Shared".into();
    assert_eq!(
        project.scenes(),
        &[project.scenes()[0].clone(), expected_scene]
    );
    assert_eq!(project.inputs()[1].id, input_id(2));
    assert_eq!(project.inputs()[1].name, "Shared");
    assert_eq!(
        project.inputs()[1].kind,
        InputKind::Scene {
            scene_id: scene_id(2),
            audio_source: None,
        }
    );
    assert!(project.inputs()[1].required_capabilities.is_empty());
    assert_eq!(
        project.input_audio_strip(input_id(2)),
        Some(Default::default())
    );

    let mut reject = |source: SceneId,
                      new_scene: SceneId,
                      new_input: InputId,
                      scene_name: &str,
                      input_name: &str,
                      error: DuplicateSceneInputError| {
        let before = project.clone();
        assert_eq!(
            project.duplicate_scene_input_checked(
                source,
                new_scene,
                scene_name.to_owned(),
                new_input,
                input_name.to_owned(),
            ),
            Err(error),
        );
        assert_eq!(project, before);
    };
    let (s1, s3, s99) = (scene_id(1), scene_id(3), scene_id(99));
    let (i1, i3) = (input_id(1), input_id(3));
    reject(s99, s3, i3, "New", "New", UnknownSourceScene(s99));
    reject(s1, s1, i3, "New", "New", DuplicateSceneId(s1));
    reject(s1, s3, i1, "New", "New", DuplicateInputId(i1));
    reject(s1, s3, i3, " ", "New", EmptySceneName);
    reject(s1, s3, i3, "source", "New", DuplicateSceneName);
    reject(s1, s3, i3, "New", "\t", EmptyInputName);
    reject(
        s1,
        s3,
        i3,
        "New",
        &"x".repeat(MAX_INPUT_NAME_BYTES + 1),
        InputNameTooLong,
    );
    reject(s1, s3, i3, "New", "Camera", DuplicateInputName);
}

#[test]
fn add_scene_input_is_atomic_and_uses_current_name_contract() {
    use AddSceneInputError::*;

    let mut project = valid_project();
    project
        .add_scene_input_checked(scene_id(2), "  Exact  ".into(), input_id(2), "Exact".into())
        .unwrap();
    assert_eq!(project.scenes()[1].name, "  Exact  ");
    assert_eq!(project.inputs()[1].name, "Exact");
    assert_eq!(project.scenes()[1].background, Rgba8::OPAQUE_BLACK);
    assert!(matches!(
        project.inputs()[1].kind,
        InputKind::Scene {
            scene_id: added_scene,
            audio_source: None
        } if added_scene == scene_id(2)
    ));
    assert!(project.inputs()[1].required_capabilities.is_empty());
    assert_eq!(
        project.input_audio_strip(input_id(2)),
        Some(Default::default())
    );

    let mut reject = |scene: SceneId, input: InputId, scene_name: &str, input_name: &str, error| {
        let before = project.clone();
        assert_eq!(
            project.add_scene_input_checked(
                scene,
                scene_name.to_owned(),
                input,
                input_name.to_owned(),
            ),
            Err(error)
        );
        assert_eq!(project, before);
    };
    for (scene, input, scene_name, input_name, error) in [
        (
            scene_id(2),
            input_id(3),
            "New",
            "New",
            DuplicateSceneId(scene_id(2)),
        ),
        (
            scene_id(3),
            input_id(1),
            "New",
            "New",
            DuplicateInputId(input_id(1)),
        ),
        (scene_id(3), input_id(3), " ", "New", EmptySceneName),
        (scene_id(3), input_id(3), "wide", "New", DuplicateSceneName),
        (scene_id(3), input_id(3), "New", "\t", EmptyInputName),
        (
            scene_id(3),
            input_id(3),
            "New",
            "Camera",
            DuplicateInputName,
        ),
    ] {
        reject(scene, input, scene_name, input_name, error);
    }
    reject(
        scene_id(3),
        input_id(3),
        "New",
        &"x".repeat(MAX_INPUT_NAME_BYTES + 1),
        InputNameTooLong,
    );

    let mut invalid = project.clone();
    invalid.add_input(Input {
        id: input_id(9),
        name: "x".repeat(MAX_INPUT_NAME_BYTES + 1),
        kind: InputKind::Color,
        required_capabilities: Vec::new(),
    });
    let expected = invalid.validate().unwrap_err();
    let before = invalid.clone();
    assert_eq!(
        invalid.add_scene_input_checked(scene_id(9), "New".into(), input_id(10), "New".into()),
        Err(InvalidProject(expected))
    );
    assert_eq!(invalid, before);
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
fn remove_input_removes_pair_and_rejects_domain_references() {
    let mut base = Project::new(project_id(70), "Remove", settings());
    for id in [1, 2, 3] {
        base.add_input(Input {
            id: input_id(id),
            name: format!("Input {id}"),
            kind: InputKind::Simulated(SimulatedInput::new(
                SimulatedVideo::Bars,
                SimulatedAudio::Silence,
            )),
            required_capabilities: Vec::new(),
        });
    }
    let mut unused = base.clone();
    unused.remove_input(input_id(3)).unwrap();
    assert!(!unused.inputs().iter().any(|input| input.id == input_id(3)));
    assert!(
        !unused
            .input_audio_strips()
            .iter()
            .any(|strip| strip.input == input_id(3))
    );

    let mut cases: Vec<Box<dyn FnOnce(&mut Project)>> = vec![
        Box::new(|project| project.set_main_mix(MainMix::new(input_id(3), input_id(1)))),
        Box::new(|project| {
            project.add_stinger(StingerConfig::new(
                StingerSlotNumber::new(1).unwrap(),
                input_id(3),
                false,
                0,
                StingerAudioPolicy::Muted,
                StingerMissingMediaFallback::Cut,
            ))
        }),
        Box::new(|project| {
            project.add_scene(Scene {
                id: scene_id(3),
                name: "Scene".into(),
                background: Rgba8::OPAQUE_BLACK,
                layers: vec![layer("Input", SourceRef::Input(input_id(3)))],
            })
        }),
        Box::new(|project| {
            project.add_input(Input {
                id: input_id(4),
                name: "Scene input".into(),
                kind: InputKind::Scene {
                    scene_id: scene_id(3),
                    audio_source: Some(input_id(3)),
                },
                required_capabilities: Vec::new(),
            })
        }),
    ];
    for configure in cases.drain(..) {
        let mut project = base.clone();
        configure(&mut project);
        assert_eq!(
            project.remove_input(input_id(3)),
            Err(RemoveInputError::DomainReference(input_id(3)))
        );
    }

    let mut scene_project = base.clone();
    scene_project.add_scene(Scene {
        id: scene_id(4),
        name: "Scene input owner".into(),
        background: Rgba8::OPAQUE_BLACK,
        layers: Vec::new(),
    });
    scene_project.add_input(Input {
        id: input_id(4),
        name: "Scene input".into(),
        kind: InputKind::Scene {
            scene_id: scene_id(4),
            audio_source: None,
        },
        required_capabilities: Vec::new(),
    });
    let before = scene_project.clone();
    assert_eq!(
        scene_project.remove_input(input_id(4)),
        Err(RemoveInputError::DomainReference(input_id(4)))
    );
    assert_eq!(scene_project, before);
}

#[test]
fn add_input_checked_is_atomic_and_preserves_exact_name() {
    let mut project = valid_project();
    let candidate = |id: u128, name: &str| Input {
        id: input_id(id),
        name: name.into(),
        kind: InputKind::Simulated(SimulatedInput::new(
            SimulatedVideo::Bars,
            SimulatedAudio::Silence,
        )),
        required_capabilities: Vec::new(),
    };
    project
        .add_input_checked(candidate(2, "Exact  name  "))
        .unwrap();
    assert_eq!(project.inputs()[1].name, "Exact  name  ");
    assert_eq!(
        project.input_audio_strip(input_id(2)),
        Some(Default::default())
    );
    assert_eq!(
        project
            .inputs()
            .iter()
            .map(|input| input.id)
            .collect::<Vec<_>>(),
        vec![input_id(1), input_id(2)]
    );
    project.add_input_checked(candidate(3, "camera")).unwrap();
    for (input, error) in [
        (
            candidate(1, "Other"),
            AddInputError::DuplicateId(input_id(1)),
        ),
        (candidate(4, "   "), AddInputError::EmptyName),
        (
            candidate(4, &"x".repeat(MAX_INPUT_NAME_BYTES + 1)),
            AddInputError::NameTooLong,
        ),
        (candidate(4, "Exact  name  "), AddInputError::DuplicateName),
    ] {
        let before = project.clone();
        assert_eq!(project.add_input_checked(input), Err(error));
        assert_eq!(project, before);
    }
}

#[test]
fn remove_outputs_and_audio_buses_preserves_order_and_rejects_references() {
    let mut base = valid_project();
    base.add_audio_bus(AudioBus {
        id: bus_id(2),
        name: "Aux".into(),
        sends: Vec::new(),
    });
    base.add_output(Output {
        id: output_id(2),
        name: "Auxiliary".into(),
        video_source: scene_id(1),
        audio_source: bus_id(2),
        startup: StartupPolicy::Stopped,
        required_capabilities: Vec::new(),
    });

    let mut unknown_output = base.clone();
    let snapshot = unknown_output.clone();
    assert_eq!(
        unknown_output.remove_output(output_id(99)),
        Err(RemoveOutputError::UnknownOutput(output_id(99)))
    );
    assert_eq!(unknown_output, snapshot);

    let mut output = base.clone();
    output.remove_output(output_id(1)).unwrap();
    assert_eq!(
        output
            .outputs()
            .iter()
            .map(|item| item.id)
            .collect::<Vec<_>>(),
        vec![output_id(2)]
    );

    for (bus, expected) in [
        (bus_id(99), RemoveAudioBusError::UnknownBus(bus_id(99))),
        (
            bus_id(1),
            RemoveAudioBusError::OutputReference {
                bus: bus_id(1),
                output: output_id(1),
            },
        ),
    ] {
        let mut project = base.clone();
        let snapshot = project.clone();
        assert_eq!(project.remove_audio_bus(bus), Err(expected));
        assert_eq!(project, snapshot);
    }

    let mut outgoing = valid_project();
    outgoing.remove_output(output_id(1)).unwrap();
    outgoing.add_audio_bus(AudioBus {
        id: bus_id(2),
        name: "Aux".into(),
        sends: vec![BusSend {
            destination: bus_id(3),
        }],
    });
    outgoing.add_audio_bus(AudioBus {
        id: bus_id(3),
        name: "Record".into(),
        sends: Vec::new(),
    });
    let snapshot = outgoing.clone();
    assert_eq!(
        outgoing.remove_audio_bus(bus_id(2)),
        Err(RemoveAudioBusError::OutgoingSend {
            bus: bus_id(2),
            destination: bus_id(3),
        })
    );
    assert_eq!(outgoing, snapshot);

    let mut incoming = valid_project();
    incoming.remove_output(output_id(1)).unwrap();
    incoming.add_audio_bus(AudioBus {
        id: bus_id(2),
        name: "Aux".into(),
        sends: vec![BusSend {
            destination: bus_id(3),
        }],
    });
    incoming.add_audio_bus(AudioBus {
        id: bus_id(3),
        name: "Record".into(),
        sends: Vec::new(),
    });
    let snapshot = incoming.clone();
    assert_eq!(
        incoming.remove_audio_bus(bus_id(3)),
        Err(RemoveAudioBusError::IncomingSend {
            bus: bus_id(3),
            source: bus_id(2),
        })
    );
    assert_eq!(incoming, snapshot);

    let mut ordered = valid_project();
    ordered.remove_output(output_id(1)).unwrap();
    ordered.add_audio_bus(AudioBus {
        id: bus_id(2),
        name: "Aux".into(),
        sends: Vec::new(),
    });
    ordered.add_audio_bus(AudioBus {
        id: bus_id(3),
        name: "Record".into(),
        sends: Vec::new(),
    });
    ordered.remove_audio_bus(bus_id(2)).unwrap();
    assert_eq!(
        ordered
            .audio_buses()
            .iter()
            .map(|item| item.id)
            .collect::<Vec<_>>(),
        vec![bus_id(1), bus_id(3)]
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
fn project_uses_the_current_schema_contract() {
    assert_eq!(valid_project().schema_version(), CURRENT_SCHEMA_VERSION);
}

#[test]
fn stinger_slots_are_unique_and_reference_project_inputs() {
    let mut project = valid_project();
    let slot = StingerSlotNumber::new(1).unwrap();
    let config = |media_input| {
        StingerConfig::new(
            slot,
            media_input,
            true,
            12,
            StingerAudioPolicy::MixWithProgram,
            StingerMissingMediaFallback::Fade,
        )
    };
    project.add_stinger(config(input_id(1)));
    assert!(project.validate().is_ok());

    project.add_stinger(config(input_id(99)));
    let errors = project.validate().unwrap_err();
    assert!(errors.iter().any(|error| {
        error.field == "stingers.slot" && error.kind == ValidationErrorKind::DuplicateId
    }));
    assert!(errors.iter().any(|error| {
        error.field == "stingers.media_input"
            && error.kind == ValidationErrorKind::MissingReference(EntityRef::Input(input_id(99)))
    }));
    assert_eq!(StingerSlotNumber::new(0), None);
    assert_eq!(StingerSlotNumber::new(9), None);
}

#[test]
fn stinger_slots_can_be_reconfigured_and_removed_without_duplicates() {
    let mut project = simulated_project();
    let first = StingerConfig::new(
        StingerSlotNumber::new(1).unwrap(),
        input_id(1),
        true,
        12,
        StingerAudioPolicy::Muted,
        StingerMissingMediaFallback::Cut,
    );
    let replacement = StingerConfig::new(
        first.slot,
        input_id(2),
        false,
        24,
        StingerAudioPolicy::MixWithProgram,
        StingerMissingMediaFallback::KeepProgram,
    );

    project.set_stinger(first);
    project.set_stinger(replacement);
    assert_eq!(project.stingers(), &[replacement]);
    assert_eq!(project.remove_stinger(first.slot), Some(replacement));
    assert!(project.stingers().is_empty());
    assert_eq!(project.remove_stinger(first.slot), None);
    assert_eq!(project.validate(), Ok(()));
}

#[test]
fn set_stinger_checked_rejects_unknown_input_without_mutation() {
    let mut project = valid_project();
    let config = |slot, input, cut| {
        StingerConfig::new(
            StingerSlotNumber::new(slot).unwrap(),
            input_id(input),
            false,
            cut,
            StingerAudioPolicy::Muted,
            StingerMissingMediaFallback::Cut,
        )
    };
    project.set_stinger_checked(config(1, 1, 3)).unwrap();
    let before = project.clone();
    let unknown = config(1, 99, 9);
    assert_eq!(
        project.set_stinger_checked(unknown),
        Err(SetStingerError::UnknownInput(input_id(99)))
    );
    assert_eq!(project, before);

    let second = config(2, 1, 7);
    project.set_stinger_checked(second).unwrap();
    let replacement = config(1, 1, 11);
    project.set_stinger_checked(replacement).unwrap();
    assert_eq!(project.stingers(), &[replacement, second]);
}

#[test]
fn persisted_input_gain_is_exact_and_bounded() {
    assert_eq!(
        InputGainMilliDb::new(InputGainMilliDb::MIN).unwrap().get(),
        -96_000
    );
    assert_eq!(InputGainMilliDb::UNITY.get(), 0);
    assert_eq!(
        InputGainMilliDb::new(InputGainMilliDb::MAX).unwrap().get(),
        24_000
    );
    assert_eq!(InputGainMilliDb::new(InputGainMilliDb::MIN - 1), None);
    assert_eq!(InputGainMilliDb::new(InputGainMilliDb::MAX + 1), None);
    assert_eq!(
        InputBalanceBasisPoints::new(InputBalanceBasisPoints::MIN)
            .unwrap()
            .get(),
        -10_000
    );
    assert_eq!(
        InputBalanceBasisPoints::new(InputBalanceBasisPoints::MAX)
            .unwrap()
            .get(),
        10_000
    );
    assert_eq!(
        InputBalanceBasisPoints::new(InputBalanceBasisPoints::MIN - 1),
        None
    );
    assert_eq!(
        InputBalanceBasisPoints::new(InputBalanceBasisPoints::MAX + 1),
        None
    );
    assert_eq!(InputDelaySamples::new(0), Some(InputDelaySamples::ZERO));
    assert_eq!(
        InputDelaySamples::new(InputDelaySamples::MAX)
            .unwrap()
            .get(),
        48_000
    );
    assert_eq!(InputDelaySamples::new(InputDelaySamples::MAX + 1), None);
    assert_eq!(
        InputAudioStripState::default(),
        InputAudioStripState {
            gain: InputGainMilliDb::UNITY,
            balance: InputBalanceBasisPoints::CENTER,
            delay_samples: InputDelaySamples::ZERO,
            muted: false,
            soloed: false,
            follow_video: true,
        }
    );
}

#[test]
fn rectangular_masks_use_half_open_post_crop_source_bounds() {
    let project_with_mask = |mask| {
        let mut project = Project::new(project_id(99), "Mask bounds", settings());
        project.add_input(Input {
            id: input_id(99),
            name: "Source".into(),
            kind: InputKind::Color,
            required_capabilities: Vec::new(),
        });
        let mut masked = layer("Masked", SourceRef::Input(input_id(99)));
        masked.crop = Some(CropRect::new(100, 50, 300, 200));
        masked.mask = Some(mask);
        project.add_scene(Scene {
            id: scene_id(99),
            name: "Masked scene".into(),
            background: Rgba8::OPAQUE_BLACK,
            layers: vec![masked],
        });
        project
    };

    assert_eq!(
        project_with_mask(RectMask::new(299, 199, 1, 1).inverted(true)).validate(),
        Ok(())
    );

    for mask in [
        RectMask::new(0, 0, 0, 1),
        RectMask::new(0, 0, 1, 0),
        RectMask::new(300, 0, 1, 1),
        RectMask::new(0, 200, 1, 1),
        RectMask::new(u32::MAX, 0, 2, 1),
        RectMask::new(0, u32::MAX, 1, 2),
    ] {
        assert!(
            project_with_mask(mask)
                .validate()
                .unwrap_err()
                .iter()
                .any(|error| {
                    error.field == "layers.mask" && error.kind == ValidationErrorKind::OutOfRange
                }),
            "mask {mask:?} was accepted"
        );
    }
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
        id: input_id(2),
        name: "camera".into(),
        kind: InputKind::Color,
        required_capabilities: Vec::new(),
    });
    assert_eq!(project.validate(), Ok(()));

    project.add_input(Input {
        id: input_id(1),
        name: "Camera copy".into(),
        kind: InputKind::Color,
        required_capabilities: Vec::new(),
    });
    project.add_input(Input {
        id: input_id(3),
        name: "Camera".into(),
        kind: InputKind::Color,
        required_capabilities: Vec::new(),
    });
    project.add_input(Input {
        id: input_id(4),
        name: "x".repeat(MAX_INPUT_NAME_BYTES + 1),
        kind: InputKind::Color,
        required_capabilities: Vec::new(),
    });

    let errors = project.validate().unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| error.kind == ValidationErrorKind::DuplicateId)
    );
    assert!(errors.iter().any(|error| {
        error.entity == Some(EntityRef::Input(input_id(3)))
            && error.kind == ValidationErrorKind::DuplicateName
    }));
    assert!(errors.iter().any(|error| {
        error.entity == Some(EntityRef::Input(input_id(4)))
            && error.field == "name"
            && error.kind == ValidationErrorKind::OutOfRange
    }));
}

#[test]
fn nested_scene_cycles_are_rejected() {
    let mut project = valid_project();
    project.add_scene(Scene {
        id: scene_id(2),
        name: "Nested A".into(),
        background: Rgba8::OPAQUE_BLACK,
        layers: vec![layer("B", SourceRef::Scene(scene_id(3)))],
    });
    project.add_scene(Scene {
        id: scene_id(3),
        name: "Nested B".into(),
        background: Rgba8::OPAQUE_BLACK,
        layers: vec![layer("A", SourceRef::Scene(scene_id(2)))],
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
fn scene_inputs_validate_full_width_scene_and_audio_references() {
    let high = u128::from(u64::MAX) + 100;
    let mut project = Project::new(project_id(high), "Scene routing", settings());
    project.add_input(Input {
        id: input_id(high + 1),
        name: "Audio".into(),
        kind: InputKind::Color,
        required_capabilities: Vec::new(),
    });
    project.add_input(Input {
        id: input_id(high + 2),
        name: "Scene input".into(),
        kind: InputKind::Scene {
            scene_id: scene_id(high + 3),
            audio_source: Some(input_id(high + 1)),
        },
        required_capabilities: Vec::new(),
    });
    project.add_scene(Scene {
        id: scene_id(high + 3),
        name: "Scene".into(),
        background: Rgba8::new(8, 4, 2, 16),
        layers: Vec::new(),
    });

    assert_eq!(project.validate(), Ok(()));
}

#[test]
fn scene_input_missing_scene_and_audio_references_are_reported() {
    let mut project = Project::new(project_id(9), "Missing scene routes", settings());
    project.add_input(Input {
        id: input_id(9),
        name: "Broken scene input".into(),
        kind: InputKind::Scene {
            scene_id: scene_id(99),
            audio_source: Some(input_id(98)),
        },
        required_capabilities: Vec::new(),
    });

    let errors = project.validate().unwrap_err();
    assert!(errors.iter().any(|error| {
        error.kind == ValidationErrorKind::MissingReference(EntityRef::Scene(scene_id(99)))
    }));
    assert!(errors.iter().any(|error| {
        error.kind == ValidationErrorKind::MissingReference(EntityRef::Input(input_id(98)))
    }));
}

#[test]
fn scene_removal_checks_references_in_order_without_mutation() {
    let mut project = valid_project();
    project.add_scene(Scene {
        id: scene_id(2),
        name: "Close".into(),
        background: Rgba8::OPAQUE_BLACK,
        layers: Vec::new(),
    });
    project.add_scene(Scene {
        id: scene_id(3),
        name: "Wide 2".into(),
        background: Rgba8::OPAQUE_BLACK,
        layers: vec![layer("Close", SourceRef::Scene(scene_id(2)))],
    });
    project.add_input(Input {
        id: input_id(2),
        name: "Close input".into(),
        kind: InputKind::Scene {
            scene_id: scene_id(2),
            audio_source: None,
        },
        required_capabilities: Vec::new(),
    });
    project
        .set_output_route(output_id(1), scene_id(2), bus_id(1))
        .unwrap();
    let original = project.clone();
    assert_eq!(
        project.remove_scene(scene_id(2)),
        Err(RemoveSceneError::InputReference {
            input: input_id(2),
            scene: scene_id(2)
        })
    );
    assert_eq!(project, original);
    project.remove_input(input_id(2)).unwrap();
    assert_eq!(
        project.remove_scene(scene_id(2)),
        Err(RemoveSceneError::LayerReference {
            owner: scene_id(3),
            source: scene_id(2)
        })
    );
    assert_eq!(
        project
            .scenes()
            .iter()
            .map(|scene| scene.id)
            .collect::<Vec<_>>(),
        vec![scene_id(1), scene_id(2), scene_id(3)]
    );
    project
        .set_scene_layer_source(scene_id(3), 0, SourceRef::Input(input_id(1)))
        .unwrap();
    assert_eq!(
        project.remove_scene(scene_id(2)),
        Err(RemoveSceneError::OutputReference {
            output: output_id(1),
            scene: scene_id(2)
        })
    );
    project
        .set_output_route(output_id(1), scene_id(1), bus_id(1))
        .unwrap();
    project.remove_scene(scene_id(2)).unwrap();
    assert_eq!(
        project
            .scenes()
            .iter()
            .map(|scene| scene.id)
            .collect::<Vec<_>>(),
        vec![scene_id(1), scene_id(3)]
    );
    assert_eq!(
        project.remove_scene(scene_id(99)),
        Err(RemoveSceneError::UnknownScene(scene_id(99)))
    );
}

#[test]
fn checked_output_creation_and_route_validate_references_without_mutation() {
    let mut project = valid_project();
    project.add_scene(Scene {
        id: scene_id(2),
        name: "Close".into(),
        background: Rgba8::OPAQUE_BLACK,
        layers: Vec::new(),
    });
    project
        .add_audio_bus_checked(AudioBus {
            id: bus_id(2),
            name: "Aux".into(),
            sends: Vec::new(),
        })
        .unwrap();
    let before_empty_bus = project.clone();
    let mut invalid_bus = project.audio_buses()[0].clone();
    invalid_bus.id = bus_id(3);
    invalid_bus.name = " \t ".into();
    assert_eq!(
        project.add_audio_bus_checked(invalid_bus),
        Err(fm_model::AddAudioBusError::EmptyName)
    );
    assert_eq!(project, before_empty_bus);
    let buses_before_duplicate = project.audio_buses().to_vec();
    assert!(matches!(
        project.add_audio_bus_checked(AudioBus {
            id: bus_id(3),
            name: "mAsTeR".into(),
            sends: Vec::new(),
        }),
        Err(fm_model::AddAudioBusError::DuplicateName)
    ));
    assert_eq!(project.audio_buses(), buses_before_duplicate);
    project
        .add_output_checked(Output {
            id: output_id(2),
            name: "Auxiliary".into(),
            video_source: scene_id(2),
            audio_source: bus_id(2),
            startup: StartupPolicy::Stopped,
            required_capabilities: Vec::new(),
        })
        .unwrap();
    let before_empty_output = project.clone();
    let mut invalid_output = project.outputs()[0].clone();
    invalid_output.id = output_id(3);
    invalid_output.name = " \n ".into();
    assert_eq!(
        project.add_output_checked(invalid_output),
        Err(fm_model::AddOutputError::EmptyName)
    );
    assert_eq!(project, before_empty_output);
    let outputs_before_duplicate = project.outputs().to_vec();
    assert!(matches!(
        project.add_output_checked(Output {
            id: output_id(3),
            name: "pRoGrAm".into(),
            video_source: scene_id(2),
            audio_source: bus_id(2),
            startup: StartupPolicy::Stopped,
            required_capabilities: Vec::new(),
        }),
        Err(fm_model::AddOutputError::DuplicateName)
    ));
    assert_eq!(project.outputs(), outputs_before_duplicate);
    let before = project.outputs().to_vec();
    project
        .set_output_route(output_id(1), scene_id(2), bus_id(2))
        .unwrap();
    assert_eq!(project.outputs()[0].id, before[0].id);
    assert_eq!(project.outputs()[0].name, before[0].name);
    assert_eq!(project.outputs()[0].startup, before[0].startup);
    assert_eq!(
        project.outputs()[0].required_capabilities,
        before[0].required_capabilities
    );
    assert_eq!(project.outputs()[0].video_source, scene_id(2));
    assert_eq!(project.outputs()[0].audio_source, bus_id(2));

    let unchanged = project.outputs().to_vec();
    assert!(matches!(
        project.set_output_route(output_id(99), scene_id(99), bus_id(99)),
        Err(fm_model::SetOutputRouteError::UnknownOutput(output)) if output == output_id(99)
    ));
    assert_eq!(project.outputs(), unchanged);
    assert!(matches!(
        project.set_output_route(output_id(1), scene_id(99), bus_id(2)),
        Err(fm_model::SetOutputRouteError::UnknownScene(scene)) if scene == scene_id(99)
    ));
    assert!(matches!(
        project.set_output_route(output_id(1), scene_id(2), bus_id(99)),
        Err(fm_model::SetOutputRouteError::UnknownBus(bus)) if bus == bus_id(99)
    ));
    assert_eq!(project.outputs(), unchanged);
}

#[test]
fn scene_composition_value_bounds_and_premultiplication_are_validated() {
    let mut project = Project::new(project_id(10), "Bounds", settings());
    project.add_input(Input {
        id: input_id(1),
        name: "Source".into(),
        kind: InputKind::Color,
        required_capabilities: Vec::new(),
    });
    let mut invalid_layer = layer("Source", SourceRef::Input(input_id(1)));
    invalid_layer.geometry.width = 0;
    invalid_layer.crop = Some(CropRect::new(1919, 0, 2, 1));
    project.add_scene(Scene {
        id: scene_id(1),
        name: "Invalid".into(),
        background: Rgba8::new(2, 0, 0, 1),
        layers: vec![invalid_layer],
    });

    let errors = project.validate().unwrap_err();
    for field in ["background", "layers.geometry", "layers.crop"] {
        assert!(errors.iter().any(|error| error.field == field));
    }
}

#[test]
fn persisted_scenes_can_exceed_the_renderer_execution_limit() {
    let mut project = Project::new(project_id(11), "Large stored scene", settings());
    project.add_input(Input {
        id: input_id(1),
        name: "Source".into(),
        kind: InputKind::Color,
        required_capabilities: Vec::new(),
    });
    let layers = (0..=Scene::MAX_RENDERED_LAYERS)
        .map(|index| layer(&format!("Layer {index}"), SourceRef::Input(input_id(1))))
        .collect();
    project.add_scene(Scene {
        id: scene_id(1),
        name: "Preserved".into(),
        background: Rgba8::OPAQUE_BLACK,
        layers,
    });

    assert_eq!(project.validate(), Ok(()));
}

#[test]
fn scene_input_video_and_audio_cycles_are_rejected() {
    let mut project = Project::new(project_id(20), "Cycles", settings());
    project.add_input(Input {
        id: input_id(20),
        name: "A".into(),
        kind: InputKind::Scene {
            scene_id: scene_id(20),
            audio_source: Some(input_id(21)),
        },
        required_capabilities: Vec::new(),
    });
    project.add_input(Input {
        id: input_id(21),
        name: "B".into(),
        kind: InputKind::Scene {
            scene_id: scene_id(21),
            audio_source: Some(input_id(20)),
        },
        required_capabilities: Vec::new(),
    });
    project.add_scene(Scene {
        id: scene_id(20),
        name: "A".into(),
        background: Rgba8::OPAQUE_BLACK,
        layers: vec![layer("B", SourceRef::Input(input_id(21)))],
    });
    project.add_scene(Scene {
        id: scene_id(21),
        name: "B".into(),
        background: Rgba8::OPAQUE_BLACK,
        layers: vec![layer("A", SourceRef::Input(input_id(20)))],
    });

    let errors = project.validate().unwrap_err();
    assert_eq!(
        errors
            .iter()
            .filter(|error| error.kind == ValidationErrorKind::Cycle)
            .count(),
        4
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
