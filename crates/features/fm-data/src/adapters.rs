use std::{collections::BTreeMap, fmt};

use crate::DataValue;

/// Converts an adapter-specific representation into typed data.
pub trait DataAdapter {
    /// Produces the adapter's typed representation.
    ///
    /// # Errors
    ///
    /// Returns an error when the in-memory representation is malformed.
    fn representation(&self) -> Result<DataValue, AdapterError>;
}

/// An in-memory adapter representation is malformed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdapterError {
    DuplicateHeader(String),
    RowWidth {
        row: usize,
        expected: usize,
        actual: usize,
    },
    ExpectedObject,
}

impl fmt::Display for AdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateHeader(header) => write!(formatter, "duplicate CSV header `{header}`"),
            Self::RowWidth {
                row,
                expected,
                actual,
            } => write!(
                formatter,
                "CSV row {row} has {actual} columns; expected {expected}"
            ),
            Self::ExpectedObject => formatter.write_str("JSON-like adapter root must be an object"),
        }
    }
}

impl std::error::Error for AdapterError {}

/// CSV-like headers and rows. Parsing and I/O are intentionally out of scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CsvRowsAdapter {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl CsvRowsAdapter {
    #[must_use]
    pub fn new(headers: Vec<String>, rows: Vec<Vec<String>>) -> Self {
        Self { headers, rows }
    }
}

impl DataAdapter for CsvRowsAdapter {
    fn representation(&self) -> Result<DataValue, AdapterError> {
        let mut seen = BTreeMap::new();
        for header in &self.headers {
            if seen.insert(header, ()).is_some() {
                return Err(AdapterError::DuplicateHeader(header.clone()));
            }
        }
        let mut output = Vec::with_capacity(self.rows.len());
        for (index, row) in self.rows.iter().enumerate() {
            if row.len() != self.headers.len() {
                return Err(AdapterError::RowWidth {
                    row: index,
                    expected: self.headers.len(),
                    actual: row.len(),
                });
            }
            let object = self
                .headers
                .iter()
                .cloned()
                .zip(row.iter().cloned().map(DataValue::String))
                .collect();
            output.push(DataValue::Object(object));
        }
        Ok(DataValue::List(output))
    }
}

/// A pre-parsed JSON-like object representation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonObjectAdapter {
    object: BTreeMap<String, DataValue>,
}

impl JsonObjectAdapter {
    #[must_use]
    pub fn new(object: BTreeMap<String, DataValue>) -> Self {
        Self { object }
    }

    /// Builds an adapter from an object value.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::ExpectedObject`] for any non-object value.
    pub fn try_from_value(value: DataValue) -> Result<Self, AdapterError> {
        let DataValue::Object(object) = value else {
            return Err(AdapterError::ExpectedObject);
        };
        Ok(Self { object })
    }

    #[must_use]
    pub const fn object(&self) -> &BTreeMap<String, DataValue> {
        &self.object
    }
}

impl DataAdapter for JsonObjectAdapter {
    fn representation(&self) -> Result<DataValue, AdapterError> {
        Ok(DataValue::Object(self.object.clone()))
    }
}
