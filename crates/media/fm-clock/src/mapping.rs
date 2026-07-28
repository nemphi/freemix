use core::fmt;

use crate::{ClockDomainId, ClockSnapshot, ClockTime};

const PARTS_PER_BILLION: i128 = 1_000_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MappingError {
    DomainMismatch,
    NonMonotonicSamples,
    InsufficientSamples,
    InvalidRate,
    ArithmeticOverflow,
}

impl fmt::Display for MappingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DomainMismatch => "clock sample belongs to an unexpected domain",
            Self::NonMonotonicSamples => "clock samples must advance in both domains",
            Self::InsufficientSamples => "at least two clock sample pairs are required",
            Self::InvalidRate => "clock mapping rate must be positive",
            Self::ArithmeticOverflow => "clock mapping arithmetic overflow",
        })
    }
}

impl std::error::Error for MappingError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockMapping {
    source_anchor: ClockSnapshot,
    master_anchor: ClockSnapshot,
    drift_ppb: i64,
}

impl ClockMapping {
    /// Creates an affine source-to-master clock mapping.
    ///
    /// # Errors
    ///
    /// Returns [`MappingError::InvalidRate`] when `drift_ppb` would produce a
    /// zero or negative clock rate.
    pub fn new(
        source_anchor: ClockSnapshot,
        master_anchor: ClockSnapshot,
        drift_ppb: i64,
    ) -> Result<Self, MappingError> {
        if i128::from(drift_ppb) <= -PARTS_PER_BILLION {
            return Err(MappingError::InvalidRate);
        }
        Ok(Self {
            source_anchor,
            master_anchor,
            drift_ppb,
        })
    }

    #[must_use]
    pub const fn source_domain(self) -> ClockDomainId {
        self.source_anchor.domain()
    }

    #[must_use]
    pub const fn master_domain(self) -> ClockDomainId {
        self.master_anchor.domain()
    }

    #[must_use]
    pub const fn drift_ppb(self) -> i64 {
        self.drift_ppb
    }

    #[must_use]
    pub const fn source_anchor(self) -> ClockSnapshot {
        self.source_anchor
    }

    #[must_use]
    pub const fn master_anchor(self) -> ClockSnapshot {
        self.master_anchor
    }

    /// Maps a signed source-domain nanosecond position into the master domain.
    /// Fractional nanoseconds round toward negative infinity.
    ///
    /// # Errors
    ///
    /// Returns [`MappingError::ArithmeticOverflow`] when the mapped position is
    /// outside `i64` or intermediate checked arithmetic overflows.
    pub fn map_source_nanos(self, source_nanos: i64) -> Result<i64, MappingError> {
        let source_delta = i128::from(source_nanos)
            .checked_sub(i128::from(self.source_anchor.time().as_nanos()))
            .ok_or(MappingError::ArithmeticOverflow)?;
        let scale = PARTS_PER_BILLION + i128::from(self.drift_ppb);
        let numerator = source_delta
            .checked_mul(scale)
            .ok_or(MappingError::ArithmeticOverflow)?;
        let master_delta = floor_div(numerator, PARTS_PER_BILLION)?;
        let mapped = i128::from(self.master_anchor.time().as_nanos())
            .checked_add(master_delta)
            .ok_or(MappingError::ArithmeticOverflow)?;
        i64::try_from(mapped).map_err(|_| MappingError::ArithmeticOverflow)
    }

    /// Inverts this affine mapping for a signed master-domain nanosecond
    /// position. Fractional source nanoseconds round toward negative infinity.
    ///
    /// # Errors
    ///
    /// Returns [`MappingError::ArithmeticOverflow`] when the source position is
    /// outside `i64` or intermediate checked arithmetic overflows.
    pub fn source_nanos_at_master(self, master_nanos: i64) -> Result<i64, MappingError> {
        let master_delta = i128::from(master_nanos)
            .checked_sub(i128::from(self.master_anchor.time().as_nanos()))
            .ok_or(MappingError::ArithmeticOverflow)?;
        let numerator = master_delta
            .checked_mul(PARTS_PER_BILLION)
            .ok_or(MappingError::ArithmeticOverflow)?;
        let scale = PARTS_PER_BILLION + i128::from(self.drift_ppb);
        let source_delta = floor_div(numerator, scale)?;
        let source = i128::from(self.source_anchor.time().as_nanos())
            .checked_add(source_delta)
            .ok_or(MappingError::ArithmeticOverflow)?;
        i64::try_from(source).map_err(|_| MappingError::ArithmeticOverflow)
    }

    /// Maps a snapshot into the master domain.
    ///
    /// # Errors
    ///
    /// Returns [`MappingError::DomainMismatch`] for a snapshot from another
    /// source domain, or [`MappingError::ArithmeticOverflow`] when the mapped
    /// time is outside the representable timeline.
    pub fn map(self, source: ClockSnapshot) -> Result<ClockSnapshot, MappingError> {
        if source.domain() != self.source_domain() {
            return Err(MappingError::DomainMismatch);
        }
        let source_nanos = i64::try_from(source.time().as_nanos())
            .map_err(|_| MappingError::ArithmeticOverflow)?;
        let mapped = self.map_source_nanos(source_nanos)?;
        let nanos = u64::try_from(mapped).map_err(|_| MappingError::ArithmeticOverflow)?;
        Ok(ClockSnapshot::new(
            self.master_domain(),
            ClockTime::from_nanos(nanos),
        ))
    }
}

fn floor_div(numerator: i128, denominator: i128) -> Result<i128, MappingError> {
    if denominator <= 0 {
        return Err(MappingError::InvalidRate);
    }
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    if remainder < 0 {
        quotient
            .checked_sub(1)
            .ok_or(MappingError::ArithmeticOverflow)
    } else {
        Ok(quotient)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DriftEstimator {
    source_domain: ClockDomainId,
    master_domain: ClockDomainId,
    first: Option<(ClockSnapshot, ClockSnapshot)>,
    latest: Option<(ClockSnapshot, ClockSnapshot)>,
}

impl DriftEstimator {
    #[must_use]
    pub const fn new(source_domain: ClockDomainId, master_domain: ClockDomainId) -> Self {
        Self {
            source_domain,
            master_domain,
            first: None,
            latest: None,
        }
    }

    /// Adds a paired observation from the source and master clocks.
    ///
    /// # Errors
    ///
    /// Returns [`MappingError::DomainMismatch`] for unexpected domains, or
    /// [`MappingError::NonMonotonicSamples`] unless both clocks advanced.
    pub fn observe(
        &mut self,
        source: ClockSnapshot,
        master: ClockSnapshot,
    ) -> Result<(), MappingError> {
        if source.domain() != self.source_domain || master.domain() != self.master_domain {
            return Err(MappingError::DomainMismatch);
        }
        if let Some((previous_source, previous_master)) = self.latest
            && (source.time() <= previous_source.time() || master.time() <= previous_master.time())
        {
            return Err(MappingError::NonMonotonicSamples);
        }
        if self.first.is_none() {
            self.first = Some((source, master));
        }
        self.latest = Some((source, master));
        Ok(())
    }

    /// Estimates signed drift from the complete observed span without narrowing it.
    ///
    /// This is useful for callers that apply a bounded drift policy before
    /// constructing a [`ClockMapping`].
    ///
    /// # Errors
    ///
    /// Returns [`MappingError::InsufficientSamples`] until two distinct sample
    /// pairs are available.
    pub fn estimated_drift_ppb(self) -> Result<i128, MappingError> {
        let (first_source, first_master) = self.first.ok_or(MappingError::InsufficientSamples)?;
        let (latest_source, latest_master) =
            self.latest.ok_or(MappingError::InsufficientSamples)?;
        let source_delta = latest_source
            .time()
            .duration_since(first_source.time())
            .ok_or(MappingError::NonMonotonicSamples)?
            .as_nanos();
        let master_delta = latest_master
            .time()
            .duration_since(first_master.time())
            .ok_or(MappingError::NonMonotonicSamples)?
            .as_nanos();
        if source_delta == 0 {
            return Err(MappingError::InsufficientSamples);
        }
        let scaled_rate = i128::from(master_delta)
            .checked_mul(PARTS_PER_BILLION)
            .ok_or(MappingError::ArithmeticOverflow)?
            / i128::from(source_delta);
        Ok(scaled_rate - PARTS_PER_BILLION)
    }

    /// Estimates a mapping from the complete observed span.
    ///
    /// # Errors
    ///
    /// Returns [`MappingError::InsufficientSamples`] until two distinct sample
    /// pairs are available, and an arithmetic error when the estimated mapping
    /// cannot be represented.
    pub fn mapping(self) -> Result<ClockMapping, MappingError> {
        let (first_source, first_master) = self.first.ok_or(MappingError::InsufficientSamples)?;
        let drift_ppb = i64::try_from(self.estimated_drift_ppb()?)
            .map_err(|_| MappingError::ArithmeticOverflow)?;
        ClockMapping::new(first_source, first_master, drift_ppb)
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroU128;

    use super::*;

    fn domain(value: u128) -> ClockDomainId {
        ClockDomainId::new(NonZeroU128::new(value).unwrap())
    }

    fn sample(domain: ClockDomainId, nanos: u64) -> ClockSnapshot {
        ClockSnapshot::new(domain, ClockTime::from_nanos(nanos))
    }

    #[test]
    fn mapping_applies_offset_and_drift() {
        let source = domain(1);
        let master = domain(2);
        let mapping =
            ClockMapping::new(sample(source, 1_000), sample(master, 10_000), 1_000_000).unwrap();
        assert_eq!(
            mapping.map(sample(source, 2_000)).unwrap(),
            sample(master, 11_001)
        );
        assert_eq!(
            mapping.map(sample(source, 0)).unwrap(),
            sample(master, 8_999)
        );
    }

    #[test]
    fn signed_mapping_and_inverse_use_floor_rounding_before_anchors() {
        let source = domain(1);
        let master = domain(2);
        let mapping =
            ClockMapping::new(sample(source, 1_000), sample(master, 10_000), 500_000_000).unwrap();
        assert_eq!(mapping.map_source_nanos(999).unwrap(), 9_998);
        assert_eq!(mapping.source_nanos_at_master(9_998).unwrap(), 998);
        assert_eq!(mapping.source_nanos_at_master(10_003).unwrap(), 1_002);
        assert_eq!(mapping.source_anchor(), sample(source, 1_000));
        assert_eq!(mapping.master_anchor(), sample(master, 10_000));
    }

    #[test]
    fn signed_mapping_reports_representability_overflow() {
        let source = domain(1);
        let master = domain(2);
        let mapping = ClockMapping::new(
            sample(source, 0),
            sample(master, i64::MAX.cast_unsigned()),
            i64::MAX,
        )
        .unwrap();
        assert_eq!(
            mapping.map_source_nanos(i64::MAX),
            Err(MappingError::ArithmeticOverflow)
        );
    }

    #[test]
    fn estimator_derives_mapping_from_sample_span() {
        let source = domain(1);
        let master = domain(2);
        let mut estimator = DriftEstimator::new(source, master);
        estimator
            .observe(sample(source, 1_000), sample(master, 5_000))
            .unwrap();
        estimator
            .observe(sample(source, 1_001_000), sample(master, 1_006_000))
            .unwrap();
        let mapping = estimator.mapping().unwrap();
        assert_eq!(mapping.drift_ppb(), 1_000_000);
        assert_eq!(
            mapping.map(sample(source, 2_001_000)).unwrap(),
            sample(master, 2_007_000)
        );
    }

    #[test]
    fn estimator_exposes_drift_before_i64_narrowing() {
        let source = domain(1);
        let master = domain(2);
        let mut estimator = DriftEstimator::new(source, master);
        estimator
            .observe(sample(source, 1), sample(master, 1))
            .unwrap();
        estimator
            .observe(sample(source, 2), sample(master, u64::MAX))
            .unwrap();
        assert!(estimator.estimated_drift_ppb().unwrap() > i128::from(i64::MAX));
        assert_eq!(estimator.mapping(), Err(MappingError::ArithmeticOverflow));
    }

    #[test]
    fn estimator_rejects_wrong_domains_and_regression() {
        let source = domain(1);
        let master = domain(2);
        let mut estimator = DriftEstimator::new(source, master);
        assert_eq!(
            estimator.observe(sample(master, 1), sample(master, 1)),
            Err(MappingError::DomainMismatch)
        );
        estimator
            .observe(sample(source, 2), sample(master, 2))
            .unwrap();
        assert_eq!(
            estimator.observe(sample(source, 2), sample(master, 3)),
            Err(MappingError::NonMonotonicSamples)
        );
    }

    #[test]
    fn one_sample_cannot_estimate_drift() {
        let source = domain(1);
        let master = domain(2);
        let mut estimator = DriftEstimator::new(source, master);
        estimator
            .observe(sample(source, 1), sample(master, 1))
            .unwrap();
        assert_eq!(estimator.mapping(), Err(MappingError::InsufficientSamples));
    }
}
