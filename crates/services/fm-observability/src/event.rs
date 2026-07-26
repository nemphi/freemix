use std::{collections::VecDeque, error::Error, fmt};

use crate::Redactor;

/// Event importance, ordered from least to most severe.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Severity {
    Trace,
    Debug,
    Info,
    Warning,
    Error,
    Critical,
}

impl Severity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
            Self::Critical => "critical",
        }
    }
}

/// Stable operational event categories.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Category {
    Runtime,
    Media,
    Control,
    Hardware,
    Storage,
    Network,
    Security,
}

impl Category {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Media => "media",
            Self::Control => "control",
            Self::Hardware => "hardware",
            Self::Storage => "storage",
            Self::Network => "network",
            Self::Security => "security",
        }
    }
}

/// A dependency-free structured event value.
#[derive(Clone, Debug, PartialEq)]
pub enum EventValue {
    Boolean(bool),
    Integer(i64),
    Unsigned(u64),
    Float(f64),
    Text(String),
}

impl From<bool> for EventValue {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

impl From<i64> for EventValue {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl From<u64> for EventValue {
    fn from(value: u64) -> Self {
        Self::Unsigned(value)
    }
}

impl From<f64> for EventValue {
    fn from(value: f64) -> Self {
        Self::Float(value)
    }
}

impl From<String> for EventValue {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for EventValue {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

/// One named field on an event.
#[derive(Clone, Debug, PartialEq)]
pub struct EventField {
    pub name: String,
    pub value: EventValue,
}

impl EventField {
    #[must_use]
    pub fn new(name: impl Into<String>, value: impl Into<EventValue>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// A structured event retained by [`EventLog`].
#[derive(Clone, Debug, PartialEq)]
pub struct EventRecord {
    pub sequence: u64,
    /// Caller-supplied monotonic process time.
    pub monotonic_millis: u64,
    pub severity: Severity,
    pub category: Category,
    pub message: String,
    pub fields: Vec<EventField>,
}

/// A bounded, insertion-ordered event ring.
#[derive(Clone, Debug)]
pub struct EventLog {
    capacity: usize,
    records: VecDeque<EventRecord>,
    next_sequence: Option<u64>,
    dropped: u64,
    redactor: Redactor,
}

impl EventLog {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self::with_redactor(capacity, Redactor)
    }

    #[must_use]
    pub const fn with_redactor(capacity: usize, redactor: Redactor) -> Self {
        Self {
            capacity,
            records: VecDeque::new(),
            next_sequence: Some(1),
            dropped: 0,
            redactor,
        }
    }

    /// Records a redacted event and returns its process-local sequence.
    ///
    /// # Errors
    ///
    /// Returns [`SequenceExhausted`] rather than reusing a sequence after
    /// `u64::MAX` events.
    pub fn record(
        &mut self,
        monotonic_millis: u64,
        severity: Severity,
        category: Category,
        message: impl AsRef<str>,
        fields: impl IntoIterator<Item = EventField>,
    ) -> Result<u64, SequenceExhausted> {
        let sequence = self.next_sequence.ok_or(SequenceExhausted)?;
        self.next_sequence = sequence.checked_add(1);

        let fields = fields
            .into_iter()
            .map(|mut field| {
                field.value = match field.value {
                    EventValue::Text(value) => {
                        EventValue::Text(self.redactor.redact_field(&field.name, &value))
                    }
                    _value if self.redactor.is_secret_name(&field.name) => {
                        EventValue::Text(Redactor::SECRET_MARKER.to_owned())
                    }
                    value => value,
                };
                field
            })
            .collect();
        let record = EventRecord {
            sequence,
            monotonic_millis,
            severity,
            category,
            message: self.redactor.redact(message.as_ref()),
            fields,
        };

        if self.capacity == 0 {
            self.dropped = self.dropped.saturating_add(1);
        } else {
            if self.records.len() == self.capacity {
                self.records.pop_front();
                self.dropped = self.dropped.saturating_add(1);
            }
            self.records.push_back(record);
        }
        Ok(sequence)
    }

    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &EventRecord> {
        self.records.iter()
    }
}

/// Event sequence space has been consumed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SequenceExhausted;

impl fmt::Display for SequenceExhausted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("event sequence exhausted")
    }
}

impl Error for SequenceExhausted {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_is_bounded_and_sequences_remain_monotonic() {
        let mut log = EventLog::new(2);
        for time in 0..4 {
            assert_eq!(
                log.record(time, Severity::Info, Category::Runtime, "tick", []),
                Ok(time + 1)
            );
        }

        assert_eq!(log.len(), 2);
        assert_eq!(log.dropped(), 2);
        assert_eq!(
            log.iter().map(|event| event.sequence).collect::<Vec<_>>(),
            [3, 4]
        );
    }

    #[test]
    fn zero_capacity_counts_every_drop() {
        let mut log = EventLog::new(0);
        log.record(0, Severity::Info, Category::Runtime, "tick", [])
            .unwrap();
        assert!(log.is_empty());
        assert_eq!(log.dropped(), 1);
    }
}
