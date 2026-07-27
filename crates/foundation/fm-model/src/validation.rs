use fm_types::{BusId, InputId, OutputId, SceneId};

use crate::{
    InputKind, Project, ProjectSettings, RestartPolicy, SimulatedAudio, SourceRef,
    cycles::{mark_audio_input_cycles, mark_bus_cycles, mark_scene_cycles},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntityRef {
    MainMix,
    Input(InputId),
    Scene(SceneId),
    AudioBus(BusId),
    Output(OutputId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationError {
    pub entity: Option<EntityRef>,
    pub field: &'static str,
    pub kind: ValidationErrorKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationErrorKind {
    Empty,
    DuplicateId,
    DuplicateName,
    MissingReference(EntityRef),
    SelfReference,
    Cycle,
    MalformedCapabilityKey,
    DuplicateReference,
    OutOfRange,
    FormatMismatch,
}

pub(crate) fn validate_project(project: &Project) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    require_name(&mut errors, None, "name", project.name());
    validate_settings(project, &mut errors);
    validate_unique_entities(project, &mut errors);
    validate_inputs(project, &mut errors);
    validate_main_mix(project, &mut errors);
    validate_scenes(project, &mut errors);
    validate_buses(project, &mut errors);
    validate_outputs(project, &mut errors);
    mark_scene_cycles(project, &mut errors);
    mark_audio_input_cycles(project, &mut errors);
    mark_bus_cycles(project, &mut errors);
    errors
}

fn validate_settings(project: &Project, errors: &mut Vec<ValidationError>) {
    let settings = project.settings();
    let frame_rate = settings.frame_rate;
    let numerator = u64::from(frame_rate.numerator());
    let denominator = u64::from(frame_rate.denominator());
    if numerator < u64::from(ProjectSettings::MIN_FRAME_RATE_FPS) * denominator
        || numerator > u64::from(ProjectSettings::MAX_FRAME_RATE_FPS) * denominator
    {
        errors.push(ValidationError {
            entity: None,
            field: "settings.frame_rate",
            kind: ValidationErrorKind::OutOfRange,
        });
    }
    if settings.video.frame_rate != frame_rate {
        errors.push(ValidationError {
            entity: None,
            field: "settings.video.frame_rate",
            kind: ValidationErrorKind::FormatMismatch,
        });
    }
    if settings.video.dimensions.width() > ProjectSettings::MAX_VIDEO_WIDTH
        || settings.video.dimensions.height() > ProjectSettings::MAX_VIDEO_HEIGHT
    {
        errors.push(ValidationError {
            entity: None,
            field: "settings.video.dimensions",
            kind: ValidationErrorKind::OutOfRange,
        });
    }

    let sample_rate = settings.audio.sample_rate.hertz();
    if !(ProjectSettings::MIN_AUDIO_SAMPLE_RATE_HZ..=ProjectSettings::MAX_AUDIO_SAMPLE_RATE_HZ)
        .contains(&sample_rate)
    {
        errors.push(ValidationError {
            entity: None,
            field: "settings.audio.sample_rate",
            kind: ValidationErrorKind::OutOfRange,
        });
    }
    let channels = settings.audio.channels.channels();
    if channels.len() > ProjectSettings::MAX_AUDIO_CHANNELS {
        errors.push(ValidationError {
            entity: None,
            field: "settings.audio.channels",
            kind: ValidationErrorKind::OutOfRange,
        });
    }
    if channels
        .iter()
        .enumerate()
        .any(|(index, channel)| channels[..index].contains(channel))
    {
        errors.push(ValidationError {
            entity: None,
            field: "settings.audio.channels",
            kind: ValidationErrorKind::DuplicateReference,
        });
    }

    if let RestartPolicy::OnFailure { max_attempts } = project.restart_policy()
        && !(1..=RestartPolicy::MAX_RESTART_ATTEMPTS).contains(&max_attempts)
    {
        errors.push(ValidationError {
            entity: None,
            field: "restart_policy.max_attempts",
            kind: ValidationErrorKind::OutOfRange,
        });
    }
}

fn validate_unique_entities(project: &Project, errors: &mut Vec<ValidationError>) {
    duplicates(
        errors,
        project
            .inputs()
            .iter()
            .map(|input| (input.id, input.name.as_str(), EntityRef::Input(input.id))),
    );
    duplicates(
        errors,
        project
            .scenes()
            .iter()
            .map(|scene| (scene.id, scene.name.as_str(), EntityRef::Scene(scene.id))),
    );
    duplicates(
        errors,
        project
            .audio_buses()
            .iter()
            .map(|bus| (bus.id, bus.name.as_str(), EntityRef::AudioBus(bus.id))),
    );
    duplicates(
        errors,
        project.outputs().iter().map(|output| {
            (
                output.id,
                output.name.as_str(),
                EntityRef::Output(output.id),
            )
        }),
    );
}

fn validate_inputs(project: &Project, errors: &mut Vec<ValidationError>) {
    for input in project.inputs() {
        let entity = Some(EntityRef::Input(input.id));
        require_name(errors, entity, "name", &input.name);
        match &input.kind {
            InputKind::Color => {}
            InputKind::Media { asset_uri } => {
                require_name(errors, entity, "asset_uri", asset_uri);
            }
            InputKind::Device { stable_key } => {
                require_name(errors, entity, "stable_key", stable_key);
            }
            InputKind::Network { endpoint } => {
                require_name(errors, entity, "endpoint", endpoint);
            }
            InputKind::Scene {
                scene_id,
                audio_source,
            } => {
                if !project.scenes().iter().any(|scene| scene.id == *scene_id) {
                    errors.push(ValidationError {
                        entity,
                        field: "kind.scene.scene_id",
                        kind: ValidationErrorKind::MissingReference(EntityRef::Scene(*scene_id)),
                    });
                }
                if let Some(audio_source) = audio_source {
                    if *audio_source == input.id {
                        errors.push(ValidationError {
                            entity,
                            field: "kind.scene.audio_source",
                            kind: ValidationErrorKind::SelfReference,
                        });
                    } else if !project
                        .inputs()
                        .iter()
                        .any(|other| other.id == *audio_source)
                    {
                        errors.push(ValidationError {
                            entity,
                            field: "kind.scene.audio_source",
                            kind: ValidationErrorKind::MissingReference(EntityRef::Input(
                                *audio_source,
                            )),
                        });
                    }
                }
            }
            InputKind::Simulated(simulated) => {
                if let SimulatedAudio::Sine { frequency_hz } = simulated.audio {
                    let nyquist_hz = project.settings().audio.sample_rate.hertz() / 2;
                    if !(SimulatedAudio::MIN_SINE_FREQUENCY_HZ
                        ..=SimulatedAudio::MAX_SINE_FREQUENCY_HZ.min(nyquist_hz))
                        .contains(&frequency_hz)
                    {
                        errors.push(ValidationError {
                            entity,
                            field: "kind.simulated.audio.frequency_hz",
                            kind: ValidationErrorKind::OutOfRange,
                        });
                    }
                }
            }
        }
        validate_capabilities(
            errors,
            entity,
            input.required_capabilities.iter().map(String::as_str),
        );
    }
}

fn validate_main_mix(project: &Project, errors: &mut Vec<ValidationError>) {
    let Some(main_mix) = project.main_mix() else {
        return;
    };
    let entity = Some(EntityRef::MainMix);
    for (field, id) in [
        ("main_mix.desired_program", main_mix.desired_program),
        ("main_mix.desired_preview", main_mix.desired_preview),
    ] {
        if !project.inputs().iter().any(|input| input.id == id) {
            errors.push(ValidationError {
                entity,
                field,
                kind: ValidationErrorKind::MissingReference(EntityRef::Input(id)),
            });
        }
    }
    if main_mix.desired_program == main_mix.desired_preview {
        errors.push(ValidationError {
            entity,
            field: "main_mix.desired_preview",
            kind: ValidationErrorKind::DuplicateReference,
        });
    }
}

fn validate_scenes(project: &Project, errors: &mut Vec<ValidationError>) {
    for scene in project.scenes() {
        let entity = Some(EntityRef::Scene(scene.id));
        require_name(errors, entity, "name", &scene.name);
        if !scene.background.is_premultiplied() {
            errors.push(ValidationError {
                entity,
                field: "background",
                kind: ValidationErrorKind::OutOfRange,
            });
        }
        for layer in &scene.layers {
            require_name(errors, entity, "layers.name", &layer.name);
            if layer.geometry.width == 0
                || layer.geometry.height == 0
                || layer.geometry.width > ProjectSettings::MAX_VIDEO_WIDTH
                || layer.geometry.height > ProjectSettings::MAX_VIDEO_HEIGHT
            {
                errors.push(ValidationError {
                    entity,
                    field: "layers.geometry",
                    kind: ValidationErrorKind::OutOfRange,
                });
            }
            if layer.crop.is_some_and(|crop| {
                crop.width == 0
                    || crop.height == 0
                    || crop
                        .x
                        .checked_add(crop.width)
                        .is_none_or(|right| right > project.settings().video.dimensions.width())
                    || crop
                        .y
                        .checked_add(crop.height)
                        .is_none_or(|bottom| bottom > project.settings().video.dimensions.height())
            }) {
                errors.push(ValidationError {
                    entity,
                    field: "layers.crop",
                    kind: ValidationErrorKind::OutOfRange,
                });
            }
            match layer.source {
                SourceRef::Input(id) if !project.inputs().iter().any(|input| input.id == id) => {
                    errors.push(ValidationError {
                        entity,
                        field: "layers.source",
                        kind: ValidationErrorKind::MissingReference(EntityRef::Input(id)),
                    });
                }
                SourceRef::Scene(id) if id == scene.id => errors.push(ValidationError {
                    entity,
                    field: "layers.source",
                    kind: ValidationErrorKind::SelfReference,
                }),
                SourceRef::Scene(id) if !project.scenes().iter().any(|other| other.id == id) => {
                    errors.push(ValidationError {
                        entity,
                        field: "layers.source",
                        kind: ValidationErrorKind::MissingReference(EntityRef::Scene(id)),
                    });
                }
                SourceRef::Input(_) | SourceRef::Scene(_) => {}
            }
        }
    }
}

fn validate_buses(project: &Project, errors: &mut Vec<ValidationError>) {
    for bus in project.audio_buses() {
        let entity = Some(EntityRef::AudioBus(bus.id));
        require_name(errors, entity, "name", &bus.name);
        for (index, send) in bus.sends.iter().enumerate() {
            if send.destination == bus.id {
                errors.push(ValidationError {
                    entity,
                    field: "sends.destination",
                    kind: ValidationErrorKind::SelfReference,
                });
            } else if !project
                .audio_buses()
                .iter()
                .any(|other| other.id == send.destination)
            {
                errors.push(ValidationError {
                    entity,
                    field: "sends.destination",
                    kind: ValidationErrorKind::MissingReference(EntityRef::AudioBus(
                        send.destination,
                    )),
                });
            } else if bus.sends[..index]
                .iter()
                .any(|prior| prior.destination == send.destination)
            {
                errors.push(ValidationError {
                    entity,
                    field: "sends.destination",
                    kind: ValidationErrorKind::DuplicateReference,
                });
            }
        }
    }
}

fn validate_outputs(project: &Project, errors: &mut Vec<ValidationError>) {
    for output in project.outputs() {
        let entity = Some(EntityRef::Output(output.id));
        require_name(errors, entity, "name", &output.name);
        if !project
            .scenes()
            .iter()
            .any(|scene| scene.id == output.video_source)
        {
            errors.push(ValidationError {
                entity,
                field: "video_source",
                kind: ValidationErrorKind::MissingReference(EntityRef::Scene(output.video_source)),
            });
        }
        if !project
            .audio_buses()
            .iter()
            .any(|bus| bus.id == output.audio_source)
        {
            errors.push(ValidationError {
                entity,
                field: "audio_source",
                kind: ValidationErrorKind::MissingReference(EntityRef::AudioBus(
                    output.audio_source,
                )),
            });
        }
        validate_capabilities(
            errors,
            entity,
            output.required_capabilities.iter().map(String::as_str),
        );
    }
}

fn require_name(
    errors: &mut Vec<ValidationError>,
    entity: Option<EntityRef>,
    field: &'static str,
    value: &str,
) {
    if value.trim().is_empty() {
        errors.push(ValidationError {
            entity,
            field,
            kind: ValidationErrorKind::Empty,
        });
    }
}

fn duplicates<'a, I, Id>(errors: &mut Vec<ValidationError>, items: I)
where
    I: IntoIterator<Item = (Id, &'a str, EntityRef)>,
    Id: Copy + Eq,
{
    let items: Vec<_> = items.into_iter().collect();
    for (index, (id, name, entity)) in items.iter().copied().enumerate() {
        if items[..index].iter().any(|(prior, _, _)| *prior == id) {
            errors.push(ValidationError {
                entity: Some(entity),
                field: "id",
                kind: ValidationErrorKind::DuplicateId,
            });
        }
        if !name.trim().is_empty()
            && items[..index]
                .iter()
                .any(|(_, prior, _)| prior.eq_ignore_ascii_case(name))
        {
            errors.push(ValidationError {
                entity: Some(entity),
                field: "name",
                kind: ValidationErrorKind::DuplicateName,
            });
        }
    }
}

fn validate_capabilities<'a>(
    errors: &mut Vec<ValidationError>,
    entity: Option<EntityRef>,
    keys: impl IntoIterator<Item = &'a str>,
) {
    for key in keys {
        let valid = key.split('.').count() >= 2
            && key.split('.').all(|segment| {
                !segment.is_empty()
                    && segment.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'
                    })
            });
        if !valid {
            errors.push(ValidationError {
                entity,
                field: "required_capabilities",
                kind: ValidationErrorKind::MalformedCapabilityKey,
            });
        }
    }
}
