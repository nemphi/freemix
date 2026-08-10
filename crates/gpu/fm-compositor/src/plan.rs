use core::fmt;

use fm_video::{CropRect, Rgba8, Transform};

use crate::AlphaMode;
use crate::scene::{
    Effect, Key, OutputInclusion, OutputTarget, RectMask, SafeAreaGuide, Scene, SourceId,
    SourceLayer, is_premultiplied,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanLayer {
    source: SourceId,
    z: i32,
    transform: Transform,
    crop: Option<CropRect>,
    opacity: u8,
    alpha_mode: AlphaMode,
    mask: Option<RectMask>,
    key: Option<Key>,
    effects: Vec<Effect>,
    overlay_inclusion: Option<OutputInclusion>,
    inset_border_width: u32,
    scene_index: usize,
}

impl PlanLayer {
    #[must_use]
    pub const fn source(&self) -> SourceId {
        self.source
    }

    #[must_use]
    pub const fn z(&self) -> i32 {
        self.z
    }

    #[must_use]
    pub const fn transform(&self) -> Transform {
        self.transform
    }

    #[must_use]
    pub const fn crop(&self) -> Option<CropRect> {
        self.crop
    }

    #[must_use]
    pub const fn opacity(&self) -> u8 {
        self.opacity
    }

    #[must_use]
    pub const fn alpha_mode(&self) -> AlphaMode {
        self.alpha_mode
    }

    #[must_use]
    pub const fn mask(&self) -> Option<RectMask> {
        self.mask
    }

    #[must_use]
    pub const fn key(&self) -> Option<Key> {
        self.key
    }

    #[must_use]
    pub fn effects(&self) -> &[Effect] {
        &self.effects
    }

    #[must_use]
    pub const fn overlay_inclusion(&self) -> Option<OutputInclusion> {
        self.overlay_inclusion
    }

    #[must_use]
    pub const fn inset_border_width(&self) -> u32 {
        self.inset_border_width
    }

    #[must_use]
    pub const fn scene_index(&self) -> usize {
        self.scene_index
    }
}

/// Immutable, validated work description suitable for a CPU or GPU backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompositionPlan {
    width: u32,
    height: u32,
    background: Rgba8,
    target: OutputTarget,
    layers: Vec<PlanLayer>,
    safe_areas: Vec<SafeAreaGuide>,
}

impl CompositionPlan {
    pub const MAX_LAYERS: usize = 64;
    pub const MAX_EFFECTS_PER_LAYER: usize = 16;
    pub const MAX_EFFECT_PARAMETERS: usize = 32;
    pub const MAX_EFFECT_NAME_BYTES: usize = 64;
    pub const MAX_SAFE_AREAS: usize = 16;

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    #[must_use]
    pub const fn background(&self) -> Rgba8 {
        self.background
    }

    #[must_use]
    pub const fn target(&self) -> OutputTarget {
        self.target
    }

    #[must_use]
    pub fn layers(&self) -> &[PlanLayer] {
        &self.layers
    }

    #[must_use]
    pub fn safe_areas(&self) -> &[SafeAreaGuide] {
        &self.safe_areas
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReportEntry {
    IncludedLayer {
        scene_index: usize,
        plan_index: usize,
    },
    DisabledLayer {
        scene_index: usize,
    },
    OverlayExcluded {
        scene_index: usize,
        target: OutputTarget,
    },
    SafeAreasIncluded {
        count: usize,
    },
    SafeAreasOmitted {
        count: usize,
        target: OutputTarget,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompilationReport {
    entries: Vec<ReportEntry>,
    scene_layers: usize,
    planned_layers: usize,
}

impl CompilationReport {
    #[must_use]
    pub fn entries(&self) -> &[ReportEntry] {
        &self.entries
    }

    #[must_use]
    pub const fn scene_layers(&self) -> usize {
        self.scene_layers
    }

    #[must_use]
    pub const fn planned_layers(&self) -> usize {
        self.planned_layers
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlanError {
    TooManyLayers {
        actual: usize,
        maximum: usize,
    },
    ZeroTransformWidth {
        layer: usize,
    },
    ZeroTransformHeight {
        layer: usize,
    },
    InvalidCrop {
        layer: usize,
    },
    InvalidMask {
        layer: usize,
    },
    TooManyEffects {
        layer: usize,
        actual: usize,
        maximum: usize,
    },
    EmptyEffectName {
        layer: usize,
        effect: usize,
    },
    EffectNameTooLong {
        layer: usize,
        effect: usize,
        actual: usize,
        maximum: usize,
    },
    TooManyEffectParameters {
        layer: usize,
        effect: usize,
        actual: usize,
        maximum: usize,
    },
    TooManySafeAreas {
        actual: usize,
        maximum: usize,
    },
    InvalidSafeArea {
        guide: usize,
    },
    SafeAreaColorNotPremultiplied {
        guide: usize,
    },
}

impl fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyLayers { actual, maximum } => {
                write!(
                    formatter,
                    "composition has {actual} active layers; maximum is {maximum}"
                )
            }
            Self::ZeroTransformWidth { layer } => {
                write!(formatter, "layer {layer} transform width must be nonzero")
            }
            Self::ZeroTransformHeight { layer } => {
                write!(formatter, "layer {layer} transform height must be nonzero")
            }
            Self::InvalidCrop { layer } => write!(formatter, "layer {layer} has an invalid crop"),
            Self::InvalidMask { layer } => write!(formatter, "layer {layer} has an invalid mask"),
            Self::TooManyEffects {
                layer,
                actual,
                maximum,
            } => write!(
                formatter,
                "layer {layer} has {actual} effects; maximum is {maximum}"
            ),
            Self::EmptyEffectName { layer, effect } => {
                write!(formatter, "layer {layer} effect {effect} has an empty name")
            }
            Self::EffectNameTooLong {
                layer,
                effect,
                actual,
                maximum,
            } => write!(
                formatter,
                "layer {layer} effect {effect} name has {actual} bytes; maximum is {maximum}"
            ),
            Self::TooManyEffectParameters {
                layer,
                effect,
                actual,
                maximum,
            } => write!(
                formatter,
                "layer {layer} effect {effect} has {actual} parameters; maximum is {maximum}"
            ),
            Self::TooManySafeAreas { actual, maximum } => write!(
                formatter,
                "scene has {actual} safe-area guides; maximum is {maximum}"
            ),
            Self::InvalidSafeArea { guide } => {
                write!(formatter, "safe-area guide {guide} is invalid")
            }
            Self::SafeAreaColorNotPremultiplied { guide } => write!(
                formatter,
                "safe-area guide {guide} color is not premultiplied"
            ),
        }
    }
}

impl std::error::Error for PlanError {}

/// Compiles a scene into a stable, bounded plan for one output target.
///
/// Disabled layers and overlays excluded from `target` are reported but do not
/// consume plan capacity. Equal z values preserve scene order.
///
/// # Errors
/// Returns a typed error if any retained operation is malformed or exceeds a limit.
pub fn compile_scene(
    scene: &Scene,
    target: OutputTarget,
) -> Result<(CompositionPlan, CompilationReport), PlanError> {
    validate_safe_areas(scene)?;

    let mut retained = Vec::new();
    let mut entries = Vec::new();
    for (scene_index, layer) in scene.layers().iter().enumerate() {
        if !layer.enabled() {
            entries.push(ReportEntry::DisabledLayer { scene_index });
            continue;
        }
        if layer
            .overlay_inclusion()
            .is_some_and(|inclusion| !inclusion.includes(target))
        {
            entries.push(ReportEntry::OverlayExcluded {
                scene_index,
                target,
            });
            continue;
        }
        validate_layer(scene_index, layer)?;
        retained.push((scene_index, layer));
    }
    if retained.len() > CompositionPlan::MAX_LAYERS {
        return Err(PlanError::TooManyLayers {
            actual: retained.len(),
            maximum: CompositionPlan::MAX_LAYERS,
        });
    }

    retained.sort_by_key(|(scene_index, layer)| (layer.z(), *scene_index));
    let layers = retained
        .into_iter()
        .enumerate()
        .map(|(plan_index, (scene_index, layer))| {
            entries.push(ReportEntry::IncludedLayer {
                scene_index,
                plan_index,
            });
            PlanLayer {
                source: layer.source(),
                z: layer.z(),
                transform: layer.transform(),
                crop: layer.crop(),
                opacity: layer.opacity(),
                alpha_mode: layer.alpha_mode(),
                mask: layer.mask(),
                key: layer.key(),
                effects: layer.effects().to_vec(),
                overlay_inclusion: layer.overlay_inclusion(),
                inset_border_width: layer.inset_border_width(),
                scene_index,
            }
        })
        .collect::<Vec<_>>();

    let safe_areas = if target == OutputTarget::Operator {
        entries.push(ReportEntry::SafeAreasIncluded {
            count: scene.safe_areas().len(),
        });
        scene.safe_areas().to_vec()
    } else {
        entries.push(ReportEntry::SafeAreasOmitted {
            count: scene.safe_areas().len(),
            target,
        });
        Vec::new()
    };
    let report = CompilationReport {
        entries,
        scene_layers: scene.layers().len(),
        planned_layers: layers.len(),
    };
    Ok((
        CompositionPlan {
            width: scene.width(),
            height: scene.height(),
            background: scene.background(),
            target,
            layers,
            safe_areas,
        },
        report,
    ))
}

fn validate_layer(index: usize, layer: &SourceLayer) -> Result<(), PlanError> {
    if layer.transform().scale_width == 0 {
        return Err(PlanError::ZeroTransformWidth { layer: index });
    }
    if layer.transform().scale_height == 0 {
        return Err(PlanError::ZeroTransformHeight { layer: index });
    }
    if layer.crop().is_some_and(|crop| {
        crop.width == 0
            || crop.height == 0
            || crop.x.checked_add(crop.width).is_none()
            || crop.y.checked_add(crop.height).is_none()
    }) {
        return Err(PlanError::InvalidCrop { layer: index });
    }
    if layer.mask().is_some_and(|mask| {
        mask.width == 0
            || mask.height == 0
            || mask.x.checked_add(mask.width).is_none()
            || mask.y.checked_add(mask.height).is_none()
    }) {
        return Err(PlanError::InvalidMask { layer: index });
    }
    if layer.effects().len() > CompositionPlan::MAX_EFFECTS_PER_LAYER {
        return Err(PlanError::TooManyEffects {
            layer: index,
            actual: layer.effects().len(),
            maximum: CompositionPlan::MAX_EFFECTS_PER_LAYER,
        });
    }
    for (effect_index, effect) in layer.effects().iter().enumerate() {
        if effect.name().is_empty() {
            return Err(PlanError::EmptyEffectName {
                layer: index,
                effect: effect_index,
            });
        }
        if effect.name().len() > CompositionPlan::MAX_EFFECT_NAME_BYTES {
            return Err(PlanError::EffectNameTooLong {
                layer: index,
                effect: effect_index,
                actual: effect.name().len(),
                maximum: CompositionPlan::MAX_EFFECT_NAME_BYTES,
            });
        }
        if effect.parameters().len() > CompositionPlan::MAX_EFFECT_PARAMETERS {
            return Err(PlanError::TooManyEffectParameters {
                layer: index,
                effect: effect_index,
                actual: effect.parameters().len(),
                maximum: CompositionPlan::MAX_EFFECT_PARAMETERS,
            });
        }
    }
    Ok(())
}

fn validate_safe_areas(scene: &Scene) -> Result<(), PlanError> {
    if scene.safe_areas().len() > CompositionPlan::MAX_SAFE_AREAS {
        return Err(PlanError::TooManySafeAreas {
            actual: scene.safe_areas().len(),
            maximum: CompositionPlan::MAX_SAFE_AREAS,
        });
    }
    for (index, guide) in scene.safe_areas().iter().enumerate() {
        let valid = guide.width > 0
            && guide.height > 0
            && guide
                .x
                .checked_add(guide.width)
                .is_some_and(|right| right <= scene.width())
            && guide
                .y
                .checked_add(guide.height)
                .is_some_and(|bottom| bottom <= scene.height());
        if !valid {
            return Err(PlanError::InvalidSafeArea { guide: index });
        }
        if !is_premultiplied(guide.color) {
            return Err(PlanError::SafeAreaColorNotPremultiplied { guide: index });
        }
    }
    Ok(())
}
