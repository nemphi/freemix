use fm_types::{BusId, SceneId};

use crate::{EntityRef, Project, SourceRef, ValidationError, ValidationErrorKind};

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
                let SourceRef::Scene(next) = layer.source else {
                    return false;
                };
                next == target || reaches_scene(project, next, target, visited)
            })
        });
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
