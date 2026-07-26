use crate::{ClipId, CodecError, FrameCodec, FrameIndex, PlaybackClip, Speed, SpeedDirection};
use core::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportState {
    Stopped,
    Playing,
    Paused,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndBehavior {
    Hold,
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Marks {
    pub mark_in: FrameIndex,
    pub mark_out: FrameIndex,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MarkError {
    Reversed {
        mark_in: FrameIndex,
        mark_out: FrameIndex,
    },
}

impl fmt::Display for MarkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reversed { mark_in, mark_out } => {
                write!(formatter, "mark-in {mark_in} is after mark-out {mark_out}")
            }
        }
    }
}

impl std::error::Error for MarkError {}

impl Marks {
    /// Creates an inclusive marked range.
    ///
    /// # Errors
    ///
    /// Returns [`MarkError::Reversed`] when mark-in is after mark-out.
    pub fn new(mark_in: FrameIndex, mark_out: FrameIndex) -> Result<Self, MarkError> {
        if mark_in > mark_out {
            return Err(MarkError::Reversed { mark_in, mark_out });
        }
        Ok(Self { mark_in, mark_out })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaybackError {
    InvalidMarks(MarkError),
    MarkOutOfRange {
        mark_out: FrameIndex,
        frame_count: u64,
    },
    SeekOutsideMarks {
        requested: FrameIndex,
        marks: Marks,
    },
    MissingFrame {
        clip: ClipId,
        frame: FrameIndex,
    },
    MissingCodec(ClipId),
    Codec(CodecError),
}

impl fmt::Display for PlaybackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMarks(error) => error.fmt(formatter),
            Self::MarkOutOfRange {
                mark_out,
                frame_count,
            } => write!(
                formatter,
                "mark-out {mark_out} is outside a {frame_count}-frame clip"
            ),
            Self::SeekOutsideMarks { requested, marks } => write!(
                formatter,
                "seek {requested} is outside inclusive marks {}..={}",
                marks.mark_in, marks.mark_out
            ),
            Self::MissingFrame { clip, frame } => {
                write!(formatter, "clip {clip} has no frame at {frame}")
            }
            Self::MissingCodec(clip) => write!(formatter, "clip {clip} requires a codec"),
            Self::Codec(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for PlaybackError {}

impl From<CodecError> for PlaybackError {
    fn from(value: CodecError) -> Self {
        Self::Codec(value)
    }
}

impl From<MarkError> for PlaybackError {
    fn from(value: MarkError) -> Self {
        Self::InvalidMarks(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaybackFrame<F> {
    pub clip_id: ClipId,
    pub index: FrameIndex,
    pub frame: F,
    /// True when this output crossed the inclusive marked boundary.
    pub ended: bool,
}

#[derive(Clone, Debug)]
pub struct Transport<F> {
    clip: PlaybackClip<F>,
    state: TransportState,
    cursor: FrameIndex,
    marks: Marks,
    motion: Speed,
    looping: bool,
    end_behavior: EndBehavior,
}

impl<F> Transport<F> {
    #[must_use]
    pub fn new(clip: PlaybackClip<F>) -> Self {
        let mark_in = FrameIndex::new(0);
        let mark_out = FrameIndex::new(clip.frame_count() - 1);
        Self {
            clip,
            state: TransportState::Stopped,
            cursor: mark_in,
            marks: Marks { mark_in, mark_out },
            motion: Speed::Forward1x,
            looping: false,
            end_behavior: EndBehavior::Hold,
        }
    }

    #[must_use]
    pub const fn clip_id(&self) -> ClipId {
        self.clip.id()
    }

    #[must_use]
    pub const fn state(&self) -> TransportState {
        self.state
    }

    #[must_use]
    pub const fn cursor(&self) -> FrameIndex {
        self.cursor
    }

    #[must_use]
    pub const fn marks(&self) -> Marks {
        self.marks
    }

    /// Returns pause when the transport is not advancing.
    #[must_use]
    pub const fn speed(&self) -> Speed {
        if matches!(self.state, TransportState::Playing) {
            self.motion
        } else {
            Speed::Pause
        }
    }

    #[must_use]
    pub const fn direction(&self) -> SpeedDirection {
        match self.motion.direction() {
            Some(direction) => direction,
            None => SpeedDirection::Forward,
        }
    }

    #[must_use]
    pub const fn is_looping(&self) -> bool {
        self.looping
    }

    #[must_use]
    pub const fn end_behavior(&self) -> EndBehavior {
        self.end_behavior
    }

    pub fn play(&mut self) {
        self.state = TransportState::Playing;
    }

    pub fn pause(&mut self) {
        self.state = TransportState::Paused;
    }

    /// Stops and rewinds to mark-in.
    pub fn stop(&mut self) {
        self.state = TransportState::Stopped;
        self.cursor = self.marks.mark_in;
    }

    pub fn set_speed(&mut self, speed: Speed) {
        if speed == Speed::Pause {
            self.pause();
        } else {
            self.motion = speed;
            self.play();
        }
    }

    pub const fn set_looping(&mut self, looping: bool) {
        self.looping = looping;
    }

    pub const fn set_end_behavior(&mut self, behavior: EndBehavior) {
        self.end_behavior = behavior;
    }

    /// Replaces the inclusive playable range.
    ///
    /// The cursor is reset to mark-in when it falls outside the new range.
    ///
    /// # Errors
    ///
    /// Returns an error if mark-out is outside the clip.
    pub fn set_marks(&mut self, marks: Marks) -> Result<(), PlaybackError> {
        if marks.mark_in > marks.mark_out {
            return Err(MarkError::Reversed {
                mark_in: marks.mark_in,
                mark_out: marks.mark_out,
            }
            .into());
        }
        if marks.mark_out.get() >= self.clip.frame_count() {
            return Err(PlaybackError::MarkOutOfRange {
                mark_out: marks.mark_out,
                frame_count: self.clip.frame_count(),
            });
        }
        self.marks = marks;
        if self.cursor < marks.mark_in || self.cursor > marks.mark_out {
            self.cursor = marks.mark_in;
        }
        Ok(())
    }

    pub fn clear_marks(&mut self) {
        self.marks = Marks {
            mark_in: FrameIndex::new(0),
            mark_out: FrameIndex::new(self.clip.frame_count() - 1),
        };
    }

    /// Seeks to an exact frame in the marked range without changing state.
    ///
    /// # Errors
    ///
    /// Returns [`PlaybackError::SeekOutsideMarks`] outside the inclusive marks.
    pub fn seek(&mut self, frame: FrameIndex) -> Result<(), PlaybackError> {
        if frame < self.marks.mark_in || frame > self.marks.mark_out {
            return Err(PlaybackError::SeekOutsideMarks {
                requested: frame,
                marks: self.marks,
            });
        }
        self.cursor = frame;
        Ok(())
    }
}

impl<F: Clone> Transport<F> {
    /// Reads the current frame, then advances by the configured integral speed.
    ///
    /// Failed reads do not move the cursor or alter transport state.
    ///
    /// # Errors
    ///
    /// Returns a missing-frame error for a fixture hole, a missing-codec error
    /// for an encoded clip without an adapter, or the adapter's codec error.
    pub fn pull_frame(
        &mut self,
        codec: Option<&mut dyn FrameCodec<F>>,
    ) -> Result<PlaybackFrame<F>, PlaybackError> {
        let index = self.cursor;
        let frame = match &self.clip {
            PlaybackClip::Fixture(clip) => {
                clip.frame(index)
                    .cloned()
                    .ok_or(PlaybackError::MissingFrame {
                        clip: clip.id(),
                        frame: index,
                    })?
            }
            PlaybackClip::Encoded(clip) => codec
                .ok_or(PlaybackError::MissingCodec(clip.id()))?
                .read_frame(clip, index)?,
        };

        let ended = if self.state == TransportState::Playing {
            self.advance()
        } else {
            false
        };
        Ok(PlaybackFrame {
            clip_id: self.clip.id(),
            index,
            frame,
            ended,
        })
    }

    fn advance(&mut self) -> bool {
        let step = self.motion.frame_step();
        let mark_in = self.marks.mark_in.get();
        let mark_out = self.marks.mark_out.get();
        let current = self.cursor.get();
        let range_len = mark_out - mark_in + 1;

        let crossed = match self.direction() {
            SpeedDirection::Forward => current.checked_add(step).is_none_or(|next| next > mark_out),
            SpeedDirection::Reverse => step > current - mark_in,
        };

        if !crossed {
            self.cursor = match self.direction() {
                SpeedDirection::Forward => FrameIndex::new(current + step),
                SpeedDirection::Reverse => FrameIndex::new(current - step),
            };
            return false;
        }

        if self.looping {
            let offset = current - mark_in;
            let delta = step % range_len;
            let next_offset = match self.direction() {
                SpeedDirection::Forward if delta >= range_len - offset => {
                    delta - (range_len - offset)
                }
                SpeedDirection::Forward => offset + delta,
                SpeedDirection::Reverse if delta > offset => range_len - (delta - offset),
                SpeedDirection::Reverse => offset - delta,
            };
            self.cursor = FrameIndex::new(mark_in + next_offset);
        } else {
            match self.end_behavior {
                EndBehavior::Hold => {
                    self.cursor = match self.direction() {
                        SpeedDirection::Forward => self.marks.mark_out,
                        SpeedDirection::Reverse => self.marks.mark_in,
                    };
                    self.state = TransportState::Paused;
                }
                EndBehavior::Stop => {
                    self.cursor = self.marks.mark_in;
                    self.state = TransportState::Stopped;
                }
            }
        }
        true
    }
}
