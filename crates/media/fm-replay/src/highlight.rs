use std::{collections::BTreeMap, path::PathBuf};

use fm_audio::Gain;

use crate::{
    CameraId, EventId, ExportJobId, HighlightId, MusicTrackId, PlaybackRate, ReplayError,
    TimelineRange,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Transition {
    Cut,
    Dissolve { duration_nanos: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HighlightItem {
    pub id: HighlightId,
    pub event_id: EventId,
    pub timeline: TimelineRange,
    pub angle: CameraId,
    pub speed: PlaybackRate,
    pub transition: Transition,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MusicBed {
    pub id: MusicTrackId,
    pub locator: String,
    pub gain: Gain,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HighlightTimeline {
    items: Vec<HighlightItem>,
    music: Option<MusicBed>,
}

impl HighlightTimeline {
    /// Appends one event reference without copying media.
    ///
    /// # Errors
    ///
    /// Highlight IDs must be unique.
    pub fn push(&mut self, item: HighlightItem) -> Result<(), ReplayError> {
        if self.items.iter().any(|existing| existing.id == item.id) {
            return Err(ReplayError::DuplicateHighlight(item.id));
        }
        self.items.push(item);
        Ok(())
    }

    pub fn set_music(&mut self, music: Option<MusicBed>) {
        self.music = music;
    }

    #[must_use]
    pub fn items(&self) -> &[HighlightItem] {
        &self.items
    }

    #[must_use]
    pub const fn music(&self) -> Option<&MusicBed> {
        self.music.as_ref()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CaptureState {
    #[default]
    Idle,
    Recording,
    LowSpaceStopped,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExportJobState {
    Queued,
    Running { completed_items: usize },
    Completed { bytes: u64 },
    Failed { reason: String },
    Cancelled,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExportJob {
    pub id: ExportJobId,
    pub destination: PathBuf,
    pub state: ExportJobState,
    pub timeline_snapshot: HighlightTimeline,
    pub submitted_while_recording: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ReplaySession {
    capture_state: CaptureState,
    exports: BTreeMap<ExportJobId, ExportJob>,
}

impl ReplaySession {
    #[must_use]
    pub const fn capture_state(&self) -> CaptureState {
        self.capture_state
    }

    pub const fn set_capture_state(&mut self, state: CaptureState) {
        self.capture_state = state;
    }

    /// Snapshots a highlight edit for export. Capture state is not modified.
    ///
    /// # Errors
    ///
    /// Export IDs must be unique.
    pub fn submit_export(
        &mut self,
        id: ExportJobId,
        timeline: &HighlightTimeline,
        destination: impl Into<PathBuf>,
    ) -> Result<(), ReplayError> {
        if self.exports.contains_key(&id) {
            return Err(ReplayError::DuplicateExport(id));
        }
        self.exports.insert(
            id,
            ExportJob {
                id,
                destination: destination.into(),
                state: ExportJobState::Queued,
                timeline_snapshot: timeline.clone(),
                submitted_while_recording: self.capture_state == CaptureState::Recording,
            },
        );
        Ok(())
    }

    #[must_use]
    pub fn export(&self, id: ExportJobId) -> Option<&ExportJob> {
        self.exports.get(&id)
    }

    /// Advances a job through a monotonic state machine without affecting capture.
    ///
    /// # Errors
    ///
    /// Terminal jobs cannot transition and progress cannot move backwards.
    pub fn update_export(
        &mut self,
        id: ExportJobId,
        state: ExportJobState,
    ) -> Result<(), ReplayError> {
        let job = self
            .exports
            .get_mut(&id)
            .ok_or(ReplayError::UnknownExport(id))?;
        let valid = match (&job.state, &state) {
            (
                ExportJobState::Queued,
                ExportJobState::Running { .. }
                | ExportJobState::Failed { .. }
                | ExportJobState::Cancelled,
            )
            | (
                ExportJobState::Running { .. },
                ExportJobState::Completed { .. }
                | ExportJobState::Failed { .. }
                | ExportJobState::Cancelled,
            ) => true,
            (
                ExportJobState::Running {
                    completed_items: previous,
                },
                ExportJobState::Running {
                    completed_items: next,
                },
            ) => next >= previous,
            _ => false,
        };
        if !valid {
            return Err(ReplayError::ExportStateTransition);
        }
        job.state = state;
        Ok(())
    }
}
