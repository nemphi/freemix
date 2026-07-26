use crate::{
    ClipId, ClipLibrary, EndBehavior, FrameCodec, GoId, LibraryError, Marks, PlaybackError,
    PlaybackFrame, PlaylistEntryId, ScheduleCoordinate, Speed, SpeedDirection, Transport,
};
use core::fmt;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndAction {
    Stop,
    Loop,
    Next(PlaylistEntryId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaylistEntry {
    id: PlaylistEntryId,
    clip_id: ClipId,
    marks: Option<Marks>,
    speed: Speed,
    end_action: EndAction,
}

impl PlaylistEntry {
    #[must_use]
    pub const fn new(id: PlaylistEntryId, clip_id: ClipId) -> Self {
        Self {
            id,
            clip_id,
            marks: None,
            speed: Speed::Forward1x,
            end_action: EndAction::Stop,
        }
    }

    #[must_use]
    pub const fn with_marks(mut self, marks: Marks) -> Self {
        self.marks = Some(marks);
        self
    }

    #[must_use]
    pub const fn with_speed(mut self, speed: Speed) -> Self {
        self.speed = speed;
        self
    }

    #[must_use]
    pub const fn with_end_action(mut self, end_action: EndAction) -> Self {
        self.end_action = end_action;
        self
    }

    #[must_use]
    pub const fn id(self) -> PlaylistEntryId {
        self.id
    }

    #[must_use]
    pub const fn clip_id(self) -> ClipId {
        self.clip_id
    }

    #[must_use]
    pub const fn marks(self) -> Option<Marks> {
        self.marks
    }

    #[must_use]
    pub const fn speed(self) -> Speed {
        self.speed
    }

    #[must_use]
    pub const fn end_action(self) -> EndAction {
        self.end_action
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaylistError {
    Empty,
    DuplicateEntry(PlaylistEntryId),
    MissingEntry(PlaylistEntryId),
    DanglingNext {
        entry: PlaylistEntryId,
        next: PlaylistEntryId,
    },
    Library(LibraryError),
    InvalidMarks {
        entry: PlaylistEntryId,
        source: PlaybackError,
    },
}

impl fmt::Display for PlaylistError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("a playlist must contain at least one entry"),
            Self::DuplicateEntry(id) => write!(formatter, "playlist entry {id} is duplicated"),
            Self::MissingEntry(id) => write!(formatter, "playlist entry {id} does not exist"),
            Self::DanglingNext { entry, next } => {
                write!(
                    formatter,
                    "playlist entry {entry} points to missing entry {next}"
                )
            }
            Self::Library(error) => error.fmt(formatter),
            Self::InvalidMarks { entry, source } => {
                write!(
                    formatter,
                    "playlist entry {entry} has invalid marks: {source}"
                )
            }
        }
    }
}

impl std::error::Error for PlaylistError {}

impl From<LibraryError> for PlaylistError {
    fn from(value: LibraryError) -> Self {
        Self::Library(value)
    }
}

#[derive(Clone, Debug)]
pub struct Playlist {
    entries: BTreeMap<PlaylistEntryId, PlaylistEntry>,
    first: PlaylistEntryId,
}

impl Playlist {
    /// Builds a playlist and validates every explicit next edge.
    ///
    /// Vector order selects the first entry; all later addressing uses stable
    /// entry IDs.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty list, duplicate IDs, or a missing next
    /// target.
    pub fn new(entries: Vec<PlaylistEntry>) -> Result<Self, PlaylistError> {
        let first = entries.first().ok_or(PlaylistError::Empty)?.id();
        let mut by_id = BTreeMap::new();
        for entry in entries {
            let id = entry.id();
            if by_id.insert(id, entry).is_some() {
                return Err(PlaylistError::DuplicateEntry(id));
            }
        }
        for entry in by_id.values() {
            if let EndAction::Next(next) = entry.end_action()
                && !by_id.contains_key(&next)
            {
                return Err(PlaylistError::DanglingNext {
                    entry: entry.id(),
                    next,
                });
            }
        }
        Ok(Self {
            entries: by_id,
            first,
        })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub const fn first(&self) -> PlaylistEntryId {
        self.first
    }

    #[must_use]
    pub fn entry(&self, id: PlaylistEntryId) -> Option<&PlaylistEntry> {
        self.entries.get(&id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProgrammedGo {
    pub id: GoId,
    pub coordinate: ScheduleCoordinate,
    pub entry: PlaylistEntryId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoStatus {
    Pending,
    Cancelled,
    Executed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoScheduleOutcome {
    Scheduled,
    Unchanged(GoStatus),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelGoOutcome {
    Cancelled,
    AlreadyCancelled,
    AlreadyExecuted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoError {
    UnknownGo(GoId),
    ConflictingGo(GoId),
    CoordinateElapsed(ScheduleCoordinate),
    CoordinateReversed {
        previous: ScheduleCoordinate,
        requested: ScheduleCoordinate,
    },
    Playlist(PlaylistError),
}

impl fmt::Display for GoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownGo(id) => write!(formatter, "programmed GO {id} does not exist"),
            Self::ConflictingGo(id) => {
                write!(
                    formatter,
                    "programmed GO {id} was reused with different data"
                )
            }
            Self::CoordinateElapsed(coordinate) => {
                write!(formatter, "schedule coordinate {coordinate} has elapsed")
            }
            Self::CoordinateReversed {
                previous,
                requested,
            } => write!(
                formatter,
                "coordinate {requested} precedes previously applied coordinate {previous}"
            ),
            Self::Playlist(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for GoError {}

impl From<PlaylistError> for GoError {
    fn from(value: PlaylistError) -> Self {
        Self::Playlist(value)
    }
}

#[derive(Clone, Copy, Debug)]
struct GoRecord {
    command: ProgrammedGo,
    status: GoStatus,
}

#[derive(Clone, Debug)]
pub struct PlaylistPlayer<F> {
    library: ClipLibrary<F>,
    playlist: Playlist,
    current_entry: Option<PlaylistEntryId>,
    transport: Option<Transport<F>>,
    go_records: BTreeMap<GoId, GoRecord>,
    pending_go: BTreeSet<(ScheduleCoordinate, GoId)>,
    applied_coordinate: Option<ScheduleCoordinate>,
}

impl<F: Clone> PlaylistPlayer<F> {
    /// Creates a player after validating every clip reference and marked range.
    ///
    /// # Errors
    ///
    /// Returns an error when an entry references a missing clip or has marks
    /// outside that clip.
    pub fn new(library: ClipLibrary<F>, playlist: Playlist) -> Result<Self, PlaylistError> {
        for entry in playlist.entries.values() {
            let clip = library
                .get(entry.clip_id())
                .ok_or(LibraryError::MissingClip(entry.clip_id()))?;
            if let Some(marks) = entry.marks() {
                let mut transport = Transport::new(clip.clone());
                transport
                    .set_marks(marks)
                    .map_err(|source| PlaylistError::InvalidMarks {
                        entry: entry.id(),
                        source,
                    })?;
            }
        }
        Ok(Self {
            library,
            playlist,
            current_entry: None,
            transport: None,
            go_records: BTreeMap::new(),
            pending_go: BTreeSet::new(),
            applied_coordinate: None,
        })
    }

    #[must_use]
    pub const fn current_entry(&self) -> Option<PlaylistEntryId> {
        self.current_entry
    }

    #[must_use]
    pub const fn transport(&self) -> Option<&Transport<F>> {
        self.transport.as_ref()
    }

    /// Immediately cues and applies an entry's programmed speed.
    ///
    /// # Errors
    ///
    /// Returns [`PlaylistError::MissingEntry`] for an unknown entry ID.
    pub fn go(&mut self, entry: PlaylistEntryId) -> Result<(), PlaylistError> {
        self.activate(entry)
    }

    /// Schedules an idempotent programmed GO at an exact frame coordinate.
    ///
    /// Repeating the same ID and payload never creates a second command.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown entry, conflicting ID reuse, or a new
    /// command targeting an already applied coordinate.
    pub fn schedule_go(&mut self, command: ProgrammedGo) -> Result<GoScheduleOutcome, GoError> {
        if self.playlist.entry(command.entry).is_none() {
            return Err(PlaylistError::MissingEntry(command.entry).into());
        }
        if let Some(record) = self.go_records.get(&command.id) {
            return if record.command == command {
                Ok(GoScheduleOutcome::Unchanged(record.status))
            } else {
                Err(GoError::ConflictingGo(command.id))
            };
        }
        if self
            .applied_coordinate
            .is_some_and(|coordinate| command.coordinate <= coordinate)
        {
            return Err(GoError::CoordinateElapsed(command.coordinate));
        }
        self.pending_go.insert((command.coordinate, command.id));
        self.go_records.insert(
            command.id,
            GoRecord {
                command,
                status: GoStatus::Pending,
            },
        );
        Ok(GoScheduleOutcome::Scheduled)
    }

    /// Cancels a programmed GO while retaining its terminal status.
    ///
    /// # Errors
    ///
    /// Returns [`GoError::UnknownGo`] only when the ID was never scheduled.
    pub fn cancel_go(&mut self, id: GoId) -> Result<CancelGoOutcome, GoError> {
        let record = self.go_records.get_mut(&id).ok_or(GoError::UnknownGo(id))?;
        match record.status {
            GoStatus::Pending => {
                self.pending_go.remove(&(record.command.coordinate, id));
                record.status = GoStatus::Cancelled;
                Ok(CancelGoOutcome::Cancelled)
            }
            GoStatus::Cancelled => Ok(CancelGoOutcome::AlreadyCancelled),
            GoStatus::Executed => Ok(CancelGoOutcome::AlreadyExecuted),
        }
    }

    #[must_use]
    pub fn go_status(&self, id: GoId) -> Option<GoStatus> {
        self.go_records.get(&id).map(|record| record.status)
    }

    /// Applies all due commands ordered by coordinate and then stable GO ID.
    ///
    /// # Errors
    ///
    /// Returns an error if coordinates move backwards or playlist activation
    /// fails.
    pub fn apply_scheduled(
        &mut self,
        coordinate: ScheduleCoordinate,
    ) -> Result<Vec<GoId>, GoError> {
        if let Some(previous) = self.applied_coordinate
            && coordinate < previous
        {
            return Err(GoError::CoordinateReversed {
                previous,
                requested: coordinate,
            });
        }
        let due: Vec<_> = self
            .pending_go
            .range(..=(coordinate, GoId::new(u128::MAX)))
            .copied()
            .collect();
        let mut executed = Vec::with_capacity(due.len());
        for (target, id) in due {
            let entry = self.go_records[&id].command.entry;
            self.activate(entry)?;
            self.pending_go.remove(&(target, id));
            if let Some(record) = self.go_records.get_mut(&id) {
                record.status = GoStatus::Executed;
            }
            executed.push(id);
        }
        self.applied_coordinate = Some(coordinate);
        Ok(executed)
    }

    /// Pulls one frame and performs the current entry's programmed end action.
    ///
    /// # Errors
    ///
    /// Returns an error when no entry is active or frame retrieval fails.
    pub fn pull_frame(
        &mut self,
        codec: Option<&mut dyn FrameCodec<F>>,
    ) -> Result<PlaybackFrame<F>, PlayerError> {
        let entry_id = self.current_entry.ok_or(PlayerError::NoActiveEntry)?;
        let output = self
            .transport
            .as_mut()
            .ok_or(PlayerError::NoActiveEntry)?
            .pull_frame(codec)?;
        if output.ended {
            let end_action = self
                .playlist
                .entry(entry_id)
                .ok_or(PlaylistError::MissingEntry(entry_id))?
                .end_action();
            match end_action {
                EndAction::Stop => {}
                EndAction::Loop => self.activate(entry_id)?,
                EndAction::Next(next) => self.activate(next)?,
            }
        }
        Ok(output)
    }

    fn activate(&mut self, entry_id: PlaylistEntryId) -> Result<(), PlaylistError> {
        let entry = *self
            .playlist
            .entry(entry_id)
            .ok_or(PlaylistError::MissingEntry(entry_id))?;
        let clip = self
            .library
            .get(entry.clip_id())
            .ok_or(LibraryError::MissingClip(entry.clip_id()))?
            .clone();
        let mut transport = Transport::new(clip);
        if let Some(marks) = entry.marks() {
            transport
                .set_marks(marks)
                .map_err(|source| PlaylistError::InvalidMarks {
                    entry: entry_id,
                    source,
                })?;
        }
        if entry.speed().direction() == Some(SpeedDirection::Reverse) {
            transport
                .seek(transport.marks().mark_out)
                .map_err(|source| PlaylistError::InvalidMarks {
                    entry: entry_id,
                    source,
                })?;
        }
        if entry.end_action() == EndAction::Stop {
            transport.set_end_behavior(EndBehavior::Stop);
        }
        transport.set_speed(entry.speed());
        self.current_entry = Some(entry_id);
        self.transport = Some(transport);
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlayerError {
    NoActiveEntry,
    Playback(PlaybackError),
    Playlist(PlaylistError),
}

impl fmt::Display for PlayerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoActiveEntry => formatter.write_str("no playlist entry is active"),
            Self::Playback(error) => error.fmt(formatter),
            Self::Playlist(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for PlayerError {}

impl From<PlaybackError> for PlayerError {
    fn from(value: PlaybackError) -> Self {
        Self::Playback(value)
    }
}

impl From<PlaylistError> for PlayerError {
    fn from(value: PlaylistError) -> Self {
        Self::Playlist(value)
    }
}
