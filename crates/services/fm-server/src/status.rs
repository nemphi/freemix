use std::{error::Error, fmt};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HealthState {
    Healthy,
    Unhealthy,
}

impl HealthState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Unhealthy => "unhealthy",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadinessState {
    Starting,
    Ready,
    Draining,
    Unhealthy,
}

impl ReadinessState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Draining => "draining",
            Self::Unhealthy => "unhealthy",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceStatus {
    health: HealthState,
    readiness: ReadinessState,
}

impl Default for ServiceStatus {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceStatus {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            health: HealthState::Healthy,
            readiness: ReadinessState::Starting,
        }
    }

    #[must_use]
    pub const fn health(self) -> HealthState {
        self.health
    }

    #[must_use]
    pub const fn readiness(self) -> ReadinessState {
        self.readiness
    }

    /// Marks startup complete. Draining and unhealthy states are terminal.
    ///
    /// # Errors
    ///
    /// Returns an invalid-transition error if the service is no longer
    /// starting.
    pub fn mark_ready(&mut self) -> Result<(), StatusTransitionError> {
        if self.readiness != ReadinessState::Starting {
            return Err(StatusTransitionError {
                from: self.readiness,
                requested: ReadinessState::Ready,
            });
        }
        self.readiness = ReadinessState::Ready;
        Ok(())
    }

    /// Stops admission of new sessions while preserving liveness.
    ///
    /// # Errors
    ///
    /// Returns an invalid-transition error unless the service is ready.
    pub fn begin_draining(&mut self) -> Result<(), StatusTransitionError> {
        if self.readiness != ReadinessState::Ready {
            return Err(StatusTransitionError {
                from: self.readiness,
                requested: ReadinessState::Draining,
            });
        }
        self.readiness = ReadinessState::Draining;
        Ok(())
    }

    pub fn mark_unhealthy(&mut self) {
        self.health = HealthState::Unhealthy;
        self.readiness = ReadinessState::Unhealthy;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatusTransitionError {
    pub from: ReadinessState,
    pub requested: ReadinessState,
}

impl fmt::Display for StatusTransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "cannot transition readiness from {} to {}",
            self.from.as_str(),
            self.requested.as_str()
        )
    }
}

impl Error for StatusTransitionError {}
