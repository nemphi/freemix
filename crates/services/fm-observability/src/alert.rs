use std::{error::Error, fmt};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThresholdDirection {
    Above,
    Below,
}

/// Trigger and clear thresholds separated by a required hysteresis band.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AlertPolicy {
    direction: ThresholdDirection,
    trigger: f64,
    clear: f64,
}

impl AlertPolicy {
    /// Creates a finite policy with a non-empty hysteresis band.
    ///
    /// # Errors
    ///
    /// For an above policy, `clear` must be below `trigger`; for a below
    /// policy, `clear` must be above `trigger`.
    pub fn new(
        direction: ThresholdDirection,
        trigger: f64,
        clear: f64,
    ) -> Result<Self, AlertPolicyError> {
        if !trigger.is_finite() || !clear.is_finite() {
            return Err(AlertPolicyError::NonFinite);
        }
        let valid = match direction {
            ThresholdDirection::Above => clear < trigger,
            ThresholdDirection::Below => clear > trigger,
        };
        if !valid {
            return Err(AlertPolicyError::InvalidHysteresis);
        }
        Ok(Self {
            direction,
            trigger,
            clear,
        })
    }

    #[must_use]
    pub const fn direction(self) -> ThresholdDirection {
        self.direction
    }

    #[must_use]
    pub const fn trigger(self) -> f64 {
        self.trigger
    }

    #[must_use]
    pub const fn clear(self) -> f64 {
        self.clear
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AlertState {
    Inactive,
    Firing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AlertTransition {
    None,
    Fired,
    Cleared,
}

/// Stateful evaluation of one threshold policy.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThresholdAlert {
    policy: AlertPolicy,
    state: AlertState,
}

impl ThresholdAlert {
    #[must_use]
    pub const fn new(policy: AlertPolicy) -> Self {
        Self {
            policy,
            state: AlertState::Inactive,
        }
    }

    #[must_use]
    pub const fn policy(self) -> AlertPolicy {
        self.policy
    }

    #[must_use]
    pub const fn state(self) -> AlertState {
        self.state
    }

    /// Applies a sample, returning only edge transitions.
    ///
    /// # Errors
    ///
    /// Returns [`AlertPolicyError::NonFinite`] for a non-finite sample.
    pub fn evaluate(&mut self, value: f64) -> Result<AlertTransition, AlertPolicyError> {
        if !value.is_finite() {
            return Err(AlertPolicyError::NonFinite);
        }
        let fire = match self.policy.direction {
            ThresholdDirection::Above => value >= self.policy.trigger,
            ThresholdDirection::Below => value <= self.policy.trigger,
        };
        let clear = match self.policy.direction {
            ThresholdDirection::Above => value <= self.policy.clear,
            ThresholdDirection::Below => value >= self.policy.clear,
        };
        match (self.state, fire, clear) {
            (AlertState::Inactive, true, _) => {
                self.state = AlertState::Firing;
                Ok(AlertTransition::Fired)
            }
            (AlertState::Firing, _, true) => {
                self.state = AlertState::Inactive;
                Ok(AlertTransition::Cleared)
            }
            _ => Ok(AlertTransition::None),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AlertPolicyError {
    NonFinite,
    InvalidHysteresis,
}

impl fmt::Display for AlertPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFinite => formatter.write_str("alert thresholds and samples must be finite"),
            Self::InvalidHysteresis => {
                formatter.write_str("alert clear threshold must define a hysteresis band")
            }
        }
    }
}

impl Error for AlertPolicyError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn above_threshold_does_not_flap_inside_hysteresis_band() {
        let policy = AlertPolicy::new(ThresholdDirection::Above, 90.0, 80.0).unwrap();
        let mut alert = ThresholdAlert::new(policy);

        assert_eq!(alert.evaluate(90.0), Ok(AlertTransition::Fired));
        assert_eq!(alert.evaluate(85.0), Ok(AlertTransition::None));
        assert_eq!(alert.evaluate(89.9), Ok(AlertTransition::None));
        assert_eq!(alert.evaluate(80.0), Ok(AlertTransition::Cleared));
        assert_eq!(alert.evaluate(85.0), Ok(AlertTransition::None));
    }

    #[test]
    fn below_threshold_has_mirrored_hysteresis() {
        let policy = AlertPolicy::new(ThresholdDirection::Below, 10.0, 20.0).unwrap();
        let mut alert = ThresholdAlert::new(policy);
        assert_eq!(alert.evaluate(9.0), Ok(AlertTransition::Fired));
        assert_eq!(alert.evaluate(15.0), Ok(AlertTransition::None));
        assert_eq!(alert.evaluate(20.0), Ok(AlertTransition::Cleared));
    }
}
