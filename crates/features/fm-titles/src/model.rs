use crate::{
    AnimationTrack, ClockSpec, ElementId, FieldId, TemplateId, TickerSpec, TitleId,
    validation::validate_template_structure,
};
use core::fmt;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const TRANSPARENT: Self = Self::new(0, 0, 0, 0);
    pub const BLACK: Self = Self::new(0, 0, 0, 255);

    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HorizontalAlignment {
    #[default]
    Start,
    Center,
    End,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VerticalAlignment {
    #[default]
    Start,
    Center,
    End,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Alignment {
    pub horizontal: HorizontalAlignment,
    pub vertical: VerticalAlignment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FontStyle {
    pub family: String,
    pub size_px: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Style {
    pub fill: Color,
    pub background: Option<Color>,
    pub opacity: u8,
    pub font: Option<FontStyle>,
    pub alignment: Alignment,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            fill: Color::new(255, 255, 255, 255),
            background: None,
            opacity: 255,
            font: None,
            alignment: Alignment::default(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageSource {
    pub asset: String,
}

impl ImageSource {
    #[must_use]
    pub fn new(asset: impl Into<String>) -> Self {
        Self {
            asset: asset.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FieldType {
    Text,
    Number,
    Image,
    Color,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FieldValue {
    Text(String),
    Number(i64),
    Image(ImageSource),
    Color(Color),
}

impl FieldValue {
    #[must_use]
    pub const fn field_type(&self) -> FieldType {
        match self {
            Self::Text(_) => FieldType::Text,
            Self::Number(_) => FieldType::Number,
            Self::Image(_) => FieldType::Image,
            Self::Color(_) => FieldType::Color,
        }
    }

    pub(crate) fn display_text(&self) -> Option<String> {
        match self {
            Self::Text(value) => Some(value.clone()),
            Self::Number(value) => Some(value.to_string()),
            Self::Image(_) | Self::Color(_) => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldDefinition {
    pub id: FieldId,
    pub name: String,
    pub default: FieldValue,
}

impl FieldDefinition {
    #[must_use]
    pub const fn field_type(&self) -> FieldType {
        self.default.field_type()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ElementKind {
    Text { field: FieldId },
    Rectangle,
    ImagePlaceholder { field: FieldId },
    Clock(ClockSpec),
    Ticker(TickerSpec),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Element {
    pub id: ElementId,
    pub name: String,
    pub bounds: Bounds,
    pub z_index: i32,
    pub visible: bool,
    pub style: Style,
    /// Optional color field overriding `style.fill`.
    pub color_field: Option<FieldId>,
    pub kind: ElementKind,
    pub animations: Vec<AnimationTrack>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TitleTemplate {
    pub id: TemplateId,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub background: Color,
    pub fields: Vec<FieldDefinition>,
    pub elements: Vec<Element>,
}

impl TitleTemplate {
    /// Creates a live scene while preserving template, element, and field IDs.
    ///
    /// # Errors
    ///
    /// Returns all structural validation errors. Missing external assets are
    /// reported separately by [`crate::validate_template`] and rendering.
    pub fn instantiate(&self, id: TitleId) -> Result<TitleScene, InstantiationError> {
        let errors = validate_template_structure(self);
        if !errors.is_empty() {
            return Err(InstantiationError { errors });
        }
        let fields = self
            .fields
            .iter()
            .map(|field| (field.id, field.default.clone()))
            .collect();
        let field_types = self
            .fields
            .iter()
            .map(|field| (field.id, field.field_type()))
            .collect();
        Ok(TitleScene {
            id,
            template_id: self.id,
            width: self.width,
            height: self.height,
            background: self.background,
            revision: 0,
            fields,
            field_types,
            elements: self.elements.clone(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstantiationError {
    pub errors: Vec<crate::ValidationError>,
}

impl fmt::Display for InstantiationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "template has {} validation error(s)",
            self.errors.len()
        )
    }
}

impl std::error::Error for InstantiationError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TitleScene {
    id: TitleId,
    template_id: TemplateId,
    width: u32,
    height: u32,
    background: Color,
    revision: u64,
    fields: BTreeMap<FieldId, FieldValue>,
    field_types: BTreeMap<FieldId, FieldType>,
    elements: Vec<Element>,
}

impl TitleScene {
    #[must_use]
    pub const fn id(&self) -> TitleId {
        self.id
    }

    #[must_use]
    pub const fn template_id(&self) -> TemplateId {
        self.template_id
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    #[must_use]
    pub const fn background(&self) -> Color {
        self.background
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub fn field_value(&self, field: FieldId) -> Option<&FieldValue> {
        self.fields.get(&field)
    }

    #[must_use]
    pub fn fields(&self) -> &BTreeMap<FieldId, FieldValue> {
        &self.fields
    }

    #[must_use]
    pub fn elements(&self) -> &[Element] {
        &self.elements
    }

    /// Atomically applies typed field values if `expected_revision` is current.
    /// An empty update is a no-op and does not advance the revision.
    ///
    /// # Errors
    ///
    /// Returns a conflict, unknown/duplicate field, type mismatch, or revision
    /// exhaustion without changing any field.
    pub fn update_fields(
        &mut self,
        expected_revision: u64,
        updates: &[(FieldId, FieldValue)],
    ) -> Result<u64, UpdateError> {
        if expected_revision != self.revision {
            return Err(UpdateError::RevisionConflict {
                expected: expected_revision,
                actual: self.revision,
            });
        }
        if updates.is_empty() {
            return Ok(self.revision);
        }

        let mut seen = BTreeSet::new();
        for (field, value) in updates {
            if !seen.insert(*field) {
                return Err(UpdateError::DuplicateField(*field));
            }
            let Some(expected) = self.field_types.get(field).copied() else {
                return Err(UpdateError::UnknownField(*field));
            };
            let actual = value.field_type();
            if actual != expected {
                return Err(UpdateError::TypeMismatch {
                    field: *field,
                    expected,
                    actual,
                });
            }
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or(UpdateError::RevisionExhausted)?;
        for (field, value) in updates {
            self.fields.insert(*field, value.clone());
        }
        self.revision = revision;
        Ok(revision)
    }

    /// Applies one optimistic live-field update.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::update_fields`].
    pub fn update_field(
        &mut self,
        expected_revision: u64,
        field: FieldId,
        value: FieldValue,
    ) -> Result<u64, UpdateError> {
        self.update_fields(expected_revision, &[(field, value)])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateError {
    RevisionConflict {
        expected: u64,
        actual: u64,
    },
    UnknownField(FieldId),
    DuplicateField(FieldId),
    TypeMismatch {
        field: FieldId,
        expected: FieldType,
        actual: FieldType,
    },
    RevisionExhausted,
}

impl fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RevisionConflict { expected, actual } => {
                write!(
                    formatter,
                    "expected revision {expected}, current revision is {actual}"
                )
            }
            Self::UnknownField(field) => write!(formatter, "unknown field {field}"),
            Self::DuplicateField(field) => write!(formatter, "field {field} is updated twice"),
            Self::TypeMismatch {
                field,
                expected,
                actual,
            } => write!(
                formatter,
                "field {field} expects {expected:?}, received {actual:?}"
            ),
            Self::RevisionExhausted => formatter.write_str("title revision is exhausted"),
        }
    }
}

impl std::error::Error for UpdateError {}
