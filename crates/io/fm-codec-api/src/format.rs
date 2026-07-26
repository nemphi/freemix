use std::fmt;

use fm_frame::{
    AudioBlock, ChannelLayout, CodecId, CpuVideoFrame, MediaTiming, PixelFormat, SampleRate,
    TimeBase, VideoDimensions,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MediaKind {
    Audio,
    Video,
}

/// Well-known portable codec identifiers.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum KnownCodec {
    H264,
    H265,
    Av1,
    Vp9,
    Aac,
    Opus,
    Pcm,
}

impl KnownCodec {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::H264 => "video/h264",
            Self::H265 => "video/h265",
            Self::Av1 => "video/av1",
            Self::Vp9 => "video/vp9",
            Self::Aac => "audio/aac",
            Self::Opus => "audio/opus",
            Self::Pcm => "audio/pcm",
        }
    }

    /// Returns the canonical extensible [`CodecId`] for this codec.
    ///
    /// # Panics
    ///
    /// Panics only if a crate-defined identifier violates `fm-frame`'s codec
    /// identifier contract.
    #[must_use]
    pub fn codec_id(self) -> CodecId {
        CodecId::new(self.as_str()).expect("known codec identifiers are valid")
    }
}

macro_rules! portable_name {
    ($name:ident, $empty:ident, $long:ident, $invalid:ident, $label:literal) => {
        #[derive(Clone, Debug, Eq, Hash, PartialEq)]
        pub struct $name(String);

        impl $name {
            pub const MAX_LENGTH: usize = 64;

            /// Creates a portable codec value.
            ///
            /// # Errors
            ///
            /// Rejects empty, oversized, or non-portable values.
            pub fn new(value: impl Into<String>) -> Result<Self, FormatError> {
                let value = value.into();
                if value.is_empty() {
                    return Err(FormatError::$empty);
                }
                if value.len() > Self::MAX_LENGTH {
                    return Err(FormatError::$long {
                        actual: value.len(),
                        maximum: Self::MAX_LENGTH,
                    });
                }
                if !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
                {
                    return Err(FormatError::$invalid);
                }
                Ok(Self(value))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

portable_name!(
    CodecProfile,
    EmptyProfile,
    ProfileTooLong,
    InvalidProfile,
    "profile"
);
portable_name!(CodecLevel, EmptyLevel, LevelTooLong, InvalidLevel, "level");

/// Complete compressed-stream configuration needed to initialize a codec.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedFormat {
    codec: CodecId,
    media_kind: MediaKind,
    profile: Option<CodecProfile>,
    level: Option<CodecLevel>,
    time_base: TimeBase,
    codec_config: Vec<u8>,
}

impl EncodedFormat {
    pub const MAX_CODEC_CONFIG_BYTES: usize = 1024 * 1024;

    /// Creates an encoded format descriptor with no codec initialization data.
    #[must_use]
    pub const fn new(codec: CodecId, media_kind: MediaKind, time_base: TimeBase) -> Self {
        Self {
            codec,
            media_kind,
            profile: None,
            level: None,
            time_base,
            codec_config: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_profile(mut self, profile: CodecProfile) -> Self {
        self.profile = Some(profile);
        self
    }

    #[must_use]
    pub fn with_level(mut self, level: CodecLevel) -> Self {
        self.level = Some(level);
        self
    }

    /// Attaches bounded codec initialization bytes such as an audio-specific
    /// config or video sequence header.
    ///
    /// # Errors
    ///
    /// Returns [`FormatError::CodecConfigTooLarge`] before retaining oversized
    /// data.
    pub fn with_codec_config(mut self, bytes: Vec<u8>) -> Result<Self, FormatError> {
        if bytes.len() > Self::MAX_CODEC_CONFIG_BYTES {
            return Err(FormatError::CodecConfigTooLarge {
                actual: bytes.len(),
                maximum: Self::MAX_CODEC_CONFIG_BYTES,
            });
        }
        self.codec_config = bytes;
        Ok(self)
    }

    #[must_use]
    pub const fn codec(&self) -> &CodecId {
        &self.codec
    }

    #[must_use]
    pub const fn media_kind(&self) -> MediaKind {
        self.media_kind
    }

    #[must_use]
    pub const fn profile(&self) -> Option<&CodecProfile> {
        self.profile.as_ref()
    }

    #[must_use]
    pub const fn level(&self) -> Option<&CodecLevel> {
        self.level.as_ref()
    }

    #[must_use]
    pub const fn time_base(&self) -> TimeBase {
        self.time_base
    }

    #[must_use]
    pub fn codec_config(&self) -> &[u8] {
        &self.codec_config
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedAudioFormat {
    sample_rate: SampleRate,
    channels: ChannelLayout,
}

impl DecodedAudioFormat {
    #[must_use]
    pub const fn new(sample_rate: SampleRate, channels: ChannelLayout) -> Self {
        Self {
            sample_rate,
            channels,
        }
    }

    #[must_use]
    pub const fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    #[must_use]
    pub const fn channels(&self) -> &ChannelLayout {
        &self.channels
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodedVideoFormat {
    pixel_format: PixelFormat,
    dimensions: VideoDimensions,
}

impl DecodedVideoFormat {
    #[must_use]
    pub const fn new(pixel_format: PixelFormat, dimensions: VideoDimensions) -> Self {
        Self {
            pixel_format,
            dimensions,
        }
    }

    #[must_use]
    pub const fn pixel_format(self) -> PixelFormat {
        self.pixel_format
    }

    #[must_use]
    pub const fn dimensions(self) -> VideoDimensions {
        self.dimensions
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodedFormat {
    Audio(DecodedAudioFormat),
    Video(DecodedVideoFormat),
}

impl DecodedFormat {
    #[must_use]
    pub const fn media_kind(&self) -> MediaKind {
        match self {
            Self::Audio(_) => MediaKind::Audio,
            Self::Video(_) => MediaKind::Video,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum DecodedFrame {
    Audio(AudioBlock),
    Video(CpuVideoFrame),
}

impl DecodedFrame {
    #[must_use]
    pub const fn media_kind(&self) -> MediaKind {
        match self {
            Self::Audio(_) => MediaKind::Audio,
            Self::Video(_) => MediaKind::Video,
        }
    }

    #[must_use]
    pub const fn timing(&self) -> MediaTiming {
        match self {
            Self::Audio(block) => block.timing(),
            Self::Video(frame) => frame.timing(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatError {
    EmptyProfile,
    ProfileTooLong { actual: usize, maximum: usize },
    InvalidProfile,
    EmptyLevel,
    LevelTooLong { actual: usize, maximum: usize },
    InvalidLevel,
    CodecConfigTooLarge { actual: usize, maximum: usize },
}

impl fmt::Display for FormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyProfile => formatter.write_str("codec profile must not be empty"),
            Self::ProfileTooLong { actual, maximum } => {
                write!(formatter, "codec profile length {actual} exceeds {maximum}")
            }
            Self::InvalidProfile => formatter.write_str("codec profile contains invalid bytes"),
            Self::EmptyLevel => formatter.write_str("codec level must not be empty"),
            Self::LevelTooLong { actual, maximum } => {
                write!(formatter, "codec level length {actual} exceeds {maximum}")
            }
            Self::InvalidLevel => formatter.write_str("codec level contains invalid bytes"),
            Self::CodecConfigTooLarge { actual, maximum } => {
                write!(formatter, "codec config {actual} bytes exceeds {maximum}")
            }
        }
    }
}

impl std::error::Error for FormatError {}
