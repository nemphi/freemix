use std::{
    error::Error,
    fs::File,
    num::NonZeroU128,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use fm_clock::{ClockDomainId, ClockTime};
use fm_command::{
    AcceptedReceipt, CommandEnvelope, CommandId, CommandReceipt, EventSequence, IdempotencyKey,
    RejectedReceipt, Rejection, RejectionCode, Revision, RuntimeGeneration, StateEpoch,
};
use fm_engine::{
    Engine, EngineAcceptance, EngineCommand, EngineFadeToBlackState, EngineManualTransitionKind,
    EngineManualTransitionPosition, EngineRestoreState, ShowState,
};
use fm_model::{
    Input, InputKind, MainMix, Project, ProjectSettings, SimulatedAudio, SimulatedInput,
    SimulatedVideo, SolidColor,
};
use fm_persistence::{
    FadeToBlackState as PersistedFadeToBlackState, IdempotencyReceipt,
    ManualTransitionKind as PersistedManualTransitionKind,
    ManualTransitionState as PersistedManualTransitionState, ProjectPosition, ProjectStore,
    ProjectValidationError, ReceiptOutcome, RuntimeFadeToBlack, RuntimeManualTransitions,
    RuntimeRouting, StoreError, StoredProject,
};
use fm_sim::{Rgba8, SimulatedPipeline, SimulatedSource, SourcePattern};
use fm_switcher::{SwitcherState, TBarPosition, TBarState, TransitionKind};
use fm_types::{
    AudioFormat, ChannelLayout, ColorMetadata, FrameRate, InputId, PixelFormat, ProjectId,
    SampleFormat, SampleRate, ScanMode, VideoDimensions, VideoFormat,
};
use fm_video::write_ppm;

use crate::{
    args::{Command, ManualTransitionKind, TBarAction},
    remote,
};

type AppResult<T> = Result<T, Box<dyn Error>>;
static IMPLICIT_KEY_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static PROJECT_ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
        Command::Status { path } => print_status(&load_engine(&path)?),
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
    let inputs = project.inputs().iter().map(|input| input.id).collect();
    let show = ShowState::new(
        project.name(),
        inputs,
        main_mix.desired_program,
        main_mix.desired_preview,
    )?;
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
        None => generate_implicit_key(command, expected_revision)?,
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
    command: EngineCommand,
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
    let stored = StoredProject::from_project_with_runtime_state(
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

fn load_engine(path: &Path) -> AppResult<ProjectEngine> {
    let stored = load_stored_project(path)?;
    let project = stored.project().clone();
    let inputs = project
        .inputs()
        .iter()
        .map(|input| input.id)
        .collect::<Vec<_>>();
    let main_mix = required_main_mix(&project)?;
    let routing = stored.runtime_routing();
    let realized_program = required_routing(routing.realized_program_id, "realized program")?;
    let realized_preview = required_routing(routing.realized_preview_id, "realized preview")?;
    let mut show = ShowState::new(
        project.name(),
        inputs.clone(),
        main_mix.desired_program,
        main_mix.desired_preview,
    )?;
    let mut realized = SwitcherState::new(inputs, realized_program, realized_preview)?;
    let manual = stored.runtime_manual_transitions();
    if let Some(state) = manual.desired {
        show.restore_manual_transition(restored_t_bar(state)?)?;
    }
    if let Some(state) = manual.realized {
        realized.restore_t_bar(restored_t_bar(state)?)?;
    }
    let fade_to_black = stored.runtime_fade_to_black();
    show.restore_fade_to_black(fade_to_black.desired.target_active);
    let _ = realized.set_fade_to_black(fade_to_black.realized.target_active);
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
        _ => unreachable!("engine manual transitions are Fade, Wipe, or AlphaFade"),
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
    match store.load() {
        Ok(project) => Ok(project),
        Err(StoreError::Validation(ProjectValidationError::UnsupportedSchema {
            found: 2, ..
        })) => {
            store.migrate_v2()?;
            Ok(store.load()?)
        }
        Err(StoreError::Validation(ProjectValidationError::UnsupportedSchema {
            found: 3, ..
        })) => {
            store.migrate_v3()?;
            Ok(store.load()?)
        }
        Err(StoreError::Validation(ProjectValidationError::UnsupportedSchema {
            found: 4, ..
        })) => {
            store.migrate_v4()?;
            Ok(store.load()?)
        }
        Err(StoreError::Validation(ProjectValidationError::UnsupportedSchema {
            found: 5, ..
        })) => {
            store.migrate_v5()?;
            Ok(store.load()?)
        }
        Err(StoreError::Validation(ProjectValidationError::UnsupportedSchema {
            found: 6, ..
        })) => {
            store.migrate_v6()?;
            Ok(store.load()?)
        }
        Err(StoreError::Validation(ProjectValidationError::UnsupportedSchema {
            found: 7, ..
        })) => {
            store.migrate_v7()?;
            Ok(store.load()?)
        }
        Err(StoreError::Validation(ProjectValidationError::UnsupportedSchema {
            found: 8, ..
        })) => {
            store.migrate_v8()?;
            Ok(store.load()?)
        }
        Err(error) => Err(error.into()),
    }
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
    let mut file = File::create(output)?;
    write_ppm(&frame, &mut file)?;
    file.sync_all()?;
    println!(
        "rendered {}x{} Program input {} to {}",
        width,
        height,
        engine.realized_switcher().program(),
        output.display()
    );
    Ok(())
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
        "project_id={} show={:?} revision={} frame={} Program(desired={}, realized={}) Preview(desired={}, realized={}) TBar(desired={}, realized={}) FTB(desired={}, realized={})",
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
    );
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
                    _ => unreachable!("engine manual transitions are fade, wipe, or AlphaFade"),
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
  freemix-cli status <show.freemix>
  freemix-cli preview <show.freemix> <input> [--key <key>] [--expect <revision>]
  freemix-cli cut <show.freemix> [--key <key>] [--expect <revision>]
  freemix-cli fade <show.freemix> <frames> [--key <key>] [--expect <revision>]
  freemix-cli alpha-fade <show.freemix> <frames> [--key <key>] [--expect <revision>]
  freemix-cli slide <show.freemix> <frames> [--key <key>] [--expect <revision>]
  freemix-cli zoom <show.freemix> <frames> [--key <key>] [--expect <revision>]
  freemix-cli wipe <show.freemix> <frames> [--key <key>] [--expect <revision>]
  freemix-cli tbar-start <show.freemix> <fade|wipe|alpha-fade> [--key <key>] [--expect <revision>]
  freemix-cli tbar-position <show.freemix> <basis-points:0..=10000> [--key <key>] [--expect <revision>]
  freemix-cli tbar-commit <show.freemix> [--key <key>] [--expect <revision>]
  freemix-cli tbar-cancel <show.freemix> [--key <key>] [--expect <revision>]
  freemix-cli ftb <show.freemix> <live|black> <frames> [--key <key>] [--expect <revision>]
  freemix-cli remote-status <127.0.0.1:port>
  freemix-cli remote-preview <127.0.0.1:port> <input> [--key <key>] [--expect <revision>]
  freemix-cli remote-cut <127.0.0.1:port> [--key <key>] [--expect <revision>]
  freemix-cli remote-fade <127.0.0.1:port> <frames> [--key <key>] [--expect <revision>]
  freemix-cli remote-alpha-fade <127.0.0.1:port> <frames> [--key <key>] [--expect <revision>]
  freemix-cli remote-slide <127.0.0.1:port> <frames> [--key <key>] [--expect <revision>]
  freemix-cli remote-zoom <127.0.0.1:port> <frames> [--key <key>] [--expect <revision>]
  freemix-cli remote-wipe <127.0.0.1:port> <frames> [--key <key>] [--expect <revision>]
  freemix-cli remote-tbar-start <127.0.0.1:port> <fade|wipe|alpha-fade> [--key <key>] [--expect <revision>]
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

const fn protocol_manual_kind(kind: ManualTransitionKind) -> fm_protocol::ManualTransitionKind {
    match kind {
        ManualTransitionKind::Fade => fm_protocol::ManualTransitionKind::Fade,
        ManualTransitionKind::Wipe => fm_protocol::ManualTransitionKind::Wipe,
        ManualTransitionKind::AlphaFade => fm_protocol::ManualTransitionKind::AlphaFade,
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
}
