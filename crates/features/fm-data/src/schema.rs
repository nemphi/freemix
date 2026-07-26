use std::{collections::BTreeMap, fmt, str::FromStr};

use crate::{DataValue, ValueType};

/// One step in a nested data path.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PathSegment {
    Field(String),
    Index(usize),
}

/// A deterministic object/list path such as `players[0].name`.
#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct DataPath(Vec<PathSegment>);

/// A path is malformed or cannot be followed through a value/schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PathError {
    Invalid(String),
    MissingField { path: DataPath, field: String },
    MissingIndex { path: DataPath, index: usize },
    ExpectedObject { path: DataPath, actual: ValueType },
    ExpectedList { path: DataPath, actual: ValueType },
}

impl fmt::Display for PathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(path) => write!(formatter, "invalid data path: {path}"),
            Self::MissingField { path, field } => {
                write!(formatter, "field `{field}` is missing at `{path}`")
            }
            Self::MissingIndex { path, index } => {
                write!(formatter, "index {index} is missing at `{path}`")
            }
            Self::ExpectedObject { path, actual } => {
                write!(formatter, "expected object at `{path}`, found {actual:?}")
            }
            Self::ExpectedList { path, actual } => {
                write!(formatter, "expected list at `{path}`, found {actual:?}")
            }
        }
    }
}

impl std::error::Error for PathError {}

impl DataPath {
    #[must_use]
    pub fn root() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn new(segments: impl IntoIterator<Item = PathSegment>) -> Self {
        Self(segments.into_iter().collect())
    }

    #[must_use]
    pub fn segments(&self) -> &[PathSegment] {
        &self.0
    }

    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn field(mut self, field: impl Into<String>) -> Self {
        self.0.push(PathSegment::Field(field.into()));
        self
    }

    #[must_use]
    pub fn index(mut self, index: usize) -> Self {
        self.0.push(PathSegment::Index(index));
        self
    }

    /// Extracts a borrowed value without coercion.
    ///
    /// # Errors
    ///
    /// Returns a path error when a segment is missing or traverses the wrong type.
    pub fn extract<'a>(&self, root: &'a DataValue) -> Result<&'a DataValue, PathError> {
        let mut value = root;
        let mut traversed = Self::root();
        for segment in &self.0 {
            match segment {
                PathSegment::Field(field) => {
                    let DataValue::Object(object) = value else {
                        return Err(PathError::ExpectedObject {
                            path: traversed,
                            actual: value.value_type(),
                        });
                    };
                    value = object.get(field).ok_or_else(|| PathError::MissingField {
                        path: traversed.clone(),
                        field: field.clone(),
                    })?;
                }
                PathSegment::Index(index) => {
                    let DataValue::List(list) = value else {
                        return Err(PathError::ExpectedList {
                            path: traversed,
                            actual: value.value_type(),
                        });
                    };
                    value = list.get(*index).ok_or_else(|| PathError::MissingIndex {
                        path: traversed.clone(),
                        index: *index,
                    })?;
                }
            }
            traversed.0.push(segment.clone());
        }
        Ok(value)
    }
}

impl FromStr for DataPath {
    type Err = PathError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if input.is_empty() {
            return Ok(Self::root());
        }
        let bytes = input.as_bytes();
        let mut segments = Vec::new();
        let mut cursor = 0;
        while cursor < bytes.len() {
            if bytes[cursor] == b'.' {
                return Err(PathError::Invalid(input.to_owned()));
            }
            if bytes[cursor] == b'[' {
                cursor += 1;
                let start = cursor;
                while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
                    cursor += 1;
                }
                if start == cursor || cursor >= bytes.len() || bytes[cursor] != b']' {
                    return Err(PathError::Invalid(input.to_owned()));
                }
                let index = input[start..cursor]
                    .parse()
                    .map_err(|_| PathError::Invalid(input.to_owned()))?;
                segments.push(PathSegment::Index(index));
                cursor += 1;
            } else {
                let start = cursor;
                while cursor < bytes.len() && bytes[cursor] != b'.' && bytes[cursor] != b'[' {
                    cursor += 1;
                }
                segments.push(PathSegment::Field(input[start..cursor].to_owned()));
            }
            if cursor < bytes.len() {
                if bytes[cursor] == b'[' {
                    continue;
                }
                if bytes[cursor] != b'.' || cursor + 1 == bytes.len() || bytes[cursor + 1] == b'[' {
                    return Err(PathError::Invalid(input.to_owned()));
                }
                cursor += 1;
            }
        }
        Ok(Self(segments))
    }
}

impl fmt::Display for DataPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return formatter.write_str("$");
        }
        let mut wrote_segment = false;
        for segment in &self.0 {
            match segment {
                PathSegment::Field(field) => {
                    if wrote_segment {
                        formatter.write_str(".")?;
                    }
                    formatter.write_str(field)?;
                }
                PathSegment::Index(index) => write!(formatter, "[{index}]")?,
            }
            wrote_segment = true;
        }
        Ok(())
    }
}

/// A recursively typed schema node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchemaType {
    Any,
    Null,
    Bool,
    Integer,
    Decimal,
    String,
    List(Box<Self>),
    Object(BTreeMap<String, Self>),
}

impl SchemaType {
    #[must_use]
    pub const fn expected_type(&self) -> Option<ValueType> {
        match self {
            Self::Any => None,
            Self::Null => Some(ValueType::Null),
            Self::Bool => Some(ValueType::Bool),
            Self::Integer => Some(ValueType::Integer),
            Self::Decimal => Some(ValueType::Decimal),
            Self::String => Some(ValueType::String),
            Self::List(_) => Some(ValueType::List),
            Self::Object(_) => Some(ValueType::Object),
        }
    }
}

/// A value does not match its declared schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TypeError {
    pub path: DataPath,
    pub expected: ValueType,
    pub actual: ValueType,
}

impl fmt::Display for TypeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "expected {:?} at `{}`, found {:?}",
            self.expected, self.path, self.actual
        )
    }
}

impl std::error::Error for TypeError {}

/// A root schema used to validate and extract typed values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Schema {
    root: SchemaType,
}

impl Schema {
    #[must_use]
    pub const fn new(root: SchemaType) -> Self {
        Self { root }
    }

    #[must_use]
    pub const fn root(&self) -> &SchemaType {
        &self.root
    }

    /// Validates all declared fields and list items. Extra object fields are allowed.
    ///
    /// # Errors
    ///
    /// Returns the first deterministic schema mismatch in path order.
    pub fn validate(&self, value: &DataValue) -> Result<(), TypeError> {
        validate_node(&self.root, value, &DataPath::root())
    }

    /// Extracts a value and verifies it against the schema node at the same path.
    ///
    /// # Errors
    ///
    /// Returns a path error for unresolved segments or a type error for a schema mismatch.
    pub fn extract<'a>(
        &self,
        value: &'a DataValue,
        path: &DataPath,
    ) -> Result<&'a DataValue, SchemaExtractError> {
        let schema_node = schema_at(&self.root, path)?;
        let extracted = path.extract(value)?;
        validate_node(schema_node, extracted, path)?;
        Ok(extracted)
    }
}

/// Failure while resolving both a schema path and its value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchemaExtractError {
    Path(PathError),
    Type(TypeError),
}

impl fmt::Display for SchemaExtractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(error) => error.fmt(formatter),
            Self::Type(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for SchemaExtractError {}

impl From<PathError> for SchemaExtractError {
    fn from(value: PathError) -> Self {
        Self::Path(value)
    }
}

impl From<TypeError> for SchemaExtractError {
    fn from(value: TypeError) -> Self {
        Self::Type(value)
    }
}

fn schema_at<'a>(root: &'a SchemaType, path: &DataPath) -> Result<&'a SchemaType, PathError> {
    let mut node = root;
    let mut traversed = DataPath::root();
    for segment in path.segments() {
        node = match (node, segment) {
            (SchemaType::Object(fields), PathSegment::Field(field)) => {
                fields.get(field).ok_or_else(|| PathError::MissingField {
                    path: traversed.clone(),
                    field: field.clone(),
                })?
            }
            (SchemaType::List(item), PathSegment::Index(_)) => item,
            (node, PathSegment::Field(_)) => {
                return Err(PathError::ExpectedObject {
                    path: traversed,
                    actual: node.expected_type().unwrap_or(ValueType::Null),
                });
            }
            (node, PathSegment::Index(_)) => {
                return Err(PathError::ExpectedList {
                    path: traversed,
                    actual: node.expected_type().unwrap_or(ValueType::Null),
                });
            }
        };
        traversed.0.push(segment.clone());
    }
    Ok(node)
}

fn validate_node(schema: &SchemaType, value: &DataValue, path: &DataPath) -> Result<(), TypeError> {
    if let Some(expected) = schema.expected_type()
        && value.value_type() != expected
    {
        return Err(TypeError {
            path: path.clone(),
            expected,
            actual: value.value_type(),
        });
    }
    match (schema, value) {
        (SchemaType::List(item), DataValue::List(values)) => {
            for (index, value) in values.iter().enumerate() {
                validate_node(item, value, &path.clone().index(index))?;
            }
        }
        (SchemaType::Object(fields), DataValue::Object(values)) => {
            for (field, schema) in fields {
                let field_path = path.clone().field(field);
                let value = values.get(field).ok_or_else(|| TypeError {
                    path: field_path.clone(),
                    expected: schema.expected_type().unwrap_or(ValueType::Null),
                    actual: ValueType::Null,
                })?;
                validate_node(schema, value, &field_path)?;
            }
        }
        _ => {}
    }
    Ok(())
}
