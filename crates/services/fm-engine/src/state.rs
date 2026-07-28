use core::fmt;
use std::collections::HashSet;

use fm_switcher::{StingerDescriptor, StingerSlotId, SwitcherEvent, SwitcherState, TBarState};
use fm_types::InputId;

use crate::ShowError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShowState {
    name: String,
    inputs: Vec<InputId>,
    desired_switcher: SwitcherState,
}

impl ShowState {
    /// Creates a durable show and its desired switcher state.
    ///
    /// # Errors
    ///
    /// Returns a [`ShowError`] for an empty name, an empty or duplicate input
    /// set, or unavailable initial switcher selections.
    pub fn new(
        name: impl Into<String>,
        inputs: Vec<InputId>,
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
        let distinct: HashSet<_> = inputs.iter().copied().collect();
        if distinct.len() != inputs.len() {
            return Err(ShowError::DuplicateInput);
        }
        let desired_switcher =
            SwitcherState::new(inputs.clone(), program, preview).map_err(ShowError::Switcher)?;
        Ok(Self {
            name,
            inputs,
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

    #[must_use]
    pub const fn desired_switcher(&self) -> &SwitcherState {
        &self.desired_switcher
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
        let _ = self.desired_switcher.set_fade_to_black(active);
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

    pub(crate) const fn desired_switcher_mut(&mut self) -> &mut SwitcherState {
        &mut self.desired_switcher
    }
}

impl fmt::Display for ShowState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.name.fmt(formatter)
    }
}
