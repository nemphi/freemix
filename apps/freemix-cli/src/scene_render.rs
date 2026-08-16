use core::fmt;
use std::collections::BTreeMap;

use fm_compositor::{
    CompositionPlan, CpuExecutionError, CpuSourceFrame, OutputTarget, PlanError, RectMask,
    Rotation, Scene, SceneError, SourceId, SourceLayer, Transform, compile_scene, execute_cpu,
};
use fm_model::{Input, InputKind, Project, SimulatedVideo, SourceRef};
use fm_types::{InputId, SceneId};
use fm_video::{CropRect, ImageFrame, Rgba8};

#[derive(Debug)]
pub(crate) enum SceneRenderError {
    UnknownScene(SceneId),
    UnknownInput(InputId),
    SceneSource(SceneId),
    UnsupportedInput {
        input: InputId,
    },
    CropBoundsOverflow {
        layer: usize,
    },
    CropOutOfBounds {
        layer: usize,
        width: u32,
        height: u32,
    },
    MaskBoundsOverflow {
        layer: usize,
    },
    MaskOutOfBounds {
        layer: usize,
        width: u32,
        height: u32,
    },
    Scene(SceneError),
    Plan(PlanError),
    Source(fm_sim::RenderError),
    Cpu(CpuExecutionError),
}

impl fmt::Display for SceneRenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownScene(scene) => write!(formatter, "scene {scene} does not exist"),
            Self::UnknownInput(input) => write!(formatter, "input {input} does not exist"),
            Self::SceneSource(scene) => write!(formatter, "scene source {scene} is unsupported"),
            Self::UnsupportedInput { input } => {
                write!(
                    formatter,
                    "input {input} is unsupported for scene rendering"
                )
            }
            Self::CropBoundsOverflow { layer } => {
                write!(formatter, "layer {layer} crop bounds overflow")
            }
            Self::CropOutOfBounds {
                layer,
                width,
                height,
            } => write!(
                formatter,
                "layer {layer} crop exceeds source bounds {width}x{height}"
            ),
            Self::MaskBoundsOverflow { layer } => {
                write!(formatter, "layer {layer} mask bounds overflow")
            }
            Self::MaskOutOfBounds {
                layer,
                width,
                height,
            } => write!(
                formatter,
                "layer {layer} mask exceeds source bounds {width}x{height}"
            ),
            Self::Scene(error) => error.fmt(formatter),
            Self::Plan(error) => error.fmt(formatter),
            Self::Source(error) => error.fmt(formatter),
            Self::Cpu(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for SceneRenderError {}

/// Renders one flat simulated scene at the pipeline output dimensions.
pub(crate) fn render_scene(
    project: &Project,
    scene: SceneId,
    pipeline: &fm_sim::SimulatedPipeline,
    frame_number: u64,
) -> Result<ImageFrame, SceneRenderError> {
    let model = project
        .scenes()
        .iter()
        .find(|candidate| candidate.id == scene)
        .ok_or(SceneRenderError::UnknownScene(scene))?;
    let mut composition = Scene::new(
        pipeline.width(),
        pipeline.height(),
        Rgba8::new(
            model.background.red,
            model.background.green,
            model.background.blue,
            model.background.alpha,
        ),
    )
    .map_err(SceneRenderError::Scene)?;
    let mut sources = BTreeMap::new();

    for (index, layer) in model.layers.iter().enumerate() {
        if !layer.enabled {
            continue;
        }
        if composition.layers().len() == CompositionPlan::MAX_LAYERS {
            return Err(SceneRenderError::Plan(PlanError::TooManyLayers {
                actual: composition.layers().len() + 1,
                maximum: CompositionPlan::MAX_LAYERS,
            }));
        }
        let input = input_for_layer(project, layer.source)?;
        validate_input(input)?;
        validate_bounds(
            index,
            layer.crop,
            layer.mask,
            pipeline.width(),
            pipeline.height(),
        )?;
        let token = if let Some(token) = sources.get(&input.id) {
            *token
        } else {
            let token = SourceId::new(sources.len() as u64);
            sources.insert(input.id, token);
            token
        };
        composition.push_layer(compositor_layer(token, layer));
    }

    let (plan, _) =
        compile_scene(&composition, OutputTarget::Program).map_err(SceneRenderError::Plan)?;
    let frames = sources
        .into_iter()
        .map(|(input, source)| {
            pipeline
                .render_source(input, frame_number)
                .map(|frame| (source, frame))
                .map_err(SceneRenderError::Source)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let bindings = frames
        .iter()
        .map(|(source, frame)| CpuSourceFrame::new(*source, frame))
        .collect::<Vec<_>>();
    execute_cpu(&plan, &bindings).map_err(SceneRenderError::Cpu)
}

fn input_for_layer(project: &Project, source: SourceRef) -> Result<&Input, SceneRenderError> {
    let input = match source {
        SourceRef::Input(input) => input,
        SourceRef::Scene(scene) => return Err(SceneRenderError::SceneSource(scene)),
    };
    project
        .inputs()
        .iter()
        .find(|candidate| candidate.id == input)
        .ok_or(SceneRenderError::UnknownInput(input))
}

fn validate_input(input: &Input) -> Result<(), SceneRenderError> {
    if matches!(
        &input.kind,
        InputKind::Simulated(simulated)
            if matches!(simulated.video, SimulatedVideo::Bars | SimulatedVideo::Solid(_))
    ) {
        return Ok(());
    }
    Err(SceneRenderError::UnsupportedInput { input: input.id })
}

fn validate_bounds(
    layer: usize,
    crop: Option<fm_model::CropRect>,
    mask: Option<fm_model::RectMask>,
    source_width: u32,
    source_height: u32,
) -> Result<(), SceneRenderError> {
    let (width, height) = crop.map_or(Ok((source_width, source_height)), |crop| {
        let right = crop
            .x
            .checked_add(crop.width)
            .ok_or(SceneRenderError::CropBoundsOverflow { layer })?;
        let bottom = crop
            .y
            .checked_add(crop.height)
            .ok_or(SceneRenderError::CropBoundsOverflow { layer })?;
        if right > source_width || bottom > source_height {
            return Err(SceneRenderError::CropOutOfBounds {
                layer,
                width: source_width,
                height: source_height,
            });
        }
        Ok((crop.width, crop.height))
    })?;
    let Some(mask) = mask else { return Ok(()) };
    let right = mask
        .x
        .checked_add(mask.width)
        .ok_or(SceneRenderError::MaskBoundsOverflow { layer })?;
    let bottom = mask
        .y
        .checked_add(mask.height)
        .ok_or(SceneRenderError::MaskBoundsOverflow { layer })?;
    if right > width || bottom > height {
        return Err(SceneRenderError::MaskOutOfBounds {
            layer,
            width,
            height,
        });
    }
    Ok(())
}

fn compositor_layer(token: SourceId, layer: &fm_model::Layer) -> SourceLayer {
    let geometry = layer.geometry;
    let rotation = match geometry.rotation {
        fm_model::Rotation::Deg0 => Rotation::Deg0,
        fm_model::Rotation::Deg90 => Rotation::Deg90,
        fm_model::Rotation::Deg180 => Rotation::Deg180,
        fm_model::Rotation::Deg270 => Rotation::Deg270,
    };
    let mut result = SourceLayer::new(
        token,
        layer.z_order,
        Transform::new(
            geometry.translation_x,
            geometry.translation_y,
            geometry.width,
            geometry.height,
            rotation,
        ),
    )
    .with_opacity(layer.opacity);
    if let Some(crop) = layer.crop {
        result = result.with_crop(CropRect::new(crop.x, crop.y, crop.width, crop.height));
    }
    if let Some(mask) = layer.mask {
        result = result.with_mask(
            RectMask::new(mask.x, mask.y, mask.width, mask.height).inverted(mask.invert),
        );
    }
    result
}
