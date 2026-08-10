use core::fmt;
use std::collections::{BTreeMap, HashSet};

use fm_switcher::{StingerDescriptor, StingerSlotId, SwitcherEvent, SwitcherState, TBarState};
use fm_types::InputId;

use crate::ShowError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineInputAudioStripState {
    pub gain_millidb: i32,
    pub balance_basis_points: i32,
    pub muted: bool,
    pub soloed: bool,
    pub follow_video: bool,
    pub delay_samples: u32,
}

impl Default for EngineInputAudioStripState {
    fn default() -> Self {
        Self {
            gain_millidb: 0,
            balance_basis_points: 0,
            muted: false,
            soloed: false,
            follow_video: true,
            delay_samples: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShowState {
    name: String,
    inputs: Vec<InputId>,
    input_names: Vec<String>,
    input_audio_strips: BTreeMap<InputId, EngineInputAudioStripState>,
    desired_switcher: SwitcherState,
}

impl ShowState {
    /// Creates a durable show and its desired switcher state.
    ///
    /// # Errors
    ///
    /// Returns a [`ShowError`] for an empty show/input name, an empty or
    /// duplicate input set, or unavailable initial switcher selections.
    pub fn new(
        name: impl Into<String>,
        inputs: Vec<(InputId, String)>,
        program: InputId,
        preview: InputId,
    ) -> Result<Self, ShowError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(ShowError::EmptyName);
        }
        if inputs.is_empty() {
            return Err(ShowError::NoInputs);
        }
        if inputs.iter().any(|(_, name)| name.trim().is_empty()) {
            return Err(ShowError::EmptyInputName);
        }
        let distinct: HashSet<_> = inputs.iter().map(|(input, _)| *input).collect();
        if distinct.len() != inputs.len() {
            return Err(ShowError::DuplicateInput);
        }
        let input_ids = inputs.iter().map(|(input, _)| *input).collect::<Vec<_>>();
        let desired_switcher =
            SwitcherState::new(input_ids.clone(), program, preview).map_err(ShowError::Switcher)?;
        Ok(Self {
            name,
            input_audio_strips: input_ids
                .iter()
                .copied()
                .map(|input| (input, EngineInputAudioStripState::default()))
                .collect(),
            inputs: input_ids,
            input_names: inputs.into_iter().map(|(_, name)| name).collect(),
            desired_switcher,
        })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn inputs(&self) -> &[InputId] {
        &self.inputs
    }

    /// Returns canonical input names in the same order as [`Self::inputs`].
    #[must_use]
    pub fn input_names(&self) -> &[String] {
        &self.input_names
    }

    #[must_use]
    pub fn input_name(&self, input: InputId) -> Option<&str> {
        self.inputs
            .iter()
            .position(|candidate| *candidate == input)
            .map(|index| self.input_names[index].as_str())
    }

    #[must_use]
    pub const fn desired_switcher(&self) -> &SwitcherState {
        &self.desired_switcher
    }

    #[must_use]
    pub fn input_audio_strip(&self, input: InputId) -> Option<EngineInputAudioStripState> {
        self.input_audio_strips.get(&input).copied()
    }

    #[must_use]
    pub fn input_audio_strips(&self) -> &BTreeMap<InputId, EngineInputAudioStripState> {
        &self.input_audio_strips
    }

    /// Sets the exact desired Master strip state for one show input.
    ///
    /// # Errors
    ///
    /// Returns [`ShowError::UnknownInput`] when the input is outside this show.
    pub fn set_input_audio_strip(
        &mut self,
        input: InputId,
        state: EngineInputAudioStripState,
    ) -> Result<(), ShowError> {
        let strip = self
            .input_audio_strips
            .get_mut(&input)
            .ok_or(ShowError::UnknownInput(input))?;
        *strip = state;
        Ok(())
    }

    /// Restores exact desired manual-transition state from a checkpoint.
    ///
    /// # Errors
    ///
    /// Returns [`ShowError::Switcher`] when the checkpoint is inconsistent
    /// with the desired Program/Preview routing.
    pub fn restore_manual_transition(&mut self, state: TBarState) -> Result<(), ShowError> {
        self.desired_switcher
            .restore_t_bar(state)
            .map_err(ShowError::Switcher)
    }

    /// Restores a settled desired Fade-to-Black endpoint from a checkpoint.
    pub fn restore_fade_to_black(&mut self, active: bool) {
        self.desired_switcher.restore_settled_fade_to_black(active);
    }

    /// Configures one durable Stinger slot.
    ///
    /// # Errors
    ///
    /// Returns [`ShowError::Switcher`] when the descriptor references an input
    /// outside this show.
    pub fn configure_stinger(
        &mut self,
        slot: StingerSlotId,
        descriptor: StingerDescriptor,
    ) -> Result<(), ShowError> {
        self.desired_switcher
            .configure_stinger(slot, descriptor)
            .map_err(ShowError::Switcher)
    }

    /// Removes one durable Stinger slot and its readiness state.
    pub fn remove_stinger(&mut self, slot: StingerSlotId) {
        self.desired_switcher.remove_stinger(slot);
    }

    /// Records whether one configured Stinger media input is ready.
    ///
    /// # Errors
    ///
    /// Returns [`ShowError::Switcher`] when the slot is unconfigured.
    pub fn preload_stinger(
        &mut self,
        slot: StingerSlotId,
        media_available: bool,
    ) -> Result<SwitcherEvent, ShowError> {
        self.desired_switcher
            .preload_stinger(slot, media_available)
            .map_err(ShowError::Switcher)
    }

    pub const fn desired_switcher_mut(&mut self) -> &mut SwitcherState {
        &mut self.desired_switcher
    }
}

impl fmt::Display for ShowState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.name.fmt(formatter)
    }
}
