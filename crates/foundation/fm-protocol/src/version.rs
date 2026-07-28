use core::fmt;

pub const BASE_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::new(1, 0);
pub const WIPE_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::new(1, 3);
pub const MANUAL_TRANSITION_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::new(1, 4);
pub const CURRENT_PROTOCOL_VERSION: ProtocolVersion = MANUAL_TRANSITION_PROTOCOL_VERSION;

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
        formatter.write_str("client and server have no compatible protocol major")
    }
}

impl std::error::Error for NegotiationError {}

/// Selects the newest shared major and the lower minor implemented by both
/// peers for that major.
///
/// # Errors
///
/// Returns [`NegotiationError`] when no major appears in both sets.
pub fn negotiate_version(
    client: &[ProtocolVersion],
    server: &[ProtocolVersion],
) -> Result<ProtocolVersion, NegotiationError> {
    client
        .iter()
        .flat_map(|client| {
            server
                .iter()
                .filter(move |server| server.major == client.major)
                .map(move |server| {
                    ProtocolVersion::new(client.major, client.minor.min(server.minor))
                })
        })
        .max()
        .ok_or(NegotiationError)
}
