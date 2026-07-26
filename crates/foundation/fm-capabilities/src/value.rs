use std::cmp::Ordering;

use crate::StableId;

/// A stable unit for an integer quantity, such as `frames_per_second_milli`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct QuantityUnit(StableId);

impl QuantityUnit {
    #[must_use]
    pub const fn from_id(id: StableId) -> Self {
        Self(id)
    }

    #[must_use]
    pub fn id(&self) -> &StableId {
        &self.0
    }
}

/// A typed scalar advertised as a capability limit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LimitValue {
    Boolean(bool),
    Signed(i64),
    Unsigned(u64),
    Quantity { value: i64, unit: QuantityUnit },
}

impl LimitValue {
    #[must_use]
    pub const fn kind(&self) -> ValueKind {
        match self {
            Self::Boolean(_) => ValueKind::Boolean,
            Self::Signed(_) => ValueKind::Signed,
            Self::Unsigned(_) => ValueKind::Unsigned,
            Self::Quantity { .. } => ValueKind::Quantity,
        }
    }

    pub(crate) fn typed_cmp(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (Self::Boolean(left), Self::Boolean(right)) => left.partial_cmp(right),
            (Self::Signed(left), Self::Signed(right)) => left.partial_cmp(right),
            (Self::Unsigned(left), Self::Unsigned(right)) => left.partial_cmp(right),
            (
                Self::Quantity {
                    value: left,
                    unit: left_unit,
                },
                Self::Quantity {
                    value: right,
                    unit: right_unit,
                },
            ) if left_unit == right_unit => left.partial_cmp(right),
            _ => None,
        }
    }
}

/// The scalar's data type, reported when a requirement cannot be compared.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValueKind {
    Boolean,
    Signed,
    Unsigned,
    Quantity,
}

/// How an advertised limit is compared with a project requirement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LimitComparison {
    AtLeast,
    AtMost,
    Equal,
}

/// One typed limit requirement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LimitConstraint {
    pub comparison: LimitComparison,
    pub value: LimitValue,
}

impl LimitConstraint {
    #[must_use]
    pub const fn new(comparison: LimitComparison, value: LimitValue) -> Self {
        Self { comparison, value }
    }

    pub(crate) fn matches(&self, actual: &LimitValue) -> Option<bool> {
        let ordering = actual.typed_cmp(&self.value)?;
        Some(match self.comparison {
            LimitComparison::AtLeast => ordering.is_ge(),
            LimitComparison::AtMost => ordering.is_le(),
            LimitComparison::Equal => ordering.is_eq(),
        })
    }
}
