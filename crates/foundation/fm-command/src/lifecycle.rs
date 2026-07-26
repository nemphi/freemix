use crate::{Revision, RuntimeGeneration, RuntimeSequence};
use core::fmt;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureDisposition {
    RolledBack,
    RetainedForRetry,
    FallbackRealized,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecyclePhase {
    Accepted,
    Preparing,
    Scheduled,
    Realized,
    Failed,
    Superseded,
}

impl LifecyclePhase {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Realized | Self::Failed | Self::Superseded)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduledDomain<D> {
    pub domain: D,
    pub boundary: u64,
}

impl<D> ScheduledDomain<D> {
    #[must_use]
    pub const fn new(domain: D, boundary: u64) -> Self {
        Self { domain, boundary }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LifecycleEvent<D, F> {
    Accepted,
    Preparing,
    Scheduled {
        domains: Vec<ScheduledDomain<D>>,
    },
    Realized {
        domain: D,
        generation: RuntimeGeneration,
    },
    Failed {
        failure: F,
        disposition: FailureDisposition,
    },
    Superseded {
        by_revision: Revision,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleRecord<D, F> {
    pub revision: Revision,
    pub sequence: RuntimeSequence,
    pub event: LifecycleEvent<D, F>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleError {
    InvalidTransition {
        from: LifecyclePhase,
        to: LifecyclePhase,
    },
    EmptySchedule,
    DuplicateDomain,
    UnknownDomain,
    DomainAlreadyRealized,
    InvalidSupersedingRevision,
    SequenceExhausted,
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTransition { from, to } => {
                write!(
                    formatter,
                    "invalid lifecycle transition from {from:?} to {to:?}"
                )
            }
            Self::EmptySchedule => formatter.write_str("a schedule must contain a clock domain"),
            Self::DuplicateDomain => formatter.write_str("clock domain appears more than once"),
            Self::UnknownDomain => formatter.write_str("clock domain is not scheduled"),
            Self::DomainAlreadyRealized => formatter.write_str("clock domain was already realized"),
            Self::InvalidSupersedingRevision => {
                formatter.write_str("superseding revision must be newer")
            }
            Self::SequenceExhausted => formatter.write_str("runtime event sequence exhausted"),
        }
    }
}

impl std::error::Error for LifecycleError {}

#[derive(Clone, Debug)]
pub struct Lifecycle<D, F> {
    revision: Revision,
    phase: LifecyclePhase,
    sequence: RuntimeSequence,
    scheduled: HashSet<D>,
    realized: HashMap<D, RuntimeGeneration>,
    records: Vec<LifecycleRecord<D, F>>,
}

impl<D, F> Lifecycle<D, F>
where
    D: Clone + Eq + Hash,
{
    #[must_use]
    pub fn new(revision: Revision) -> Self {
        Self {
            revision,
            phase: LifecyclePhase::Accepted,
            sequence: RuntimeSequence::new(1),
            scheduled: HashSet::new(),
            realized: HashMap::new(),
            records: vec![LifecycleRecord {
                revision,
                sequence: RuntimeSequence::new(1),
                event: LifecycleEvent::Accepted,
            }],
        }
    }

    #[must_use]
    pub const fn revision(&self) -> Revision {
        self.revision
    }

    #[must_use]
    pub const fn phase(&self) -> LifecyclePhase {
        self.phase
    }

    #[must_use]
    pub fn records(&self) -> &[LifecycleRecord<D, F>] {
        &self.records
    }

    #[must_use]
    pub fn generation(&self, domain: &D) -> Option<RuntimeGeneration> {
        self.realized.get(domain).copied()
    }

    /// Starts preparing resources for the accepted revision.
    ///
    /// # Errors
    ///
    /// Returns [`LifecycleError::InvalidTransition`] unless the revision was
    /// just accepted, or a sequence exhaustion error.
    pub fn begin_preparing(&mut self) -> Result<(), LifecycleError> {
        self.require_transition(LifecyclePhase::Preparing, &[LifecyclePhase::Accepted])?;
        self.push(LifecycleEvent::Preparing)?;
        self.phase = LifecyclePhase::Preparing;
        Ok(())
    }

    /// Schedules all required clock domains.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid transition, an empty schedule, duplicate
    /// domains, or exhausted runtime sequence.
    pub fn schedule(
        &mut self,
        domains: impl IntoIterator<Item = ScheduledDomain<D>>,
    ) -> Result<(), LifecycleError> {
        self.require_transition(LifecyclePhase::Scheduled, &[LifecyclePhase::Preparing])?;
        let domains: Vec<_> = domains.into_iter().collect();
        if domains.is_empty() {
            return Err(LifecycleError::EmptySchedule);
        }

        let scheduled: HashSet<_> = domains.iter().map(|entry| entry.domain.clone()).collect();
        if scheduled.len() != domains.len() {
            return Err(LifecycleError::DuplicateDomain);
        }

        self.push(LifecycleEvent::Scheduled { domains })?;
        self.scheduled = scheduled;
        self.phase = LifecyclePhase::Scheduled;
        Ok(())
    }

    /// Marks one scheduled clock domain as realized.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid transition, an unknown or already
    /// realized domain, or exhausted runtime sequence.
    pub fn realize(
        &mut self,
        domain: D,
        generation: RuntimeGeneration,
    ) -> Result<(), LifecycleError> {
        self.require_transition(LifecyclePhase::Realized, &[LifecyclePhase::Scheduled])?;
        if !self.scheduled.contains(&domain) {
            return Err(LifecycleError::UnknownDomain);
        }
        if self.realized.contains_key(&domain) {
            return Err(LifecycleError::DomainAlreadyRealized);
        }

        self.push(LifecycleEvent::Realized {
            domain: domain.clone(),
            generation,
        })?;
        self.realized.insert(domain, generation);
        if self.realized.len() == self.scheduled.len() {
            self.phase = LifecyclePhase::Realized;
        }
        Ok(())
    }

    /// Terminates this lifecycle with a realization failure.
    ///
    /// # Errors
    ///
    /// Returns an error when the lifecycle is already terminal or its runtime
    /// sequence is exhausted.
    pub fn fail(
        &mut self,
        failure: F,
        disposition: FailureDisposition,
    ) -> Result<(), LifecycleError> {
        self.require_transition(
            LifecyclePhase::Failed,
            &[
                LifecyclePhase::Accepted,
                LifecyclePhase::Preparing,
                LifecyclePhase::Scheduled,
            ],
        )?;
        self.push(LifecycleEvent::Failed {
            failure,
            disposition,
        })?;
        self.phase = LifecyclePhase::Failed;
        Ok(())
    }

    /// Terminates this lifecycle because a newer revision replaced it.
    ///
    /// # Errors
    ///
    /// Returns an error when the lifecycle is already terminal, the supplied
    /// revision is not newer, or its runtime sequence is exhausted.
    pub fn supersede(&mut self, by_revision: Revision) -> Result<(), LifecycleError> {
        self.require_transition(
            LifecyclePhase::Superseded,
            &[
                LifecyclePhase::Accepted,
                LifecyclePhase::Preparing,
                LifecyclePhase::Scheduled,
            ],
        )?;
        if by_revision <= self.revision {
            return Err(LifecycleError::InvalidSupersedingRevision);
        }
        self.push(LifecycleEvent::Superseded { by_revision })?;
        self.phase = LifecyclePhase::Superseded;
        Ok(())
    }

    fn require_transition(
        &self,
        to: LifecyclePhase,
        allowed: &[LifecyclePhase],
    ) -> Result<(), LifecycleError> {
        if allowed.contains(&self.phase) {
            Ok(())
        } else {
            Err(LifecycleError::InvalidTransition {
                from: self.phase,
                to,
            })
        }
    }

    fn push(&mut self, event: LifecycleEvent<D, F>) -> Result<(), LifecycleError> {
        let sequence = self
            .sequence
            .checked_next()
            .map_err(|_| LifecycleError::SequenceExhausted)?;
        self.sequence = sequence;
        self.records.push(LifecycleRecord {
            revision: self.revision,
            sequence,
            event,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests;
