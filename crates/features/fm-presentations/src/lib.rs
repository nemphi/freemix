//! Parser-neutral presentation import contracts and deterministic navigation.
//!
//! This crate deliberately does not parse presentation formats. Format support is
//! supplied through [`PresentationImporter`], which returns the normalized model
//! and an explicit fidelity report.

mod import;
mod model;
mod navigation;

pub use import::{
    FeatureLoss, ImportError, ImportLossKind, ImportOutcome, ImportReport, ImportSource,
    LegalScope, LegalScopeReport, PresentationFeature, PresentationImporter, UnsupportedSurface,
    UnsupportedSurfaceReport,
};
pub use model::{
    BuildStep, Deck, DeckId, Document, DocumentId, IdError, LinkTarget, ModelError, PresenterNotes,
    Slide, SlideId, SlideImage, SlideLink,
};
pub use navigation::{NavigationError, NavigationEvent, PresentationNavigator};
