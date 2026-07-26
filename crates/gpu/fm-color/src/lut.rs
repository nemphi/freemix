use core::fmt;

use crate::Rgb;

const MAX_1D_SIZE: usize = 65_536;
const MAX_3D_EDGE: usize = 65;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LutError {
    TooFewEntries,
    TooManyEntries,
    SizeMismatch { expected: usize, actual: usize },
    InvalidDomain,
    NonFiniteValue,
}

impl fmt::Display for LutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooFewEntries => {
                formatter.write_str("LUT requires at least two entries per axis")
            }
            Self::TooManyEntries => formatter.write_str("LUT exceeds the reference size limit"),
            Self::SizeMismatch { expected, actual } => {
                write!(formatter, "LUT has {actual} entries, expected {expected}")
            }
            Self::InvalidDomain => formatter.write_str("LUT domain must be finite and increasing"),
            Self::NonFiniteValue => formatter.write_str("LUT entries must be finite"),
        }
    }
}

impl std::error::Error for LutError {}

#[derive(Clone, Debug, PartialEq)]
pub struct Lut1D {
    entries: Vec<Rgb>,
    domain_min: Rgb,
    domain_max: Rgb,
}

impl Lut1D {
    /// Creates a bounded 1D RGB LUT.
    ///
    /// # Errors
    ///
    /// Returns a validation error for an invalid size or domain, or for any
    /// non-finite entry.
    pub fn new(entries: Vec<Rgb>, domain_min: Rgb, domain_max: Rgb) -> Result<Self, LutError> {
        validate_domain(domain_min, domain_max)?;
        if entries.len() < 2 {
            return Err(LutError::TooFewEntries);
        }
        if entries.len() > MAX_1D_SIZE {
            return Err(LutError::TooManyEntries);
        }
        validate_entries(&entries)?;
        Ok(Self {
            entries,
            domain_min,
            domain_max,
        })
    }

    #[must_use]
    pub fn size(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub const fn domain_min(&self) -> Rgb {
        self.domain_min
    }

    #[must_use]
    pub const fn domain_max(&self) -> Rgb {
        self.domain_max
    }

    #[must_use]
    pub fn entries(&self) -> &[Rgb] {
        &self.entries
    }

    /// Samples each channel independently using clamped linear interpolation.
    #[must_use]
    pub fn sample(&self, input: Rgb) -> Rgb {
        Rgb::new(
            sample_1d(
                &self.entries,
                0,
                normalize(input.r, self.domain_min.r, self.domain_max.r),
            ),
            sample_1d(
                &self.entries,
                1,
                normalize(input.g, self.domain_min.g, self.domain_max.g),
            ),
            sample_1d(
                &self.entries,
                2,
                normalize(input.b, self.domain_min.b, self.domain_max.b),
            ),
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Lut3D {
    edge: usize,
    entries: Vec<Rgb>,
    domain_min: Rgb,
    domain_max: Rgb,
}

impl Lut3D {
    /// Creates a red-fastest 3D LUT (`index = (blue * edge + green) * edge + red`).
    ///
    /// # Errors
    ///
    /// Returns a validation error for an invalid edge, entry count, domain, or
    /// non-finite entry.
    pub fn new(
        edge: usize,
        entries: Vec<Rgb>,
        domain_min: Rgb,
        domain_max: Rgb,
    ) -> Result<Self, LutError> {
        validate_domain(domain_min, domain_max)?;
        if edge < 2 {
            return Err(LutError::TooFewEntries);
        }
        if edge > MAX_3D_EDGE {
            return Err(LutError::TooManyEntries);
        }
        let expected = edge
            .checked_mul(edge)
            .and_then(|square| square.checked_mul(edge))
            .ok_or(LutError::TooManyEntries)?;
        if entries.len() != expected {
            return Err(LutError::SizeMismatch {
                expected,
                actual: entries.len(),
            });
        }
        validate_entries(&entries)?;
        Ok(Self {
            edge,
            entries,
            domain_min,
            domain_max,
        })
    }

    #[must_use]
    pub const fn edge(&self) -> usize {
        self.edge
    }

    #[must_use]
    pub fn entries(&self) -> &[Rgb] {
        &self.entries
    }

    /// Samples the cube with clamped trilinear interpolation.
    #[must_use]
    pub fn sample(&self, input: Rgb) -> Rgb {
        let r = axis(
            normalize(input.r, self.domain_min.r, self.domain_max.r),
            self.edge,
        );
        let g = axis(
            normalize(input.g, self.domain_min.g, self.domain_max.g),
            self.edge,
        );
        let b = axis(
            normalize(input.b, self.domain_min.b, self.domain_max.b),
            self.edge,
        );

        let c00 = lerp(self.entry(r.0, g.0, b.0), self.entry(r.1, g.0, b.0), r.2);
        let c10 = lerp(self.entry(r.0, g.1, b.0), self.entry(r.1, g.1, b.0), r.2);
        let c01 = lerp(self.entry(r.0, g.0, b.1), self.entry(r.1, g.0, b.1), r.2);
        let c11 = lerp(self.entry(r.0, g.1, b.1), self.entry(r.1, g.1, b.1), r.2);
        lerp(lerp(c00, c10, g.2), lerp(c01, c11, g.2), b.2)
    }

    fn entry(&self, red: usize, green: usize, blue: usize) -> Rgb {
        self.entries[(blue * self.edge + green) * self.edge + red]
    }
}

fn validate_domain(minimum: Rgb, maximum: Rgb) -> Result<(), LutError> {
    let finite = minimum.r.is_finite()
        && minimum.g.is_finite()
        && minimum.b.is_finite()
        && maximum.r.is_finite()
        && maximum.g.is_finite()
        && maximum.b.is_finite();
    if finite && minimum.r < maximum.r && minimum.g < maximum.g && minimum.b < maximum.b {
        Ok(())
    } else {
        Err(LutError::InvalidDomain)
    }
}

fn validate_entries(entries: &[Rgb]) -> Result<(), LutError> {
    if entries
        .iter()
        .all(|entry| entry.r.is_finite() && entry.g.is_finite() && entry.b.is_finite())
    {
        Ok(())
    } else {
        Err(LutError::NonFiniteValue)
    }
}

fn normalize(value: f32, minimum: f32, maximum: f32) -> f32 {
    ((value - minimum) / (maximum - minimum)).clamp(0.0, 1.0)
}

fn sample_1d(entries: &[Rgb], channel: usize, position: f32) -> f32 {
    let (low, high, fraction) = axis(position, entries.len());
    let component = |entry: Rgb| match channel {
        0 => entry.r,
        1 => entry.g,
        _ => entry.b,
    };
    component(entries[low]) * (1.0 - fraction) + component(entries[high]) * fraction
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn axis(position: f32, size: usize) -> (usize, usize, f32) {
    // Constructors bound size to 65,536 and callers clamp position to [0, 1].
    let scaled = position * (size - 1) as f32;
    let low = scaled.floor() as usize;
    let high = (low + 1).min(size - 1);
    (low, high, scaled - low as f32)
}

fn lerp(left: Rgb, right: Rgb, amount: f32) -> Rgb {
    Rgb::new(
        left.r * (1.0 - amount) + right.r * amount,
        left.g * (1.0 - amount) + right.g * amount,
        left.b * (1.0 - amount) + right.b * amount,
    )
}
