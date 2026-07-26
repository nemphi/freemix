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
        let source_delta =
            i128::from(source.time().as_nanos()) - i128::from(self.source_anchor.time().as_nanos());
        let scale = PARTS_PER_BILLION + i128::from(self.drift_ppb);
        let master_delta = source_delta
            .checked_mul(scale)
            .ok_or(MappingError::ArithmeticOverflow)?
            / PARTS_PER_BILLION;
        let mapped = i128::from(self.master_anchor.time().as_nanos())
            .checked_add(master_delta)
            .ok_or(MappingError::ArithmeticOverflow)?;
        let nanos = u64::try_from(mapped).map_err(|_| MappingError::ArithmeticOverflow)?;
        Ok(ClockSnapshot::new(
            self.master_domain(),
            ClockTime::from_nanos(nanos),
        ))
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

    /// Estimates a mapping from the complete observed span.
    ///
    /// # Errors
    ///
    /// Returns [`MappingError::InsufficientSamples`] until two distinct sample
    /// pairs are available, and an arithmetic error when the estimated mapping
    /// cannot be represented.
    pub fn mapping(self) -> Result<ClockMapping, MappingError> {
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
        let drift = scaled_rate - PARTS_PER_BILLION;
        let drift_ppb = i64::try_from(drift).map_err(|_| MappingError::ArithmeticOverflow)?;
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
