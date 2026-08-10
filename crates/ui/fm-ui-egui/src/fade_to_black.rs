use egui::{Button, Color32, DragValue, Frame, Margin, RichText, Stroke, Ui, Vec2};
use fm_protocol::{FadeToBlackPosition, FadeToBlackState};

use crate::{
    AMBER, GRAPHITE_RAISED, MUTED, PROGRAM, StudioConnectionStatus, StudioIntent, StudioUiState,
};

/// Pure Fade-to-Black control availability derived from replicated state and session gates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FadeToBlackAvailability {
    pub to_black: bool,
    pub to_live: bool,
    pub duration: bool,
}

/// Session and replicated-state gates for Fade-to-Black controls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FadeToBlackGate {
    pub connection_status: StudioConnectionStatus,
    pub has_view: bool,
    pub can_transition: bool,
}

impl FadeToBlackGate {
    #[must_use]
    pub const fn from_state(state: &StudioUiState) -> Self {
        Self {
            connection_status: state.connection_status,
            has_view: state.view.is_some(),
            can_transition: state.can_transition,
        }
    }
}

/// Computes Fade-to-Black availability without drawing or dispatching intents.
#[must_use]
pub const fn fade_to_black_availability(
    gate: FadeToBlackGate,
    desired: Option<FadeToBlackState>,
) -> FadeToBlackAvailability {
    let base = gate.connection_status.controls_enabled() && gate.has_view && gate.can_transition;
    match desired {
        Some(desired) if base => FadeToBlackAvailability {
            to_black: !desired.target_active,
            to_live: desired.target_active,
            duration: true,
        },
        Some(_) | None => FadeToBlackAvailability {
            to_black: false,
            to_live: false,
            duration: false,
        },
    }
}

pub(super) fn draw_fade_to_black(
    ui: &mut Ui,
    duration_frames: &mut u32,
    state: &StudioUiState,
    intents: &mut Vec<StudioIntent>,
) {
    let desired = state
        .view
        .as_ref()
        .map(|view| view.switcher.desired_fade_to_black);
    let realized = state
        .view
        .as_ref()
        .map(|view| view.switcher.realized_fade_to_black);
    let availability = fade_to_black_availability(FadeToBlackGate::from_state(state), desired);

    Frame::new()
        .fill(GRAPHITE_RAISED)
        .stroke(Stroke::new(1.0, Color32::from_rgb(77, 39, 42)))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new("FADE TO BLACK")
                        .small()
                        .strong()
                        .color(PROGRAM),
                );
                ui.label(
                    RichText::new(format!("DESIRED {}", fade_to_black_label(desired))).small(),
                );
                ui.label(
                    RichText::new(format!("REALIZED {}", fade_to_black_label(realized)))
                        .small()
                        .color(MUTED),
                );
            });
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(
                        availability.to_black,
                        Button::new(RichText::new("FADE TO BLACK").strong())
                            .fill(Color32::from_rgb(92, 28, 32))
                            .min_size(Vec2::new(138.0, 32.0)),
                    )
                    .on_hover_text("Fade the realized Program video and audio to black")
                    .clicked()
                {
                    intents.push(StudioIntent::FadeToBlack {
                        active: true,
                        duration_frames: *duration_frames,
                    });
                }
                if ui
                    .add_enabled(
                        availability.to_live,
                        Button::new(RichText::new("FADE TO LIVE").strong())
                            .fill(Color32::from_rgb(98, 66, 17))
                            .min_size(Vec2::new(138.0, 32.0)),
                    )
                    .on_hover_text("Reverse Fade-to-Black and restore live Program")
                    .clicked()
                {
                    intents.push(StudioIntent::FadeToBlack {
                        active: false,
                        duration_frames: *duration_frames,
                    });
                }
                ui.label(RichText::new("DURATION").small().color(AMBER));
                ui.add_enabled(
                    availability.duration,
                    DragValue::new(duration_frames)
                        .range(1..=3_600)
                        .suffix(" FRAMES")
                        .speed(1.0),
                );
            });
        });
}

fn fade_to_black_label(state: Option<FadeToBlackState>) -> String {
    state.map_or_else(
        || "UNAVAILABLE".to_owned(),
        |state| {
            format!(
                "TARGET {} @ {}/{}",
                if state.target_active { "BLACK" } else { "LIVE" },
                state.position.numerator(),
                FadeToBlackPosition::DENOMINATOR,
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const AVAILABLE: FadeToBlackGate = FadeToBlackGate {
        connection_status: StudioConnectionStatus::Ready,
        has_view: true,
        can_transition: true,
    };

    fn state(target_active: bool, position: u16) -> FadeToBlackState {
        FadeToBlackState {
            target_active,
            position: FadeToBlackPosition::new(position),
        }
    }

    #[test]
    fn availability_enables_only_the_opposite_target_for_reversal() {
        assert_eq!(
            fade_to_black_availability(AVAILABLE, Some(state(false, 0))),
            FadeToBlackAvailability {
                to_black: true,
                to_live: false,
                duration: true,
            }
        );
        assert_eq!(
            fade_to_black_availability(AVAILABLE, Some(state(true, 30_000))),
            FadeToBlackAvailability {
                to_black: false,
                to_live: true,
                duration: true,
            }
        );
    }

    #[test]
    fn every_session_and_state_gate_disables_controls() {
        for gate in [
            FadeToBlackGate {
                connection_status: StudioConnectionStatus::Connecting,
                ..AVAILABLE
            },
            FadeToBlackGate {
                has_view: false,
                ..AVAILABLE
            },
            FadeToBlackGate {
                can_transition: false,
                ..AVAILABLE
            },
        ] {
            assert_eq!(
                fade_to_black_availability(gate, Some(state(false, 0))),
                FadeToBlackAvailability {
                    to_black: false,
                    to_live: false,
                    duration: false,
                }
            );
        }
        assert_eq!(
            fade_to_black_availability(AVAILABLE, None),
            FadeToBlackAvailability {
                to_black: false,
                to_live: false,
                duration: false,
            }
        );
    }

    #[test]
    fn exact_desired_and_realized_labels_are_stable() {
        assert_eq!(
            fade_to_black_label(Some(state(true, 40_000))),
            "TARGET BLACK @ 40000/65535"
        );
        assert_eq!(
            fade_to_black_label(Some(state(false, 20_000))),
            "TARGET LIVE @ 20000/65535"
        );
        assert_eq!(fade_to_black_label(None), "UNAVAILABLE");
    }
}
