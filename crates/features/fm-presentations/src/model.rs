use core::{fmt, num::NonZeroU128, time::Duration};
use std::collections::BTreeSet;

macro_rules! presentation_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(NonZeroU128);

        impl $name {
            #[must_use]
            pub const fn new(value: NonZeroU128) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn get(self) -> NonZeroU128 {
                self.0
            }
        }

        impl From<NonZeroU128> for $name {
            fn from(value: NonZeroU128) -> Self {
                Self::new(value)
            }
        }

        impl TryFrom<u128> for $name {
            type Error = IdError;

            fn try_from(value: u128) -> Result<Self, Self::Error> {
                NonZeroU128::new(value).map(Self::new).ok_or(IdError::Zero)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdError {
    Zero,
}

impl fmt::Display for IdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("presentation IDs must be non-zero")
    }
}

impl std::error::Error for IdError {}

presentation_id!(DocumentId);
presentation_id!(DeckId);
presentation_id!(SlideId);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Document {
    pub id: DocumentId,
    pub title: String,
    pub decks: Vec<Deck>,
}

impl Document {
    /// Validates structural invariants required for deterministic navigation.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError`] for empty decks, duplicate IDs, invalid build
    /// references, missing linked slides, or zero-length auto-advance timers.
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.decks.is_empty() {
            return Err(ModelError::NoDecks);
        }

        let mut deck_ids = BTreeSet::new();
        let mut slide_ids = BTreeSet::new();
        for deck in &self.decks {
            if !deck_ids.insert(deck.id) {
                return Err(ModelError::DuplicateDeckId(deck.id));
            }
            if deck.slides.is_empty() {
                return Err(ModelError::EmptyDeck(deck.id));
            }

            for slide in &deck.slides {
                if !slide_ids.insert(slide.id) {
                    return Err(ModelError::DuplicateSlideId(slide.id));
                }
                if slide.auto_advance == Some(Duration::ZERO) {
                    return Err(ModelError::ZeroAutoAdvance(slide.id));
                }
                for link in &slide.links {
                    if link.available_after_build > slide.build_steps.len() {
                        return Err(ModelError::InvalidLinkBuild {
                            slide: slide.id,
                            available_after_build: link.available_after_build,
                            build_count: slide.build_steps.len(),
                        });
                    }
                }
            }
        }

        for deck in &self.decks {
            for slide in &deck.slides {
                for link in &slide.links {
                    if let LinkTarget::Slide(target) = link.target
                        && !slide_ids.contains(&target)
                    {
                        return Err(ModelError::MissingLinkedSlide {
                            slide: slide.id,
                            target,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn deck(&self, id: DeckId) -> Option<&Deck> {
        self.decks.iter().find(|deck| deck.id == id)
    }

    #[must_use]
    pub fn slide(&self, id: SlideId) -> Option<&Slide> {
        self.decks
            .iter()
            .flat_map(|deck| &deck.slides)
            .find(|slide| slide.id == id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Deck {
    pub id: DeckId,
    pub title: String,
    pub slides: Vec<Slide>,
    /// Wrap from the final slide to the first instead of entering the ended state.
    pub looping: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Slide {
    pub id: SlideId,
    pub image: Option<SlideImage>,
    pub presenter_notes: Option<PresenterNotes>,
    pub build_steps: Vec<BuildStep>,
    pub links: Vec<SlideLink>,
    pub auto_advance: Option<Duration>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlideImage {
    /// An IANA media type such as `image/png`.
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub alternative_text: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PresenterNotes(pub String);

impl PresenterNotes {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildStep {
    /// Importer-provided description of the content revealed by this step.
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlideLink {
    pub label: String,
    pub target: LinkTarget,
    /// Number of completed build steps required before this link is available.
    pub available_after_build: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LinkTarget {
    Uri(String),
    Slide(SlideId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelError {
    NoDecks,
    DuplicateDeckId(DeckId),
    EmptyDeck(DeckId),
    DuplicateSlideId(SlideId),
    ZeroAutoAdvance(SlideId),
    InvalidLinkBuild {
        slide: SlideId,
        available_after_build: usize,
        build_count: usize,
    },
    MissingLinkedSlide {
        slide: SlideId,
        target: SlideId,
    },
}

impl fmt::Display for ModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDecks => formatter.write_str("presentation document has no decks"),
            Self::DuplicateDeckId(id) => write!(formatter, "duplicate deck ID {id}"),
            Self::EmptyDeck(id) => write!(formatter, "deck {id} has no slides"),
            Self::DuplicateSlideId(id) => write!(formatter, "duplicate slide ID {id}"),
            Self::ZeroAutoAdvance(id) => {
                write!(formatter, "slide {id} has a zero auto-advance duration")
            }
            Self::InvalidLinkBuild {
                slide,
                available_after_build,
                build_count,
            } => write!(
                formatter,
                "slide {slide} link requires build {available_after_build}, but only {build_count} builds exist"
            ),
            Self::MissingLinkedSlide { slide, target } => {
                write!(formatter, "slide {slide} links to missing slide {target}")
            }
        }
    }
}

impl std::error::Error for ModelError {}
