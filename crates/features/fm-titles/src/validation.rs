use crate::{
    AnimatedProperty, Element, ElementId, ElementKind, FieldId, FieldType, FieldValue, FontError,
    FontFace, TitleScene, TitleTemplate,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Assets a scene may reference: parsed font faces keyed by family name, and
/// image asset names.
///
/// Fonts are owned bytes the caller loaded from a project asset. There is no
/// system font discovery and no fallback chain: a family that is not in the
/// catalog simply does not render, and is reported as missing.
#[derive(Clone, Debug, Default)]
pub struct AssetCatalog {
    fonts: BTreeMap<String, Arc<FontFace>>,
    images: BTreeSet<String>,
}

impl AssetCatalog {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            fonts: BTreeMap::new(),
            images: BTreeSet::new(),
        }
    }

    /// Registers an already parsed face under a family name.
    #[must_use]
    pub fn with_font(mut self, family: impl Into<String>, face: FontFace) -> Self {
        self.fonts.insert(family.into(), Arc::new(face));
        self
    }

    /// Parses owned font bytes and registers them under a family name.
    ///
    /// # Errors
    ///
    /// Returns [`FontError`] if the bytes are empty, oversized, unparsable, or
    /// metrically unusable.
    pub fn with_font_bytes(
        self,
        family: impl Into<String>,
        bytes: Vec<u8>,
    ) -> Result<Self, FontError> {
        let face = FontFace::from_bytes(bytes)?;
        Ok(self.with_font(family, face))
    }

    #[must_use]
    pub fn with_image(mut self, asset: impl Into<String>) -> Self {
        self.images.insert(asset.into());
        self
    }

    #[must_use]
    pub fn has_font(&self, family: &str) -> bool {
        self.fonts.contains_key(family)
    }

    #[must_use]
    pub fn font(&self, family: &str) -> Option<&FontFace> {
        self.fonts.get(family).map(AsRef::as_ref)
    }

    #[must_use]
    pub fn has_image(&self, asset: &str) -> bool {
        self.images.contains(asset)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValidationError {
    ZeroCanvas,
    EmptyTemplateName,
    EmptyFieldName(FieldId),
    EmptyElementName(ElementId),
    DuplicateFieldId(FieldId),
    DuplicateElementId(ElementId),
    ZeroElementBounds(ElementId),
    ZeroFontSize(ElementId),
    /// A declared font family is blank and can never resolve to a face.
    EmptyFontFamily(ElementId),
    /// A text, clock, or ticker element carries no font style, so there is no
    /// face to rasterize with.
    MissingFontStyle(ElementId),
    UnknownField {
        element: ElementId,
        field: FieldId,
    },
    IncompatibleField {
        element: ElementId,
        field: FieldId,
        actual: FieldType,
        required: &'static str,
    },
    EmptyImageAsset(FieldId),
    EmptyAnimation(ElementId),
    NonIncreasingKeyframes(ElementId),
    DuplicateAnimationProperty {
        element: ElementId,
        property: AnimatedProperty,
    },
    InvalidAnimationValue {
        element: ElementId,
        property: AnimatedProperty,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissingFont {
    pub element: ElementId,
    pub family: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissingImage {
    pub field: FieldId,
    pub asset: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ValidationReport {
    pub errors: Vec<ValidationError>,
    pub missing_fonts: Vec<MissingFont>,
    pub missing_images: Vec<MissingImage>,
}

impl ValidationReport {
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }
}

#[must_use]
pub fn validate_template(template: &TitleTemplate, assets: &AssetCatalog) -> ValidationReport {
    let errors = validate_template_structure(template);
    let values: BTreeMap<_, _> = template
        .fields
        .iter()
        .map(|field| (field.id, &field.default))
        .collect();
    let (missing_fonts, missing_images) = missing_assets(&template.elements, &values, assets);
    ValidationReport {
        errors,
        missing_fonts,
        missing_images,
    }
}

#[must_use]
pub fn validate_scene(scene: &TitleScene, assets: &AssetCatalog) -> ValidationReport {
    let values = scene
        .fields()
        .iter()
        .map(|(id, value)| (*id, value))
        .collect();
    let (missing_fonts, missing_images) = missing_assets(scene.elements(), &values, assets);
    ValidationReport {
        errors: Vec::new(),
        missing_fonts,
        missing_images,
    }
}

pub(crate) fn validate_template_structure(template: &TitleTemplate) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    if template.width == 0 || template.height == 0 {
        errors.push(ValidationError::ZeroCanvas);
    }
    if template.name.trim().is_empty() {
        errors.push(ValidationError::EmptyTemplateName);
    }

    let mut fields = BTreeMap::new();
    for field in &template.fields {
        if field.name.trim().is_empty() {
            errors.push(ValidationError::EmptyFieldName(field.id));
        }
        if fields.insert(field.id, field.field_type()).is_some() {
            errors.push(ValidationError::DuplicateFieldId(field.id));
        }
        if matches!(&field.default, FieldValue::Image(image) if image.asset.trim().is_empty()) {
            errors.push(ValidationError::EmptyImageAsset(field.id));
        }
    }

    let mut elements = BTreeSet::new();
    for element in &template.elements {
        if !elements.insert(element.id) {
            errors.push(ValidationError::DuplicateElementId(element.id));
        }
        validate_element(element, &fields, &mut errors);
    }
    errors
}

fn validate_element(
    element: &Element,
    fields: &BTreeMap<FieldId, FieldType>,
    errors: &mut Vec<ValidationError>,
) {
    if element.name.trim().is_empty() {
        errors.push(ValidationError::EmptyElementName(element.id));
    }
    if element.bounds.width == 0 || element.bounds.height == 0 {
        errors.push(ValidationError::ZeroElementBounds(element.id));
    }
    let needs_font = matches!(
        element.kind,
        ElementKind::Text { .. } | ElementKind::Clock(_) | ElementKind::Ticker(_)
    );
    match element.style.font.as_ref() {
        Some(font) => {
            if font.size_px == 0 {
                errors.push(ValidationError::ZeroFontSize(element.id));
            }
            if font.family.trim().is_empty() {
                errors.push(ValidationError::EmptyFontFamily(element.id));
            }
        }
        None if needs_font => errors.push(ValidationError::MissingFontStyle(element.id)),
        None => {}
    }

    match element.kind {
        ElementKind::Text { field } => require_field(
            errors,
            fields,
            element.id,
            field,
            &[FieldType::Text, FieldType::Number],
            "text or number",
        ),
        ElementKind::ImagePlaceholder { field } => require_field(
            errors,
            fields,
            element.id,
            field,
            &[FieldType::Image],
            "image",
        ),
        ElementKind::Ticker(spec) => require_field(
            errors,
            fields,
            element.id,
            spec.field,
            &[FieldType::Text, FieldType::Number],
            "text or number",
        ),
        ElementKind::Rectangle | ElementKind::Clock(_) => {}
    }
    if let Some(field) = element.color_field {
        require_field(
            errors,
            fields,
            element.id,
            field,
            &[FieldType::Color],
            "color",
        );
    }

    let mut properties = Vec::new();
    for track in &element.animations {
        if properties.contains(&track.property) {
            errors.push(ValidationError::DuplicateAnimationProperty {
                element: element.id,
                property: track.property,
            });
        }
        properties.push(track.property);
        if track.keyframes.is_empty() {
            errors.push(ValidationError::EmptyAnimation(element.id));
        }
        if track
            .keyframes
            .windows(2)
            .any(|pair| pair[0].at_ms >= pair[1].at_ms)
        {
            errors.push(ValidationError::NonIncreasingKeyframes(element.id));
        }
        if track.keyframes.iter().any(|keyframe| {
            matches!(
                track.property,
                AnimatedProperty::Width | AnimatedProperty::Height
            ) && !(0..=i64::from(u32::MAX)).contains(&keyframe.value)
                || track.property == AnimatedProperty::Opacity
                    && !(0..=255).contains(&keyframe.value)
        }) {
            errors.push(ValidationError::InvalidAnimationValue {
                element: element.id,
                property: track.property,
            });
        }
    }
}

fn require_field(
    errors: &mut Vec<ValidationError>,
    fields: &BTreeMap<FieldId, FieldType>,
    element: ElementId,
    field: FieldId,
    accepted: &[FieldType],
    required: &'static str,
) {
    match fields.get(&field).copied() {
        None => errors.push(ValidationError::UnknownField { element, field }),
        Some(actual) if !accepted.contains(&actual) => {
            errors.push(ValidationError::IncompatibleField {
                element,
                field,
                actual,
                required,
            });
        }
        Some(_) => {}
    }
}

fn missing_assets(
    elements: &[crate::Element],
    values: &BTreeMap<FieldId, &FieldValue>,
    assets: &AssetCatalog,
) -> (Vec<MissingFont>, Vec<MissingImage>) {
    let mut fonts = Vec::new();
    let mut images = Vec::new();
    for element in elements {
        if let Some(font) = &element.style.font
            && !font.family.is_empty()
            && !assets.has_font(&font.family)
        {
            let missing = MissingFont {
                element: element.id,
                family: font.family.clone(),
            };
            if !fonts.contains(&missing) {
                fonts.push(missing);
            }
        }
        if let ElementKind::ImagePlaceholder { field } = element.kind
            && let Some(FieldValue::Image(image)) = values.get(&field)
            && !assets.has_image(&image.asset)
        {
            let missing = MissingImage {
                field,
                asset: image.asset.clone(),
            };
            if !images.contains(&missing) {
                images.push(missing);
            }
        }
    }
    (fonts, images)
}
