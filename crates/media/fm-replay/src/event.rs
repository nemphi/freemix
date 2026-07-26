use std::collections::{BTreeMap, BTreeSet};

use fm_frame::NormalizedTimestamp;

use crate::{CameraId, EventId, FolderId, ListId, ReplayError, TimelineRange};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReplayMarks {
    pub mark_in: Option<NormalizedTimestamp>,
    pub mark_out: Option<NormalizedTimestamp>,
}

impl ReplayMarks {
    pub fn set_in(&mut self, timestamp: NormalizedTimestamp) {
        self.mark_in = Some(timestamp);
    }

    pub fn set_out(&mut self, timestamp: NormalizedTimestamp) {
        self.mark_out = Some(timestamp);
    }

    /// Returns the marked half-open event range.
    ///
    /// # Errors
    ///
    /// Both marks must exist and be ordered.
    pub fn range(self) -> Result<TimelineRange, ReplayError> {
        TimelineRange::new(
            self.mark_in.ok_or(ReplayError::MarkIncomplete)?,
            self.mark_out.ok_or(ReplayError::MarkIncomplete)?,
        )
    }

    /// Creates a last-N-seconds range ending at the supplied live edge.
    ///
    /// # Errors
    ///
    /// Zero or overflowing durations are rejected.
    pub fn last_n(
        live_edge: NormalizedTimestamp,
        seconds: u64,
    ) -> Result<TimelineRange, ReplayError> {
        let duration = seconds
            .checked_mul(1_000_000_000)
            .and_then(|value| i64::try_from(value).ok())
            .ok_or(ReplayError::InvalidLastN)?;
        if duration == 0 {
            return Err(ReplayError::InvalidLastN);
        }
        let start = live_edge
            .as_nanos()
            .checked_sub(duration)
            .ok_or(ReplayError::InvalidLastN)?;
        TimelineRange::new(NormalizedTimestamp::from_nanos(start), live_edge)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayEvent {
    pub id: EventId,
    pub name: String,
    pub timeline: TimelineRange,
    pub angles: BTreeSet<CameraId>,
    pub preferred_angle: CameraId,
    pub tags: BTreeSet<String>,
    pub note: String,
    pub folders: BTreeSet<FolderId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventList {
    pub id: ListId,
    pub name: String,
    pub events: Vec<EventId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventFolder {
    pub id: FolderId,
    pub name: String,
}

#[derive(Clone, Debug, Default)]
pub struct EventDatabase {
    events: BTreeMap<EventId, ReplayEvent>,
    lists: BTreeMap<ListId, EventList>,
    folders: BTreeMap<FolderId, EventFolder>,
}

impl EventDatabase {
    /// Adds a multi-angle event.
    ///
    /// # Errors
    ///
    /// IDs must be unique, names and angle sets non-empty, and the preferred
    /// angle must be one of the event angles.
    pub fn insert_event(&mut self, event: ReplayEvent) -> Result<(), ReplayError> {
        if event.name.trim().is_empty() {
            return Err(ReplayError::EmptyName);
        }
        if !event.angles.contains(&event.preferred_angle) {
            return Err(ReplayError::PreferredAngleUnavailable(
                event.preferred_angle,
            ));
        }
        if self.events.contains_key(&event.id) {
            return Err(ReplayError::DuplicateEvent(event.id));
        }
        self.events.insert(event.id, event);
        Ok(())
    }

    #[must_use]
    pub fn event(&self, id: EventId) -> Option<&ReplayEvent> {
        self.events.get(&id)
    }

    pub fn events(&self) -> impl Iterator<Item = &ReplayEvent> {
        self.events.values()
    }

    /// Replaces an event note.
    ///
    /// # Errors
    ///
    /// The event must exist.
    pub fn set_note(&mut self, id: EventId, note: impl Into<String>) -> Result<(), ReplayError> {
        self.events
            .get_mut(&id)
            .ok_or(ReplayError::UnknownEvent(id))?
            .note = note.into();
        Ok(())
    }

    /// Adds a non-empty tag to an event.
    ///
    /// # Errors
    ///
    /// The event must exist and the tag must not be empty.
    pub fn add_tag(&mut self, id: EventId, tag: impl Into<String>) -> Result<(), ReplayError> {
        let tag = tag.into();
        if tag.trim().is_empty() {
            return Err(ReplayError::EmptyName);
        }
        self.events
            .get_mut(&id)
            .ok_or(ReplayError::UnknownEvent(id))?
            .tags
            .insert(tag);
        Ok(())
    }

    /// Adds a named folder.
    ///
    /// # Errors
    ///
    /// Empty names and duplicate folder IDs are rejected.
    pub fn add_folder(&mut self, folder: EventFolder) -> Result<(), ReplayError> {
        if folder.name.trim().is_empty() {
            return Err(ReplayError::EmptyName);
        }
        if self.folders.contains_key(&folder.id) {
            return Err(ReplayError::DuplicateFolder(folder.id));
        }
        self.folders.insert(folder.id, folder);
        Ok(())
    }

    /// Adds an event to a folder without removing other folder memberships.
    ///
    /// # Errors
    ///
    /// Both event and folder must exist.
    pub fn place_in_folder(
        &mut self,
        event_id: EventId,
        folder_id: FolderId,
    ) -> Result<(), ReplayError> {
        if !self.folders.contains_key(&folder_id) {
            return Err(ReplayError::UnknownFolder(folder_id));
        }
        self.events
            .get_mut(&event_id)
            .ok_or(ReplayError::UnknownEvent(event_id))?
            .folders
            .insert(folder_id);
        Ok(())
    }

    /// Adds an ordered named event list.
    ///
    /// # Errors
    ///
    /// Empty/duplicate lists and references to unknown events are rejected.
    pub fn add_list(&mut self, list: EventList) -> Result<(), ReplayError> {
        if list.name.trim().is_empty() {
            return Err(ReplayError::EmptyName);
        }
        if self.lists.contains_key(&list.id) {
            return Err(ReplayError::DuplicateList(list.id));
        }
        for event in &list.events {
            if !self.events.contains_key(event) {
                return Err(ReplayError::UnknownEvent(*event));
            }
        }
        self.lists.insert(list.id, list);
        Ok(())
    }

    /// Appends one event to an existing list.
    ///
    /// # Errors
    ///
    /// The list and event must exist, and an event may occur only once per list.
    pub fn append_to_list(
        &mut self,
        list_id: ListId,
        event_id: EventId,
    ) -> Result<(), ReplayError> {
        if !self.events.contains_key(&event_id) {
            return Err(ReplayError::UnknownEvent(event_id));
        }
        let list = self
            .lists
            .get_mut(&list_id)
            .ok_or(ReplayError::UnknownList(list_id))?;
        if list.events.contains(&event_id) {
            return Err(ReplayError::EventAlreadyInList {
                event: event_id,
                list: list_id,
            });
        }
        list.events.push(event_id);
        Ok(())
    }

    #[must_use]
    pub fn list(&self, id: ListId) -> Option<&EventList> {
        self.lists.get(&id)
    }

    #[must_use]
    pub fn folder(&self, id: FolderId) -> Option<&EventFolder> {
        self.folders.get(&id)
    }
}
