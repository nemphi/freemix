use std::{convert::Infallible, net::IpAddr, num::NonZeroU128};

use fm_auth::{Principal, Role as AuthRole, SessionId, UserId};
use fm_protocol::{
    CURRENT_PROTOCOL_VERSION, ClientType, CommandMessage, CommandPayload, EngineIdentity,
    EventCursor, EventMessage, EventPayload, FadeToBlackPosition, FadeToBlackState,
    HandshakeRequest, ManualTransitionStatus, ProtocolVersion, ResumeCursor, Role, ServerIdentity,
    SnapshotMessage, WireInputId,
};
use fm_server::{
    AuthenticationMode, ConfigError, ControlPlane, DisconnectReason, HandshakeError, HealthState,
    Heartbeat, InitialSync, RateLimit, ReadinessState, Server, ServerConfig, ServerMode,
    SessionError, SessionLimits, SessionState, SyncPayload,
};

#[derive(Clone, Debug)]
struct FakeControl {
    engine: EngineIdentity,
    snapshot: SnapshotMessage,
    events: Vec<EventMessage>,
}

impl ControlPlane for FakeControl {
    type Error = Infallible;

    fn initial_sync(&self, cursor: Option<&EventCursor>) -> Result<InitialSync, Self::Error> {
        let payload = if cursor.is_some_and(|cursor| {
            cursor.engine == self.engine && cursor.revision <= self.snapshot.revision
        }) {
            SyncPayload::Resume(
                self.events
                    .iter()
                    .filter(|event| event.cursor.revision > cursor.unwrap().revision)
                    .cloned()
                    .collect(),
            )
        } else {
            SyncPayload::Snapshot(Box::new(self.snapshot.clone()))
        };
        Ok(InitialSync {
            engine: self.engine.clone(),
            current_revision: self.snapshot.revision,
            payload,
        })
    }
}

fn engine() -> EngineIdentity {
    EngineIdentity {
        engine_id: "engine-a".into(),
        state_epoch: 2,
        log_id: "log-a".into(),
    }
}

fn input(value: u128) -> WireInputId {
    WireInputId::new(NonZeroU128::new(value).unwrap())
}

fn control() -> FakeControl {
    let engine = engine();
    let snapshot = SnapshotMessage {
        engine: engine.clone(),
        revision: 4,
        show_name: "show".into(),
        inputs: vec![input(1), input(2)],
        desired_program: input(1),
        desired_preview: input(2),
        realized_program: input(1),
        realized_preview: input(2),
        desired_manual_transition: ManualTransitionStatus::Inactive,
        realized_manual_transition: ManualTransitionStatus::Inactive,
        desired_fade_to_black: FadeToBlackState {
            target_active: false,
            position: FadeToBlackPosition::LIVE,
        },
        realized_fade_to_black: FadeToBlackState {
            target_active: false,
            position: FadeToBlackPosition::LIVE,
        },
        stingers: Vec::new(),
        desired_overlays: fm_protocol::OverlayStatus::empty_channels(),
        realized_overlays: fm_protocol::OverlayStatus::empty_channels(),
    };
    let events = [3, 4]
        .map(|revision| EventMessage {
            cursor: EventCursor {
                engine: engine.clone(),
                revision,
            },
            payload: EventPayload::DesiredSwitcher {
                program: input(1),
                preview: input(2),
                manual_transition: ManualTransitionStatus::Inactive,
                fade_to_black: FadeToBlackState {
                    target_active: false,
                    position: FadeToBlackPosition::LIVE,
                },
                overlays: fm_protocol::OverlayStatus::empty_channels(),
            },
        })
        .to_vec();
    FakeControl {
        engine,
        snapshot,
        events,
    }
}

fn config() -> ServerConfig {
    ServerConfig::new(
        ServerMode::Production,
        AuthenticationMode::Required,
        IpAddr::from([127, 0, 0, 1]),
        vec![CURRENT_PROTOCOL_VERSION],
        "capabilities-v1",
    )
}

fn principal(role: AuthRole) -> Principal {
    Principal::authenticated(
        UserId::new("alice").unwrap(),
        SessionId::new("session-1").unwrap(),
        [role],
    )
}

fn development_principal(role: AuthRole) -> Principal {
    Principal::development(
        UserId::new("developer").unwrap(),
        SessionId::new("local-session").unwrap(),
        [role],
    )
}

fn hello(role: Role, cursor: Option<EventCursor>) -> HandshakeRequest {
    HandshakeRequest {
        versions: vec![CURRENT_PROTOCOL_VERSION],
        build: "test".into(),
        client_type: ClientType::Cli,
        desired_role: role,
        resume_cursor: cursor.map(|cursor| ResumeCursor {
            server: ServerIdentity {
                engine_id: cursor.engine.engine_id,
                project_id: "project-9".into(),
                state_epoch: cursor.engine.state_epoch,
                log_id: cursor.engine.log_id,
            },
            revision: cursor.revision,
        }),
    }
}

fn ready_server(config: ServerConfig) -> Server<FakeControl> {
    let mut server = Server::new(config, control()).unwrap();
    server.mark_ready().unwrap();
    server
}

#[test]
fn incompatible_versions_are_rejected() {
    let server = ready_server(config());
    let mut hello = hello(Role::Viewer, None);
    hello.versions = vec![ProtocolVersion::new(99, 0)];

    assert!(matches!(
        server.handshake(&hello, &principal(AuthRole::Viewer), 0),
        Err(HandshakeError::IncompatibleVersion)
    ));
}

#[test]
fn development_auth_requires_development_mode_and_loopback() {
    let production = ServerConfig::new(
        ServerMode::Production,
        AuthenticationMode::Development,
        IpAddr::from([127, 0, 0, 1]),
        vec![CURRENT_PROTOCOL_VERSION],
        "digest",
    );
    assert_eq!(
        Server::new(production, control()).unwrap_err(),
        ConfigError::DevelopmentAuthInProduction
    );

    let exposed = ServerConfig::new(
        ServerMode::Development,
        AuthenticationMode::Development,
        IpAddr::from([0, 0, 0, 0]),
        vec![CURRENT_PROTOCOL_VERSION],
        "digest",
    );
    assert_eq!(
        Server::new(exposed, control()).unwrap_err(),
        ConfigError::DevelopmentAuthRequiresLoopback
    );
}

#[test]
fn production_denies_development_principals() {
    let server = ready_server(config());
    assert!(matches!(
        server.handshake(
            &hello(Role::Admin, None),
            &development_principal(AuthRole::Admin),
            0,
        ),
        Err(HandshakeError::DevelopmentPrincipalDenied)
    ));
}

#[test]
fn handshake_delegates_snapshot_and_resume_selection() {
    let server = ready_server(config());
    let snapshot = server
        .handshake(&hello(Role::Viewer, None), &principal(AuthRole::Viewer), 10)
        .unwrap();
    assert_eq!(snapshot.server_hello.negotiated, CURRENT_PROTOCOL_VERSION);
    assert!(!snapshot.server_hello.resume);
    assert!(matches!(snapshot.sync, SyncPayload::Snapshot(_)));

    let cursor = EventCursor {
        engine: engine(),
        revision: 2,
    };
    let resumed = server
        .handshake(
            &hello(Role::Viewer, Some(cursor)),
            &principal(AuthRole::Viewer),
            10,
        )
        .unwrap();
    assert!(resumed.server_hello.resume);
    assert!(matches!(resumed.sync, SyncPayload::Resume(events) if events.len() == 2));
}

#[test]
fn desired_role_and_commands_are_authorized_to_the_granted_scope() {
    let server = ready_server(config());
    assert!(matches!(
        server.handshake(
            &hello(Role::Operator, None),
            &principal(AuthRole::Viewer),
            0,
        ),
        Err(HandshakeError::RoleDenied(Role::Operator))
    ));

    let mut outcome = server
        .handshake(&hello(Role::Viewer, None), &principal(AuthRole::Admin), 0)
        .unwrap();
    let command = command(CommandPayload::Cut);
    assert!(matches!(
        outcome.session.admit_command(&command, 10, 0),
        Err(SessionError::Authorization(_))
    ));
}

#[test]
fn wipe_uses_transition_authorization_and_fade_rate_accounting() {
    let server = ready_server(config());
    let wipe = command(CommandPayload::Wipe { duration_frames: 3 });
    let mut viewer = server
        .handshake(&hello(Role::Viewer, None), &principal(AuthRole::Viewer), 0)
        .unwrap()
        .session;
    assert!(matches!(
        viewer.admit_command(&wipe, 10, 0),
        Err(SessionError::Authorization(_))
    ));

    let limits = SessionLimits {
        inbound_commands: RateLimit::new(1, 100),
        ..SessionLimits::default()
    };
    let current_config = ServerConfig::new(
        ServerMode::Production,
        AuthenticationMode::Required,
        IpAddr::from([127, 0, 0, 1]),
        vec![CURRENT_PROTOCOL_VERSION],
        "capabilities-v1",
    )
    .with_session_limits(limits);
    let server = ready_server(current_config);
    let mut current_hello = hello(Role::Operator, None);
    current_hello.versions = vec![CURRENT_PROTOCOL_VERSION];
    let mut operator = server
        .handshake(&current_hello, &principal(AuthRole::Operator), 0)
        .unwrap()
        .session;
    let mut wipe = wipe;
    wipe.protocol = CURRENT_PROTOCOL_VERSION;
    operator.admit_command(&wipe, 10, 0).unwrap();
    operator.command_completed().unwrap();
    let mut fade = command(CommandPayload::Fade { duration_frames: 3 });
    fade.protocol = CURRENT_PROTOCOL_VERSION;
    assert_eq!(
        operator.admit_command(&fade, 10, 0),
        Err(SessionError::InboundRateLimited)
    );
}

#[test]
fn heartbeat_tracks_cursor_and_times_out() {
    let server = ready_server(config());
    let mut session = server
        .handshake(
            &hello(Role::Viewer, None),
            &principal(AuthRole::Viewer),
            100,
        )
        .unwrap()
        .session;
    let cursor = EventCursor {
        engine: engine(),
        revision: 4,
    };
    session
        .record_heartbeat(
            Heartbeat {
                last_applied: cursor.clone(),
                clock_sample_ms: 90,
            },
            200,
        )
        .unwrap();
    assert_eq!(session.heartbeat().last_applied.as_ref(), Some(&cursor));
    assert_eq!(session.heartbeat().received_total, 1);
    assert!(session.check_heartbeat(15_199).is_ok());
    assert_eq!(
        session.check_heartbeat(15_200),
        Err(SessionError::Disconnected(
            DisconnectReason::HeartbeatTimeout
        ))
    );
}

#[test]
fn command_rates_and_outbound_buffers_are_bounded() {
    let limits = SessionLimits {
        max_command_bytes: 32,
        max_inflight_commands: 1,
        inbound_commands: RateLimit::new(1, 100),
        max_outbound_messages: 2,
        max_outbound_bytes: 10,
        outbound_messages: RateLimit::new(10, 100),
        heartbeat_timeout_ms: 1_000,
    };
    let server = ready_server(config().with_session_limits(limits));
    let mut session = server
        .handshake(
            &hello(Role::Operator, None),
            &principal(AuthRole::Operator),
            0,
        )
        .unwrap()
        .session;
    let command = command(CommandPayload::Cut);

    assert_eq!(
        session.admit_command(&command, 33, 0),
        Err(SessionError::CommandTooLarge {
            size: 33,
            maximum: 32,
        })
    );
    session.admit_command(&command, 20, 0).unwrap();
    assert_eq!(
        session.admit_command(&command, 20, 0),
        Err(SessionError::TooManyInflightCommands)
    );
    session.command_completed().unwrap();
    assert_eq!(
        session.admit_command(&command, 20, 0),
        Err(SessionError::InboundRateLimited)
    );
    session.admit_command(&command, 20, 100).unwrap();

    session.queue_outbound(5, 0).unwrap();
    session.queue_outbound(5, 0).unwrap();
    assert_eq!(session.accounting().outbound_bytes_queued, 10);
    assert_eq!(
        session.queue_outbound(1, 0),
        Err(SessionError::Disconnected(DisconnectReason::SlowClient))
    );
    assert_eq!(
        session.state(),
        SessionState::Disconnected(DisconnectReason::SlowClient)
    );
}

#[test]
fn outbound_message_rate_limit_is_retryable() {
    let limits = SessionLimits {
        max_command_bytes: 32,
        max_inflight_commands: 1,
        inbound_commands: RateLimit::new(1, 100),
        max_outbound_messages: 2,
        max_outbound_bytes: 10,
        outbound_messages: RateLimit::new(1, 100),
        heartbeat_timeout_ms: 1_000,
    };
    let server = ready_server(config().with_session_limits(limits));
    let mut session = server
        .handshake(&hello(Role::Viewer, None), &principal(AuthRole::Viewer), 0)
        .unwrap()
        .session;

    session.queue_outbound(5, 0).unwrap();
    assert_eq!(
        session.queue_outbound(5, 0),
        Err(SessionError::OutboundRateLimited)
    );
    assert_eq!(session.state(), SessionState::Connected);
    session.outbound_delivered().unwrap();
    session.queue_outbound(5, 100).unwrap();
}

#[test]
fn health_and_readiness_have_stable_lifecycle_states() {
    let mut server = Server::new(config(), control()).unwrap();
    assert_eq!(server.status().health(), HealthState::Healthy);
    assert_eq!(server.status().readiness(), ReadinessState::Starting);
    assert!(matches!(
        server.handshake(&hello(Role::Viewer, None), &principal(AuthRole::Viewer), 0),
        Err(HandshakeError::NotReady(ReadinessState::Starting))
    ));

    server.mark_ready().unwrap();
    assert_eq!(server.status().readiness(), ReadinessState::Ready);
    server.begin_draining().unwrap();
    assert_eq!(server.status().health(), HealthState::Healthy);
    assert_eq!(server.status().readiness(), ReadinessState::Draining);
    assert!(server.mark_ready().is_err());
    server.mark_unhealthy();
    assert_eq!(server.status().health(), HealthState::Unhealthy);
    assert_eq!(server.status().readiness(), ReadinessState::Unhealthy);
}

fn command(payload: CommandPayload) -> CommandMessage {
    CommandMessage {
        protocol: CURRENT_PROTOCOL_VERSION,
        id: "command-1".into(),
        idempotency_key: "alice-1".into(),
        expected_revision: None,
        deadline_ms: None,
        payload,
    }
}
