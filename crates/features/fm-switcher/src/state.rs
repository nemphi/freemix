use fm_types::{InputId, OutputId};

use crate::{
    MissingMediaFallback, OVERLAY_CHANNEL_COUNT, OverlayChannelId, OverlayChannelState,
    STINGER_SLOT_COUNT, StingerDescriptor, StingerPlaybackDecision, StingerPreloadState,
    StingerSlotId, StingerSlotState, SwitcherCommand, SwitcherError, SwitcherEvent, TBarPosition,
    TBarState, TransitionKind, TransitionState,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProgramFrame {
    pub primary: InputId,
    pub secondary: Option<InputId>,
    pub transition_kind: Option<TransitionKind>,
    /// Point mix used to compose the video frame.
    pub mix_numerator: u32,
    pub mix_denominator: u32,
    /// Transition mix at the start of this frame's media interval.
    pub mix_start_numerator: u32,
    /// Transition mix at the end of this frame's media interval.
    pub mix_end_numerator: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwitcherState {
    inputs: Vec<InputId>,
    preview: InputId,
    program: InputId,
    transition: Option<TransitionState>,
    t_bar: Option<TBarState>,
    fade_to_black: bool,
    overlays: [OverlayChannelState; OVERLAY_CHANNEL_COUNT],
    stingers: [StingerSlotState; STINGER_SLOT_COUNT],
}

impl SwitcherState {
    /// Creates a mix with distinct initial program and preview inputs.
    ///
    /// # Errors
    ///
    /// Returns [`SwitcherError`] if either selected input is unavailable.
    pub fn new(
        inputs: Vec<InputId>,
        program: InputId,
        preview: InputId,
    ) -> Result<Self, SwitcherError> {
        let state = Self {
            inputs,
            preview,
            program,
            transition: None,
            t_bar: None,
            fade_to_black: false,
            overlays: std::array::from_fn(|_| OverlayChannelState::empty()),
            stingers: std::array::from_fn(|_| StingerSlotState::empty()),
        };
        state.require_input(program)?;
        state.require_input(preview)?;
        Ok(state)
    }

    #[must_use]
    pub fn inputs(&self) -> &[InputId] {
        &self.inputs
    }

    #[must_use]
    pub const fn preview(&self) -> InputId {
        self.preview
    }

    #[must_use]
    pub const fn program(&self) -> InputId {
        self.program
    }

    #[must_use]
    pub const fn transition(&self) -> Option<TransitionState> {
        self.transition
    }

    #[must_use]
    pub const fn t_bar(&self) -> Option<TBarState> {
        self.t_bar
    }

    #[must_use]
    pub const fn fade_to_black(&self) -> bool {
        self.fade_to_black
    }

    #[must_use]
    pub fn overlays(&self) -> &[OverlayChannelState; OVERLAY_CHANNEL_COUNT] {
        &self.overlays
    }

    #[must_use]
    pub fn overlay(&self, channel: OverlayChannelId) -> &OverlayChannelState {
        &self.overlays[channel.index()]
    }

    #[must_use]
    pub fn stingers(&self) -> &[StingerSlotState; STINGER_SLOT_COUNT] {
        &self.stingers
    }

    #[must_use]
    pub fn stinger(&self, slot: StingerSlotId) -> &StingerSlotState {
        &self.stingers[slot.index()]
    }

    /// Applies one operator command atomically to desired switcher state.
    ///
    /// # Errors
    ///
    /// Returns [`SwitcherError`] for unknown inputs or invalid transition state.
    pub fn apply(&mut self, command: SwitcherCommand) -> Result<Vec<SwitcherEvent>, SwitcherError> {
        match command {
            SwitcherCommand::SelectPreview(input) => {
                self.require_idle()?;
                self.require_input(input)?;
                self.preview = input;
                Ok(vec![SwitcherEvent::PreviewSelected { input }])
            }
            SwitcherCommand::Cut => {
                self.require_idle()?;
                Ok(vec![self.complete_take(TransitionKind::Fade)])
            }
            SwitcherCommand::Transition {
                kind,
                duration_frames,
            } => {
                self.require_idle()?;
                if duration_frames == 0 {
                    return Err(SwitcherError::ZeroDuration);
                }
                self.start_transition(kind, duration_frames)
            }
            SwitcherCommand::Wipe { duration_frames } => {
                self.require_idle()?;
                if duration_frames == 0 {
                    return Err(SwitcherError::ZeroDuration);
                }
                self.start_transition(TransitionKind::Wipe, duration_frames)
            }
            SwitcherCommand::StartTBar { kind } => self.start_t_bar(kind),
            SwitcherCommand::SetTBarPosition(position) => self.set_t_bar_position(position),
            SwitcherCommand::CommitTBar => self.commit_t_bar(),
            SwitcherCommand::CancelTBar => self.cancel_t_bar(),
            SwitcherCommand::SetFadeToBlack(active) => Ok(self.set_fade_to_black(active)),
            SwitcherCommand::TakeOverlay { channel, source } => self.take_overlay(channel, source),
            SwitcherCommand::UpdateOverlay { channel, source } => {
                self.update_overlay(channel, source)
            }
            SwitcherCommand::OverlayOff(channel) => self.overlay_off(channel),
            SwitcherCommand::SetOverlayOutputInclusion {
                channel,
                output,
                included,
            } => Ok(self.set_overlay_output_inclusion(channel, output, included)),
        }
    }

    #[must_use]
    pub fn program_frame(&self) -> ProgramFrame {
        if let Some(transition) = self.transition {
            ProgramFrame {
                primary: transition.from(),
                secondary: Some(transition.to()),
                transition_kind: Some(transition.kind()),
                mix_numerator: transition.mix_numerator(),
                mix_denominator: transition.mix_denominator(),
                mix_start_numerator: transition.mix_numerator(),
                mix_end_numerator: transition.mix_end_numerator(),
            }
        } else if let Some(t_bar) = self.t_bar {
            ProgramFrame {
                primary: t_bar.from(),
                secondary: Some(t_bar.to()),
                transition_kind: Some(t_bar.kind()),
                mix_numerator: u32::from(t_bar.position().basis_points()),
                mix_denominator: u32::from(TBarPosition::MAX),
                mix_start_numerator: u32::from(t_bar.interval_start().basis_points()),
                mix_end_numerator: u32::from(t_bar.position().basis_points()),
            }
        } else {
            ProgramFrame {
                primary: self.program,
                secondary: None,
                transition_kind: None,
                mix_numerator: 0,
                mix_denominator: 1,
                mix_start_numerator: 0,
                mix_end_numerator: 0,
            }
        }
    }

    pub fn advance_frame(&mut self) -> Option<SwitcherEvent> {
        if let Some(t_bar) = &mut self.t_bar {
            t_bar.settle_frame();
        }
        let mut transition = self.transition?;
        if transition.advance() {
            let kind = transition.kind();
            self.transition = None;
            Some(self.complete_take(kind))
        } else {
            self.transition = Some(transition);
            None
        }
    }

    /// Advances one frame and reports both the legacy program change and completion event.
    #[must_use]
    pub fn advance_frame_events(&mut self) -> Vec<SwitcherEvent> {
        if let Some(t_bar) = &mut self.t_bar {
            t_bar.settle_frame();
        }
        let Some(mut transition) = self.transition else {
            return Vec::new();
        };
        if !transition.advance() {
            self.transition = Some(transition);
            return Vec::new();
        }

        let kind = transition.kind();
        self.transition = None;
        let program_changed = self.complete_take(kind);
        vec![
            program_changed,
            SwitcherEvent::TransitionCompleted {
                kind,
                program: self.program,
            },
        ]
    }

    /// Configures a stinger slot and clears any readiness result for its previous media.
    pub fn configure_stinger(&mut self, slot: StingerSlotId, descriptor: StingerDescriptor) {
        self.stingers[slot.index()].configure(descriptor);
    }

    /// Records the deterministic result of preloading a stinger's media.
    pub fn preload_stinger(&mut self, slot: StingerSlotId, media_available: bool) -> SwitcherEvent {
        let state = if media_available {
            StingerPreloadState::Ready
        } else {
            StingerPreloadState::Missing
        };
        self.stingers[slot.index()].set_preload_state(state);
        SwitcherEvent::StingerPreloadChanged { slot, state }
    }

    #[must_use]
    pub fn stinger_playback_decision(&self, slot: StingerSlotId) -> StingerPlaybackDecision {
        let stinger = self.stinger(slot);
        let Some(descriptor) = stinger.descriptor() else {
            return StingerPlaybackDecision::Unconfigured;
        };
        if stinger.preload_state() == StingerPreloadState::Ready {
            StingerPlaybackDecision::Play
        } else {
            StingerPlaybackDecision::Fallback(descriptor.missing_media_fallback)
        }
    }

    /// Starts a reversible manual transition at the zero position.
    ///
    /// # Errors
    ///
    /// Returns [`SwitcherError::TransitionInProgress`] when another transition is active.
    pub fn start_t_bar(
        &mut self,
        kind: TransitionKind,
    ) -> Result<Vec<SwitcherEvent>, SwitcherError> {
        self.require_idle()?;
        self.t_bar = Some(TBarState::new(kind, self.program, self.preview));
        Ok(vec![SwitcherEvent::TBarStarted {
            kind,
            from: self.program,
            to: self.preview,
        }])
    }

    /// Moves an active T-bar to an absolute position; lower positions reverse its progress.
    ///
    /// # Errors
    ///
    /// Returns [`SwitcherError::TransitionInProgress`] when no manual transition is active.
    pub fn set_t_bar_position(
        &mut self,
        position: TBarPosition,
    ) -> Result<Vec<SwitcherEvent>, SwitcherError> {
        let t_bar = self
            .t_bar
            .as_mut()
            .ok_or(SwitcherError::TransitionInProgress)?;
        t_bar.set_position(position);
        Ok(vec![SwitcherEvent::TBarPositionChanged { position }])
    }

    /// Completes the active manual transition and swaps program with preview.
    ///
    /// # Errors
    ///
    /// Returns [`SwitcherError::TransitionInProgress`] when no manual transition is active.
    pub fn commit_t_bar(&mut self) -> Result<Vec<SwitcherEvent>, SwitcherError> {
        let t_bar = self
            .t_bar
            .take()
            .ok_or(SwitcherError::TransitionInProgress)?;
        let program_changed = self.complete_take(t_bar.kind());
        Ok(vec![
            program_changed,
            SwitcherEvent::TransitionCompleted {
                kind: t_bar.kind(),
                program: self.program,
            },
        ])
    }

    /// Cancels the active manual transition without changing program or preview.
    ///
    /// # Errors
    ///
    /// Returns [`SwitcherError::TransitionInProgress`] when no manual transition is active.
    pub fn cancel_t_bar(&mut self) -> Result<Vec<SwitcherEvent>, SwitcherError> {
        self.t_bar
            .take()
            .ok_or(SwitcherError::TransitionInProgress)?;
        Ok(vec![SwitcherEvent::TBarCancelled])
    }

    #[must_use]
    pub fn set_fade_to_black(&mut self, active: bool) -> Vec<SwitcherEvent> {
        self.fade_to_black = active;
        vec![SwitcherEvent::FadeToBlackChanged { active }]
    }

    /// Configures and activates an overlay channel.
    ///
    /// # Errors
    ///
    /// Returns [`SwitcherError::UnknownInput`] when the source is unavailable.
    pub fn take_overlay(
        &mut self,
        channel: OverlayChannelId,
        source: InputId,
    ) -> Result<Vec<SwitcherEvent>, SwitcherError> {
        self.require_input(source)?;
        self.overlays[channel.index()].take(source);
        Ok(vec![SwitcherEvent::OverlayTaken { channel, source }])
    }

    /// Replaces the source of a configured overlay without changing its on/off state.
    ///
    /// # Errors
    ///
    /// Returns [`SwitcherError::UnknownInput`] when the source is unavailable.
    pub fn update_overlay(
        &mut self,
        channel: OverlayChannelId,
        source: InputId,
    ) -> Result<Vec<SwitcherEvent>, SwitcherError> {
        self.require_input(source)?;
        self.overlays[channel.index()].update(source);
        Ok(vec![SwitcherEvent::OverlayUpdated { channel, source }])
    }

    /// Turns off a configured overlay while retaining its source and output routing.
    ///
    /// # Errors
    ///
    /// This operation is idempotent, including for an unconfigured channel.
    pub fn overlay_off(
        &mut self,
        channel: OverlayChannelId,
    ) -> Result<Vec<SwitcherEvent>, SwitcherError> {
        self.overlays[channel.index()].off();
        Ok(vec![SwitcherEvent::OverlayTurnedOff { channel }])
    }

    #[must_use]
    pub fn set_overlay_output_inclusion(
        &mut self,
        channel: OverlayChannelId,
        output: OutputId,
        included: bool,
    ) -> Vec<SwitcherEvent> {
        self.overlays[channel.index()].set_output_inclusion(output, included);
        vec![SwitcherEvent::OverlayOutputInclusionChanged {
            channel,
            output,
            included,
        }]
    }

    fn start_transition(
        &mut self,
        kind: TransitionKind,
        duration_frames: u32,
    ) -> Result<Vec<SwitcherEvent>, SwitcherError> {
        if let TransitionKind::Stinger(slot) = kind {
            match self.stinger_playback_decision(slot) {
                StingerPlaybackDecision::Fallback(fallback) => {
                    return self.apply_stinger_fallback(slot, fallback, duration_frames);
                }
                StingerPlaybackDecision::Play | StingerPlaybackDecision::Unconfigured => {}
            }
        }
        self.transition = Some(TransitionState::new(
            kind,
            self.program,
            self.preview,
            duration_frames,
        ));
        Ok(vec![SwitcherEvent::TransitionStarted {
            kind,
            from: self.program,
            to: self.preview,
            duration_frames,
        }])
    }

    fn apply_stinger_fallback(
        &mut self,
        slot: StingerSlotId,
        fallback: MissingMediaFallback,
        duration_frames: u32,
    ) -> Result<Vec<SwitcherEvent>, SwitcherError> {
        let fallback_event = SwitcherEvent::StingerFallbackApplied { slot, fallback };
        match fallback {
            MissingMediaFallback::Cut => Ok(vec![
                fallback_event,
                self.complete_take(TransitionKind::Fade),
            ]),
            MissingMediaFallback::Fade => {
                let mut events = vec![fallback_event];
                events.extend(self.start_transition(TransitionKind::Fade, duration_frames)?);
                Ok(events)
            }
            MissingMediaFallback::KeepProgram => Ok(vec![fallback_event]),
        }
    }

    fn complete_take(&mut self, _kind: TransitionKind) -> SwitcherEvent {
        let previous = self.program;
        std::mem::swap(&mut self.program, &mut self.preview);
        SwitcherEvent::ProgramChanged {
            previous,
            program: self.program,
        }
    }

    fn require_input(&self, input: InputId) -> Result<(), SwitcherError> {
        if self.inputs.contains(&input) {
            Ok(())
        } else {
            Err(SwitcherError::UnknownInput(input))
        }
    }

    fn require_idle(&self) -> Result<(), SwitcherError> {
        if self.transition.is_none() && self.t_bar.is_none() {
            Ok(())
        } else {
            Err(SwitcherError::TransitionInProgress)
        }
    }
}
