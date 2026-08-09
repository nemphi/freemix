use std::{collections::BTreeMap, num::NonZeroU128};

use fm_model::{
    AudioBus, BusSend, CropRect, Input, InputAudioStrip, InputAudioStripState, InputGainMilliDb,
    InputKind, Layer, LayerGeometry, MainMix, Output, Project, ProjectSettings, RectMask,
    RestartPolicy, Rgba8, Rotation, Scene, SimulatedAudio, SimulatedInput, SimulatedVideo,
    SolidColor, SourceRef, StartupPolicy, StingerAudioPolicy, StingerConfig,
    StingerMissingMediaFallback, StingerSlotNumber,
};
use fm_types::{
    AudioFormat, BusId, Channel, ChannelLayout, ChromaLocation, ColorMetadata, ColorPrimaries,
    FrameRate, InputId, MatrixCoefficients, OutputId, PixelFormat, ProjectId, SampleFormat,
    SampleRate, ScanMode, SceneId, SignalRange, TransferFunction, VideoDimensions, VideoFormat,
};

use crate::{
    CURRENT_SCHEMA_VERSION, FadeToBlackState, IdempotencyReceipt, ManualTransitionKind,
    ManualTransitionState, ProjectPosition, ProjectValidationError, RuntimeFadeToBlack,
    RuntimeManualTransitions, RuntimeOverlayBorder, RuntimeOverlayChannel, RuntimeOverlayPosition,
    RuntimeOverlayTransition, RuntimeOverlays, RuntimeRouting, StoredProject,
};

use super::{
    DecodeError,
    reader::{Reader, Value},
};

pub(crate) fn decode(source: &str) -> Result<StoredProject, DecodeError> {
    let mut root = Object::new(Reader::new(source).document()?, "manifest")?;
    let schema = root.u32("schema_version")?;
    if schema != CURRENT_SCHEMA_VERSION {
        return Err(DecodeError::Validation(
            ProjectValidationError::UnsupportedSchema {
                found: schema,
                supported: CURRENT_SCHEMA_VERSION,
            },
        ));
    }
    let project = ProjectDto::parse(root.take("project")?)?.into_domain();
    let (routing, manual_transitions, fade_to_black, overlays, position, receipts) =
        parse_runtime(root.take("runtime")?)?;
    root.finish()?;
    StoredProject::from_project_with_complete_runtime_state(
        project,
        routing,
        manual_transitions,
        fade_to_black,
        overlays,
        position,
        receipts,
    )
    .map_err(DecodeError::Validation)
}

struct ProjectDto {
    id: ProjectId,
    name: String,
    settings: ProjectSettings,
    inputs: Vec<Input>,
    input_audio_strips: Vec<InputAudioStrip>,
    scenes: Vec<Scene>,
    audio_buses: Vec<AudioBus>,
    outputs: Vec<Output>,
    main_mix: Option<MainMix>,
    stingers: Vec<StingerConfig>,
    restart_policy: RestartPolicy,
}

impl ProjectDto {
    fn parse(value: Value) -> Result<Self, DecodeError> {
        let mut object = Object::new(value, "project")?;
        let id = ProjectId::new(object.nonzero_u128("id")?);
        let name = object.string("name")?;
        let settings = parse_settings(object.take("settings")?)?;
        let inputs = parse_array(object.take("inputs")?, "inputs", parse_input)?;
        let dto = Self {
            id,
            name,
            settings,
            inputs,
            input_audio_strips: parse_array(
                object.take("input_audio_strips")?,
                "input_audio_strips",
                parse_input_audio_strip,
            )?,
            scenes: parse_array(object.take("scenes")?, "scenes", parse_scene)?,
            audio_buses: parse_array(object.take("audio_buses")?, "audio_buses", parse_bus)?,
            outputs: parse_array(object.take("outputs")?, "outputs", parse_output)?,
            main_mix: parse_optional(object.take("main_mix")?, parse_main_mix)?,
            stingers: parse_array(object.take("stingers")?, "stingers", parse_stinger)?,
            restart_policy: parse_restart_policy(object.take("restart_policy")?)?,
        };
        object.finish()?;
        for input in &dto.inputs {
            if dto
                .input_audio_strips
                .iter()
                .filter(|strip| strip.input == input.id)
                .count()
                != 1
            {
                return Err(syntax(
                    "input_audio_strips must contain exactly one strip for every input",
                ));
            }
        }
        if dto
            .input_audio_strips
            .iter()
            .any(|strip| !dto.inputs.iter().any(|input| input.id == strip.input))
        {
            return Err(syntax(
                "input_audio_strips must not reference an unknown input",
            ));
        }
        Ok(dto)
    }

    fn into_domain(self) -> Project {
        let mut project = Project::new(self.id, self.name, self.settings)
            .with_restart_policy(self.restart_policy);
        for input in self.inputs {
            project.add_input(input);
        }
        for strip in self.input_audio_strips {
            assert!(
                project.set_input_audio_strip(strip.input, strip.state),
                "validated input audio strip must reference an input"
            );
        }
        for scene in self.scenes {
            project.add_scene(scene);
        }
        for bus in self.audio_buses {
            project.add_audio_bus(bus);
        }
        for output in self.outputs {
            project.add_output(output);
        }
        if let Some(main_mix) = self.main_mix {
            project.set_main_mix(main_mix);
        }
        for stinger in self.stingers {
            project.add_stinger(stinger);
        }
        project
    }
}

fn parse_stinger(value: Value) -> Result<StingerConfig, DecodeError> {
    let mut object = Object::new(value, "stinger")?;
    let slot_number = object.u8("slot")?;
    let slot = StingerSlotNumber::new(slot_number)
        .ok_or_else(|| syntax("stinger slot must be between 1 and 8"))?;
    let stinger = StingerConfig::new(
        slot,
        InputId::new(object.nonzero_u128("media_input")?),
        object.boolean("preload")?,
        object.u32("cut_point_frames")?,
        match object.string("audio_policy")?.as_str() {
            "muted" => StingerAudioPolicy::Muted,
            "stinger_only" => StingerAudioPolicy::StingerOnly,
            "mix_with_program" => StingerAudioPolicy::MixWithProgram,
            value => return Err(unknown_enum("stinger audio policy", value)),
        },
        match object.string("missing_media_fallback")?.as_str() {
            "cut" => StingerMissingMediaFallback::Cut,
            "fade" => StingerMissingMediaFallback::Fade,
            "keep_program" => StingerMissingMediaFallback::KeepProgram,
            value => return Err(unknown_enum("stinger missing-media fallback", value)),
        },
    );
    object.finish()?;
    Ok(stinger)
}

fn parse_input_audio_strip(value: Value) -> Result<InputAudioStrip, DecodeError> {
    let mut object = Object::new(value, "input audio strip")?;
    let input = InputId::new(object.nonzero_u128("input")?);
    let gain_value = object.i32("gain_milli_db")?;
    let gain = InputGainMilliDb::new(gain_value).ok_or_else(|| {
        syntax(format!(
            "input gain_milli_db must be between {} and {}",
            InputGainMilliDb::MIN,
            InputGainMilliDb::MAX
        ))
    })?;
    let strip = InputAudioStrip {
        input,
        state: InputAudioStripState {
            gain,
            muted: object.boolean("muted")?,
            follow_video: object.boolean("follow_video")?,
        },
    };
    object.finish()?;
    Ok(strip)
}

fn parse_settings(value: Value) -> Result<ProjectSettings, DecodeError> {
    let mut object = Object::new(value, "settings")?;
    let settings = ProjectSettings {
        frame_rate: parse_frame_rate(object.take("frame_rate")?)?,
        video: parse_video(object.take("video")?)?,
        audio: parse_audio(object.take("audio")?)?,
    };
    object.finish()?;
    Ok(settings)
}

fn parse_frame_rate(value: Value) -> Result<FrameRate, DecodeError> {
    let mut object = Object::new(value, "frame_rate")?;
    let numerator = object.u32("numerator")?;
    let denominator = object.u32("denominator")?;
    object.finish()?;
    FrameRate::new(numerator, denominator).map_err(|error| syntax(error.to_string()))
}

fn parse_video(value: Value) -> Result<VideoFormat, DecodeError> {
    let mut object = Object::new(value, "video")?;
    let width = object.u32("width")?;
    let height = object.u32("height")?;
    let video = VideoFormat {
        dimensions: VideoDimensions::new(width, height)
            .ok_or_else(|| syntax("video dimensions must be nonzero"))?,
        frame_rate: parse_frame_rate(object.take("frame_rate")?)?,
        pixel_format: match object.string("pixel_format")?.as_str() {
            "rgba8" => PixelFormat::Rgba8,
            "bgra8" => PixelFormat::Bgra8,
            "rgba16_float" => PixelFormat::Rgba16Float,
            "nv12" => PixelFormat::Nv12,
            "p010" => PixelFormat::P010,
            "yuv422" => PixelFormat::Yuv422,
            value => return Err(unknown_enum("pixel_format", value)),
        },
        scan: match object.string("scan")?.as_str() {
            "progressive" => ScanMode::Progressive,
            "interlaced_top_field_first" => ScanMode::InterlacedTopFieldFirst,
            "interlaced_bottom_field_first" => ScanMode::InterlacedBottomFieldFirst,
            value => return Err(unknown_enum("scan", value)),
        },
        color: parse_color(object.take("color")?)?,
    };
    object.finish()?;
    Ok(video)
}

fn parse_color(value: Value) -> Result<ColorMetadata, DecodeError> {
    let mut object = Object::new(value, "color")?;
    let primaries = match object.string("primaries")?.as_str() {
        "bt601" => ColorPrimaries::Bt601,
        "bt709" => ColorPrimaries::Bt709,
        "bt2020" => ColorPrimaries::Bt2020,
        "display_p3" => ColorPrimaries::DisplayP3,
        value => return Err(unknown_enum("primaries", value)),
    };
    let transfer = match object.string("transfer")?.as_str() {
        "linear" => TransferFunction::Linear,
        "srgb" => TransferFunction::Srgb,
        "bt709" => TransferFunction::Bt709,
        "bt1886" => TransferFunction::Bt1886,
        "hlg" => TransferFunction::Hlg,
        "pq" => TransferFunction::Pq,
        value => return Err(unknown_enum("transfer", value)),
    };
    let matrix = match object.string("matrix")?.as_str() {
        "identity" => MatrixCoefficients::Identity,
        "bt601" => MatrixCoefficients::Bt601,
        "bt709" => MatrixCoefficients::Bt709,
        "bt2020_non_constant" => MatrixCoefficients::Bt2020NonConstant,
        value => return Err(unknown_enum("matrix", value)),
    };
    let range = match object.string("range")?.as_str() {
        "full" => SignalRange::Full,
        "limited" => SignalRange::Limited,
        value => return Err(unknown_enum("range", value)),
    };
    let chroma_location = match object.string("chroma_location")?.as_str() {
        "left" => ChromaLocation::Left,
        "center" => ChromaLocation::Center,
        "top_left" => ChromaLocation::TopLeft,
        value => return Err(unknown_enum("chroma_location", value)),
    };
    object.finish()?;
    Ok(ColorMetadata {
        primaries,
        transfer,
        matrix,
        range,
        chroma_location,
    })
}

fn parse_audio(value: Value) -> Result<AudioFormat, DecodeError> {
    let mut object = Object::new(value, "audio")?;
    let sample_rate = SampleRate::new(object.u32("sample_rate_hz")?)
        .ok_or_else(|| syntax("audio sample rate must be nonzero"))?;
    let sample_format = match object.string("sample_format")?.as_str() {
        "i16" => SampleFormat::I16,
        "i24" => SampleFormat::I24,
        "i32" => SampleFormat::I32,
        "f32" => SampleFormat::F32,
        "f64" => SampleFormat::F64,
        value => return Err(unknown_enum("sample_format", value)),
    };
    let channels = parse_array(object.take("channels")?, "channels", |value| {
        let Value::String(value) = value else {
            return Err(syntax("channel must be a string"));
        };
        match value.as_str() {
            "mono" => Ok(Channel::Mono),
            "left" => Ok(Channel::Left),
            "right" => Ok(Channel::Right),
            "center" => Ok(Channel::Center),
            "low_frequency" => Ok(Channel::LowFrequency),
            "left_surround" => Ok(Channel::LeftSurround),
            "right_surround" => Ok(Channel::RightSurround),
            value => Err(unknown_enum("channel", value)),
        }
    })?;
    object.finish()?;
    Ok(AudioFormat {
        sample_rate,
        sample_format,
        channels: ChannelLayout::new(channels)
            .ok_or_else(|| syntax("audio channel layout must not be empty"))?,
    })
}

fn parse_input(value: Value) -> Result<Input, DecodeError> {
    let mut object = Object::new(value, "input")?;
    let input = Input {
        id: InputId::new(object.nonzero_u128("id")?),
        name: object.string("name")?,
        kind: parse_input_kind(object.take("kind")?)?,
        required_capabilities: parse_strings(
            object.take("required_capabilities")?,
            "required_capabilities",
        )?,
    };
    object.finish()?;
    Ok(input)
}

fn parse_input_kind(value: Value) -> Result<InputKind, DecodeError> {
    let mut object = Object::new(value, "input kind")?;
    let kind = match object.string("type")?.as_str() {
        "color" => InputKind::Color,
        "media" => InputKind::Media {
            asset_uri: object.string("asset_uri")?,
        },
        "device" => InputKind::Device {
            stable_key: object.string("stable_key")?,
        },
        "network" => InputKind::Network {
            endpoint: object.string("endpoint")?,
        },
        "scene" => InputKind::Scene {
            scene_id: SceneId::new(object.nonzero_u128("scene_id")?),
            audio_source: object.optional_input_id("audio_source")?,
        },
        "simulated" => InputKind::Simulated(SimulatedInput::new(
            parse_simulated_video(object.take("video")?)?,
            parse_simulated_audio(object.take("audio")?)?,
        )),
        value => return Err(unknown_enum("input kind", value)),
    };
    object.finish()?;
    Ok(kind)
}

fn parse_simulated_video(value: Value) -> Result<SimulatedVideo, DecodeError> {
    let mut object = Object::new(value, "simulated video")?;
    let video = match object.string("type")?.as_str() {
        "bars" => SimulatedVideo::Bars,
        "solid" => SimulatedVideo::Solid(SolidColor::new(
            object.u8("red")?,
            object.u8("green")?,
            object.u8("blue")?,
            object.u8("alpha")?,
        )),
        value => return Err(unknown_enum("simulated video", value)),
    };
    object.finish()?;
    Ok(video)
}

fn parse_simulated_audio(value: Value) -> Result<SimulatedAudio, DecodeError> {
    let mut object = Object::new(value, "simulated audio")?;
    let audio = match object.string("type")?.as_str() {
        "silence" => SimulatedAudio::Silence,
        "sine" => SimulatedAudio::Sine {
            frequency_hz: object.u32("frequency_hz")?,
        },
        value => return Err(unknown_enum("simulated audio", value)),
    };
    object.finish()?;
    Ok(audio)
}

fn parse_scene(value: Value) -> Result<Scene, DecodeError> {
    let mut object = Object::new(value, "scene")?;
    let id = SceneId::new(object.nonzero_u128("id")?);
    let name = object.string("name")?;
    let background = parse_rgba(object.take("background")?)?;
    let scene = Scene {
        id,
        name,
        background,
        layers: parse_array(object.take("layers")?, "layers", parse_layer)?,
    };
    object.finish()?;
    Ok(scene)
}

fn parse_layer(value: Value) -> Result<Layer, DecodeError> {
    let mut object = Object::new(value, "layer")?;
    let name = object.string("name")?;
    let source = parse_source(object.take("source")?)?;
    let enabled = object.boolean("enabled")?;
    let geometry = parse_geometry(object.take("geometry")?)?;
    let crop = parse_optional(object.take("crop")?, parse_crop)?;
    let mask = parse_optional(object.take("mask")?, parse_mask)?;
    let opacity = object.u8("opacity")?;
    let z_order = object.i32("z_order")?;
    let layer = Layer {
        name,
        source,
        enabled,
        geometry,
        crop,
        mask,
        opacity,
        z_order,
    };
    object.finish()?;
    Ok(layer)
}

fn parse_rgba(value: Value) -> Result<Rgba8, DecodeError> {
    let mut object = Object::new(value, "RGBA8 color")?;
    let color = Rgba8::new(
        object.u8("red")?,
        object.u8("green")?,
        object.u8("blue")?,
        object.u8("alpha")?,
    );
    object.finish()?;
    Ok(color)
}

fn parse_geometry(value: Value) -> Result<LayerGeometry, DecodeError> {
    let mut object = Object::new(value, "layer geometry")?;
    let geometry = LayerGeometry::new(
        object.i32("translation_x")?,
        object.i32("translation_y")?,
        object.u32("width")?,
        object.u32("height")?,
        match object.string("rotation")?.as_str() {
            "deg0" => Rotation::Deg0,
            "deg90" => Rotation::Deg90,
            "deg180" => Rotation::Deg180,
            "deg270" => Rotation::Deg270,
            value => return Err(unknown_enum("rotation", value)),
        },
    );
    object.finish()?;
    Ok(geometry)
}

fn parse_crop(value: Value) -> Result<CropRect, DecodeError> {
    let mut object = Object::new(value, "crop")?;
    let crop = CropRect::new(
        object.u32("x")?,
        object.u32("y")?,
        object.u32("width")?,
        object.u32("height")?,
    );
    object.finish()?;
    Ok(crop)
}

fn parse_mask(value: Value) -> Result<RectMask, DecodeError> {
    let mut object = Object::new(value, "rectangular mask")?;
    let mask = RectMask::new(
        object.u32("x")?,
        object.u32("y")?,
        object.u32("width")?,
        object.u32("height")?,
    )
    .inverted(object.boolean("invert")?);
    object.finish()?;
    Ok(mask)
}

fn parse_source(value: Value) -> Result<SourceRef, DecodeError> {
    let mut object = Object::new(value, "source")?;
    let source = match object.string("type")?.as_str() {
        "input" => SourceRef::Input(InputId::new(object.nonzero_u128("id")?)),
        "scene" => SourceRef::Scene(SceneId::new(object.nonzero_u128("id")?)),
        value => return Err(unknown_enum("source", value)),
    };
    object.finish()?;
    Ok(source)
}

fn parse_bus(value: Value) -> Result<AudioBus, DecodeError> {
    let mut object = Object::new(value, "audio bus")?;
    let bus = AudioBus {
        id: BusId::new(object.nonzero_u128("id")?),
        name: object.string("name")?,
        sends: parse_array(object.take("sends")?, "sends", |value| {
            let mut send = Object::new(value, "bus send")?;
            let destination = BusId::new(send.nonzero_u128("destination")?);
            send.finish()?;
            Ok(BusSend { destination })
        })?,
    };
    object.finish()?;
    Ok(bus)
}

fn parse_output(value: Value) -> Result<Output, DecodeError> {
    let mut object = Object::new(value, "output")?;
    let output = Output {
        id: OutputId::new(object.nonzero_u128("id")?),
        name: object.string("name")?,
        video_source: SceneId::new(object.nonzero_u128("video_source")?),
        audio_source: BusId::new(object.nonzero_u128("audio_source")?),
        startup: match object.string("startup")?.as_str() {
            "stopped" => StartupPolicy::Stopped,
            "reconcile_desired_state" => StartupPolicy::ReconcileDesiredState,
            value => return Err(unknown_enum("startup", value)),
        },
        required_capabilities: parse_strings(
            object.take("required_capabilities")?,
            "required_capabilities",
        )?,
    };
    object.finish()?;
    Ok(output)
}

fn parse_main_mix(value: Value) -> Result<MainMix, DecodeError> {
    let mut object = Object::new(value, "main mix")?;
    let mix = MainMix::new(
        InputId::new(object.nonzero_u128("desired_program")?),
        InputId::new(object.nonzero_u128("desired_preview")?),
    );
    object.finish()?;
    Ok(mix)
}

fn parse_restart_policy(value: Value) -> Result<RestartPolicy, DecodeError> {
    let mut object = Object::new(value, "restart policy")?;
    let policy = match object.string("type")?.as_str() {
        "never" => RestartPolicy::Never,
        "always" => RestartPolicy::Always,
        "on_failure" => RestartPolicy::OnFailure {
            max_attempts: object.u8("max_attempts")?,
        },
        value => return Err(unknown_enum("restart policy", value)),
    };
    object.finish()?;
    Ok(policy)
}

type ParsedRuntime = (
    RuntimeRouting,
    RuntimeManualTransitions,
    RuntimeFadeToBlack,
    RuntimeOverlays,
    ProjectPosition,
    Vec<IdempotencyReceipt>,
);

fn parse_runtime(value: Value) -> Result<ParsedRuntime, DecodeError> {
    let mut object = Object::new(value, "runtime")?;
    let routing = parse_routing(object.take("routing")?)?;
    let manual_transitions = parse_manual_transitions(object.take("manual_transitions")?)?;
    let fade_to_black = parse_fade_to_black(object.take("fade_to_black")?)?;
    let overlays = parse_overlays(object.take("overlays")?)?;
    let position = parse_position(object.take("position")?)?;
    let receipts = parse_array(
        object.take("idempotency_receipts")?,
        "idempotency_receipts",
        parse_receipt,
    )?;
    object.finish()?;
    Ok((
        routing,
        manual_transitions,
        fade_to_black,
        overlays,
        position,
        receipts,
    ))
}

fn parse_overlays(value: Value) -> Result<RuntimeOverlays, DecodeError> {
    let mut object = Object::new(value, "overlays")?;
    let desired = parse_overlay_channels(object.take("desired")?)?;
    let realized = parse_overlay_channels(object.take("realized")?)?;
    object.finish()?;
    Ok(RuntimeOverlays { desired, realized })
}

fn parse_overlay_channels(value: Value) -> Result<[RuntimeOverlayChannel; 8], DecodeError> {
    let channels = parse_array(value, "overlay channels", |value| {
        let mut object = Object::new(value, "overlay channel")?;
        let source = object.optional_input_id("source")?;
        let active = object.boolean("active")?;
        let transition = match object.string("transition")?.as_str() {
            "cut" => RuntimeOverlayTransition::Cut,
            "fade" => RuntimeOverlayTransition::Fade,
            value => return Err(syntax(format!("unknown overlay transition `{value}`"))),
        };
        let duration_frames = object.u32("duration_frames")?;
        let position = match object.string("position")?.as_str() {
            "full_frame" => RuntimeOverlayPosition::FullFrame,
            "top_left" => RuntimeOverlayPosition::TopLeft,
            "top_right" => RuntimeOverlayPosition::TopRight,
            "bottom_left" => RuntimeOverlayPosition::BottomLeft,
            "bottom_right" => RuntimeOverlayPosition::BottomRight,
            value => return Err(syntax(format!("unknown overlay position `{value}`"))),
        };
        let border = match object.string("border")?.as_str() {
            "none" => RuntimeOverlayBorder::None,
            "thin_white" => RuntimeOverlayBorder::ThinWhite,
            "thick_white" => RuntimeOverlayBorder::ThickWhite,
            value => return Err(syntax(format!("unknown overlay border `{value}`"))),
        };
        let queued_sources = parse_array(
            object.take("queued_sources")?,
            "overlay queued sources",
            |value| Ok(InputId::new(nonzero_u128(&value, "input ID")?)),
        )?;
        let included_outputs = parse_array(
            object.take("included_outputs")?,
            "overlay included outputs",
            |value| Ok(OutputId::new(nonzero_u128(&value, "output ID")?)),
        )?;
        object.finish()?;
        Ok(RuntimeOverlayChannel {
            source,
            active,
            transition,
            duration_frames,
            position,
            border,
            queued_sources,
            included_outputs,
        })
    })?;
    channels.try_into().map_err(|channels: Vec<_>| {
        syntax(format!(
            "expected 8 overlay channels, found {}",
            channels.len()
        ))
    })
}

fn parse_fade_to_black(value: Value) -> Result<RuntimeFadeToBlack, DecodeError> {
    let mut object = Object::new(value, "fade to black")?;
    let state = RuntimeFadeToBlack {
        desired: parse_fade_to_black_state(object.take("desired")?)?,
        realized: parse_fade_to_black_state(object.take("realized")?)?,
    };
    object.finish()?;
    Ok(state)
}

fn parse_fade_to_black_state(value: Value) -> Result<FadeToBlackState, DecodeError> {
    let mut object = Object::new(value, "fade to black state")?;
    let state = FadeToBlackState::new(
        object.boolean("target_active")?,
        object.u16("position_numerator")?,
    );
    object.finish()?;
    Ok(state)
}

fn parse_manual_transitions(value: Value) -> Result<RuntimeManualTransitions, DecodeError> {
    let mut object = Object::new(value, "manual transitions")?;
    let transitions = RuntimeManualTransitions {
        desired: parse_optional(object.take("desired")?, parse_manual_transition)?,
        realized: parse_optional(object.take("realized")?, parse_manual_transition)?,
    };
    object.finish()?;
    Ok(transitions)
}

fn parse_manual_transition(value: Value) -> Result<ManualTransitionState, DecodeError> {
    let mut object = Object::new(value, "manual transition")?;
    let kind = match object.string("kind")?.as_str() {
        "fade" => ManualTransitionKind::Fade,
        "wipe" => ManualTransitionKind::Wipe,
        "alpha_fade" => ManualTransitionKind::AlphaFade,
        value => return Err(unknown_enum("manual transition kind", value)),
    };
    let from_id = InputId::new(object.nonzero_u128("from_id")?);
    let to_id = InputId::new(object.nonzero_u128("to_id")?);
    let interval_start_basis_points = object.u16("interval_start_basis_points")?;
    let position_basis_points = object.u16("position_basis_points")?;
    object.finish()?;
    ManualTransitionState::new(
        kind,
        from_id,
        to_id,
        interval_start_basis_points,
        position_basis_points,
    )
    .ok_or_else(|| syntax("manual transition position must not exceed 10000 basis points"))
}

fn parse_routing(value: Value) -> Result<RuntimeRouting, DecodeError> {
    let mut object = Object::new(value, "routing")?;
    let routing = RuntimeRouting {
        desired_program_id: object.optional_input_id("desired_program_id")?,
        realized_program_id: object.optional_input_id("realized_program_id")?,
        desired_preview_id: object.optional_input_id("desired_preview_id")?,
        realized_preview_id: object.optional_input_id("realized_preview_id")?,
    };
    object.finish()?;
    Ok(routing)
}

fn parse_position(value: Value) -> Result<ProjectPosition, DecodeError> {
    let mut object = Object::new(value, "position")?;
    let position = ProjectPosition {
        revision: object.u64("revision")?,
        state_epoch: object.u64("state_epoch")?,
        event_sequence: object.u64("event_sequence")?,
        frames_rendered: object.u64("frames_rendered")?,
        runtime_generation: object.u64("runtime_generation")?,
        clock_time_nanos: object.u64("clock_time_nanos")?,
    };
    object.finish()?;
    Ok(position)
}

fn parse_receipt(value: Value) -> Result<IdempotencyReceipt, DecodeError> {
    let mut object = Object::new(value, "receipt")?;
    let key = object.string("key")?;
    let command_id = object.string("command_id")?;
    let receipt = match object.string("outcome")?.as_str() {
        "accepted" => IdempotencyReceipt::accepted(
            key,
            command_id,
            object.u64("revision")?,
            object.u64("target_frame")?,
        ),
        "rejected" => IdempotencyReceipt::rejected(
            key,
            command_id,
            object.u64("current_revision")?,
            object.string("code")?,
            object.string("message")?,
            object.boolean("retryable")?,
        ),
        value => return Err(unknown_enum("receipt outcome", value)),
    };
    object.finish()?;
    Ok(receipt)
}

struct Object {
    context: &'static str,
    values: BTreeMap<String, Value>,
}

impl Object {
    fn new(value: Value, context: &'static str) -> Result<Self, DecodeError> {
        let Value::Object(values) = value else {
            return Err(syntax(format!("{context} must be an object")));
        };
        Ok(Self { context, values })
    }

    fn take(&mut self, field: &str) -> Result<Value, DecodeError> {
        self.values
            .remove(field)
            .ok_or_else(|| syntax(format!("missing field `{field}` in {}", self.context)))
    }

    fn string(&mut self, field: &str) -> Result<String, DecodeError> {
        let Value::String(value) = self.take(field)? else {
            return Err(syntax(format!("field `{field}` must be a string")));
        };
        Ok(value)
    }

    fn boolean(&mut self, field: &str) -> Result<bool, DecodeError> {
        let Value::Bool(value) = self.take(field)? else {
            return Err(syntax(format!("field `{field}` must be a boolean")));
        };
        Ok(value)
    }

    fn number(&mut self, field: &str) -> Result<u128, DecodeError> {
        let Value::Number(value) = self.take(field)? else {
            return Err(syntax(format!(
                "field `{field}` must be an unsigned integer"
            )));
        };
        Ok(value)
    }

    fn u8(&mut self, field: &str) -> Result<u8, DecodeError> {
        u8::try_from(self.number(field)?).map_err(|_| syntax(format!("field `{field}` exceeds u8")))
    }

    fn u32(&mut self, field: &str) -> Result<u32, DecodeError> {
        u32::try_from(self.number(field)?)
            .map_err(|_| syntax(format!("field `{field}` exceeds u32")))
    }

    fn u16(&mut self, field: &str) -> Result<u16, DecodeError> {
        u16::try_from(self.number(field)?)
            .map_err(|_| syntax(format!("field `{field}` exceeds u16")))
    }

    fn i32(&mut self, field: &str) -> Result<i32, DecodeError> {
        match self.take(field)? {
            Value::Number(value) => {
                i32::try_from(value).map_err(|_| syntax(format!("field `{field}` exceeds i32")))
            }
            Value::NegativeNumber(value) => {
                let magnitude = i128::try_from(value)
                    .map_err(|_| syntax(format!("field `{field}` exceeds i32")))?;
                i32::try_from(-magnitude)
                    .map_err(|_| syntax(format!("field `{field}` exceeds i32")))
            }
            _ => Err(syntax(format!("field `{field}` must be an integer"))),
        }
    }

    fn u64(&mut self, field: &str) -> Result<u64, DecodeError> {
        u64::try_from(self.number(field)?)
            .map_err(|_| syntax(format!("field `{field}` exceeds u64")))
    }

    fn nonzero_u128(&mut self, field: &str) -> Result<NonZeroU128, DecodeError> {
        NonZeroU128::new(self.number(field)?)
            .ok_or_else(|| syntax(format!("field `{field}` must be nonzero")))
    }

    fn optional_input_id(&mut self, field: &str) -> Result<Option<InputId>, DecodeError> {
        match self.take(field)? {
            Value::Null => Ok(None),
            Value::Number(value) => NonZeroU128::new(value)
                .map(InputId::new)
                .map(Some)
                .ok_or_else(|| syntax(format!("field `{field}` must be nonzero or null"))),
            _ => Err(syntax(format!(
                "field `{field}` must be an unsigned integer or null"
            ))),
        }
    }

    fn finish(self) -> Result<(), DecodeError> {
        if let Some(field) = self.values.keys().next() {
            Err(syntax(format!(
                "unknown field `{field}` in {}",
                self.context
            )))
        } else {
            Ok(())
        }
    }
}

fn parse_array<T>(
    value: Value,
    context: &'static str,
    parse: impl FnMut(Value) -> Result<T, DecodeError>,
) -> Result<Vec<T>, DecodeError> {
    let Value::Array(values) = value else {
        return Err(syntax(format!("{context} must be an array")));
    };
    values.into_iter().map(parse).collect()
}

fn parse_strings(value: Value, context: &'static str) -> Result<Vec<String>, DecodeError> {
    parse_array(value, context, |value| {
        let Value::String(value) = value else {
            return Err(syntax(format!("{context} entries must be strings")));
        };
        Ok(value)
    })
}

fn parse_optional<T>(
    value: Value,
    parse: impl FnOnce(Value) -> Result<T, DecodeError>,
) -> Result<Option<T>, DecodeError> {
    if matches!(value, Value::Null) {
        Ok(None)
    } else {
        parse(value).map(Some)
    }
}

fn nonzero_u128(value: &Value, context: &str) -> Result<NonZeroU128, DecodeError> {
    let Value::Number(value) = value else {
        return Err(syntax(format!("{context} must be an unsigned integer")));
    };
    NonZeroU128::new(*value).ok_or_else(|| syntax(format!("{context} must be nonzero")))
}

fn unknown_enum(field: &str, value: &str) -> DecodeError {
    syntax(format!("unknown {field} value `{value}`"))
}

fn syntax(message: impl Into<String>) -> DecodeError {
    DecodeError::Syntax {
        offset: 0,
        message: message.into(),
    }
}
