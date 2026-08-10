use fm_client::{ConnectionState, Session};
use fm_protocol::{CommandPayload, WireInputId};
use fm_types::InputId;
use fm_ui_model::InputAudioStripStatus;

use crate::TransitionControlState;

/// Transport-free semantic controls for an exact per-input Master strip.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AudioStripControls;

impl AudioStripControls {
    pub const MIN_GAIN_MILLIDB: i32 = -96_000;
    pub const MAX_GAIN_MILLIDB: i32 = 24_000;
    pub const MIN_BALANCE_BASIS_POINTS: i32 = -10_000;
    pub const MAX_BALANCE_BASIS_POINTS: i32 = 10_000;
    pub const MAX_DELAY_SAMPLES: u32 = 48_000;

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
                    .any(|permission| permission == "control_audio")
            })
        {
            TransitionControlState::Enabled
        } else {
            TransitionControlState::Disabled
        }
    }

    #[must_use]
    pub fn accessibility_label(self, input: InputId) -> String {
        format!("Input {input} audio strip")
    }

    #[must_use]
    pub fn current_state(
        self,
        input: InputId,
        strips: &[InputAudioStripStatus],
    ) -> Option<&InputAudioStripStatus> {
        strips.iter().find(|status| status.input == input)
    }

    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn command_payload(
        self,
        input: InputId,
        gain_millidb: i32,
        balance_basis_points: i32,
        muted: bool,
        soloed: bool,
        follow_video: bool,
        delay_samples: u32,
        connection_state: &ConnectionState,
        session: Option<&Session>,
    ) -> Option<CommandPayload> {
        ((Self::MIN_GAIN_MILLIDB..=Self::MAX_GAIN_MILLIDB).contains(&gain_millidb)
            && (Self::MIN_BALANCE_BASIS_POINTS..=Self::MAX_BALANCE_BASIS_POINTS)
                .contains(&balance_basis_points)
            && delay_samples <= Self::MAX_DELAY_SAMPLES
            && self.control_state(connection_state, session).is_enabled())
        .then_some(CommandPayload::SetInputAudioStrip {
            input: WireInputId::from_domain(input),
            gain_millidb,
            balance_basis_points,
            muted,
            soloed,
            follow_video,
            delay_samples,
        })
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroU128;

    use fm_protocol::{CURRENT_PROTOCOL_VERSION, ProtocolVersion, Role, ServerIdentity};

    use super::*;

    fn input(value: u128) -> InputId {
        InputId::new(NonZeroU128::new(value).unwrap())
    }

    fn session(permissions: &[&str]) -> Session {
        Session {
            protocol: CURRENT_PROTOCOL_VERSION,
            granted_role: Role::Audio,
            permissions: permissions
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            server: ServerIdentity {
                engine_id: "engine".into(),
                project_id: "1".into(),
                state_epoch: 1,
                log_id: "log".into(),
            },
            capabilities_digest: "audio-strip".into(),
        }
    }

    #[test]
    fn audio_strip_requires_current_session_permission_and_bounds() {
        assert_eq!(CURRENT_PROTOCOL_VERSION, ProtocolVersion::new(2, 9));
        let controls = AudioStripControls;
        let ready = session(&["control_audio"]);
        assert_eq!(
            controls.command_payload(
                input(7),
                -6_000,
                2_500,
                true,
                true,
                false,
                2_400,
                &ConnectionState::Ready,
                Some(&ready)
            ),
            Some(CommandPayload::SetInputAudioStrip {
                input: WireInputId::from_domain(input(7)),
                gain_millidb: -6_000,
                balance_basis_points: 2_500,
                muted: true,
                soloed: true,
                follow_video: false,
                delay_samples: 2_400,
            })
        );
        assert_eq!(
            controls.command_payload(
                input(7),
                24_001,
                0,
                false,
                false,
                true,
                0,
                &ConnectionState::Ready,
                Some(&ready)
            ),
            None
        );
        assert_eq!(
            controls.command_payload(
                input(7),
                0,
                0,
                false,
                false,
                true,
                48_001,
                &ConnectionState::Ready,
                Some(&ready)
            ),
            None
        );
        assert_eq!(
            controls.command_payload(
                input(7),
                0,
                10_001,
                false,
                false,
                true,
                0,
                &ConnectionState::Ready,
                Some(&ready)
            ),
            None
        );
    }
}
