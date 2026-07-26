use core::{fmt, time::Duration};

use crate::{Deck, DeckId, Document, ModelError, PresenterNotes, Slide, SlideId, SlideLink};

#[derive(Debug)]
pub struct PresentationNavigator<'a> {
    document: &'a Document,
    deck_index: usize,
    slide_index: usize,
    revealed_builds: usize,
    ended: bool,
    elapsed: Duration,
}

impl<'a> PresentationNavigator<'a> {
    /// Starts navigation at the first slide of `deck`.
    ///
    /// # Errors
    ///
    /// Returns [`NavigationError::InvalidDocument`] for an invalid normalized
    /// model or [`NavigationError::UnknownDeck`] when `deck` does not exist.
    pub fn new(document: &'a Document, deck: DeckId) -> Result<Self, NavigationError> {
        document
            .validate()
            .map_err(NavigationError::InvalidDocument)?;
        let deck_index = document
            .decks
            .iter()
            .position(|candidate| candidate.id == deck)
            .ok_or(NavigationError::UnknownDeck(deck))?;
        Ok(Self {
            document,
            deck_index,
            slide_index: 0,
            revealed_builds: 0,
            ended: false,
            elapsed: Duration::ZERO,
        })
    }

    #[must_use]
    pub fn deck(&self) -> &Deck {
        &self.document.decks[self.deck_index]
    }

    #[must_use]
    pub fn current_slide(&self) -> &Slide {
        &self.deck().slides[self.slide_index]
    }

    #[must_use]
    pub fn revealed_build_count(&self) -> usize {
        self.revealed_builds
    }

    #[must_use]
    pub fn visible_build_steps(&self) -> &[crate::BuildStep] {
        &self.current_slide().build_steps[..self.revealed_builds]
    }

    pub fn visible_links(&self) -> impl Iterator<Item = &SlideLink> {
        self.current_slide()
            .links
            .iter()
            .filter(|link| link.available_after_build <= self.revealed_builds)
    }

    #[must_use]
    pub fn presenter_notes(&self) -> Option<&PresenterNotes> {
        self.current_slide().presenter_notes.as_ref()
    }

    #[must_use]
    pub const fn is_ended(&self) -> bool {
        self.ended
    }

    /// Reveals the next build before moving to the next slide.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> NavigationEvent {
        self.elapsed = Duration::ZERO;
        if self.ended {
            return NavigationEvent::Ended;
        }
        if self.revealed_builds < self.current_slide().build_steps.len() {
            self.revealed_builds += 1;
            return NavigationEvent::BuildAdvanced {
                slide: self.current_slide().id,
                revealed: self.revealed_builds,
            };
        }
        self.advance_slide()
    }

    /// Reverses one build, then moves to the previous fully-built slide.
    pub fn previous(&mut self) -> NavigationEvent {
        self.elapsed = Duration::ZERO;
        if self.ended {
            self.ended = false;
            self.revealed_builds = self.current_slide().build_steps.len();
            return NavigationEvent::EndLeft {
                slide: self.current_slide().id,
            };
        }
        if self.revealed_builds > 0 {
            self.revealed_builds -= 1;
            return NavigationEvent::BuildReversed {
                slide: self.current_slide().id,
                revealed: self.revealed_builds,
            };
        }
        if self.slide_index > 0 {
            self.slide_index -= 1;
            self.revealed_builds = self.current_slide().build_steps.len();
            return NavigationEvent::SlideChanged {
                slide: self.current_slide().id,
            };
        }
        if self.deck().looping {
            self.slide_index = self.deck().slides.len() - 1;
            self.revealed_builds = self.current_slide().build_steps.len();
            return NavigationEvent::Looped {
                slide: self.current_slide().id,
            };
        }
        NavigationEvent::AtStart
    }

    /// Jumps to a slide in the active deck with no builds revealed.
    ///
    /// # Errors
    ///
    /// Returns [`NavigationError::SlideNotInDeck`] when the target is not part
    /// of the active deck.
    pub fn go_to(&mut self, slide: SlideId) -> Result<NavigationEvent, NavigationError> {
        let slide_index = self
            .deck()
            .slides
            .iter()
            .position(|candidate| candidate.id == slide)
            .ok_or(NavigationError::SlideNotInDeck {
                deck: self.deck().id,
                slide,
            })?;
        self.slide_index = slide_index;
        self.revealed_builds = 0;
        self.ended = false;
        self.elapsed = Duration::ZERO;
        Ok(NavigationEvent::SlideChanged { slide })
    }

    /// Selects a deck and starts at its first slide.
    ///
    /// # Errors
    ///
    /// Returns [`NavigationError::UnknownDeck`] when the document does not
    /// contain `deck`.
    pub fn select_deck(&mut self, deck: DeckId) -> Result<NavigationEvent, NavigationError> {
        let deck_index = self
            .document
            .decks
            .iter()
            .position(|candidate| candidate.id == deck)
            .ok_or(NavigationError::UnknownDeck(deck))?;
        self.deck_index = deck_index;
        self.slide_index = 0;
        self.revealed_builds = 0;
        self.ended = false;
        self.elapsed = Duration::ZERO;
        Ok(NavigationEvent::SlideChanged {
            slide: self.current_slide().id,
        })
    }

    /// Applies elapsed time and returns every resulting slide boundary in order.
    ///
    /// Auto-advance moves directly to the next slide; build steps are manual reveals.
    pub fn advance_time(&mut self, mut elapsed: Duration) -> Vec<NavigationEvent> {
        let mut events = Vec::new();
        while !self.ended {
            let Some(after) = self.current_slide().auto_advance else {
                self.elapsed = Duration::ZERO;
                break;
            };
            let remaining = after.saturating_sub(self.elapsed);
            if elapsed < remaining {
                self.elapsed += elapsed;
                break;
            }
            elapsed -= remaining;
            self.elapsed = Duration::ZERO;
            events.push(self.advance_slide());
            if elapsed.is_zero() {
                break;
            }
        }
        events
    }

    fn advance_slide(&mut self) -> NavigationEvent {
        self.revealed_builds = 0;
        if self.slide_index + 1 < self.deck().slides.len() {
            self.slide_index += 1;
            return NavigationEvent::SlideChanged {
                slide: self.current_slide().id,
            };
        }
        if self.deck().looping {
            self.slide_index = 0;
            return NavigationEvent::Looped {
                slide: self.current_slide().id,
            };
        }
        self.ended = true;
        NavigationEvent::Ended
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NavigationEvent {
    BuildAdvanced { slide: SlideId, revealed: usize },
    BuildReversed { slide: SlideId, revealed: usize },
    SlideChanged { slide: SlideId },
    Looped { slide: SlideId },
    Ended,
    EndLeft { slide: SlideId },
    AtStart,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NavigationError {
    InvalidDocument(ModelError),
    UnknownDeck(DeckId),
    SlideNotInDeck { deck: DeckId, slide: SlideId },
}

impl fmt::Display for NavigationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDocument(error) => write!(formatter, "invalid presentation: {error}"),
            Self::UnknownDeck(deck) => write!(formatter, "unknown deck {deck}"),
            Self::SlideNotInDeck { deck, slide } => {
                write!(formatter, "slide {slide} is not in deck {deck}")
            }
        }
    }
}

impl std::error::Error for NavigationError {}
