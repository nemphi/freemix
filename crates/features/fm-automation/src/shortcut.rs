use core::fmt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShortcutScope {
    Global,
    Local(String),
}

impl ShortcutScope {
    fn overlaps(&self, other: &Self) -> bool {
        matches!(self, Self::Global)
            || matches!(other, Self::Global)
            || matches!((self, other), (Self::Local(left), Self::Local(right)) if left == right)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Modifiers(u8);

impl Modifiers {
    pub const CONTROL: Self = Self(1 << 0);
    pub const ALT: Self = Self(1 << 1);
    pub const SHIFT: Self = Self(1 << 2);
    pub const META: Self = Self(1 << 3);

    #[must_use]
    pub const fn contains(self, modifier: Self) -> bool {
        self.0 & modifier.0 == modifier.0
    }
}

impl core::ops::BitOr for Modifiers {
    type Output = Self;

    fn bitor(self, right: Self) -> Self::Output {
        Self(self.0 | right.0)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct KeyStroke {
    pub key: String,
    pub modifiers: Modifiers,
}

impl KeyStroke {
    #[must_use]
    pub fn new(key: impl Into<String>, modifiers: Modifiers) -> Self {
        Self {
            key: key.into(),
            modifiers,
        }
    }
}

pub const MAX_CHORD_STROKES: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChordError {
    Empty,
    TooLong { maximum: usize },
}

impl fmt::Display for ChordError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("a shortcut chord must contain a key stroke"),
            Self::TooLong { maximum } => {
                write!(
                    formatter,
                    "a shortcut chord may contain at most {maximum} strokes"
                )
            }
        }
    }
}

impl std::error::Error for ChordError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Chord(Vec<KeyStroke>);

impl Chord {
    /// Creates a nonempty, bounded key chord.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty chord or more than four strokes.
    pub fn new(strokes: impl IntoIterator<Item = KeyStroke>) -> Result<Self, ChordError> {
        let strokes: Vec<_> = strokes.into_iter().collect();
        if strokes.is_empty() {
            return Err(ChordError::Empty);
        }
        if strokes.len() > MAX_CHORD_STROKES {
            return Err(ChordError::TooLong {
                maximum: MAX_CHORD_STROKES,
            });
        }
        Ok(Self(strokes))
    }

    #[must_use]
    pub fn strokes(&self) -> &[KeyStroke] {
        &self.0
    }

    fn conflict(&self, other: &Self) -> Option<ConflictKind> {
        if self.0 == other.0 {
            Some(ConflictKind::Exact)
        } else if self.0.starts_with(&other.0) || other.0.starts_with(&self.0) {
            Some(ConflictKind::Prefix)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Shortcut<C> {
    pub id: String,
    pub scope: ShortcutScope,
    pub chord: Chord,
    pub intent: CommandIntent<C>,
}

use crate::CommandIntent;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictKind {
    Exact,
    Prefix,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShortcutConflict {
    pub existing_id: String,
    pub incoming_id: String,
    pub kind: ConflictKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShortcutError {
    DuplicateId(String),
    Conflict(Vec<ShortcutConflict>),
}

impl fmt::Display for ShortcutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateId(id) => write!(formatter, "shortcut {id} already exists"),
            Self::Conflict(conflicts) => {
                write!(
                    formatter,
                    "shortcut conflicts with {} binding(s)",
                    conflicts.len()
                )
            }
        }
    }
}

impl std::error::Error for ShortcutError {}

#[derive(Clone, Debug)]
pub struct ShortcutRegistry<C> {
    shortcuts: Vec<Shortcut<C>>,
}

impl<C> Default for ShortcutRegistry<C> {
    fn default() -> Self {
        Self {
            shortcuts: Vec::new(),
        }
    }
}

impl<C> ShortcutRegistry<C> {
    #[must_use]
    pub fn shortcuts(&self) -> &[Shortcut<C>] {
        &self.shortcuts
    }

    #[must_use]
    pub fn conflicts(&self, incoming: &Shortcut<C>) -> Vec<ShortcutConflict> {
        self.shortcuts
            .iter()
            .filter(|existing| existing.scope.overlaps(&incoming.scope))
            .filter_map(|existing| {
                existing
                    .chord
                    .conflict(&incoming.chord)
                    .map(|kind| ShortcutConflict {
                        existing_id: existing.id.clone(),
                        incoming_id: incoming.id.clone(),
                        kind,
                    })
            })
            .collect()
    }

    /// Adds a shortcut if its identifier and active chord are unambiguous.
    ///
    /// # Errors
    ///
    /// Returns duplicate identifier or chord conflict details.
    pub fn insert(&mut self, shortcut: Shortcut<C>) -> Result<(), ShortcutError> {
        if self.shortcuts.iter().any(|entry| entry.id == shortcut.id) {
            return Err(ShortcutError::DuplicateId(shortcut.id));
        }
        let conflicts = self.conflicts(&shortcut);
        if !conflicts.is_empty() {
            return Err(ShortcutError::Conflict(conflicts));
        }
        self.shortcuts.push(shortcut);
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> Option<Shortcut<C>> {
        let index = self.shortcuts.iter().position(|entry| entry.id == id)?;
        Some(self.shortcuts.remove(index))
    }

    #[must_use]
    pub fn resolve(&self, scope: Option<&str>, chord: &Chord) -> Option<&CommandIntent<C>> {
        scope
            .and_then(|scope| {
                self.shortcuts.iter().find(|entry| {
                    entry.chord == *chord
                        && matches!(&entry.scope, ShortcutScope::Local(local) if local == scope)
                })
            })
            .or_else(|| {
                self.shortcuts.iter().find(|entry| {
                    entry.chord == *chord && matches!(entry.scope, ShortcutScope::Global)
                })
            })
            .map(|entry| &entry.intent)
    }
}
