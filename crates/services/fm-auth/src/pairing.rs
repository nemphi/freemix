use std::{error::Error, fmt, num::NonZeroU64};

/// Maximum accepted pairing-code lifetime: ten minutes.
pub const MAX_PAIRING_LIFETIME_SECONDS: u64 = 600;

/// Caller-supplied opaque pairing secret.
#[derive(Clone, Eq, PartialEq)]
pub struct PairingCode(String);

impl PairingCode {
    /// Wraps an externally generated opaque code.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidPairingCode`] when the value is empty.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidPairingCode> {
        let value = value.into();
        if value.is_empty() {
            Err(InvalidPairingCode)
        } else {
            Ok(Self(value))
        }
    }

    fn matches(&self, candidate: &Self) -> bool {
        if self.0.len() != candidate.0.len() {
            return false;
        }
        self.0
            .bytes()
            .zip(candidate.0.bytes())
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            })
            == 0
    }
}

impl fmt::Debug for PairingCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PairingCode([redacted])")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidPairingCode;

impl fmt::Display for InvalidPairingCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("pairing code is empty")
    }
}

impl Error for InvalidPairingCode {}

/// One-use pairing-code validator with caller-injected time and secret.
pub struct PairingCodeValidator {
    expected: PairingCode,
    expires_at: u64,
    consumed: bool,
}

impl PairingCodeValidator {
    /// Creates a validator without reading clocks or generating randomness.
    ///
    /// # Errors
    ///
    /// Returns [`PairingConfigurationError`] when the lifetime is longer than
    /// ten minutes or the expiry timestamp overflows.
    pub fn new(
        expected: PairingCode,
        issued_at: u64,
        lifetime_seconds: NonZeroU64,
    ) -> Result<Self, PairingConfigurationError> {
        if lifetime_seconds.get() > MAX_PAIRING_LIFETIME_SECONDS {
            return Err(PairingConfigurationError::LifetimeTooLong {
                provided: lifetime_seconds.get(),
                maximum: MAX_PAIRING_LIFETIME_SECONDS,
            });
        }
        let expires_at = issued_at
            .checked_add(lifetime_seconds.get())
            .ok_or(PairingConfigurationError::ExpiryOverflow)?;
        Ok(Self {
            expected,
            expires_at,
            consumed: false,
        })
    }

    #[must_use]
    pub const fn expires_at(&self) -> u64 {
        self.expires_at
    }

    #[must_use]
    pub const fn is_consumed(&self) -> bool {
        self.consumed
    }

    /// Validates and consumes the code at an injected timestamp.
    ///
    /// Invalid and expired attempts do not consume the code.
    ///
    /// # Errors
    ///
    /// Returns [`PairingError`] when already consumed, expired, or incorrect.
    pub fn consume(&mut self, candidate: &PairingCode, now: u64) -> Result<(), PairingError> {
        if self.consumed {
            return Err(PairingError::AlreadyConsumed);
        }
        if now >= self.expires_at {
            return Err(PairingError::Expired);
        }
        if !self.expected.matches(candidate) {
            return Err(PairingError::Invalid);
        }
        self.consumed = true;
        Ok(())
    }
}

impl fmt::Debug for PairingCodeValidator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PairingCodeValidator")
            .field("expected", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .field("consumed", &self.consumed)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PairingConfigurationError {
    LifetimeTooLong { provided: u64, maximum: u64 },
    ExpiryOverflow,
}

impl fmt::Display for PairingConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LifetimeTooLong { provided, maximum } => write!(
                formatter,
                "pairing lifetime {provided}s exceeds the {maximum}s maximum"
            ),
            Self::ExpiryOverflow => formatter.write_str("pairing expiry timestamp overflowed"),
        }
    }
}

impl Error for PairingConfigurationError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PairingError {
    Invalid,
    Expired,
    AlreadyConsumed,
}

impl fmt::Display for PairingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Invalid => "pairing code is invalid",
            Self::Expired => "pairing code has expired",
            Self::AlreadyConsumed => "pairing code was already consumed",
        })
    }
}

impl Error for PairingError {}
