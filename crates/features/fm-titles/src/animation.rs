use crate::Bounds;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnimatedProperty {
    X,
    Y,
    Width,
    Height,
    Opacity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Interpolation {
    Hold,
    Linear,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Keyframe {
    pub at_ms: u64,
    pub value: i64,
    /// Controls interpolation from this keyframe to the next one.
    pub interpolation: Interpolation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnimationTrack {
    pub property: AnimatedProperty,
    pub keyframes: Vec<Keyframe>,
}

impl AnimationTrack {
    /// Evaluates with integer arithmetic. Linear division truncates toward zero.
    #[must_use]
    pub fn value_at(&self, time_ms: u64) -> Option<i64> {
        let first = *self.keyframes.first()?;
        if time_ms <= first.at_ms {
            return Some(first.value);
        }

        for pair in self.keyframes.windows(2) {
            let from = pair[0];
            let to = pair[1];
            if time_ms < to.at_ms {
                return Some(match from.interpolation {
                    Interpolation::Hold => from.value,
                    Interpolation::Linear => interpolate(from, to, time_ms),
                });
            }
        }
        self.keyframes.last().map(|keyframe| keyframe.value)
    }
}

fn interpolate(from: Keyframe, to: Keyframe, time_ms: u64) -> i64 {
    let elapsed = i128::from(time_ms - from.at_ms);
    let duration = i128::from(to.at_ms - from.at_ms);
    let start = i128::from(from.value);
    let difference = i128::from(to.value) - start;
    i64::try_from(start + difference * elapsed / duration).unwrap_or_else(|_| {
        if difference.is_negative() {
            i64::MIN
        } else {
            i64::MAX
        }
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EvaluatedElement {
    pub bounds: Bounds,
    pub opacity: u8,
}

pub(crate) fn evaluate_tracks(
    bounds: Bounds,
    opacity: u8,
    tracks: &[AnimationTrack],
    time_ms: u64,
) -> EvaluatedElement {
    let mut evaluated = EvaluatedElement { bounds, opacity };
    for track in tracks {
        let Some(value) = track.value_at(time_ms) else {
            continue;
        };
        match track.property {
            AnimatedProperty::X => {
                evaluated.bounds.x = i32::try_from(value).unwrap_or(if value.is_negative() {
                    i32::MIN
                } else {
                    i32::MAX
                });
            }
            AnimatedProperty::Y => {
                evaluated.bounds.y = i32::try_from(value).unwrap_or(if value.is_negative() {
                    i32::MIN
                } else {
                    i32::MAX
                });
            }
            AnimatedProperty::Width => {
                evaluated.bounds.width =
                    u32::try_from(value).unwrap_or(if value.is_negative() { 0 } else { u32::MAX });
            }
            AnimatedProperty::Height => {
                evaluated.bounds.height =
                    u32::try_from(value).unwrap_or(if value.is_negative() { 0 } else { u32::MAX });
            }
            AnimatedProperty::Opacity => {
                evaluated.opacity =
                    u8::try_from(value).unwrap_or(if value.is_negative() { 0 } else { u8::MAX });
            }
        }
    }
    evaluated
}
