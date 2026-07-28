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
        self.anchor_generation = self.anchor_generation.wrapping_add(1);
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

    pub(crate) fn observe(
        &mut self,
        mapping: ClockMapping,
        source: ClockSnapshot,
        master: ClockSnapshot,
    ) -> Result<(ClockMapping, ClockRecalibrationUpdate), ClockRecalibrationError> {
        let prepared = match self.prepare_update(mapping, source, master) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.rejected_recalibrations = self.rejected_recalibrations.saturating_add(1);
                return Err(error);
            }
        };

        if self.observations.len() == prepared.policy.window_observations {
            self.observations.pop_front();
        }
        self.observations.push_back((source, master));
        if let Some(candidate) = prepared.mapping {
            self.current_drift_ppb = candidate.drift_ppb();
            self.anchor_generation = self.anchor_generation.wrapping_add(1);
            self.accepted_recalibrations = self.accepted_recalibrations.saturating_add(1);
            Ok((
                candidate,
                ClockRecalibrationUpdate::Recalibrated {
                    drift_ppb: candidate.drift_ppb(),
                    clamped: prepared.clamped,
                },
            ))
        } else {
            Ok((
                mapping,
                ClockRecalibrationUpdate::Collecting {
                    observations: self.observations.len(),
                    required: prepared.policy.minimum_observations,
                },
            ))
        }
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
                mapping: None,
                clamped: false,
            });
        }

        let estimate = estimator.mapping()?;
        let maximum_ppb = i64::from(policy.max_drift_ppm) * 1_000;
        let drift_ppb = estimate.drift_ppb().clamp(-maximum_ppb, maximum_ppb);
        let continuity_anchor = mapping.map(source)?;
        let candidate = ClockMapping::new(source, continuity_anchor, drift_ppb)?;
        debug_assert_eq!(candidate.map(source), Ok(continuity_anchor));
        Ok(PreparedUpdate {
            policy,
            mapping: Some(candidate),
            clamped: drift_ppb != estimate.drift_ppb(),
        })
    }
}

struct PreparedUpdate {
    policy: ClockRecalibrationPolicy,
    mapping: Option<ClockMapping>,
    clamped: bool,
}
