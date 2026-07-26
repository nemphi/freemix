//! Deterministic, frame-exact playback and playlist transport.
//!
//! This crate intentionally does not decode media. [`FixtureClip`] provides an
//! in-memory reference source, while [`FrameCodec`] is the boundary for a real
//! encoded-media adapter.

mod clip;
mod playlist;
mod transport;
mod types;

pub use clip::{
    ClipError, ClipLibrary, CodecError, EncodedClip, FixtureClip, FrameCodec, LibraryError,
    PlaybackClip,
};
pub use playlist::{
    CancelGoOutcome, EndAction, GoError, GoScheduleOutcome, GoStatus, PlayerError, Playlist,
    PlaylistEntry, PlaylistError, PlaylistPlayer, ProgrammedGo,
};
pub use transport::{
    EndBehavior, MarkError, Marks, PlaybackError, PlaybackFrame, Transport, TransportState,
};
pub use types::{
    ClipId, FrameIndex, GoId, PlaylistEntryId, ScheduleCoordinate, Speed, SpeedDirection,
};

#[cfg(test)]
mod tests;
