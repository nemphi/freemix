use fm_audio::Gain;
use fm_frame::NormalizedTimestamp;
use fm_playback::{FrameIndex, Speed};

use crate::{CameraId, EventId, ReplayError, ReplayEvent, TimelineRange};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayChannelId {
    A,
    B,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaybackRate(i32);

impl PlaybackRate {
    pub const PAUSED: Self = Self(0);
    pub const FORWARD_1X: Self = Self(1_000);
    pub const REVERSE_1X: Self = Self(-1_000);

    /// Creates a rate in thousandths of real time.
    ///
    /// # Errors
    ///
    /// Rates outside -16x through +16x are rejected.
    pub const fn from_milli(rate: i32) -> Result<Self, ReplayError> {
        if rate < -16_000 || rate > 16_000 {
            Err(ReplayError::InvalidPlaybackRate(rate))
        } else {
            Ok(Self(rate))
        }
    }

    #[must_use]
    pub const fn as_milli(self) -> i32 {
        self.0
    }
}

impl From<Speed> for PlaybackRate {
    fn from(speed: Speed) -> Self {
        match speed {
            Speed::Pause => Self::PAUSED,
            Speed::Forward1x => Self::FORWARD_1X,
            Speed::Forward2x => Self(2_000),
            Speed::Reverse1x => Self::REVERSE_1X,
            Speed::Reverse2x => Self(-2_000),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayAudioSource {
    FollowAngle,
    Camera(CameraId),
    Muted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VariableSpeedAudio {
    Mute,
    Resample,
    PitchCorrect,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioPolicy {
    pub source: ReplayAudioSource,
    pub variable_speed: VariableSpeedAudio,
    pub gain: Gain,
}

impl Default for AudioPolicy {
    fn default() -> Self {
        Self {
            source: ReplayAudioSource::FollowAngle,
            variable_speed: VariableSpeedAudio::Mute,
            gain: Gain::UNITY,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelMode {
    Live,
    Recorded {
        event_id: EventId,
        timeline: TimelineRange,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelTransport {
    Stopped,
    Paused,
    Playing,
    Jog,
    Shuttle,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReplayChannel {
    pub id: ReplayChannelId,
    pub mode: ChannelMode,
    pub angle: CameraId,
    pub cursor: NormalizedTimestamp,
    pub transport: ChannelTransport,
    pub rate: PlaybackRate,
    pub auto_return: bool,
    pub audio: AudioPolicy,
}

impl ReplayChannel {
    #[must_use]
    pub fn new(id: ReplayChannelId, angle: CameraId, live_edge: NormalizedTimestamp) -> Self {
        Self {
            id,
            mode: ChannelMode::Live,
            angle,
            cursor: live_edge,
            transport: ChannelTransport::Stopped,
            rate: PlaybackRate::PAUSED,
            auto_return: false,
            audio: AudioPolicy::default(),
        }
    }

    fn cue(&mut self, event: &ReplayEvent) {
        self.mode = ChannelMode::Recorded {
            event_id: event.id,
            timeline: event.timeline,
        };
        self.angle = event.preferred_angle;
        self.cursor = event.timeline.start;
        self.transport = ChannelTransport::Paused;
        self.rate = PlaybackRate::PAUSED;
    }

    fn go_live(&mut self, live_edge: NormalizedTimestamp) {
        self.mode = ChannelMode::Live;
        self.cursor = live_edge;
        self.transport = ChannelTransport::Stopped;
        self.rate = PlaybackRate::PAUSED;
    }

    fn play(&mut self, rate: PlaybackRate, transport: ChannelTransport) {
        self.rate = rate;
        self.transport = if rate == PlaybackRate::PAUSED {
            ChannelTransport::Paused
        } else {
            transport
        };
    }

    fn jog(&mut self, frames: i64, frame_duration_nanos: u64) {
        let delta = i128::from(frames) * i128::from(frame_duration_nanos);
        self.move_cursor(delta);
        self.transport = ChannelTransport::Jog;
        self.rate = PlaybackRate::PAUSED;
    }

    fn advance(&mut self, elapsed_nanos: u64, live_edge: NormalizedTimestamp) {
        if !matches!(
            self.transport,
            ChannelTransport::Playing | ChannelTransport::Shuttle
        ) {
            return;
        }
        let delta = i128::from(elapsed_nanos) * i128::from(self.rate.as_milli()) / 1_000;
        let crossed = self.move_cursor(delta);
        if crossed {
            if self.auto_return {
                self.go_live(live_edge);
            } else {
                self.transport = ChannelTransport::Paused;
                self.rate = PlaybackRate::PAUSED;
            }
        }
    }

    fn move_cursor(&mut self, delta: i128) -> bool {
        let ChannelMode::Recorded { timeline, .. } = self.mode else {
            return false;
        };
        let start = i128::from(timeline.start.as_nanos());
        let end = i128::from(timeline.end.as_nanos()) - 1;
        let requested = i128::from(self.cursor.as_nanos()).saturating_add(delta);
        let bounded = requested.clamp(start, end);
        self.cursor = NormalizedTimestamp::from_nanos(
            i64::try_from(bounded).expect("timeline bounds originate as i64"),
        );
        requested < start || requested > end
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReplayDecks {
    a: ReplayChannel,
    b: ReplayChannel,
    linked: bool,
}

impl ReplayDecks {
    #[must_use]
    pub fn new(angle_a: CameraId, angle_b: CameraId, live_edge: NormalizedTimestamp) -> Self {
        Self {
            a: ReplayChannel::new(ReplayChannelId::A, angle_a, live_edge),
            b: ReplayChannel::new(ReplayChannelId::B, angle_b, live_edge),
            linked: false,
        }
    }

    #[must_use]
    pub const fn linked(&self) -> bool {
        self.linked
    }

    pub const fn set_linked(&mut self, linked: bool) {
        self.linked = linked;
    }

    #[must_use]
    pub const fn channel(&self, id: ReplayChannelId) -> &ReplayChannel {
        match id {
            ReplayChannelId::A => &self.a,
            ReplayChannelId::B => &self.b,
        }
    }

    pub fn select_angle(&mut self, id: ReplayChannelId, camera_id: CameraId) {
        self.channel_mut(id).angle = camera_id;
    }

    pub fn set_audio_policy(&mut self, id: ReplayChannelId, policy: AudioPolicy) {
        self.channel_mut(id).audio = policy;
    }

    pub fn set_auto_return(&mut self, id: ReplayChannelId, enabled: bool) {
        self.for_transport_mut(id, |channel| channel.auto_return = enabled);
    }

    pub fn cue_event(&mut self, id: ReplayChannelId, event: &ReplayEvent) {
        self.for_transport_mut(id, |channel| channel.cue(event));
    }

    pub fn go_live(&mut self, id: ReplayChannelId, live_edge: NormalizedTimestamp) {
        self.for_transport_mut(id, |channel| channel.go_live(live_edge));
    }

    pub fn play(&mut self, id: ReplayChannelId, rate: PlaybackRate) {
        self.for_transport_mut(id, |channel| channel.play(rate, ChannelTransport::Playing));
    }

    pub fn shuttle(&mut self, id: ReplayChannelId, rate: PlaybackRate) {
        self.for_transport_mut(id, |channel| channel.play(rate, ChannelTransport::Shuttle));
    }

    pub fn pause(&mut self, id: ReplayChannelId) {
        self.play(id, PlaybackRate::PAUSED);
    }

    /// Moves one or both linked channels by exact frame increments.
    ///
    /// # Errors
    ///
    /// A zero frame duration is rejected without changing either channel.
    pub fn jog(
        &mut self,
        id: ReplayChannelId,
        frames: i64,
        frame_duration_nanos: u64,
    ) -> Result<(), ReplayError> {
        if frame_duration_nanos == 0 {
            return Err(ReplayError::FrameDurationZero);
        }
        self.for_transport_mut(id, |channel| {
            channel.jog(frames, frame_duration_nanos);
        });
        Ok(())
    }

    /// Seeks by frame offset from the event mark-in and pauses.
    ///
    /// # Errors
    ///
    /// A zero frame duration is rejected without changing either channel.
    pub fn seek_frame(
        &mut self,
        id: ReplayChannelId,
        frame: FrameIndex,
        frame_duration_nanos: u64,
    ) -> Result<(), ReplayError> {
        if frame_duration_nanos == 0 {
            return Err(ReplayError::FrameDurationZero);
        }
        let offset = i128::from(frame.get()) * i128::from(frame_duration_nanos);
        self.for_transport_mut(id, |channel| {
            if let ChannelMode::Recorded { timeline, .. } = channel.mode {
                channel.cursor = timeline.start;
                channel.move_cursor(offset);
                channel.transport = ChannelTransport::Paused;
            }
        });
        Ok(())
    }

    pub fn advance(
        &mut self,
        id: ReplayChannelId,
        elapsed_nanos: u64,
        live_edge: NormalizedTimestamp,
    ) {
        self.for_transport_mut(id, |channel| channel.advance(elapsed_nanos, live_edge));
    }

    fn channel_mut(&mut self, id: ReplayChannelId) -> &mut ReplayChannel {
        match id {
            ReplayChannelId::A => &mut self.a,
            ReplayChannelId::B => &mut self.b,
        }
    }

    fn for_transport_mut(
        &mut self,
        id: ReplayChannelId,
        mut operation: impl FnMut(&mut ReplayChannel),
    ) {
        operation(self.channel_mut(id));
        if self.linked {
            let other = match id {
                ReplayChannelId::A => ReplayChannelId::B,
                ReplayChannelId::B => ReplayChannelId::A,
            };
            operation(self.channel_mut(other));
        }
    }
}
