use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
};

use crate::{SessionId, UserId};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Role {
    Viewer,
    Graphics,
    Audio,
    Operator,
    Admin,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Permission {
    ViewStatus,
    SelectPreview,
    Transition,
    ControlAudio,
    EditProject,
    ManageUsers,
}

/// A target-free authorization category for an incoming command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandClass {
    ViewStatus,
    SelectPreview,
    Transition,
    ControlAudio,
    EditProject,
    ManageUsers,
}

impl CommandClass {
    #[must_use]
    pub const fn required_permission(self) -> Permission {
        match self {
            Self::ViewStatus => Permission::ViewStatus,
            Self::SelectPreview => Permission::SelectPreview,
            Self::Transition => Permission::Transition,
            Self::ControlAudio => Permission::ControlAudio,
            Self::EditProject => Permission::EditProject,
            Self::ManageUsers => Permission::ManageUsers,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrincipalKind {
    Authenticated,
    /// Explicit local-development identity. Production policy rejects it.
    DevelopmentOnly,
}

/// One authenticated identity and the union of its assigned roles.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Principal {
    user_id: UserId,
    session_id: SessionId,
    roles: BTreeSet<Role>,
    kind: PrincipalKind,
}

impl Principal {
    #[must_use]
    pub fn authenticated(
        user_id: UserId,
        session_id: SessionId,
        roles: impl IntoIterator<Item = Role>,
    ) -> Self {
        Self {
            user_id,
            session_id,
            roles: roles.into_iter().collect(),
            kind: PrincipalKind::Authenticated,
        }
    }

    /// Creates an explicitly marked principal for local development only.
    #[must_use]
    pub fn development(
        user_id: UserId,
        session_id: SessionId,
        roles: impl IntoIterator<Item = Role>,
    ) -> Self {
        Self {
            user_id,
            session_id,
            roles: roles.into_iter().collect(),
            kind: PrincipalKind::DevelopmentOnly,
        }
    }

    #[must_use]
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    #[must_use]
    pub fn roles(&self) -> &BTreeSet<Role> {
        &self.roles
    }

    #[must_use]
    pub const fn kind(&self) -> PrincipalKind {
        self.kind
    }
}

/// Role grants and the explicit development-principal switch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Policy {
    grants: BTreeMap<Role, BTreeSet<Permission>>,
    allow_development_principals: bool,
}

impl Policy {
    /// The Phase 1 least-privilege production role matrix.
    #[must_use]
    pub fn production() -> Self {
        let mut grants = BTreeMap::new();
        grants.insert(Role::Viewer, set([Permission::ViewStatus]));
        grants.insert(
            Role::Graphics,
            set([Permission::ViewStatus, Permission::EditProject]),
        );
        grants.insert(
            Role::Audio,
            set([Permission::ViewStatus, Permission::ControlAudio]),
        );
        grants.insert(
            Role::Operator,
            set([
                Permission::ViewStatus,
                Permission::SelectPreview,
                Permission::Transition,
                Permission::ControlAudio,
            ]),
        );
        grants.insert(
            Role::Admin,
            set([
                Permission::ViewStatus,
                Permission::SelectPreview,
                Permission::Transition,
                Permission::ControlAudio,
                Permission::EditProject,
                Permission::ManageUsers,
            ]),
        );
        Self {
            grants,
            allow_development_principals: false,
        }
    }

    /// The standard role matrix with explicitly enabled development identities.
    #[must_use]
    pub fn development() -> Self {
        Self {
            allow_development_principals: true,
            ..Self::production()
        }
    }

    #[must_use]
    pub fn permissions_for(&self, role: Role) -> Option<&BTreeSet<Permission>> {
        self.grants.get(&role)
    }

    #[must_use]
    pub fn effective_permissions(&self, principal: &Principal) -> BTreeSet<Permission> {
        principal
            .roles
            .iter()
            .filter_map(|role| self.grants.get(role))
            .flatten()
            .copied()
            .collect()
    }

    /// Authorizes a target-free command class.
    ///
    /// # Errors
    ///
    /// Returns a structured [`AuthorizationDenial`] when a development-only
    /// principal is disabled or no assigned role grants the permission.
    pub fn authorize(
        &self,
        principal: &Principal,
        command: CommandClass,
    ) -> Result<(), AuthorizationDenial> {
        if principal.kind == PrincipalKind::DevelopmentOnly && !self.allow_development_principals {
            return Err(AuthorizationDenial {
                command,
                required: command.required_permission(),
                reason: DenialReason::DevelopmentPrincipalDisabled,
            });
        }

        let required = command.required_permission();
        if self.effective_permissions(principal).contains(&required) {
            Ok(())
        } else {
            Err(AuthorizationDenial {
                command,
                required,
                reason: DenialReason::MissingPermission,
            })
        }
    }
}

fn set<const N: usize>(permissions: [Permission; N]) -> BTreeSet<Permission> {
    permissions.into_iter().collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DenialReason {
    MissingPermission,
    DevelopmentPrincipalDisabled,
}

/// A target-free denial safe to return to a caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthorizationDenial {
    pub command: CommandClass,
    pub required: Permission,
    pub reason: DenialReason,
}

impl fmt::Display for AuthorizationDenial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "command denied: {:?} ({:?})",
            self.required, self.reason
        )
    }
}

impl Error for AuthorizationDenial {}
