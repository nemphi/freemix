use core::{
    fmt,
    num::{NonZeroU32, NonZeroU64},
    ops::{BitOr, BitOrAssign},
};

use crate::{MediaTiming, OriginalTimestamp, ResourceLease};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CodecId(String);

impl CodecId {
    pub const MAX_LENGTH: usize = 64;

    /// Creates a portable codec identifier such as `video/h264`.
    ///
    /// # Errors
    ///
    /// Returns a typed error for an empty, oversized, or non-portable value.
    pub fn new(value: impl Into<String>) -> Result<Self, EncodedPacketError> {
        let value = value.into();
        if value.is_empty() {
            return Err(EncodedPacketError::EmptyCodecId);
        }
        if value.len() > Self::MAX_LENGTH {
            return Err(EncodedPacketError::CodecIdTooLong {
                actual: value.len(),
                maximum: Self::MAX_LENGTH,
            });
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'))
        {
            return Err(EncodedPacketError::InvalidCodecId);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CodecId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StreamId(NonZeroU32);

impl StreamId {
    #[must_use]
    pub const fn new(value: NonZeroU32) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> NonZeroU32 {
        self.0
    }
}

impl From<NonZeroU32> for StreamId {
    fn from(value: NonZeroU32) -> Self {
        Self::new(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CodecConfigGeneration(NonZeroU64);

impl CodecConfigGeneration {
    #[must_use]
    pub const fn new(value: NonZeroU64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> NonZeroU64 {
        self.0
    }
}

impl From<NonZeroU64> for CodecConfigGeneration {
    fn from(value: NonZeroU64) -> Self {
        Self::new(value)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct PacketFlags(u8);

impl PacketFlags {
    pub const NONE: Self = Self(0);
    pub const RANDOM_ACCESS: Self = Self(1 << 0);
    pub const DEPENDS_ON_OTHERS: Self = Self(1 << 1);
    pub const DISPOSABLE: Self = Self(1 << 2);
    const ALL: u8 = Self::RANDOM_ACCESS.0 | Self::DEPENDS_ON_OTHERS.0 | Self::DISPOSABLE.0;

    /// Validates a serialized bit representation.
    ///
    /// # Errors
    ///
    /// Returns [`PacketFlagError`] if unknown bits are set.
    pub const fn from_bits(bits: u8) -> Result<Self, PacketFlagError> {
        if bits & !Self::ALL == 0 {
            Ok(Self(bits))
        } else {
            Err(PacketFlagError { bits })
        }
    }

    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    #[must_use]
    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl BitOr for PacketFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for PacketFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketFlagError {
    bits: u8,
}

impl PacketFlagError {
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.bits
    }
}

impl fmt::Display for PacketFlagError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "packet flags contain unknown bits: {:#04x}",
            self.bits
        )
    }
}

impl std::error::Error for PacketFlagError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedPacketMetadata {
    codec: CodecId,
    config_generation: CodecConfigGeneration,
    stream_id: StreamId,
    channel_index: Option<u16>,
    timing: MediaTiming,
    decode_timestamp: OriginalTimestamp,
    flags: PacketFlags,
}

impl EncodedPacketMetadata {
    /// Creates metadata for one compressed access unit.
    ///
    /// # Errors
    ///
    /// Returns an error if PTS and DTS do not share the declared source
    /// timebase.
    pub fn new(
        codec: CodecId,
        config_generation: CodecConfigGeneration,
        stream_id: StreamId,
        channel_index: Option<u16>,
        timing: MediaTiming,
        decode_timestamp: OriginalTimestamp,
        flags: PacketFlags,
    ) -> Result<Self, EncodedPacketError> {
        if timing.original_timestamp().time_base() != decode_timestamp.time_base() {
            return Err(EncodedPacketError::TimestampTimeBaseMismatch);
        }
        Ok(Self {
            codec,
            config_generation,
            stream_id,
            channel_index,
            timing,
            decode_timestamp,
            flags,
        })
    }

    #[must_use]
    pub const fn codec(&self) -> &CodecId {
        &self.codec
    }

    #[must_use]
    pub const fn config_generation(&self) -> CodecConfigGeneration {
        self.config_generation
    }

    #[must_use]
    pub const fn stream_id(&self) -> StreamId {
        self.stream_id
    }

    #[must_use]
    pub const fn channel_index(&self) -> Option<u16> {
        self.channel_index
    }

    #[must_use]
    pub const fn timing(&self) -> MediaTiming {
        self.timing
    }

    #[must_use]
    pub const fn decode_timestamp(&self) -> OriginalTimestamp {
        self.decode_timestamp
    }

    #[must_use]
    pub const fn flags(&self) -> PacketFlags {
        self.flags
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum EncodedPayload {
    Bytes(Vec<u8>),
    Resource(ResourceLease),
}

#[derive(Debug, Eq, PartialEq)]
pub struct EncodedPacket {
    metadata: EncodedPacketMetadata,
    payload: EncodedPayload,
}

impl EncodedPacket {
    pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

    /// Creates a packet with a bounded owned byte payload.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty or oversized access unit.
    pub fn from_bytes(
        metadata: EncodedPacketMetadata,
        payload: Vec<u8>,
    ) -> Result<Self, EncodedPacketError> {
        if payload.is_empty() {
            return Err(EncodedPacketError::EmptyPayload);
        }
        if payload.len() > Self::MAX_PAYLOAD_BYTES {
            return Err(EncodedPacketError::PayloadTooLarge {
                actual: payload.len(),
                maximum: Self::MAX_PAYLOAD_BYTES,
            });
        }
        Ok(Self {
            metadata,
            payload: EncodedPayload::Bytes(payload),
        })
    }

    #[must_use]
    pub const fn from_resource(metadata: EncodedPacketMetadata, lease: ResourceLease) -> Self {
        Self {
            metadata,
            payload: EncodedPayload::Resource(lease),
        }
    }

    #[must_use]
    pub const fn metadata(&self) -> &EncodedPacketMetadata {
        &self.metadata
    }

    #[must_use]
    pub const fn payload(&self) -> &EncodedPayload {
        &self.payload
    }

    #[must_use]
    pub fn into_payload(self) -> EncodedPayload {
        self.payload
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncodedPacketError {
    EmptyCodecId,
    CodecIdTooLong { actual: usize, maximum: usize },
    InvalidCodecId,
    TimestampTimeBaseMismatch,
    EmptyPayload,
    PayloadTooLarge { actual: usize, maximum: usize },
}

impl fmt::Display for EncodedPacketError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyCodecId => formatter.write_str("codec identifier must not be empty"),
            Self::CodecIdTooLong { actual, maximum } => {
                write!(
                    formatter,
                    "codec identifier length {actual} exceeds {maximum}"
                )
            }
            Self::InvalidCodecId => formatter.write_str("codec identifier contains invalid bytes"),
            Self::TimestampTimeBaseMismatch => {
                formatter.write_str("packet PTS and DTS timebases differ")
            }
            Self::EmptyPayload => formatter.write_str("encoded packet payload must not be empty"),
            Self::PayloadTooLarge { actual, maximum } => {
                write!(
                    formatter,
                    "encoded payload {actual} bytes exceeds {maximum}"
                )
            }
        }
    }
}

impl std::error::Error for EncodedPacketError {}
