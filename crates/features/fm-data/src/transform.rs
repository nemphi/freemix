use std::fmt;

use crate::{DataPath, DataValue, PathError, ValueType};

/// Selects an input used by a transform.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValueSelector {
    Current,
    Path(DataPath),
    Literal(DataValue),
}

/// A typed, deterministic mapping transform.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Transform {
    /// Replaces every `{value}` marker with the scalar input.
    Format(String),
    /// Restricts a number while preserving integer or decimal type.
    Clamp { min: DataValue, max: DataValue },
    /// Joins string selectors in declaration order.
    Concatenate {
        parts: Vec<ValueSelector>,
        separator: String,
    },
    /// Replaces only `Null`; all other values pass through unchanged.
    Fallback(DataValue),
}

/// A transform received an incompatible value or invalid configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransformError {
    ExpectedScalar {
        actual: ValueType,
    },
    ExpectedString {
        actual: ValueType,
    },
    InvalidClampBounds,
    ClampTypeMismatch {
        value: ValueType,
        min: ValueType,
        max: ValueType,
    },
    Path(PathError),
}

impl fmt::Display for TransformError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExpectedScalar { actual } => {
                write!(formatter, "format requires a scalar, found {actual:?}")
            }
            Self::ExpectedString { actual } => {
                write!(formatter, "concatenate requires strings, found {actual:?}")
            }
            Self::InvalidClampBounds => formatter.write_str("clamp minimum exceeds maximum"),
            Self::ClampTypeMismatch { value, min, max } => write!(
                formatter,
                "clamp types differ: value={value:?}, min={min:?}, max={max:?}"
            ),
            Self::Path(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for TransformError {}

impl From<PathError> for TransformError {
    fn from(value: PathError) -> Self {
        Self::Path(value)
    }
}

impl Transform {
    /// Applies the transform. `root` is used only by path selectors.
    ///
    /// # Errors
    ///
    /// Returns an error for incompatible input types, invalid bounds, or unresolved paths.
    pub fn apply(&self, current: DataValue, root: &DataValue) -> Result<DataValue, TransformError> {
        match self {
            Self::Format(template) => {
                let text = current
                    .scalar_text()
                    .ok_or_else(|| TransformError::ExpectedScalar {
                        actual: current.value_type(),
                    })?;
                Ok(DataValue::String(template.replace("{value}", &text)))
            }
            Self::Clamp { min, max } => clamp(current, min, max),
            Self::Concatenate { parts, separator } => {
                let mut strings = Vec::with_capacity(parts.len());
                for part in parts {
                    let value = match part {
                        ValueSelector::Current => &current,
                        ValueSelector::Path(path) => path.extract(root)?,
                        ValueSelector::Literal(value) => value,
                    };
                    let DataValue::String(value) = value else {
                        return Err(TransformError::ExpectedString {
                            actual: value.value_type(),
                        });
                    };
                    strings.push(value.as_str());
                }
                Ok(DataValue::String(strings.join(separator)))
            }
            Self::Fallback(fallback) => {
                if current.is_null() {
                    Ok(fallback.clone())
                } else {
                    Ok(current)
                }
            }
        }
    }
}

fn clamp(value: DataValue, min: &DataValue, max: &DataValue) -> Result<DataValue, TransformError> {
    let value_type = value.value_type();
    let min_type = min.value_type();
    let max_type = max.value_type();
    match (value, min, max) {
        (DataValue::Integer(value), DataValue::Integer(min), DataValue::Integer(max)) => {
            if min > max {
                return Err(TransformError::InvalidClampBounds);
            }
            Ok(DataValue::Integer(value.clamp(*min, *max)))
        }
        (DataValue::Decimal(value), DataValue::Decimal(min), DataValue::Decimal(max)) => {
            if min > max {
                return Err(TransformError::InvalidClampBounds);
            }
            Ok(DataValue::Decimal(value.clamp(*min, *max)))
        }
        _ => Err(TransformError::ClampTypeMismatch {
            value: value_type,
            min: min_type,
            max: max_type,
        }),
    }
}
