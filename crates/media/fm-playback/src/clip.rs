use crate::{ClipId, FrameIndex};
use core::fmt;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipError {
    Empty,
}

impl fmt::Display for ClipError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a clip must declare at least one frame")
    }
}

impl std::error::Error for ClipError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CodecError {
    Unavailable,
    OpenFailed(String),
    DecodeFailed { frame: FrameIndex, reason: String },
}

impl fmt::Display for CodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("codec is unavailable"),
            Self::OpenFailed(reason) => write!(formatter, "codec open failed: {reason}"),
            Self::DecodeFailed { frame, reason } => {
                write!(formatter, "codec failed to decode frame {frame}: {reason}")
            }
        }
    }
}

impl std::error::Error for CodecError {}

/// The only integration point required for encoded or file-backed clips.
pub trait FrameCodec<F> {
    /// Decodes exactly one frame from `clip`.
    ///
    /// # Errors
    ///
    /// Returns a typed codec error when the source cannot be opened or the
    /// requested frame cannot be decoded.
    fn read_frame(&mut self, clip: &EncodedClip, frame: FrameIndex) -> Result<F, CodecError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FixtureClip<F> {
    id: ClipId,
    frames: Vec<Option<F>>,
}

impl<F> FixtureClip<F> {
    /// Creates a complete in-memory fixture clip.
    ///
    /// # Errors
    ///
    /// Returns [`ClipError::Empty`] when `frames` is empty.
    pub fn new(id: ClipId, frames: Vec<F>) -> Result<Self, ClipError> {
        Self::from_slots(id, frames.into_iter().map(Some).collect())
    }

    /// Creates a fixture whose `None` slots deliberately model missing frames.
    ///
    /// # Errors
    ///
    /// Returns [`ClipError::Empty`] when `frames` is empty.
    pub fn from_slots(id: ClipId, frames: Vec<Option<F>>) -> Result<Self, ClipError> {
        if frames.is_empty() {
            return Err(ClipError::Empty);
        }
        Ok(Self { id, frames })
    }

    #[must_use]
    pub const fn id(&self) -> ClipId {
        self.id
    }

    #[must_use]
    pub fn frame_count(&self) -> u64 {
        u64::try_from(self.frames.len()).unwrap_or(u64::MAX)
    }

    pub(crate) fn frame(&self, index: FrameIndex) -> Option<&F> {
        usize::try_from(index.get())
            .ok()
            .and_then(|index| self.frames.get(index))
            .and_then(Option::as_ref)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedClip {
    id: ClipId,
    locator: String,
    frame_count: u64,
}

impl EncodedClip {
    /// Describes an encoded clip without opening or probing it.
    ///
    /// # Errors
    ///
    /// Returns [`ClipError::Empty`] when `frame_count` is zero.
    pub fn new(
        id: ClipId,
        locator: impl Into<String>,
        frame_count: u64,
    ) -> Result<Self, ClipError> {
        if frame_count == 0 {
            return Err(ClipError::Empty);
        }
        Ok(Self {
            id,
            locator: locator.into(),
            frame_count,
        })
    }

    #[must_use]
    pub const fn id(&self) -> ClipId {
        self.id
    }

    #[must_use]
    pub fn locator(&self) -> &str {
        &self.locator
    }

    #[must_use]
    pub const fn frame_count(&self) -> u64 {
        self.frame_count
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaybackClip<F> {
    Fixture(FixtureClip<F>),
    Encoded(EncodedClip),
}

impl<F> PlaybackClip<F> {
    #[must_use]
    pub const fn id(&self) -> ClipId {
        match self {
            Self::Fixture(clip) => clip.id(),
            Self::Encoded(clip) => clip.id(),
        }
    }

    #[must_use]
    pub fn frame_count(&self) -> u64 {
        match self {
            Self::Fixture(clip) => clip.frame_count(),
            Self::Encoded(clip) => clip.frame_count(),
        }
    }
}

impl<F> From<FixtureClip<F>> for PlaybackClip<F> {
    fn from(value: FixtureClip<F>) -> Self {
        Self::Fixture(value)
    }
}

impl<F> From<EncodedClip> for PlaybackClip<F> {
    fn from(value: EncodedClip) -> Self {
        Self::Encoded(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LibraryError {
    DuplicateClip(ClipId),
    MissingClip(ClipId),
}

impl fmt::Display for LibraryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateClip(id) => write!(formatter, "clip {id} is already registered"),
            Self::MissingClip(id) => write!(formatter, "clip {id} is not registered"),
        }
    }
}

impl std::error::Error for LibraryError {}

#[derive(Clone, Debug)]
pub struct ClipLibrary<F> {
    clips: BTreeMap<ClipId, PlaybackClip<F>>,
}

impl<F> Default for ClipLibrary<F> {
    fn default() -> Self {
        Self {
            clips: BTreeMap::new(),
        }
    }
}

impl<F> ClipLibrary<F> {
    #[must_use]
    pub fn len(&self) -> usize {
        self.clips.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.clips.is_empty()
    }

    /// Registers a clip by its stable ID.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::DuplicateClip`] if the ID is already present.
    pub fn insert(&mut self, clip: PlaybackClip<F>) -> Result<(), LibraryError> {
        let id = clip.id();
        if self.clips.contains_key(&id) {
            return Err(LibraryError::DuplicateClip(id));
        }
        self.clips.insert(id, clip);
        Ok(())
    }

    #[must_use]
    pub fn get(&self, id: ClipId) -> Option<&PlaybackClip<F>> {
        self.clips.get(&id)
    }
}
