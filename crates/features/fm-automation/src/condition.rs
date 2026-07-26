use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Value {
    Bool(bool),
    Number(i64),
    Text(String),
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::Number(value)
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

pub type ConditionContext = BTreeMap<String, Value>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Predicate {
    Exists,
    Missing,
    Equal(Value),
    NotEqual(Value),
    GreaterThan(i64),
    GreaterOrEqual(i64),
    LessThan(i64),
    LessOrEqual(i64),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Condition {
    pub field: String,
    pub predicate: Predicate,
}

impl Condition {
    #[must_use]
    pub fn new(field: impl Into<String>, predicate: Predicate) -> Self {
        Self {
            field: field.into(),
            predicate,
        }
    }

    #[must_use]
    pub fn evaluate(&self, context: &ConditionContext) -> bool {
        let value = context.get(&self.field);
        match &self.predicate {
            Predicate::Exists => value.is_some(),
            Predicate::Missing => value.is_none(),
            Predicate::Equal(expected) => value == Some(expected),
            Predicate::NotEqual(expected) => value.is_some_and(|value| value != expected),
            Predicate::GreaterThan(expected) => number(value).is_some_and(|v| v > *expected),
            Predicate::GreaterOrEqual(expected) => number(value).is_some_and(|v| v >= *expected),
            Predicate::LessThan(expected) => number(value).is_some_and(|v| v < *expected),
            Predicate::LessOrEqual(expected) => number(value).is_some_and(|v| v <= *expected),
        }
    }
}

pub(crate) fn conditions_match(conditions: &[Condition], context: &ConditionContext) -> bool {
    conditions
        .iter()
        .all(|condition| condition.evaluate(context))
}

fn number(value: Option<&Value>) -> Option<i64> {
    match value {
        Some(Value::Number(value)) => Some(*value),
        _ => None,
    }
}
