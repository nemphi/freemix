use core::{
    fmt,
    num::NonZeroU128,
    ops::{BitOr, BitOrAssign},
};

use fm_types::{MediaTimestamp, TimeBase, Timecode};

const NANOS_PER_SECOND: i128 = 1_000_000_000;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ClockDomainId(NonZeroU128);

impl ClockDomainId {
    #[must_use]
    pub const fn new(value: NonZeroU128) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> NonZeroU128 {
        self.0
    }
}

impl From<NonZeroU128> for ClockDomainId {
    fn from(value: NonZeroU128) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for ClockDomainId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SequenceNumber(u64);

impl SequenceNumber {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimingError {
    NormalizationOverflow,
    ZeroDuration,
    PresentationEndOverflow,
}

impl fmt::Display for TimingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NormalizationOverflow => "timestamp normalization overflow",
            Self::ZeroDuration => "media duration must be nonzero",
            Self::PresentationEndOverflow => "presentation timestamp plus duration overflows",
        })
    }
}

impl std::error::Error for TimingError {}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct OriginalTimestamp {
    timestamp: MediaTimestamp,
    time_base: TimeBase,
}

impl OriginalTimestamp {
    #[must_use]
    pub const fn new(timestamp: MediaTimestamp, time_base: TimeBase) -> Self {
        Self {
            timestamp,
            time_base,
        }
    }

    #[must_use]
    pub const fn timestamp(self) -> MediaTimestamp {
        self.timestamp
    }

    #[must_use]
    pub const fn time_base(self) -> TimeBase {
        self.time_base
    }

    /// Converts source ticks to the normalized nanosecond timeline.
    ///
    /// # Errors
    ///
    /// Returns [`TimingError::NormalizationOverflow`] if the result is outside
    /// the signed normalized timestamp range.
    pub fn normalize(self) -> Result<NormalizedTimestamp, TimingError> {
        let nanos = i128::from(self.timestamp.ticks())
            .checked_mul(i128::from(self.time_base.numerator()))
            .and_then(|value| value.checked_mul(NANOS_PER_SECOND))
            .ok_or(TimingError::NormalizationOverflow)?
            / i128::from(self.time_base.denominator());
        let nanos = i64::try_from(nanos).map_err(|_| TimingError::NormalizationOverflow)?;
        Ok(NormalizedTimestamp(nanos))
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NormalizedTimestamp(i64);

impl NormalizedTimestamp {
    #[must_use]
    pub const fn from_nanos(nanos: i64) -> Self {
        Self(nanos)
    }

    #[must_use]
    pub const fn as_nanos(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NormalizedDuration(u64);

impl NormalizedDuration {
    /// Creates a non-empty duration on the normalized nanosecond timeline.
    ///
    /// # Errors
    ///
    /// Returns [`TimingError::ZeroDuration`] for a zero duration.
    pub const fn from_nanos(nanos: u64) -> Result<Self, TimingError> {
        if nanos == 0 {
            Err(TimingError::ZeroDuration)
        } else {
            Ok(Self(nanos))
        }
    }

    #[must_use]
    pub const fn as_nanos(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct MediaFlags(u8);

impl MediaFlags {
    pub const NONE: Self = Self(0);
    pub const DISCONTINUITY: Self = Self(1 << 0);
    pub const CORRUPTED: Self = Self(1 << 1);
    const ALL: u8 = Self::DISCONTINUITY.0 | Self::CORRUPTED.0;

    /// Validates a serialized bit representation.
    ///
    /// # Errors
    ///
    /// Returns [`MediaFlagError`] if unknown bits are set.
    pub const fn from_bits(bits: u8) -> Result<Self, MediaFlagError> {
        if bits & !Self::ALL == 0 {
            Ok(Self(bits))
        } else {
            Err(MediaFlagError { bits })
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

impl BitOr for MediaFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for MediaFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaFlagError {
    bits: u8,
}

impl MediaFlagError {
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.bits
    }
}

impl fmt::Display for MediaFlagError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "media flags contain unknown bits: {:#04x}",
            self.bits
        )
    }
}

impl std::error::Error for MediaFlagError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaTiming {
    original_timestamp: OriginalTimestamp,
    presentation_timestamp: NormalizedTimestamp,
    duration: NormalizedDuration,
    clock_domain: ClockDomainId,
    sequence: SequenceNumber,
    flags: MediaFlags,
    capture_timestamp: Option<OriginalTimestamp>,
    timecode: Option<Timecode>,
}

impl MediaTiming {
    /// Creates timing metadata and verifies that its normalized interval fits.
    ///
    /// # Errors
    ///
    /// Returns [`TimingError::PresentationEndOverflow`] if PTS plus duration is
    /// not representable on the signed normalized timeline.
    pub fn new(
        original_timestamp: OriginalTimestamp,
        presentation_timestamp: NormalizedTimestamp,
        duration: NormalizedDuration,
        clock_domain: ClockDomainId,
        sequence: SequenceNumber,
    ) -> Result<Self, TimingError> {
        let duration_nanos =
            i64::try_from(duration.0).map_err(|_| TimingError::PresentationEndOverflow)?;
        presentation_timestamp
            .0
            .checked_add(duration_nanos)
            .ok_or(TimingError::PresentationEndOverflow)?;
        Ok(Self {
            original_timestamp,
            presentation_timestamp,
            duration,
            clock_domain,
            sequence,
            flags: MediaFlags::NONE,
            capture_timestamp: None,
            timecode: None,
        })
    }

    #[must_use]
    pub const fn with_flags(mut self, flags: MediaFlags) -> Self {
        self.flags = flags;
        self
    }

    #[must_use]
    pub const fn with_capture_timestamp(mut self, timestamp: OriginalTimestamp) -> Self {
        self.capture_timestamp = Some(timestamp);
        self
    }

    #[must_use]
    pub const fn with_timecode(mut self, timecode: Timecode) -> Self {
        self.timecode = Some(timecode);
        self
    }

    #[must_use]
    pub const fn original_timestamp(self) -> OriginalTimestamp {
        self.original_timestamp
    }

    #[must_use]
    pub const fn presentation_timestamp(self) -> NormalizedTimestamp {
        self.presentation_timestamp
    }

    #[must_use]
    pub const fn duration(self) -> NormalizedDuration {
        self.duration
    }

    #[must_use]
    pub const fn clock_domain(self) -> ClockDomainId {
        self.clock_domain
    }

    #[must_use]
    pub const fn sequence(self) -> SequenceNumber {
        self.sequence
    }

    #[must_use]
    pub const fn flags(self) -> MediaFlags {
        self.flags
    }

    #[must_use]
    pub const fn capture_timestamp(self) -> Option<OriginalTimestamp> {
        self.capture_timestamp
    }

    #[must_use]
    pub const fn timecode(self) -> Option<Timecode> {
        self.timecode
    }
}
