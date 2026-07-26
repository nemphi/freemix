use core::fmt;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use fm_frame::{NormalizedDuration, NormalizedTimestamp, VideoDimensions};

use crate::RenditionId;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum HlsPlaylistType {
    Live,
    Event,
    VideoOnDemand,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HlsSegmentMetadata {
    rendition: RenditionId,
    media_sequence: u64,
    discontinuity_sequence: u64,
    start: NormalizedTimestamp,
    duration: NormalizedDuration,
    byte_length: u64,
    independent: bool,
    uri: String,
}

impl HlsSegmentMetadata {
    /// Creates metadata for one completed media segment.
    ///
    /// # Errors
    ///
    /// Rejects empty/control-containing URIs and zero byte lengths.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rendition: RenditionId,
        media_sequence: u64,
        discontinuity_sequence: u64,
        start: NormalizedTimestamp,
        duration: NormalizedDuration,
        byte_length: u64,
        independent: bool,
        uri: impl Into<String>,
    ) -> Result<Self, HlsError> {
        let uri = uri.into();
        if uri.is_empty() || uri.len() > 2_048 || uri.chars().any(char::is_control) {
            return Err(HlsError::InvalidUri);
        }
        if byte_length == 0 {
            return Err(HlsError::EmptySegment);
        }
        Ok(Self {
            rendition,
            media_sequence,
            discontinuity_sequence,
            start,
            duration,
            byte_length,
            independent,
            uri,
        })
    }

    #[must_use]
    pub const fn rendition(&self) -> RenditionId {
        self.rendition
    }

    #[must_use]
    pub const fn media_sequence(&self) -> u64 {
        self.media_sequence
    }

    #[must_use]
    pub const fn discontinuity_sequence(&self) -> u64 {
        self.discontinuity_sequence
    }

    #[must_use]
    pub const fn start(&self) -> NormalizedTimestamp {
        self.start
    }

    #[must_use]
    pub const fn duration(&self) -> NormalizedDuration {
        self.duration
    }

    #[must_use]
    pub const fn byte_length(&self) -> u64 {
        self.byte_length
    }

    #[must_use]
    pub const fn is_independent(&self) -> bool {
        self.independent
    }

    #[must_use]
    pub fn uri(&self) -> &str {
        &self.uri
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HlsPlaylistMetadata {
    version: u8,
    rendition: RenditionId,
    target_duration: NormalizedDuration,
    media_sequence: u64,
    discontinuity_sequence: u64,
    playlist_type: HlsPlaylistType,
    end_list: bool,
    segments: Vec<HlsSegmentMetadata>,
}

impl HlsPlaylistMetadata {
    #[must_use]
    pub const fn version(&self) -> u8 {
        self.version
    }

    #[must_use]
    pub const fn rendition(&self) -> RenditionId {
        self.rendition
    }

    #[must_use]
    pub const fn target_duration(&self) -> NormalizedDuration {
        self.target_duration
    }

    #[must_use]
    pub const fn media_sequence(&self) -> u64 {
        self.media_sequence
    }

    #[must_use]
    pub const fn discontinuity_sequence(&self) -> u64 {
        self.discontinuity_sequence
    }

    #[must_use]
    pub const fn playlist_type(&self) -> HlsPlaylistType {
        self.playlist_type
    }

    #[must_use]
    pub const fn is_end_list(&self) -> bool {
        self.end_list
    }

    #[must_use]
    pub fn segments(&self) -> &[HlsSegmentMetadata] {
        &self.segments
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HlsPlaylist {
    version: u8,
    rendition: RenditionId,
    target_duration: NormalizedDuration,
    initial_media_sequence: u64,
    maximum_segments: usize,
    playlist_type: HlsPlaylistType,
    end_list: bool,
    segments: VecDeque<HlsSegmentMetadata>,
}

impl HlsPlaylist {
    /// Creates a bounded media playlist metadata accumulator.
    ///
    /// # Errors
    ///
    /// Rejects version zero or a zero segment window.
    pub fn new(
        version: u8,
        rendition: RenditionId,
        target_duration: NormalizedDuration,
        initial_media_sequence: u64,
        maximum_segments: usize,
        playlist_type: HlsPlaylistType,
    ) -> Result<Self, HlsError> {
        if version == 0 {
            return Err(HlsError::InvalidVersion);
        }
        if maximum_segments == 0 {
            return Err(HlsError::InvalidWindow);
        }
        Ok(Self {
            version,
            rendition,
            target_duration,
            initial_media_sequence,
            maximum_segments,
            playlist_type,
            end_list: false,
            segments: VecDeque::with_capacity(maximum_segments),
        })
    }

    /// Appends one contiguous segment and rolls the bounded live window.
    ///
    /// # Errors
    ///
    /// Rejects wrong renditions, gaps/regressions, invalid discontinuities,
    /// overlapping timestamps, oversized durations, or writes after end-list.
    pub fn append(&mut self, segment: HlsSegmentMetadata) -> Result<(), HlsError> {
        if self.end_list {
            return Err(HlsError::PlaylistEnded);
        }
        if segment.rendition != self.rendition {
            return Err(HlsError::WrongRendition);
        }
        if segment.duration.as_nanos() > self.target_duration.as_nanos() {
            return Err(HlsError::DurationExceedsTarget);
        }
        let expected_sequence = self
            .segments
            .back()
            .map_or(self.initial_media_sequence, |last| {
                last.media_sequence.saturating_add(1)
            });
        if segment.media_sequence != expected_sequence {
            return Err(HlsError::MediaSequence);
        }
        if let Some(previous) = self.segments.back() {
            if segment.discontinuity_sequence < previous.discontinuity_sequence
                || segment.discontinuity_sequence
                    > previous.discontinuity_sequence.saturating_add(1)
            {
                return Err(HlsError::DiscontinuitySequence);
            }
            let previous_end = previous
                .start
                .as_nanos()
                .saturating_add(i64::try_from(previous.duration.as_nanos()).unwrap_or(i64::MAX));
            if segment.start.as_nanos() < previous_end {
                return Err(HlsError::TimestampOverlap);
            }
        }
        if self.segments.len() == self.maximum_segments {
            self.segments.pop_front();
        }
        self.segments.push_back(segment);
        Ok(())
    }

    pub const fn finish(&mut self) {
        self.end_list = true;
    }

    #[must_use]
    pub fn metadata(&self) -> HlsPlaylistMetadata {
        let media_sequence = self.segments.front().map_or(
            self.initial_media_sequence,
            HlsSegmentMetadata::media_sequence,
        );
        let discontinuity_sequence = self
            .segments
            .front()
            .map_or(0, HlsSegmentMetadata::discontinuity_sequence);
        HlsPlaylistMetadata {
            version: self.version,
            rendition: self.rendition,
            target_duration: self.target_duration,
            media_sequence,
            discontinuity_sequence,
            playlist_type: self.playlist_type,
            end_list: self.end_list,
            segments: self.segments.iter().cloned().collect(),
        }
    }

    fn segment(&self, sequence: u64) -> Option<&HlsSegmentMetadata> {
        self.segments
            .iter()
            .find(|segment| segment.media_sequence == sequence)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HlsVariantMetadata {
    rendition: RenditionId,
    name: String,
    bandwidth_bps: u64,
    dimensions: VideoDimensions,
    codecs: String,
    playlist_uri: String,
}

impl HlsVariantMetadata {
    /// Creates one master-playlist variant descriptor.
    ///
    /// # Errors
    ///
    /// Rejects zero bandwidth or empty/control-containing text fields.
    pub fn new(
        rendition: RenditionId,
        name: impl Into<String>,
        bandwidth_bps: u64,
        dimensions: VideoDimensions,
        codecs: impl Into<String>,
        playlist_uri: impl Into<String>,
    ) -> Result<Self, HlsError> {
        let name = name.into();
        let codecs = codecs.into();
        let playlist_uri = playlist_uri.into();
        if bandwidth_bps == 0 {
            return Err(HlsError::ZeroBandwidth);
        }
        if [name.as_str(), codecs.as_str(), playlist_uri.as_str()]
            .into_iter()
            .any(|value| {
                value.is_empty() || value.len() > 2_048 || value.chars().any(char::is_control)
            })
        {
            return Err(HlsError::InvalidUri);
        }
        Ok(Self {
            rendition,
            name,
            bandwidth_bps,
            dimensions,
            codecs,
            playlist_uri,
        })
    }

    #[must_use]
    pub const fn rendition(&self) -> RenditionId {
        self.rendition
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn bandwidth_bps(&self) -> u64 {
        self.bandwidth_bps
    }

    #[must_use]
    pub const fn dimensions(&self) -> VideoDimensions {
        self.dimensions
    }

    #[must_use]
    pub fn codecs(&self) -> &str {
        &self.codecs
    }

    #[must_use]
    pub fn playlist_uri(&self) -> &str {
        &self.playlist_uri
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HlsAbrCoordinator {
    variants: Vec<HlsVariantMetadata>,
    playlists: BTreeMap<RenditionId, HlsPlaylist>,
}

impl HlsAbrCoordinator {
    /// Creates aligned, bounded playlists for a validated set of ABR variants.
    ///
    /// # Errors
    ///
    /// Requires at least two variants with unique rendition IDs and names.
    pub fn new(
        variants: Vec<HlsVariantMetadata>,
        version: u8,
        target_duration: NormalizedDuration,
        initial_media_sequence: u64,
        maximum_segments: usize,
        playlist_type: HlsPlaylistType,
    ) -> Result<Self, HlsError> {
        if variants.len() < 2 {
            return Err(HlsError::InsufficientVariants);
        }
        let mut renditions = BTreeSet::new();
        let mut names = BTreeSet::new();
        let mut playlists = BTreeMap::new();
        for variant in &variants {
            if !renditions.insert(variant.rendition) || !names.insert(variant.name.clone()) {
                return Err(HlsError::DuplicateVariant);
            }
            playlists.insert(
                variant.rendition,
                HlsPlaylist::new(
                    version,
                    variant.rendition,
                    target_duration,
                    initial_media_sequence,
                    maximum_segments,
                    playlist_type,
                )?,
            );
        }
        Ok(Self {
            variants,
            playlists,
        })
    }

    /// Appends a segment after checking boundary alignment with every available
    /// segment of the same media sequence in the other variants.
    ///
    /// # Errors
    ///
    /// Returns a playlist sequencing error or [`HlsError::AbrSequenceMismatch`].
    pub fn append(&mut self, segment: HlsSegmentMetadata) -> Result<(), HlsError> {
        if !segment.independent {
            return Err(HlsError::AbrSegmentNotIndependent);
        }
        let rendition = segment.rendition;
        if !self.playlists.contains_key(&rendition) {
            return Err(HlsError::WrongRendition);
        }
        for (other_rendition, playlist) in &self.playlists {
            if *other_rendition == rendition {
                continue;
            }
            if let Some(other) = playlist.segment(segment.media_sequence)
                && (other.start != segment.start
                    || other.duration != segment.duration
                    || other.discontinuity_sequence != segment.discontinuity_sequence
                    || !other.independent)
            {
                return Err(HlsError::AbrSequenceMismatch);
            }
        }
        let Some(playlist) = self.playlists.get_mut(&rendition) else {
            return Err(HlsError::WrongRendition);
        };
        playlist.append(segment)
    }

    #[must_use]
    pub fn variants(&self) -> &[HlsVariantMetadata] {
        &self.variants
    }

    #[must_use]
    pub fn playlist(&self, rendition: RenditionId) -> Option<HlsPlaylistMetadata> {
        self.playlists.get(&rendition).map(HlsPlaylist::metadata)
    }

    pub fn finish(&mut self) {
        for playlist in self.playlists.values_mut() {
            playlist.finish();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HlsError {
    InvalidUri,
    EmptySegment,
    InvalidVersion,
    InvalidWindow,
    PlaylistEnded,
    WrongRendition,
    DurationExceedsTarget,
    MediaSequence,
    DiscontinuitySequence,
    TimestampOverlap,
    ZeroBandwidth,
    InsufficientVariants,
    DuplicateVariant,
    AbrSegmentNotIndependent,
    AbrSequenceMismatch,
}

impl fmt::Display for HlsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidUri => "HLS URI or descriptor text is invalid",
            Self::EmptySegment => "HLS segment byte length must be nonzero",
            Self::InvalidVersion => "HLS playlist version must be nonzero",
            Self::InvalidWindow => "HLS playlist window must be nonzero",
            Self::PlaylistEnded => "HLS playlist is already ended",
            Self::WrongRendition => "HLS segment belongs to another rendition",
            Self::DurationExceedsTarget => "HLS segment exceeds target duration",
            Self::MediaSequence => "HLS media sequence is not contiguous",
            Self::DiscontinuitySequence => "HLS discontinuity sequence is invalid",
            Self::TimestampOverlap => "HLS segment timestamps overlap",
            Self::ZeroBandwidth => "HLS variant bandwidth must be nonzero",
            Self::InsufficientVariants => "HLS ABR output requires at least two variants",
            Self::DuplicateVariant => "HLS ABR variant identifiers and names must be unique",
            Self::AbrSegmentNotIndependent => "HLS ABR segments must begin independently",
            Self::AbrSequenceMismatch => "HLS ABR segment boundaries are not aligned",
        })
    }
}

impl std::error::Error for HlsError {}
