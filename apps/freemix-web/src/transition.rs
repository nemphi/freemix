use fm_client::{ConnectionState, Session};
use fm_protocol::{CommandPayload, WIPE_PROTOCOL_VERSION};

/// A transition control represented by the semantic presentation model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionControl {
    Cut,
    Auto,
    Wipe,
    Duration,
}

impl TransitionControl {
    /// Accessibility label exposed by the presentation model.
    #[must_use]
    pub const fn accessibility_label(self) -> &'static str {
        match self {
            Self::Cut => "Cut Preview to Program",
            Self::Auto => "Transition Preview to Program",
            Self::Wipe => "Wipe Preview to Program",
            Self::Duration => "Transition duration",
        }
    }
}

/// Whether the presentation model exposes a semantic transition control as interactive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionControlState {
    Hidden,
    Disabled,
    Enabled,
}

impl TransitionControlState {
    /// Whether the presentation model includes the control.
    #[must_use]
    pub const fn is_exposed(self) -> bool {
        !matches!(self, Self::Hidden)
    }

    /// Whether the control is interactive in the semantic presentation model.
    ///
    /// Interactive input controls such as Duration update local model state instead of issuing
    /// command payloads.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

/// Transport-free transition state for semantic presentation consumers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransitionControls {
    duration_frames: u32,
}

impl TransitionControls {
    pub const MIN_DURATION_FRAMES: u32 = 1;
    pub const MAX_DURATION_FRAMES: u32 = 3_600;
    pub const DEFAULT_DURATION_FRAMES: u32 = 30;

    #[must_use]
    pub const fn duration_frames(&self) -> u32 {
        self.duration_frames
    }

    /// Sets the Fade/Wipe duration, bounded to the range accepted by the control model.
    pub fn set_duration_frames(&mut self, duration_frames: u32) {
        self.duration_frames =
            duration_frames.clamp(Self::MIN_DURATION_FRAMES, Self::MAX_DURATION_FRAMES);
    }

    /// Derives visibility and interactivity from the active negotiated session.
    #[must_use]
    pub fn control_state(
        &self,
        control: TransitionControl,
        connection_state: &ConnectionState,
        session: Option<&Session>,
    ) -> TransitionControlState {
        if matches!(control, TransitionControl::Wipe) && !session.is_some_and(session_supports_wipe)
        {
            return TransitionControlState::Hidden;
        }

        if matches!(connection_state, ConnectionState::Ready)
            && session.is_some_and(session_can_transition)
        {
            TransitionControlState::Enabled
        } else {
            TransitionControlState::Disabled
        }
    }

    /// Builds a command for an enabled command-emitting control.
    #[must_use]
    pub fn command_payload(
        &self,
        control: TransitionControl,
        connection_state: &ConnectionState,
        session: Option<&Session>,
    ) -> Option<CommandPayload> {
        if !self
            .control_state(control, connection_state, session)
            .is_enabled()
        {
            return None;
        }

        match control {
            TransitionControl::Cut => Some(CommandPayload::Cut),
            TransitionControl::Auto => Some(CommandPayload::Fade {
                duration_frames: self.duration_frames,
            }),
            TransitionControl::Wipe => Some(CommandPayload::Wipe {
                duration_frames: self.duration_frames,
            }),
            TransitionControl::Duration => None,
        }
    }
}

impl Default for TransitionControls {
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

fn session_supports_wipe(session: &Session) -> bool {
    session.protocol.major == WIPE_PROTOCOL_VERSION.major
        && session.protocol.minor >= WIPE_PROTOCOL_VERSION.minor
}

#[cfg(test)]
mod tests {
    use fm_client::{ReconnectBackoff, SyncMode};
    use fm_protocol::{ProtocolVersion, Role, ServerIdentity};

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
                engine_id: "engine-web-test".to_owned(),
                project_id: "project-web-test".to_owned(),
                state_epoch: 1,
                log_id: "log-web-test".to_owned(),
            },
            capabilities_digest: "sha256:web-test".to_owned(),
        }
    }

    #[test]
    fn protocol_1_3_exposes_and_enables_wipe_for_transition_operators() {
        let controls = TransitionControls::default();
        let session = session(WIPE_PROTOCOL_VERSION, &["transition"]);

        assert_eq!(
            controls.control_state(
                TransitionControl::Wipe,
                &ConnectionState::Ready,
                Some(&session)
            ),
            TransitionControlState::Enabled
        );
    }

    #[test]
    fn protocol_1_2_neither_exposes_nor_builds_wipe() {
        let controls = TransitionControls::default();
        let session = session(ProtocolVersion::new(1, 2), &["transition"]);

        let state = controls.control_state(
            TransitionControl::Wipe,
            &ConnectionState::Ready,
            Some(&session),
        );
        assert!(!state.is_exposed());
        assert_eq!(
            controls.command_payload(
                TransitionControl::Wipe,
                &ConnectionState::Ready,
                Some(&session)
            ),
            None
        );
        assert_eq!(
            controls.command_payload(
                TransitionControl::Auto,
                &ConnectionState::Ready,
                Some(&session)
            ),
            Some(CommandPayload::Fade {
                duration_frames: TransitionControls::DEFAULT_DURATION_FRAMES
            })
        );
    }

    #[test]
    fn transition_permission_gates_cut_fade_and_wipe_independently_of_protocol() {
        let controls = TransitionControls::default();
        let session = session(WIPE_PROTOCOL_VERSION, &["view_status"]);

        for control in [
            TransitionControl::Cut,
            TransitionControl::Auto,
            TransitionControl::Wipe,
        ] {
            assert_eq!(
                controls.control_state(control, &ConnectionState::Ready, Some(&session)),
                TransitionControlState::Disabled
            );
            assert_eq!(
                controls.command_payload(control, &ConnectionState::Ready, Some(&session)),
                None
            );
        }
    }

    #[test]
    fn duration_is_an_enabled_input_without_a_command_payload() {
        let controls = TransitionControls::default();
        let session = session(WIPE_PROTOCOL_VERSION, &["transition"]);

        assert!(
            controls
                .control_state(
                    TransitionControl::Duration,
                    &ConnectionState::Ready,
                    Some(&session)
                )
                .is_enabled()
        );
        assert_eq!(
            controls.command_payload(
                TransitionControl::Duration,
                &ConnectionState::Ready,
                Some(&session)
            ),
            None
        );
    }

    #[test]
    fn fade_and_wipe_payloads_share_the_bounded_duration() {
        let mut controls = TransitionControls::default();
        let session = session(WIPE_PROTOCOL_VERSION, &["transition"]);

        controls.set_duration_frames(45);
        assert_eq!(
            controls.command_payload(
                TransitionControl::Auto,
                &ConnectionState::Ready,
                Some(&session)
            ),
            Some(CommandPayload::Fade {
                duration_frames: 45
            })
        );
        assert_eq!(
            controls.command_payload(
                TransitionControl::Wipe,
                &ConnectionState::Ready,
                Some(&session)
            ),
            Some(CommandPayload::Wipe {
                duration_frames: 45
            })
        );

        controls.set_duration_frames(0);
        assert_eq!(
            controls.duration_frames(),
            TransitionControls::MIN_DURATION_FRAMES
        );
        controls.set_duration_frames(u32::MAX);
        assert_eq!(
            controls.duration_frames(),
            TransitionControls::MAX_DURATION_FRAMES
        );
    }

    #[test]
    fn reconnect_hides_wipe_until_the_new_protocol_is_negotiated() {
        let controls = TransitionControls::default();
        let current = session(WIPE_PROTOCOL_VERSION, &["transition"]);
        assert!(
            controls
                .control_state(
                    TransitionControl::Wipe,
                    &ConnectionState::Ready,
                    Some(&current)
                )
                .is_enabled()
        );

        assert_eq!(
            controls.control_state(
                TransitionControl::Wipe,
                &ConnectionState::Backoff(ReconnectBackoff {
                    attempt: 1,
                    delay_ms: 250,
                }),
                None
            ),
            TransitionControlState::Hidden
        );

        let downgraded = session(ProtocolVersion::new(1, 2), &["transition"]);
        assert_eq!(
            controls.control_state(
                TransitionControl::Wipe,
                &ConnectionState::Synchronizing {
                    mode: SyncMode::Resume,
                    target_revision: 4,
                },
                Some(&downgraded)
            ),
            TransitionControlState::Hidden
        );
        assert_eq!(
            controls.command_payload(
                TransitionControl::Wipe,
                &ConnectionState::Ready,
                Some(&downgraded)
            ),
            None
        );
    }
}
