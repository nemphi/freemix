use std::collections::{BTreeMap, BTreeSet};

use fm_frame::{ClockDomainId, NormalizedTimestamp, SequenceNumber};
use fm_playback::FrameIndex;
use fm_record::RecorderId;
use fm_types::ChannelLayout;

use crate::{CameraId, ReplayError, StorageRootId};

pub const MAX_REPLAY_SOURCES: usize = 8;
pub const MAX_AUDIO_CHANNELS: usize = 4;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CameraSource {
    pub id: CameraId,
    pub name: String,
    pub storage_root_id: StorageRootId,
    pub recorder_id: RecorderId,
    pub clock_domain: ClockDomainId,
    pub audio_layout: ChannelLayout,
}

#[derive(Clone, Debug, Default)]
pub struct SourceCatalog {
    sources: BTreeMap<CameraId, CameraSource>,
}

impl SourceCatalog {
    /// Validates a replay source set. Empty sets are allowed while configuring.
    ///
    /// # Errors
    ///
    /// More than eight cameras, duplicate IDs, empty names, or audio layouts
    /// wider than four channels are rejected.
    pub fn new(sources: Vec<CameraSource>) -> Result<Self, ReplayError> {
        if sources.len() > MAX_REPLAY_SOURCES {
            return Err(ReplayError::TooManySources(sources.len()));
        }
        let mut catalog = Self::default();
        for source in sources {
            if source.name.trim().is_empty() {
                return Err(ReplayError::EmptyName);
            }
            let channels = source.audio_layout.channels().len();
            if channels > MAX_AUDIO_CHANNELS {
                return Err(ReplayError::TooManyAudioChannels(channels));
            }
            if catalog.sources.insert(source.id, source.clone()).is_some() {
                return Err(ReplayError::DuplicateCamera(source.id));
            }
        }
        Ok(catalog)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.sources.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    #[must_use]
    pub fn get(&self, id: CameraId) -> Option<&CameraSource> {
        self.sources.get(&id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &CameraSource> {
        self.sources.values()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceObservation {
    pub synchronized_at: NormalizedTimestamp,
    pub source_sequence: SequenceNumber,
    pub source_frame: FrameIndex,
    pub discontinuity_epoch: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineDiscontinuity {
    pub camera_id: CameraId,
    pub synchronized_at: NormalizedTimestamp,
    pub epoch: u64,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuadCell {
    pub camera_id: CameraId,
    pub observation: SourceObservation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuadViewDescription {
    pub synchronized_at: NormalizedTimestamp,
    pub cells: [QuadCell; 4],
    pub skew_nanos: u64,
    pub tolerance_nanos: u64,
    pub in_sync: bool,
}

#[derive(Clone, Debug)]
pub struct SourceTimeline {
    observations: BTreeMap<CameraId, Vec<SourceObservation>>,
    discontinuities: Vec<TimelineDiscontinuity>,
    epochs: BTreeMap<CameraId, u64>,
}

impl SourceTimeline {
    #[must_use]
    pub fn new(catalog: &SourceCatalog) -> Self {
        Self {
            observations: catalog
                .iter()
                .map(|source| (source.id, Vec::new()))
                .collect(),
            discontinuities: Vec::new(),
            epochs: catalog.iter().map(|source| (source.id, 0)).collect(),
        }
    }

    /// Appends one frame mapping to the shared synchronized timeline.
    ///
    /// # Errors
    ///
    /// Unknown cameras, non-increasing time, and non-increasing source sequence
    /// within a discontinuity epoch are rejected.
    pub fn observe(
        &mut self,
        camera_id: CameraId,
        synchronized_at: NormalizedTimestamp,
        source_sequence: SequenceNumber,
        source_frame: FrameIndex,
    ) -> Result<SourceObservation, ReplayError> {
        let observations = self
            .observations
            .get_mut(&camera_id)
            .ok_or(ReplayError::UnknownCamera(camera_id))?;
        let epoch = self.epochs[&camera_id];
        if let Some(previous) = observations.last() {
            if synchronized_at <= previous.synchronized_at {
                return Err(ReplayError::TimelineRegression(camera_id));
            }
            if previous.discontinuity_epoch == epoch
                && source_sequence.get() <= previous.source_sequence.get()
            {
                return Err(ReplayError::SequenceRegression(camera_id));
            }
        }
        let observation = SourceObservation {
            synchronized_at,
            source_sequence,
            source_frame,
            discontinuity_epoch: epoch,
        };
        observations.push(observation);
        Ok(observation)
    }

    /// Starts a new source epoch, allowing source sequence/frame counters to reset.
    ///
    /// # Errors
    ///
    /// The camera must exist, the reason must be non-empty, and the marker may
    /// not precede its latest observation.
    pub fn record_discontinuity(
        &mut self,
        camera_id: CameraId,
        synchronized_at: NormalizedTimestamp,
        reason: impl Into<String>,
    ) -> Result<&TimelineDiscontinuity, ReplayError> {
        let reason = reason.into();
        if reason.trim().is_empty() {
            return Err(ReplayError::EmptyName);
        }
        let observations = self
            .observations
            .get(&camera_id)
            .ok_or(ReplayError::UnknownCamera(camera_id))?;
        if observations
            .last()
            .is_some_and(|last| synchronized_at < last.synchronized_at)
        {
            return Err(ReplayError::TimelineRegression(camera_id));
        }
        let epoch = self
            .epochs
            .get_mut(&camera_id)
            .ok_or(ReplayError::UnknownCamera(camera_id))?;
        *epoch = epoch.saturating_add(1);
        self.discontinuities.push(TimelineDiscontinuity {
            camera_id,
            synchronized_at,
            epoch: *epoch,
            reason,
        });
        self.discontinuities
            .last()
            .ok_or(ReplayError::UnknownCamera(camera_id))
    }

    #[must_use]
    pub fn discontinuities(&self) -> &[TimelineDiscontinuity] {
        &self.discontinuities
    }

    /// Resolves four distinct cameras at or immediately before one target time.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate cameras or a camera without an observation
    /// at/before the requested target.
    pub fn quad_view(
        &self,
        cameras: [CameraId; 4],
        synchronized_at: NormalizedTimestamp,
        tolerance_nanos: u64,
    ) -> Result<QuadViewDescription, ReplayError> {
        let mut unique = BTreeSet::new();
        let mut cells = Vec::with_capacity(4);
        for camera_id in cameras {
            if !unique.insert(camera_id) {
                return Err(ReplayError::DuplicateQuadCamera(camera_id));
            }
            let observation = self
                .observations
                .get(&camera_id)
                .ok_or(ReplayError::UnknownCamera(camera_id))?
                .iter()
                .rev()
                .find(|item| item.synchronized_at <= synchronized_at)
                .copied()
                .ok_or(ReplayError::ObservationUnavailable(camera_id))?;
            cells.push(QuadCell {
                camera_id,
                observation,
            });
        }
        let minimum = cells
            .iter()
            .map(|cell| cell.observation.synchronized_at.as_nanos())
            .min()
            .unwrap_or(synchronized_at.as_nanos());
        let maximum = cells
            .iter()
            .map(|cell| cell.observation.synchronized_at.as_nanos())
            .max()
            .unwrap_or(synchronized_at.as_nanos());
        let skew_nanos = u64::try_from(maximum - minimum).unwrap_or(u64::MAX);
        let cells: [QuadCell; 4] = cells
            .try_into()
            .map_err(|_| ReplayError::ObservationUnavailable(cameras[0]))?;
        Ok(QuadViewDescription {
            synchronized_at,
            cells,
            skew_nanos,
            tolerance_nanos,
            in_sync: skew_nanos <= tolerance_nanos,
        })
    }
}
