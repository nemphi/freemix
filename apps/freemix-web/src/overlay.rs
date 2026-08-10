use fm_client::{ConnectionState, Session};
use fm_protocol::{
    CommandPayload, OverlayBorderPreset, OverlayPositionPreset, OverlayTransitionKind, WireInputId,
    WireOutputId, WireOverlayChannelId,
};
use fm_types::{InputId, OutputId};
use fm_ui_model::OverlayStatus;

use crate::TransitionControlState;

/// One of the eight downstream overlay controls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OverlayControl {
    channel: WireOverlayChannelId,
}

impl OverlayControl {
    #[must_use]
    pub const fn new(channel: u8) -> Option<Self> {
        match WireOverlayChannelId::new(channel) {
            Some(channel) => Some(Self { channel }),
            None => None,
        }
    }

    #[must_use]
    pub const fn channel(self) -> u8 {
        self.channel.number()
    }

    #[must_use]
    pub fn accessibility_label(self) -> String {
        format!("Overlay channel {}", self.channel)
    }
}

/// Transport-free overlay operator model.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OverlayControls;

impl OverlayControls {
    #[must_use]
    pub fn control_state(
        self,
        connection_state: &ConnectionState,
        session: Option<&Session>,
    ) -> TransitionControlState {
        if matches!(connection_state, ConnectionState::Ready)
            && session.is_some_and(|session| {
                session
                    .permissions
                    .iter()
                    .any(|permission| permission == "transition")
            })
        {
            TransitionControlState::Enabled
        } else {
            TransitionControlState::Disabled
        }
    }

    #[must_use]
    pub fn take(
        self,
        control: OverlayControl,
        source: InputId,
        connection_state: &ConnectionState,
        session: Option<&Session>,
    ) -> Option<CommandPayload> {
        self.control_state(connection_state, session)
            .is_enabled()
            .then_some(CommandPayload::TakeOverlay {
                channel: control.channel,
                source: WireInputId::from_domain(source),
            })
    }

    #[must_use]
    pub fn update(
        self,
        control: OverlayControl,
        source: InputId,
        connection_state: &ConnectionState,
        session: Option<&Session>,
    ) -> Option<CommandPayload> {
        self.control_state(connection_state, session)
            .is_enabled()
            .then_some(CommandPayload::UpdateOverlay {
                channel: control.channel,
                source: WireInputId::from_domain(source),
            })
    }

    #[must_use]
    pub fn off(
        self,
        control: OverlayControl,
        connection_state: &ConnectionState,
        session: Option<&Session>,
    ) -> Option<CommandPayload> {
        self.control_state(connection_state, session)
            .is_enabled()
            .then_some(CommandPayload::OverlayOff {
                channel: control.channel,
            })
    }

    #[must_use]
    pub fn set_output_inclusion(
        self,
        control: OverlayControl,
        output: OutputId,
        included: bool,
        connection_state: &ConnectionState,
        session: Option<&Session>,
    ) -> Option<CommandPayload> {
        self.control_state(connection_state, session)
            .is_enabled()
            .then_some(CommandPayload::SetOverlayOutputInclusion {
                channel: control.channel,
                output: WireOutputId::from_domain(output),
                included,
            })
    }

    #[must_use]
    pub fn configure_transition(
        self,
        control: OverlayControl,
        transition: OverlayTransitionKind,
        duration_frames: u32,
        connection_state: &ConnectionState,
        session: Option<&Session>,
    ) -> Option<CommandPayload> {
        self.control_state(connection_state, session)
            .is_enabled()
            .then_some(CommandPayload::ConfigureOverlayTransition {
                channel: control.channel,
                transition,
                duration_frames,
            })
    }

    #[must_use]
    pub fn configure_appearance(
        self,
        control: OverlayControl,
        position: OverlayPositionPreset,
        border: OverlayBorderPreset,
        connection_state: &ConnectionState,
        session: Option<&Session>,
    ) -> Option<CommandPayload> {
        self.control_state(connection_state, session)
            .is_enabled()
            .then_some(CommandPayload::ConfigureOverlayAppearance {
                channel: control.channel,
                position,
                border,
            })
    }

    #[must_use]
    pub fn queue(
        self,
        control: OverlayControl,
        source: InputId,
        connection_state: &ConnectionState,
        session: Option<&Session>,
    ) -> Option<CommandPayload> {
        self.control_state(connection_state, session)
            .is_enabled()
            .then_some(CommandPayload::QueueOverlay {
                channel: control.channel,
                source: WireInputId::from_domain(source),
            })
    }

    #[must_use]
    pub fn take_next(
        self,
        control: OverlayControl,
        connection_state: &ConnectionState,
        session: Option<&Session>,
    ) -> Option<CommandPayload> {
        self.control_state(connection_state, session)
            .is_enabled()
            .then_some(CommandPayload::TakeNextOverlay {
                channel: control.channel,
            })
    }

    #[must_use]
    pub fn status(
        self,
        control: OverlayControl,
        overlays: &[OverlayStatus],
    ) -> Option<&OverlayStatus> {
        overlays
            .iter()
            .find(|status| status.channel == control.channel())
    }
}

#[cfg(test)]
mod tests {
    use fm_protocol::{CURRENT_PROTOCOL_VERSION, Role, ServerIdentity};

    use super::*;

    fn session(permissions: &[&str]) -> Session {
        Session {
            protocol: CURRENT_PROTOCOL_VERSION,
            granted_role: Role::Operator,
            permissions: permissions
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            server: ServerIdentity {
                engine_id: "engine-overlay".into(),
                project_id: "project-overlay".into(),
                state_epoch: 1,
                log_id: "log-overlay".into(),
            },
            capabilities_digest: "overlay".into(),
        }
    }

    #[test]
    fn appearance_command_is_permission_and_connection_gated() {
        let controls = OverlayControls;
        let control = OverlayControl::new(8).unwrap();
        let operator = session(&["transition"]);
        let expected = CommandPayload::ConfigureOverlayAppearance {
            channel: WireOverlayChannelId::new(8).unwrap(),
            position: OverlayPositionPreset::BottomLeft,
            border: OverlayBorderPreset::ThinWhite,
        };
        assert_eq!(
            controls.configure_appearance(
                control,
                OverlayPositionPreset::BottomLeft,
                OverlayBorderPreset::ThinWhite,
                &ConnectionState::Ready,
                Some(&operator),
            ),
            Some(expected)
        );
        assert_eq!(
            controls.configure_appearance(
                control,
                OverlayPositionPreset::BottomLeft,
                OverlayBorderPreset::ThinWhite,
                &ConnectionState::Connecting,
                Some(&operator),
            ),
            None
        );
        let source = InputId::new(core::num::NonZeroU128::new(2).unwrap());
        assert_eq!(
            controls.queue(control, source, &ConnectionState::Ready, Some(&operator)),
            Some(CommandPayload::QueueOverlay {
                channel: WireOverlayChannelId::new(8).unwrap(),
                source: WireInputId::from_domain(source),
            })
        );
        assert_eq!(
            controls.take_next(control, &ConnectionState::Ready, Some(&operator)),
            Some(CommandPayload::TakeNextOverlay {
                channel: WireOverlayChannelId::new(8).unwrap(),
            })
        );
    }
}
