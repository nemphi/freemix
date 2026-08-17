use std::collections::{BTreeMap, VecDeque};

/// A discrete command or a last-intent-wins continuous update.
#[derive(Clone, Debug, PartialEq)]
pub enum CommandIntent<C> {
    Discrete(C),
    Continuous {
        stream: String,
        command: C,
    },
    /// The durable final value for a continuous stream.
    CommitContinuous {
        stream: String,
        command: C,
    },
}

impl<C> CommandIntent<C> {
    #[must_use]
    pub fn discrete(command: C) -> Self {
        Self::Discrete(command)
    }

    #[must_use]
    pub fn continuous(stream: impl Into<String>, command: C) -> Self {
        Self::Continuous {
            stream: stream.into(),
            command,
        }
    }

    #[must_use]
    pub fn commit_continuous(stream: impl Into<String>, command: C) -> Self {
        Self::CommitContinuous {
            stream: stream.into(),
            command,
        }
    }

    /// Rewrites the carried command while preserving its delivery class.
    #[must_use]
    pub fn map<D>(self, rewrite: impl FnOnce(C) -> D) -> CommandIntent<D> {
        match self {
            Self::Discrete(command) => CommandIntent::Discrete(rewrite(command)),
            Self::Continuous { stream, command } => CommandIntent::Continuous {
                stream,
                command: rewrite(command),
            },
            Self::CommitContinuous { stream, command } => CommandIntent::CommitContinuous {
                stream,
                command: rewrite(command),
            },
        }
    }

    /// Returns the carried command, discarding its delivery class.
    #[must_use]
    pub fn into_command(self) -> C {
        match self {
            Self::Discrete(command)
            | Self::Continuous { command, .. }
            | Self::CommitContinuous { command, .. } => command,
        }
    }
}

/// Keeps edge-triggered commands separate while retaining only the newest
/// transient value for each continuous stream.
#[derive(Clone, Debug)]
pub struct IntentBuffer<C> {
    discrete: VecDeque<CommandIntent<C>>,
    continuous: BTreeMap<String, C>,
}

impl<C> Default for IntentBuffer<C> {
    fn default() -> Self {
        Self {
            discrete: VecDeque::new(),
            continuous: BTreeMap::new(),
        }
    }
}

impl<C> IntentBuffer<C> {
    pub fn push(&mut self, intent: CommandIntent<C>) {
        match intent {
            CommandIntent::Discrete(_) => self.discrete.push_back(intent),
            CommandIntent::Continuous { stream, command } => {
                self.continuous.insert(stream, command);
            }
            CommandIntent::CommitContinuous { stream, command } => {
                self.continuous.remove(&stream);
                self.discrete
                    .push_back(CommandIntent::CommitContinuous { stream, command });
            }
        }
    }

    #[must_use]
    pub fn discrete_len(&self) -> usize {
        self.discrete.len()
    }

    #[must_use]
    pub fn continuous_len(&self) -> usize {
        self.continuous.len()
    }

    pub fn pop_discrete(&mut self) -> Option<CommandIntent<C>> {
        self.discrete.pop_front()
    }

    pub fn drain_discrete(&mut self) -> Vec<CommandIntent<C>> {
        self.discrete.drain(..).collect()
    }

    pub fn drain_continuous(&mut self) -> Vec<CommandIntent<C>> {
        std::mem::take(&mut self.continuous)
            .into_iter()
            .map(|(stream, command)| CommandIntent::Continuous { stream, command })
            .collect()
    }
}
