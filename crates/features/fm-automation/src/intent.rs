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
/// transient value for each continuous stream of each coalescing scope.
///
/// Last-intent-wins is only safe within one stream of one scope. Two bindings
/// driving a stream that happens to share a name are two independent commands,
/// and collapsing them would destroy one of them without a trace, so the scope
/// -- the binding that produced the intent -- is part of the coalescing key.
/// [`Self::push`] returns whatever it coalesced away so a caller can always
/// report the drop instead of letting a command vanish.
#[derive(Clone, Debug)]
pub struct IntentBuffer<C, S = ()> {
    discrete: VecDeque<CommandIntent<C>>,
    continuous: BTreeMap<(S, String), C>,
}

impl<C, S> Default for IntentBuffer<C, S> {
    fn default() -> Self {
        Self {
            discrete: VecDeque::new(),
            continuous: BTreeMap::new(),
        }
    }
}

impl<C, S: Ord> IntentBuffer<C, S> {
    /// Buffers one intent within `scope`.
    ///
    /// Returns the command this intent superseded: a continuous intent replaces
    /// the pending value for its stream in its scope, and a commit discards it.
    pub fn push(&mut self, scope: S, intent: CommandIntent<C>) -> Option<C> {
        match intent {
            CommandIntent::Discrete(_) => {
                self.discrete.push_back(intent);
                None
            }
            CommandIntent::Continuous { stream, command } => {
                self.continuous.insert((scope, stream), command)
            }
            CommandIntent::CommitContinuous { stream, command } => {
                let key = (scope, stream);
                let superseded = self.continuous.remove(&key);
                let (_, stream) = key;
                self.discrete
                    .push_back(CommandIntent::CommitContinuous { stream, command });
                superseded
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

    /// Drains the pending continuous value of every stream in scope then stream
    /// order. The scope is the caller's own key and is not returned; a caller
    /// that needs it back carries it in the command.
    pub fn drain_continuous(&mut self) -> Vec<CommandIntent<C>> {
        std::mem::take(&mut self.continuous)
            .into_iter()
            .map(|((_, stream), command)| CommandIntent::Continuous { stream, command })
            .collect()
    }
}
