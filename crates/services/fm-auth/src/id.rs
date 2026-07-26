use std::{error::Error, fmt, str::FromStr};

macro_rules! identity {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            /// Validates a stable identity.
            ///
            /// # Errors
            ///
            /// Returns [`InvalidIdentity`] when the value is empty or contains
            /// unsupported characters.
            pub fn new(value: impl Into<String>) -> Result<Self, InvalidIdentity> {
                let value = value.into();
                validate(&value)?;
                Ok(Self(value))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = InvalidIdentity;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }
    };
}

identity!(UserId, "A stable authenticated user identifier.");
identity!(SessionId, "A stable authenticated session identifier.");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidIdentity {
    Empty,
    InvalidStart { found: char },
    InvalidCharacter { found: char },
}

impl fmt::Display for InvalidIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("identity is empty"),
            Self::InvalidStart { found } => {
                write!(
                    formatter,
                    "identity starts with invalid character `{found}`"
                )
            }
            Self::InvalidCharacter { found } => {
                write!(formatter, "identity contains invalid character `{found}`")
            }
        }
    }
}

impl Error for InvalidIdentity {}

fn validate(value: &str) -> Result<(), InvalidIdentity> {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return Err(InvalidIdentity::Empty);
    };
    if !first.is_ascii_lowercase() {
        return Err(InvalidIdentity::InvalidStart { found: first });
    }
    if let Some(found) = characters.find(|character| {
        !character.is_ascii_lowercase()
            && !character.is_ascii_digit()
            && *character != '_'
            && *character != '-'
    }) {
        return Err(InvalidIdentity::InvalidCharacter { found });
    }
    Ok(())
}
