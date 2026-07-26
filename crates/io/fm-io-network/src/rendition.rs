use core::fmt;
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;

use fm_frame::{CodecId, TimeBase, VideoDimensions};

use crate::{DestinationId, MAX_DESTINATIONS};

const MAX_ABR_VARIANTS: usize = 8;
const MAX_SETTING_LENGTH: usize = 128;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FrameRate {
    numerator: u32,
    denominator: u32,
}

impl FrameRate {
    /// Creates a normalized positive rational frame rate.
    ///
    /// # Errors
    ///
    /// Returns [`RenditionError::InvalidFrameRate`] when either part is zero.
    pub fn new(numerator: u32, denominator: u32) -> Result<Self, RenditionError> {
        if numerator == 0 || denominator == 0 {
            return Err(RenditionError::InvalidFrameRate);
        }
        let divisor = gcd(numerator, denominator);
        Ok(Self {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        })
    }

    #[must_use]
    pub const fn numerator(self) -> u32 {
        self.numerator
    }

    #[must_use]
    pub const fn denominator(self) -> u32 {
        self.denominator
    }
}

const fn gcd(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ColorDescription {
    Rec601Limited,
    Rec709Limited,
    Rec709Full,
    Rec2020Sdr,
    Rec2020Pq,
    Rec2020Hlg,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct VideoRendition {
    codec: CodecId,
    codec_profile: String,
    codec_settings: BTreeMap<String, String>,
    dimensions: VideoDimensions,
    frame_rate: FrameRate,
    color: ColorDescription,
    bitrate_bps: u64,
    gop_frames: u32,
}

impl VideoRendition {
    /// Creates the video portion of an exact rendition identity.
    ///
    /// # Errors
    ///
    /// Rejects empty profiles, zero bitrate, or zero GOP length.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        codec: CodecId,
        codec_profile: impl Into<String>,
        dimensions: VideoDimensions,
        frame_rate: FrameRate,
        color: ColorDescription,
        bitrate_bps: u64,
        gop_frames: u32,
    ) -> Result<Self, RenditionError> {
        let codec_profile = codec_profile.into();
        validate_setting_value(&codec_profile)?;
        if bitrate_bps == 0 {
            return Err(RenditionError::ZeroBitrate);
        }
        if gop_frames == 0 {
            return Err(RenditionError::ZeroGop);
        }
        Ok(Self {
            codec,
            codec_profile,
            codec_settings: BTreeMap::new(),
            dimensions,
            frame_rate,
            color,
            bitrate_bps,
            gop_frames,
        })
    }

    /// Adds a codec-specific option that participates in exact sharing.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, or control-containing keys and values.
    pub fn with_codec_setting(
        mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<Self, RenditionError> {
        let key = key.into();
        let value = value.into();
        validate_setting_value(&key)?;
        validate_setting_value(&value)?;
        self.codec_settings.insert(key, value);
        Ok(self)
    }

    #[must_use]
    pub const fn codec(&self) -> &CodecId {
        &self.codec
    }

    #[must_use]
    pub fn codec_profile(&self) -> &str {
        &self.codec_profile
    }

    #[must_use]
    pub const fn codec_settings(&self) -> &BTreeMap<String, String> {
        &self.codec_settings
    }

    #[must_use]
    pub const fn dimensions(&self) -> VideoDimensions {
        self.dimensions
    }

    #[must_use]
    pub const fn frame_rate(&self) -> FrameRate {
        self.frame_rate
    }

    #[must_use]
    pub const fn color(&self) -> ColorDescription {
        self.color
    }

    #[must_use]
    pub const fn bitrate_bps(&self) -> u64 {
        self.bitrate_bps
    }

    #[must_use]
    pub const fn gop_frames(&self) -> u32 {
        self.gop_frames
    }

    fn abr_compatible_with(&self, other: &Self) -> bool {
        self.codec == other.codec
            && self.codec_profile == other.codec_profile
            && self.codec_settings == other.codec_settings
            && self.frame_rate == other.frame_rate
            && self.color == other.color
            && self.gop_frames == other.gop_frames
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AudioRendition {
    codec: CodecId,
    codec_profile: String,
    codec_settings: BTreeMap<String, String>,
    bitrate_bps: u64,
    sample_rate_hz: u32,
    channels: u16,
    audio_map: Vec<u16>,
}

impl AudioRendition {
    /// Creates the audio portion of an exact rendition identity.
    ///
    /// # Errors
    ///
    /// Rejects empty settings, zero rates/channels, or an invalid channel map.
    pub fn new(
        codec: CodecId,
        codec_profile: impl Into<String>,
        bitrate_bps: u64,
        sample_rate_hz: u32,
        channels: u16,
        audio_map: Vec<u16>,
    ) -> Result<Self, RenditionError> {
        let codec_profile = codec_profile.into();
        validate_setting_value(&codec_profile)?;
        if bitrate_bps == 0 {
            return Err(RenditionError::ZeroBitrate);
        }
        if sample_rate_hz == 0 || channels == 0 {
            return Err(RenditionError::InvalidAudioFormat);
        }
        if audio_map.len() != usize::from(channels)
            || audio_map.iter().any(|channel| *channel >= channels)
        {
            return Err(RenditionError::InvalidAudioMap);
        }
        Ok(Self {
            codec,
            codec_profile,
            codec_settings: BTreeMap::new(),
            bitrate_bps,
            sample_rate_hz,
            channels,
            audio_map,
        })
    }

    /// Adds a codec-specific option that participates in exact sharing.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, or control-containing keys and values.
    pub fn with_codec_setting(
        mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<Self, RenditionError> {
        let key = key.into();
        let value = value.into();
        validate_setting_value(&key)?;
        validate_setting_value(&value)?;
        self.codec_settings.insert(key, value);
        Ok(self)
    }

    #[must_use]
    pub const fn codec(&self) -> &CodecId {
        &self.codec
    }

    #[must_use]
    pub fn codec_profile(&self) -> &str {
        &self.codec_profile
    }

    #[must_use]
    pub const fn bitrate_bps(&self) -> u64 {
        self.bitrate_bps
    }

    #[must_use]
    pub const fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    #[must_use]
    pub const fn channels(&self) -> u16 {
        self.channels
    }

    #[must_use]
    pub fn audio_map(&self) -> &[u16] {
        &self.audio_map
    }

    fn abr_compatible_with(&self, other: &Self) -> bool {
        self.codec == other.codec
            && self.codec_profile == other.codec_profile
            && self.codec_settings == other.codec_settings
            && self.sample_rate_hz == other.sample_rate_hz
            && self.channels == other.channels
            && self.audio_map == other.audio_map
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TimingProfile {
    time_base: TimeBase,
    keyframe_alignment_origin: i64,
}

impl TimingProfile {
    #[must_use]
    pub const fn new(time_base: TimeBase, keyframe_alignment_origin: i64) -> Self {
        Self {
            time_base,
            keyframe_alignment_origin,
        }
    }

    #[must_use]
    pub const fn time_base(self) -> TimeBase {
        self.time_base
    }

    #[must_use]
    pub const fn keyframe_alignment_origin(self) -> i64 {
        self.keyframe_alignment_origin
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RenditionProfile {
    video: VideoRendition,
    audio: AudioRendition,
    timing: TimingProfile,
}

impl RenditionProfile {
    #[must_use]
    pub const fn new(video: VideoRendition, audio: AudioRendition, timing: TimingProfile) -> Self {
        Self {
            video,
            audio,
            timing,
        }
    }

    #[must_use]
    pub const fn video(&self) -> &VideoRendition {
        &self.video
    }

    #[must_use]
    pub const fn audio(&self) -> &AudioRendition {
        &self.audio
    }

    #[must_use]
    pub const fn timing(&self) -> TimingProfile {
        self.timing
    }

    fn abr_compatible_with(&self, other: &Self) -> bool {
        self.video.abr_compatible_with(&other.video)
            && self.audio.abr_compatible_with(&other.audio)
            && self.timing == other.timing
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AbrVariant {
    name: String,
    profile: RenditionProfile,
}

impl AbrVariant {
    /// Creates a named ABR variant.
    ///
    /// # Errors
    ///
    /// Rejects an empty, oversized, or control-containing name.
    pub fn new(name: impl Into<String>, profile: RenditionProfile) -> Result<Self, RenditionError> {
        let name = name.into();
        validate_setting_value(&name)?;
        Ok(Self { name, profile })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn profile(&self) -> &RenditionProfile {
        &self.profile
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AbrLadder {
    variants: Vec<AbrVariant>,
}

impl AbrLadder {
    /// Validates a low-to-high, independently encoded but switch-aligned ladder.
    ///
    /// # Errors
    ///
    /// Requires 2..=8 uniquely named variants, increasing video bitrate,
    /// nondecreasing dimensions, and identical switch-critical settings.
    pub fn new(variants: Vec<AbrVariant>) -> Result<Self, RenditionError> {
        if !(2..=MAX_ABR_VARIANTS).contains(&variants.len()) {
            return Err(RenditionError::InvalidAbrVariantCount);
        }
        let mut names = BTreeSet::new();
        for variant in &variants {
            if !names.insert(variant.name.clone()) {
                return Err(RenditionError::DuplicateAbrName);
            }
        }
        for pair in variants.windows(2) {
            let lower = &pair[0].profile;
            let upper = &pair[1].profile;
            if lower == upper {
                return Err(RenditionError::DuplicateAbrProfile);
            }
            if lower.video.bitrate_bps >= upper.video.bitrate_bps {
                return Err(RenditionError::AbrBitrateOrder);
            }
            let lower_dimensions = lower.video.dimensions;
            let upper_dimensions = upper.video.dimensions;
            if lower_dimensions.width() > upper_dimensions.width()
                || lower_dimensions.height() > upper_dimensions.height()
            {
                return Err(RenditionError::AbrDimensionOrder);
            }
            if !lower.abr_compatible_with(upper) {
                return Err(RenditionError::AbrIncompatibleProfiles);
            }
        }
        Ok(Self { variants })
    }

    #[must_use]
    pub fn variants(&self) -> &[AbrVariant] {
        &self.variants
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DestinationRenditions {
    destination: DestinationId,
    variants: Vec<AbrVariant>,
}

impl DestinationRenditions {
    #[must_use]
    pub fn single(destination: DestinationId, profile: RenditionProfile) -> Self {
        Self {
            destination,
            variants: vec![AbrVariant {
                name: "source".to_owned(),
                profile,
            }],
        }
    }

    #[must_use]
    pub fn ladder(destination: DestinationId, ladder: AbrLadder) -> Self {
        Self {
            destination,
            variants: ladder.variants,
        }
    }

    #[must_use]
    pub const fn destination(&self) -> DestinationId {
        self.destination
    }

    #[must_use]
    pub fn variants(&self) -> &[AbrVariant] {
        &self.variants
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RenditionId(NonZeroU32);

impl RenditionId {
    #[must_use]
    pub const fn new(value: NonZeroU32) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> NonZeroU32 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedRendition {
    id: RenditionId,
    profile: RenditionProfile,
    destinations: Vec<DestinationId>,
}

impl PlannedRendition {
    #[must_use]
    pub const fn id(&self) -> RenditionId {
        self.id
    }

    #[must_use]
    pub const fn profile(&self) -> &RenditionProfile {
        &self.profile
    }

    #[must_use]
    pub fn destinations(&self) -> &[DestinationId] {
        &self.destinations
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenditionPlan {
    renditions: Vec<PlannedRendition>,
    routes: BTreeMap<DestinationId, Vec<RenditionId>>,
}

impl RenditionPlan {
    #[must_use]
    pub fn renditions(&self) -> &[PlannedRendition] {
        &self.renditions
    }

    #[must_use]
    pub fn destination_renditions(&self, destination: DestinationId) -> Option<&[RenditionId]> {
        self.routes.get(&destination).map(Vec::as_slice)
    }

    #[must_use]
    pub fn destinations_for(&self, rendition: RenditionId) -> Option<&[DestinationId]> {
        self.renditions
            .iter()
            .find(|planned| planned.id == rendition)
            .map(|planned| planned.destinations.as_slice())
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RenditionPlanner;

impl RenditionPlanner {
    /// Deduplicates only profiles whose complete values are equal.
    ///
    /// # Errors
    ///
    /// Rejects duplicate destinations, an empty request set, or more than five destinations.
    pub fn plan(requests: &[DestinationRenditions]) -> Result<RenditionPlan, RenditionError> {
        if requests.is_empty() {
            return Err(RenditionError::EmptyPlan);
        }
        if requests.len() > MAX_DESTINATIONS {
            return Err(RenditionError::TooManyDestinations);
        }
        let mut destinations = BTreeSet::new();
        let mut renditions: Vec<PlannedRendition> = Vec::new();
        let mut routes = BTreeMap::new();

        for request in requests {
            if !destinations.insert(request.destination) {
                return Err(RenditionError::DuplicateDestination);
            }
            let mut destination_routes = Vec::with_capacity(request.variants.len());
            for variant in &request.variants {
                let index = renditions
                    .iter()
                    .position(|planned| planned.profile == variant.profile);
                let id = if let Some(index) = index {
                    let planned = &mut renditions[index];
                    planned.destinations.push(request.destination);
                    planned.id
                } else {
                    let value = u32::try_from(renditions.len() + 1)
                        .map_err(|_| RenditionError::TooManyRenditions)?;
                    let value = NonZeroU32::new(value).ok_or(RenditionError::TooManyRenditions)?;
                    let id = RenditionId::new(value);
                    renditions.push(PlannedRendition {
                        id,
                        profile: variant.profile.clone(),
                        destinations: vec![request.destination],
                    });
                    id
                };
                destination_routes.push(id);
            }
            routes.insert(request.destination, destination_routes);
        }

        Ok(RenditionPlan { renditions, routes })
    }
}

fn validate_setting_value(value: &str) -> Result<(), RenditionError> {
    if value.is_empty()
        || value.len() > MAX_SETTING_LENGTH
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        Err(RenditionError::InvalidSetting)
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenditionError {
    InvalidFrameRate,
    InvalidSetting,
    ZeroBitrate,
    ZeroGop,
    InvalidAudioFormat,
    InvalidAudioMap,
    InvalidAbrVariantCount,
    DuplicateAbrName,
    DuplicateAbrProfile,
    AbrBitrateOrder,
    AbrDimensionOrder,
    AbrIncompatibleProfiles,
    EmptyPlan,
    TooManyDestinations,
    TooManyRenditions,
    DuplicateDestination,
}

impl fmt::Display for RenditionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidFrameRate => "frame rate must be positive",
            Self::InvalidSetting => "rendition setting is invalid",
            Self::ZeroBitrate => "rendition bitrate must be nonzero",
            Self::ZeroGop => "GOP length must be nonzero",
            Self::InvalidAudioFormat => "audio rate and channel count must be nonzero",
            Self::InvalidAudioMap => "audio map must map every output channel",
            Self::InvalidAbrVariantCount => "ABR ladder must contain two through eight variants",
            Self::DuplicateAbrName => "ABR variant names must be unique",
            Self::DuplicateAbrProfile => "ABR variants must not be identical",
            Self::AbrBitrateOrder => "ABR video bitrates must increase",
            Self::AbrDimensionOrder => "ABR dimensions must not decrease",
            Self::AbrIncompatibleProfiles => "ABR switch-critical settings must match",
            Self::EmptyPlan => "rendition plan must contain a destination",
            Self::TooManyDestinations => "rendition plan exceeds five destinations",
            Self::TooManyRenditions => "rendition plan contains too many unique renditions",
            Self::DuplicateDestination => "rendition plan contains a duplicate destination",
        })
    }
}

impl std::error::Error for RenditionError {}
