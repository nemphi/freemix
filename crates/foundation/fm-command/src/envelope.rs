use crate::Revision;
use core::fmt;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CommandId(String);

impl CommandId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for CommandId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for CommandId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl fmt::Display for CommandId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for IdempotencyKey {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for IdempotencyKey {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl fmt::Display for IdempotencyKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// An absolute deadline in a caller-defined millisecond clock domain.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Deadline(u64);

impl Deadline {
    #[must_use]
    pub const fn from_millis(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn as_millis(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn is_exceeded_at(self, now_millis: u64) -> bool {
        now_millis > self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandEnvelope<C> {
    pub id: CommandId,
    pub idempotency_key: IdempotencyKey,
    pub expected_revision: Option<Revision>,
    pub deadline: Option<Deadline>,
    pub command: C,
}

impl<C> CommandEnvelope<C> {
    #[must_use]
    pub fn new(
        id: impl Into<CommandId>,
        idempotency_key: impl Into<IdempotencyKey>,
        command: C,
    ) -> Self {
        Self {
            id: id.into(),
            idempotency_key: idempotency_key.into(),
            expected_revision: None,
            deadline: None,
            command,
        }
    }

    #[must_use]
    pub const fn expecting(mut self, revision: Revision) -> Self {
        self.expected_revision = Some(revision);
        self
    }

    #[must_use]
    pub const fn with_deadline(mut self, deadline: Deadline) -> Self {
        self.deadline = Some(deadline);
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum RejectionCode {
    PermissionDenied,
    DeadlineExceeded,
    RevisionConflict,
    InvalidCommand,
    NotFound,
    Conflict,
    Unavailable,
    ResourceExhausted,
    Internal,
}

impl RejectionCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PermissionDenied => "permission_denied",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::RevisionConflict => "revision_conflict",
            Self::InvalidCommand => "invalid_command",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::Unavailable => "unavailable",
            Self::ResourceExhausted => "resource_exhausted",
            Self::Internal => "internal",
        }
    }
}

impl fmt::Display for RejectionCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldIssue {
    pub field: String,
    pub code: String,
    pub message: String,
}

impl FieldIssue {
    #[must_use]
    pub fn new(
        field: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            field: field.into(),
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rejection {
    pub code: RejectionCode,
    pub message: String,
    pub fields: Vec<FieldIssue>,
    pub retryable: bool,
}

impl Rejection {
    #[must_use]
    pub fn new(code: RejectionCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            fields: Vec::new(),
            retryable: false,
        }
    }

    #[must_use]
    pub fn with_field(mut self, issue: FieldIssue) -> Self {
        self.fields.push(issue);
        self
    }

    #[must_use]
    pub const fn retryable(mut self, value: bool) -> Self {
        self.retryable = value;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedReceipt<R> {
    pub revision: Revision,
    pub result: R,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RejectedReceipt {
    pub rejection: Rejection,
    pub current_revision: Revision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandReceipt<R> {
    Accepted {
        command_id: CommandId,
        acceptance: AcceptedReceipt<R>,
    },
    Rejected {
        command_id: CommandId,
        rejection: RejectedReceipt,
    },
}

impl<R> CommandReceipt<R> {
    #[must_use]
    pub const fn command_id(&self) -> &CommandId {
        match self {
            Self::Accepted { command_id, .. } | Self::Rejected { command_id, .. } => command_id,
        }
    }

    #[must_use]
    pub const fn accepted(&self) -> Option<&AcceptedReceipt<R>> {
        match self {
            Self::Accepted { acceptance, .. } => Some(acceptance),
            Self::Rejected { .. } => None,
        }
    }

    #[must_use]
    pub const fn rejected(&self) -> Option<&RejectedReceipt> {
        match self {
            Self::Accepted { .. } => None,
            Self::Rejected { rejection, .. } => Some(rejection),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejection_codes_are_stable_snake_case_values() {
        assert_eq!(
            RejectionCode::RevisionConflict.as_str(),
            "revision_conflict"
        );
        assert_eq!(
            RejectionCode::DeadlineExceeded.to_string(),
            "deadline_exceeded"
        );
    }

    #[test]
    fn deadline_allows_work_at_the_exact_boundary() {
        let deadline = Deadline::from_millis(100);
        assert!(!deadline.is_exceeded_at(100));
        assert!(deadline.is_exceeded_at(101));
    }
}
