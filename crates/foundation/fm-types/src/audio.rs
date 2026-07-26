#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SampleFormat {
    I16,
    I24,
    I32,
    F32,
    F64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SampleRate(u32);

impl SampleRate {
    #[must_use]
    pub const fn new(hertz: u32) -> Option<Self> {
        if hertz == 0 { None } else { Some(Self(hertz)) }
    }

    #[must_use]
    pub const fn hertz(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Channel {
    Mono,
    Left,
    Right,
    Center,
    LowFrequency,
    LeftSurround,
    RightSurround,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ChannelLayout(Vec<Channel>);

impl ChannelLayout {
    #[must_use]
    pub fn new(channels: Vec<Channel>) -> Option<Self> {
        if channels.is_empty() {
            None
        } else {
            Some(Self(channels))
        }
    }

    #[must_use]
    pub fn channels(&self) -> &[Channel] {
        &self.0
    }

    #[must_use]
    pub fn stereo() -> Self {
        Self(vec![Channel::Left, Channel::Right])
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AudioFormat {
    pub sample_rate: SampleRate,
    pub sample_format: SampleFormat,
    pub channels: ChannelLayout,
}
