use fm_types::{BusId, InputId, SceneId};

use crate::{EntityRef, InputKind, Project, SourceRef, ValidationError, ValidationErrorKind};

pub(crate) fn mark_scene_cycles(project: &Project, errors: &mut Vec<ValidationError>) {
    for scene in project.scenes() {
        if reaches_scene(project, scene.id, scene.id, &mut Vec::new()) {
            errors.push(ValidationError {
                entity: Some(EntityRef::Scene(scene.id)),
                field: "layers.source",
                kind: ValidationErrorKind::Cycle,
            });
        }
    }
}

fn reaches_scene(
    project: &Project,
    current: SceneId,
    target: SceneId,
    visited: &mut Vec<SceneId>,
) -> bool {
    if visited.contains(&current) {
        return false;
    }
    visited.push(current);
    let reaches = project
        .scenes()
        .iter()
        .find(|scene| scene.id == current)
        .is_some_and(|scene| {
            scene.layers.iter().any(|layer| {
                let next = match layer.source {
                    SourceRef::Scene(next) => Some(next),
                    SourceRef::Input(input) => project
                        .inputs()
                        .iter()
                        .find(|candidate| candidate.id == input)
                        .and_then(|candidate| match candidate.kind {
                            InputKind::Scene { scene_id, .. } => Some(scene_id),
                            _ => None,
                        }),
                };
                next.is_some_and(|next| {
                    next == target || reaches_scene(project, next, target, visited)
                })
            })
        });
    visited.pop();
    reaches
}

pub(crate) fn mark_audio_input_cycles(project: &Project, errors: &mut Vec<ValidationError>) {
    for input in project.inputs() {
        if reaches_audio_input(project, input.id, input.id, &mut Vec::new()) {
            errors.push(ValidationError {
                entity: Some(EntityRef::Input(input.id)),
                field: "kind.scene.audio_source",
                kind: ValidationErrorKind::Cycle,
            });
        }
    }
}

fn reaches_audio_input(
    project: &Project,
    current: InputId,
    target: InputId,
    visited: &mut Vec<InputId>,
) -> bool {
    if visited.contains(&current) {
        return false;
    }
    visited.push(current);
    let reaches = project
        .inputs()
        .iter()
        .find(|input| input.id == current)
        .and_then(|input| match input.kind {
            InputKind::Scene { audio_source, .. } => audio_source,
            _ => None,
        })
        .is_some_and(|next| next == target || reaches_audio_input(project, next, target, visited));
    visited.pop();
    reaches
}

pub(crate) fn mark_bus_cycles(project: &Project, errors: &mut Vec<ValidationError>) {
    for bus in project.audio_buses() {
        if reaches_bus(project, bus.id, bus.id, &mut Vec::new()) {
            errors.push(ValidationError {
                entity: Some(EntityRef::AudioBus(bus.id)),
                field: "sends.destination",
                kind: ValidationErrorKind::Cycle,
            });
        }
    }
}

fn reaches_bus(project: &Project, current: BusId, target: BusId, visited: &mut Vec<BusId>) -> bool {
    if visited.contains(&current) {
        return false;
    }
    visited.push(current);
    let reaches = project
        .audio_buses()
        .iter()
        .find(|bus| bus.id == current)
        .is_some_and(|bus| {
            bus.sends.iter().any(|send| {
                send.destination == target
                    || reaches_bus(project, send.destination, target, visited)
            })
        });
    visited.pop();
    reaches
}
