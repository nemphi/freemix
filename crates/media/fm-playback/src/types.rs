use core::fmt;

macro_rules! stable_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(u128);

        impl $name {
            #[must_use]
            pub const fn new(value: u128) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn get(self) -> u128 {
                self.0
            }
        }

        impl From<u128> for $name {
            fn from(value: u128) -> Self {
                Self::new(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

stable_id!(ClipId);
stable_id!(PlaylistEntryId);
stable_id!(GoId);

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FrameIndex(u64);

impl FrameIndex {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for FrameIndex {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for FrameIndex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A scheduler-independent output-frame coordinate.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ScheduleCoordinate(u64);

impl ScheduleCoordinate {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for ScheduleCoordinate {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for ScheduleCoordinate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Speed {
    Pause,
    Forward1x,
    Forward2x,
    Reverse1x,
    Reverse2x,
}

impl Speed {
    #[must_use]
    pub const fn direction(self) -> Option<SpeedDirection> {
        match self {
            Self::Pause => None,
            Self::Forward1x | Self::Forward2x => Some(SpeedDirection::Forward),
            Self::Reverse1x | Self::Reverse2x => Some(SpeedDirection::Reverse),
        }
    }

    #[must_use]
    pub const fn frame_step(self) -> u64 {
        match self {
            Self::Pause => 0,
            Self::Forward1x | Self::Reverse1x => 1,
            Self::Forward2x | Self::Reverse2x => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpeedDirection {
    Forward,
    Reverse,
}
