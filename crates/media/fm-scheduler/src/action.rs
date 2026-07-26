use crate::{ActionId, FrameNumber};
use core::fmt;
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionError {
    SequenceExhausted,
    UnknownAction(ActionId),
}

impl fmt::Display for ActionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SequenceExhausted => formatter.write_str("action sequence exhausted"),
            Self::UnknownAction(id) => write!(formatter, "scheduled action {id} does not exist"),
        }
    }
}

impl std::error::Error for ActionError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduledAction<A> {
    pub id: ActionId,
    pub target_frame: FrameNumber,
    pub action: A,
}

#[derive(Clone, Debug)]
pub struct ActionQueue<A> {
    next_sequence: u64,
    ordered: BTreeMap<(FrameNumber, ActionId), ScheduledAction<A>>,
    by_id: HashMap<ActionId, (FrameNumber, ActionId)>,
}

impl<A> Default for ActionQueue<A> {
    fn default() -> Self {
        Self {
            next_sequence: 0,
            ordered: BTreeMap::new(),
            by_id: HashMap::new(),
        }
    }
}

impl<A> ActionQueue<A> {
    #[must_use]
    pub fn len(&self) -> usize {
        self.ordered.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ordered.is_empty()
    }

    /// Schedules an action, preserving insertion order within a frame.
    ///
    /// # Errors
    ///
    /// Returns [`ActionError::SequenceExhausted`] when no action identifier
    /// remains.
    pub fn schedule(
        &mut self,
        target_frame: FrameNumber,
        action: A,
    ) -> Result<ActionId, ActionError> {
        let id = self.allocate_id()?;
        self.insert(id, target_frame, action);
        Ok(id)
    }

    pub fn cancel(&mut self, id: ActionId) -> Option<ScheduledAction<A>> {
        let key = self.by_id.remove(&id)?;
        self.ordered.remove(&key)
    }

    /// Atomically replaces a scheduled action with a newly ordered action.
    ///
    /// # Errors
    ///
    /// Returns an error when `superseded` is unknown or the action sequence is
    /// exhausted. The original remains scheduled on error.
    pub fn supersede(
        &mut self,
        superseded: ActionId,
        target_frame: FrameNumber,
        action: A,
    ) -> Result<(ActionId, ScheduledAction<A>), ActionError> {
        if !self.by_id.contains_key(&superseded) {
            return Err(ActionError::UnknownAction(superseded));
        }
        let id = self.allocate_id()?;
        let previous = self
            .cancel(superseded)
            .ok_or(ActionError::UnknownAction(superseded))?;
        self.insert(id, target_frame, action);
        Ok((id, previous))
    }

    pub fn drain_due(&mut self, frame: FrameNumber) -> Vec<ScheduledAction<A>> {
        let keys: Vec<_> = self
            .ordered
            .range(..=(frame, ActionId::new(u64::MAX)))
            .map(|(key, _)| *key)
            .collect();
        keys.into_iter()
            .filter_map(|(_, id)| self.cancel(id))
            .collect()
    }

    fn allocate_id(&mut self) -> Result<ActionId, ActionError> {
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(ActionError::SequenceExhausted)?;
        Ok(ActionId::new(self.next_sequence))
    }

    fn insert(&mut self, id: ActionId, target_frame: FrameNumber, action: A) {
        let key = (target_frame, id);
        self.by_id.insert(id, key);
        self.ordered.insert(
            key,
            ScheduledAction {
                id,
                target_frame,
                action,
            },
        );
    }
}
