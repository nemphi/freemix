use core::fmt;
use std::collections::VecDeque;

use fm_clock::{ClockMapping, ClockSnapshot, DriftEstimator, MappingError};

/// Hard ceiling for retained paired clock observations.
pub const MAX_CLOCK_RECALIBRATION_OBSERVATIONS: usize = 256;
/// Largest symmetric rate correction accepted by the recalibration policy.
pub const MAX_CLOCK_DRIFT_PPM: u32 = 999_999;

/// Bounded policy for source-to-Master clock drift recalibration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockRecalibrationPolicy {
    window_observations: usize,
    minimum_observations: usize,
    max_drift_ppm: u32,
    max_observation_error_nanos: u64,
}

impl ClockRecalibrationPolicy {
    /// Creates an explicit bounded recalibration policy.
    ///
    /// `max_drift_ppm` is symmetric and estimates outside it are clamped.
    /// Observations farther than `max_observation_error_nanos` from the current
    /// mapping are rejected as outliers.
    ///
    /// # Errors
    ///
    /// Returns a typed error for invalid observation counts or drift bounds.
    pub fn new(
        window_observations: usize,
        minimum_observations: usize,
        max_drift_ppm: u32,
        max_observation_error_nanos: u64,
    ) -> Result<Self, ClockRecalibrationError> {
        if !(2..=MAX_CLOCK_RECALIBRATION_OBSERVATIONS).contains(&window_observations) {
            return Err(ClockRecalibrationError::InvalidWindowObservations(
                window_observations,
            ));
        }
        if !(2..=window_observations).contains(&minimum_observations) {
            return Err(ClockRecalibrationError::InvalidMinimumObservations {
                minimum: minimum_observations,
                window: window_observations,
            });
        }
        if max_drift_ppm > MAX_CLOCK_DRIFT_PPM {
            return Err(ClockRecalibrationError::DriftLimitOutOfRange(max_drift_ppm));
        }
        Ok(Self {
            window_observations,
            minimum_observations,
            max_drift_ppm,
            max_observation_error_nanos,
        })
    }

    #[must_use]
    pub const fn window_observations(self) -> usize {
        self.window_observations
    }

    #[must_use]
    pub const fn minimum_observations(self) -> usize {
        self.minimum_observations
    }

    #[must_use]
    pub const fn max_drift_ppm(self) -> u32 {
        self.max_drift_ppm
    }

    #[must_use]
    pub const fn max_observation_error_nanos(self) -> u64 {
        self.max_observation_error_nanos
    }
}

/// Result of one accepted paired clock observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClockRecalibrationUpdate {
    Collecting {
        observations: usize,
        required: usize,
    },
    Recalibrated {
        drift_ppb: i64,
        clamped: bool,
    },
}

/// Bounded clock recalibration counters and current mapping state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockRecalibrationTelemetry {
    observation_count: usize,
    accepted_recalibrations: u64,
    rejected_recalibrations: u64,
    current_drift_ppb: i64,
    anchor_generation: u64,
}

impl ClockRecalibrationTelemetry {
    #[must_use]
    pub const fn observation_count(self) -> usize {
        self.observation_count
    }

    #[must_use]
    pub const fn accepted_recalibrations(self) -> u64 {
        self.accepted_recalibrations
    }

    #[must_use]
    pub const fn rejected_recalibrations(self) -> u64 {
        self.rejected_recalibrations
    }

    #[must_use]
    pub const fn current_drift_ppb(self) -> i64 {
        self.current_drift_ppb
    }

    #[must_use]
    pub const fn anchor_generation(self) -> u64 {
        self.anchor_generation
    }
}

/// Errors from policy configuration or paired clock observations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClockRecalibrationError {
    Disabled,
    InvalidWindowObservations(usize),
    InvalidMinimumObservations { minimum: usize, window: usize },
    DriftLimitOutOfRange(u32),
    ObservationOutlier { deviation_nanos: u64, maximum: u64 },
    Mapping(MappingError),
}

impl fmt::Display for ClockRecalibrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => formatter.write_str("clock drift recalibration is disabled"),
            Self::InvalidWindowObservations(actual) => write!(
                formatter,
                "clock recalibration window {actual} is outside 2..={MAX_CLOCK_RECALIBRATION_OBSERVATIONS}"
            ),
            Self::InvalidMinimumObservations { minimum, window } => write!(
                formatter,
                "clock recalibration minimum {minimum} is outside 2..={window}"
            ),
            Self::DriftLimitOutOfRange(actual) => write!(
                formatter,
                "clock drift limit {actual} ppm exceeds {MAX_CLOCK_DRIFT_PPM} ppm"
            ),
            Self::ObservationOutlier {
                deviation_nanos,
                maximum,
            } => write!(
                formatter,
                "clock observation deviation {deviation_nanos} ns exceeds {maximum} ns"
            ),
            Self::Mapping(error) => write!(formatter, "clock mapping update failed: {error}"),
        }
    }
}

impl std::error::Error for ClockRecalibrationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Mapping(error) => Some(error),
            _ => None,
        }
    }
}

impl From<MappingError> for ClockRecalibrationError {
    fn from(error: MappingError) -> Self {
        Self::Mapping(error)
    }
}

#[derive(Debug)]
pub(crate) struct ClockRecalibrator {
    policy: Option<ClockRecalibrationPolicy>,
    observations: VecDeque<(ClockSnapshot, ClockSnapshot)>,
    accepted_recalibrations: u64,
    rejected_recalibrations: u64,
    current_drift_ppb: i64,
    anchor_generation: u64,
}

impl ClockRecalibrator {
    pub(crate) fn disabled(mapping: ClockMapping) -> Self {
        Self {
            policy: None,
            observations: VecDeque::new(),
            accepted_recalibrations: 0,
            rejected_recalibrations: 0,
            current_drift_ppb: mapping.drift_ppb(),
            anchor_generation: 0,
        }
    }

    pub(crate) fn configure(&mut self, policy: ClockRecalibrationPolicy) {
        self.policy = Some(policy);
        self.observations = VecDeque::with_capacity(policy.window_observations);
    }

    pub(crate) fn clear_observations(&mut self) {
        self.observations.clear();
    }

    pub(crate) fn reanchor(&mut self, mapping: ClockMapping) {
        self.observations.clear();
        self.current_drift_ppb = mapping.drift_ppb();
        self.anchor_generation = next_anchor_generation(self.anchor_generation);
    }

    pub(crate) fn telemetry(&self) -> ClockRecalibrationTelemetry {
        ClockRecalibrationTelemetry {
            observation_count: self.observations.len(),
            accepted_recalibrations: self.accepted_recalibrations,
            rejected_recalibrations: self.rejected_recalibrations,
            current_drift_ppb: self.current_drift_ppb,
            anchor_generation: self.anchor_generation,
        }
    }

    pub(crate) fn prepare_observation(
        &mut self,
        mapping: ClockMapping,
        source: ClockSnapshot,
        master: ClockSnapshot,
    ) -> Result<PreparedUpdate, ClockRecalibrationError> {
        let prepared = match self.prepare_update(mapping, source, master) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.rejected_recalibrations = self.rejected_recalibrations.saturating_add(1);
                return Err(error);
            }
        };
        Ok(prepared)
    }

    pub(crate) fn commit_observation(
        &mut self,
        prepared: &PreparedUpdate,
    ) -> ClockRecalibrationUpdate {
        if self.observations.len() == prepared.policy.window_observations {
            self.observations.pop_front();
        }
        self.observations
            .push_back((prepared.source, prepared.master));
        if let Some(drift_ppb) = prepared.drift_ppb {
            self.current_drift_ppb = drift_ppb;
            self.anchor_generation = next_anchor_generation(self.anchor_generation);
            self.accepted_recalibrations = self.accepted_recalibrations.saturating_add(1);
            ClockRecalibrationUpdate::Recalibrated {
                drift_ppb,
                clamped: prepared.clamped,
            }
        } else {
            ClockRecalibrationUpdate::Collecting {
                observations: self.observations.len(),
                required: prepared.policy.minimum_observations,
            }
        }
    }

    pub(crate) fn reject_prepared_observation(&mut self) {
        self.rejected_recalibrations = self.rejected_recalibrations.saturating_add(1);
    }

    fn prepare_update(
        &self,
        mapping: ClockMapping,
        source: ClockSnapshot,
        master: ClockSnapshot,
    ) -> Result<PreparedUpdate, ClockRecalibrationError> {
        let policy = self.policy.ok_or(ClockRecalibrationError::Disabled)?;
        let skip = usize::from(self.observations.len() == policy.window_observations);
        let mut estimator = DriftEstimator::new(mapping.source_domain(), mapping.master_domain());
        for &(observed_source, observed_master) in self.observations.iter().skip(skip) {
            estimator.observe(observed_source, observed_master)?;
        }
        estimator.observe(source, master)?;

        let mapped_master = mapping.map(source)?;
        if master.domain() != mapping.master_domain() {
            return Err(MappingError::DomainMismatch.into());
        }
        let deviation_nanos = mapped_master
            .time()
            .as_nanos()
            .abs_diff(master.time().as_nanos());
        if deviation_nanos > policy.max_observation_error_nanos {
            return Err(ClockRecalibrationError::ObservationOutlier {
                deviation_nanos,
                maximum: policy.max_observation_error_nanos,
            });
        }

        let projected_count = self
            .observations
            .len()
            .saturating_add(1)
            .min(policy.window_observations);
        if projected_count < policy.minimum_observations {
            return Ok(PreparedUpdate {
                policy,
                source,
                master,
                drift_ppb: None,
                clamped: false,
            });
        }

        let estimated_drift_ppb = estimator.estimated_drift_ppb()?;
        let maximum_ppb = i128::from(policy.max_drift_ppm) * 1_000;
        let bounded_drift_ppb = estimated_drift_ppb.clamp(-maximum_ppb, maximum_ppb);
        let drift_ppb =
            i64::try_from(bounded_drift_ppb).expect("policy-bounded clock drift always fits i64");
        Ok(PreparedUpdate {
            policy,
            source,
            master,
            drift_ppb: Some(drift_ppb),
            clamped: bounded_drift_ppb != estimated_drift_ppb,
        })
    }
}

pub(crate) struct PreparedUpdate {
    policy: ClockRecalibrationPolicy,
    source: ClockSnapshot,
    master: ClockSnapshot,
    drift_ppb: Option<i64>,
    clamped: bool,
}

impl PreparedUpdate {
    pub(crate) const fn drift_ppb(&self) -> Option<i64> {
        self.drift_ppb
    }
}

const fn next_anchor_generation(generation: u64) -> u64 {
    generation.saturating_add(1)
}

#[cfg(test)]
mod tests {
    use super::next_anchor_generation;

    #[test]
    fn anchor_generation_saturates() {
        assert_eq!(next_anchor_generation(u64::MAX - 1), u64::MAX);
        assert_eq!(next_anchor_generation(u64::MAX), u64::MAX);
    }
}
