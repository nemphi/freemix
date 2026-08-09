use std::{error::Error, fmt};

use fm_auth::{Permission, Policy, Principal, PrincipalKind, Role as AuthRole};
use fm_protocol::{
    EngineIdentity, EventCursor, HandshakeRequest, Role as ProtocolRole, ServerHello,
    negotiate_version,
};

use crate::{
    AuthenticationMode, ConfigError, ControlPlane, InitialSync, ReadinessState, ServerConfig,
    ServiceStatus, Session, StatusTransitionError, SyncPayload,
};

#[derive(Debug)]
pub struct HandshakeOutcome {
    pub server_hello: ServerHello,
    pub sync: SyncPayload,
    pub session: Session,
}

#[derive(Debug)]
pub struct Server<C> {
    config: ServerConfig,
    control: C,
    policy: Policy,
    status: ServiceStatus,
}

impl<C> Server<C> {
    /// Creates a server after enforcing all configuration invariants.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] for an unsafe or unbounded configuration.
    pub fn new(config: ServerConfig, control: C) -> Result<Self, ConfigError> {
        config.validate()?;
        let policy = match config.authentication {
            AuthenticationMode::Required => Policy::production(),
            AuthenticationMode::Development => Policy::development(),
        };
        Ok(Self {
            config,
            control,
            policy,
            status: ServiceStatus::new(),
        })
    }

    #[must_use]
    pub const fn config(&self) -> &ServerConfig {
        &self.config
    }

    #[must_use]
    pub const fn status(&self) -> ServiceStatus {
        self.status
    }

    /// Marks startup complete and enables handshakes.
    ///
    /// # Errors
    ///
    /// Returns an invalid-transition error after startup has ended.
    pub fn mark_ready(&mut self) -> Result<(), StatusTransitionError> {
        self.status.mark_ready()
    }

    /// Stops accepting handshakes while remaining healthy.
    ///
    /// # Errors
    ///
    /// Returns an invalid-transition error unless the server is ready.
    pub fn begin_draining(&mut self) -> Result<(), StatusTransitionError> {
        self.status.begin_draining()
    }

    pub fn mark_unhealthy(&mut self) {
        self.status.mark_unhealthy();
    }

    #[must_use]
    pub fn control(&self) -> &C {
        &self.control
    }

    #[must_use]
    pub fn into_control(self) -> C {
        self.control
    }
}

impl<C: ControlPlane> Server<C> {
    /// Negotiates, authorizes, and obtains initial state from the control plane.
    ///
    /// # Errors
    ///
    /// Returns a readiness, compatibility, authentication, authorization, or
    /// control-plane error when a session cannot be established.
    pub fn handshake(
        &self,
        hello: &HandshakeRequest,
        principal: &Principal,
        now_ms: u64,
    ) -> Result<HandshakeOutcome, HandshakeError<C::Error>> {
        if self.status.readiness() != ReadinessState::Ready {
            return Err(HandshakeError::NotReady(self.status.readiness()));
        }
        if principal.kind() == PrincipalKind::DevelopmentOnly
            && self.config.authentication != AuthenticationMode::Development
        {
            return Err(HandshakeError::DevelopmentPrincipalDenied);
        }

        let negotiated = negotiate_version(&hello.versions, &self.config.supported_versions)
            .map_err(|_| HandshakeError::IncompatibleVersion)?;
        let requested_role =
            map_role(hello.desired_role).ok_or(HandshakeError::RoleDenied(hello.desired_role))?;
        if !principal.roles().contains(&requested_role)
            && !principal.roles().contains(&AuthRole::Admin)
        {
            return Err(HandshakeError::RoleDenied(hello.desired_role));
        }

        let scoped_principal = scoped_principal(principal, requested_role);
        let cached_cursor = hello.resume_cursor.as_ref().map(|cursor| EventCursor {
            engine: EngineIdentity {
                engine_id: cursor.server.engine_id.clone(),
                state_epoch: cursor.server.state_epoch,
                log_id: cursor.server.log_id.clone(),
            },
            revision: cursor.revision,
        });
        let initial = self
            .control
            .initial_sync(cached_cursor.as_ref())
            .map_err(HandshakeError::Control)?;
        validate_initial_sync(&initial, cached_cursor.as_ref())?;

        let permissions = self
            .policy
            .permissions_for(requested_role)
            .into_iter()
            .flatten()
            .map(|permission| permission_name(*permission).to_owned())
            .collect();
        let sync = initial.payload;
        let server_hello = ServerHello {
            negotiated,
            granted_role: hello.desired_role,
            permissions,
            capabilities_digest: self.config.capabilities_digest.clone(),
            engine: initial.engine.clone(),
            current_revision: initial.current_revision,
            resume: sync.is_resume(),
        };
        let session = Session::new(
            negotiated,
            initial.engine,
            initial.current_revision,
            scoped_principal,
            self.policy.clone(),
            self.config.session_limits.clone(),
            now_ms,
        );
        Ok(HandshakeOutcome {
            server_hello,
            sync,
            session,
        })
    }
}

fn map_role(role: ProtocolRole) -> Option<AuthRole> {
    match role {
        ProtocolRole::Viewer => Some(AuthRole::Viewer),
        ProtocolRole::Graphics => Some(AuthRole::Graphics),
        ProtocolRole::Audio => Some(AuthRole::Audio),
        ProtocolRole::Operator => Some(AuthRole::Operator),
        ProtocolRole::Admin => Some(AuthRole::Admin),
        ProtocolRole::Replay => None,
    }
}

fn scoped_principal(principal: &Principal, role: AuthRole) -> Principal {
    match principal.kind() {
        PrincipalKind::Authenticated => Principal::authenticated(
            principal.user_id().clone(),
            principal.session_id().clone(),
            [role],
        ),
        PrincipalKind::DevelopmentOnly => Principal::development(
            principal.user_id().clone(),
            principal.session_id().clone(),
            [role],
        ),
    }
}

const fn permission_name(permission: Permission) -> &'static str {
    match permission {
        Permission::ViewStatus => "view_status",
        Permission::SelectPreview => "select_preview",
        Permission::Transition => "transition",
        Permission::EditProject => "edit_project",
        Permission::ManageUsers => "manage_users",
    }
}

fn validate_initial_sync<E>(
    initial: &InitialSync,
    requested: Option<&EventCursor>,
) -> Result<(), HandshakeError<E>> {
    match &initial.payload {
        SyncPayload::Snapshot(snapshot) => {
            if snapshot.engine != initial.engine || snapshot.revision != initial.current_revision {
                return Err(HandshakeError::InvalidControlSync);
            }
        }
        SyncPayload::Resume(events) => {
            let Some(requested) = requested else {
                return Err(HandshakeError::InvalidControlSync);
            };
            if requested.engine != initial.engine || requested.revision > initial.current_revision {
                return Err(HandshakeError::InvalidControlSync);
            }
            let mut previous_revision = requested.revision;
            for event in events {
                if event.cursor.engine != initial.engine
                    || event.cursor.revision <= previous_revision
                    || event.cursor.revision > initial.current_revision
                {
                    return Err(HandshakeError::InvalidControlSync);
                }
                previous_revision = event.cursor.revision;
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
pub enum HandshakeError<E> {
    NotReady(ReadinessState),
    IncompatibleVersion,
    DevelopmentPrincipalDenied,
    RoleDenied(ProtocolRole),
    Control(E),
    InvalidControlSync,
}

impl<E: fmt::Display> fmt::Display for HandshakeError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotReady(state) => write!(formatter, "server is not ready: {}", state.as_str()),
            Self::IncompatibleVersion => {
                formatter.write_str("client and server protocol versions are incompatible")
            }
            Self::DevelopmentPrincipalDenied => {
                formatter.write_str("development principal is disabled")
            }
            Self::RoleDenied(role) => write!(formatter, "requested role {role:?} is denied"),
            Self::Control(error) => write!(formatter, "control plane failed: {error}"),
            Self::InvalidControlSync => {
                formatter.write_str("control plane returned an inconsistent initial sync")
            }
        }
    }
}

impl<E: Error + 'static> Error for HandshakeError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Control(error) => Some(error),
            _ => None,
        }
    }
}
