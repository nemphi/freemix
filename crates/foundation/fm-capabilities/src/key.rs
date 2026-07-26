use std::{error::Error, fmt, str::FromStr};

/// A validated lowercase identifier used by provider-neutral descriptors.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StableId(String);

impl StableId {
    /// Validates an identifier such as `video.raw` or `dmabuf`.
    ///
    /// Segments start with an ASCII lowercase letter and may subsequently
    /// contain lowercase letters, digits, `_`, or `-`.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidKey`] when the identifier is empty or malformed.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidKey> {
        let value = value.into();
        validate(&value, false)?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StableId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for StableId {
    type Err = InvalidKey;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

/// A validated hierarchical capability key.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CapabilityKey(StableId);

impl CapabilityKey {
    /// Validates a stable key containing at least two dot-separated segments.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidKey`] when the key is not hierarchical or a segment is
    /// malformed.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidKey> {
        let value = value.into();
        validate(&value, true)?;
        Ok(Self(StableId(value)))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for CapabilityKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for CapabilityKey {
    type Err = InvalidKey;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

/// Explains why a stable identifier was rejected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InvalidKey {
    Empty,
    NotHierarchical,
    EmptySegment { segment: usize },
    InvalidSegmentStart { segment: usize, found: char },
    InvalidCharacter { segment: usize, found: char },
}

impl fmt::Display for InvalidKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("key is empty"),
            Self::NotHierarchical => {
                formatter.write_str("capability key must contain at least two segments")
            }
            Self::EmptySegment { segment } => write!(formatter, "segment {segment} is empty"),
            Self::InvalidSegmentStart { segment, found } => {
                write!(
                    formatter,
                    "segment {segment} starts with invalid character `{found}`"
                )
            }
            Self::InvalidCharacter { segment, found } => {
                write!(
                    formatter,
                    "segment {segment} contains invalid character `{found}`"
                )
            }
        }
    }
}

impl Error for InvalidKey {}

fn validate(value: &str, hierarchical: bool) -> Result<(), InvalidKey> {
    if value.is_empty() {
        return Err(InvalidKey::Empty);
    }
    if hierarchical && !value.contains('.') {
        return Err(InvalidKey::NotHierarchical);
    }

    for (segment_index, segment) in value.split('.').enumerate() {
        if segment.is_empty() {
            return Err(InvalidKey::EmptySegment {
                segment: segment_index,
            });
        }

        let mut chars = segment.chars();
        let first = chars.next().expect("non-empty segment");
        if !first.is_ascii_lowercase() {
            return Err(InvalidKey::InvalidSegmentStart {
                segment: segment_index,
                found: first,
            });
        }
        if let Some(found) = chars.find(|character| {
            !character.is_ascii_lowercase()
                && !character.is_ascii_digit()
                && *character != '_'
                && *character != '-'
        }) {
            return Err(InvalidKey::InvalidCharacter {
                segment: segment_index,
                found,
            });
        }
    }
    Ok(())
}
