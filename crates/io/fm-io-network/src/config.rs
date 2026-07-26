use core::fmt;
use std::num::NonZeroU16;

use fm_codec_api::QueueCapacity;

pub const MAX_DESTINATIONS: usize = 5;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DestinationId(u8);

impl DestinationId {
    /// Creates one of the five independently controlled destination slots.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidDestinationId`] unless `value` is 1..=5.
    pub const fn new(value: u8) -> Result<Self, ConfigError> {
        if value == 0 || value as usize > MAX_DESTINATIONS {
            Err(ConfigError::InvalidDestinationId(value))
        } else {
            Ok(Self(value))
        }
    }

    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl fmt::Display for DestinationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OutputProtocol {
    Rtmp,
    Rtmps,
    Hls,
    LiveLan,
}

impl OutputProtocol {
    #[must_use]
    pub const fn requires_tls(self) -> bool {
        matches!(self, Self::Rtmps)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CredentialReference(String);

impl CredentialReference {
    pub const MAX_LENGTH: usize = 512;

    /// Creates an opaque lookup reference, never secret material itself.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, whitespace-padded, or control-containing values.
    pub fn new(value: impl Into<String>) -> Result<Self, ConfigError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ConfigError::EmptyCredentialReference);
        }
        if value.len() > Self::MAX_LENGTH {
            return Err(ConfigError::CredentialReferenceTooLong);
        }
        if value.trim() != value || value.chars().any(char::is_control) {
            return Err(ConfigError::InvalidCredentialReference);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Endpoint {
    host: String,
    port: NonZeroU16,
    path: String,
}

impl Endpoint {
    /// Creates a structured endpoint without resolving it or opening a socket.
    ///
    /// # Errors
    ///
    /// Rejects invalid host, port, or absolute-path values.
    pub fn new(
        host: impl Into<String>,
        port: u16,
        path: impl Into<String>,
    ) -> Result<Self, ConfigError> {
        let host = host.into();
        let path = path.into();
        if host.is_empty()
            || host.len() > 253
            || host.trim() != host
            || host.chars().any(char::is_control)
            || host.contains('/')
        {
            return Err(ConfigError::InvalidHost);
        }
        let port = NonZeroU16::new(port).ok_or(ConfigError::InvalidPort)?;
        if path.is_empty()
            || path.len() > 2_048
            || !path.starts_with('/')
            || path.chars().any(char::is_control)
        {
            return Err(ConfigError::InvalidPath);
        }
        Ok(Self { host, port, path })
    }

    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    #[must_use]
    pub const fn port(&self) -> NonZeroU16 {
        self.port
    }

    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TlsMinimumVersion {
    Tls12,
    Tls13,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum TlsVerification {
    SystemRoots,
    PinnedCertificate(CredentialReference),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TlsConfig {
    server_name: Option<String>,
    minimum_version: TlsMinimumVersion,
    verification: TlsVerification,
    client_identity: Option<CredentialReference>,
}

impl TlsConfig {
    /// Creates strict TLS configuration. Certificate verification cannot be disabled.
    ///
    /// # Errors
    ///
    /// Rejects an invalid explicit server name.
    pub fn new(
        server_name: Option<String>,
        minimum_version: TlsMinimumVersion,
        verification: TlsVerification,
        client_identity: Option<CredentialReference>,
    ) -> Result<Self, ConfigError> {
        if server_name.as_ref().is_some_and(|name| {
            name.is_empty()
                || name.len() > 253
                || name.trim() != name
                || name.contains('/')
                || name.chars().any(char::is_control)
        }) {
            return Err(ConfigError::InvalidTlsServerName);
        }
        Ok(Self {
            server_name,
            minimum_version,
            verification,
            client_identity,
        })
    }

    /// Creates TLS 1.2-or-newer configuration using platform trust roots.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidTlsServerName`] for an invalid explicit name.
    pub fn system_roots(server_name: Option<String>) -> Result<Self, ConfigError> {
        Self::new(
            server_name,
            TlsMinimumVersion::Tls12,
            TlsVerification::SystemRoots,
            None,
        )
    }

    #[must_use]
    pub fn server_name(&self) -> Option<&str> {
        self.server_name.as_deref()
    }

    #[must_use]
    pub const fn minimum_version(&self) -> TlsMinimumVersion {
        self.minimum_version
    }

    #[must_use]
    pub const fn verification(&self) -> &TlsVerification {
        &self.verification
    }

    #[must_use]
    pub const fn client_identity(&self) -> Option<&CredentialReference> {
        self.client_identity.as_ref()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ReconnectPolicy {
    initial_delay_ms: u64,
    maximum_delay_ms: u64,
    multiplier: u32,
    maximum_attempts: Option<u32>,
}

impl ReconnectPolicy {
    /// Creates deterministic exponential backoff parameters.
    ///
    /// `maximum_attempts == None` retries indefinitely. A value of `Some(0)`
    /// disables retries.
    ///
    /// # Errors
    ///
    /// Rejects zero delays, a maximum below the initial delay, or a multiplier below two.
    pub const fn new(
        initial_delay_ms: u64,
        maximum_delay_ms: u64,
        multiplier: u32,
        maximum_attempts: Option<u32>,
    ) -> Result<Self, ConfigError> {
        if initial_delay_ms == 0 || maximum_delay_ms < initial_delay_ms {
            return Err(ConfigError::InvalidReconnectDelay);
        }
        if multiplier < 2 {
            return Err(ConfigError::InvalidReconnectMultiplier);
        }
        Ok(Self {
            initial_delay_ms,
            maximum_delay_ms,
            multiplier,
            maximum_attempts,
        })
    }

    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            initial_delay_ms: 1,
            maximum_delay_ms: 1,
            multiplier: 2,
            maximum_attempts: Some(0),
        }
    }

    #[must_use]
    pub const fn permits_attempt(self, attempt: u32) -> bool {
        match self.maximum_attempts {
            Some(maximum) => attempt <= maximum,
            None => true,
        }
    }

    #[must_use]
    pub fn delay_ms(self, attempt: u32) -> u64 {
        let exponent = attempt.saturating_sub(1);
        let mut delay = self.initial_delay_ms;
        for _ in 0..exponent {
            delay = delay
                .saturating_mul(u64::from(self.multiplier))
                .min(self.maximum_delay_ms);
        }
        delay
    }

    #[must_use]
    pub const fn maximum_attempts(self) -> Option<u32> {
        self.maximum_attempts
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DestinationConfig {
    id: DestinationId,
    protocol: OutputProtocol,
    endpoint: Endpoint,
    backup_endpoint: Option<Endpoint>,
    tls: Option<TlsConfig>,
    credential: Option<CredentialReference>,
    queue_capacity: QueueCapacity,
    reconnect: ReconnectPolicy,
}

impl DestinationConfig {
    /// Creates validated output configuration without retaining a secret value.
    ///
    /// # Errors
    ///
    /// RTMPS requires TLS and plain RTMP rejects contradictory TLS configuration.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: DestinationId,
        protocol: OutputProtocol,
        endpoint: Endpoint,
        backup_endpoint: Option<Endpoint>,
        tls: Option<TlsConfig>,
        credential: Option<CredentialReference>,
        queue_capacity: QueueCapacity,
        reconnect: ReconnectPolicy,
    ) -> Result<Self, ConfigError> {
        if protocol.requires_tls() && tls.is_none() {
            return Err(ConfigError::TlsRequired);
        }
        if protocol == OutputProtocol::Rtmp && tls.is_some() {
            return Err(ConfigError::TlsNotSupported);
        }
        Ok(Self {
            id,
            protocol,
            endpoint,
            backup_endpoint,
            tls,
            credential,
            queue_capacity,
            reconnect,
        })
    }

    #[must_use]
    pub const fn id(&self) -> DestinationId {
        self.id
    }

    #[must_use]
    pub const fn protocol(&self) -> OutputProtocol {
        self.protocol
    }

    #[must_use]
    pub const fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    #[must_use]
    pub const fn backup_endpoint(&self) -> Option<&Endpoint> {
        self.backup_endpoint.as_ref()
    }

    #[must_use]
    pub const fn tls(&self) -> Option<&TlsConfig> {
        self.tls.as_ref()
    }

    #[must_use]
    pub const fn credential(&self) -> Option<&CredentialReference> {
        self.credential.as_ref()
    }

    #[must_use]
    pub const fn queue_capacity(&self) -> QueueCapacity {
        self.queue_capacity
    }

    #[must_use]
    pub const fn reconnect(&self) -> ReconnectPolicy {
        self.reconnect
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigError {
    InvalidDestinationId(u8),
    EmptyCredentialReference,
    CredentialReferenceTooLong,
    InvalidCredentialReference,
    InvalidHost,
    InvalidPort,
    InvalidPath,
    InvalidTlsServerName,
    TlsRequired,
    TlsNotSupported,
    InvalidReconnectDelay,
    InvalidReconnectMultiplier,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDestinationId(value) => {
                write!(formatter, "destination id {value} is outside 1..=5")
            }
            Self::EmptyCredentialReference => formatter.write_str("credential reference is empty"),
            Self::CredentialReferenceTooLong => {
                formatter.write_str("credential reference is too long")
            }
            Self::InvalidCredentialReference => {
                formatter.write_str("credential reference is invalid")
            }
            Self::InvalidHost => formatter.write_str("endpoint host is invalid"),
            Self::InvalidPort => formatter.write_str("endpoint port must be nonzero"),
            Self::InvalidPath => formatter.write_str("endpoint path is invalid"),
            Self::InvalidTlsServerName => formatter.write_str("TLS server name is invalid"),
            Self::TlsRequired => formatter.write_str("this protocol requires TLS configuration"),
            Self::TlsNotSupported => formatter.write_str("plain RTMP cannot use TLS configuration"),
            Self::InvalidReconnectDelay => formatter.write_str("reconnect delay is invalid"),
            Self::InvalidReconnectMultiplier => {
                formatter.write_str("reconnect multiplier must be at least two")
            }
        }
    }
}

impl std::error::Error for ConfigError {}
