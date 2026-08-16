use std::{
    error::Error,
    fs::{self, File, OpenOptions},
    io,
    num::NonZeroU128,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use fm_clock::{ClockDomainId, ClockTime};
use fm_command::{
    AcceptedReceipt, CommandEnvelope, CommandId, CommandReceipt, EventSequence, IdempotencyKey,
    RejectedReceipt, Rejection, RejectionCode, Revision, RuntimeGeneration, StateEpoch,
};
use fm_engine::{
    Engine, EngineAcceptance, EngineCommand, EngineFadeToBlackState, EngineInputAudioStripState,
    EngineManualTransitionKind, EngineManualTransitionPosition, EngineRestoreState, ShowState,
};
use fm_model::{
    Input, InputAudioStripState, InputBalanceBasisPoints, InputDelaySamples, InputGainMilliDb,
    InputKind, Layer, LayerGeometry, MainMix, Project, ProjectSettings, Rgba8 as ModelRgba8,
    Rotation, Scene, SimulatedAudio, SimulatedInput, SimulatedVideo, SolidColor, SourceRef,
    StingerAudioPolicy as ModelStingerAudioPolicy, StingerConfig, StingerMissingMediaFallback,
    StingerSlotNumber,
};
use fm_persistence::{
    FadeToBlackState as PersistedFadeToBlackState, IdempotencyReceipt,
    ManualTransitionKind as PersistedManualTransitionKind,
    ManualTransitionState as PersistedManualTransitionState, ProjectPosition, ProjectStore,
    ReceiptOutcome, RuntimeFadeToBlack, RuntimeManualTransitions, RuntimeOverlayBorder,
    RuntimeOverlayChannel, RuntimeOverlayPosition, RuntimeOverlayTransition, RuntimeOverlays,
    RuntimeRouting, StoredProject,
};
use fm_sim::{Rgba8, SimulatedPipeline, SimulatedSource, SourcePattern};
use fm_switcher::{
    MissingMediaFallback, OverlayBorderPreset, OverlayChannelId, OverlayChannelState,
    OverlayPositionPreset, OverlayTransitionKind, StingerAudioPolicy, StingerDescriptor,
    StingerSlotId, SwitcherState, TBarPosition, TBarState, TransitionKind,
};
use fm_types::{
    AudioFormat, ChannelLayout, ColorMetadata, FrameRate, InputId, OutputId, PixelFormat,
    ProjectId, SampleFormat, SampleRate, ScanMode, SceneId, VideoDimensions, VideoFormat,
};
use fm_video::write_ppm;

use crate::{
    args::{
        Command, ManualTransitionKind, OverlayBorder as CliOverlayBorder,
        OverlayPosition as CliOverlayPosition, OverlayTransition as CliOverlayTransition,
        StingerAudioPolicy as CliStingerAudioPolicy, StingerFallback as CliStingerFallback,
        TBarAction,
    },
    remote,
};

type AppResult<T> = Result<T, Box<dyn Error>>;
static IMPLICIT_KEY_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static PROJECT_ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static PPM_OUTPUT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct ProjectEngine {
    project: Project,
    engine: Engine,
}

#[allow(clippy::too_many_lines)]
pub fn run(command: Command) -> AppResult<()> {
    match command {
        Command::New { path, name } => {
            if path.try_exists()? {
                return Err(AppFailure(format!(
                    "destination bundle already exists: {}",
                    path.display()
                ))
                .into());
            }
            let project = default_project(name)?;
            save_engine(&path, &project)?;
            print_status(&project);
        }
        Command::InputAdd { path, input, name } => {
            add_input(&path, input_id(input)?, name)?;
        }
        Command::SceneInputAdd {
            path,
            input,
            scene,
            name,
        } => {
            add_scene_input(&path, input_id(input)?, scene_id(scene)?, name)?;
        }
        Command::SceneLayerAdd {
            path,
            scene,
            source,
            z_order,
            name,
        } => add_scene_layer(&path, scene_id(scene)?, input_id(source)?, z_order, name)?,
        Command::SceneLayerRemove { path, scene, index } => {
            remove_scene_layer(&path, scene_id(scene)?, index)?
        }
        Command::InputRemove { path, input } => remove_input(&path, input_id(input)?)?,
        Command::InputDuplicate {
            path,
            source,
            input,
            name,
        } => duplicate_input(&path, input_id(source)?, input_id(input)?, name)?,
        Command::InputReplaceSimulated { path, input } => {
            replace_input_simulated(&path, input_id(input)?)?
        }
        Command::Status { path } => print_status(&load_engine(&path)?),
        Command::AudioStrip {
            path,
            input,
            gain_millidb,
            balance_basis_points,
            muted,
            soloed,
            follow_video,
            delay_samples,
        } => mutate(
            &path,
            EngineCommand::SetInputAudioStrip {
                input: input_id(input)?,
                state: engine_audio_strip_state(
                    gain_millidb,
                    balance_basis_points,
                    muted,
                    soloed,
                    follow_video,
                    delay_samples,
                )?,
            },
            1,
            None,
            None,
        )?,
        Command::Rename {
            path,
            input,
            name,
            key,
            expected_revision,
        } => mutate(
            &path,
            EngineCommand::RenameInput {
                input: input_id(input)?,
                name,
            },
            1,
            key,
            expected_revision,
        )?,
        Command::Reorder {
            path,
            inputs,
            key,
            expected_revision,
        } => mutate(
            &path,
            EngineCommand::ReorderInputs {
                inputs: inputs
                    .into_iter()
                    .map(input_id)
                    .collect::<AppResult<Vec<_>>>()?,
            },
            1,
            key,
            expected_revision,
        )?,
        Command::Preview {
            path,
            input,
            key,
            expected_revision,
        } => {
            let input = input_id(input)?;
            mutate(
                &path,
                EngineCommand::SelectPreview(input),
                1,
                key,
                expected_revision,
            )?;
        }
        Command::Cut {
            path,
            key,
            expected_revision,
        } => mutate(&path, EngineCommand::Cut, 1, key, expected_revision)?,
        Command::Fade {
            path,
            frames,
            key,
            expected_revision,
        } => mutate(
            &path,
            EngineCommand::Fade {
                duration_frames: frames,
            },
            frames,
            key,
            expected_revision,
        )?,
        Command::AlphaFade {
            path,
            frames,
            key,
            expected_revision,
        } => mutate(
            &path,
            EngineCommand::AlphaFade {
                duration_frames: frames,
            },
            frames,
            key,
            expected_revision,
        )?,
        Command::Slide {
            path,
            frames,
            key,
            expected_revision,
        } => mutate(
            &path,
            EngineCommand::Slide {
                duration_frames: frames,
            },
            frames,
            key,
            expected_revision,
        )?,
        Command::Zoom {
            path,
            frames,
            key,
            expected_revision,
        } => mutate(
            &path,
            EngineCommand::Zoom {
                duration_frames: frames,
            },
            frames,
            key,
            expected_revision,
        )?,
        Command::Stinger {
            path,
            slot,
            frames,
            key,
            expected_revision,
        } => mutate(
            &path,
            EngineCommand::Stinger {
                slot: StingerSlotId::new(slot).expect("CLI parser validates Stinger slots"),
                duration_frames: frames,
            },
            frames,
            key,
            expected_revision,
        )?,
        Command::StingerConfigure {
            path,
            slot,
            media_input,
            preload,
            cut_point_frames,
            audio_policy,
            fallback,
        } => {
            configure_stinger(
                &path,
                StingerConfig::new(
                    StingerSlotNumber::new(slot).expect("CLI parser validates Stinger slots"),
                    input_id(media_input)?,
                    preload,
                    cut_point_frames,
                    model_stinger_audio_policy(audio_policy),
                    model_stinger_fallback(fallback),
                ),
            )?;
        }
        Command::StingerRemove { path, slot } => {
            remove_stinger(
                &path,
                StingerSlotNumber::new(slot).expect("CLI parser validates Stinger slots"),
            )?;
        }
        Command::OverlayTake {
            path,
            channel,
            source,
            key,
            expected_revision,
        } => mutate(
            &path,
            EngineCommand::TakeOverlay {
                channel: OverlayChannelId::new(channel).expect("CLI parser validates overlays"),
                source: input_id(source)?,
            },
            1,
            key,
            expected_revision,
        )?,
        Command::OverlayUpdate {
            path,
            channel,
            source,
            key,
            expected_revision,
        } => mutate(
            &path,
            EngineCommand::UpdateOverlay {
                channel: OverlayChannelId::new(channel).expect("CLI parser validates overlays"),
                source: input_id(source)?,
            },
            1,
            key,
            expected_revision,
        )?,
        Command::OverlayOff {
            path,
            channel,
            key,
            expected_revision,
        } => mutate(
            &path,
            EngineCommand::OverlayOff {
                channel: OverlayChannelId::new(channel).expect("CLI parser validates overlays"),
            },
            1,
            key,
            expected_revision,
        )?,
        Command::OverlayOutput {
            path,
            channel,
            output,
            included,
            key,
            expected_revision,
        } => mutate(
            &path,
            EngineCommand::SetOverlayOutputInclusion {
                channel: OverlayChannelId::new(channel).expect("CLI parser validates overlays"),
                output: output_id(output)?,
                included,
            },
            1,
            key,
            expected_revision,
        )?,
        Command::OverlayTransition {
            path,
            channel,
            transition,
            frames,
            key,
            expected_revision,
        } => mutate(
            &path,
            EngineCommand::ConfigureOverlayTransition {
                channel: OverlayChannelId::new(channel).expect("CLI parser validates overlays"),
                transition: switcher_overlay_transition(transition),
                duration_frames: frames,
            },
            1,
            key,
            expected_revision,
        )?,
        Command::OverlayAppearance {
            path,
            channel,
            position,
            border,
            key,
            expected_revision,
        } => mutate(
            &path,
            EngineCommand::ConfigureOverlayAppearance {
                channel: OverlayChannelId::new(channel).expect("CLI parser validates overlays"),
                position: switcher_overlay_position(position),
                border: switcher_overlay_border(border),
            },
            1,
            key,
            expected_revision,
        )?,
        Command::OverlayQueue {
            path,
            channel,
            source,
            key,
            expected_revision,
        } => mutate(
            &path,
            EngineCommand::QueueOverlay {
                channel: OverlayChannelId::new(channel).expect("CLI parser validates overlays"),
                source: input_id(source)?,
            },
            1,
            key,
            expected_revision,
        )?,
        Command::OverlayNext {
            path,
            channel,
            key,
            expected_revision,
        } => mutate(
            &path,
            EngineCommand::TakeNextOverlay {
                channel: OverlayChannelId::new(channel).expect("CLI parser validates overlays"),
            },
            1,
            key,
            expected_revision,
        )?,
        Command::Wipe {
            path,
            frames,
            key,
            expected_revision,
        } => mutate(
            &path,
            EngineCommand::Wipe {
                duration_frames: frames,
            },
            frames,
            key,
            expected_revision,
        )?,
        Command::TBar {
            path,
            action,
            key,
            expected_revision,
        } => mutate(
            &path,
            engine_t_bar_command(action),
            1,
            key,
            expected_revision,
        )?,
        Command::FadeToBlack {
            path,
            target,
            frames,
            key,
            expected_revision,
        } => mutate(
            &path,
            EngineCommand::FadeToBlack {
                active: target.active(),
                duration_frames: frames,
            },
            frames,
            key,
            expected_revision,
        )?,
        Command::RemoteStatus { address } => remote::status(address)?,
        Command::RemoteDiagnostics { address } => remote::diagnostics(address)?,
        Command::RemoteAudioStrip {
            address,
            input,
            gain_millidb,
            balance_basis_points,
            muted,
            soloed,
            follow_video,
            delay_samples,
            key,
            expected_revision,
        } => remote::execute(
            address,
            fm_protocol::CommandPayload::SetInputAudioStrip {
                input: fm_protocol::WireInputId::new(
                    NonZeroU128::new(input)
                        .ok_or_else(|| AppFailure("input ID must be nonzero".into()))?,
                ),
                gain_millidb: InputGainMilliDb::new(gain_millidb)
                    .ok_or_else(|| AppFailure("gain must be in -96000..=24000 millidB".into()))?
                    .get(),
                balance_basis_points: InputBalanceBasisPoints::new(balance_basis_points)
                    .ok_or_else(|| {
                        AppFailure("balance must be in -10000..=10000 basis points".into())
                    })?
                    .get(),
                muted,
                soloed,
                follow_video,
                delay_samples: InputDelaySamples::new(delay_samples)
                    .ok_or_else(|| AppFailure("delay samples must be in 0..=48000".into()))?
                    .get(),
            },
            key,
            expected_revision,
        )?,
        Command::RemoteRename {
            address,
            input,
            name,
            key,
            expected_revision,
        } => remote::execute(
            address,
            fm_protocol::CommandPayload::RenameInput {
                input: fm_protocol::WireInputId::new(
                    NonZeroU128::new(input)
                        .ok_or_else(|| AppFailure("input ID must be nonzero".into()))?,
                ),
                name,
            },
            key,
            expected_revision,
        )?,
        Command::RemoteReorder {
            address,
            inputs,
            key,
            expected_revision,
        } => remote::execute(
            address,
            fm_protocol::CommandPayload::ReorderInputs {
                inputs: inputs
                    .into_iter()
                    .map(|input| {
                        NonZeroU128::new(input)
                            .ok_or_else(|| AppFailure("input ID must be nonzero".into()))
                            .map(fm_protocol::WireInputId::new)
                            .map_err(|error| -> Box<dyn Error> { Box::new(error) })
                    })
                    .collect::<AppResult<Vec<_>>>()?,
            },
            key,
            expected_revision,
        )?,
        Command::RemotePreview {
            address,
            input,
            key,
            expected_revision,
        } => remote::execute(
            address,
            fm_protocol::CommandPayload::SelectPreview {
                input: fm_protocol::WireInputId::new(
                    NonZeroU128::new(input)
                        .ok_or_else(|| AppFailure("input ID must be nonzero".into()))?,
                ),
            },
            key,
            expected_revision,
        )?,
        Command::RemoteCut {
            address,
            key,
            expected_revision,
        } => remote::execute(
            address,
            fm_protocol::CommandPayload::Cut,
            key,
            expected_revision,
        )?,
        Command::RemoteFade {
            address,
            frames,
            key,
            expected_revision,
        } => remote::execute(
            address,
            fm_protocol::CommandPayload::Fade {
                duration_frames: frames,
            },
            key,
            expected_revision,
        )?,
        Command::RemoteAlphaFade {
            address,
            frames,
            key,
            expected_revision,
        } => remote::execute(
            address,
            fm_protocol::CommandPayload::AlphaFade {
                duration_frames: frames,
            },
            key,
            expected_revision,
        )?,
        Command::RemoteSlide {
            address,
            frames,
            key,
            expected_revision,
        } => remote::execute(
            address,
            fm_protocol::CommandPayload::Slide {
                duration_frames: frames,
            },
            key,
            expected_revision,
        )?,
        Command::RemoteZoom {
            address,
            frames,
            key,
            expected_revision,
        } => remote::execute(
            address,
            fm_protocol::CommandPayload::Zoom {
                duration_frames: frames,
            },
            key,
            expected_revision,
        )?,
        Command::RemoteStinger {
            address,
            slot,
            frames,
            key,
            expected_revision,
        } => remote::execute(
            address,
            fm_protocol::CommandPayload::Stinger {
                slot: fm_protocol::WireStingerSlotId::new(slot)
                    .expect("CLI parser validates Stinger slots"),
                duration_frames: frames,
            },
            key,
            expected_revision,
        )?,
        Command::RemoteStingerConfigure {
            address,
            slot,
            media_input,
            preload,
            cut_point_frames,
            audio_policy,
            fallback,
            key,
            expected_revision,
        } => remote::execute(
            address,
            fm_protocol::CommandPayload::ConfigureStinger {
                slot: fm_protocol::WireStingerSlotId::new(slot)
                    .expect("CLI parser validates Stinger slots"),
                media_input: fm_protocol::WireInputId::new(
                    NonZeroU128::new(media_input)
                        .ok_or_else(|| AppFailure("media input ID must be nonzero".into()))?,
                ),
                preload,
                cut_point_frames,
                audio_policy: protocol_stinger_audio_policy(audio_policy),
                missing_media_fallback: protocol_stinger_fallback(fallback),
            },
            key,
            expected_revision,
        )?,
        Command::RemoteStingerRemove {
            address,
            slot,
            key,
            expected_revision,
        } => remote::execute(
            address,
            fm_protocol::CommandPayload::RemoveStinger {
                slot: fm_protocol::WireStingerSlotId::new(slot)
                    .expect("CLI parser validates Stinger slots"),
            },
            key,
            expected_revision,
        )?,
        Command::RemoteOverlayTake {
            address,
            channel,
            source,
            key,
            expected_revision,
        } => remote::execute(
            address,
            fm_protocol::CommandPayload::TakeOverlay {
                channel: fm_protocol::WireOverlayChannelId::new(channel)
                    .expect("CLI parser validates overlays"),
                source: fm_protocol::WireInputId::new(
                    NonZeroU128::new(source)
                        .ok_or_else(|| AppFailure("input ID must be nonzero".into()))?,
                ),
            },
            key,
            expected_revision,
        )?,
        Command::RemoteOverlayUpdate {
            address,
            channel,
            source,
            key,
            expected_revision,
        } => remote::execute(
            address,
            fm_protocol::CommandPayload::UpdateOverlay {
                channel: fm_protocol::WireOverlayChannelId::new(channel)
                    .expect("CLI parser validates overlays"),
                source: fm_protocol::WireInputId::new(
                    NonZeroU128::new(source)
                        .ok_or_else(|| AppFailure("input ID must be nonzero".into()))?,
                ),
            },
            key,
            expected_revision,
        )?,
        Command::RemoteOverlayOff {
            address,
            channel,
            key,
            expected_revision,
        } => remote::execute(
            address,
            fm_protocol::CommandPayload::OverlayOff {
                channel: fm_protocol::WireOverlayChannelId::new(channel)
                    .expect("CLI parser validates overlays"),
            },
            key,
            expected_revision,
        )?,
        Command::RemoteOverlayOutput {
            address,
            channel,
            output,
            included,
            key,
            expected_revision,
        } => remote::execute(
            address,
            fm_protocol::CommandPayload::SetOverlayOutputInclusion {
                channel: fm_protocol::WireOverlayChannelId::new(channel)
                    .expect("CLI parser validates overlays"),
                output: fm_protocol::WireOutputId::new(
                    NonZeroU128::new(output)
                        .ok_or_else(|| AppFailure("output ID must be nonzero".into()))?,
                ),
                included,
            },
            key,
            expected_revision,
        )?,
        Command::RemoteOverlayTransition {
            address,
            channel,
            transition,
            frames,
            key,
            expected_revision,
        } => remote::execute(
            address,
            fm_protocol::CommandPayload::ConfigureOverlayTransition {
                channel: fm_protocol::WireOverlayChannelId::new(channel)
                    .expect("CLI parser validates overlays"),
                transition: protocol_overlay_transition(transition),
                duration_frames: frames,
            },
            key,
            expected_revision,
        )?,
        Command::RemoteOverlayAppearance {
            address,
            channel,
            position,
            border,
            key,
            expected_revision,
        } => remote::execute(
            address,
            fm_protocol::CommandPayload::ConfigureOverlayAppearance {
                channel: fm_protocol::WireOverlayChannelId::new(channel)
                    .expect("CLI parser validates overlays"),
                position: protocol_overlay_position(position),
                border: protocol_overlay_border(border),
            },
            key,
            expected_revision,
        )?,
        Command::RemoteOverlayQueue {
            address,
            channel,
            source,
            key,
            expected_revision,
        } => remote::execute(
            address,
            fm_protocol::CommandPayload::QueueOverlay {
                channel: fm_protocol::WireOverlayChannelId::new(channel)
                    .expect("CLI parser validates overlays"),
                source: fm_protocol::WireInputId::new(
                    NonZeroU128::new(source)
                        .ok_or_else(|| AppFailure("source input ID must be nonzero".into()))?,
                ),
            },
            key,
            expected_revision,
        )?,
        Command::RemoteOverlayNext {
            address,
            channel,
            key,
            expected_revision,
        } => remote::execute(
            address,
            fm_protocol::CommandPayload::TakeNextOverlay {
                channel: fm_protocol::WireOverlayChannelId::new(channel)
                    .expect("CLI parser validates overlays"),
            },
            key,
            expected_revision,
        )?,
        Command::RemoteWipe {
            address,
            frames,
            key,
            expected_revision,
        } => remote::execute(
            address,
            fm_protocol::CommandPayload::Wipe {
                duration_frames: frames,
            },
            key,
            expected_revision,
        )?,
        Command::RemoteTBar {
            address,
            action,
            key,
            expected_revision,
        } => remote::execute(
            address,
            protocol_t_bar_payload(action),
            key,
            expected_revision,
        )?,
        Command::RemoteFadeToBlack {
            address,
            target,
            frames,
            key,
            expected_revision,
        } => remote::execute(
            address,
            fm_protocol::CommandPayload::FadeToBlack {
                active: target.active(),
                duration_frames: frames,
            },
            key,
            expected_revision,
        )?,
        Command::Render {
            path,
            output,
            width,
            height,
        } => render(&path, &output, width, height)?,
        Command::Demo { path, output } => demo(&path, output.as_deref())?,
        Command::Help => print_help(),
    }
    Ok(())
}

fn default_project(name: String) -> AppResult<ProjectEngine> {
    let first = input_id(1)?;
    let second = input_id(2)?;
    let frame_rate = FrameRate::new(60_000, 1_001)?;
    let settings = ProjectSettings {
        frame_rate,
        video: VideoFormat {
            dimensions: VideoDimensions::new(1_920, 1_080)
                .ok_or_else(|| AppFailure("default video dimensions are invalid".into()))?,
            frame_rate,
            pixel_format: PixelFormat::Nv12,
            scan: ScanMode::Progressive,
            color: ColorMetadata::default(),
        },
        audio: AudioFormat {
            sample_rate: SampleRate::new(48_000)
                .ok_or_else(|| AppFailure("default audio sample rate is invalid".into()))?,
            sample_format: SampleFormat::F32,
            channels: ChannelLayout::stereo(),
        },
    };
    let mut project = Project::new(generate_project_id()?, name, settings)
        .with_main_mix(MainMix::new(first, second));
    project.add_input(Input {
        id: first,
        name: "Input 1".into(),
        kind: InputKind::Simulated(SimulatedInput::new(
            SimulatedVideo::Solid(SolidColor::new(73, 151, 199, u8::MAX)),
            SimulatedAudio::Silence,
        )),
        required_capabilities: Vec::new(),
    });
    project.add_input(Input {
        id: second,
        name: "Input 2".into(),
        kind: InputKind::Simulated(SimulatedInput::new(
            SimulatedVideo::Solid(SolidColor::new(146, 46, 142, u8::MAX)),
            SimulatedAudio::Sine {
                frequency_hz: 1_000,
            },
        )),
        required_capabilities: Vec::new(),
    });
    project
        .validate()
        .map_err(|errors| AppFailure(format!("default project is invalid: {errors:?}")))?;
    engine_from_project(project)
}

fn engine_from_project(project: Project) -> AppResult<ProjectEngine> {
    let main_mix = required_main_mix(&project)?;
    let inputs = project
        .inputs()
        .iter()
        .map(|input| (input.id, input.name.clone()))
        .collect();
    let mut show = ShowState::new(
        project.name(),
        inputs,
        main_mix.desired_program,
        main_mix.desired_preview,
    )?
    .with_outputs(
        project
            .outputs()
            .iter()
            .map(|output| (output.id, output.name.clone()))
            .collect(),
    )?;
    restore_input_audio_strips(&mut show, &project)?;
    let engine = Engine::new(show, project.settings().frame_rate, clock_domain());
    Ok(ProjectEngine { project, engine })
}

fn mutate(
    path: &Path,
    command: EngineCommand,
    ticks: u32,
    key: Option<String>,
    expected_revision: Option<u64>,
) -> AppResult<()> {
    let mut project = load_engine(path)?;
    let ticks = match &command {
        EngineCommand::FadeToBlack { active, .. }
            if project.engine.realized_fade_to_black().active == *active =>
        {
            1
        }
        EngineCommand::TakeOverlay { channel, .. } | EngineCommand::OverlayOff { channel } => {
            let overlay = project.engine.show().desired_switcher().overlay(*channel);
            match overlay.transition() {
                OverlayTransitionKind::Cut => 1,
                OverlayTransitionKind::Fade => overlay.duration_frames(),
            }
        }
        _ => ticks,
    };
    let result = execute(&mut project.engine, command, ticks, key, expected_revision)?;
    if result.replayed {
        if let Some(rejection) = result.rejection {
            return Err(rejection.into());
        }
        print_status(&project);
        return Ok(());
    }
    save_engine(path, &project)?;
    if let Some(rejection) = result.rejection {
        return Err(rejection.into());
    }
    print_status(&project);
    Ok(())
}

fn execute(
    engine: &mut Engine,
    command: EngineCommand,
    ticks: u32,
    key: Option<String>,
    expected_revision: Option<u64>,
) -> AppResult<ExecuteResult> {
    let key = match key {
        Some(key) => key,
        None => generate_implicit_key(&command, expected_revision)?,
    };
    let mut envelope = CommandEnvelope::new(format!("command-{key}"), key, command);
    if let Some(revision) = expected_revision {
        envelope = envelope.expecting(Revision::new(revision));
    }
    let outcome = engine.execute(envelope, 0)?;
    let rejection = outcome.receipt.rejected().map(|rejected| {
        AppFailure(format!(
            "{}: {}",
            rejected.rejection.code, rejected.rejection.message
        ))
    });
    if rejection.is_none() && !outcome.replayed {
        for _ in 0..ticks {
            engine.tick()?;
        }
    }
    Ok(ExecuteResult {
        replayed: outcome.replayed,
        rejection,
    })
}

fn generate_implicit_key(
    command: &EngineCommand,
    expected_revision: Option<u64>,
) -> AppResult<String> {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let sequence = IMPLICIT_KEY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    Ok(format!(
        "cli:{timestamp:032x}:{:08x}:{sequence:016x}:{command:?}:expect={expected_revision:?}",
        std::process::id()
    ))
}

fn persisted_receipt(
    (key, receipt): &(IdempotencyKey, CommandReceipt<EngineAcceptance>),
) -> AppResult<IdempotencyReceipt> {
    Ok(match receipt {
        CommandReceipt::Accepted {
            command_id,
            acceptance,
        } => IdempotencyReceipt::accepted(
            key.as_str(),
            command_id.as_str(),
            acceptance.revision.get(),
            acceptance.result.target_frame.get(),
        ),
        CommandReceipt::Rejected {
            command_id,
            rejection,
        } => IdempotencyReceipt::rejected(
            key.as_str(),
            command_id.as_str(),
            rejection.current_revision.get(),
            persisted_rejection_code(rejection.rejection.code)?,
            &rejection.rejection.message,
            rejection.rejection.retryable,
        ),
    })
}

fn runtime_receipt(
    receipt: &IdempotencyReceipt,
) -> AppResult<(IdempotencyKey, CommandReceipt<EngineAcceptance>)> {
    let command_id = CommandId::new(receipt.command_id());
    let runtime = match receipt.outcome() {
        ReceiptOutcome::Accepted {
            revision,
            target_frame,
        } => CommandReceipt::Accepted {
            command_id,
            acceptance: AcceptedReceipt {
                revision: Revision::new(*revision),
                result: EngineAcceptance {
                    target_frame: (*target_frame).into(),
                },
            },
        },
        ReceiptOutcome::Rejected {
            current_revision,
            code,
            message,
            retryable,
        } => CommandReceipt::Rejected {
            command_id,
            rejection: RejectedReceipt {
                rejection: Rejection::new(runtime_rejection_code(code)?, message)
                    .retryable(*retryable),
                current_revision: Revision::new(*current_revision),
            },
        },
    };
    Ok((IdempotencyKey::new(receipt.key()), runtime))
}

fn persisted_rejection_code(code: RejectionCode) -> AppResult<&'static str> {
    let stable = match code {
        RejectionCode::PermissionDenied => "permission_denied",
        RejectionCode::DeadlineExceeded => "deadline_exceeded",
        RejectionCode::RevisionConflict => "revision_conflict",
        RejectionCode::InvalidCommand => "invalid_command",
        RejectionCode::NotFound => "not_found",
        RejectionCode::Conflict => "conflict",
        RejectionCode::Unavailable => "unavailable",
        RejectionCode::ResourceExhausted => "resource_exhausted",
        RejectionCode::Internal => "internal",
        _ => {
            return Err(AppFailure(format!(
                "cannot persist unknown rejection code `{}`",
                code.as_str()
            ))
            .into());
        }
    };
    Ok(stable)
}

fn runtime_rejection_code(code: &str) -> AppResult<RejectionCode> {
    match code {
        "permission_denied" => Ok(RejectionCode::PermissionDenied),
        "deadline_exceeded" => Ok(RejectionCode::DeadlineExceeded),
        "revision_conflict" => Ok(RejectionCode::RevisionConflict),
        "invalid_command" => Ok(RejectionCode::InvalidCommand),
        "not_found" => Ok(RejectionCode::NotFound),
        "conflict" => Ok(RejectionCode::Conflict),
        "unavailable" => Ok(RejectionCode::Unavailable),
        "resource_exhausted" => Ok(RejectionCode::ResourceExhausted),
        "internal" => Ok(RejectionCode::Internal),
        _ => Err(AppFailure(format!("project contains unknown rejection code `{code}`")).into()),
    }
}

fn save_engine(path: &Path, project_engine: &ProjectEngine) -> AppResult<()> {
    let snapshot = project_engine.engine.snapshot()?;
    let desired = snapshot.show().desired_switcher();
    let realized = snapshot.realized_switcher();
    let mut project = project_engine.project.clone();
    project.set_main_mix(MainMix::new(desired.program(), desired.preview()));
    project.reorder_inputs(snapshot.show().inputs().to_vec())?;
    sync_input_names(&mut project, snapshot.show())?;
    sync_input_audio_strips(&mut project, snapshot.show())?;
    let stored = StoredProject::from_project_with_complete_runtime_state(
        project,
        RuntimeRouting {
            desired_program_id: Some(desired.program()),
            realized_program_id: Some(realized.program()),
            desired_preview_id: Some(desired.preview()),
            realized_preview_id: Some(realized.preview()),
        },
        RuntimeManualTransitions {
            desired: desired.t_bar().map(persisted_t_bar),
            realized: realized.t_bar().map(persisted_t_bar),
        },
        RuntimeFadeToBlack {
            desired: persisted_fade_to_black(snapshot.desired_fade_to_black()),
            realized: persisted_fade_to_black(snapshot.realized_fade_to_black()),
        },
        persisted_overlays(desired, realized),
        ProjectPosition {
            revision: snapshot.revision().get(),
            state_epoch: snapshot.state_epoch().get(),
            event_sequence: snapshot.event_sequence().get(),
            frames_rendered: snapshot.frames_rendered(),
            runtime_generation: snapshot.runtime_generation().get(),
            clock_time_nanos: snapshot.clock_time().as_nanos(),
        },
        snapshot
            .receipts()
            .iter()
            .map(persisted_receipt)
            .collect::<AppResult<Vec<_>>>()?,
    )?;
    ProjectStore::new(path)?.save(&stored)?;
    Ok(())
}

fn sync_input_names(project: &mut Project, show: &ShowState) -> AppResult<()> {
    for (&input, name) in show.inputs().iter().zip(show.input_names()) {
        if project
            .inputs()
            .iter()
            .find(|candidate| candidate.id == input)
            .is_none_or(|candidate| candidate.name != *name)
        {
            project.rename_input(input, name.clone())?;
        }
    }
    Ok(())
}

fn configure_stinger(path: &Path, config: StingerConfig) -> AppResult<()> {
    update_stingers(path, |project| project.set_stinger(config))
}

fn add_input(path: &Path, input: InputId, name: String) -> AppResult<()> {
    let store = ProjectStore::new(path)?;
    let stored = load_stored_project(path)?;
    let mut project = stored.project().clone();
    project.add_input(Input {
        id: input,
        name,
        kind: InputKind::Simulated(SimulatedInput::new(
            SimulatedVideo::Bars,
            SimulatedAudio::Silence,
        )),
        required_capabilities: Vec::new(),
    });
    let configured = StoredProject::from_project_with_complete_runtime_state(
        project,
        stored.runtime_routing(),
        stored.runtime_manual_transitions(),
        stored.runtime_fade_to_black(),
        stored.runtime_overlays().clone(),
        stored.position(),
        stored.idempotency_receipts().to_vec(),
    )?;
    store.save(&configured)?;
    print_status(&load_engine(path)?);
    Ok(())
}

fn add_scene_input(path: &Path, input: InputId, scene: SceneId, name: String) -> AppResult<()> {
    let store = ProjectStore::new(path)?;
    let stored = load_stored_project(path)?;
    let mut project = stored.project().clone();
    project.add_scene(Scene {
        id: scene,
        name: name.clone(),
        background: ModelRgba8::OPAQUE_BLACK,
        layers: Vec::new(),
    });
    project.add_input(Input {
        id: input,
        name,
        kind: InputKind::Scene {
            scene_id: scene,
            audio_source: None,
        },
        required_capabilities: Vec::new(),
    });
    let configured = StoredProject::from_project_with_complete_runtime_state(
        project,
        stored.runtime_routing(),
        stored.runtime_manual_transitions(),
        stored.runtime_fade_to_black(),
        stored.runtime_overlays().clone(),
        stored.position(),
        stored.idempotency_receipts().to_vec(),
    )?;
    store.save(&configured)?;
    print_status(&load_engine(path)?);
    Ok(())
}

fn add_scene_layer(
    path: &Path,
    scene: SceneId,
    source: InputId,
    z_order: i32,
    name: String,
) -> AppResult<()> {
    let store = ProjectStore::new(path)?;
    let stored = load_stored_project(path)?;
    let mut project = stored.project().clone();
    let dimensions = project.settings().video.dimensions;
    project.add_layer_to_scene(
        scene,
        Layer {
            name,
            source: SourceRef::Input(source),
            enabled: true,
            geometry: LayerGeometry::new(
                0,
                0,
                dimensions.width(),
                dimensions.height(),
                Rotation::Deg0,
            ),
            crop: None,
            mask: None,
            opacity: u8::MAX,
            z_order,
        },
    )?;
    let configured = StoredProject::from_project_with_complete_runtime_state(
        project,
        stored.runtime_routing(),
        stored.runtime_manual_transitions(),
        stored.runtime_fade_to_black(),
        stored.runtime_overlays().clone(),
        stored.position(),
        stored.idempotency_receipts().to_vec(),
    )?;
    store.save(&configured)?;
    print_status(&load_engine(path)?);
    Ok(())
}

fn remove_scene_layer(path: &Path, scene: SceneId, index: usize) -> AppResult<()> {
    let store = ProjectStore::new(path)?;
    let stored = load_stored_project(path)?;
    let mut project = stored.project().clone();
    project.remove_layer_from_scene(scene, index)?;
    let configured = StoredProject::from_project_with_complete_runtime_state(
        project,
        stored.runtime_routing(),
        stored.runtime_manual_transitions(),
        stored.runtime_fade_to_black(),
        stored.runtime_overlays().clone(),
        stored.position(),
        stored.idempotency_receipts().to_vec(),
    )?;
    store.save(&configured)?;
    print_status(&load_engine(path)?);
    Ok(())
}

fn remove_input(path: &Path, input: InputId) -> AppResult<()> {
    let stored = load_stored_project(path)?;
    let runtime = stored.runtime_routing();
    let transitions = stored.runtime_manual_transitions();
    let overlays = stored.runtime_overlays();
    let referenced = [
        runtime.desired_program_id,
        runtime.realized_program_id,
        runtime.desired_preview_id,
        runtime.realized_preview_id,
    ]
    .into_iter()
    .flatten()
    .any(|candidate| candidate == input)
        || [transitions.desired, transitions.realized]
            .into_iter()
            .flatten()
            .any(|state| state.from_id == input || state.to_id == input)
        || overlays
            .desired
            .iter()
            .chain(overlays.realized.iter())
            .any(|channel| {
                channel.source == Some(input) || channel.queued_sources.contains(&input)
            });
    if referenced {
        return Err(AppFailure(format!("cannot remove input {input}: runtime reference")).into());
    }
    let store = ProjectStore::new(path)?;
    let mut project = stored.project().clone();
    project.remove_input(input)?;
    let configured = StoredProject::from_project_with_complete_runtime_state(
        project,
        stored.runtime_routing(),
        transitions,
        stored.runtime_fade_to_black(),
        overlays.clone(),
        stored.position(),
        stored.idempotency_receipts().to_vec(),
    )?;
    store.save(&configured)?;
    print_status(&load_engine(path)?);
    Ok(())
}

fn duplicate_input(path: &Path, source: InputId, input: InputId, name: String) -> AppResult<()> {
    let stored = load_stored_project(path)?;
    let source_input = stored
        .project()
        .inputs()
        .iter()
        .find(|candidate| candidate.id == source)
        .ok_or_else(|| AppFailure(format!("unknown source input {source}")))?;
    let source_strip = stored
        .project()
        .input_audio_strip(source)
        .ok_or_else(|| AppFailure(format!("project is missing audio strip for input {source}")))?;
    let store = ProjectStore::new(path)?;
    let mut project = stored.project().clone();
    project.add_input(Input {
        id: input,
        name,
        kind: source_input.kind.clone(),
        required_capabilities: source_input.required_capabilities.clone(),
    });
    if !project.set_input_audio_strip(input, source_strip) {
        return Err(AppFailure(format!("project is missing audio strip for input {input}")).into());
    }
    let configured = StoredProject::from_project_with_complete_runtime_state(
        project,
        stored.runtime_routing(),
        stored.runtime_manual_transitions(),
        stored.runtime_fade_to_black(),
        stored.runtime_overlays().clone(),
        stored.position(),
        stored.idempotency_receipts().to_vec(),
    )?;
    store.save(&configured)?;
    print_status(&load_engine(path)?);
    Ok(())
}

fn replace_input_simulated(path: &Path, input: InputId) -> AppResult<()> {
    let stored = load_stored_project(path)?;
    let mut project = stored.project().clone();
    project.replace_input_source(
        input,
        InputKind::Simulated(SimulatedInput::new(
            SimulatedVideo::Bars,
            SimulatedAudio::Silence,
        )),
        Vec::new(),
    )?;
    let configured = StoredProject::from_project_with_complete_runtime_state(
        project,
        stored.runtime_routing(),
        stored.runtime_manual_transitions(),
        stored.runtime_fade_to_black(),
        stored.runtime_overlays().clone(),
        stored.position(),
        stored.idempotency_receipts().to_vec(),
    )?;
    ProjectStore::new(path)?.save(&configured)?;
    print_status(&load_engine(path)?);
    Ok(())
}

fn remove_stinger(path: &Path, slot: StingerSlotNumber) -> AppResult<()> {
    update_stingers(path, |project| {
        let _ = project.remove_stinger(slot);
    })
}

fn update_stingers(path: &Path, update: impl FnOnce(&mut Project)) -> AppResult<()> {
    let store = ProjectStore::new(path)?;
    let stored = load_stored_project(path)?;
    let mut project = stored.project().clone();
    update(&mut project);
    let configured = StoredProject::from_project_with_complete_runtime_state(
        project,
        stored.runtime_routing(),
        stored.runtime_manual_transitions(),
        stored.runtime_fade_to_black(),
        stored.runtime_overlays().clone(),
        stored.position(),
        stored.idempotency_receipts().to_vec(),
    )?;
    store.save(&configured)?;
    print_status(&load_engine(path)?);
    Ok(())
}

fn load_engine(path: &Path) -> AppResult<ProjectEngine> {
    let stored = load_stored_project(path)?;
    let project = stored.project().clone();
    let inputs = project
        .inputs()
        .iter()
        .map(|input| (input.id, input.name.clone()))
        .collect::<Vec<_>>();
    let input_ids = inputs.iter().map(|(input, _)| *input).collect::<Vec<_>>();
    let main_mix = required_main_mix(&project)?;
    let routing = stored.runtime_routing();
    let realized_program = required_routing(routing.realized_program_id, "realized program")?;
    let realized_preview = required_routing(routing.realized_preview_id, "realized preview")?;
    let mut show = ShowState::new(
        project.name(),
        inputs,
        main_mix.desired_program,
        main_mix.desired_preview,
    )?
    .with_outputs(
        project
            .outputs()
            .iter()
            .map(|output| (output.id, output.name.clone()))
            .collect(),
    )?;
    restore_input_audio_strips(&mut show, &project)?;
    let mut realized = SwitcherState::new(input_ids, realized_program, realized_preview)?;
    for config in project.stingers() {
        restore_stinger(&mut show, &mut realized, *config)?;
    }
    let manual = stored.runtime_manual_transitions();
    if let Some(state) = manual.desired {
        show.restore_manual_transition(restored_t_bar(state)?)?;
    }
    if let Some(state) = manual.realized {
        realized.restore_t_bar(restored_t_bar(state)?)?;
    }
    let fade_to_black = stored.runtime_fade_to_black();
    show.restore_fade_to_black(fade_to_black.desired.target_active);
    realized.restore_settled_fade_to_black(fade_to_black.realized.target_active);
    restore_overlays(&mut show, &mut realized, stored.runtime_overlays())?;
    let position = stored.position();
    let engine = Engine::restore_persisted(
        show,
        realized,
        project.settings().frame_rate,
        clock_domain(),
        EngineRestoreState {
            state_epoch: StateEpoch::new(position.state_epoch),
            revision: Revision::new(position.revision),
            event_sequence: EventSequence::new(position.event_sequence),
            runtime_generation: RuntimeGeneration::new(position.runtime_generation),
            clock_time: ClockTime::from_nanos(position.clock_time_nanos),
            frame_cursor: position.frames_rendered.into(),
            receipts: stored
                .idempotency_receipts()
                .iter()
                .map(runtime_receipt)
                .collect::<AppResult<Vec<_>>>()?,
        },
    )?;
    Ok(ProjectEngine { project, engine })
}

fn engine_audio_strip_state(
    gain_millidb: i32,
    balance_basis_points: i32,
    muted: bool,
    soloed: bool,
    follow_video: bool,
    delay_samples: u32,
) -> AppResult<EngineInputAudioStripState> {
    Ok(EngineInputAudioStripState {
        gain_millidb: InputGainMilliDb::new(gain_millidb)
            .ok_or_else(|| AppFailure("gain must be in -96000..=24000 millidB".into()))?
            .get(),
        balance_basis_points: InputBalanceBasisPoints::new(balance_basis_points)
            .ok_or_else(|| AppFailure("balance must be in -10000..=10000 basis points".into()))?
            .get(),
        muted,
        soloed,
        follow_video,
        delay_samples: InputDelaySamples::new(delay_samples)
            .ok_or_else(|| AppFailure("delay samples must be in 0..=48000".into()))?
            .get(),
    })
}

fn restore_input_audio_strips(show: &mut ShowState, project: &Project) -> AppResult<()> {
    for strip in project.input_audio_strips() {
        show.set_input_audio_strip(
            strip.input,
            EngineInputAudioStripState {
                gain_millidb: strip.state.gain.get(),
                balance_basis_points: strip.state.balance.get(),
                muted: strip.state.muted,
                soloed: strip.state.soloed,
                follow_video: strip.state.follow_video,
                delay_samples: strip.state.delay_samples.get(),
            },
        )?;
    }
    Ok(())
}

fn sync_input_audio_strips(project: &mut Project, show: &ShowState) -> AppResult<()> {
    for (&input, &state) in show.input_audio_strips() {
        project.input_audio_strip(input).ok_or_else(|| {
            AppFailure(format!("project is missing audio strip for input {input}"))
        })?;
        let strip = InputAudioStripState {
            gain: InputGainMilliDb::new(state.gain_millidb)
                .expect("engine input audio gain is bounded by the model contract"),
            balance: InputBalanceBasisPoints::new(state.balance_basis_points)
                .expect("engine input audio balance is bounded by the model contract"),
            muted: state.muted,
            soloed: state.soloed,
            follow_video: state.follow_video,
            delay_samples: InputDelaySamples::new(state.delay_samples)
                .expect("engine input audio delay is bounded by the model contract"),
        };
        if !project.set_input_audio_strip(input, strip) {
            return Err(AppFailure(format!("project is missing input {input}")).into());
        }
    }
    Ok(())
}

fn persisted_overlays(desired: &SwitcherState, realized: &SwitcherState) -> RuntimeOverlays {
    RuntimeOverlays {
        desired: std::array::from_fn(|index| persisted_overlay(&desired.overlays()[index])),
        realized: std::array::from_fn(|index| persisted_overlay(&realized.overlays()[index])),
    }
}

fn persisted_overlay(channel: &OverlayChannelState) -> RuntimeOverlayChannel {
    RuntimeOverlayChannel {
        source: channel.source(),
        active: channel.is_active(),
        transition: match channel.transition() {
            OverlayTransitionKind::Cut => RuntimeOverlayTransition::Cut,
            OverlayTransitionKind::Fade => RuntimeOverlayTransition::Fade,
        },
        duration_frames: channel.duration_frames(),
        position: persisted_overlay_position(channel.position()),
        border: persisted_overlay_border(channel.border()),
        queued_sources: channel.queued_sources().to_vec(),
        included_outputs: channel.included_outputs().to_vec(),
    }
}

fn persisted_overlay_position(position: OverlayPositionPreset) -> RuntimeOverlayPosition {
    match position {
        OverlayPositionPreset::FullFrame => RuntimeOverlayPosition::FullFrame,
        OverlayPositionPreset::TopLeft => RuntimeOverlayPosition::TopLeft,
        OverlayPositionPreset::TopRight => RuntimeOverlayPosition::TopRight,
        OverlayPositionPreset::BottomLeft => RuntimeOverlayPosition::BottomLeft,
        OverlayPositionPreset::BottomRight => RuntimeOverlayPosition::BottomRight,
    }
}

fn restored_overlay_position(position: RuntimeOverlayPosition) -> OverlayPositionPreset {
    match position {
        RuntimeOverlayPosition::FullFrame => OverlayPositionPreset::FullFrame,
        RuntimeOverlayPosition::TopLeft => OverlayPositionPreset::TopLeft,
        RuntimeOverlayPosition::TopRight => OverlayPositionPreset::TopRight,
        RuntimeOverlayPosition::BottomLeft => OverlayPositionPreset::BottomLeft,
        RuntimeOverlayPosition::BottomRight => OverlayPositionPreset::BottomRight,
    }
}

fn persisted_overlay_border(border: OverlayBorderPreset) -> RuntimeOverlayBorder {
    match border {
        OverlayBorderPreset::None => RuntimeOverlayBorder::None,
        OverlayBorderPreset::ThinWhite => RuntimeOverlayBorder::ThinWhite,
        OverlayBorderPreset::ThickWhite => RuntimeOverlayBorder::ThickWhite,
    }
}

fn restored_overlay_border(border: RuntimeOverlayBorder) -> OverlayBorderPreset {
    match border {
        RuntimeOverlayBorder::None => OverlayBorderPreset::None,
        RuntimeOverlayBorder::ThinWhite => OverlayBorderPreset::ThinWhite,
        RuntimeOverlayBorder::ThickWhite => OverlayBorderPreset::ThickWhite,
    }
}

fn restore_overlays(
    show: &mut ShowState,
    realized: &mut SwitcherState,
    overlays: &RuntimeOverlays,
) -> AppResult<()> {
    for (index, (desired, realized_state)) in
        overlays.desired.iter().zip(&overlays.realized).enumerate()
    {
        let channel = OverlayChannelId::from_index(index).expect("overlay index is bounded");
        restore_overlay(show.desired_switcher_mut(), channel, desired)?;
        restore_overlay(realized, channel, realized_state)?;
    }
    Ok(())
}

fn restore_overlay(
    switcher: &mut SwitcherState,
    channel: OverlayChannelId,
    state: &RuntimeOverlayChannel,
) -> AppResult<()> {
    switcher.configure_overlay_transition(
        channel,
        match state.transition {
            RuntimeOverlayTransition::Cut => OverlayTransitionKind::Cut,
            RuntimeOverlayTransition::Fade => OverlayTransitionKind::Fade,
        },
        state.duration_frames,
    )?;
    let _ = switcher.configure_overlay_appearance(
        channel,
        restored_overlay_position(state.position),
        restored_overlay_border(state.border),
    );
    if let Some(source) = state.source {
        if state.active {
            switcher.take_overlay(channel, source)?;
        } else {
            switcher.update_overlay(channel, source)?;
        }
    }
    for source in &state.queued_sources {
        switcher.queue_overlay(channel, *source)?;
    }
    for output in &state.included_outputs {
        let _ = switcher.set_overlay_output_inclusion(channel, *output, true);
    }
    Ok(())
}

fn restore_stinger(
    show: &mut ShowState,
    realized: &mut SwitcherState,
    config: StingerConfig,
) -> AppResult<()> {
    let slot = StingerSlotId::new(config.slot.number())
        .expect("validated model Stinger slots are in the switcher range");
    let descriptor = StingerDescriptor::new(
        config.media_input,
        config.preload,
        config.cut_point_frames,
        match config.audio_policy {
            ModelStingerAudioPolicy::Muted => StingerAudioPolicy::Muted,
            ModelStingerAudioPolicy::StingerOnly => StingerAudioPolicy::StingerOnly,
            ModelStingerAudioPolicy::MixWithProgram => StingerAudioPolicy::MixWithProgram,
        },
        match config.missing_media_fallback {
            StingerMissingMediaFallback::Cut => MissingMediaFallback::Cut,
            StingerMissingMediaFallback::Fade => MissingMediaFallback::Fade,
            StingerMissingMediaFallback::KeepProgram => MissingMediaFallback::KeepProgram,
        },
    );
    show.configure_stinger(slot, descriptor)?;
    realized.configure_stinger(slot, descriptor)?;
    if config.preload {
        let _ = show.preload_stinger(slot, true)?;
        let _ = realized.preload_stinger(slot, true)?;
    }
    Ok(())
}

fn persisted_fade_to_black(state: fm_engine::EngineFadeToBlackState) -> PersistedFadeToBlackState {
    PersistedFadeToBlackState::new(
        state.active,
        u16::try_from(state.position.numerator())
            .expect("engine fade-to-black numerator uses the u16 contract"),
    )
}

fn persisted_t_bar(state: TBarState) -> PersistedManualTransitionState {
    let kind = match state.kind() {
        TransitionKind::Fade => PersistedManualTransitionKind::Fade,
        TransitionKind::Wipe => PersistedManualTransitionKind::Wipe,
        TransitionKind::AlphaFade => PersistedManualTransitionKind::AlphaFade,
        TransitionKind::Slide => PersistedManualTransitionKind::Slide,
        _ => unreachable!("engine manual transitions are Fade, Wipe, AlphaFade, or Slide"),
    };
    PersistedManualTransitionState::new(
        kind,
        state.from(),
        state.to(),
        state.interval_start().basis_points(),
        state.position().basis_points(),
    )
    .expect("engine manual transition positions are bounded")
}

fn restored_t_bar(state: PersistedManualTransitionState) -> AppResult<TBarState> {
    let kind = match state.kind {
        PersistedManualTransitionKind::Fade => TransitionKind::Fade,
        PersistedManualTransitionKind::Wipe => TransitionKind::Wipe,
        PersistedManualTransitionKind::AlphaFade => TransitionKind::AlphaFade,
        PersistedManualTransitionKind::Slide => TransitionKind::Slide,
    };
    let interval_start = TBarPosition::new(state.interval_start_basis_points)
        .ok_or_else(|| AppFailure("invalid persisted manual-transition interval start".into()))?;
    let position = TBarPosition::new(state.position_basis_points)
        .ok_or_else(|| AppFailure("invalid persisted manual-transition position".into()))?;
    Ok(TBarState::restore(
        kind,
        state.from_id,
        state.to_id,
        interval_start,
        position,
    ))
}

fn load_stored_project(path: &Path) -> AppResult<StoredProject> {
    let store = ProjectStore::new(path)?;
    let project = store.load()?;
    if store.journal_path().try_exists()? {
        let scan = store.scan_journal()?;
        if !scan.batches().is_empty() {
            return Err(AppFailure(
                "project has unapplied journal batches that freemix-cli cannot safely interpret"
                    .into(),
            )
            .into());
        }
    }
    Ok(project)
}

fn render(path: &Path, output: &Path, width: u32, height: u32) -> AppResult<()> {
    let project = load_engine(path)?;
    let engine = &project.engine;
    let mut pipeline = SimulatedPipeline::new(width, height)?;
    for input in project.project.inputs() {
        pipeline.register(SimulatedSource::new(input.id, source_pattern(input)?))?;
    }
    let frame = pipeline.render(
        engine.frame_cursor().get(),
        engine.realized_switcher().program_frame(),
    )?;
    write_ppm_atomic(output, |file| write_ppm(&frame, file))?;
    println!(
        "rendered {}x{} Program input {} to {}",
        width,
        height,
        engine.realized_switcher().program(),
        output.display()
    );
    Ok(())
}

fn write_ppm_atomic(
    output: &Path,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut name = std::ffi::OsString::from(".");
    name.push(output.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "PPM output has no file name")
    })?);
    name.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        PPM_OUTPUT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let temp = parent.join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let mut cleanup = PpmTempGuard(Some(temp.clone()));
    write(&mut file)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, output)?;
    cleanup.disarm();
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

struct PpmTempGuard(Option<PathBuf>);

impl PpmTempGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for PpmTempGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

fn demo(path: &Path, output: Option<&Path>) -> AppResult<()> {
    let mut project = default_project("FreeMix MVP Demo".into())?;
    println!("created deterministic two-input show");
    print_status(&project);

    reject_unexpected(execute(
        &mut project.engine,
        EngineCommand::Cut,
        1,
        Some("demo-cut".into()),
        Some(0),
    )?)?;
    println!("realized Cut on frame boundary");
    print_status(&project);

    reject_unexpected(execute(
        &mut project.engine,
        EngineCommand::Fade { duration_frames: 4 },
        4,
        Some("demo-fade".into()),
        Some(1),
    )?)?;
    println!("realized four-frame Fade");
    save_engine(path, &project)?;
    print_status(&project);

    let reloaded = load_engine(path)?;
    println!("reloaded persisted show");
    print_status(&reloaded);
    if let Some(output) = output {
        render(path, output, 640, 360)?;
    }
    Ok(())
}

fn reject_unexpected(result: ExecuteResult) -> AppResult<()> {
    match result.rejection {
        Some(rejection) => Err(rejection.into()),
        None => Ok(()),
    }
}

fn print_status(project: &ProjectEngine) {
    let engine = &project.engine;
    let desired = engine.show().desired_switcher();
    let realized = engine.realized_switcher();
    println!(
        "project_id={} show={:?} revision={} frame={} Program(desired={}, realized={}) Preview(desired={}, realized={}) TBar(desired={}, realized={}) FTB(desired={}, realized={}) Overlays(desired={}, realized={}) AudioStrips={} Stingers={}",
        project.project.id(),
        engine.show().name(),
        engine.revision(),
        engine.frame_cursor(),
        desired.program(),
        realized.program(),
        desired.preview(),
        realized.preview(),
        format_t_bar(desired.t_bar()),
        format_t_bar(realized.t_bar()),
        format_fade_to_black(engine.desired_fade_to_black()),
        format_fade_to_black(engine.realized_fade_to_black()),
        format_overlays(desired.overlays()),
        format_overlays(realized.overlays()),
        format_audio_strips(&project.project),
        format_stingers(project.project.stingers()),
    );
}

fn format_audio_strips(project: &Project) -> String {
    let strips = project
        .input_audio_strips()
        .iter()
        .map(|strip| {
            let name = project
                .inputs()
                .iter()
                .find(|input| input.id == strip.input)
                .map_or("", |input| input.name.as_str());
            format!(
                "{}:{name:?}:gain_mdb={}:balance_bp={}:delay_samples={}:{}:{}:{}",
                strip.input,
                strip.state.gain.get(),
                strip.state.balance.get(),
                strip.state.delay_samples.get(),
                if strip.state.muted { "muted" } else { "live" },
                if strip.state.soloed { "solo" } else { "mix" },
                if strip.state.follow_video {
                    "afv"
                } else {
                    "always"
                },
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("[{strips}]")
}

fn format_overlays(overlays: &[OverlayChannelState; 8]) -> String {
    let channels = overlays
        .iter()
        .enumerate()
        .map(|(index, overlay)| {
            let outputs = overlay
                .included_outputs()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("+");
            format!(
                "{}:{}:{}:opacity={}:{}@{}:{}:{}:queue=[{}]:outputs=[{}]",
                index + 1,
                overlay
                    .source()
                    .map_or_else(|| "none".to_owned(), |source| source.to_string()),
                if overlay.is_active() { "on" } else { "off" },
                overlay.opacity(),
                match overlay.transition() {
                    OverlayTransitionKind::Cut => "cut",
                    OverlayTransitionKind::Fade => "fade",
                },
                overlay.duration_frames(),
                overlay_position_name(overlay.position()),
                overlay_border_name(overlay.border()),
                overlay
                    .queued_sources()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("+"),
                outputs,
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("[{channels}]")
}

fn format_stingers(stingers: &[StingerConfig]) -> String {
    let configured = stingers
        .iter()
        .map(|config| {
            format!(
                "{}:{}:{}:{}:{}:{}",
                config.slot.number(),
                config.media_input,
                if config.preload {
                    "preload"
                } else {
                    "deferred"
                },
                config.cut_point_frames,
                match config.audio_policy {
                    ModelStingerAudioPolicy::Muted => "muted",
                    ModelStingerAudioPolicy::StingerOnly => "stinger-only",
                    ModelStingerAudioPolicy::MixWithProgram => "mix-with-program",
                },
                match config.missing_media_fallback {
                    StingerMissingMediaFallback::Cut => "cut",
                    StingerMissingMediaFallback::Fade => "fade",
                    StingerMissingMediaFallback::KeepProgram => "keep-program",
                },
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("[{configured}]")
}

fn format_fade_to_black(state: EngineFadeToBlackState) -> String {
    format!(
        "{}@{}/{}",
        if state.active { "black" } else { "live" },
        state.position.numerator(),
        state.position.denominator(),
    )
}

fn format_t_bar(state: Option<TBarState>) -> String {
    state.map_or_else(
        || "inactive".to_owned(),
        |state| {
            format!(
                "{}:{}->{}@{}",
                match state.kind() {
                    TransitionKind::Fade => "fade",
                    TransitionKind::Wipe => "wipe",
                    TransitionKind::AlphaFade => "alpha_fade",
                    TransitionKind::Slide => "slide",
                    _ => unreachable!(
                        "engine manual transitions are Fade, Wipe, AlphaFade, or Slide"
                    ),
                },
                state.from(),
                state.to(),
                state.position().basis_points(),
            )
        },
    )
}

fn print_help() {
    println!(
        "\
FreeMix deterministic MVP

Usage:
  freemix-cli new <show.freemix> [--name <name>]
  freemix-cli input-add <show.freemix> <nonzero-input-id> <name>
  freemix-cli scene-input-add <show.freemix> <nonzero-input-id> <nonzero-scene-id> <name>
  freemix-cli scene-layer-add <show.freemix> <scene-id> <source-input-id> <z-order> <layer-name>
  freemix-cli scene-layer-remove <show.freemix> <scene-id> <zero-based-layer-index>
  freemix-cli input-remove <show.freemix> <input-id>
  freemix-cli input-duplicate <show.freemix> <source-input-id> <new-nonzero-input-id> <new-name>
  freemix-cli input-replace-simulated <show.freemix> <input-id>
  freemix-cli status <show.freemix>
  freemix-cli audio-strip <show.freemix> <input> <gain-millidb:-96000..=24000> <balance-bp:-10000..=10000> <muted:on|off> <soloed:on|off> <follow-video:on|off> <delay-samples:0..=48000>
  freemix-cli rename <show.freemix> <input> <name> [--key <key>] [--expect <revision>]
  freemix-cli input-reorder <show.freemix> <input> [<input>...] [--key <key>] [--expect <revision>]
  freemix-cli preview <show.freemix> <input> [--key <key>] [--expect <revision>]
  freemix-cli cut <show.freemix> [--key <key>] [--expect <revision>]
  freemix-cli fade <show.freemix> <frames> [--key <key>] [--expect <revision>]
  freemix-cli alpha-fade <show.freemix> <frames> [--key <key>] [--expect <revision>]
  freemix-cli slide <show.freemix> <frames> [--key <key>] [--expect <revision>]
  freemix-cli zoom <show.freemix> <frames> [--key <key>] [--expect <revision>]
  freemix-cli stinger <show.freemix> <slot:1..=8> <frames> [--key <key>] [--expect <revision>]
  freemix-cli stinger-configure <show.freemix> <slot:1..=8> <media-input> <true|false> <cut-point-frames> <muted|stinger-only|mix-with-program> <cut|fade|keep-program>
  freemix-cli stinger-remove <show.freemix> <slot:1..=8>
  freemix-cli overlay-take <show.freemix> <channel:1..=8> <source-input> [--key <key>] [--expect <revision>]
  freemix-cli overlay-update <show.freemix> <channel:1..=8> <source-input> [--key <key>] [--expect <revision>]
  freemix-cli overlay-off <show.freemix> <channel:1..=8> [--key <key>] [--expect <revision>]
  freemix-cli overlay-output <show.freemix> <channel:1..=8> <output> <true|false> [--key <key>] [--expect <revision>]
  freemix-cli overlay-transition <show.freemix> <channel:1..=8> <cut|fade> <frames> [--key <key>] [--expect <revision>]
  freemix-cli overlay-appearance <show.freemix> <channel:1..=8> <full-frame|top-left|top-right|bottom-left|bottom-right> <none|thin-white|thick-white> [--key <key>] [--expect <revision>]
  freemix-cli overlay-queue <show.freemix> <channel:1..=8> <source-input> [--key <key>] [--expect <revision>]
  freemix-cli overlay-next <show.freemix> <channel:1..=8> [--key <key>] [--expect <revision>]
  freemix-cli wipe <show.freemix> <frames> [--key <key>] [--expect <revision>]
  freemix-cli tbar-start <show.freemix> <fade|wipe|alpha-fade|slide> [--key <key>] [--expect <revision>]
  freemix-cli tbar-position <show.freemix> <basis-points:0..=10000> [--key <key>] [--expect <revision>]
  freemix-cli tbar-commit <show.freemix> [--key <key>] [--expect <revision>]
  freemix-cli tbar-cancel <show.freemix> [--key <key>] [--expect <revision>]
  freemix-cli ftb <show.freemix> <live|black> <frames> [--key <key>] [--expect <revision>]
  freemix-cli remote-status <127.0.0.1:port>
  freemix-cli remote-diagnostics <127.0.0.1:port>
  freemix-cli remote-audio-strip <127.0.0.1:port> <input> <gain-millidb:-96000..=24000> <balance-bp:-10000..=10000> <muted:on|off> <soloed:on|off> <follow-video:on|off> <delay-samples:0..=48000> [--key <key>] [--expect <revision>]
  freemix-cli remote-rename <127.0.0.1:port> <input> <name> [--key <key>] [--expect <revision>]
  freemix-cli remote-input-reorder <127.0.0.1:port> <input> [<input>...] [--key <key>] [--expect <revision>]
  freemix-cli remote-preview <127.0.0.1:port> <input> [--key <key>] [--expect <revision>]
  freemix-cli remote-cut <127.0.0.1:port> [--key <key>] [--expect <revision>]
  freemix-cli remote-fade <127.0.0.1:port> <frames> [--key <key>] [--expect <revision>]
  freemix-cli remote-alpha-fade <127.0.0.1:port> <frames> [--key <key>] [--expect <revision>]
  freemix-cli remote-slide <127.0.0.1:port> <frames> [--key <key>] [--expect <revision>]
  freemix-cli remote-zoom <127.0.0.1:port> <frames> [--key <key>] [--expect <revision>]
  freemix-cli remote-stinger <127.0.0.1:port> <slot:1..=8> <frames> [--key <key>] [--expect <revision>]
  freemix-cli remote-stinger-configure <127.0.0.1:port> <slot:1..=8> <media-input> <true|false> <cut-point-frames> <muted|stinger-only|mix-with-program> <cut|fade|keep-program> [--key <key>] [--expect <revision>]
  freemix-cli remote-stinger-remove <127.0.0.1:port> <slot:1..=8> [--key <key>] [--expect <revision>]
  freemix-cli remote-overlay-take <127.0.0.1:port> <channel:1..=8> <source-input> [--key <key>] [--expect <revision>]
  freemix-cli remote-overlay-update <127.0.0.1:port> <channel:1..=8> <source-input> [--key <key>] [--expect <revision>]
  freemix-cli remote-overlay-off <127.0.0.1:port> <channel:1..=8> [--key <key>] [--expect <revision>]
  freemix-cli remote-overlay-output <127.0.0.1:port> <channel:1..=8> <output> <true|false> [--key <key>] [--expect <revision>]
  freemix-cli remote-overlay-transition <127.0.0.1:port> <channel:1..=8> <cut|fade> <frames> [--key <key>] [--expect <revision>]
  freemix-cli remote-overlay-appearance <127.0.0.1:port> <channel:1..=8> <full-frame|top-left|top-right|bottom-left|bottom-right> <none|thin-white|thick-white> [--key <key>] [--expect <revision>]
  freemix-cli remote-overlay-queue <127.0.0.1:port> <channel:1..=8> <source-input> [--key <key>] [--expect <revision>]
  freemix-cli remote-overlay-next <127.0.0.1:port> <channel:1..=8> [--key <key>] [--expect <revision>]
  freemix-cli remote-wipe <127.0.0.1:port> <frames> [--key <key>] [--expect <revision>]
  freemix-cli remote-tbar-start <127.0.0.1:port> <fade|wipe|alpha-fade|slide> [--key <key>] [--expect <revision>]
  freemix-cli remote-tbar-position <127.0.0.1:port> <basis-points:0..=10000> [--key <key>] [--expect <revision>]
  freemix-cli remote-tbar-commit <127.0.0.1:port> [--key <key>] [--expect <revision>]
  freemix-cli remote-tbar-cancel <127.0.0.1:port> [--key <key>] [--expect <revision>]
  freemix-cli remote-ftb <127.0.0.1:port> <live|black> <frames> [--key <key>] [--expect <revision>]
  freemix-cli render <show.freemix> <output.ppm> [--width <px>] [--height <px>]
  freemix-cli demo <show.freemix> [output.ppm]"
    );
}

const fn engine_manual_kind(kind: ManualTransitionKind) -> EngineManualTransitionKind {
    match kind {
        ManualTransitionKind::Fade => EngineManualTransitionKind::Fade,
        ManualTransitionKind::Wipe => EngineManualTransitionKind::Wipe,
        ManualTransitionKind::AlphaFade => EngineManualTransitionKind::AlphaFade,
        ManualTransitionKind::Slide => EngineManualTransitionKind::Slide,
    }
}

const fn switcher_overlay_transition(kind: CliOverlayTransition) -> OverlayTransitionKind {
    match kind {
        CliOverlayTransition::Cut => OverlayTransitionKind::Cut,
        CliOverlayTransition::Fade => OverlayTransitionKind::Fade,
    }
}

const fn protocol_overlay_transition(
    kind: CliOverlayTransition,
) -> fm_protocol::OverlayTransitionKind {
    match kind {
        CliOverlayTransition::Cut => fm_protocol::OverlayTransitionKind::Cut,
        CliOverlayTransition::Fade => fm_protocol::OverlayTransitionKind::Fade,
    }
}

const fn switcher_overlay_position(kind: CliOverlayPosition) -> OverlayPositionPreset {
    match kind {
        CliOverlayPosition::FullFrame => OverlayPositionPreset::FullFrame,
        CliOverlayPosition::TopLeft => OverlayPositionPreset::TopLeft,
        CliOverlayPosition::TopRight => OverlayPositionPreset::TopRight,
        CliOverlayPosition::BottomLeft => OverlayPositionPreset::BottomLeft,
        CliOverlayPosition::BottomRight => OverlayPositionPreset::BottomRight,
    }
}

const fn switcher_overlay_border(kind: CliOverlayBorder) -> OverlayBorderPreset {
    match kind {
        CliOverlayBorder::None => OverlayBorderPreset::None,
        CliOverlayBorder::ThinWhite => OverlayBorderPreset::ThinWhite,
        CliOverlayBorder::ThickWhite => OverlayBorderPreset::ThickWhite,
    }
}

const fn protocol_overlay_position(kind: CliOverlayPosition) -> fm_protocol::OverlayPositionPreset {
    match kind {
        CliOverlayPosition::FullFrame => fm_protocol::OverlayPositionPreset::FullFrame,
        CliOverlayPosition::TopLeft => fm_protocol::OverlayPositionPreset::TopLeft,
        CliOverlayPosition::TopRight => fm_protocol::OverlayPositionPreset::TopRight,
        CliOverlayPosition::BottomLeft => fm_protocol::OverlayPositionPreset::BottomLeft,
        CliOverlayPosition::BottomRight => fm_protocol::OverlayPositionPreset::BottomRight,
    }
}

const fn protocol_overlay_border(kind: CliOverlayBorder) -> fm_protocol::OverlayBorderPreset {
    match kind {
        CliOverlayBorder::None => fm_protocol::OverlayBorderPreset::None,
        CliOverlayBorder::ThinWhite => fm_protocol::OverlayBorderPreset::ThinWhite,
        CliOverlayBorder::ThickWhite => fm_protocol::OverlayBorderPreset::ThickWhite,
    }
}

const fn protocol_stinger_audio_policy(
    policy: CliStingerAudioPolicy,
) -> fm_protocol::StingerAudioPolicy {
    match policy {
        CliStingerAudioPolicy::Muted => fm_protocol::StingerAudioPolicy::Muted,
        CliStingerAudioPolicy::StingerOnly => fm_protocol::StingerAudioPolicy::StingerOnly,
        CliStingerAudioPolicy::MixWithProgram => fm_protocol::StingerAudioPolicy::MixWithProgram,
    }
}

const fn protocol_stinger_fallback(
    fallback: CliStingerFallback,
) -> fm_protocol::StingerMissingMediaFallback {
    match fallback {
        CliStingerFallback::Cut => fm_protocol::StingerMissingMediaFallback::Cut,
        CliStingerFallback::Fade => fm_protocol::StingerMissingMediaFallback::Fade,
        CliStingerFallback::KeepProgram => fm_protocol::StingerMissingMediaFallback::KeepProgram,
    }
}

const fn overlay_position_name(position: OverlayPositionPreset) -> &'static str {
    match position {
        OverlayPositionPreset::FullFrame => "full-frame",
        OverlayPositionPreset::TopLeft => "top-left",
        OverlayPositionPreset::TopRight => "top-right",
        OverlayPositionPreset::BottomLeft => "bottom-left",
        OverlayPositionPreset::BottomRight => "bottom-right",
    }
}

const fn overlay_border_name(border: OverlayBorderPreset) -> &'static str {
    match border {
        OverlayBorderPreset::None => "none",
        OverlayBorderPreset::ThinWhite => "thin-white",
        OverlayBorderPreset::ThickWhite => "thick-white",
    }
}

fn engine_t_bar_command(action: TBarAction) -> EngineCommand {
    match action {
        TBarAction::Start(kind) => EngineCommand::StartManualTransition {
            kind: engine_manual_kind(kind),
        },
        TBarAction::SetPosition(position) => EngineCommand::SetManualTransitionPosition {
            position: EngineManualTransitionPosition::new(position)
                .expect("CLI parser bounds manual positions"),
        },
        TBarAction::Commit => EngineCommand::CommitManualTransition,
        TBarAction::Cancel => EngineCommand::CancelManualTransition,
    }
}

const fn model_stinger_audio_policy(policy: CliStingerAudioPolicy) -> ModelStingerAudioPolicy {
    match policy {
        CliStingerAudioPolicy::Muted => ModelStingerAudioPolicy::Muted,
        CliStingerAudioPolicy::StingerOnly => ModelStingerAudioPolicy::StingerOnly,
        CliStingerAudioPolicy::MixWithProgram => ModelStingerAudioPolicy::MixWithProgram,
    }
}

const fn model_stinger_fallback(fallback: CliStingerFallback) -> StingerMissingMediaFallback {
    match fallback {
        CliStingerFallback::Cut => StingerMissingMediaFallback::Cut,
        CliStingerFallback::Fade => StingerMissingMediaFallback::Fade,
        CliStingerFallback::KeepProgram => StingerMissingMediaFallback::KeepProgram,
    }
}

const fn protocol_manual_kind(kind: ManualTransitionKind) -> fm_protocol::ManualTransitionKind {
    match kind {
        ManualTransitionKind::Fade => fm_protocol::ManualTransitionKind::Fade,
        ManualTransitionKind::Wipe => fm_protocol::ManualTransitionKind::Wipe,
        ManualTransitionKind::AlphaFade => fm_protocol::ManualTransitionKind::AlphaFade,
        ManualTransitionKind::Slide => fm_protocol::ManualTransitionKind::Slide,
    }
}

fn protocol_t_bar_payload(action: TBarAction) -> fm_protocol::CommandPayload {
    match action {
        TBarAction::Start(kind) => fm_protocol::CommandPayload::StartManualTransition {
            kind: protocol_manual_kind(kind),
        },
        TBarAction::SetPosition(position) => {
            fm_protocol::CommandPayload::SetManualTransitionPosition {
                position: fm_protocol::ManualTransitionPosition::new(position)
                    .expect("CLI parser bounds manual positions"),
            }
        }
        TBarAction::Commit => fm_protocol::CommandPayload::CommitManualTransition,
        TBarAction::Cancel => fm_protocol::CommandPayload::CancelManualTransition,
    }
}

fn clock_domain() -> ClockDomainId {
    ClockDomainId::new(NonZeroU128::new(1).expect("one is nonzero"))
}

fn generate_project_id() -> AppResult<ProjectId> {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let sequence = u128::from(PROJECT_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed));
    let process = u128::from(std::process::id());
    let value = timestamp ^ (sequence << 64) ^ (process << 96);
    Ok(ProjectId::new(NonZeroU128::new(value | 1).expect(
        "setting the low bit makes the project ID nonzero",
    )))
}

fn input_id(value: u128) -> AppResult<InputId> {
    NonZeroU128::new(value)
        .map(InputId::new)
        .ok_or_else(|| AppFailure("input ID must be nonzero".into()).into())
}

fn scene_id(value: u128) -> AppResult<SceneId> {
    NonZeroU128::new(value)
        .map(SceneId::new)
        .ok_or_else(|| AppFailure("scene ID must be nonzero".into()).into())
}

fn output_id(value: u128) -> AppResult<OutputId> {
    NonZeroU128::new(value)
        .map(OutputId::new)
        .ok_or_else(|| AppFailure("output ID must be nonzero".into()).into())
}

fn required_routing(value: Option<InputId>, field: &'static str) -> AppResult<InputId> {
    value.ok_or_else(|| AppFailure(format!("project is missing {field} routing")).into())
}

fn required_main_mix(project: &Project) -> AppResult<MainMix> {
    project
        .main_mix()
        .ok_or_else(|| AppFailure("project is missing canonical main-mix routing".into()).into())
}

fn source_pattern(input: &Input) -> AppResult<SourcePattern> {
    let InputKind::Simulated(simulated) = &input.kind else {
        return Err(AppFailure(format!(
            "input {} ({:?}) is not simulated; render supports only simulated inputs",
            input.id, input.name
        ))
        .into());
    };
    Ok(match simulated.video {
        SimulatedVideo::Bars => SourcePattern::Bars,
        SimulatedVideo::Solid(color) => {
            SourcePattern::Solid(Rgba8::new(color.red, color.green, color.blue, color.alpha))
        }
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExecuteResult {
    replayed: bool,
    rejection: Option<AppFailure>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AppFailure(String);

impl core::fmt::Display for AppFailure {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Error for AppFailure {}

#[cfg(test)]
mod tests {
    use super::*;
    use fm_persistence::MutationBatch;

    #[test]
    fn ppm_publication_write_failure_keeps_previous_target() {
        use std::io::Write as _;

        let root = std::env::temp_dir().join(format!(
            "freemix-cli-ppm-publication-{}-{}",
            std::process::id(),
            PPM_OUTPUT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let output = root.join("output.ppm");
        let previous = b"complete PPM";
        fs::write(&output, previous).unwrap();

        let error = write_ppm_atomic(&output, |file| {
            file.write_all(b"partial PPM")?;
            Err(io::Error::other("controlled write failure"))
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(fs::read(&output).unwrap(), previous);
        assert!(fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stable_rejection_codes_round_trip_and_unknown_codes_fail() {
        let stable = [
            (RejectionCode::PermissionDenied, "permission_denied"),
            (RejectionCode::DeadlineExceeded, "deadline_exceeded"),
            (RejectionCode::RevisionConflict, "revision_conflict"),
            (RejectionCode::InvalidCommand, "invalid_command"),
            (RejectionCode::NotFound, "not_found"),
            (RejectionCode::Conflict, "conflict"),
            (RejectionCode::Unavailable, "unavailable"),
            (RejectionCode::ResourceExhausted, "resource_exhausted"),
            (RejectionCode::Internal, "internal"),
        ];
        for (code, name) in stable {
            assert_eq!(persisted_rejection_code(code).unwrap(), name);
            assert_eq!(runtime_rejection_code(name).unwrap(), code);
        }

        let error = runtime_rejection_code("future_code").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unknown rejection code `future_code`")
        );
    }

    #[test]
    fn local_load_rejects_unapplied_batch_without_mutating_bundle() {
        let root = std::env::temp_dir().join(format!(
            "freemix-cli-journal-{}-{}",
            std::process::id(),
            PROJECT_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("show.freemix");
        save_engine(&path, &default_project("Journal".into()).unwrap()).unwrap();
        let store = ProjectStore::new(&path).unwrap();
        store
            .append_batch(&MutationBatch::new(1, 0, 1, b"unapplied".to_vec()))
            .unwrap();
        let manifest = fs::read(path.join("project.json")).unwrap();
        let record = store.journal_path().join("00000000000000000001.batch");
        let journal = fs::read(&record).unwrap();

        let error = load_stored_project(&path).unwrap_err();
        assert_eq!(
            error.to_string(),
            "project has unapplied journal batches that freemix-cli cannot safely interpret"
        );
        assert_eq!(fs::read(path.join("project.json")).unwrap(), manifest);
        assert_eq!(fs::read(record).unwrap(), journal);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn local_load_allows_torn_final_record_and_leaves_it_in_place() {
        let root = std::env::temp_dir().join(format!(
            "freemix-cli-journal-{}-{}",
            std::process::id(),
            PROJECT_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("show.freemix");
        save_engine(&path, &default_project("Journal".into()).unwrap()).unwrap();
        let store = ProjectStore::new(&path).unwrap();
        store
            .append_batch(&MutationBatch::new(1, 0, 1, b"torn".to_vec()))
            .unwrap();
        let record = store.journal_path().join("00000000000000000001.batch");
        fs::OpenOptions::new()
            .write(true)
            .open(&record)
            .unwrap()
            .set_len(12)
            .unwrap();

        assert!(load_stored_project(&path).is_ok());
        assert!(record.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restore_stinger_only_marks_requested_preload_ready() {
        let program = InputId::new(NonZeroU128::new(1).unwrap());
        let preview = InputId::new(NonZeroU128::new(2).unwrap());
        let inputs = vec![program, preview];
        let named_inputs = inputs
            .iter()
            .copied()
            .map(|input| (input, format!("Input {input}")))
            .collect();
        let mut show = ShowState::new("restore", named_inputs, program, preview).unwrap();
        let mut realized = SwitcherState::new(inputs, program, preview).unwrap();
        let slot = fm_model::StingerSlotNumber::new(1).unwrap();
        let config = |preload| {
            StingerConfig::new(
                slot,
                preview,
                preload,
                1,
                ModelStingerAudioPolicy::Muted,
                StingerMissingMediaFallback::KeepProgram,
            )
        };

        restore_stinger(&mut show, &mut realized, config(false)).unwrap();
        let switcher_slot = StingerSlotId::new(1).unwrap();
        assert_eq!(
            show.desired_switcher()
                .stinger(switcher_slot)
                .preload_state(),
            fm_switcher::StingerPreloadState::NotRequested
        );
        assert_eq!(
            realized.stinger(switcher_slot).preload_state(),
            fm_switcher::StingerPreloadState::NotRequested
        );

        restore_stinger(&mut show, &mut realized, config(true)).unwrap();
        assert_eq!(
            show.desired_switcher()
                .stinger(switcher_slot)
                .preload_state(),
            fm_switcher::StingerPreloadState::Ready
        );
        assert_eq!(
            realized.stinger(switcher_slot).preload_state(),
            fm_switcher::StingerPreloadState::Ready
        );
    }
}
