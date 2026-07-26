use fm_types::InputId;

use crate::StingerSlotId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionKind {
    Fade,
    Wipe,
    Slide,
    Zoom,
    AlphaFade,
    Stinger(StingerSlotId),
}

/// Normalized manual transition position expressed in basis points.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TBarPosition(u16);

impl TBarPosition {
    pub const MAX: u16 = 10_000;
    pub const START: Self = Self(0);
    pub const END: Self = Self(Self::MAX);

    #[must_use]
    pub const fn new(basis_points: u16) -> Option<Self> {
        if basis_points <= Self::MAX {
            Some(Self(basis_points))
        } else {
            None
        }
    }

    #[must_use]
    pub const fn basis_points(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TBarState {
    kind: TransitionKind,
    from: InputId,
    to: InputId,
    position: TBarPosition,
}

impl TBarState {
    pub(crate) const fn new(kind: TransitionKind, from: InputId, to: InputId) -> Self {
        Self {
            kind,
            from,
            to,
            position: TBarPosition::START,
        }
    }

    #[must_use]
    pub const fn kind(self) -> TransitionKind {
        self.kind
    }

    #[must_use]
    pub const fn from(self) -> InputId {
        self.from
    }

    #[must_use]
    pub const fn to(self) -> InputId {
        self.to
    }

    #[must_use]
    pub const fn position(self) -> TBarPosition {
        self.position
    }

    pub(crate) const fn set_position(&mut self, position: TBarPosition) {
        self.position = position;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransitionState {
    kind: TransitionKind,
    from: InputId,
    to: InputId,
    duration_frames: u32,
    elapsed_frames: u32,
}

impl TransitionState {
    #[must_use]
    pub const fn new(
        kind: TransitionKind,
        from: InputId,
        to: InputId,
        duration_frames: u32,
    ) -> Self {
        Self {
            kind,
            from,
            to,
            duration_frames,
            elapsed_frames: 0,
        }
    }

    #[must_use]
    pub const fn kind(self) -> TransitionKind {
        self.kind
    }

    #[must_use]
    pub const fn from(self) -> InputId {
        self.from
    }

    #[must_use]
    pub const fn to(self) -> InputId {
        self.to
    }

    #[must_use]
    pub const fn duration_frames(self) -> u32 {
        self.duration_frames
    }

    #[must_use]
    pub const fn elapsed_frames(self) -> u32 {
        self.elapsed_frames
    }

    #[must_use]
    pub const fn mix_numerator(self) -> u32 {
        self.elapsed_frames
    }

    #[must_use]
    pub const fn mix_denominator(self) -> u32 {
        self.duration_frames
    }

    pub(crate) const fn advance(&mut self) -> bool {
        self.elapsed_frames += 1;
        self.elapsed_frames >= self.duration_frames
    }
}
