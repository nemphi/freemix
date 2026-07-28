use fm_client::{ConnectionState, Session};
use fm_protocol::{CommandPayload, STINGER_PROTOCOL_VERSION, StingerReadiness, WireStingerSlotId};
use fm_ui_model::StingerStatus;

use crate::TransitionControlState;

/// One of the eight semantic Stinger fire controls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StingerControl {
    slot: WireStingerSlotId,
}

impl StingerControl {
    /// Creates a control for an operator-facing slot in `1..=8`.
    #[must_use]
    pub const fn new(slot: u8) -> Option<Self> {
        match WireStingerSlotId::new(slot) {
            Some(slot) => Some(Self { slot }),
            None => None,
        }
    }

    #[must_use]
    pub const fn slot(self) -> u8 {
        self.slot.number()
    }

    #[must_use]
    pub fn accessibility_label(self) -> String {
        format!("Fire Stinger slot {}", self.slot)
    }
}

/// Transport-free state shared by the eight Stinger controls.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StingerControls {
    duration_frames: u32,
}

impl StingerControls {
    pub const MIN_DURATION_FRAMES: u32 = 1;
    pub const MAX_DURATION_FRAMES: u32 = 3_600;
    pub const DEFAULT_DURATION_FRAMES: u32 = 30;

    #[must_use]
    pub const fn duration_frames(&self) -> u32 {
        self.duration_frames
    }

    pub fn set_duration_frames(&mut self, duration_frames: u32) {
        self.duration_frames =
            duration_frames.clamp(Self::MIN_DURATION_FRAMES, Self::MAX_DURATION_FRAMES);
    }

    /// Derives visibility and interactivity without assuming slot configuration.
    #[must_use]
    pub fn control_state(
        &self,
        control: StingerControl,
        connection_state: &ConnectionState,
        session: Option<&Session>,
        stingers: &[StingerStatus],
    ) -> TransitionControlState {
        if !session.is_some_and(session_supports_stinger) {
            return TransitionControlState::Hidden;
        }
        if matches!(connection_state, ConnectionState::Ready)
            && session.is_some_and(session_can_transition)
            && stingers.iter().any(|status| {
                status.slot == control.slot() && status.readiness == StingerReadiness::Ready
            })
        {
            TransitionControlState::Enabled
        } else {
            TransitionControlState::Disabled
        }
    }

    /// Builds the exact protocol command for an enabled numbered control.
    #[must_use]
    pub fn command_payload(
        &self,
        control: StingerControl,
        connection_state: &ConnectionState,
        session: Option<&Session>,
        stingers: &[StingerStatus],
    ) -> Option<CommandPayload> {
        self.control_state(control, connection_state, session, stingers)
            .is_enabled()
            .then_some(CommandPayload::Stinger {
                slot: control.slot,
                duration_frames: self.duration_frames,
            })
    }
}

impl Default for StingerControls {
    fn default() -> Self {
        Self {
            duration_frames: Self::DEFAULT_DURATION_FRAMES,
        }
    }
}

fn session_can_transition(session: &Session) -> bool {
    session
        .permissions
        .iter()
        .any(|permission| permission == "transition")
}

fn session_supports_stinger(session: &Session) -> bool {
    session.protocol.major == STINGER_PROTOCOL_VERSION.major
        && session.protocol.minor >= STINGER_PROTOCOL_VERSION.minor
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroU128;

    use fm_protocol::{
        ProtocolVersion, Role, ServerIdentity, StingerAudioPolicy, StingerMissingMediaFallback,
    };
    use fm_types::InputId;

    use super::*;

    fn session(protocol: ProtocolVersion, permissions: &[&str]) -> Session {
        Session {
            protocol,
            granted_role: Role::Operator,
            permissions: permissions
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            server: ServerIdentity {
                engine_id: "engine-web-stinger".into(),
                project_id: "project-web-stinger".into(),
                state_epoch: 1,
                log_id: "log-web-stinger".into(),
            },
            capabilities_digest: "web-stinger".into(),
        }
    }

    fn ready_stinger(slot: u8) -> StingerStatus {
        StingerStatus {
            slot,
            media_input: InputId::new(NonZeroU128::new(2).unwrap()),
            preload: true,
            cut_point_frames: 1,
            audio_policy: StingerAudioPolicy::Muted,
            missing_media_fallback: StingerMissingMediaFallback::Cut,
            readiness: StingerReadiness::Ready,
        }
    }

    #[test]
    fn slots_are_exactly_one_through_eight_and_accessibly_named() {
        assert!(StingerControl::new(0).is_none());
        assert!(StingerControl::new(9).is_none());
        for slot in 1..=8 {
            let control = StingerControl::new(slot).unwrap();
            assert_eq!(control.slot(), slot);
            assert_eq!(
                control.accessibility_label(),
                format!("Fire Stinger slot {slot}")
            );
        }
    }

    #[test]
    fn protocol_1_10_preserves_slot_and_bounded_duration() {
        let session = session(STINGER_PROTOCOL_VERSION, &["transition"]);
        let mut controls = StingerControls::default();
        controls.set_duration_frames(91);
        let projected = [ready_stinger(8)];
        assert_eq!(
            controls.command_payload(
                StingerControl::new(8).unwrap(),
                &ConnectionState::Ready,
                Some(&session),
                &projected,
            ),
            Some(CommandPayload::Stinger {
                slot: WireStingerSlotId::new(8).unwrap(),
                duration_frames: 91,
            })
        );
        controls.set_duration_frames(0);
        assert_eq!(controls.duration_frames(), 1);
        controls.set_duration_frames(u32::MAX);
        assert_eq!(controls.duration_frames(), 3_600);
    }

    #[test]
    fn protocol_permission_and_readiness_gate_every_slot() {
        let current = session(STINGER_PROTOCOL_VERSION, &["transition"]);
        let old = session(ProtocolVersion::new(1, 9), &["transition"]);
        let denied = session(STINGER_PROTOCOL_VERSION, &[]);
        let controls = StingerControls::default();
        let control = StingerControl::new(1).unwrap();
        let projected = [ready_stinger(1)];

        assert_eq!(
            controls.control_state(control, &ConnectionState::Ready, Some(&current), &projected),
            TransitionControlState::Enabled
        );
        assert_eq!(
            controls.control_state(control, &ConnectionState::Ready, Some(&old), &projected),
            TransitionControlState::Hidden
        );
        assert_eq!(
            controls.control_state(control, &ConnectionState::Ready, Some(&denied), &projected),
            TransitionControlState::Disabled
        );
        assert_eq!(
            controls.control_state(
                control,
                &ConnectionState::Connecting,
                Some(&current),
                &projected
            ),
            TransitionControlState::Disabled
        );
        assert_eq!(
            controls.control_state(control, &ConnectionState::Ready, Some(&current), &[]),
            TransitionControlState::Disabled
        );
    }
}
