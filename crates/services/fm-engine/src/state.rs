use core::fmt;
use std::collections::HashSet;

use fm_switcher::SwitcherState;
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

    pub(crate) const fn desired_switcher_mut(&mut self) -> &mut SwitcherState {
        &mut self.desired_switcher
    }
}

impl fmt::Display for ShowState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.name.fmt(formatter)
    }
}
