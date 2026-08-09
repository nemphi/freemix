use fm_client::{ConnectionState, Session};
use fm_protocol::{CommandPayload, FadeToBlackPosition, FadeToBlackState};
use fm_ui_model::SwitcherState;

use crate::TransitionControlState;

/// One semantic Fade-to-Black input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FadeToBlackControl {
    ToBlack,
    ToLive,
    Duration,
}

impl FadeToBlackControl {
    /// Stable accessible name for the input.
    #[must_use]
    pub const fn accessibility_label(self) -> &'static str {
        match self {
            Self::ToBlack => "Fade Program to black",
            Self::ToLive => "Fade Program from black",
            Self::Duration => "Fade-to-Black duration",
        }
    }

    const fn target_active(self) -> Option<bool> {
        match self {
            Self::ToBlack => Some(true),
            Self::ToLive => Some(false),
            Self::Duration => None,
        }
    }
}

/// One authoritative desired or realized FTB projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FadeToBlackPresentation {
    pub target_active: bool,
    pub position: FadeToBlackPosition,
}

/// Presence of one authoritative FTB projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FadeToBlackProjection {
    Missing,
    Present(FadeToBlackPresentation),
}

impl FadeToBlackProjection {
    const fn from_state(state: Option<FadeToBlackState>) -> Self {
        match state {
            Some(state) => Self::Present(FadeToBlackPresentation {
                target_active: state.target_active,
                position: state.position,
            }),
            None => Self::Missing,
        }
    }
}

/// Transport-free semantic model for FTB controls and exact replicated state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FadeToBlackModel {
    duration_frames: u32,
    desired: FadeToBlackProjection,
    realized: FadeToBlackProjection,
}

impl FadeToBlackModel {
    pub const MIN_DURATION_FRAMES: u32 = 1;
    pub const MAX_DURATION_FRAMES: u32 = 3_600;
    pub const DEFAULT_DURATION_FRAMES: u32 = 30;

    /// Creates a model from independently optional authoritative projections.
    #[must_use]
    pub const fn new(
        desired: Option<FadeToBlackState>,
        realized: Option<FadeToBlackState>,
    ) -> Self {
        Self {
            duration_frames: Self::DEFAULT_DURATION_FRAMES,
            desired: FadeToBlackProjection::from_state(desired),
            realized: FadeToBlackProjection::from_state(realized),
        }
    }

    /// Extracts exact desired and realized projections from replicated state.
    #[must_use]
    pub fn from_switcher(switcher: Option<&SwitcherState>) -> Self {
        switcher.map_or_else(Self::default, |switcher| {
            Self::new(
                Some(switcher.desired_fade_to_black),
                Some(switcher.realized_fade_to_black),
            )
        })
    }

    #[must_use]
    pub const fn duration_frames(&self) -> u32 {
        self.duration_frames
    }

    /// Sets and clamps the duration to the engine FTB contract.
    pub fn set_duration_frames(&mut self, duration_frames: u32) {
        self.duration_frames =
            duration_frames.clamp(Self::MIN_DURATION_FRAMES, Self::MAX_DURATION_FRAMES);
    }

    #[must_use]
    pub const fn desired(&self) -> FadeToBlackProjection {
        self.desired
    }

    #[must_use]
    pub const fn realized(&self) -> FadeToBlackProjection {
        self.realized
    }

    /// Derives visibility and interactivity from protocol, permission, and authoritative state.
    #[must_use]
    pub fn control_state(
        &self,
        control: FadeToBlackControl,
        connection_state: &ConnectionState,
        session: Option<&Session>,
    ) -> TransitionControlState {
        if !matches!(connection_state, ConnectionState::Ready)
            || !session.is_some_and(session_can_transition)
        {
            return TransitionControlState::Disabled;
        }
        let (FadeToBlackProjection::Present(desired), FadeToBlackProjection::Present(_)) =
            (self.desired, self.realized)
        else {
            return TransitionControlState::Disabled;
        };

        match control.target_active() {
            Some(target_active) if target_active == desired.target_active => {
                TransitionControlState::Disabled
            }
            Some(_) | None => TransitionControlState::Enabled,
        }
    }

    /// Builds a protocol command only for an enabled target control.
    #[must_use]
    pub fn command_payload(
        &self,
        control: FadeToBlackControl,
        connection_state: &ConnectionState,
        session: Option<&Session>,
    ) -> Option<CommandPayload> {
        if !self
            .control_state(control, connection_state, session)
            .is_enabled()
        {
            return None;
        }
        control
            .target_active()
            .map(|active| CommandPayload::FadeToBlack {
                active,
                duration_frames: self.duration_frames,
            })
    }
}

impl Default for FadeToBlackModel {
    fn default() -> Self {
        Self::new(None, None)
    }
}

fn session_can_transition(session: &Session) -> bool {
    session
        .permissions
        .iter()
        .any(|permission| permission == "transition")
}

#[cfg(test)]
mod tests {
    use fm_protocol::{CURRENT_PROTOCOL_VERSION, ProtocolVersion, Role, ServerIdentity};

    use super::*;

    fn session(protocol: ProtocolVersion, permissions: &[&str]) -> Session {
        Session {
            protocol,
            granted_role: Role::Operator,
            permissions: permissions
                .iter()
                .map(|permission| (*permission).to_owned())
                .collect(),
            server: ServerIdentity {
                engine_id: "engine-web-ftb".to_owned(),
                project_id: "project-web-ftb".to_owned(),
                state_epoch: 1,
                log_id: "log-web-ftb".to_owned(),
            },
            capabilities_digest: "sha256:web-ftb".to_owned(),
        }
    }

    fn state(target_active: bool, numerator: u16) -> FadeToBlackState {
        FadeToBlackState {
            target_active,
            position: FadeToBlackPosition::new(numerator),
        }
    }

    #[test]
    fn exact_projections_and_accessible_labels_are_stable() {
        let model = FadeToBlackModel::new(Some(state(true, 40_000)), Some(state(true, 20_000)));
        assert_eq!(
            model.desired(),
            FadeToBlackProjection::Present(FadeToBlackPresentation {
                target_active: true,
                position: FadeToBlackPosition::new(40_000),
            })
        );
        assert_eq!(
            model.realized(),
            FadeToBlackProjection::Present(FadeToBlackPresentation {
                target_active: true,
                position: FadeToBlackPosition::new(20_000),
            })
        );
        assert_eq!(
            FadeToBlackControl::ToBlack.accessibility_label(),
            "Fade Program to black"
        );
        assert_eq!(
            FadeToBlackControl::ToLive.accessibility_label(),
            "Fade Program from black"
        );
        assert_eq!(
            FadeToBlackControl::Duration.accessibility_label(),
            "Fade-to-Black duration"
        );
    }

    #[test]
    fn opposite_target_remains_enabled_for_reversal() {
        let model = FadeToBlackModel::new(Some(state(true, 40_000)), Some(state(true, 20_000)));
        let session = session(CURRENT_PROTOCOL_VERSION, &["transition"]);

        assert_eq!(
            model.control_state(
                FadeToBlackControl::ToBlack,
                &ConnectionState::Ready,
                Some(&session),
            ),
            TransitionControlState::Disabled
        );
        assert_eq!(
            model.command_payload(
                FadeToBlackControl::ToLive,
                &ConnectionState::Ready,
                Some(&session),
            ),
            Some(CommandPayload::FadeToBlack {
                active: false,
                duration_frames: FadeToBlackModel::DEFAULT_DURATION_FRAMES,
            })
        );
    }

    #[test]
    fn protocol_permission_readiness_and_complete_state_gate_commands() {
        let model = FadeToBlackModel::new(Some(state(false, 0)), Some(state(false, 0)));
        let current = session(CURRENT_PROTOCOL_VERSION, &["transition"]);
        let viewer = session(CURRENT_PROTOCOL_VERSION, &["view_status"]);

        assert_eq!(
            model.control_state(
                FadeToBlackControl::ToBlack,
                &ConnectionState::Ready,
                Some(&viewer),
            ),
            TransitionControlState::Disabled
        );
        assert_eq!(
            model.control_state(
                FadeToBlackControl::ToBlack,
                &ConnectionState::Connecting,
                Some(&current),
            ),
            TransitionControlState::Disabled
        );
        assert_eq!(
            FadeToBlackModel::default().control_state(
                FadeToBlackControl::ToBlack,
                &ConnectionState::Ready,
                Some(&current),
            ),
            TransitionControlState::Disabled
        );
    }

    #[test]
    fn duration_is_bounded_and_does_not_emit_a_command() {
        let mut model = FadeToBlackModel::new(Some(state(false, 0)), Some(state(false, 0)));
        let session = session(CURRENT_PROTOCOL_VERSION, &["transition"]);
        model.set_duration_frames(0);
        assert_eq!(
            model.duration_frames(),
            FadeToBlackModel::MIN_DURATION_FRAMES
        );
        model.set_duration_frames(u32::MAX);
        assert_eq!(
            model.duration_frames(),
            FadeToBlackModel::MAX_DURATION_FRAMES
        );
        assert_eq!(
            model.command_payload(
                FadeToBlackControl::Duration,
                &ConnectionState::Ready,
                Some(&session),
            ),
            None
        );
    }
}
