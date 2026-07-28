use fm_protocol::{EngineIdentity, EventCursor, EventMessage, SnapshotMessage};

/// The small control-plane surface needed to establish a server session.
pub trait ControlPlane {
    type Error;

    /// Selects a fresh snapshot or resumable events for the supplied cursor.
    ///
    /// # Errors
    ///
    /// Returns the implementation's error when authoritative state cannot be
    /// read.
    fn initial_sync(&self, cursor: Option<&EventCursor>) -> Result<InitialSync, Self::Error>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitialSync {
    pub engine: EngineIdentity,
    pub current_revision: u64,
    pub payload: SyncPayload,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncPayload {
    Snapshot(Box<SnapshotMessage>),
    Resume(Vec<EventMessage>),
}

impl SyncPayload {
    #[must_use]
    pub const fn is_resume(&self) -> bool {
        matches!(self, Self::Resume(_))
    }
}
