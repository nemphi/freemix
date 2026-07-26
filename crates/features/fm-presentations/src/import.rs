use core::fmt;
use std::collections::BTreeSet;

use crate::{Document, ModelError, SlideId};

/// Input presented to an importer. The contract assigns no meaning to an extension.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImportSource<'a> {
    pub name: &'a str,
    pub bytes: &'a [u8],
}

/// Adapter boundary for open, independently supplied presentation importers.
///
/// `fm-presentations` does not contain or require a proprietary format parser.
pub trait PresentationImporter {
    /// Imports a source into the normalized presentation model.
    ///
    /// # Errors
    ///
    /// Returns [`ImportError`] when the format is unsupported, the source is
    /// malformed, or the importer cannot complete its work.
    fn import(&self, source: ImportSource<'_>) -> Result<ImportOutcome, ImportError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportOutcome {
    pub document: Document,
    pub report: ImportReport,
}

impl ImportOutcome {
    /// Creates an outcome after validating the normalized document.
    ///
    /// # Errors
    ///
    /// Returns [`ImportError::Malformed`] when the document violates model
    /// invariants needed for deterministic navigation.
    pub fn new(document: Document, report: ImportReport) -> Result<Self, ImportError> {
        document
            .validate()
            .map_err(|error| ImportError::Malformed {
                message: error.to_string(),
            })?;
        Ok(Self { document, report })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportReport {
    /// Features preserved at full fidelity, sorted for stable reporting.
    pub supported_features: BTreeSet<PresentationFeature>,
    /// Partial downgrades and complete losses, retained in source order.
    pub losses: Vec<FeatureLoss>,
    /// Out-of-scope product surfaces reported explicitly rather than silently ignored.
    pub unsupported_surfaces: Vec<UnsupportedSurfaceReport>,
    pub legal_scope: LegalScopeReport,
}

impl ImportReport {
    #[must_use]
    pub fn new(
        supported_features: impl IntoIterator<Item = PresentationFeature>,
        losses: Vec<FeatureLoss>,
    ) -> Self {
        Self {
            supported_features: supported_features.into_iter().collect(),
            losses,
            unsupported_surfaces: UnsupportedSurfaceReport::explicit_contract_limits().to_vec(),
            legal_scope: LegalScopeReport::parser_neutral_contract(),
        }
    }

    #[must_use]
    pub fn supports(&self, feature: PresentationFeature) -> bool {
        self.supported_features.contains(&feature)
    }

    pub fn losses_for(&self, feature: PresentationFeature) -> impl Iterator<Item = &FeatureLoss> {
        self.losses
            .iter()
            .filter(move |loss| loss.feature == feature)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PresentationFeature {
    SlideImages,
    PresenterNotes,
    BuildSteps,
    Links,
    TimedAutoAdvance,
    Looping,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeatureLoss {
    pub feature: PresentationFeature,
    pub kind: ImportLossKind,
    pub detail: String,
    /// Empty means that the loss applies to the whole document.
    pub affected_slides: Vec<SlideId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportLossKind {
    Downgraded,
    Dropped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnsupportedSurface {
    DvdPlaybackAndAuthoring,
    InteractiveMenus,
    LegacyProprietaryFormats,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnsupportedSurfaceReport {
    pub surface: UnsupportedSurface,
    pub detail: &'static str,
}

impl UnsupportedSurfaceReport {
    #[must_use]
    pub const fn explicit_contract_limits() -> [Self; 3] {
        [
            Self {
                surface: UnsupportedSurface::DvdPlaybackAndAuthoring,
                detail: "DVD playback and authoring are outside the presentation import contract",
            },
            Self {
                surface: UnsupportedSurface::InteractiveMenus,
                detail: "interactive menu runtimes are not represented as slide navigation",
            },
            Self {
                surface: UnsupportedSurface::LegacyProprietaryFormats,
                detail: "legacy proprietary formats require an independently supplied lawful importer",
            },
        ]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegalScope {
    ParserNeutralDataContract,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegalScopeReport {
    pub scope: LegalScope,
    pub proprietary_parser_included: bool,
    pub detail: &'static str,
}

impl LegalScopeReport {
    #[must_use]
    pub const fn parser_neutral_contract() -> Self {
        Self {
            scope: LegalScope::ParserNeutralDataContract,
            proprietary_parser_included: false,
            detail: "importers are external adapters and are responsible for format rights and compliance",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImportError {
    Malformed { message: String },
    UnsupportedFormat { format: String },
    Importer { message: String },
}

impl From<ModelError> for ImportError {
    fn from(error: ModelError) -> Self {
        Self::Malformed {
            message: error.to_string(),
        }
    }
}

impl fmt::Display for ImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed { message } => write!(formatter, "malformed presentation: {message}"),
            Self::UnsupportedFormat { format } => {
                write!(formatter, "unsupported presentation format: {format}")
            }
            Self::Importer { message } => {
                write!(formatter, "presentation importer failed: {message}")
            }
        }
    }
}

impl std::error::Error for ImportError {}
