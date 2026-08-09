use fm_client::{ConnectionState, Session};
use fm_protocol::{CommandPayload, ManualTransitionKind, ManualTransitionPosition};
use fm_types::InputId;
use fm_ui_model::{ManualTransitionStatus, SwitcherState};

use crate::TransitionControlState;

/// One semantic manual-transition input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManualTransitionControl {
    StartFade,
    StartWipe,
    StartAlphaFade,
    Position(ManualTransitionPosition),
    Commit,
    Cancel,
}

impl ManualTransitionControl {
    /// Stable accessible name for the input.
    #[must_use]
    pub const fn accessibility_label(self) -> &'static str {
        match self {
            Self::StartFade => "Start manual Fade transition",
            Self::StartWipe => "Start manual Wipe transition",
            Self::StartAlphaFade => "Start manual AlphaFade transition",
            Self::Position(_) => "Manual transition position in basis points",
            Self::Commit => "Commit manual transition",
            Self::Cancel => "Cancel manual transition",
        }
    }

    const fn requires_active_transition(self) -> bool {
        matches!(self, Self::Position(_) | Self::Commit | Self::Cancel)
    }
}

/// Direction represented by one authoritative interval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManualTransitionMotion {
    Held,
    Forward,
    Reverse,
}

/// Presentation fields for one active authoritative projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManualTransitionPresentation {
    pub kind: ManualTransitionKind,
    pub from: InputId,
    pub to: InputId,
    pub interval_start: ManualTransitionPosition,
    pub position: ManualTransitionPosition,
}

impl ManualTransitionPresentation {
    /// Compares the interval start with the current position without smoothing or local state.
    #[must_use]
    pub const fn motion(self) -> ManualTransitionMotion {
        let interval_start = self.interval_start.basis_points();
        let position = self.position.basis_points();
        if interval_start < position {
            ManualTransitionMotion::Forward
        } else if interval_start > position {
            ManualTransitionMotion::Reverse
        } else {
            ManualTransitionMotion::Held
        }
    }
}

/// Presence and activity of one authoritative manual-transition projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManualTransitionProjection {
    Missing,
    Inactive,
    Active(ManualTransitionPresentation),
}

impl ManualTransitionProjection {
    const fn from_status(status: Option<ManualTransitionStatus>) -> Self {
        match status {
            None => Self::Missing,
            Some(ManualTransitionStatus::Inactive) => Self::Inactive,
            Some(ManualTransitionStatus::Active(active)) => {
                Self::Active(ManualTransitionPresentation {
                    kind: active.kind,
                    from: active.from,
                    to: active.to,
                    interval_start: active.interval_start,
                    position: active.position,
                })
            }
        }
    }
}

/// Transport-free semantic model for manual Fade/Wipe/AlphaFade controls and presentation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManualTransitionModel {
    desired: ManualTransitionProjection,
    realized: ManualTransitionProjection,
}

impl ManualTransitionModel {
    /// Creates a model from independently optional authoritative projections.
    #[must_use]
    pub const fn new(
        desired: Option<ManualTransitionStatus>,
        realized: Option<ManualTransitionStatus>,
    ) -> Self {
        Self {
            desired: ManualTransitionProjection::from_status(desired),
            realized: ManualTransitionProjection::from_status(realized),
        }
    }

    /// Extracts the distinct desired and realized projections from replicated client state.
    #[must_use]
    pub fn from_switcher(switcher: Option<&SwitcherState>) -> Self {
        switcher.map_or_else(Self::default, |switcher| {
            Self::new(
                Some(switcher.desired_manual_transition),
                Some(switcher.realized_manual_transition),
            )
        })
    }

    #[must_use]
    pub const fn desired(self) -> ManualTransitionProjection {
        self.desired
    }

    #[must_use]
    pub const fn realized(self) -> ManualTransitionProjection {
        self.realized
    }

    /// Derives visibility and interactivity from the negotiated session and replicated state.
    #[must_use]
    pub fn control_state(
        self,
        control: ManualTransitionControl,
        connection_state: &ConnectionState,
        session: Option<&Session>,
    ) -> TransitionControlState {
        let Some(session) = session else {
            return TransitionControlState::Hidden;
        };
        if !matches!(connection_state, ConnectionState::Ready)
            || !session_can_transition(session)
            || matches!(self.desired, ManualTransitionProjection::Missing)
            || matches!(self.realized, ManualTransitionProjection::Missing)
        {
            return TransitionControlState::Disabled;
        }

        let valid = if control.requires_active_transition() {
            matches!(self.desired, ManualTransitionProjection::Active(_))
        } else {
            matches!(
                (self.desired, self.realized),
                (
                    ManualTransitionProjection::Inactive,
                    ManualTransitionProjection::Inactive
                )
            )
        };

        if valid {
            TransitionControlState::Enabled
        } else {
            TransitionControlState::Disabled
        }
    }

    /// Builds only an existing protocol payload and only for an enabled control.
    #[must_use]
    pub fn command_payload(
        self,
        control: ManualTransitionControl,
        connection_state: &ConnectionState,
        session: Option<&Session>,
    ) -> Option<CommandPayload> {
        if !self
            .control_state(control, connection_state, session)
            .is_enabled()
        {
            return None;
        }

        Some(match control {
            ManualTransitionControl::StartFade => CommandPayload::StartManualTransition {
                kind: ManualTransitionKind::Fade,
            },
            ManualTransitionControl::StartWipe => CommandPayload::StartManualTransition {
                kind: ManualTransitionKind::Wipe,
            },
            ManualTransitionControl::StartAlphaFade => CommandPayload::StartManualTransition {
                kind: ManualTransitionKind::AlphaFade,
            },
            ManualTransitionControl::Position(position) => {
                CommandPayload::SetManualTransitionPosition { position }
            }
            ManualTransitionControl::Commit => CommandPayload::CommitManualTransition,
            ManualTransitionControl::Cancel => CommandPayload::CancelManualTransition,
        })
    }
}

impl Default for ManualTransitionModel {
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
