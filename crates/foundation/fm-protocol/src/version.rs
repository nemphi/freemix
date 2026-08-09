use core::fmt;

/// The only protocol accepted by a current-development client or server.
pub const CURRENT_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::new(2, 2);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    #[must_use]
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.major, self.minor)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NegotiationError;

impl fmt::Display for NegotiationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("client and server do not share the current protocol contract")
    }
}

impl std::error::Error for NegotiationError {}

/// Accepts only the exact current-development protocol contract.
///
/// # Errors
///
/// Returns [`NegotiationError`] unless both peers advertise the exact current version.
pub fn negotiate_version(
    client: &[ProtocolVersion],
    server: &[ProtocolVersion],
) -> Result<ProtocolVersion, NegotiationError> {
    client
        .iter()
        .copied()
        .filter(|version| *version == CURRENT_PROTOCOL_VERSION)
        .filter(|version| server.contains(version))
        .max()
        .ok_or(NegotiationError)
}
