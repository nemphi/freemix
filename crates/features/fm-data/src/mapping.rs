use std::{collections::BTreeMap, fmt};

use crate::{DataPath, DataValue, PathError, Transform, TransformError, ValueType};

/// Maps one source path into one named output field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldBinding {
    pub field: String,
    pub source: DataPath,
    pub transforms: Vec<Transform>,
    pub expected: Option<ValueType>,
    pub required: bool,
}

impl FieldBinding {
    #[must_use]
    pub fn new(field: impl Into<String>, source: DataPath) -> Self {
        Self {
            field: field.into(),
            source,
            transforms: Vec::new(),
            expected: None,
            required: true,
        }
    }
}

/// The outcome for one field binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MappingStatus {
    Mapped,
    Missing,
    TypeError {
        expected: ValueType,
        actual: ValueType,
    },
    PathError(PathError),
    TransformError(TransformError),
}

/// Report entry for one field, in binding declaration order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldMappingReport {
    pub field: String,
    pub status: MappingStatus,
}

/// Mapping output and complete per-field diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MappingReport {
    pub output: BTreeMap<String, DataValue>,
    pub fields: Vec<FieldMappingReport>,
}

impl MappingReport {
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.fields
            .iter()
            .all(|field| field.status == MappingStatus::Mapped)
    }

    pub fn errors(&self) -> impl Iterator<Item = &FieldMappingReport> {
        self.fields
            .iter()
            .filter(|field| field.status != MappingStatus::Mapped)
    }
}

/// Applies field bindings in deterministic declaration order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Mapper {
    bindings: Vec<FieldBinding>,
}

impl Mapper {
    #[must_use]
    pub fn new(bindings: Vec<FieldBinding>) -> Self {
        Self { bindings }
    }

    #[must_use]
    pub fn bindings(&self) -> &[FieldBinding] {
        &self.bindings
    }

    #[must_use]
    pub fn map(&self, root: &DataValue) -> MappingReport {
        let mut output = BTreeMap::new();
        let mut reports = Vec::with_capacity(self.bindings.len());
        for binding in &self.bindings {
            let result = map_field(binding, root);
            let status = match result {
                Ok(Some(value)) => {
                    output.insert(binding.field.clone(), value);
                    MappingStatus::Mapped
                }
                Ok(None) => MappingStatus::Missing,
                Err(error) => error,
            };
            reports.push(FieldMappingReport {
                field: binding.field.clone(),
                status,
            });
        }
        MappingReport {
            output,
            fields: reports,
        }
    }
}

fn map_field(binding: &FieldBinding, root: &DataValue) -> Result<Option<DataValue>, MappingStatus> {
    let mut value = match binding.source.extract(root) {
        Ok(value) => value.clone(),
        Err(PathError::MissingField { .. } | PathError::MissingIndex { .. })
            if binding
                .transforms
                .iter()
                .any(|transform| matches!(transform, Transform::Fallback(_))) =>
        {
            DataValue::Null
        }
        Err(PathError::MissingField { .. } | PathError::MissingIndex { .. })
            if !binding.required =>
        {
            return Ok(None);
        }
        Err(PathError::MissingField { .. } | PathError::MissingIndex { .. }) => {
            return Err(MappingStatus::Missing);
        }
        Err(error) => return Err(MappingStatus::PathError(error)),
    };
    for transform in &binding.transforms {
        value = transform
            .apply(value, root)
            .map_err(MappingStatus::TransformError)?;
    }
    if value.is_null() {
        return if binding.required {
            Err(MappingStatus::Missing)
        } else {
            Ok(None)
        };
    }
    if let Some(expected) = binding.expected
        && value.value_type() != expected
    {
        return Err(MappingStatus::TypeError {
            expected,
            actual: value.value_type(),
        });
    }
    Ok(Some(value))
}

impl fmt::Display for MappingStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mapped => formatter.write_str("mapped"),
            Self::Missing => formatter.write_str("missing"),
            Self::TypeError { expected, actual } => {
                write!(formatter, "expected {expected:?}, found {actual:?}")
            }
            Self::PathError(error) => error.fmt(formatter),
            Self::TransformError(error) => error.fmt(formatter),
        }
    }
}
