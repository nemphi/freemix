use std::fmt;

/// An opaque identifier resolved by an integration-layer secret store.
///
/// This type cannot contain a secret value. Its debug output is redacted so
/// derived debug output on surrounding source configurations remains safe.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SecretRefId(String);

impl SecretRefId {
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretRefId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretRefId([REDACTED])")
    }
}
