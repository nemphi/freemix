use std::{error::Error, fmt, net::IpAddr};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerMode {
    Development,
    Production,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthenticationMode {
    Required,
    Development,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RateLimit {
    pub maximum: u64,
    pub window_ms: u64,
}

impl RateLimit {
    #[must_use]
    pub const fn new(maximum: u64, window_ms: u64) -> Self {
        Self { maximum, window_ms }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionLimits {
    pub max_command_bytes: usize,
    pub max_inflight_commands: usize,
    pub inbound_commands: RateLimit,
    pub max_outbound_messages: usize,
    pub max_outbound_bytes: usize,
    pub outbound_messages: RateLimit,
    pub heartbeat_timeout_ms: u64,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            max_command_bytes: 64 * 1024,
            max_inflight_commands: 32,
            inbound_commands: RateLimit::new(100, 1_000),
            max_outbound_messages: 256,
            max_outbound_bytes: 4 * 1024 * 1024,
            outbound_messages: RateLimit::new(1_000, 1_000),
            heartbeat_timeout_ms: 15_000,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerConfig {
    pub mode: ServerMode,
    pub authentication: AuthenticationMode,
    pub bind_address: IpAddr,
    pub capabilities_digest: String,
    pub session_limits: SessionLimits,
}

impl ServerConfig {
    #[must_use]
    pub fn new(
        mode: ServerMode,
        authentication: AuthenticationMode,
        bind_address: IpAddr,
        capabilities_digest: impl Into<String>,
    ) -> Self {
        Self {
            mode,
            authentication,
            bind_address,
            capabilities_digest: capabilities_digest.into(),
            session_limits: SessionLimits::default(),
        }
    }

    #[must_use]
    pub fn with_session_limits(mut self, limits: SessionLimits) -> Self {
        self.session_limits = limits;
        self
    }

    /// Validates security invariants and bounded session limits.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for unsafe development authentication,
    /// or a zero-valued limit.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.authentication == AuthenticationMode::Development {
            if self.mode == ServerMode::Production {
                return Err(ConfigError::DevelopmentAuthInProduction);
            }
            if !self.bind_address.is_loopback() {
                return Err(ConfigError::DevelopmentAuthRequiresLoopback);
            }
        }
        let limits = &self.session_limits;
        for (name, value) in [
            ("max_command_bytes", limits.max_command_bytes),
            ("max_inflight_commands", limits.max_inflight_commands),
            ("max_outbound_messages", limits.max_outbound_messages),
            ("max_outbound_bytes", limits.max_outbound_bytes),
        ] {
            if value == 0 {
                return Err(ConfigError::ZeroLimit(name));
            }
        }
        for (name, limit) in [
            ("inbound_commands", limits.inbound_commands),
            ("outbound_messages", limits.outbound_messages),
        ] {
            if limit.maximum == 0 || limit.window_ms == 0 {
                return Err(ConfigError::ZeroLimit(name));
            }
        }
        if limits.heartbeat_timeout_ms == 0 {
            return Err(ConfigError::ZeroLimit("heartbeat_timeout_ms"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigError {
    DevelopmentAuthInProduction,
    DevelopmentAuthRequiresLoopback,
    ZeroLimit(&'static str),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DevelopmentAuthInProduction => {
                formatter.write_str("development authentication is forbidden in production")
            }
            Self::DevelopmentAuthRequiresLoopback => {
                formatter.write_str("development authentication requires a loopback bind address")
            }
            Self::ZeroLimit(name) => write!(formatter, "session limit `{name}` must be nonzero"),
        }
    }
}

impl Error for ConfigError {}
