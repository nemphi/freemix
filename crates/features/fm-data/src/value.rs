use std::{cmp::Ordering, collections::BTreeMap, fmt, str::FromStr};

/// The concrete type of a [`DataValue`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValueType {
    Null,
    Bool,
    Integer,
    Decimal,
    String,
    List,
    Object,
}

/// An exact base-10 number backed by a signed coefficient and at most 18 decimal places.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct Decimal {
    coefficient: i64,
    scale: u32,
}

/// A decimal could not be represented.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecimalError {
    Empty,
    Invalid,
    TooPrecise,
    OutOfRange,
}

impl fmt::Display for DecimalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("decimal is empty"),
            Self::Invalid => formatter.write_str("decimal has an invalid format"),
            Self::TooPrecise => formatter.write_str("decimal has more than 18 fractional digits"),
            Self::OutOfRange => formatter.write_str("decimal coefficient is out of range"),
        }
    }
}

impl std::error::Error for DecimalError {}

impl Decimal {
    pub const MAX_SCALE: u32 = 18;

    /// Creates an exact decimal, removing insignificant trailing zeroes.
    ///
    /// # Errors
    ///
    /// Returns [`DecimalError::TooPrecise`] when `scale` exceeds 18.
    pub fn new(mut coefficient: i64, mut scale: u32) -> Result<Self, DecimalError> {
        if scale > Self::MAX_SCALE {
            return Err(DecimalError::TooPrecise);
        }
        while scale > 0 && coefficient % 10 == 0 {
            coefficient /= 10;
            scale -= 1;
        }
        Ok(Self { coefficient, scale })
    }

    #[must_use]
    pub const fn coefficient(self) -> i64 {
        self.coefficient
    }

    #[must_use]
    pub const fn scale(self) -> u32 {
        self.scale
    }

    #[must_use]
    pub fn from_integer(value: i64) -> Self {
        Self {
            coefficient: value,
            scale: 0,
        }
    }

    fn aligned(self, other: Self) -> (i128, i128) {
        let scale = self.scale.max(other.scale);
        let left = i128::from(self.coefficient) * 10_i128.pow(scale - self.scale);
        let right = i128::from(other.coefficient) * 10_i128.pow(scale - other.scale);
        (left, right)
    }
}

impl FromStr for Decimal {
    type Err = DecimalError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if input.is_empty() {
            return Err(DecimalError::Empty);
        }
        let (negative, unsigned) = input
            .strip_prefix('-')
            .map_or((false, input), |rest| (true, rest));
        if unsigned.is_empty() || unsigned.starts_with('+') {
            return Err(DecimalError::Invalid);
        }
        let mut parts = unsigned.split('.');
        let whole = parts.next().unwrap_or_default();
        let fraction = parts.next();
        if parts.next().is_some()
            || whole.is_empty()
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || fraction.is_some_and(|part| {
                part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            return Err(DecimalError::Invalid);
        }
        let fraction = fraction.unwrap_or_default();
        let scale = u32::try_from(fraction.len()).map_err(|_| DecimalError::TooPrecise)?;
        if scale > Self::MAX_SCALE {
            return Err(DecimalError::TooPrecise);
        }
        let digits = format!("{whole}{fraction}");
        let magnitude = digits
            .parse::<u64>()
            .map_err(|_| DecimalError::OutOfRange)?;
        let coefficient = if negative {
            if magnitude == i64::MAX as u64 + 1 {
                i64::MIN
            } else {
                -i64::try_from(magnitude).map_err(|_| DecimalError::OutOfRange)?
            }
        } else {
            i64::try_from(magnitude).map_err(|_| DecimalError::OutOfRange)?
        };
        Self::new(coefficient, scale)
    }
}

impl Ord for Decimal {
    fn cmp(&self, other: &Self) -> Ordering {
        let (left, right) = self.aligned(*other);
        left.cmp(&right)
    }
}

impl PartialOrd for Decimal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Decimal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let negative = self.coefficient.is_negative();
        let digits = self.coefficient.unsigned_abs().to_string();
        if negative {
            formatter.write_str("-")?;
        }
        if self.scale == 0 {
            return formatter.write_str(&digits);
        }
        let scale = self.scale as usize;
        if digits.len() <= scale {
            formatter.write_str("0.")?;
            for _ in 0..(scale - digits.len()) {
                formatter.write_str("0")?;
            }
            formatter.write_str(&digits)
        } else {
            let split = digits.len() - scale;
            write!(formatter, "{}.{}", &digits[..split], &digits[split..])
        }
    }
}

impl fmt::Debug for Decimal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Decimal({self})")
    }
}

/// A data value with no implicit type coercion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataValue {
    Null,
    Bool(bool),
    Integer(i64),
    Decimal(Decimal),
    String(String),
    List(Vec<Self>),
    Object(BTreeMap<String, Self>),
}

impl DataValue {
    #[must_use]
    pub const fn value_type(&self) -> ValueType {
        match self {
            Self::Null => ValueType::Null,
            Self::Bool(_) => ValueType::Bool,
            Self::Integer(_) => ValueType::Integer,
            Self::Decimal(_) => ValueType::Decimal,
            Self::String(_) => ValueType::String,
            Self::List(_) => ValueType::List,
            Self::Object(_) => ValueType::Object,
        }
    }

    #[must_use]
    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Renders scalar values for formatting. Containers are intentionally rejected.
    #[must_use]
    pub fn scalar_text(&self) -> Option<String> {
        match self {
            Self::Null => Some("null".to_owned()),
            Self::Bool(value) => Some(value.to_string()),
            Self::Integer(value) => Some(value.to_string()),
            Self::Decimal(value) => Some(value.to_string()),
            Self::String(value) => Some(value.clone()),
            Self::List(_) | Self::Object(_) => None,
        }
    }
}

impl From<bool> for DataValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<i64> for DataValue {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl From<Decimal> for DataValue {
    fn from(value: Decimal) -> Self {
        Self::Decimal(value)
    }
}

impl From<String> for DataValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for DataValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}
