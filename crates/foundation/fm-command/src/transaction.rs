use core::fmt;

/// Maximum number of commands accepted in one transaction.
pub const MAX_TRANSACTION_COMMANDS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionError {
    Empty,
    TooManyCommands { maximum: usize },
}

impl fmt::Display for TransactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("a transaction must contain at least one command"),
            Self::TooManyCommands { maximum } => {
                write!(
                    formatter,
                    "a transaction may contain at most {maximum} commands"
                )
            }
        }
    }
}

impl std::error::Error for TransactionError {}

/// A nonempty, bounded batch of commands accepted under one command envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transaction<C> {
    commands: Vec<C>,
}

impl<C> Transaction<C> {
    /// Creates a transaction from one or more commands.
    ///
    /// # Errors
    ///
    /// Returns [`TransactionError::Empty`] for an empty batch or
    /// [`TransactionError::TooManyCommands`] when the batch exceeds
    /// [`MAX_TRANSACTION_COMMANDS`].
    pub fn new(commands: impl IntoIterator<Item = C>) -> Result<Self, TransactionError> {
        let mut bounded = Vec::new();
        for command in commands {
            if bounded.len() == MAX_TRANSACTION_COMMANDS {
                return Err(TransactionError::TooManyCommands {
                    maximum: MAX_TRANSACTION_COMMANDS,
                });
            }
            bounded.push(command);
        }

        if bounded.is_empty() {
            return Err(TransactionError::Empty);
        }

        Ok(Self { commands: bounded })
    }

    #[must_use]
    pub fn commands(&self) -> &[C] {
        &self.commands
    }

    pub fn iter(&self) -> core::slice::Iter<'_, C> {
        self.commands.iter()
    }

    #[must_use]
    pub fn into_commands(self) -> Vec<C> {
        self.commands
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }
}

impl<C> IntoIterator for Transaction<C> {
    type Item = C;
    type IntoIter = std::vec::IntoIter<C>;

    fn into_iter(self) -> Self::IntoIter {
        self.commands.into_iter()
    }
}

impl<'a, C> IntoIterator for &'a Transaction<C> {
    type Item = &'a C;
    type IntoIter = core::slice::Iter<'a, C>;

    fn into_iter(self) -> Self::IntoIter {
        self.commands.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transactions_are_nonempty_and_bounded() {
        assert_eq!(Transaction::<u8>::new([]), Err(TransactionError::Empty));

        let maximum = Transaction::new(0..MAX_TRANSACTION_COMMANDS).unwrap();
        assert_eq!(maximum.len(), MAX_TRANSACTION_COMMANDS);
        assert!(!maximum.is_empty());

        assert_eq!(
            Transaction::new(0..=MAX_TRANSACTION_COMMANDS),
            Err(TransactionError::TooManyCommands {
                maximum: MAX_TRANSACTION_COMMANDS,
            })
        );
    }
}
