use std::{error::Error, fmt, str::FromStr};

macro_rules! graph_id {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            /// Validates a stable graph identifier.
            ///
            /// # Errors
            ///
            /// Returns [`InvalidGraphId`] when the identifier is empty or
            /// contains unsupported characters.
            pub fn new(value: impl Into<String>) -> Result<Self, InvalidGraphId> {
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
            type Err = InvalidGraphId;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }
    };
}

graph_id!(NodeId, "A stable node identifier within an editable graph.");
graph_id!(PortId, "A stable port identifier local to a node.");

/// Explains why a node or port identifier was rejected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InvalidGraphId {
    Empty,
    InvalidStart { found: char },
    InvalidCharacter { found: char },
}

impl fmt::Display for InvalidGraphId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("graph identifier is empty"),
            Self::InvalidStart { found } => {
                write!(
                    formatter,
                    "graph identifier starts with invalid character `{found}`"
                )
            }
            Self::InvalidCharacter { found } => {
                write!(
                    formatter,
                    "graph identifier contains invalid character `{found}`"
                )
            }
        }
    }
}

impl Error for InvalidGraphId {}

fn validate(value: &str) -> Result<(), InvalidGraphId> {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return Err(InvalidGraphId::Empty);
    };
    if !first.is_ascii_lowercase() {
        return Err(InvalidGraphId::InvalidStart { found: first });
    }
    if let Some(found) = characters.find(|character| {
        !character.is_ascii_lowercase()
            && !character.is_ascii_digit()
            && *character != '_'
            && *character != '-'
    }) {
        return Err(InvalidGraphId::InvalidCharacter { found });
    }
    Ok(())
}
