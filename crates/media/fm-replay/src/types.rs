use core::{fmt, num::NonZeroU128};

use fm_frame::NormalizedTimestamp;

macro_rules! stable_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(NonZeroU128);

        impl $name {
            #[must_use]
            pub const fn new(value: NonZeroU128) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn get(self) -> NonZeroU128 {
                self.0
            }
        }

        impl From<NonZeroU128> for $name {
            fn from(value: NonZeroU128) -> Self {
                Self::new(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

stable_id!(CameraId);
stable_id!(StorageRootId);
stable_id!(SegmentId);
stable_id!(EventId);
stable_id!(ListId);
stable_id!(FolderId);
stable_id!(HighlightId);
stable_id!(MusicTrackId);
stable_id!(ExportJobId);

/// Half-open range on the shared replay timeline.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TimelineRange {
    pub start: NormalizedTimestamp,
    pub end: NormalizedTimestamp,
}

impl TimelineRange {
    /// Creates a non-empty half-open range.
    ///
    /// # Errors
    ///
    /// Returns [`ReplayError::InvalidRange`] unless `start < end`.
    pub fn new(start: NormalizedTimestamp, end: NormalizedTimestamp) -> Result<Self, ReplayError> {
        if start >= end {
            return Err(ReplayError::InvalidRange);
        }
        Ok(Self { start, end })
    }

    #[must_use]
    pub fn duration_nanos(self) -> u64 {
        let duration = i128::from(self.end.as_nanos()) - i128::from(self.start.as_nanos());
        u64::try_from(duration).unwrap_or(u64::MAX)
    }

    #[must_use]
    pub const fn contains(self, timestamp: NormalizedTimestamp) -> bool {
        timestamp.as_nanos() >= self.start.as_nanos() && timestamp.as_nanos() < self.end.as_nanos()
    }

    #[must_use]
    pub const fn overlaps(self, other: Self) -> bool {
        self.start.as_nanos() < other.end.as_nanos() && other.start.as_nanos() < self.end.as_nanos()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplayError {
    InvalidRange,
    EmptyName,
    TooManySources(usize),
    TooManyAudioChannels(usize),
    DuplicateCamera(CameraId),
    DuplicateStorageRoot(StorageRootId),
    DuplicateSegment(SegmentId),
    DuplicateEvent(EventId),
    DuplicateList(ListId),
    DuplicateFolder(FolderId),
    DuplicateHighlight(HighlightId),
    DuplicateExport(ExportJobId),
    UnknownCamera(CameraId),
    UnknownStorageRoot(StorageRootId),
    UnknownSegment(SegmentId),
    UnknownEvent(EventId),
    UnknownList(ListId),
    UnknownFolder(FolderId),
    UnknownExport(ExportJobId),
    CameraRootMismatch,
    TimelineRegression(CameraId),
    SequenceRegression(CameraId),
    DuplicateQuadCamera(CameraId),
    ObservationUnavailable(CameraId),
    MarkIncomplete,
    InvalidLastN,
    PreferredAngleUnavailable(CameraId),
    EventAlreadyInList { event: EventId, list: ListId },
    InvalidPlaybackRate(i32),
    FrameDurationZero,
    ExportStateTransition,
}

impl fmt::Display for ReplayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRange => {
                formatter.write_str("timeline range must be non-empty and ordered")
            }
            Self::EmptyName => formatter.write_str("name must not be empty"),
            Self::TooManySources(count) => {
                write!(formatter, "{count} replay sources exceed the limit")
            }
            Self::TooManyAudioChannels(count) => write!(
                formatter,
                "{count} audio channels exceed the replay source limit"
            ),
            Self::DuplicateCamera(id) => write!(formatter, "duplicate replay camera {id}"),
            Self::DuplicateStorageRoot(id) => write!(formatter, "duplicate storage root {id}"),
            Self::DuplicateSegment(id) => write!(formatter, "duplicate replay segment {id}"),
            Self::DuplicateEvent(id) => write!(formatter, "duplicate replay event {id}"),
            Self::DuplicateList(id) => write!(formatter, "duplicate replay list {id}"),
            Self::DuplicateFolder(id) => write!(formatter, "duplicate event folder {id}"),
            Self::DuplicateHighlight(id) => write!(formatter, "duplicate highlight item {id}"),
            Self::DuplicateExport(id) => write!(formatter, "duplicate export job {id}"),
            Self::UnknownCamera(id) => write!(formatter, "unknown replay camera {id}"),
            Self::UnknownStorageRoot(id) => write!(formatter, "unknown storage root {id}"),
            Self::UnknownSegment(id) => write!(formatter, "unknown replay segment {id}"),
            Self::UnknownEvent(id) => write!(formatter, "unknown replay event {id}"),
            Self::UnknownList(id) => write!(formatter, "unknown replay list {id}"),
            Self::UnknownFolder(id) => write!(formatter, "unknown event folder {id}"),
            Self::UnknownExport(id) => write!(formatter, "unknown export job {id}"),
            Self::CameraRootMismatch => {
                formatter.write_str("segment storage root does not match its camera")
            }
            Self::TimelineRegression(id) => write!(formatter, "timeline regressed for camera {id}"),
            Self::SequenceRegression(id) => {
                write!(formatter, "source sequence did not advance for camera {id}")
            }
            Self::DuplicateQuadCamera(id) => {
                write!(formatter, "camera {id} occurs more than once in quad view")
            }
            Self::ObservationUnavailable(id) => write!(
                formatter,
                "camera {id} has no observation at the requested time"
            ),
            Self::MarkIncomplete => formatter.write_str("mark-in and mark-out are both required"),
            Self::InvalidLastN => {
                formatter.write_str("last-N duration must be positive and fit the timeline")
            }
            Self::PreferredAngleUnavailable(id) => {
                write!(formatter, "preferred camera {id} is not an event angle")
            }
            Self::EventAlreadyInList { event, list } => {
                write!(formatter, "event {event} is already in list {list}")
            }
            Self::InvalidPlaybackRate(rate) => write!(
                formatter,
                "playback rate {rate} milli-x is outside -16000..=16000"
            ),
            Self::FrameDurationZero => formatter.write_str("frame duration must be nonzero"),
            Self::ExportStateTransition => {
                formatter.write_str("invalid export job state transition")
            }
        }
    }
}

impl std::error::Error for ReplayError {}
