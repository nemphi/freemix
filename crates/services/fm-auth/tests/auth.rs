use std::{collections::BTreeSet, num::NonZeroU64};

use fm_auth::{
    CommandClass, DenialReason, PairingCode, PairingCodeValidator, PairingConfigurationError,
    PairingError, Permission, Policy, Principal, PrincipalKind, Role, SessionId, UserId,
};

const COMMANDS: [CommandClass; 6] = [
    CommandClass::ViewStatus,
    CommandClass::SelectPreview,
    CommandClass::Transition,
    CommandClass::ControlAudio,
    CommandClass::EditProject,
    CommandClass::ManageUsers,
];

fn user(value: &str) -> UserId {
    UserId::new(value).unwrap()
}

fn session(value: &str) -> SessionId {
    SessionId::new(value).unwrap()
}

fn principal(roles: impl IntoIterator<Item = Role>) -> Principal {
    Principal::authenticated(user("alice"), session("session_1"), roles)
}

fn code(value: &str) -> PairingCode {
    PairingCode::new(value).unwrap()
}

#[test]
fn stable_identity_validation_rejects_ambiguous_values() {
    assert_eq!(user("operator_1").as_str(), "operator_1");
    assert_eq!(session("local-session").as_str(), "local-session");
    assert!(UserId::new("").is_err());
    assert!(UserId::new("Admin").is_err());
    assert!(SessionId::new("session.1").is_err());
}

#[test]
fn production_role_matrix_is_least_privilege() {
    let policy = Policy::production();
    let cases = [
        (Role::Viewer, [true, false, false, false, false, false]),
        (Role::Graphics, [true, false, false, false, true, false]),
        (Role::Audio, [true, false, false, true, true, false]),
        (Role::Operator, [true, true, true, true, false, false]),
        (Role::Admin, [true, true, true, true, true, true]),
    ];

    for (role, expected) in cases {
        let principal = principal([role]);
        for (command, allowed) in COMMANDS.into_iter().zip(expected) {
            assert_eq!(
                policy.authorize(&principal, command).is_ok(),
                allowed,
                "{role:?} / {command:?}"
            );
        }
    }
}

#[test]
fn multiple_roles_union_their_permissions() {
    let policy = Policy::production();
    let principal = principal([Role::Graphics, Role::Operator]);

    assert!(
        policy
            .authorize(&principal, CommandClass::EditProject)
            .is_ok()
    );
    assert!(
        policy
            .authorize(&principal, CommandClass::Transition)
            .is_ok()
    );
    assert_eq!(
        policy.effective_permissions(&principal),
        BTreeSet::from([
            Permission::ViewStatus,
            Permission::SelectPreview,
            Permission::Transition,
            Permission::ControlAudio,
            Permission::EditProject,
        ])
    );
}

#[test]
fn denial_is_structured_and_contains_no_identity_or_target_details() {
    let policy = Policy::production();
    let principal = principal([Role::Viewer]);

    let denial = policy
        .authorize(&principal, CommandClass::ManageUsers)
        .unwrap_err();

    assert_eq!(denial.required, Permission::ManageUsers);
    assert_eq!(denial.reason, DenialReason::MissingPermission);
    let rendered = denial.to_string();
    assert!(!rendered.contains("alice"));
    assert!(!rendered.contains("session_1"));
}

#[test]
fn development_principal_is_explicit_and_disabled_in_production() {
    let principal = Principal::development(user("developer"), session("local"), [Role::Admin]);
    assert_eq!(principal.kind(), PrincipalKind::DevelopmentOnly);
    let denial = Policy::production()
        .authorize(&principal, CommandClass::ManageUsers)
        .unwrap_err();
    assert_eq!(denial.reason, DenialReason::DevelopmentPrincipalDisabled);
    assert!(
        Policy::development()
            .authorize(&principal, CommandClass::ManageUsers)
            .is_ok()
    );
}

#[test]
fn pairing_code_is_short_lived_and_expires_at_boundary() {
    let mut validator =
        PairingCodeValidator::new(code("opaque-123"), 100, NonZeroU64::new(30).unwrap()).unwrap();
    assert_eq!(validator.expires_at(), 130);
    assert_eq!(
        validator.consume(&code("opaque-123"), 130),
        Err(PairingError::Expired)
    );
    assert!(!validator.is_consumed());
}

#[test]
fn pairing_lifetime_is_capped_by_the_primitive() {
    let error =
        PairingCodeValidator::new(code("opaque"), 0, NonZeroU64::new(601).unwrap()).unwrap_err();
    assert!(matches!(
        error,
        PairingConfigurationError::LifetimeTooLong {
            provided: 601,
            maximum: 600
        }
    ));
}

#[test]
fn invalid_pairing_attempt_does_not_consume_but_success_does() {
    let mut validator =
        PairingCodeValidator::new(code("correct"), 10, NonZeroU64::new(5).unwrap()).unwrap();

    assert_eq!(
        validator.consume(&code("incorrect"), 11),
        Err(PairingError::Invalid)
    );
    assert!(!validator.is_consumed());
    assert_eq!(validator.consume(&code("correct"), 12), Ok(()));
    assert!(validator.is_consumed());
    assert_eq!(
        validator.consume(&code("correct"), 13),
        Err(PairingError::AlreadyConsumed)
    );
}

#[test]
fn pairing_secrets_are_redacted_in_debug_output() {
    let secret = code("do-not-log-this");
    assert!(!format!("{secret:?}").contains("do-not-log-this"));
    let validator = PairingCodeValidator::new(secret, 0, NonZeroU64::new(1).unwrap()).unwrap();
    assert!(!format!("{validator:?}").contains("do-not-log-this"));
}
