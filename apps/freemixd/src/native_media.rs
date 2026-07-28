//! Opt-in local-file decode and native GPU composition root.
//!
//! [`NativeMediaRuntime::preroll_local_blocking`] launches bounded `FFmpeg` and
//! ffprobe subprocesses synchronously. Call it from a worker thread, even
//! though the function is async so that GPU normalization can remain async.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    future::Future,
    num::{NonZeroU32, NonZeroUsize},
    path::{Path, PathBuf},
    pin::pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    task::{Context, Poll, Wake, Waker},
    thread,
};

use fm_audio::{
    AudioCadenceOrigin, AudioRenderPlan, AudioSilenceSpan, AudioSynchronizerLimits, ChannelMapping,
    ClippingPolicy, ClockMappedAudioSynchronizer, InputState, MAX_CHANNEL_MAPPING_CHANNELS,
    MasterAudioInterval, MasterMixer, PlanarAudioSource, SourceGain,
};
use fm_clock::{
    ClockDomainId as MappingClockDomainId, ClockMapping, ClockSnapshot, ClockTime, FrameCadence,
};
#[cfg(test)]
use fm_codec_ffmpeg::SequenceRequest;
use fm_codec_ffmpeg::{
    Adapter, DecodeRequest, DecodedAudioWindow, DecodedVideoWindow, LocalAudioDecoder,
    LocalVideoDecoder, StreamKind, StreamSelector,
};
use fm_color::{
    NativeImportError, NativeImportNormalizer, NativeSdrOutputTransform, NativeWorkingFrame,
};
use fm_compositor::{
    CompositionPlan, FadeToBlackPlan, FadeToBlackPlanError,
    FadeToBlackPosition as CompositorFadeToBlackPosition, NativeCompositionError,
    NativeCompositionRenderer, NativeFadeToBlackError, NativeFadeToBlackRenderer,
    NativeSourceFrame, NativeStingerError, NativeStingerRenderer, NativeTransitionError,
    NativeTransitionRenderer, OutputTarget, PlanError, RectMask as CompositorRectMask,
    Rgba8 as CompositorRgba8, Rotation as CompositorRotation, Scene as CompositorScene, SourceId,
    SourceLayer, StingerFramePlan, StingerPlanError, Transform, TransitionError, TransitionKind,
    TransitionPlan, compile_scene,
};
use fm_engine::FrameResult;
use fm_frame::{
    AudioBlock, ClockDomainId, CpuVideoFrame, MediaTimestamp, MediaTiming, NormalizedDuration,
    NormalizedTimestamp, OriginalTimestamp, SequenceNumber, TimingError,
};
use fm_gpu::{
    DiagnosticReadback, NativeAdapterInfo, NativeBackend, NativeContext, NativeGpuError,
    NativeTexture, NativeTextureReadback,
};
use fm_model::{
    InputAudioStripState, InputKind, Project, Rotation, SourceRef,
    StingerAudioPolicy as ModelStingerAudioPolicy,
};
use fm_sim::{CollectingAudioSink, OverflowPolicy, SinkConfigError, SinkTelemetry};
use fm_switcher::{FadeToBlackFrame, ProgramFrame, TransitionKind as SwitcherTransitionKind};
use fm_types::{AudioFormat, FrameRate, InputId, SampleFormat, SceneId, TimeBase};

const RGBA16_FLOAT_BYTES_PER_PIXEL: u64 = 8;
const NATIVE_PROJECT_IN_FLIGHT_SLOTS: usize = 1;
const SOURCE_REFILL_LOW_WATERMARK: usize = 4;
const SOURCE_REFILL_MAX_PAGE: u32 = 4;
const AUDIO_REFILL_LOW_WATERMARK: usize = 8;
const MAX_NATIVE_AUDIO_STRIPS: usize = NativeSourceLimits::DEFAULT_MAX_MEDIA_INPUTS;

/// Resource bounds applied while compiling native scenes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeProjectLimits {
    pub max_reachable_scenes: usize,
    pub max_total_active_layers: usize,
    pub max_transient_rgba16f_bytes: u64,
}

impl NativeProjectLimits {
    pub const DEFAULT_MAX_REACHABLE_SCENES: usize = 64;
    pub const DEFAULT_MAX_TOTAL_ACTIVE_LAYERS: usize = 64;
    pub const DEFAULT_MAX_TRANSIENT_RGBA16F_BYTES: u64 = 512 * 1024 * 1024;
}

impl Default for NativeProjectLimits {
    fn default() -> Self {
        Self {
            max_reachable_scenes: Self::DEFAULT_MAX_REACHABLE_SCENES,
            max_total_active_layers: Self::DEFAULT_MAX_TOTAL_ACTIVE_LAYERS,
            max_transient_rgba16f_bytes: Self::DEFAULT_MAX_TRANSIENT_RGBA16F_BYTES,
        }
    }
}

/// A schema input's native video realization route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeVideoRoute {
    Leaf(InputId),
    Scene(SceneId),
}

/// A schema input's recursively resolved native audio terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeAudioRoute {
    Leaf(InputId),
    Silence,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum NativeSceneSource {
    Leaf(InputId),
    Scene(SceneId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NativeScenePlan {
    id: SceneId,
    output: SourceId,
    composition: CompositionPlan,
    sources: Vec<(SourceId, NativeSceneSource)>,
}

impl NativeScenePlan {
    fn scene_dependencies(&self) -> impl Iterator<Item = SceneId> + '_ {
        self.sources.iter().filter_map(|(_, source)| match source {
            NativeSceneSource::Scene(scene) => Some(*scene),
            NativeSceneSource::Leaf(_) => None,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NativeSceneExecution {
    roots: BTreeSet<SceneId>,
    required: BTreeSet<SceneId>,
    remaining_consumers: BTreeMap<SceneId, usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeStingerConfig {
    media_input: InputId,
    preload: bool,
    cut_point_frames: u32,
    audio_policy: ModelStingerAudioPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeStingerFrameRequest {
    pub input: InputId,
    pub deadline: ClockTime,
}

/// Typed failures produced before any native device or decoder is opened.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeProjectPlanError {
    MissingInput(InputId),
    MissingScene(SceneId),
    InvalidStingerSlot(u8),
    SceneCycle {
        scene: SceneId,
    },
    AudioCycle {
        input: InputId,
    },
    TooManyReachableScenes {
        actual: usize,
        maximum: usize,
    },
    TooManyActiveLayers {
        actual: usize,
        maximum: usize,
    },
    TransientByteSizeOverflow {
        width: u32,
        height: u32,
        targets: usize,
    },
    TransientTargetCountOverflow,
    TransientBytesExceeded {
        required: u64,
        maximum: u64,
    },
    Scene {
        scene: SceneId,
        error: fm_compositor::SceneError,
    },
    Composition {
        scene: SceneId,
        error: PlanError,
    },
    SourceTokenExhausted,
}

impl fmt::Display for NativeProjectPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingInput(input) => {
                write!(formatter, "native scene references missing input {input}")
            }
            Self::MissingScene(scene) => write!(formatter, "native scene {scene} is missing"),
            Self::InvalidStingerSlot(slot) => {
                write!(formatter, "native project has invalid Stinger slot {slot}")
            }
            Self::SceneCycle { scene } => {
                write!(formatter, "native scene dependency cycle reaches {scene}")
            }
            Self::AudioCycle { input } => {
                write!(formatter, "native scene audio cycle reaches input {input}")
            }
            Self::TooManyReachableScenes { actual, maximum } => write!(
                formatter,
                "native project has {actual} reachable scenes; maximum is {maximum}"
            ),
            Self::TooManyActiveLayers { actual, maximum } => write!(
                formatter,
                "native project has {actual} active reachable scene layers; maximum is {maximum}"
            ),
            Self::TransientByteSizeOverflow {
                width,
                height,
                targets,
            } => write!(
                formatter,
                "{targets} simultaneous native targets at {width}x{height} overflow the RGBA16F byte charge"
            ),
            Self::TransientTargetCountOverflow => {
                formatter.write_str("native transient target count overflowed")
            }
            Self::TransientBytesExceeded { required, maximum } => write!(
                formatter,
                "native transient RGBA16F bytes {required} exceed limit {maximum}"
            ),
            Self::Scene { scene, error } => {
                write!(formatter, "native scene {scene} is invalid: {error}")
            }
            Self::Composition { scene, error } => {
                write!(formatter, "native scene {scene} plan is invalid: {error}")
            }
            Self::SourceTokenExhausted => {
                formatter.write_str("native scene source tokens are exhausted")
            }
        }
    }
}

impl Error for NativeProjectPlanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Scene { error, .. } => Some(error),
            Self::Composition { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// Immutable, bounded native realization plan for one canonical project.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeProjectPlan {
    video_routes: BTreeMap<InputId, NativeVideoRoute>,
    audio_routes: BTreeMap<InputId, NativeAudioRoute>,
    audio_strips: BTreeMap<InputId, InputAudioStripState>,
    stingers: BTreeMap<fm_switcher::StingerSlotId, NativeStingerConfig>,
    frame_rate: FrameRate,
    scenes: Vec<NativeScenePlan>,
    peak_rgba16f_targets: usize,
    transient_rgba16f_bytes: u64,
}

impl NativeProjectPlan {
    /// Compiles all scene-input roots and their enabled dependencies without I/O.
    ///
    /// # Errors
    ///
    /// Returns typed missing-resource, cycle, scene-validation, or bound failures.
    #[allow(clippy::too_many_lines)]
    pub fn compile(
        project: &Project,
        limits: NativeProjectLimits,
    ) -> Result<Self, NativeProjectPlanError> {
        let inputs = project
            .inputs()
            .iter()
            .map(|input| (input.id, input))
            .collect::<BTreeMap<_, _>>();
        let scene_models = project
            .scenes()
            .iter()
            .map(|scene| (scene.id, scene))
            .collect::<BTreeMap<_, _>>();
        let video_routes = project
            .inputs()
            .iter()
            .map(|input| {
                let route = match input.kind {
                    InputKind::Scene { scene_id, .. } => NativeVideoRoute::Scene(scene_id),
                    _ => NativeVideoRoute::Leaf(input.id),
                };
                (input.id, route)
            })
            .collect::<BTreeMap<_, _>>();

        let mut order = Vec::new();
        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        let roots = video_routes
            .values()
            .filter_map(|route| match route {
                NativeVideoRoute::Scene(scene) => Some(*scene),
                NativeVideoRoute::Leaf(_) => None,
            })
            .collect::<BTreeSet<_>>();
        for root in roots {
            visit_native_scene(
                root,
                &inputs,
                &scene_models,
                &mut visiting,
                &mut visited,
                &mut order,
            )?;
        }
        if order.len() > limits.max_reachable_scenes {
            return Err(NativeProjectPlanError::TooManyReachableScenes {
                actual: order.len(),
                maximum: limits.max_reachable_scenes,
            });
        }
        let active_layers = order.iter().try_fold(0_usize, |total, scene_id| {
            let scene = scene_models
                .get(scene_id)
                .ok_or(NativeProjectPlanError::MissingScene(*scene_id))?;
            total
                .checked_add(scene.layers.iter().filter(|layer| layer.enabled).count())
                .ok_or(NativeProjectPlanError::TooManyActiveLayers {
                    actual: usize::MAX,
                    maximum: limits.max_total_active_layers,
                })
        })?;
        if active_layers > limits.max_total_active_layers {
            return Err(NativeProjectPlanError::TooManyActiveLayers {
                actual: active_layers,
                maximum: limits.max_total_active_layers,
            });
        }

        let dimensions = project.settings().video.dimensions;
        let mut next_source = 0_u64;
        let mut scenes = Vec::with_capacity(order.len());
        for scene_id in order {
            let model = scene_models
                .get(&scene_id)
                .ok_or(NativeProjectPlanError::MissingScene(scene_id))?;
            let mut scene = CompositorScene::new(
                dimensions.width(),
                dimensions.height(),
                CompositorRgba8::new(
                    model.background.red,
                    model.background.green,
                    model.background.blue,
                    model.background.alpha,
                ),
            )
            .map_err(|error| NativeProjectPlanError::Scene {
                scene: scene_id,
                error,
            })?;
            let mut tokens = BTreeMap::new();
            for (layer_index, layer) in model.layers.iter().enumerate() {
                let (source_width, source_height) = layer
                    .crop
                    .map_or((dimensions.width(), dimensions.height()), |crop| {
                        (crop.width, crop.height)
                    });
                if layer.mask.is_some_and(|mask| {
                    mask.width == 0
                        || mask.height == 0
                        || mask
                            .x
                            .checked_add(mask.width)
                            .is_none_or(|right| right > source_width)
                        || mask
                            .y
                            .checked_add(mask.height)
                            .is_none_or(|bottom| bottom > source_height)
                }) {
                    return Err(NativeProjectPlanError::Composition {
                        scene: scene_id,
                        error: PlanError::InvalidMask { layer: layer_index },
                    });
                }
                let token = if layer.enabled {
                    let source = resolve_scene_source(layer.source, &inputs, &scene_models)?;
                    if let Some(token) = tokens.get(&source) {
                        *token
                    } else {
                        let token = SourceId::new(next_source);
                        next_source = next_source
                            .checked_add(1)
                            .ok_or(NativeProjectPlanError::SourceTokenExhausted)?;
                        tokens.insert(source, token);
                        token
                    }
                } else {
                    SourceId::new(0)
                };
                let geometry = layer.geometry;
                let rotation = match geometry.rotation {
                    Rotation::Deg0 => CompositorRotation::Deg0,
                    Rotation::Deg90 => CompositorRotation::Deg90,
                    Rotation::Deg180 => CompositorRotation::Deg180,
                    Rotation::Deg270 => CompositorRotation::Deg270,
                };
                let mut native_layer = SourceLayer::new(
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
                .with_enabled(layer.enabled)
                .with_opacity(layer.opacity);
                if let Some(crop) = layer.crop {
                    native_layer = native_layer.with_crop(fm_compositor::CropRect::new(
                        crop.x,
                        crop.y,
                        crop.width,
                        crop.height,
                    ));
                }
                if let Some(mask) = layer.mask {
                    native_layer = native_layer.with_mask(
                        CompositorRectMask::new(mask.x, mask.y, mask.width, mask.height)
                            .inverted(mask.invert),
                    );
                }
                scene.push_layer(native_layer);
            }
            let (composition, _) =
                compile_scene(&scene, OutputTarget::Program).map_err(|error| {
                    NativeProjectPlanError::Composition {
                        scene: scene_id,
                        error,
                    }
                })?;
            let output = SourceId::new(next_source);
            next_source = next_source
                .checked_add(1)
                .ok_or(NativeProjectPlanError::SourceTokenExhausted)?;
            scenes.push(NativeScenePlan {
                id: scene_id,
                output,
                composition,
                sources: tokens
                    .into_iter()
                    .map(|(source, token)| (token, source))
                    .collect(),
            });
        }

        let stinger_routes = project
            .stingers()
            .iter()
            .filter_map(|config| video_routes.get(&config.media_input).copied())
            .collect::<Vec<_>>();
        let peak_rgba16f_targets =
            maximum_native_execution_peak(&video_routes, &stinger_routes, &scenes)?;
        let frame_bytes = u64::from(dimensions.width())
            .checked_mul(u64::from(dimensions.height()))
            .and_then(|pixels| pixels.checked_mul(RGBA16_FLOAT_BYTES_PER_PIXEL))
            .ok_or(NativeProjectPlanError::TransientByteSizeOverflow {
                width: dimensions.width(),
                height: dimensions.height(),
                targets: peak_rgba16f_targets,
            })?;
        let target_count = u64::try_from(peak_rgba16f_targets)
            .map_err(|_| NativeProjectPlanError::TransientTargetCountOverflow)?;
        let transient_rgba16f_bytes = frame_bytes.checked_mul(target_count).ok_or(
            NativeProjectPlanError::TransientByteSizeOverflow {
                width: dimensions.width(),
                height: dimensions.height(),
                targets: peak_rgba16f_targets,
            },
        )?;
        if transient_rgba16f_bytes > limits.max_transient_rgba16f_bytes {
            return Err(NativeProjectPlanError::TransientBytesExceeded {
                required: transient_rgba16f_bytes,
                maximum: limits.max_transient_rgba16f_bytes,
            });
        }

        let mut audio_routes = BTreeMap::new();
        for input in project.inputs() {
            let mut visiting = BTreeSet::new();
            let route = resolve_audio_route(input.id, &inputs, &mut audio_routes, &mut visiting)?;
            audio_routes.insert(input.id, route);
        }
        let stingers = project
            .stingers()
            .iter()
            .map(|config| {
                fm_switcher::StingerSlotId::new(config.slot.number())
                    .map(|slot| {
                        (
                            slot,
                            NativeStingerConfig {
                                media_input: config.media_input,
                                preload: config.preload,
                                cut_point_frames: config.cut_point_frames,
                                audio_policy: config.audio_policy,
                            },
                        )
                    })
                    .ok_or(NativeProjectPlanError::InvalidStingerSlot(
                        config.slot.number(),
                    ))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        Ok(Self {
            video_routes,
            audio_routes,
            audio_strips: project
                .input_audio_strips()
                .iter()
                .map(|strip| (strip.input, strip.state))
                .collect(),
            stingers,
            frame_rate: project.settings().frame_rate,
            scenes,
            peak_rgba16f_targets,
            transient_rgba16f_bytes,
        })
    }

    #[must_use]
    pub fn video_route(&self, input: InputId) -> Option<NativeVideoRoute> {
        self.video_routes.get(&input).copied()
    }

    #[must_use]
    pub fn audio_route(&self, input: InputId) -> Option<NativeAudioRoute> {
        self.audio_routes.get(&input).copied()
    }

    #[must_use]
    pub fn audio_strip(&self, input: InputId) -> Option<InputAudioStripState> {
        self.audio_strips.get(&input).copied()
    }

    fn stinger(&self, slot: fm_switcher::StingerSlotId) -> Option<NativeStingerConfig> {
        self.stingers.get(&slot).copied()
    }

    /// Resolves the clip-local media deadline required by an authoritative
    /// projected Stinger frame.
    ///
    /// # Errors
    ///
    /// Returns a typed project-plan or clip-deadline failure.
    pub fn stinger_frame_request(
        &self,
        frame: &FrameResult,
    ) -> Result<Option<NativeStingerFrameRequest>, NativeSourceRenderError> {
        let NativeProjectMixPlan::Stinger(plan) = native_project_mix_plan(self, frame.program)?
        else {
            return Ok(None);
        };
        Ok(Some(NativeStingerFrameRequest {
            input: plan.media,
            deadline: stinger_frame_deadline(self.frame_rate, plan.frame.frame_index())?,
        }))
    }

    #[must_use]
    pub fn scene_order(&self) -> impl ExactSizeIterator<Item = SceneId> + '_ {
        self.scenes.iter().map(|scene| scene.id)
    }

    #[must_use]
    pub fn active_layer_count(&self) -> usize {
        self.scenes
            .iter()
            .map(|scene| scene.composition.layers().len())
            .sum()
    }

    /// Maximum simultaneous scene, transition, and previous Program targets.
    #[must_use]
    pub const fn peak_rgba16f_targets(&self) -> usize {
        self.peak_rgba16f_targets
    }

    #[must_use]
    pub const fn transient_rgba16f_bytes(&self) -> u64 {
        self.transient_rgba16f_bytes
    }

    fn scene_execution(&self, inputs: &[InputId]) -> Option<NativeSceneExecution> {
        scene_execution_for_routes(
            inputs
                .iter()
                .map(|input| self.video_routes.get(input).copied()),
            &self.scenes,
        )
    }
}

fn maximum_native_execution_peak(
    routes: &BTreeMap<InputId, NativeVideoRoute>,
    stinger_routes: &[NativeVideoRoute],
    scenes: &[NativeScenePlan],
) -> Result<usize, NativeProjectPlanError> {
    let routes = routes.values().copied().collect::<Vec<_>>();
    let mut maximum = 3_usize;
    for primary in &routes {
        for secondary in &routes {
            let execution = scene_execution_for_routes([Some(*primary), Some(*secondary)], scenes)
                .ok_or(NativeProjectPlanError::TransientTargetCountOverflow)?;
            maximum = maximum.max(native_execution_peak(&execution)?);
            for stinger in stinger_routes {
                let execution = scene_execution_for_routes(
                    [Some(*primary), Some(*secondary), Some(*stinger)],
                    scenes,
                )
                .ok_or(NativeProjectPlanError::TransientTargetCountOverflow)?;
                maximum = maximum.max(native_execution_peak(&execution)?);
            }
        }
    }
    Ok(maximum)
}

fn scene_execution_for_routes(
    routes: impl IntoIterator<Item = Option<NativeVideoRoute>>,
    scenes: &[NativeScenePlan],
) -> Option<NativeSceneExecution> {
    let roots = routes
        .into_iter()
        .flatten()
        .filter_map(|route| match route {
            NativeVideoRoute::Scene(scene) => Some(scene),
            NativeVideoRoute::Leaf(_) => None,
        })
        .collect::<BTreeSet<_>>();
    let mut required = roots.clone();
    for scene in scenes.iter().rev() {
        if required.contains(&scene.id) {
            required.extend(scene.scene_dependencies());
        }
    }
    let mut remaining_consumers = required
        .iter()
        .map(|scene| (*scene, 0_usize))
        .collect::<BTreeMap<_, _>>();
    for scene in scenes {
        if !required.contains(&scene.id) {
            continue;
        }
        for dependency in scene.scene_dependencies() {
            let consumers = remaining_consumers.get_mut(&dependency)?;
            *consumers = consumers.checked_add(1)?;
        }
    }
    Some(NativeSceneExecution {
        roots,
        required,
        remaining_consumers,
    })
}

fn native_execution_peak(
    execution: &NativeSceneExecution,
) -> Result<usize, NativeProjectPlanError> {
    // One fenced slot retains the full closure, its transition target, and the
    // post-Program FTB target. The daemon also owns the previous Program target
    // until the new frame returns.
    execution
        .required
        .len()
        .checked_add(NATIVE_PROJECT_IN_FLIGHT_SLOTS)
        .and_then(|targets| targets.checked_add(1))
        .and_then(|targets| targets.checked_add(1))
        .ok_or(NativeProjectPlanError::TransientTargetCountOverflow)
}

fn resolve_scene_source(
    source: SourceRef,
    inputs: &BTreeMap<InputId, &fm_model::Input>,
    scenes: &BTreeMap<SceneId, &fm_model::Scene>,
) -> Result<NativeSceneSource, NativeProjectPlanError> {
    match source {
        SourceRef::Input(input) => match &inputs
            .get(&input)
            .ok_or(NativeProjectPlanError::MissingInput(input))?
            .kind
        {
            InputKind::Scene { scene_id, .. } => {
                if !scenes.contains_key(scene_id) {
                    return Err(NativeProjectPlanError::MissingScene(*scene_id));
                }
                Ok(NativeSceneSource::Scene(*scene_id))
            }
            _ => Ok(NativeSceneSource::Leaf(input)),
        },
        SourceRef::Scene(scene) => {
            if !scenes.contains_key(&scene) {
                return Err(NativeProjectPlanError::MissingScene(scene));
            }
            Ok(NativeSceneSource::Scene(scene))
        }
    }
}

fn visit_native_scene(
    scene: SceneId,
    inputs: &BTreeMap<InputId, &fm_model::Input>,
    scenes: &BTreeMap<SceneId, &fm_model::Scene>,
    visiting: &mut BTreeSet<SceneId>,
    visited: &mut BTreeSet<SceneId>,
    order: &mut Vec<SceneId>,
) -> Result<(), NativeProjectPlanError> {
    if visited.contains(&scene) {
        return Ok(());
    }
    if !visiting.insert(scene) {
        return Err(NativeProjectPlanError::SceneCycle { scene });
    }
    let model = scenes
        .get(&scene)
        .ok_or(NativeProjectPlanError::MissingScene(scene))?;
    let dependencies = model
        .layers
        .iter()
        .filter(|layer| layer.enabled)
        .map(|layer| resolve_scene_source(layer.source, inputs, scenes))
        .collect::<Result<BTreeSet<_>, _>>()?;
    for dependency in dependencies {
        if let NativeSceneSource::Scene(dependency) = dependency {
            visit_native_scene(dependency, inputs, scenes, visiting, visited, order)?;
        }
    }
    visiting.remove(&scene);
    visited.insert(scene);
    order.push(scene);
    Ok(())
}

fn resolve_audio_route(
    input: InputId,
    inputs: &BTreeMap<InputId, &fm_model::Input>,
    resolved: &mut BTreeMap<InputId, NativeAudioRoute>,
    visiting: &mut BTreeSet<InputId>,
) -> Result<NativeAudioRoute, NativeProjectPlanError> {
    if let Some(route) = resolved.get(&input) {
        return Ok(*route);
    }
    if !visiting.insert(input) {
        return Err(NativeProjectPlanError::AudioCycle { input });
    }
    let model = inputs
        .get(&input)
        .ok_or(NativeProjectPlanError::MissingInput(input))?;
    let route = match model.kind {
        InputKind::Scene {
            audio_source: Some(source),
            ..
        } => resolve_audio_route(source, inputs, resolved, visiting)?,
        InputKind::Scene {
            audio_source: None, ..
        } => NativeAudioRoute::Silence,
        _ => NativeAudioRoute::Leaf(input),
    };
    visiting.remove(&input);
    resolved.insert(input, route);
    Ok(route)
}

struct ThreadWaker(thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::park(),
        }
    }
}

/// GPU-normalized video frames and decoded audio from one bounded preroll.
pub struct NativeMediaPreroll {
    video: Vec<NativeWorkingFrame>,
    audio: Vec<AudioBlock>,
}

impl NativeMediaPreroll {
    /// Returns the canonical GPU-resident video frames.
    #[must_use]
    pub fn video(&self) -> &[NativeWorkingFrame] {
        &self.video
    }

    /// Returns the decoded audio blocks without modification.
    #[must_use]
    pub fn audio(&self) -> &[AudioBlock] {
        &self.audio
    }

    /// Consumes the preroll into its GPU video frames and decoded audio blocks.
    #[must_use]
    pub fn into_parts(self) -> (Vec<NativeWorkingFrame>, Vec<AudioBlock>) {
        (self.video, self.audio)
    }
}

/// Aggregate failures from native media setup and execution.
#[derive(Debug)]
pub enum NativeMediaError {
    Ffmpeg(fm_codec_ffmpeg::Error),
    Gpu(NativeGpuError),
    Color(NativeImportError),
    SceneCompositor(NativeCompositionError),
    Compositor(NativeTransitionError),
    Stinger(NativeStingerError),
    FadeToBlack(NativeFadeToBlackError),
}

impl fmt::Display for NativeMediaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ffmpeg(error) => write!(formatter, "local media decode failed: {error}"),
            Self::Gpu(error) => write!(formatter, "native GPU setup or diagnostic failed: {error}"),
            Self::Color(error) => write!(formatter, "native color normalization failed: {error}"),
            Self::SceneCompositor(error) => {
                write!(formatter, "native scene composition failed: {error}")
            }
            Self::Compositor(error) => write!(formatter, "native composition failed: {error}"),
            Self::Stinger(error) => write!(formatter, "native Stinger setup failed: {error}"),
            Self::FadeToBlack(error) => {
                write!(formatter, "native Fade-to-Black setup failed: {error}")
            }
        }
    }
}

impl Error for NativeMediaError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Ffmpeg(error) => Some(error),
            Self::Gpu(error) => Some(error),
            Self::Color(error) => Some(error),
            Self::SceneCompositor(error) => Some(error),
            Self::Compositor(error) => Some(error),
            Self::Stinger(error) => Some(error),
            Self::FadeToBlack(error) => Some(error),
        }
    }
}

impl From<fm_codec_ffmpeg::Error> for NativeMediaError {
    fn from(value: fm_codec_ffmpeg::Error) -> Self {
        Self::Ffmpeg(value)
    }
}

impl From<NativeGpuError> for NativeMediaError {
    fn from(value: NativeGpuError) -> Self {
        Self::Gpu(value)
    }
}

impl From<NativeImportError> for NativeMediaError {
    fn from(value: NativeImportError) -> Self {
        Self::Color(value)
    }
}

impl From<NativeTransitionError> for NativeMediaError {
    fn from(value: NativeTransitionError) -> Self {
        Self::Compositor(value)
    }
}

impl From<NativeStingerError> for NativeMediaError {
    fn from(value: NativeStingerError) -> Self {
        Self::Stinger(value)
    }
}

impl From<NativeCompositionError> for NativeMediaError {
    fn from(value: NativeCompositionError) -> Self {
        Self::SceneCompositor(value)
    }
}

impl From<NativeFadeToBlackError> for NativeMediaError {
    fn from(value: NativeFadeToBlackError) -> Self {
        Self::FadeToBlack(value)
    }
}

/// Resource bounds for GPU-resident native source rings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeSourceLimits {
    pub max_media_inputs: usize,
    pub max_video_frames_per_source: NonZeroU32,
    pub max_retained_rgba16f_bytes: u64,
}

impl NativeSourceLimits {
    pub const DEFAULT_MAX_MEDIA_INPUTS: usize = 64;
    pub const DEFAULT_MAX_VIDEO_FRAMES_PER_SOURCE: NonZeroU32 =
        NonZeroU32::new(8).expect("eight is nonzero");
    pub const DEFAULT_MAX_RETAINED_RGBA16F_BYTES: u64 = 512 * 1024 * 1024;
}

impl Default for NativeSourceLimits {
    fn default() -> Self {
        Self {
            max_media_inputs: Self::DEFAULT_MAX_MEDIA_INPUTS,
            max_video_frames_per_source: Self::DEFAULT_MAX_VIDEO_FRAMES_PER_SOURCE,
            max_retained_rgba16f_bytes: Self::DEFAULT_MAX_RETAINED_RGBA16F_BYTES,
        }
    }
}

/// A fully resolved native playback source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeResolvedSource {
    LocalVideo {
        input: InputId,
        path: PathBuf,
    },
    RetainedFrame {
        input: InputId,
        frame: CpuVideoFrame,
    },
    LiveFrame {
        input: InputId,
        frame: CpuVideoFrame,
    },
}

impl NativeResolvedSource {
    /// Returns the full-width input identity of this source.
    #[must_use]
    pub const fn input(&self) -> InputId {
        match self {
            Self::LocalVideo { input, .. }
            | Self::RetainedFrame { input, .. }
            | Self::LiveFrame { input, .. } => *input,
        }
    }
}

/// Pure source-registry validation failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeSourceError {
    TooManySources {
        actual: usize,
        maximum: usize,
    },
    TooManyFrames {
        input: InputId,
        actual: usize,
        maximum: u32,
    },
    DuplicateSource(InputId),
    FrameByteSizeOverflow {
        input: InputId,
        width: u32,
        height: u32,
    },
    RetainedBytesExceeded {
        required: u64,
        maximum: u64,
    },
    DimensionMismatch {
        input: InputId,
        expected_width: u32,
        expected_height: u32,
        actual_width: u32,
        actual_height: u32,
    },
    InvalidTimeline {
        input: InputId,
    },
}

impl fmt::Display for NativeSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManySources { actual, maximum } => {
                write!(formatter, "source count {actual} exceeds limit {maximum}")
            }
            Self::TooManyFrames {
                input,
                actual,
                maximum,
            } => write!(
                formatter,
                "source {input} frame count {actual} exceeds limit {maximum}"
            ),
            Self::DuplicateSource(input) => {
                write!(formatter, "source {input} is already registered")
            }
            Self::FrameByteSizeOverflow {
                input,
                width,
                height,
            } => write!(
                formatter,
                "source {input} dimensions {width}x{height} overflow the RGBA16F byte charge"
            ),
            Self::RetainedBytesExceeded { required, maximum } => write!(
                formatter,
                "retained RGBA16F bytes {required} exceed limit {maximum}"
            ),
            Self::DimensionMismatch {
                input,
                expected_width,
                expected_height,
                actual_width,
                actual_height,
            } => write!(
                formatter,
                "source {input} dimensions {actual_width}x{actual_height} do not match {expected_width}x{expected_height}"
            ),
            Self::InvalidTimeline { input } => {
                write!(formatter, "source {input} has an invalid video timeline")
            }
        }
    }
}

impl Error for NativeSourceError {}

/// Failures while decoding and uploading a bounded source prefix.
#[derive(Debug)]
pub enum NativeSourcePreflightError {
    Source(NativeSourceError),
    Decode {
        input: InputId,
        error: fm_codec_ffmpeg::Error,
    },
    DecodeContract {
        input: InputId,
        video_frames: usize,
        audio_blocks: usize,
    },
    Normalize {
        input: InputId,
        error: NativeImportError,
    },
    CodecAdapterRequired {
        input: InputId,
    },
    WorkerUnavailable,
}

impl fmt::Display for NativeSourcePreflightError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => error.fmt(formatter),
            Self::Decode { input, error } => {
                write!(
                    formatter,
                    "source {input} video preflight decode failed: {error}"
                )
            }
            Self::DecodeContract {
                input,
                video_frames,
                audio_blocks,
            } => write!(
                formatter,
                "source {input} preflight returned {video_frames} video frames and {audio_blocks} audio blocks; expected a nonempty bounded video prefix and no audio"
            ),
            Self::Normalize { input, error } => {
                write!(
                    formatter,
                    "source {input} native normalization failed: {error}"
                )
            }
            Self::CodecAdapterRequired { input } => {
                write!(
                    formatter,
                    "source {input} requires a local video codec adapter"
                )
            }
            Self::WorkerUnavailable => {
                formatter.write_str("native source decode worker could not be started")
            }
        }
    }
}

impl Error for NativeSourcePreflightError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Decode { error, .. } => Some(error),
            Self::Normalize { error, .. } => Some(error),
            Self::DecodeContract { .. }
            | Self::CodecAdapterRequired { .. }
            | Self::WorkerUnavailable => None,
        }
    }
}

impl From<NativeSourceError> for NativeSourcePreflightError {
    fn from(value: NativeSourceError) -> Self {
        Self::Source(value)
    }
}

/// Fatal failures while pumping bounded native source playback.
#[derive(Debug)]
pub enum NativeSourcePlaybackError {
    Source(NativeSourceError),
    Decode {
        input: InputId,
        error: fm_codec_ffmpeg::Error,
    },
    DecodeContract {
        input: InputId,
    },
    Normalize {
        input: InputId,
        error: NativeImportError,
    },
    WorkerDisconnected,
    WorkerPanicked,
    WorkerQueueFull,
    SourceNotLive {
        input: InputId,
    },
    MissingSource {
        input: InputId,
    },
    StingerSourceNotFullyPreloaded {
        input: InputId,
        retained_frames: usize,
        maximum_frames: u32,
    },
    StingerSourceIsLive {
        input: InputId,
    },
    Failed,
}

impl fmt::Display for NativeSourcePlaybackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => error.fmt(formatter),
            Self::Decode { input, error } => {
                write!(
                    formatter,
                    "source {input} video refill decode failed: {error}"
                )
            }
            Self::DecodeContract { input } => {
                write!(
                    formatter,
                    "source {input} video refill violated the decode contract"
                )
            }
            Self::Normalize { input, error } => {
                write!(
                    formatter,
                    "source {input} native refill normalization failed: {error}"
                )
            }
            Self::WorkerDisconnected => {
                formatter.write_str("native source decode worker disconnected")
            }
            Self::WorkerPanicked => formatter.write_str("native source decode worker panicked"),
            Self::WorkerQueueFull => formatter
                .write_str("native source decode worker request queue is unexpectedly full"),
            Self::SourceNotLive { input } => {
                write!(formatter, "source {input} is not a live video source")
            }
            Self::MissingSource { input } => {
                write!(formatter, "source {input} is not registered")
            }
            Self::StingerSourceNotFullyPreloaded {
                input,
                retained_frames,
                maximum_frames,
            } => write!(
                formatter,
                "Stinger source {input} did not reach end of stream within {retained_frames} retained frames; maximum is {maximum_frames}"
            ),
            Self::StingerSourceIsLive { input } => {
                write!(formatter, "Stinger source {input} is a live video source")
            }
            Self::Failed => formatter.write_str("native source playback previously failed"),
        }
    }
}

impl Error for NativeSourcePlaybackError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Decode { error, .. } => Some(error),
            Self::Normalize { error, .. } => Some(error),
            Self::DecodeContract { .. }
            | Self::WorkerDisconnected
            | Self::WorkerPanicked
            | Self::WorkerQueueFull
            | Self::SourceNotLive { .. }
            | Self::MissingSource { .. }
            | Self::StingerSourceNotFullyPreloaded { .. }
            | Self::StingerSourceIsLive { .. }
            | Self::Failed => None,
        }
    }
}

impl From<NativeSourceError> for NativeSourcePlaybackError {
    fn from(value: NativeSourceError) -> Self {
        Self::Source(value)
    }
}

/// Render failures against an authoritative source-ring registry.
#[derive(Debug)]
pub enum NativeSourceRenderError {
    MissingSource {
        input: InputId,
    },
    DimensionMismatch {
        input: InputId,
        expected_width: u32,
        expected_height: u32,
        actual_width: u32,
        actual_height: u32,
    },
    MissingTransitionKind,
    MissingStingerConfiguration(fm_switcher::StingerSlotId),
    StingerSourceNotPreloaded {
        input: InputId,
    },
    UnsupportedTransition(SwitcherTransitionKind),
    InvalidMix(TransitionError),
    InvalidStinger(StingerPlanError),
    ResourceBounds,
    ConcurrentProjectRender,
    Completion(NativeGpuError),
    SceneCompositor(NativeCompositionError),
    Compositor(NativeTransitionError),
    Stinger(NativeStingerError),
    InvalidFadeToBlack(FadeToBlackPlanError),
    FadeToBlack(NativeFadeToBlackError),
}

impl fmt::Display for NativeSourceRenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSource { input } => write!(formatter, "source {input} is not registered"),
            Self::DimensionMismatch {
                input,
                expected_width,
                expected_height,
                actual_width,
                actual_height,
            } => write!(
                formatter,
                "source {input} texture dimensions {actual_width}x{actual_height} do not match registry dimensions {expected_width}x{expected_height}"
            ),
            Self::MissingTransitionKind => {
                formatter.write_str("program-frame transition kind is missing")
            }
            Self::MissingStingerConfiguration(slot) => {
                write!(
                    formatter,
                    "native project has no configuration for Stinger slot {}",
                    slot.number()
                )
            }
            Self::StingerSourceNotPreloaded { input } => {
                write!(
                    formatter,
                    "native Stinger source {input} is not pinned for clip-local playback"
                )
            }
            Self::UnsupportedTransition(kind) => {
                write!(formatter, "native transition {kind:?} is not supported")
            }
            Self::InvalidMix(error) => write!(formatter, "program-frame mix is invalid: {error}"),
            Self::InvalidStinger(error) => {
                write!(formatter, "program-frame Stinger is invalid: {error}")
            }
            Self::ResourceBounds => {
                formatter.write_str("native scene execution exceeded planned resource bounds")
            }
            Self::ConcurrentProjectRender => {
                formatter.write_str("native project rendering is already in progress")
            }
            Self::Completion(error) => {
                write!(formatter, "native project completion failed: {error}")
            }
            Self::SceneCompositor(error) => {
                write!(formatter, "native scene composition failed: {error}")
            }
            Self::Compositor(error) => write!(formatter, "native composition failed: {error}"),
            Self::Stinger(error) => write!(formatter, "native Stinger rendering failed: {error}"),
            Self::InvalidFadeToBlack(error) => {
                write!(formatter, "native Fade-to-Black plan is invalid: {error}")
            }
            Self::FadeToBlack(error) => {
                write!(formatter, "native Fade-to-Black rendering failed: {error}")
            }
        }
    }
}

impl Error for NativeSourceRenderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidMix(error) => Some(error),
            Self::InvalidStinger(error) => Some(error),
            Self::Completion(error) => Some(error),
            Self::SceneCompositor(error) => Some(error),
            Self::Compositor(error) => Some(error),
            Self::Stinger(error) => Some(error),
            Self::InvalidFadeToBlack(error) => Some(error),
            Self::FadeToBlack(error) => Some(error),
            Self::MissingSource { .. }
            | Self::DimensionMismatch { .. }
            | Self::MissingTransitionKind
            | Self::MissingStingerConfiguration(_)
            | Self::StingerSourceNotPreloaded { .. }
            | Self::ResourceBounds
            | Self::ConcurrentProjectRender
            | Self::UnsupportedTransition(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeVideoSourceKind {
    Decoded,
    Retained,
    Live,
}

struct NativeVideoPrefix {
    frames: Vec<NativeWorkingFrame>,
    offsets_ns: Vec<u64>,
    source_pts_origin: i64,
    last_source_pts: i64,
    last_sequence: u64,
    clock_domain: ClockDomainId,
    kind: NativeVideoSourceKind,
    end_of_stream: bool,
    in_flight: Option<NativeDecodeRequest>,
    available_for_stinger: bool,
    pinned_for_stinger: bool,
}

/// Bounded GPU-resident video prefixes keyed by full-width input identity.
/// Textures are retained once and selected without cloning or re-uploading.
pub struct NativeSourceRegistry {
    sources: BTreeMap<InputId, NativeVideoPrefix>,
    dimensions: Option<(u32, u32)>,
    retained_rgba16f_bytes: u64,
    limits: NativeSourceLimits,
}

impl NativeSourceRegistry {
    #[must_use]
    pub fn len(&self) -> usize {
        self.sources.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    #[must_use]
    pub const fn dimensions(&self) -> Option<(u32, u32)> {
        self.dimensions
    }

    #[must_use]
    pub const fn retained_rgba16f_bytes(&self) -> u64 {
        self.retained_rgba16f_bytes
    }

    /// Iterates full-width source IDs in deterministic map order.
    #[must_use]
    pub fn inputs(&self) -> impl ExactSizeIterator<Item = InputId> + '_ {
        self.sources.keys().copied()
    }

    /// Returns selected source timing without exposing its texture.
    #[must_use]
    pub fn timing_at_deadline(&self, input: InputId, deadline: ClockTime) -> Option<MediaTiming> {
        self.sources
            .get(&input)
            .and_then(|prefix| prefix.frame_at_deadline(deadline))
            .map(NativeWorkingFrame::timing)
    }

    #[must_use]
    pub fn contains(&self, input: InputId) -> bool {
        self.sources.contains_key(&input)
    }
}

impl NativeVideoPrefix {
    fn frame_at_deadline(&self, deadline: ClockTime) -> Option<&NativeWorkingFrame> {
        if self.kind == NativeVideoSourceKind::Live {
            return self.frames.last();
        }
        if !source_covers_deadline(
            self.offsets_ns.last().copied(),
            self.end_of_stream,
            deadline,
        ) {
            return None;
        }
        frame_index_at_deadline(&self.offsets_ns, deadline.as_nanos())
            .and_then(|index| self.frames.get(index))
    }

    fn covers_deadline(&self, deadline: ClockTime) -> bool {
        if self.kind == NativeVideoSourceKind::Live {
            !self.frames.is_empty()
        } else {
            self.in_flight
                .is_none_or(|request| request.operation != NativeDecodeOperation::Restart)
                && self
                    .offsets_ns
                    .first()
                    .is_some_and(|first| *first <= deadline.as_nanos())
                && source_covers_deadline(
                    self.offsets_ns.last().copied(),
                    self.end_of_stream,
                    deadline,
                )
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeDecodeOperation {
    Continue,
    Restart,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeDecodeRequest {
    input: InputId,
    count: NonZeroU32,
    operation: NativeDecodeOperation,
}

#[derive(Debug)]
struct NativeDecodeResult {
    request: NativeDecodeRequest,
    window: Result<DecodedVideoWindow, fm_codec_ffmpeg::Error>,
}

struct NativeDecodeWorker {
    requests: Option<SyncSender<NativeDecodeRequest>>,
    results: Receiver<NativeDecodeResult>,
    thread: Option<thread::JoinHandle<()>>,
}

impl NativeDecodeWorker {
    fn spawn(
        mut decoders: BTreeMap<InputId, LocalVideoDecoder>,
    ) -> Result<Self, NativeSourcePreflightError> {
        let capacity = decoders.len().max(1);
        let (request_sender, request_receiver) =
            mpsc::sync_channel::<NativeDecodeRequest>(capacity);
        let (result_sender, result_receiver) = mpsc::sync_channel::<NativeDecodeResult>(capacity);
        let worker = thread::Builder::new()
            .name("freemix-native-source-decode".to_owned())
            .spawn(move || {
                while let Ok(request) = request_receiver.recv() {
                    let window = decoders
                        .get_mut(&request.input)
                        .ok_or(fm_codec_ffmpeg::Error::InvalidConfig)
                        .and_then(|decoder| {
                            if request.operation == NativeDecodeOperation::Restart {
                                decoder.restart();
                            }
                            decoder.decode_up_to(request.count)
                        });
                    if result_sender
                        .send(NativeDecodeResult { request, window })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .map_err(|_| NativeSourcePreflightError::WorkerUnavailable)?;
        Ok(Self {
            requests: Some(request_sender),
            results: result_receiver,
            thread: Some(worker),
        })
    }

    fn disconnected_error(&mut self) -> NativeSourcePlaybackError {
        self.requests.take();
        match self.thread.take().map(thread::JoinHandle::join) {
            Some(Err(_)) => NativeSourcePlaybackError::WorkerPanicked,
            Some(Ok(())) | None => NativeSourcePlaybackError::WorkerDisconnected,
        }
    }
}

impl Drop for NativeDecodeWorker {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(worker) = self.thread.take() {
            let _ = worker.join();
        }
    }
}

/// Bounded GPU source rings paired with one CPU-only background decode worker.
pub struct NativeSourcePlayback {
    registry: NativeSourceRegistry,
    worker: NativeDecodeWorker,
    failed: bool,
}

impl NativeSourcePlayback {
    /// Returns the immutable registry used by the existing render API.
    #[must_use]
    pub const fn registry(&self) -> &NativeSourceRegistry {
        &self.registry
    }

    /// Stops the decode worker and consumes playback into its current registry.
    #[must_use]
    pub fn into_registry(self) -> NativeSourceRegistry {
        let Self {
            registry, worker, ..
        } = self;
        drop(worker);
        registry
    }

    /// Pins one fully decoded source for clip-local Stinger playback.
    ///
    /// Pinned frames are not evicted by the global input timeline, so repeated
    /// Stinger triggers can restart from local frame zero without decode or
    /// upload work. A decoded source must have reached EOS during bounded
    /// preflight; live sources cannot provide deterministic retriggering.
    ///
    /// # Errors
    ///
    /// Returns a typed error for failed playback, a missing source, a live
    /// source, or a local video longer than the configured GPU prefix bound.
    pub fn pin_stinger_source(&mut self, input: InputId) -> Result<(), NativeSourcePlaybackError> {
        if self.failed {
            return Err(NativeSourcePlaybackError::Failed);
        }
        let maximum_frames = self.registry.limits.max_video_frames_per_source.get();
        let prefix = self
            .registry
            .sources
            .get_mut(&input)
            .ok_or(NativeSourcePlaybackError::MissingSource { input })?;
        if prefix.kind == NativeVideoSourceKind::Live {
            return Err(NativeSourcePlaybackError::StingerSourceIsLive { input });
        }
        if prefix.kind == NativeVideoSourceKind::Decoded && !prefix.end_of_stream {
            return Err(NativeSourcePlaybackError::StingerSourceNotFullyPreloaded {
                input,
                retained_frames: prefix.frames.len(),
                maximum_frames,
            });
        }
        prefix.available_for_stinger = true;
        prefix.pinned_for_stinger = true;
        Ok(())
    }

    /// Enables one decoded or retained source for an independently serviced
    /// clip-local Stinger ring without pinning its frames.
    ///
    /// # Errors
    ///
    /// Returns a typed failed-playback, missing-source, or live-source error.
    pub fn enable_stinger_source(
        &mut self,
        input: InputId,
    ) -> Result<(), NativeSourcePlaybackError> {
        if self.failed {
            return Err(NativeSourcePlaybackError::Failed);
        }
        let prefix = self
            .registry
            .sources
            .get_mut(&input)
            .ok_or(NativeSourcePlaybackError::MissingSource { input })?;
        if prefix.kind == NativeVideoSourceKind::Live {
            return Err(NativeSourcePlaybackError::StingerSourceIsLive { input });
        }
        prefix.available_for_stinger = true;
        Ok(())
    }
}

/// Resource bounds for the independent CPU audio runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeAudioLimits {
    pub max_blocks_per_source: NonZeroU32,
    pub max_blocks_per_page: NonZeroU32,
    pub max_samples_per_page: usize,
    pub max_retained_blocks: usize,
    pub max_retained_samples: usize,
    pub max_retained_bytes: usize,
    pub max_position_blocks: usize,
    pub max_leading_silence_samples: u64,
    pub sink_blocks: usize,
}

impl Default for NativeAudioLimits {
    fn default() -> Self {
        Self {
            max_blocks_per_source: NonZeroU32::new(32).expect("32 is nonzero"),
            max_blocks_per_page: NonZeroU32::new(16).expect("16 is nonzero"),
            max_samples_per_page: 64 * 1024,
            max_retained_blocks: 1024,
            max_retained_samples: 1024 * 1024,
            max_retained_bytes: 32 * 1024 * 1024,
            max_position_blocks: 4_096,
            max_leading_silence_samples: 48_000 * 60,
            sink_blocks: 8,
        }
    }
}

fn partition_native_audio_limits(
    limits: NativeAudioLimits,
    has_stinger_audio: bool,
) -> Result<(NativeAudioLimits, NativeAudioLimits), NativeMasterError> {
    if !has_stinger_audio {
        return Ok((limits, limits));
    }
    let ordinary = NativeAudioLimits {
        max_retained_blocks: limits.max_retained_blocks / 2,
        max_retained_samples: limits.max_retained_samples / 2,
        max_retained_bytes: limits.max_retained_bytes / 2,
        ..limits
    };
    let stinger = NativeAudioLimits {
        max_retained_blocks: limits
            .max_retained_blocks
            .checked_sub(ordinary.max_retained_blocks)
            .ok_or(NativeMasterError::InvalidLimits)?,
        max_retained_samples: limits
            .max_retained_samples
            .checked_sub(ordinary.max_retained_samples)
            .ok_or(NativeMasterError::InvalidLimits)?,
        max_retained_bytes: limits
            .max_retained_bytes
            .checked_sub(ordinary.max_retained_bytes)
            .ok_or(NativeMasterError::InvalidLimits)?,
        ..limits
    };
    Ok((ordinary, stinger))
}

fn partition_stinger_audio_limits(
    limits: NativeAudioLimits,
    source_count: usize,
) -> Result<NativeAudioLimits, NativeMasterError> {
    if source_count == 0 {
        return Err(NativeMasterError::InvalidLimits);
    }
    Ok(NativeAudioLimits {
        max_retained_blocks: limits.max_retained_blocks / source_count,
        max_retained_samples: limits.max_retained_samples / source_count,
        max_retained_bytes: limits.max_retained_bytes / source_count,
        sink_blocks: 1,
        ..limits
    })
}

fn native_source_has_audio(
    adapter: Option<&Adapter>,
    source: &NativeResolvedSource,
) -> Result<bool, NativeMasterError> {
    let NativeResolvedSource::LocalVideo { input, path } = source else {
        return Ok(false);
    };
    let adapter = adapter.ok_or(NativeMasterError::InvalidFormat)?;
    let probe = adapter
        .probe_local(path)
        .map_err(|error| NativeMasterError::Ffmpeg {
            input: *input,
            error,
        })?;
    Ok(probe
        .streams
        .iter()
        .any(|stream| matches!(stream.kind, StreamKind::Audio)))
}

/// Fatal setup, decode-contract, or render failures in native CPU audio.
#[derive(Debug)]
pub enum NativeMasterError {
    Ffmpeg {
        input: InputId,
        error: fm_codec_ffmpeg::Error,
    },
    Audio(fm_audio::AudioError),
    ChannelMapping(fm_audio::ChannelMappingError),
    Synchronizer(fm_audio::AudioSynchronizerError),
    AudioBlock(fm_frame::AudioBlockError),
    Timing(TimingError),
    SinkConfig(SinkConfigError),
    InvalidLimits,
    InvalidFormat,
    InvalidTimeline {
        input: InputId,
    },
    BoundsExceeded,
    WorkerUnavailable,
    WorkerDisconnected,
    WorkerPanicked,
    WorkerQueueFull,
    DecodeContract {
        input: InputId,
    },
    MissingAudioRoute {
        input: InputId,
    },
    MissingAudioTransitionKind,
    MissingStingerConfiguration(fm_switcher::StingerSlotId),
    InvalidStinger(StingerPlanError),
    UnsupportedAudioTransition(SwitcherTransitionKind),
    UnexpectedFrame {
        expected: u64,
        actual: u64,
    },
    FrameNotReady(u64),
    SinkRejected,
    Failed,
}

impl fmt::Display for NativeMasterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ffmpeg { input, error } => {
                write!(
                    formatter,
                    "source {input} native audio decode failed: {error}"
                )
            }
            Self::Audio(error) => write!(formatter, "native Master mix failed: {error}"),
            Self::ChannelMapping(error) => {
                write!(formatter, "native audio channel mapping failed: {error}")
            }
            Self::Synchronizer(error) => {
                write!(formatter, "native audio synchronization failed: {error}")
            }
            Self::AudioBlock(error) => write!(formatter, "native audio block failed: {error}"),
            Self::Timing(error) => write!(formatter, "native audio timing failed: {error}"),
            Self::SinkConfig(error) => write!(formatter, "native fake audio sink failed: {error}"),
            Self::InvalidLimits => formatter.write_str("native audio limits are invalid"),
            Self::InvalidFormat => formatter
                .write_str("native audio requires planar F32 with a compatible channel layout"),
            Self::InvalidTimeline { input } => {
                write!(formatter, "source {input} has an invalid audio timeline")
            }
            Self::BoundsExceeded => {
                formatter.write_str("native audio retained resource bounds were exceeded")
            }
            Self::WorkerUnavailable => {
                formatter.write_str("native audio decode worker could not be started")
            }
            Self::WorkerDisconnected => {
                formatter.write_str("native audio decode worker disconnected")
            }
            Self::WorkerPanicked => formatter.write_str("native audio decode worker panicked"),
            Self::WorkerQueueFull => {
                formatter.write_str("native audio decode worker queue is unexpectedly full")
            }
            Self::DecodeContract { input } => {
                write!(
                    formatter,
                    "source {input} violated the native audio decode contract"
                )
            }
            Self::MissingAudioRoute { input } => {
                write!(formatter, "input {input} has no native project audio route")
            }
            Self::MissingAudioTransitionKind => {
                formatter.write_str("native audio transition kind is missing")
            }
            Self::MissingStingerConfiguration(slot) => {
                write!(
                    formatter,
                    "native project has no audio configuration for Stinger slot {}",
                    slot.number()
                )
            }
            Self::InvalidStinger(error) => {
                write!(formatter, "native Stinger audio plan is invalid: {error}")
            }
            Self::UnsupportedAudioTransition(kind) => {
                write!(
                    formatter,
                    "native audio transition {kind:?} is not supported"
                )
            }
            Self::UnexpectedFrame { expected, actual } => write!(
                formatter,
                "native audio expected frame {expected}, received {actual}"
            ),
            Self::FrameNotReady(frame) => {
                write!(
                    formatter,
                    "native audio frame {frame} was not serviced before render"
                )
            }
            Self::SinkRejected => formatter.write_str("native fake audio sink rejected a block"),
            Self::Failed => formatter.write_str("native audio runtime previously failed"),
        }
    }
}

impl Error for NativeMasterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Ffmpeg { error, .. } => Some(error),
            Self::Audio(error) => Some(error),
            Self::ChannelMapping(error) => Some(error),
            Self::Synchronizer(error) => Some(error),
            Self::AudioBlock(error) => Some(error),
            Self::Timing(error) => Some(error),
            Self::SinkConfig(error) => Some(error),
            Self::InvalidStinger(error) => Some(error),
            _ => None,
        }
    }
}

impl From<fm_audio::AudioError> for NativeMasterError {
    fn from(value: fm_audio::AudioError) -> Self {
        Self::Audio(value)
    }
}

impl From<fm_audio::ChannelMappingError> for NativeMasterError {
    fn from(value: fm_audio::ChannelMappingError) -> Self {
        Self::ChannelMapping(value)
    }
}

impl From<fm_audio::AudioSynchronizerError> for NativeMasterError {
    fn from(value: fm_audio::AudioSynchronizerError) -> Self {
        Self::Synchronizer(value)
    }
}

impl From<fm_frame::AudioBlockError> for NativeMasterError {
    fn from(value: fm_frame::AudioBlockError) -> Self {
        Self::AudioBlock(value)
    }
}

impl From<TimingError> for NativeMasterError {
    fn from(value: TimingError) -> Self {
        Self::Timing(value)
    }
}

impl From<SinkConfigError> for NativeMasterError {
    fn from(value: SinkConfigError) -> Self {
        Self::SinkConfig(value)
    }
}

#[derive(Debug)]
struct NativeAudioSource {
    explicit_silence: bool,
    synchronizer: Option<ClockMappedAudioSynchronizer>,
    timeline_origin: AudioCadenceOrigin,
    next_sample: u64,
    next_sequence: u64,
    audio_start_master_sample: u64,
    end_of_stream: bool,
    in_flight: Option<NativeAudioReservation>,
    decode_generation: u64,
    restart_decode: bool,
    restart_sample: u64,
    restart_sequence: u64,
    restart_target_ordinal: usize,
}

impl NativeAudioSource {
    fn silence() -> Self {
        Self {
            explicit_silence: true,
            synchronizer: None,
            timeline_origin: AudioCadenceOrigin::new(NormalizedTimestamp::from_nanos(0), 0),
            next_sample: 0,
            next_sequence: 0,
            audio_start_master_sample: 0,
            end_of_stream: true,
            in_flight: None,
            decode_generation: 0,
            restart_decode: false,
            restart_sample: 0,
            restart_sequence: 0,
            restart_target_ordinal: 0,
        }
    }

    fn decoded(
        synchronizer: ClockMappedAudioSynchronizer,
        timeline_origin: AudioCadenceOrigin,
        page: ValidatedAudioPage,
        audio_start_master_sample: u64,
        end_of_stream: bool,
        restart_target_ordinal: usize,
        restart_sequence: u64,
    ) -> Self {
        let restart_sample = synchronizer.source_origin().sample_index();
        Self {
            explicit_silence: false,
            synchronizer: Some(synchronizer),
            timeline_origin,
            next_sample: page.next_sample,
            next_sequence: page.next_sequence,
            audio_start_master_sample,
            end_of_stream,
            in_flight: None,
            decode_generation: 0,
            restart_decode: false,
            restart_sample,
            restart_sequence,
            restart_target_ordinal,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct NativeAudioCharge {
    blocks: usize,
    samples: usize,
    bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeAudioReservation {
    count: NonZeroU32,
    charge: NativeAudioCharge,
    generation: u64,
}

/// Mutation-point pressure and alignment telemetry for native audio.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NativeAudioTelemetry {
    pub reservation_requests: u64,
    pub reserved_blocks: usize,
    pub reserved_samples: usize,
    pub reserved_bytes: usize,
    pub peak_reserved_blocks: usize,
    pub peak_reserved_samples: usize,
    pub peak_reserved_bytes: usize,
    pub source_stalls: u64,
    pub positioned_blocks: u64,
    pub positioned_samples: u64,
    pub leading_silence_samples: u64,
    pub eos_padding_blocks: u64,
    pub eos_padding_samples: u64,
    pub peak_retained_blocks: usize,
    pub peak_retained_samples: usize,
    pub peak_retained_bytes: usize,
}

impl NativeAudioTelemetry {
    fn merge(&mut self, other: Self) {
        self.reservation_requests = self
            .reservation_requests
            .saturating_add(other.reservation_requests);
        self.reserved_blocks = self.reserved_blocks.saturating_add(other.reserved_blocks);
        self.reserved_samples = self.reserved_samples.saturating_add(other.reserved_samples);
        self.reserved_bytes = self.reserved_bytes.saturating_add(other.reserved_bytes);
        self.peak_reserved_blocks = self
            .peak_reserved_blocks
            .saturating_add(other.peak_reserved_blocks);
        self.peak_reserved_samples = self
            .peak_reserved_samples
            .saturating_add(other.peak_reserved_samples);
        self.peak_reserved_bytes = self
            .peak_reserved_bytes
            .saturating_add(other.peak_reserved_bytes);
        self.source_stalls = self.source_stalls.saturating_add(other.source_stalls);
        self.positioned_blocks = self
            .positioned_blocks
            .saturating_add(other.positioned_blocks);
        self.positioned_samples = self
            .positioned_samples
            .saturating_add(other.positioned_samples);
        self.leading_silence_samples = self
            .leading_silence_samples
            .saturating_add(other.leading_silence_samples);
        self.eos_padding_blocks = self
            .eos_padding_blocks
            .saturating_add(other.eos_padding_blocks);
        self.eos_padding_samples = self
            .eos_padding_samples
            .saturating_add(other.eos_padding_samples);
        self.peak_retained_blocks = self
            .peak_retained_blocks
            .saturating_add(other.peak_retained_blocks);
        self.peak_retained_samples = self
            .peak_retained_samples
            .saturating_add(other.peak_retained_samples);
        self.peak_retained_bytes = self
            .peak_retained_bytes
            .saturating_add(other.peak_retained_bytes);
    }
}

struct NativeAudioScratch {
    rendered: BTreeMap<InputId, Vec<Vec<f32>>>,
    mix: Vec<Vec<f32>>,
    plans: Vec<(InputId, AudioRenderPlan)>,
    uncovered: Vec<InputId>,
    padding_requests: Vec<(InputId, u64)>,
    padding_spans: Vec<AudioSilenceSpan>,
    padding_sources: Vec<StagedAudioPadding>,
    completed: Vec<NativeAudioDecodeResult>,
    validated: Vec<ValidatedCompletedAudioPage>,
    inputs: Vec<InputId>,
}

struct StagedAudioPadding {
    input: InputId,
    span_start: usize,
    span_end: usize,
    page: ValidatedAudioPage,
}

struct ValidatedCompletedAudioPage {
    input: InputId,
    reservation: NativeAudioReservation,
    window: DecodedAudioWindow,
    page: ValidatedAudioPage,
}

impl NativeAudioScratch {
    fn new(
        channels: usize,
        samples: usize,
        sources: &BTreeMap<InputId, NativeAudioSource>,
        padding_blocks: usize,
    ) -> Self {
        let rendered = sources
            .iter()
            .filter_map(|(&input, source)| {
                source.synchronizer.as_ref().map(|synchronizer| {
                    (
                        input,
                        vec![vec![0.0; samples]; synchronizer.channel_layout().channels().len()],
                    )
                })
            })
            .collect();
        let source_count = sources.len();
        Self {
            rendered,
            mix: vec![vec![0.0; samples]; channels],
            plans: Vec::with_capacity(source_count),
            uncovered: Vec::with_capacity(source_count),
            padding_requests: Vec::with_capacity(source_count),
            padding_spans: Vec::with_capacity(padding_blocks),
            padding_sources: Vec::with_capacity(source_count),
            completed: Vec::with_capacity(source_count),
            validated: Vec::with_capacity(source_count),
            inputs: Vec::with_capacity(source_count),
        }
    }
}

impl NativeAudioCharge {
    fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            blocks: self.blocks.checked_add(other.blocks)?,
            samples: self.samples.checked_add(other.samples)?,
            bytes: self.bytes.checked_add(other.bytes)?,
        })
    }

    fn checked_sub(self, other: Self) -> Option<Self> {
        Some(Self {
            blocks: self.blocks.checked_sub(other.blocks)?,
            samples: self.samples.checked_sub(other.samples)?,
            bytes: self.bytes.checked_sub(other.bytes)?,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct NativeAudioDecodeRequest {
    input: InputId,
    count: NonZeroU32,
    max_samples: usize,
    max_bytes: usize,
    generation: u64,
    restart: bool,
    restart_target_ordinal: usize,
    max_position_blocks: usize,
}

#[derive(Debug)]
struct NativeAudioDecodeResult {
    input: InputId,
    reservation: NativeAudioReservation,
    positioned_blocks: u64,
    positioned_samples: u64,
    window: Result<DecodedAudioWindow, fm_codec_ffmpeg::Error>,
}

struct NativeAudioDecodeWorker {
    requests: Option<SyncSender<NativeAudioDecodeRequest>>,
    results: Receiver<NativeAudioDecodeResult>,
    thread: Option<thread::JoinHandle<()>>,
}

impl NativeAudioDecodeWorker {
    fn spawn(
        mut decoders: BTreeMap<InputId, LocalAudioDecoder>,
    ) -> Result<Self, NativeMasterError> {
        let capacity = decoders.len().max(1);
        let (request_sender, request_receiver) =
            mpsc::sync_channel::<NativeAudioDecodeRequest>(capacity);
        let (result_sender, result_receiver) =
            mpsc::sync_channel::<NativeAudioDecodeResult>(capacity);
        let worker = thread::Builder::new()
            .name("freemix-native-audio-decode".to_owned())
            .spawn(move || {
                while let Ok(request) = request_receiver.recv() {
                    let mut positioned_blocks = 0;
                    let mut positioned_samples = 0;
                    let window = decoders
                        .get_mut(&request.input)
                        .ok_or(fm_codec_ffmpeg::Error::InvalidConfig)
                        .and_then(|decoder| {
                            if request.restart {
                                decoder.restart();
                                let position = decoder.skip_complete_blocks_to_sample_bounded(
                                    request.restart_target_ordinal,
                                    request.max_position_blocks,
                                )?;
                                positioned_blocks =
                                    u64::try_from(position.skipped_blocks).unwrap_or(u64::MAX);
                                positioned_samples =
                                    u64::try_from(position.skipped_samples).unwrap_or(u64::MAX);
                            }
                            decoder.decode_up_to_bounded(
                                request.count,
                                request.max_samples,
                                request.max_bytes,
                            )
                        });
                    if result_sender
                        .send(NativeAudioDecodeResult {
                            input: request.input,
                            reservation: NativeAudioReservation {
                                count: request.count,
                                charge: NativeAudioCharge {
                                    blocks: usize::try_from(request.count.get())
                                        .unwrap_or(usize::MAX),
                                    samples: request.max_samples,
                                    bytes: request.max_bytes,
                                },
                                generation: request.generation,
                            },
                            positioned_blocks,
                            positioned_samples,
                            window,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .map_err(|_| NativeMasterError::WorkerUnavailable)?;
        Ok(Self {
            requests: Some(request_sender),
            results: result_receiver,
            thread: Some(worker),
        })
    }

    fn disconnected_error(&mut self) -> NativeMasterError {
        self.requests.take();
        match self.thread.take().map(thread::JoinHandle::join) {
            Some(Err(_)) => NativeMasterError::WorkerPanicked,
            Some(Ok(())) | None => NativeMasterError::WorkerDisconnected,
        }
    }
}

impl Drop for NativeAudioDecodeWorker {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(worker) = self.thread.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Clone, Copy)]
struct ValidatedAudioPage {
    next_sample: u64,
    next_sequence: u64,
    charge: NativeAudioCharge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeStingerAudioTrigger {
    slot: fm_switcher::StingerSlotId,
    media: InputId,
    cadence_origin_frame: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeStingerAudioRequest {
    trigger: NativeStingerAudioTrigger,
    frame_index: u32,
    policy: ModelStingerAudioPolicy,
}

struct NativeStingerAudioPlayback {
    masters: BTreeMap<InputId, Box<NativeMasterRuntime>>,
    silent_inputs: BTreeSet<InputId>,
    active_trigger: Option<NativeStingerAudioTrigger>,
    ready: Option<NativeStingerAudioRequest>,
}

/// Independent bounded CPU audio playback and Master mixing runtime.
///
/// It is intentionally separate from [`NativeSourceRegistry`]. Decoder work is
/// confined to its worker; [`Self::render_frame`] only coalesces retained data,
/// mixes one authoritative primary, and writes the bounded fake sink.
pub struct NativeMasterRuntime {
    format: AudioFormat,
    frame_rate: FrameRate,
    clock_domain: ClockDomainId,
    expected_next_frame: u64,
    cadence_origin_frame: u64,
    ready_frame: Option<(u64, u64, u64)>,
    mixer: MasterMixer,
    pending_mixer: MasterMixer,
    sink: CollectingAudioSink,
    sources: BTreeMap<InputId, NativeAudioSource>,
    worker: NativeAudioDecodeWorker,
    limits: NativeAudioLimits,
    scratch: NativeAudioScratch,
    audio_telemetry: NativeAudioTelemetry,
    stinger_audio: Option<NativeStingerAudioPlayback>,
    collect_output: bool,
    failed: bool,
}

impl NativeMasterRuntime {
    /// Probes local videos, decodes one bounded initial audio page where an
    /// audio stream exists, and configures all other sources as silence.
    ///
    /// # Errors
    ///
    /// Returns a path-free format, bound, probe, decode, timeline, mixer, sink,
    /// or worker setup failure.
    #[allow(clippy::too_many_lines)]
    pub fn preflight_local_blocking(
        adapter: Option<&Adapter>,
        resolved: &[NativeResolvedSource],
        format: AudioFormat,
        frame_rate: FrameRate,
        clock_domain: ClockDomainId,
        expected_next_frame: u64,
        limits: NativeAudioLimits,
    ) -> Result<Self, NativeMasterError> {
        Self::preflight_local_blocking_with_decoder_retention(
            adapter,
            resolved,
            format,
            frame_rate,
            clock_domain,
            expected_next_frame,
            limits,
            false,
        )
    }

    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    fn preflight_local_blocking_with_decoder_retention(
        adapter: Option<&Adapter>,
        resolved: &[NativeResolvedSource],
        format: AudioFormat,
        frame_rate: FrameRate,
        clock_domain: ClockDomainId,
        expected_next_frame: u64,
        limits: NativeAudioLimits,
        retain_eos_decoders: bool,
    ) -> Result<Self, NativeMasterError> {
        if format.sample_format != SampleFormat::F32 {
            return Err(NativeMasterError::InvalidFormat);
        }
        fm_audio::FrameSampleAllocator::new(format.sample_rate, frame_rate)?;
        let mut mixer = MasterMixer::new(format.clone())?;
        let mut sources = BTreeMap::new();
        let mut decoders = BTreeMap::new();
        let mut retained = NativeAudioCharge::default();
        let mut audio_telemetry = NativeAudioTelemetry::default();
        let source_slots = resolved.len().max(1);
        let output_samples = u128::from(format.sample_rate.hertz())
            .checked_mul(u128::from(frame_rate.denominator()))
            .ok_or(NativeMasterError::BoundsExceeded)?
            .div_ceil(u128::from(frame_rate.numerator()));
        let output_samples =
            usize::try_from(output_samples).map_err(|_| NativeMasterError::BoundsExceeded)?;
        validate_audio_limits(limits, format.channels.channels().len())?;
        let scratch_plane_sets = source_slots
            .checked_add(1)
            .ok_or(NativeMasterError::BoundsExceeded)?;
        let scratch_samples = output_samples
            .checked_mul(scratch_plane_sets)
            .ok_or(NativeMasterError::BoundsExceeded)?;
        let scratch_source_channels = source_slots
            .checked_mul(MAX_CHANNEL_MAPPING_CHANNELS)
            .ok_or(NativeMasterError::BoundsExceeded)?;
        let scratch_channels = scratch_source_channels
            .checked_add(format.channels.channels().len())
            .ok_or(NativeMasterError::BoundsExceeded)?;
        let scratch_bytes = audio_sample_bytes(output_samples, scratch_channels)?;
        let source_sample_capacity = limits
            .max_retained_samples
            .checked_sub(scratch_samples)
            .map(|samples| samples / source_slots)
            .filter(|samples| *samples != 0)
            .ok_or(NativeMasterError::InvalidLimits)?;
        let source_byte_capacity = limits
            .max_retained_bytes
            .checked_sub(scratch_bytes)
            .map(|bytes| bytes / source_slots)
            .filter(|bytes| *bytes != 0)
            .ok_or(NativeMasterError::InvalidLimits)?;
        let (master_start_sample, _) = absolute_frame_sample_span(
            expected_next_frame,
            format.sample_rate.hertz(),
            frame_rate,
        )?;
        let master_start_timestamp =
            normalized_sample_endpoint(i128::from(master_start_sample), format.sample_rate.hertz())
                .ok_or(NativeMasterError::BoundsExceeded)?;
        let mapping_domain = MappingClockDomainId::new(clock_domain.get());

        for source in resolved {
            let input = source.input();
            let NativeResolvedSource::LocalVideo { path, .. } = source else {
                mixer.add_input(
                    input,
                    format.clone(),
                    ChannelMapping::identity(format.channels.clone())?,
                    InputState {
                        follow_video: true,
                        ..InputState::default()
                    },
                )?;
                sources.insert(input, NativeAudioSource::silence());
                continue;
            };
            let adapter = adapter.ok_or(NativeMasterError::InvalidFormat)?;
            let probe = adapter
                .probe_local(path)
                .map_err(|error| NativeMasterError::Ffmpeg { input, error })?;
            if !probe
                .streams
                .iter()
                .any(|stream| matches!(stream.kind, StreamKind::Audio))
            {
                mixer.add_input(
                    input,
                    format.clone(),
                    ChannelMapping::identity(format.channels.clone())?,
                    InputState {
                        follow_video: true,
                        ..InputState::default()
                    },
                )?;
                sources.insert(input, NativeAudioSource::silence());
                continue;
            }
            let selected = probe
                .select_audio(StreamSelector::Best)
                .map_err(|error| NativeMasterError::Ffmpeg { input, error })?;
            if selected.channels != u32::try_from(format.channels.channels().len()).ok() {
                return Err(NativeMasterError::InvalidFormat);
            }
            let mut decoder = adapter
                .open_local_audio(path, clock_domain, StreamSelector::Best)
                .map_err(|error| NativeMasterError::Ffmpeg { input, error })?;
            let first_page_bytes = audio_sample_bytes(
                limits.max_samples_per_page,
                format.channels.channels().len(),
            )?;
            let first_window = decoder
                .decode_up_to_bounded(
                    NonZeroU32::MIN,
                    limits.max_samples_per_page,
                    first_page_bytes,
                )
                .map_err(|error| NativeMasterError::Ffmpeg { input, error })?;
            validate_audio_window_contract(input, &first_window, NonZeroU32::MIN)?;
            let Some(first_audio) = first_window.blocks.first() else {
                if !first_window.end_of_stream {
                    return Err(NativeMasterError::DecodeContract { input });
                }
                mixer.add_input(
                    input,
                    format.clone(),
                    ChannelMapping::identity(format.channels.clone())?,
                    InputState {
                        follow_video: true,
                        ..InputState::default()
                    },
                )?;
                sources.insert(input, NativeAudioSource::silence());
                continue;
            };
            let mut video_decoder = adapter
                .open_local_video(path, clock_domain, StreamSelector::Best)
                .map_err(|error| NativeMasterError::Ffmpeg { input, error })?;
            let first_video = video_decoder
                .decode_up_to(NonZeroU32::MIN)
                .map_err(|error| NativeMasterError::Ffmpeg { input, error })?;
            let first_video_pts = first_video
                .frames
                .first()
                .map(|frame| frame.timing().presentation_timestamp().as_nanos())
                .ok_or(NativeMasterError::InvalidTimeline { input })?;
            if first_audio.timing().clock_domain() != clock_domain {
                return Err(NativeMasterError::InvalidTimeline { input });
            }
            let media_audio_origin_timestamp = first_audio.timing().presentation_timestamp();
            let source_rate = first_audio.sample_rate();
            let media_audio_phase =
                original_timestamp_samples(first_audio.timing(), source_rate.hertz())
                    .and_then(|sample| {
                        u64::try_from(sample.rem_euclid(i128::from(source_rate.hertz()))).ok()
                    })
                    .ok_or(NativeMasterError::InvalidTimeline { input })?;
            let media_audio_origin =
                AudioCadenceOrigin::new(media_audio_origin_timestamp, media_audio_phase);
            let source_layout = first_audio.channel_layout().clone();
            let audio_offset = media_audio_origin_timestamp
                .as_nanos()
                .checked_sub(first_video_pts)
                .ok_or(NativeMasterError::InvalidTimeline { input })?;
            let audio_start_master_sample = if audio_offset <= 0 {
                0
            } else {
                cadence_sample_at_or_after(audio_offset, format.sample_rate.hertz())?
            };
            if audio_start_master_sample > limits.max_leading_silence_samples {
                return Err(NativeMasterError::BoundsExceeded);
            }
            let target_source_timestamp = first_video_pts
                .checked_add(master_start_timestamp)
                .ok_or(NativeMasterError::InvalidTimeline { input })?
                .max(media_audio_origin_timestamp.as_nanos());
            let target_source_sample = cadence_sample_at_timestamp(
                media_audio_origin,
                target_source_timestamp,
                source_rate.hertz(),
            )?;
            let target_ordinal = target_source_sample
                .checked_sub(media_audio_phase)
                .and_then(|sample| usize::try_from(sample).ok())
                .ok_or(NativeMasterError::InvalidTimeline { input })?;
            let first_samples = first_audio.sample_count();
            let mut positioned_end_of_stream = first_window.end_of_stream;
            let mut positioned_next_sequence = first_audio
                .timing()
                .sequence()
                .get()
                .checked_add(1)
                .ok_or(NativeMasterError::InvalidTimeline { input })?;
            let window = if target_ordinal < first_samples {
                first_window
            } else {
                let position = decoder
                    .skip_complete_blocks_to_sample_bounded(
                        target_ordinal,
                        limits.max_position_blocks,
                    )
                    .map_err(|error| NativeMasterError::Ffmpeg { input, error })?;
                audio_telemetry.positioned_blocks = audio_telemetry
                    .positioned_blocks
                    .saturating_add(u64::try_from(position.skipped_blocks).unwrap_or(u64::MAX));
                audio_telemetry.positioned_samples = audio_telemetry
                    .positioned_samples
                    .saturating_add(u64::try_from(position.skipped_samples).unwrap_or(u64::MAX));
                positioned_end_of_stream = position.end_of_stream;
                positioned_next_sequence = u64::try_from(position.next_block)
                    .map_err(|_| NativeMasterError::InvalidTimeline { input })?;
                if position.end_of_stream {
                    DecodedAudioWindow {
                        blocks: Vec::new(),
                        end_of_stream: true,
                    }
                } else {
                    let page = decoder
                        .decode_up_to_bounded(
                            limits.max_blocks_per_page,
                            limits.max_samples_per_page,
                            first_page_bytes,
                        )
                        .map_err(|error| NativeMasterError::Ffmpeg { input, error })?;
                    validate_audio_window_contract(input, &page, limits.max_blocks_per_page)?;
                    page
                }
            };
            let sync_master_sample = master_start_sample.max(audio_start_master_sample);
            let sync_master_timestamp = normalized_sample_endpoint(
                i128::from(sync_master_sample),
                format.sample_rate.hertz(),
            )
            .ok_or(NativeMasterError::BoundsExceeded)?;
            let (source_anchor, master_anchor) = nonnegative_mapping_anchors(first_video_pts, 0)?;
            let mapping = ClockMapping::new(
                ClockSnapshot::new(mapping_domain, ClockTime::from_nanos(source_anchor)),
                ClockSnapshot::new(mapping_domain, ClockTime::from_nanos(master_anchor)),
                0,
            )
            .map_err(|_| NativeMasterError::InvalidTimeline { input })?;
            let synchronizer_limits = AudioSynchronizerLimits::new(
                usize::try_from(limits.max_blocks_per_source.get()).unwrap_or(usize::MAX),
                source_sample_capacity,
                source_byte_capacity,
                output_samples,
            )?;
            let sync_source_origin = if let Some(block) = window.blocks.first() {
                AudioCadenceOrigin::new(
                    block.timing().presentation_timestamp(),
                    cadence_sample_at_timestamp(
                        media_audio_origin,
                        block.timing().presentation_timestamp().as_nanos(),
                        source_rate.hertz(),
                    )?,
                )
            } else {
                AudioCadenceOrigin::new(
                    cadence_timestamp_at_sample(
                        media_audio_origin,
                        target_source_sample,
                        source_rate.hertz(),
                    )?,
                    target_source_sample,
                )
            };
            let mut synchronizer = ClockMappedAudioSynchronizer::new(
                source_rate,
                format.sample_rate,
                source_layout.clone(),
                mapping,
                sync_source_origin,
                AudioCadenceOrigin::new(
                    NormalizedTimestamp::from_nanos(sync_master_timestamp),
                    sync_master_sample,
                ),
                synchronizer_limits,
            )?;
            synchronizer.preflight_push_batch(&window.blocks)?;
            synchronizer.push_batch(&window.blocks)?;
            let page = if window.blocks.is_empty() {
                ValidatedAudioPage {
                    next_sample: synchronizer.source_origin().sample_index(),
                    next_sequence: positioned_next_sequence,
                    charge: NativeAudioCharge::default(),
                }
            } else {
                validate_initial_audio_page(
                    input,
                    &window.blocks,
                    media_audio_origin,
                    synchronizer.source_origin().sample_index(),
                    clock_domain,
                )?
            };
            validate_page_bounds(&page, limits)?;
            retained = retained
                .checked_add(page.charge)
                .ok_or(NativeMasterError::BoundsExceeded)?;
            validate_retained_bounds(retained, limits)?;
            let mixer_format = AudioFormat {
                sample_rate: format.sample_rate,
                sample_format: SampleFormat::F32,
                channels: source_layout.clone(),
            };
            mixer.add_input(
                input,
                mixer_format,
                ChannelMapping::matching(source_layout.clone(), format.channels.clone())?,
                InputState {
                    follow_video: true,
                    ..InputState::default()
                },
            )?;
            sources.insert(
                input,
                NativeAudioSource::decoded(
                    synchronizer,
                    media_audio_origin,
                    page,
                    audio_start_master_sample,
                    window.end_of_stream || positioned_end_of_stream,
                    target_ordinal,
                    window
                        .blocks
                        .first()
                        .map_or(positioned_next_sequence, |block| {
                            block.timing().sequence().get()
                        }),
                ),
            );
            if retain_eos_decoders || !window.end_of_stream {
                decoders.insert(input, decoder);
            }
        }

        let worker = NativeAudioDecodeWorker::spawn(decoders)?;
        let sink = CollectingAudioSink::new(limits.sink_blocks, OverflowPolicy::DropOldest)?;
        observe_retained_peak(&mut audio_telemetry, retained);
        let pending_mixer = mixer.clone();
        let scratch = NativeAudioScratch::new(
            format.channels.channels().len(),
            output_samples,
            &sources,
            limits.max_retained_blocks,
        );
        Ok(Self {
            format,
            frame_rate,
            clock_domain,
            expected_next_frame,
            cadence_origin_frame: 0,
            ready_frame: None,
            mixer,
            pending_mixer,
            sink,
            sources,
            worker,
            limits,
            scratch,
            audio_telemetry,
            stinger_audio: None,
            collect_output: true,
            failed: false,
        })
    }

    /// Preflights physical leaf audio and registers plan-routed explicit
    /// silence without opening scene inputs as media sources.
    ///
    /// # Errors
    ///
    /// Returns the same bounded setup failures as [`Self::preflight_local_blocking`].
    #[allow(clippy::too_many_arguments)]
    pub fn preflight_project_local_blocking(
        adapter: Option<&Adapter>,
        resolved: &[NativeResolvedSource],
        project: &NativeProjectPlan,
        format: &AudioFormat,
        frame_rate: FrameRate,
        clock_domain: ClockDomainId,
        expected_next_frame: u64,
        limits: NativeAudioLimits,
    ) -> Result<Self, NativeMasterError> {
        if project.audio_routes.len() > MAX_NATIVE_AUDIO_STRIPS {
            return Err(NativeMasterError::BoundsExceeded);
        }
        let requested_stinger_inputs = project
            .stingers
            .values()
            .filter(|config| {
                config.preload && config.audio_policy != ModelStingerAudioPolicy::Muted
            })
            .map(|config| config.media_input)
            .collect::<BTreeSet<_>>();
        let mut stinger_inputs = BTreeSet::new();
        let mut silent_stinger_inputs = BTreeSet::new();
        for &input in &requested_stinger_inputs {
            let source = resolved
                .iter()
                .find(|source| source.input() == input)
                .ok_or(NativeMasterError::MissingAudioRoute { input })?;
            if native_source_has_audio(adapter, source)? {
                stinger_inputs.insert(input);
            } else {
                silent_stinger_inputs.insert(input);
            }
        }
        let (ordinary_limits, stinger_limits) =
            partition_native_audio_limits(limits, !stinger_inputs.is_empty())?;
        let mut runtime = Self::preflight_local_blocking(
            adapter,
            resolved,
            format.clone(),
            frame_rate,
            clock_domain,
            expected_next_frame,
            ordinary_limits,
        )?;
        runtime.realize_project_audio(project)?;
        if !requested_stinger_inputs.is_empty() {
            let per_source_limits = (!stinger_inputs.is_empty())
                .then(|| partition_stinger_audio_limits(stinger_limits, stinger_inputs.len()))
                .transpose()?;
            let mut masters = BTreeMap::new();
            for input in stinger_inputs {
                let stinger_source = resolved
                    .iter()
                    .find(|source| source.input() == input)
                    .cloned()
                    .ok_or(NativeMasterError::MissingAudioRoute { input })?;
                let mut stinger_master = Self::preflight_local_blocking_with_decoder_retention(
                    adapter,
                    &[stinger_source],
                    format.clone(),
                    frame_rate,
                    clock_domain,
                    0,
                    per_source_limits.ok_or(NativeMasterError::InvalidLimits)?,
                    true,
                )?;
                if stinger_master.mixer.input_state(input).is_none() {
                    continue;
                }
                let state = native_input_state(project, input)?;
                stinger_master.mixer.set_input_state(input, state, 0)?;
                stinger_master
                    .pending_mixer
                    .set_input_state(input, state, 0)?;
                stinger_master.collect_output = false;
                masters.insert(input, Box::new(stinger_master));
            }
            runtime.stinger_audio = Some(NativeStingerAudioPlayback {
                masters,
                silent_inputs: silent_stinger_inputs,
                active_trigger: None,
                ready: None,
            });
        }
        Ok(runtime)
    }

    fn realize_project_audio(
        &mut self,
        project: &NativeProjectPlan,
    ) -> Result<(), NativeMasterError> {
        self.apply_project_audio_strips(project)?;
        let map = ChannelMapping::identity(self.format.channels.clone())?;
        for input in project.audio_strips.keys().copied() {
            if self.mixer.input_state(input).is_some() {
                continue;
            }
            let state = native_input_state(project, input)?;
            match project
                .audio_route(input)
                .ok_or(NativeMasterError::MissingAudioRoute { input })?
            {
                NativeAudioRoute::Leaf(source) => {
                    self.mixer.add_input_alias(input, source, state)?;
                    self.pending_mixer.add_input_alias(input, source, state)?;
                }
                NativeAudioRoute::Silence => {
                    self.mixer
                        .add_input(input, self.format.clone(), map.clone(), state)?;
                    self.pending_mixer
                        .add_input(input, self.format.clone(), map.clone(), state)?;
                    self.sources.insert(input, NativeAudioSource::silence());
                }
            }
        }
        let source_count = self.sources.len();
        if self.scratch.plans.capacity() < source_count {
            self.scratch.plans.reserve(source_count);
            self.scratch.uncovered.reserve(source_count);
            self.scratch.padding_requests.reserve(source_count);
            self.scratch.padding_sources.reserve(source_count);
            self.scratch.completed.reserve(source_count);
            self.scratch.validated.reserve(source_count);
            self.scratch.inputs.reserve(source_count);
        }
        Ok(())
    }

    fn apply_project_audio_strips(
        &mut self,
        project: &NativeProjectPlan,
    ) -> Result<(), NativeMasterError> {
        let states = self
            .sources
            .keys()
            .map(|input| Ok((*input, native_input_state(project, *input)?)))
            .collect::<Result<Vec<_>, NativeMasterError>>()?;
        let mut mixer = self.mixer.clone();
        let mut pending_mixer = self.pending_mixer.clone();
        for (input, state) in states {
            mixer.set_input_state(input, state, 0)?;
            pending_mixer.set_input_state(input, state, 0)?;
        }
        self.mixer = mixer;
        self.pending_mixer = pending_mixer;
        Ok(())
    }

    #[must_use]
    pub const fn expected_next_frame(&self) -> u64 {
        self.expected_next_frame
    }

    #[must_use]
    pub fn retained_blocks(&self) -> usize {
        self.stinger_masters()
            .fold(self.retained_charge().blocks, |total, master| {
                total.saturating_add(master.retained_charge().blocks)
            })
    }

    #[must_use]
    pub fn retained_samples(&self) -> usize {
        self.stinger_masters()
            .fold(self.retained_charge().samples, |total, master| {
                total.saturating_add(master.retained_charge().samples)
            })
    }

    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.stinger_masters()
            .fold(self.retained_charge().bytes, |total, master| {
                total.saturating_add(master.retained_charge().bytes)
            })
    }

    #[must_use]
    pub fn sink_len(&self) -> usize {
        self.sink.len()
    }

    #[must_use]
    pub const fn sink_telemetry(&self) -> SinkTelemetry {
        self.sink.telemetry()
    }

    #[must_use]
    pub fn audio_telemetry(&self) -> NativeAudioTelemetry {
        let mut telemetry = self.audio_telemetry;
        for master in self.stinger_masters() {
            telemetry.merge(master.audio_telemetry);
        }
        telemetry
    }

    #[must_use]
    pub fn collected_audio(&self) -> impl ExactSizeIterator<Item = &AudioBlock> {
        self.sink.iter()
    }

    fn stinger_masters(&self) -> impl Iterator<Item = &NativeMasterRuntime> {
        self.stinger_audio
            .iter()
            .flat_map(|playback| playback.masters.values().map(Box::as_ref))
    }

    /// Drains completed pages, plans every source for the next absolute frame
    /// interval, and schedules at most one bounded page per source.
    ///
    /// `false` means a non-EOS source does not yet cover the interval and the
    /// native tick must stall. This function never waits for decoder work.
    ///
    /// # Errors
    ///
    /// Returns a sticky fatal worker, decode, timeline, or resource-bound
    /// failure.
    pub fn service_next_frame(&mut self) -> Result<bool, NativeMasterError> {
        if self.failed {
            return Err(NativeMasterError::Failed);
        }
        let result = self.service_next_frame_inner();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    /// Services ordinary globally-timed audio plus the projected clip-local
    /// Stinger lane without allowing either cursor set to mutate the other.
    ///
    /// # Errors
    ///
    /// Returns a sticky ordinary or clip-local decode, timeline, readiness, or
    /// resource-bound failure.
    pub fn service_project_next_frame(
        &mut self,
        frame: &FrameResult,
        project: &NativeProjectPlan,
    ) -> Result<bool, NativeMasterError> {
        let ordinary_covered = self.service_next_frame()?;
        let stinger_covered = match self.stinger_audio.as_mut() {
            Some(playback) => playback.service(frame, project)?,
            None => stinger_audio_request(project, frame)?
                .is_none_or(|request| request.policy == ModelStingerAudioPolicy::Muted),
        };
        Ok(ordinary_covered && stinger_covered)
    }

    fn restart_clip_at(&mut self, cadence_origin_frame: u64) -> Result<(), NativeMasterError> {
        self.expected_next_frame = 0;
        self.cadence_origin_frame = cadence_origin_frame;
        self.ready_frame = None;
        self.mixer.reset_runtime_state();
        self.pending_mixer.reset_runtime_state();
        for source in self.sources.values_mut() {
            let Some(synchronizer) = source.synchronizer.as_mut() else {
                continue;
            };
            source.decode_generation = source
                .decode_generation
                .checked_add(1)
                .ok_or(NativeMasterError::BoundsExceeded)?;
            source.restart_decode = true;
            source.next_sample = source.restart_sample;
            source.next_sequence = source.restart_sequence;
            source.end_of_stream = false;
            synchronizer.reset(synchronizer.source_origin(), synchronizer.master_origin());
        }
        Ok(())
    }

    fn service_next_frame_inner(&mut self) -> Result<bool, NativeMasterError> {
        let (start_sample, end_sample) = self.next_frame_sample_span()?;
        self.commit_completed_pages()?;
        self.scratch.uncovered.clear();
        self.scratch.padding_requests.clear();
        for (&input, source) in &mut self.sources {
            if source.explicit_silence || end_sample <= source.audio_start_master_sample {
                continue;
            }
            let render_start = start_sample.max(source.audio_start_master_sample);
            let render_samples = usize::try_from(end_sample - render_start)
                .map_err(|_| NativeMasterError::BoundsExceeded)?;
            let render_timing = output_audio_timing(
                self.expected_next_frame,
                render_start,
                end_sample,
                self.format.sample_rate.hertz(),
                self.clock_domain,
            )?;
            let synchronizer = source
                .synchronizer
                .as_mut()
                .ok_or(NativeMasterError::InvalidFormat)?;
            match synchronizer.plan_render(master_audio_interval(render_timing), render_samples) {
                Ok(_) => {}
                Err(fm_audio::AudioSynchronizerError::NeedMoreInput {
                    required_sample, ..
                }) if source.end_of_stream => {
                    self.scratch.padding_requests.push((input, required_sample));
                }
                Err(fm_audio::AudioSynchronizerError::NeedMoreInput { .. }) => {
                    self.scratch.uncovered.push(input);
                }
                Err(error) => return Err(error.into()),
            }
        }
        if !self.scratch.padding_requests.is_empty() {
            self.commit_staged_eos_padding()?;
        }
        self.audio_telemetry.source_stalls = self
            .audio_telemetry
            .source_stalls
            .saturating_add(u64::try_from(self.scratch.uncovered.len()).unwrap_or(u64::MAX));
        self.schedule_refills(end_sample)?;
        let covered = self.scratch.uncovered.is_empty();
        self.ready_frame = covered.then_some((self.expected_next_frame, start_sample, end_sample));
        Ok(covered)
    }

    fn next_frame_sample_span(&self) -> Result<(u64, u64), NativeMasterError> {
        let cadence_frame = self
            .cadence_origin_frame
            .checked_add(self.expected_next_frame)
            .ok_or(NativeMasterError::BoundsExceeded)?;
        let (origin, _) = absolute_frame_sample_span(
            self.cadence_origin_frame,
            self.format.sample_rate.hertz(),
            self.frame_rate,
        )?;
        let (start, end) = absolute_frame_sample_span(
            cadence_frame,
            self.format.sample_rate.hertz(),
            self.frame_rate,
        )?;
        Ok((
            start
                .checked_sub(origin)
                .ok_or(NativeMasterError::BoundsExceeded)?,
            end.checked_sub(origin)
                .ok_or(NativeMasterError::BoundsExceeded)?,
        ))
    }

    fn commit_staged_eos_padding(&mut self) -> Result<(), NativeMasterError> {
        self.scratch.padding_spans.clear();
        self.scratch.padding_sources.clear();
        let mut retained = self.retained_charge();
        for &(input, required_sample) in &self.scratch.padding_requests {
            if self.scratch.padding_sources.len() == self.scratch.padding_sources.capacity() {
                return Err(NativeMasterError::BoundsExceeded);
            }
            let source = self
                .sources
                .get(&input)
                .ok_or(NativeMasterError::DecodeContract { input })?;
            let staged = stage_eos_padding_source(
                input,
                required_sample,
                source,
                self.limits,
                &mut self.scratch.padding_spans,
            )?;
            retained = retained
                .checked_add(staged.page.charge)
                .ok_or(NativeMasterError::BoundsExceeded)?;
            validate_retained_bounds(retained, self.limits)?;
            self.scratch.padding_sources.push(staged);
        }
        for staged in &self.scratch.padding_sources {
            let source =
                self.sources
                    .get_mut(&staged.input)
                    .ok_or(NativeMasterError::DecodeContract {
                        input: staged.input,
                    })?;
            source
                .synchronizer
                .as_mut()
                .ok_or(NativeMasterError::InvalidFormat)?
                .push_silence_batch(
                    &self.scratch.padding_spans[staged.span_start..staged.span_end],
                )?;
            source.next_sample = staged.page.next_sample;
            source.next_sequence = staged.page.next_sequence;
            self.audio_telemetry.eos_padding_blocks = self
                .audio_telemetry
                .eos_padding_blocks
                .saturating_add(u64::try_from(staged.page.charge.blocks).unwrap_or(u64::MAX));
            self.audio_telemetry.eos_padding_samples = self
                .audio_telemetry
                .eos_padding_samples
                .saturating_add(u64::try_from(staged.page.charge.samples).unwrap_or(u64::MAX));
        }
        observe_retained_peak(&mut self.audio_telemetry, retained);
        Ok(())
    }

    fn commit_completed_pages(&mut self) -> Result<(), NativeMasterError> {
        self.scratch.completed.clear();
        loop {
            match self.worker.results.try_recv() {
                Ok(result) => self.scratch.completed.push(result),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err(self.worker.disconnected_error());
                }
            }
        }
        self.scratch.validated.clear();
        let mut retained = self.retained_charge();
        while let Some(completed) = self.scratch.completed.pop() {
            let input = completed.input;
            let source = self
                .sources
                .get_mut(&input)
                .ok_or(NativeMasterError::DecodeContract { input })?;
            if completed.reservation.generation != source.decode_generation {
                if source.in_flight == Some(completed.reservation) {
                    source.in_flight = None;
                }
                self.release_reservation(completed.reservation.charge)?;
                continue;
            }
            self.audio_telemetry.positioned_blocks = self
                .audio_telemetry
                .positioned_blocks
                .saturating_add(completed.positioned_blocks);
            self.audio_telemetry.positioned_samples = self
                .audio_telemetry
                .positioned_samples
                .saturating_add(completed.positioned_samples);
            let window = completed
                .window
                .map_err(|error| NativeMasterError::Ffmpeg { input, error })?;
            validate_audio_window_contract(input, &window, completed.reservation.count)?;
            let source = self
                .sources
                .get(&input)
                .ok_or(NativeMasterError::DecodeContract { input })?;
            if source.in_flight != Some(completed.reservation) || source.end_of_stream {
                return Err(NativeMasterError::DecodeContract { input });
            }
            let page = validate_audio_page(input, source, &window.blocks, self.clock_domain)?;
            validate_page_bounds(&page, self.limits)?;
            retained = retained
                .checked_add(page.charge)
                .ok_or(NativeMasterError::BoundsExceeded)?;
            validate_retained_bounds(retained, self.limits)?;
            self.scratch.validated.push(ValidatedCompletedAudioPage {
                input,
                reservation: completed.reservation,
                window,
                page,
            });
        }
        while let Some(validated) = self.scratch.validated.pop() {
            let source = self.sources.get_mut(&validated.input).ok_or(
                NativeMasterError::DecodeContract {
                    input: validated.input,
                },
            )?;
            commit_audio_page(source, &validated.window.blocks, validated.page)?;
            source.in_flight = None;
            source.end_of_stream = validated.window.end_of_stream;
            self.release_reservation(validated.reservation.charge)?;
        }
        observe_retained_peak(&mut self.audio_telemetry, retained);
        Ok(())
    }

    fn schedule_refills(&mut self, requested_end: u64) -> Result<(), NativeMasterError> {
        let mut reserved = self
            .sources
            .values()
            .filter_map(|source| source.in_flight.map(|reservation| reservation.charge))
            .try_fold(NativeAudioCharge::default(), NativeAudioCharge::checked_add)
            .ok_or(NativeMasterError::BoundsExceeded)?;
        let retained = self.retained_charge();
        self.prepare_refill_order();
        for index in 0..self.scratch.inputs.len() {
            let input = self.scratch.inputs[index];
            let Some((request, reservation)) =
                self.plan_refill(input, requested_end, retained, reserved)?
            else {
                continue;
            };
            let Some(sender) = self.worker.requests.as_ref() else {
                return Err(self.worker.disconnected_error());
            };
            match sender.try_send(request) {
                Ok(()) => {}
                Err(TrySendError::Disconnected(_)) => {
                    return Err(self.worker.disconnected_error());
                }
                Err(TrySendError::Full(_)) => return Err(NativeMasterError::WorkerQueueFull),
            }
            self.sources
                .get_mut(&input)
                .ok_or(NativeMasterError::DecodeContract { input })?
                .in_flight = Some(reservation);
            self.sources
                .get_mut(&input)
                .ok_or(NativeMasterError::DecodeContract { input })?
                .restart_decode = false;
            reserved = reserved
                .checked_add(reservation.charge)
                .ok_or(NativeMasterError::BoundsExceeded)?;
            self.record_reservation(reservation.charge);
        }
        Ok(())
    }

    fn plan_refill(
        &self,
        input: InputId,
        requested_end: u64,
        retained: NativeAudioCharge,
        reserved: NativeAudioCharge,
    ) -> Result<Option<(NativeAudioDecodeRequest, NativeAudioReservation)>, NativeMasterError> {
        let source = self
            .sources
            .get(&input)
            .ok_or(NativeMasterError::DecodeContract { input })?;
        if source.explicit_silence {
            return Ok(None);
        }
        let synchronizer = source
            .synchronizer
            .as_ref()
            .ok_or(NativeMasterError::InvalidFormat)?;
        let occupancy = synchronizer.telemetry();
        let needs_coverage = self.scratch.uncovered.contains(&input);
        if source.end_of_stream
            || source.in_flight.is_some()
            || (!needs_coverage && requested_end <= source.audio_start_master_sample)
            || (!needs_coverage && occupancy.buffered_blocks() > AUDIO_REFILL_LOW_WATERMARK)
        {
            return Ok(None);
        }
        let available_blocks = usize::try_from(self.limits.max_blocks_per_source.get())
            .unwrap_or(usize::MAX)
            .saturating_sub(occupancy.buffered_blocks());
        let count = available_blocks
            .min(usize::try_from(self.limits.max_blocks_per_page.get()).unwrap_or(usize::MAX));
        let Some(count) = u32::try_from(count).ok().and_then(NonZeroU32::new) else {
            return if needs_coverage {
                Err(NativeMasterError::BoundsExceeded)
            } else {
                Ok(None)
            };
        };
        let samples = proportional_page_samples(count, self.limits)?;
        let charge = NativeAudioCharge {
            blocks: count.get() as usize,
            samples,
            bytes: audio_sample_bytes(samples, synchronizer.channel_layout().channels().len())?,
        };
        let source_limits = synchronizer.limits();
        let exceeds_bounds = retained
            .checked_add(reserved)
            .and_then(|allocated| allocated.checked_add(charge))
            .is_none_or(|allocated| validate_retained_bounds(allocated, self.limits).is_err())
            || occupancy
                .buffered_samples()
                .checked_add(charge.samples)
                .is_none_or(|samples| samples > source_limits.max_samples())
            || occupancy
                .buffered_bytes()
                .checked_add(charge.bytes)
                .is_none_or(|bytes| bytes > source_limits.max_bytes());
        if exceeds_bounds {
            return if needs_coverage {
                Err(NativeMasterError::BoundsExceeded)
            } else {
                Ok(None)
            };
        }
        let reservation = NativeAudioReservation {
            count,
            charge,
            generation: source.decode_generation,
        };
        Ok(Some((
            NativeAudioDecodeRequest {
                input,
                count,
                max_samples: charge.samples,
                max_bytes: charge.bytes,
                generation: source.decode_generation,
                restart: source.restart_decode,
                restart_target_ordinal: source.restart_target_ordinal,
                max_position_blocks: self.limits.max_position_blocks,
            },
            reservation,
        )))
    }

    fn prepare_refill_order(&mut self) {
        self.scratch.inputs.clear();
        self.scratch
            .inputs
            .extend(self.scratch.uncovered.iter().copied());
        self.scratch.inputs.extend(
            self.sources
                .keys()
                .filter(|input| !self.scratch.uncovered.contains(input))
                .copied(),
        );
    }

    fn retained_charge(&self) -> NativeAudioCharge {
        self.sources
            .values()
            .fold(NativeAudioCharge::default(), |total, source| {
                match source.synchronizer.as_ref() {
                    Some(synchronizer) => {
                        let telemetry = synchronizer.telemetry();
                        total
                            .checked_add(NativeAudioCharge {
                                blocks: telemetry.buffered_blocks(),
                                samples: telemetry.buffered_samples(),
                                bytes: telemetry.buffered_bytes(),
                            })
                            .unwrap_or(NativeAudioCharge {
                                blocks: usize::MAX,
                                samples: usize::MAX,
                                bytes: usize::MAX,
                            })
                    }
                    None => total,
                }
            })
    }

    fn record_reservation(&mut self, charge: NativeAudioCharge) {
        self.audio_telemetry.reservation_requests =
            self.audio_telemetry.reservation_requests.saturating_add(1);
        self.audio_telemetry.reserved_blocks = self
            .audio_telemetry
            .reserved_blocks
            .saturating_add(charge.blocks);
        self.audio_telemetry.reserved_samples = self
            .audio_telemetry
            .reserved_samples
            .saturating_add(charge.samples);
        self.audio_telemetry.reserved_bytes = self
            .audio_telemetry
            .reserved_bytes
            .saturating_add(charge.bytes);
        self.audio_telemetry.peak_reserved_blocks = self
            .audio_telemetry
            .peak_reserved_blocks
            .max(self.audio_telemetry.reserved_blocks);
        self.audio_telemetry.peak_reserved_samples = self
            .audio_telemetry
            .peak_reserved_samples
            .max(self.audio_telemetry.reserved_samples);
        self.audio_telemetry.peak_reserved_bytes = self
            .audio_telemetry
            .peak_reserved_bytes
            .max(self.audio_telemetry.reserved_bytes);
    }

    fn release_reservation(&mut self, charge: NativeAudioCharge) -> Result<(), NativeMasterError> {
        let current = NativeAudioCharge {
            blocks: self.audio_telemetry.reserved_blocks,
            samples: self.audio_telemetry.reserved_samples,
            bytes: self.audio_telemetry.reserved_bytes,
        };
        let next = current
            .checked_sub(charge)
            .ok_or(NativeMasterError::BoundsExceeded)?;
        self.audio_telemetry.reserved_blocks = next.blocks;
        self.audio_telemetry.reserved_samples = next.samples;
        self.audio_telemetry.reserved_bytes = next.bytes;
        Ok(())
    }

    /// Mixes the serviced Program interval, retains a copy in the bounded fake
    /// sink, and discards the authoritative owned block. Fade linearly weights
    /// both sources across the exact interval; Cut keeps one source at unity.
    ///
    /// This method performs no probe, decode, channel mapping, or blocking wait.
    ///
    /// # Errors
    ///
    /// Returns a sticky fatal frame-order, readiness, coalescing, mix, timing,
    /// sink, or resource-bound failure.
    pub fn render_frame(&mut self, frame: &FrameResult) -> Result<(), NativeMasterError> {
        self.render_frame_audio(frame).map(drop)
    }

    /// Mixes one serviced authoritative Program interval and returns its exact
    /// owned audio block. A clone is retained in the bounded fake sink so
    /// existing diagnostics remain identical to [`Self::render_frame`].
    ///
    /// Fade and Wipe linearly weight both sources across the exact interval;
    /// Cut keeps one source at unity. This method performs no probe, decode,
    /// channel mapping, or blocking wait.
    ///
    /// # Errors
    ///
    /// Returns a sticky fatal frame-order, readiness, coalescing, mix, timing,
    /// sink, or resource-bound failure. A failed frame does not advance the
    /// frame cursor or clear its serviced readiness state.
    pub fn render_frame_audio(
        &mut self,
        frame: &FrameResult,
    ) -> Result<AudioBlock, NativeMasterError> {
        if self.failed {
            return Err(NativeMasterError::Failed);
        }
        let result = self.render_frame_audio_inner(frame);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    /// Routes schema scene audio to its terminal leaf or explicit silence
    /// before applying the existing native transition mix plan.
    ///
    /// # Errors
    ///
    /// Returns a sticky route, readiness, mix, timing, sink, or resource failure.
    pub fn render_project_frame_audio(
        &mut self,
        frame: &FrameResult,
        project: &NativeProjectPlan,
    ) -> Result<AudioBlock, NativeMasterError> {
        if self.failed {
            return Err(NativeMasterError::Failed);
        }
        let result = self.render_frame_audio_inner_with_project(frame, Some(project));
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    #[allow(clippy::too_many_lines)]
    fn render_frame_audio_inner(
        &mut self,
        frame: &FrameResult,
    ) -> Result<AudioBlock, NativeMasterError> {
        self.render_frame_audio_inner_with_project(frame, None)
    }

    #[allow(clippy::too_many_lines)]
    fn render_frame_audio_inner_with_project(
        &mut self,
        frame: &FrameResult,
        project: Option<&NativeProjectPlan>,
    ) -> Result<AudioBlock, NativeMasterError> {
        let actual = frame.frame.get();
        if actual != self.expected_next_frame {
            return Err(NativeMasterError::UnexpectedFrame {
                expected: self.expected_next_frame,
                actual,
            });
        }
        let Some((ready_frame, start_sample, end_sample)) = self.ready_frame else {
            return Err(NativeMasterError::FrameNotReady(actual));
        };
        if ready_frame != actual {
            return Err(NativeMasterError::FrameNotReady(actual));
        }
        let timing = output_audio_timing(
            actual,
            start_sample,
            end_sample,
            self.format.sample_rate.hertz(),
            self.clock_domain,
        )?;
        let samples = usize::try_from(end_sample - start_sample)
            .map_err(|_| NativeMasterError::BoundsExceeded)?;
        if self.sink.policy() == OverflowPolicy::Reject && self.sink.len() == self.sink.capacity() {
            return Err(NativeMasterError::SinkRejected);
        }
        let strip_plan = match project {
            Some(project) => native_project_audio_mix_plan(project, frame.program)?,
            None => native_audio_mix_plan(frame.program)?,
        };
        self.scratch.plans.clear();
        for (&input, source) in &mut self.sources {
            if source.explicit_silence {
                continue;
            }
            let output = self
                .scratch
                .rendered
                .get_mut(&input)
                .ok_or(NativeMasterError::DecodeContract { input })?;
            for plane in &mut *output {
                plane[..samples].fill(0.0);
            }
            if end_sample <= source.audio_start_master_sample {
                continue;
            }
            let render_start = start_sample.max(source.audio_start_master_sample);
            let output_offset = usize::try_from(render_start - start_sample)
                .map_err(|_| NativeMasterError::BoundsExceeded)?;
            let render_samples = samples
                .checked_sub(output_offset)
                .ok_or(NativeMasterError::BoundsExceeded)?;
            let render_timing = output_audio_timing(
                actual,
                render_start,
                end_sample,
                self.format.sample_rate.hertz(),
                self.clock_domain,
            )?;
            let synchronizer = source
                .synchronizer
                .as_mut()
                .ok_or(NativeMasterError::InvalidFormat)?;
            let render_plan =
                synchronizer.plan_render(master_audio_interval(render_timing), render_samples)?;
            synchronizer.render_planned_planes(render_plan, output, output_offset)?;
            self.scratch.plans.push((input, render_plan));
        }
        self.pending_mixer.copy_runtime_state_from(&self.mixer)?;
        let mut active_video_inputs = [strip_plan.primary; 2];
        let active_video_input_count = if let Some((secondary, _)) = strip_plan.secondary {
            active_video_inputs[1] = secondary;
            2
        } else {
            1
        };
        let active_video_inputs = &active_video_inputs[..active_video_input_count];
        if let Some(project) = project {
            mix_project_audio_strips(
                &mut self.pending_mixer,
                timing,
                samples,
                project,
                strip_plan,
                active_video_inputs,
                &self.sources,
                &self.scratch.rendered,
                &mut self.scratch.mix,
                self.format.sample_rate,
            )?;
        } else {
            let primary = planar_audio_submission(
                strip_plan.primary,
                strip_plan.primary,
                strip_plan.primary_gain,
                samples,
                self.format.sample_rate,
                &self.sources,
                &self.scratch.rendered,
            )?;
            let secondary = strip_plan
                .secondary
                .map(|(input, gain)| {
                    planar_audio_submission(
                        input,
                        input,
                        gain,
                        samples,
                        self.format.sample_rate,
                        &self.sources,
                        &self.scratch.rendered,
                    )
                })
                .transpose()?
                .flatten();
            match (primary, secondary) {
                (Some(primary), Some(secondary)) => {
                    self.pending_mixer.mix_planar_timed_into(
                        timing,
                        samples,
                        &[primary, secondary],
                        active_video_inputs,
                        &mut self.scratch.mix,
                    )?;
                }
                (Some(submission), None) | (None, Some(submission)) => {
                    self.pending_mixer.mix_planar_timed_into(
                        timing,
                        samples,
                        &[submission],
                        active_video_inputs,
                        &mut self.scratch.mix,
                    )?;
                }
                (None, None) => {
                    self.pending_mixer.mix_planar_timed_into(
                        timing,
                        samples,
                        &[],
                        active_video_inputs,
                        &mut self.scratch.mix,
                    )?;
                }
            }
        }
        if let Some(project) = project {
            let request = stinger_audio_request(project, frame)?;
            let rendered = match self.stinger_audio.as_mut() {
                Some(playback) => playback.render(frame, project)?,
                None if request
                    .is_none_or(|request| request.policy == ModelStingerAudioPolicy::Muted) =>
                {
                    None
                }
                None => return Err(NativeMasterError::InvalidFormat),
            };
            if let Some((request, clip)) = rendered {
                mix_clip_local_stinger_audio(
                    &mut self.scratch.mix,
                    samples,
                    request.policy,
                    clip.as_ref(),
                    &self.format,
                )?;
            }
        }
        apply_master_clipping(
            &mut self.scratch.mix,
            samples,
            self.pending_mixer.clipping_policy(),
        );
        for &(input, render_plan) in &self.scratch.plans {
            self.sources
                .get(&input)
                .and_then(|source| source.synchronizer.as_ref())
                .ok_or(NativeMasterError::DecodeContract { input })?
                .preflight_commit_render(render_plan)?;
        }
        for &(input, render_plan) in &self.scratch.plans {
            self.sources
                .get_mut(&input)
                .and_then(|source| source.synchronizer.as_mut())
                .ok_or(NativeMasterError::DecodeContract { input })?
                .commit_render(render_plan)?;
        }
        apply_fade_to_black_audio(&mut self.scratch.mix, samples, frame.fade_to_black);
        // Canonical ownership is required by the returned/recorded frame. The
        // diagnostic collecting sink also retains its own bounded clone; these
        // are the only per-frame PCM allocations after reusable scratch render.
        let block = AudioBlock::new(
            timing,
            self.format.sample_rate,
            self.format.channels.clone(),
            self.scratch
                .mix
                .iter()
                .map(|plane| plane[..samples].to_vec())
                .collect(),
        )?;
        if self.collect_output {
            self.sink
                .collect(block.clone())
                .map_err(|_| NativeMasterError::SinkRejected)?;
        }
        let leading = self
            .sources
            .values()
            .filter(|source| !source.explicit_silence)
            .fold(0_u64, |total, source| {
                let end = end_sample.min(source.audio_start_master_sample);
                let start = start_sample.min(end);
                total.saturating_add(end - start)
            });
        self.audio_telemetry.leading_silence_samples = self
            .audio_telemetry
            .leading_silence_samples
            .saturating_add(leading);
        core::mem::swap(&mut self.mixer, &mut self.pending_mixer);
        self.expected_next_frame = self
            .expected_next_frame
            .checked_add(1)
            .ok_or(NativeMasterError::BoundsExceeded)?;
        self.ready_frame = None;
        Ok(block)
    }
}

impl NativeStingerAudioPlayback {
    fn service(
        &mut self,
        frame: &FrameResult,
        project: &NativeProjectPlan,
    ) -> Result<bool, NativeMasterError> {
        let Some(request) = stinger_audio_request(project, frame)? else {
            self.ready = None;
            return Ok(true);
        };
        if request.policy == ModelStingerAudioPolicy::Muted {
            self.active_trigger = Some(request.trigger);
            self.ready = Some(request);
            return Ok(true);
        }
        let silent = self.silent_inputs.contains(&request.trigger.media);
        match self.active_trigger {
            None if !silent => {
                self.master_mut(request.trigger.media)?.cadence_origin_frame =
                    request.trigger.cadence_origin_frame;
            }
            Some(active) if active != request.trigger && !silent => {
                self.master_mut(request.trigger.media)?
                    .restart_clip_at(request.trigger.cadence_origin_frame)?;
            }
            None | Some(_) => {}
        }
        self.active_trigger = Some(request.trigger);
        if silent {
            self.ready = Some(request);
            return Ok(true);
        }
        let master = self.master_mut(request.trigger.media)?;
        if master.expected_next_frame != u64::from(request.frame_index) {
            return Err(NativeMasterError::UnexpectedFrame {
                expected: master.expected_next_frame,
                actual: u64::from(request.frame_index),
            });
        }
        let covered = master.service_next_frame()?;
        self.ready = covered.then_some(request);
        Ok(covered)
    }

    fn render(
        &mut self,
        frame: &FrameResult,
        project: &NativeProjectPlan,
    ) -> Result<Option<(NativeStingerAudioRequest, Option<AudioBlock>)>, NativeMasterError> {
        let Some(request) = stinger_audio_request(project, frame)? else {
            self.ready = None;
            return Ok(None);
        };
        if self.ready != Some(request) {
            return Err(NativeMasterError::FrameNotReady(frame.frame.get()));
        }
        self.ready = None;
        if request.policy == ModelStingerAudioPolicy::Muted {
            return Ok(None);
        }
        if self.silent_inputs.contains(&request.trigger.media) {
            return Ok(Some((request, None)));
        }
        let mut local = frame.clone();
        local.frame = fm_scheduler::FrameNumber::new(u64::from(request.frame_index));
        local.program = ProgramFrame {
            primary: request.trigger.media,
            secondary: None,
            transition_kind: None,
            mix_numerator: 0,
            mix_denominator: 1,
            mix_start_numerator: 0,
            mix_end_numerator: 0,
        };
        local.fade_to_black = FadeToBlackFrame::LIVE;
        local.events.clear();
        let block = self
            .master_mut(request.trigger.media)?
            .render_frame_audio(&local)?;
        Ok(Some((request, Some(block))))
    }

    fn master_mut(
        &mut self,
        input: InputId,
    ) -> Result<&mut NativeMasterRuntime, NativeMasterError> {
        self.masters
            .get_mut(&input)
            .map(Box::as_mut)
            .ok_or(NativeMasterError::MissingAudioRoute { input })
    }
}

fn mix_clip_local_stinger_audio(
    output: &mut [Vec<f32>],
    samples: usize,
    policy: ModelStingerAudioPolicy,
    clip: Option<&AudioBlock>,
    format: &AudioFormat,
) -> Result<(), NativeMasterError> {
    if output.len() != format.channels.channels().len() {
        return Err(NativeMasterError::InvalidFormat);
    }
    if let Some(clip) = clip
        && (clip.sample_rate() != format.sample_rate
            || clip.channel_layout() != &format.channels
            || clip.sample_count() != samples)
    {
        return Err(NativeMasterError::InvalidFormat);
    }
    for (channel, output_plane) in output.iter_mut().enumerate() {
        let clip_plane = clip
            .map(|clip| clip.plane(channel).ok_or(NativeMasterError::InvalidFormat))
            .transpose()?;
        match policy {
            ModelStingerAudioPolicy::Muted => {}
            ModelStingerAudioPolicy::StingerOnly => {
                if let Some(clip_plane) = clip_plane {
                    output_plane[..samples].copy_from_slice(clip_plane);
                } else {
                    output_plane[..samples].fill(0.0);
                }
            }
            ModelStingerAudioPolicy::MixWithProgram => {
                if let Some(clip_plane) = clip_plane {
                    for (output, clip) in output_plane[..samples].iter_mut().zip(clip_plane) {
                        *output += *clip;
                    }
                }
            }
        }
    }
    Ok(())
}

fn apply_master_clipping(output: &mut [Vec<f32>], samples: usize, policy: ClippingPolicy) {
    if policy == ClippingPolicy::Clamp {
        for plane in output {
            for sample in &mut plane[..samples] {
                *sample = sample.clamp(-1.0, 1.0);
            }
        }
    }
}

fn validate_audio_limits(
    limits: NativeAudioLimits,
    channels: usize,
) -> Result<(), NativeMasterError> {
    let page_blocks = usize::try_from(limits.max_blocks_per_page.get()).unwrap_or(usize::MAX);
    let source_blocks = usize::try_from(limits.max_blocks_per_source.get()).unwrap_or(usize::MAX);
    if page_blocks > source_blocks
        || limits.max_samples_per_page == 0
        || limits.max_retained_blocks == 0
        || limits.max_retained_samples < limits.max_samples_per_page
        || limits.max_retained_bytes < audio_sample_bytes(limits.max_samples_per_page, channels)?
        || limits.max_position_blocks == 0
        || limits.max_leading_silence_samples == 0
        || limits.sink_blocks == 0
    {
        return Err(NativeMasterError::InvalidLimits);
    }
    Ok(())
}

fn validate_audio_window_contract(
    input: InputId,
    window: &DecodedAudioWindow,
    requested: NonZeroU32,
) -> Result<(), NativeMasterError> {
    let requested = usize::try_from(requested.get()).unwrap_or(usize::MAX);
    if window.blocks.len() > requested
        || (window.blocks.is_empty() && !window.end_of_stream)
        || (!window.end_of_stream && window.blocks.len() != requested)
    {
        return Err(NativeMasterError::DecodeContract { input });
    }
    Ok(())
}

fn validate_audio_page(
    input: InputId,
    source: &NativeAudioSource,
    blocks: &[AudioBlock],
    clock_domain: ClockDomainId,
) -> Result<ValidatedAudioPage, NativeMasterError> {
    let synchronizer = source
        .synchronizer
        .as_ref()
        .ok_or(NativeMasterError::InvalidFormat)?;
    let page = validate_audio_blocks(
        input,
        blocks,
        synchronizer.source_rate(),
        synchronizer.channel_layout(),
        source.timeline_origin,
        source.next_sample,
        source.next_sequence,
        clock_domain,
    )?;
    synchronizer.preflight_push_batch(blocks)?;
    Ok(page)
}

fn validate_initial_audio_page(
    input: InputId,
    blocks: &[AudioBlock],
    timeline_origin: AudioCadenceOrigin,
    source_origin_sample: u64,
    clock_domain: ClockDomainId,
) -> Result<ValidatedAudioPage, NativeMasterError> {
    let first = blocks
        .first()
        .ok_or(NativeMasterError::InvalidTimeline { input })?;
    validate_audio_blocks(
        input,
        blocks,
        first.sample_rate(),
        first.channel_layout(),
        timeline_origin,
        source_origin_sample,
        first.timing().sequence().get(),
        clock_domain,
    )
}

#[allow(clippy::too_many_arguments)]
fn validate_audio_blocks(
    input: InputId,
    blocks: &[AudioBlock],
    sample_rate: fm_types::SampleRate,
    channel_layout: &fm_types::ChannelLayout,
    timeline_origin: AudioCadenceOrigin,
    mut next_sample: u64,
    mut next_sequence: u64,
    clock_domain: ClockDomainId,
) -> Result<ValidatedAudioPage, NativeMasterError> {
    let mut charge = NativeAudioCharge::default();
    for block in blocks {
        if block.sample_rate() != sample_rate
            || block.channel_layout() != channel_layout
            || block.timing().clock_domain() != clock_domain
            || block.timing().sequence().get() != next_sequence
        {
            return Err(NativeMasterError::InvalidTimeline { input });
        }
        let raw_start = original_timestamp_samples(block.timing(), sample_rate.hertz())
            .ok_or(NativeMasterError::InvalidTimeline { input })?;
        let relative_start = cadence_sample_at_timestamp(
            timeline_origin,
            block.timing().presentation_timestamp().as_nanos(),
            sample_rate.hertz(),
        )?;
        if relative_start != next_sample {
            return Err(NativeMasterError::InvalidTimeline { input });
        }
        let sample_count = block.sample_count();
        let sample_count_u64 = u64::try_from(sample_count)
            .map_err(|_| NativeMasterError::InvalidTimeline { input })?;
        let end_sample = next_sample
            .checked_add(sample_count_u64)
            .ok_or(NativeMasterError::InvalidTimeline { input })?;
        validate_block_normalized_timing(block, raw_start, sample_rate.hertz(), input)?;
        charge = charge
            .checked_add(NativeAudioCharge {
                blocks: 1,
                samples: sample_count,
                bytes: audio_sample_bytes(sample_count, channel_layout.channels().len())?,
            })
            .ok_or(NativeMasterError::BoundsExceeded)?;
        next_sample = end_sample;
        next_sequence = next_sequence
            .checked_add(1)
            .ok_or(NativeMasterError::InvalidTimeline { input })?;
    }
    Ok(ValidatedAudioPage {
        next_sample,
        next_sequence,
        charge,
    })
}

fn validate_block_normalized_timing(
    block: &AudioBlock,
    raw_start_sample: i128,
    sample_rate: u32,
    input: InputId,
) -> Result<(), NativeMasterError> {
    let raw_end_sample = raw_start_sample
        .checked_add(
            i128::try_from(block.sample_count())
                .map_err(|_| NativeMasterError::InvalidTimeline { input })?,
        )
        .ok_or(NativeMasterError::InvalidTimeline { input })?;
    let normalized_start = normalized_sample_endpoint(raw_start_sample, sample_rate)
        .ok_or(NativeMasterError::InvalidTimeline { input })?;
    let normalized_end = normalized_sample_endpoint(raw_end_sample, sample_rate)
        .ok_or(NativeMasterError::InvalidTimeline { input })?;
    let duration = normalized_end
        .checked_sub(normalized_start)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(NativeMasterError::InvalidTimeline { input })?;
    if block.timing().presentation_timestamp().as_nanos() != normalized_start
        || block.timing().duration().as_nanos() != duration
    {
        return Err(NativeMasterError::InvalidTimeline { input });
    }
    Ok(())
}

fn original_timestamp_samples(timing: MediaTiming, sample_rate: u32) -> Option<i128> {
    let original = timing.original_timestamp();
    let time_base = original.time_base();
    let numerator = i128::from(original.timestamp().ticks())
        .checked_mul(i128::from(time_base.numerator()))?
        .checked_mul(i128::from(sample_rate))?;
    let denominator = i128::from(time_base.denominator());
    (numerator % denominator == 0).then_some(numerator / denominator)
}

fn normalized_sample_endpoint(sample: i128, sample_rate: u32) -> Option<i64> {
    sample
        .checked_mul(1_000_000_000)?
        .checked_div(i128::from(sample_rate))?
        .try_into()
        .ok()
}

fn commit_audio_page(
    source: &mut NativeAudioSource,
    blocks: &[AudioBlock],
    page: ValidatedAudioPage,
) -> Result<(), NativeMasterError> {
    source
        .synchronizer
        .as_mut()
        .ok_or(NativeMasterError::InvalidFormat)?
        .push_batch(blocks)?;
    source.next_sample = page.next_sample;
    source.next_sequence = page.next_sequence;
    Ok(())
}

fn validate_page_bounds(
    page: &ValidatedAudioPage,
    limits: NativeAudioLimits,
) -> Result<(), NativeMasterError> {
    if page.charge.blocks > usize::try_from(limits.max_blocks_per_page.get()).unwrap_or(usize::MAX)
        || page.charge.samples > limits.max_samples_per_page
        || page.charge.bytes > limits.max_retained_bytes
    {
        return Err(NativeMasterError::BoundsExceeded);
    }
    Ok(())
}

fn validate_retained_bounds(
    charge: NativeAudioCharge,
    limits: NativeAudioLimits,
) -> Result<(), NativeMasterError> {
    if charge.blocks > limits.max_retained_blocks
        || charge.samples > limits.max_retained_samples
        || charge.bytes > limits.max_retained_bytes
    {
        return Err(NativeMasterError::BoundsExceeded);
    }
    Ok(())
}

fn audio_sample_bytes(samples: usize, channels: usize) -> Result<usize, NativeMasterError> {
    samples
        .checked_mul(channels)
        .and_then(|value| value.checked_mul(size_of::<f32>()))
        .ok_or(NativeMasterError::BoundsExceeded)
}

fn proportional_page_samples(
    count: NonZeroU32,
    limits: NativeAudioLimits,
) -> Result<usize, NativeMasterError> {
    limits
        .max_samples_per_page
        .checked_mul(count.get() as usize)
        .and_then(|samples| samples.checked_add(limits.max_blocks_per_page.get() as usize - 1))
        .map(|samples| samples / limits.max_blocks_per_page.get() as usize)
        .filter(|samples| *samples != 0)
        .ok_or(NativeMasterError::BoundsExceeded)
}

fn observe_retained_peak(telemetry: &mut NativeAudioTelemetry, charge: NativeAudioCharge) {
    telemetry.peak_retained_blocks = telemetry.peak_retained_blocks.max(charge.blocks);
    telemetry.peak_retained_samples = telemetry.peak_retained_samples.max(charge.samples);
    telemetry.peak_retained_bytes = telemetry.peak_retained_bytes.max(charge.bytes);
}

fn nonnegative_mapping_anchors(source: i64, master: i64) -> Result<(u64, u64), NativeMasterError> {
    let shift = -i128::from(source).min(i128::from(master)).min(0);
    Ok((
        u64::try_from(i128::from(source) + shift).map_err(|_| NativeMasterError::BoundsExceeded)?,
        u64::try_from(i128::from(master) + shift).map_err(|_| NativeMasterError::BoundsExceeded)?,
    ))
}

fn cadence_sample_at_or_after(timestamp: i64, sample_rate: u32) -> Result<u64, NativeMasterError> {
    let timestamp = u128::try_from(timestamp).map_err(|_| NativeMasterError::BoundsExceeded)?;
    let sample = timestamp
        .checked_mul(u128::from(sample_rate))
        .and_then(|value| value.checked_add(999_999_999))
        .ok_or(NativeMasterError::BoundsExceeded)?
        / 1_000_000_000;
    u64::try_from(sample).map_err(|_| NativeMasterError::BoundsExceeded)
}

fn cadence_sample_at_timestamp(
    origin: AudioCadenceOrigin,
    timestamp: i64,
    sample_rate: u32,
) -> Result<u64, NativeMasterError> {
    let origin_boundary = i128::from(origin.sample_index())
        .checked_mul(1_000_000_000)
        .ok_or(NativeMasterError::BoundsExceeded)?
        / i128::from(sample_rate);
    let absolute_position = i128::from(timestamp)
        .checked_sub(i128::from(origin.timestamp().as_nanos()))
        .and_then(|elapsed| elapsed.checked_add(origin_boundary))
        .ok_or(NativeMasterError::BoundsExceeded)?;
    let numerator = absolute_position
        .checked_add(1)
        .and_then(|value| value.checked_mul(i128::from(sample_rate)))
        .and_then(|value| value.checked_sub(1))
        .ok_or(NativeMasterError::BoundsExceeded)?;
    let sample = floor_div_i128(numerator, 1_000_000_000)?;
    u64::try_from(sample).map_err(|_| NativeMasterError::BoundsExceeded)
}

fn cadence_timestamp_at_sample(
    origin: AudioCadenceOrigin,
    sample: u64,
    sample_rate: u32,
) -> Result<NormalizedTimestamp, NativeMasterError> {
    let origin_boundary =
        normalized_sample_endpoint(i128::from(origin.sample_index()), sample_rate)
            .ok_or(NativeMasterError::BoundsExceeded)?;
    let sample_boundary = normalized_sample_endpoint(i128::from(sample), sample_rate)
        .ok_or(NativeMasterError::BoundsExceeded)?;
    let timestamp = origin
        .timestamp()
        .as_nanos()
        .checked_add(sample_boundary - origin_boundary)
        .ok_or(NativeMasterError::BoundsExceeded)?;
    Ok(NormalizedTimestamp::from_nanos(timestamp))
}

fn floor_div_i128(numerator: i128, denominator: i128) -> Result<i128, NativeMasterError> {
    if denominator <= 0 {
        return Err(NativeMasterError::BoundsExceeded);
    }
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    if remainder < 0 {
        quotient
            .checked_sub(1)
            .ok_or(NativeMasterError::BoundsExceeded)
    } else {
        Ok(quotient)
    }
}

fn absolute_frame_sample_span(
    frame: u64,
    sample_rate: u32,
    frame_rate: FrameRate,
) -> Result<(u64, u64), NativeMasterError> {
    let samples_per_frame_numerator =
        u128::from(sample_rate) * u128::from(frame_rate.denominator());
    let denominator = u128::from(frame_rate.numerator());
    let start = u128::from(frame)
        .checked_mul(samples_per_frame_numerator)
        .ok_or(NativeMasterError::BoundsExceeded)?
        / denominator;
    let end = u128::from(frame)
        .checked_add(1)
        .and_then(|value| value.checked_mul(samples_per_frame_numerator))
        .ok_or(NativeMasterError::BoundsExceeded)?
        / denominator;
    Ok((
        u64::try_from(start).map_err(|_| NativeMasterError::BoundsExceeded)?,
        u64::try_from(end).map_err(|_| NativeMasterError::BoundsExceeded)?,
    ))
}

fn output_audio_timing(
    frame: u64,
    start_sample: u64,
    end_sample: u64,
    sample_rate: u32,
    clock_domain: ClockDomainId,
) -> Result<MediaTiming, NativeMasterError> {
    let start_tick = i64::try_from(start_sample).map_err(|_| NativeMasterError::BoundsExceeded)?;
    let start_ns = normalized_sample_endpoint(i128::from(start_sample), sample_rate)
        .ok_or(NativeMasterError::BoundsExceeded)?;
    let end_ns = normalized_sample_endpoint(i128::from(end_sample), sample_rate)
        .ok_or(NativeMasterError::BoundsExceeded)?;
    let duration_ns = end_ns
        .checked_sub(start_ns)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(NativeMasterError::BoundsExceeded)?;
    let time_base = TimeBase::new(1, sample_rate).map_err(|_| NativeMasterError::InvalidFormat)?;
    Ok(MediaTiming::new(
        OriginalTimestamp::new(MediaTimestamp::new(start_tick), time_base),
        NormalizedTimestamp::from_nanos(start_ns),
        NormalizedDuration::from_nanos(duration_ns)?,
        clock_domain,
        SequenceNumber::new(frame),
    )?)
}

fn master_audio_interval(timing: MediaTiming) -> MasterAudioInterval {
    MasterAudioInterval::new(
        MappingClockDomainId::new(timing.clock_domain().get()),
        timing.presentation_timestamp(),
        timing.duration(),
    )
}

fn stage_eos_padding_source(
    input: InputId,
    required_sample: u64,
    source: &NativeAudioSource,
    limits: NativeAudioLimits,
    spans: &mut Vec<AudioSilenceSpan>,
) -> Result<StagedAudioPadding, NativeMasterError> {
    let synchronizer = source
        .synchronizer
        .as_ref()
        .ok_or(NativeMasterError::InvalidFormat)?;
    let channels = synchronizer.channel_layout().channels().len();
    let span_start = spans.len();
    let mut next_sample = source.next_sample;
    let mut next_sequence = source.next_sequence;
    let mut charge = NativeAudioCharge::default();
    let max_span_samples = limits
        .max_samples_per_page
        .min(fm_audio::MAX_SAMPLES_PER_BLOCK);
    while next_sample <= required_sample {
        if spans.len() == spans.capacity() {
            return Err(NativeMasterError::BoundsExceeded);
        }
        let remaining = required_sample
            .checked_sub(next_sample)
            .and_then(|distance| distance.checked_add(1))
            .ok_or(NativeMasterError::BoundsExceeded)?;
        let samples = remaining
            .min(u64::try_from(max_span_samples).map_err(|_| NativeMasterError::BoundsExceeded)?);
        let samples = usize::try_from(samples).map_err(|_| NativeMasterError::BoundsExceeded)?;
        let samples = NonZeroUsize::new(samples).ok_or(NativeMasterError::BoundsExceeded)?;
        spans.push(eos_silence_span(
            source,
            next_sample,
            next_sequence,
            samples,
        )?);
        charge = charge
            .checked_add(NativeAudioCharge {
                blocks: 1,
                samples: samples.get(),
                bytes: audio_sample_bytes(samples.get(), channels)?,
            })
            .ok_or(NativeMasterError::BoundsExceeded)?;
        next_sample = next_sample
            .checked_add(
                u64::try_from(samples.get()).map_err(|_| NativeMasterError::BoundsExceeded)?,
            )
            .ok_or(NativeMasterError::BoundsExceeded)?;
        next_sequence = next_sequence
            .checked_add(1)
            .ok_or(NativeMasterError::BoundsExceeded)?;
    }
    let span_end = spans.len();
    synchronizer.preflight_push_silence_batch(&spans[span_start..span_end])?;
    Ok(StagedAudioPadding {
        input,
        span_start,
        span_end,
        page: ValidatedAudioPage {
            next_sample,
            next_sequence,
            charge,
        },
    })
}

fn eos_silence_span(
    source: &NativeAudioSource,
    start_sample: u64,
    sequence: u64,
    samples: NonZeroUsize,
) -> Result<AudioSilenceSpan, NativeMasterError> {
    let synchronizer = source
        .synchronizer
        .as_ref()
        .ok_or(NativeMasterError::InvalidFormat)?;
    let end_sample = start_sample
        .checked_add(u64::try_from(samples.get()).map_err(|_| NativeMasterError::BoundsExceeded)?)
        .ok_or(NativeMasterError::BoundsExceeded)?;
    let origin = synchronizer.source_origin();
    let rate = synchronizer.source_rate();
    let origin_boundary =
        normalized_sample_endpoint(i128::from(origin.sample_index()), rate.hertz())
            .ok_or(NativeMasterError::BoundsExceeded)?;
    let start_boundary = normalized_sample_endpoint(i128::from(start_sample), rate.hertz())
        .ok_or(NativeMasterError::BoundsExceeded)?;
    let end_boundary = normalized_sample_endpoint(i128::from(end_sample), rate.hertz())
        .ok_or(NativeMasterError::BoundsExceeded)?;
    let start = origin
        .timestamp()
        .as_nanos()
        .checked_add(start_boundary - origin_boundary)
        .ok_or(NativeMasterError::BoundsExceeded)?;
    let duration = u64::try_from(end_boundary - start_boundary)
        .map_err(|_| NativeMasterError::BoundsExceeded)?;
    let timing = MediaTiming::new(
        OriginalTimestamp::new(
            MediaTimestamp::new(
                i64::try_from(start_sample).map_err(|_| NativeMasterError::BoundsExceeded)?,
            ),
            TimeBase::new(1, rate.hertz()).map_err(|_| NativeMasterError::InvalidFormat)?,
        ),
        NormalizedTimestamp::from_nanos(start),
        NormalizedDuration::from_nanos(duration)?,
        ClockDomainId::new(synchronizer.mapping().source_domain().get()),
        SequenceNumber::new(sequence),
    )?;
    Ok(AudioSilenceSpan::new(timing, samples))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeAudioMixPlan {
    primary: InputId,
    primary_gain: SourceGain,
    secondary: Option<(InputId, SourceGain)>,
}

#[allow(clippy::too_many_arguments)]
fn mix_project_audio_strips(
    mixer: &mut MasterMixer,
    timing: MediaTiming,
    samples: usize,
    project: &NativeProjectPlan,
    program: NativeAudioMixPlan,
    active_video_inputs: &[InputId],
    sources: &BTreeMap<InputId, NativeAudioSource>,
    rendered: &BTreeMap<InputId, Vec<Vec<f32>>>,
    output: &mut [Vec<f32>],
    sample_rate: fm_types::SampleRate,
) -> Result<(), NativeMasterError> {
    let mut first = None;
    for (&logical, route) in &project.audio_routes {
        let NativeAudioRoute::Leaf(physical) = *route else {
            continue;
        };
        first = planar_audio_submission(
            logical,
            physical,
            logical_source_gain(program, logical),
            samples,
            sample_rate,
            sources,
            rendered,
        )?;
        if first.is_some() {
            break;
        }
    }
    let Some(first) = first else {
        mixer.mix_planar_timed_into(timing, samples, &[], active_video_inputs, output)?;
        return Ok(());
    };

    let mut submissions = [first; MAX_NATIVE_AUDIO_STRIPS];
    let mut submission_count = 0;
    for (&logical, route) in &project.audio_routes {
        let NativeAudioRoute::Leaf(physical) = *route else {
            continue;
        };
        let Some(submission) = planar_audio_submission(
            logical,
            physical,
            logical_source_gain(program, logical),
            samples,
            sample_rate,
            sources,
            rendered,
        )?
        else {
            continue;
        };
        let slot = submissions
            .get_mut(submission_count)
            .ok_or(NativeMasterError::BoundsExceeded)?;
        *slot = submission;
        submission_count += 1;
    }
    mixer.mix_planar_timed_into(
        timing,
        samples,
        &submissions[..submission_count],
        active_video_inputs,
        output,
    )?;
    Ok(())
}

fn planar_audio_submission<'a>(
    logical: InputId,
    physical: InputId,
    source_gain: SourceGain,
    samples: usize,
    sample_rate: fm_types::SampleRate,
    sources: &'a BTreeMap<InputId, NativeAudioSource>,
    rendered: &'a BTreeMap<InputId, Vec<Vec<f32>>>,
) -> Result<Option<PlanarAudioSource<'a>>, NativeMasterError> {
    let source = sources
        .get(&physical)
        .ok_or(NativeMasterError::DecodeContract { input: physical })?;
    if source.explicit_silence {
        return Ok(None);
    }
    let synchronizer = source
        .synchronizer
        .as_ref()
        .ok_or(NativeMasterError::DecodeContract { input: physical })?;
    let planes = rendered
        .get(&physical)
        .ok_or(NativeMasterError::DecodeContract { input: physical })?;
    Ok(Some(PlanarAudioSource {
        input: logical,
        sample_rate,
        channel_layout: synchronizer.channel_layout(),
        planes,
        samples,
        source_gain,
    }))
}

fn logical_source_gain(program: NativeAudioMixPlan, input: InputId) -> SourceGain {
    if input == program.primary {
        program.primary_gain
    } else {
        program
            .secondary
            .filter(|(secondary, _)| *secondary == input)
            .map_or(SourceGain::UNITY, |(_, gain)| gain)
    }
}

fn native_input_state(
    project: &NativeProjectPlan,
    input: InputId,
) -> Result<InputState, NativeMasterError> {
    let state = project
        .audio_strip(input)
        .ok_or(NativeMasterError::MissingAudioRoute { input })?;
    let milli_db = state.gain.get();
    let whole_db = i16::try_from(milli_db / 1_000).expect("persisted gain is bounded");
    let fractional_milli_db =
        i16::try_from(milli_db % 1_000).expect("persisted gain remainder fits i16");
    Ok(InputState {
        gain: fm_audio::Gain::from_db(
            f32::from(whole_db) + f32::from(fractional_milli_db) / 1_000.0,
        )?,
        muted: state.muted,
        follow_video: state.follow_video,
    })
}

fn native_audio_mix_plan(program: ProgramFrame) -> Result<NativeAudioMixPlan, NativeMasterError> {
    let Some(secondary) = program.secondary else {
        return Ok(NativeAudioMixPlan {
            primary: program.primary,
            primary_gain: SourceGain::UNITY,
            secondary: None,
        });
    };
    if secondary == program.primary {
        return Ok(NativeAudioMixPlan {
            primary: program.primary,
            primary_gain: SourceGain::UNITY,
            secondary: None,
        });
    }
    match program.transition_kind {
        Some(
            SwitcherTransitionKind::Fade
            | SwitcherTransitionKind::Wipe
            | SwitcherTransitionKind::AlphaFade
            | SwitcherTransitionKind::Slide
            | SwitcherTransitionKind::Zoom,
        ) => sample_linear_audio_mix_plan(program, secondary),
        Some(kind @ SwitcherTransitionKind::Stinger(_)) => {
            Err(NativeMasterError::UnsupportedAudioTransition(kind))
        }
        None => Err(NativeMasterError::MissingAudioTransitionKind),
    }
}

fn native_project_audio_mix_plan(
    project: &NativeProjectPlan,
    program: ProgramFrame,
) -> Result<NativeAudioMixPlan, NativeMasterError> {
    let Some(SwitcherTransitionKind::Stinger(slot)) = program.transition_kind else {
        return native_audio_mix_plan(program);
    };
    let preview = program
        .secondary
        .filter(|preview| *preview != program.primary)
        .ok_or(NativeMasterError::MissingAudioTransitionKind)?;
    let config = project
        .stinger(slot)
        .ok_or(NativeMasterError::MissingStingerConfiguration(slot))?;
    let frame = StingerFramePlan::compile(
        program.mix_numerator,
        program.mix_denominator,
        config.cut_point_frames,
    )
    .map_err(NativeMasterError::InvalidStinger)?;
    let base = match frame.base() {
        fm_compositor::StingerBase::Program => program.primary,
        fm_compositor::StingerBase::Preview => preview,
    };
    Ok(NativeAudioMixPlan {
        primary: base,
        primary_gain: SourceGain::UNITY,
        secondary: None,
    })
}

fn stinger_audio_request(
    project: &NativeProjectPlan,
    frame: &FrameResult,
) -> Result<Option<NativeStingerAudioRequest>, NativeMasterError> {
    let Some(SwitcherTransitionKind::Stinger(slot)) = frame.program.transition_kind else {
        return Ok(None);
    };
    let config = project
        .stinger(slot)
        .ok_or(NativeMasterError::MissingStingerConfiguration(slot))?;
    StingerFramePlan::compile(
        frame.program.mix_numerator,
        frame.program.mix_denominator,
        config.cut_point_frames,
    )
    .map_err(NativeMasterError::InvalidStinger)?;
    Ok(Some(NativeStingerAudioRequest {
        trigger: NativeStingerAudioTrigger {
            slot,
            media: config.media_input,
            cadence_origin_frame: frame
                .frame
                .get()
                .checked_sub(u64::from(frame.program.mix_numerator))
                .ok_or(NativeMasterError::BoundsExceeded)?,
        },
        frame_index: frame.program.mix_numerator,
        policy: config.audio_policy,
    }))
}

fn sample_linear_audio_mix_plan(
    program: ProgramFrame,
    secondary: InputId,
) -> Result<NativeAudioMixPlan, NativeMasterError> {
    let secondary_gain = SourceGain::new(
        program.mix_start_numerator,
        program.mix_end_numerator,
        program.mix_denominator,
    )?;
    let primary_gain = SourceGain::new(
        program.mix_denominator - program.mix_start_numerator,
        program.mix_denominator - program.mix_end_numerator,
        program.mix_denominator,
    )?;
    Ok(NativeAudioMixPlan {
        primary: program.primary,
        primary_gain,
        secondary: Some((secondary, secondary_gain)),
    })
}

#[allow(clippy::cast_possible_truncation)]
fn apply_fade_to_black_audio(planes: &mut [Vec<f32>], samples: usize, frame: FadeToBlackFrame) {
    let denominator = f64::from(frame.interval_start().denominator());
    let start =
        f64::from(frame.interval_start().denominator() - frame.interval_start().numerator())
            / denominator;
    let end = f64::from(frame.interval_end().denominator() - frame.interval_end().numerator())
        / f64::from(frame.interval_end().denominator());
    let steps = f64::from(u32::try_from(samples).expect("native audio sample count is bounded"));
    for plane in planes {
        for (sample, value) in plane[..samples].iter_mut().enumerate() {
            let step =
                f64::from(u32::try_from(sample + 1).expect("native audio sample count is bounded"));
            let gain = start + (end - start) * (step / steps);
            *value *= gain as f32;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeMixPlan {
    primary: InputId,
    secondary: InputId,
    transition: TransitionPlan,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NativeStingerMixPlan {
    program: InputId,
    preview: InputId,
    media: InputId,
    frame: StingerFramePlan,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeProjectMixPlan {
    Transition(NativeMixPlan),
    Stinger(NativeStingerMixPlan),
}

fn native_mix_plan(program: ProgramFrame) -> Result<NativeMixPlan, NativeSourceRenderError> {
    let (secondary, transition) = match program.secondary {
        Some(secondary) if secondary != program.primary => {
            let kind = match program.transition_kind {
                Some(SwitcherTransitionKind::Fade) => TransitionKind::Fade,
                Some(SwitcherTransitionKind::AlphaFade) => TransitionKind::AlphaFade,
                Some(SwitcherTransitionKind::Wipe) => TransitionKind::Wipe,
                Some(SwitcherTransitionKind::Slide) => TransitionKind::Slide,
                Some(SwitcherTransitionKind::Zoom) => TransitionKind::Zoom,
                Some(kind) => return Err(NativeSourceRenderError::UnsupportedTransition(kind)),
                None => return Err(NativeSourceRenderError::MissingTransitionKind),
            };
            (
                secondary,
                TransitionPlan::compile(kind, program.mix_numerator, program.mix_denominator)
                    .map_err(NativeSourceRenderError::InvalidMix)?,
            )
        }
        Some(_) | None => (
            program.primary,
            TransitionPlan::compile(TransitionKind::Cut, 0, 1)
                .map_err(NativeSourceRenderError::InvalidMix)?,
        ),
    };
    Ok(NativeMixPlan {
        primary: program.primary,
        secondary,
        transition,
    })
}

fn native_project_mix_plan(
    project: &NativeProjectPlan,
    program: ProgramFrame,
) -> Result<NativeProjectMixPlan, NativeSourceRenderError> {
    let Some(SwitcherTransitionKind::Stinger(slot)) = program.transition_kind else {
        return native_mix_plan(program).map(NativeProjectMixPlan::Transition);
    };
    let preview = program
        .secondary
        .filter(|preview| *preview != program.primary)
        .ok_or(NativeSourceRenderError::MissingTransitionKind)?;
    let config = project
        .stinger(slot)
        .ok_or(NativeSourceRenderError::MissingStingerConfiguration(slot))?;
    let frame = StingerFramePlan::compile(
        program.mix_numerator,
        program.mix_denominator,
        config.cut_point_frames,
    )
    .map_err(NativeSourceRenderError::InvalidStinger)?;
    Ok(NativeProjectMixPlan::Stinger(NativeStingerMixPlan {
        program: program.primary,
        preview,
        media: config.media_input,
        frame,
    }))
}

fn native_fade_to_black_plan(
    frame: FadeToBlackFrame,
) -> Result<FadeToBlackPlan, NativeSourceRenderError> {
    let start = CompositorFadeToBlackPosition::compile(
        frame.interval_start().numerator(),
        frame.interval_start().denominator(),
    )
    .map_err(NativeSourceRenderError::InvalidFadeToBlack)?;
    let end = CompositorFadeToBlackPosition::compile(
        frame.interval_end().numerator(),
        frame.interval_end().denominator(),
    )
    .map_err(NativeSourceRenderError::InvalidFadeToBlack)?;
    Ok(FadeToBlackPlan::new(
        start,
        end,
        CompositorFadeToBlackPosition::BLACK,
    ))
}

#[cfg(test)]
fn prefix_decode_request(
    clock_domain: ClockDomainId,
    selector: StreamSelector,
    count: NonZeroU32,
) -> DecodeRequest {
    DecodeRequest {
        clock_domain,
        video: Some(SequenceRequest { selector, count }),
        audio: None,
    }
}

fn validate_source_ids<T>(
    sources: &[(InputId, T)],
    maximum: usize,
) -> Result<(), NativeSourceError> {
    if sources.len() > maximum {
        return Err(NativeSourceError::TooManySources {
            actual: sources.len(),
            maximum,
        });
    }
    let mut ids = BTreeSet::new();
    for (input, _) in sources {
        if !ids.insert(*input) {
            return Err(NativeSourceError::DuplicateSource(*input));
        }
    }
    Ok(())
}

fn validate_resolved_sources(
    sources: &[NativeResolvedSource],
    adapter: Option<&Adapter>,
    maximum: usize,
) -> Result<(), NativeSourcePreflightError> {
    let ids = sources
        .iter()
        .map(|source| (source.input(), ()))
        .collect::<Vec<_>>();
    validate_source_ids(&ids, maximum)?;
    if adapter.is_none()
        && let Some(input) = sources.iter().find_map(|source| match source {
            NativeResolvedSource::LocalVideo { input, .. } => Some(*input),
            NativeResolvedSource::RetainedFrame { .. } | NativeResolvedSource::LiveFrame { .. } => {
                None
            }
        })
    {
        return Err(NativeSourcePreflightError::CodecAdapterRequired { input });
    }
    Ok(())
}

fn validate_source_layouts(
    sources: &[(InputId, u32, u32, usize)],
    limits: NativeSourceLimits,
) -> Result<(Option<(u32, u32)>, u64), NativeSourceError> {
    validate_source_ids(
        &sources
            .iter()
            .map(|(input, _, _, _)| (*input, ()))
            .collect::<Vec<_>>(),
        limits.max_media_inputs,
    )?;

    let mut dimensions = None;
    let mut retained = 0_u64;
    for &(input, width, height, frame_count) in sources {
        let frame_count_is_bounded = u32::try_from(frame_count)
            .is_ok_and(|count| count <= limits.max_video_frames_per_source.get());
        if frame_count == 0 {
            return Err(NativeSourceError::InvalidTimeline { input });
        }
        if !frame_count_is_bounded {
            return Err(NativeSourceError::TooManyFrames {
                input,
                actual: frame_count,
                maximum: limits.max_video_frames_per_source.get(),
            });
        }
        if let Some((expected_width, expected_height)) = dimensions {
            if (width, height) != (expected_width, expected_height) {
                return Err(NativeSourceError::DimensionMismatch {
                    input,
                    expected_width,
                    expected_height,
                    actual_width: width,
                    actual_height: height,
                });
            }
        } else {
            dimensions = Some((width, height));
        }
        let frame_bytes = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(RGBA16_FLOAT_BYTES_PER_PIXEL))
            .ok_or(NativeSourceError::FrameByteSizeOverflow {
                input,
                width,
                height,
            })?;
        let source_bytes = frame_bytes
            .checked_mul(u64::try_from(frame_count).unwrap_or(u64::MAX))
            .ok_or(NativeSourceError::RetainedBytesExceeded {
                required: u64::MAX,
                maximum: limits.max_retained_rgba16f_bytes,
            })?;
        retained =
            retained
                .checked_add(source_bytes)
                .ok_or(NativeSourceError::RetainedBytesExceeded {
                    required: u64::MAX,
                    maximum: limits.max_retained_rgba16f_bytes,
                })?;
        if retained > limits.max_retained_rgba16f_bytes {
            return Err(NativeSourceError::RetainedBytesExceeded {
                required: retained,
                maximum: limits.max_retained_rgba16f_bytes,
            });
        }
    }
    Ok((dimensions, retained))
}

fn validate_page_timing(
    input: InputId,
    frames: &[CpuVideoFrame],
    source_pts_origin: i64,
    previous_pts: Option<i64>,
    expected_sequence: u64,
    clock_domain: ClockDomainId,
) -> Result<(Vec<u64>, i64, u64), NativeSourceError> {
    let timestamps = frames
        .iter()
        .map(|frame| frame.timing().presentation_timestamp().as_nanos())
        .collect::<Vec<_>>();
    let sequences = frames
        .iter()
        .map(|frame| frame.timing().sequence().get())
        .collect::<Vec<_>>();
    if frames
        .iter()
        .any(|frame| frame.timing().clock_domain() != clock_domain)
    {
        return Err(NativeSourceError::InvalidTimeline { input });
    }
    validate_timing_values(
        input,
        &timestamps,
        &sequences,
        source_pts_origin,
        previous_pts,
        expected_sequence,
    )
}

fn validate_retained_frame_timing(
    input: InputId,
    frame: &CpuVideoFrame,
    clock_domain: ClockDomainId,
) -> Result<(Vec<u64>, i64, u64), NativeSourceError> {
    let timing = frame.timing();
    if timing.duration().as_nanos() == 0 {
        return Err(NativeSourceError::InvalidTimeline { input });
    }
    let source_pts_origin = timing.presentation_timestamp().as_nanos();
    validate_page_timing(
        input,
        std::slice::from_ref(frame),
        source_pts_origin,
        None,
        0,
        clock_domain,
    )
}

fn validate_live_seed_timing(
    input: InputId,
    frame: &CpuVideoFrame,
) -> Result<(Vec<u64>, i64, u64, ClockDomainId), NativeSourceError> {
    let timing = frame.timing();
    if timing.duration().as_nanos() == 0 {
        return Err(NativeSourceError::InvalidTimeline { input });
    }
    Ok((
        vec![0],
        timing.presentation_timestamp().as_nanos(),
        timing.sequence().get(),
        timing.clock_domain(),
    ))
}

fn validate_live_update_timing(
    input: InputId,
    frame: &CpuVideoFrame,
    previous_pts: i64,
    previous_sequence: u64,
    clock_domain: ClockDomainId,
) -> Result<(), NativeSourceError> {
    let timing = frame.timing();
    if timing.duration().as_nanos() == 0
        || timing.clock_domain() != clock_domain
        || timing.presentation_timestamp().as_nanos() <= previous_pts
        || timing.sequence().get() <= previous_sequence
    {
        return Err(NativeSourceError::InvalidTimeline { input });
    }
    Ok(())
}

fn validate_timing_values(
    input: InputId,
    timestamps: &[i64],
    sequences: &[u64],
    source_pts_origin: i64,
    mut previous_pts: Option<i64>,
    mut expected_sequence: u64,
) -> Result<(Vec<u64>, i64, u64), NativeSourceError> {
    if timestamps.is_empty() || timestamps.len() != sequences.len() {
        return Err(NativeSourceError::InvalidTimeline { input });
    }
    let mut offsets = Vec::with_capacity(timestamps.len());
    for (index, (&pts, &sequence)) in timestamps.iter().zip(sequences).enumerate() {
        if previous_pts.is_some_and(|previous| pts <= previous) || sequence != expected_sequence {
            return Err(NativeSourceError::InvalidTimeline { input });
        }
        offsets.push(
            u64::try_from(i128::from(pts) - i128::from(source_pts_origin))
                .map_err(|_| NativeSourceError::InvalidTimeline { input })?,
        );
        previous_pts = Some(pts);
        if index + 1 < sequences.len() {
            expected_sequence = expected_sequence
                .checked_add(1)
                .ok_or(NativeSourceError::InvalidTimeline { input })?;
        }
    }
    Ok((
        offsets,
        previous_pts.expect("nonempty timestamps"),
        *sequences.last().expect("nonempty sequences"),
    ))
}

#[cfg(test)]
fn rebased_offsets(input: InputId, timestamps: &[i64]) -> Result<Vec<u64>, NativeSourceError> {
    let first = *timestamps
        .first()
        .ok_or(NativeSourceError::InvalidTimeline { input })?;
    let mut previous = None;
    timestamps
        .iter()
        .map(|&pts| {
            if previous.is_some_and(|previous| pts <= previous) {
                return Err(NativeSourceError::InvalidTimeline { input });
            }
            previous = Some(pts);
            u64::try_from(i128::from(pts) - i128::from(first))
                .map_err(|_| NativeSourceError::InvalidTimeline { input })
        })
        .collect()
}

fn frame_index_at_deadline(offsets_ns: &[u64], deadline_ns: u64) -> Option<usize> {
    offsets_ns
        .partition_point(|offset| *offset <= deadline_ns)
        .checked_sub(1)
}

fn floor_anchor_eviction_count(offsets_ns: &[u64], deadline: ClockTime) -> usize {
    frame_index_at_deadline(offsets_ns, deadline.as_nanos()).unwrap_or_default()
}

fn source_eviction_count(
    offsets_ns: &[u64],
    pinned_for_stinger: bool,
    deadline: ClockTime,
) -> usize {
    if pinned_for_stinger {
        0
    } else {
        floor_anchor_eviction_count(offsets_ns, deadline)
    }
}

fn source_covers_deadline(
    latest_offset_ns: Option<u64>,
    end_of_stream: bool,
    deadline: ClockTime,
) -> bool {
    end_of_stream || latest_offset_ns.is_some_and(|latest| latest >= deadline.as_nanos())
}

fn refill_page_size(
    retained_frames: usize,
    in_flight: bool,
    end_of_stream: bool,
    maximum_frames: u32,
    budget_frames: u64,
) -> Option<NonZeroU32> {
    if in_flight
        || end_of_stream
        || retained_frames > SOURCE_REFILL_LOW_WATERMARK
        || retained_frames >= usize::try_from(maximum_frames).unwrap_or(usize::MAX)
    {
        return None;
    }
    let available_ring = u64::from(maximum_frames)
        .saturating_sub(u64::try_from(retained_frames).unwrap_or(u64::MAX));
    let count = available_ring
        .min(u64::from(SOURCE_REFILL_MAX_PAGE))
        .min(budget_frames);
    u32::try_from(count).ok().and_then(NonZeroU32::new)
}

fn rgba16f_frame_bytes(input: InputId, width: u32, height: u32) -> Result<u64, NativeSourceError> {
    u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(RGBA16_FLOAT_BYTES_PER_PIXEL))
        .ok_or(NativeSourceError::FrameByteSizeOverflow {
            input,
            width,
            height,
        })
}

fn registry_frame(
    registry: &NativeSourceRegistry,
    input: InputId,
    deadline: ClockTime,
) -> Result<&NativeWorkingFrame, NativeSourceRenderError> {
    let prefix = registered_source(&registry.sources, input)?;
    let frame = prefix
        .frame_at_deadline(deadline)
        .ok_or(NativeSourceRenderError::MissingSource { input })?;
    if let Some((expected_width, expected_height)) = registry.dimensions {
        let actual_width = frame.texture().width();
        let actual_height = frame.texture().height();
        if (actual_width, actual_height) != (expected_width, expected_height) {
            return Err(NativeSourceRenderError::DimensionMismatch {
                input,
                expected_width,
                expected_height,
                actual_width,
                actual_height,
            });
        }
    }
    Ok(frame)
}

fn stinger_frame_deadline(
    frame_rate: FrameRate,
    frame_index: u32,
) -> Result<ClockTime, NativeSourceRenderError> {
    let interval_end = FrameCadence::new(frame_rate, ClockTime::ZERO)
        .time_of_frame(u64::from(frame_index) + 1)
        .map_err(|_| NativeSourceRenderError::ResourceBounds)?;
    interval_end
        .as_nanos()
        .checked_sub(1)
        .map(ClockTime::from_nanos)
        .ok_or(NativeSourceRenderError::ResourceBounds)
}

fn registry_stinger_frame(
    registry: &NativeSourceRegistry,
    input: InputId,
    frame_rate: FrameRate,
    frame_index: u32,
) -> Result<&NativeWorkingFrame, NativeSourceRenderError> {
    let prefix = registered_source(&registry.sources, input)?;
    if prefix.kind != NativeVideoSourceKind::Retained && !prefix.available_for_stinger {
        return Err(NativeSourceRenderError::StingerSourceNotPreloaded { input });
    }
    let deadline = stinger_frame_deadline(frame_rate, frame_index)?;
    let frame = prefix
        .frame_at_deadline(deadline)
        .ok_or(NativeSourceRenderError::MissingSource { input })?;
    if let Some((expected_width, expected_height)) = registry.dimensions {
        let actual_width = frame.texture().width();
        let actual_height = frame.texture().height();
        if (actual_width, actual_height) != (expected_width, expected_height) {
            return Err(NativeSourceRenderError::DimensionMismatch {
                input,
                expected_width,
                expected_height,
                actual_width,
                actual_height,
            });
        }
    }
    Ok(frame)
}

fn registered_source<T>(
    sources: &BTreeMap<InputId, T>,
    input: InputId,
) -> Result<&T, NativeSourceRenderError> {
    sources
        .get(&input)
        .ok_or(NativeSourceRenderError::MissingSource { input })
}

fn project_texture<'a>(
    registry: &'a NativeSourceRegistry,
    project: &NativeProjectPlan,
    scene_outputs: &'a BTreeMap<SceneId, (SourceId, NativeTexture)>,
    input: InputId,
    deadline: ClockTime,
) -> Result<&'a NativeTexture, NativeSourceRenderError> {
    match project.video_route(input) {
        Some(NativeVideoRoute::Leaf(leaf)) => {
            Ok(registry_frame(registry, leaf, deadline)?.texture())
        }
        Some(NativeVideoRoute::Scene(scene)) => scene_outputs
            .get(&scene)
            .map(|(_, texture)| texture)
            .ok_or(NativeSourceRenderError::MissingSource { input }),
        None => Err(NativeSourceRenderError::MissingSource { input }),
    }
}

/// One native context shared by import, scene, transition, and FTB executors.
pub struct NativeMediaRuntime {
    context: NativeContext,
    normalizer: NativeImportNormalizer,
    composition_renderer: NativeCompositionRenderer,
    renderer: NativeTransitionRenderer,
    stinger_renderer: NativeStingerRenderer,
    fade_to_black_renderer: NativeFadeToBlackRenderer,
    project_rendering: AtomicBool,
    project_frames: Mutex<NativeProjectFrameState>,
}

/// Completion and slot accounting for bounded native project rendering.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NativeProjectFrameTelemetry {
    pub frames_submitted: u64,
    pub completion_waits: u64,
    pub in_flight_slots: usize,
    pub peak_in_flight_slots: usize,
}

#[derive(Default)]
struct NativeProjectFrameState {
    telemetry: NativeProjectFrameTelemetry,
}

struct NativeProjectRenderGuard<'a>(&'a AtomicBool);

impl Drop for NativeProjectRenderGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, AtomicOrdering::Release);
    }
}

/// Reusable private GPU target for blocking SDR Program capture.
///
/// The target is fixed-size `Rgba8Unorm`, and the transform explicitly writes
/// sRGB-encoded Rec.709 pixels from canonical `Rgba16Float` Program light. No
/// native texture or backend handle is exposed.
pub struct NativeProgramReadback {
    target: NativeTexture,
    transform: NativeSdrOutputTransform,
}

impl NativeProgramReadback {
    /// Returns the fixed output width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.target.width()
    }

    /// Returns the fixed output height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.target.height()
    }
}

impl NativeMediaRuntime {
    /// Synchronously creates a native runtime without requiring an async
    /// executor. The calling thread remains the runtime owner.
    ///
    /// # Errors
    ///
    /// Returns a typed GPU, color-pipeline, or compositor-pipeline failure.
    pub fn new_blocking(
        backends: impl IntoIterator<Item = NativeBackend>,
    ) -> Result<Self, NativeMediaError> {
        block_on(Self::new(backends))
    }

    /// Selects an adapter from `backends` and compiles both native pipelines on
    /// the resulting single context.
    ///
    /// # Errors
    ///
    /// Returns a typed GPU, color-pipeline, or compositor-pipeline failure.
    pub async fn new(
        backends: impl IntoIterator<Item = NativeBackend>,
    ) -> Result<Self, NativeMediaError> {
        let context = NativeContext::new(backends).await?;
        Self::from_context(context).await
    }

    /// Synchronously compiles the native media pipelines on an existing context.
    ///
    /// # Errors
    ///
    /// Returns a typed color-pipeline or compositor-pipeline failure.
    pub fn from_context_blocking(context: NativeContext) -> Result<Self, NativeMediaError> {
        block_on(Self::from_context(context))
    }

    /// Compiles both native media pipelines on an existing context.
    ///
    /// # Errors
    ///
    /// Returns a typed color-pipeline or compositor-pipeline failure.
    pub async fn from_context(context: NativeContext) -> Result<Self, NativeMediaError> {
        let normalizer = NativeImportNormalizer::new(&context).await?;
        let composition_renderer = NativeCompositionRenderer::new(&context).await?;
        let renderer = NativeTransitionRenderer::new(&context).await?;
        let stinger_renderer = NativeStingerRenderer::new(&context).await?;
        let fade_to_black_renderer = NativeFadeToBlackRenderer::new(&context).await?;
        Ok(Self {
            context,
            normalizer,
            composition_renderer,
            renderer,
            stinger_renderer,
            fade_to_black_renderer,
            project_rendering: AtomicBool::new(false),
            project_frames: Mutex::new(NativeProjectFrameState::default()),
        })
    }

    /// Returns the context shared by import and compositor resources.
    #[must_use]
    pub const fn context(&self) -> &NativeContext {
        &self.context
    }

    /// Returns bounded project-frame completion and slot accounting.
    #[must_use]
    pub fn project_frame_telemetry(&self) -> NativeProjectFrameTelemetry {
        self.project_frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .telemetry
    }

    fn acquire_project_render(
        &self,
    ) -> Result<NativeProjectRenderGuard<'_>, NativeSourceRenderError> {
        self.project_rendering
            .compare_exchange(
                false,
                true,
                AtomicOrdering::Acquire,
                AtomicOrdering::Relaxed,
            )
            .map_err(|_| NativeSourceRenderError::ConcurrentProjectRender)?;
        Ok(NativeProjectRenderGuard(&self.project_rendering))
    }

    fn complete_in_flight_project_frame(&self) -> Result<(), NativeSourceRenderError> {
        let occupied = self
            .project_frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .telemetry
            .in_flight_slots;
        if occupied == 0 {
            return Ok(());
        }
        self.context
            .wait_for_submitted_work()
            .map_err(NativeSourceRenderError::Completion)?;
        let mut state = self
            .project_frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.telemetry.in_flight_slots = 0;
        state.telemetry.completion_waits = state
            .telemetry
            .completion_waits
            .checked_add(1)
            .ok_or(NativeSourceRenderError::ResourceBounds)?;
        Ok(())
    }

    fn begin_project_frame(&self) -> Result<NativeProjectRenderGuard<'_>, NativeSourceRenderError> {
        let guard = self.acquire_project_render()?;
        self.complete_in_flight_project_frame()?;
        let mut state = self
            .project_frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.telemetry.in_flight_slots = NATIVE_PROJECT_IN_FLIGHT_SLOTS;
        state.telemetry.peak_in_flight_slots = state
            .telemetry
            .peak_in_flight_slots
            .max(NATIVE_PROJECT_IN_FLIGHT_SLOTS);
        Ok(guard)
    }

    fn finish_project_frame(&self) -> Result<(), NativeSourceRenderError> {
        let mut state = self
            .project_frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.telemetry.frames_submitted = state
            .telemetry
            .frames_submitted
            .checked_add(1)
            .ok_or(NativeSourceRenderError::ResourceBounds)?;
        Ok(())
    }

    /// Waits for the current bounded project-frame slot to complete and frees it.
    ///
    /// # Errors
    ///
    /// Returns a concurrent-render, GPU completion, or accounting failure.
    pub fn complete_project_frame_blocking(&self) -> Result<(), NativeSourceRenderError> {
        let _guard = self.acquire_project_render()?;
        self.complete_in_flight_project_frame()
    }

    /// Synchronously decodes a bounded local file, then asynchronously uploads
    /// and normalizes every decoded video frame on this runtime's GPU context.
    /// Decoded audio blocks are preserved unchanged.
    ///
    /// The adapter's subprocess decode is blocking and this method belongs on
    /// a worker thread. Only the subsequent GPU normalization is asynchronous.
    ///
    /// # Errors
    ///
    /// Returns a typed `FFmpeg` or native color/GPU failure. Error messages do
    /// not include the input path.
    pub async fn preroll_local_blocking(
        &self,
        adapter: &Adapter,
        path: impl AsRef<Path>,
        request: DecodeRequest,
    ) -> Result<NativeMediaPreroll, NativeMediaError> {
        let decoded = adapter.decode_local(path, request)?;
        let mut video = Vec::with_capacity(decoded.video.len());
        for frame in &decoded.video {
            video.push(self.normalizer.normalize(&self.context, frame).await?);
        }
        Ok(NativeMediaPreroll {
            video,
            audio: decoded.audio,
        })
    }

    /// Preflights resolved local media into atomic bounded GPU prefixes.
    ///
    /// Each `(InputId, PathBuf)` must already have been resolved by the project
    /// store (or another policy-owning resolver). The same `FFmpeg` adapter is
    /// reused to decode up to the configured number of leading selected video
    /// frames and no audio from each source. All ID, timeline, dimension, and
    /// retained RGBA16F-byte checks finish before any upload; every accepted
    /// frame is then normalized/uploaded once.
    /// Rendering the returned registry performs no decode or source upload.
    ///
    /// `FFmpeg` and ffprobe subprocess work is blocking. Call this method from a
    /// blocking worker even though GPU normalization makes the method async.
    /// The registry is returned only after every source succeeds; failed work
    /// and any temporary GPU textures are dropped without exposing a partial
    /// registry.
    ///
    /// # Errors
    ///
    /// Returns a typed, path-free source-bound, decode-contract, decode, or
    /// normalization failure.
    pub async fn preflight_resolved_sources_local_blocking(
        &self,
        adapter: &Adapter,
        sources: impl IntoIterator<Item = (InputId, PathBuf)>,
        clock_domain: ClockDomainId,
        selector: StreamSelector,
        limits: NativeSourceLimits,
    ) -> Result<NativeSourceRegistry, NativeSourcePreflightError> {
        self.preflight_resolved_source_playback_local_blocking(
            adapter,
            sources,
            clock_domain,
            selector,
            limits,
        )
        .await
        .map(NativeSourcePlayback::into_registry)
    }

    /// Preflights bounded source rings and retains their sequential cursors in
    /// one background CPU decode worker.
    ///
    /// All initial decode contracts, timelines, dimensions, and retained-byte
    /// charges are validated before any GPU upload. GPU normalization remains
    /// on this runtime and is committed only after a complete source batch has
    /// normalized. Retained-byte accounting covers committed RGBA16F ring
    /// textures; temporary CPU pages and normalization staging are not retained
    /// and are dropped if the batch cannot be committed.
    ///
    /// # Errors
    ///
    /// Returns a typed, path-free source-bound preflight or worker-start
    /// failure.
    pub async fn preflight_resolved_source_playback_local_blocking(
        &self,
        adapter: &Adapter,
        sources: impl IntoIterator<Item = (InputId, PathBuf)>,
        clock_domain: ClockDomainId,
        selector: StreamSelector,
        limits: NativeSourceLimits,
    ) -> Result<NativeSourcePlayback, NativeSourcePreflightError> {
        self.preflight_resolved_source_playback_mixed_local_blocking(
            Some(adapter),
            sources
                .into_iter()
                .map(|(input, path)| NativeResolvedSource::LocalVideo { input, path }),
            clock_domain,
            selector,
            limits,
        )
        .await
    }

    /// Preflights a mix of resolved local videos and retained CPU frames into
    /// bounded source rings.
    ///
    /// Local videos preserve their sequential decode cursors for worker refill.
    /// Each retained frame is rebased to offset zero and retained as an EOS
    /// source without a decoder cursor. All source IDs, initial timelines,
    /// dimensions, and retained RGBA16F charges are validated before any GPU
    /// upload. An adapter is required only when at least one local video exists.
    ///
    /// # Errors
    ///
    /// Returns a typed, path-free source-bound preflight or worker-start
    /// failure.
    #[allow(clippy::too_many_lines)]
    pub async fn preflight_resolved_source_playback_mixed_local_blocking(
        &self,
        adapter: Option<&Adapter>,
        sources: impl IntoIterator<Item = NativeResolvedSource>,
        clock_domain: ClockDomainId,
        selector: StreamSelector,
        limits: NativeSourceLimits,
    ) -> Result<NativeSourcePlayback, NativeSourcePreflightError> {
        let sources = sources.into_iter().collect::<Vec<_>>();
        validate_resolved_sources(&sources, adapter, limits.max_media_inputs)?;

        let mut decoded_sources = Vec::with_capacity(sources.len());
        let mut decoders = BTreeMap::new();
        for source in sources {
            match source {
                NativeResolvedSource::LocalVideo { input, path } => {
                    let adapter = adapter
                        .ok_or(NativeSourcePreflightError::CodecAdapterRequired { input })?;
                    let mut decoder = adapter
                        .open_local_video(path, clock_domain, selector)
                        .map_err(|error| NativeSourcePreflightError::Decode { input, error })?;
                    let initial_window =
                        decoder
                            .decode_up_to(limits.max_video_frames_per_source)
                            .map_err(|error| NativeSourcePreflightError::Decode { input, error })?;
                    let video_frames = initial_window.frames.len();
                    let frame_count_is_bounded = u32::try_from(video_frames)
                        .is_ok_and(|count| count <= limits.max_video_frames_per_source.get());
                    if video_frames == 0
                        || !frame_count_is_bounded
                        || (!initial_window.end_of_stream
                            && video_frames
                                != usize::try_from(limits.max_video_frames_per_source.get())
                                    .unwrap_or(usize::MAX))
                    {
                        return Err(NativeSourcePreflightError::DecodeContract {
                            input,
                            video_frames,
                            audio_blocks: 0,
                        });
                    }
                    let source_pts_origin = initial_window.frames[0]
                        .timing()
                        .presentation_timestamp()
                        .as_nanos();
                    let (offsets_ns, last_source_pts, last_sequence) = validate_page_timing(
                        input,
                        &initial_window.frames,
                        source_pts_origin,
                        None,
                        0,
                        clock_domain,
                    )?;
                    decoded_sources.push((
                        input,
                        initial_window.frames,
                        offsets_ns,
                        source_pts_origin,
                        last_source_pts,
                        last_sequence,
                        initial_window.end_of_stream,
                        clock_domain,
                        NativeVideoSourceKind::Decoded,
                    ));
                    decoders.insert(input, decoder);
                }
                NativeResolvedSource::RetainedFrame { input, frame } => {
                    let source_pts_origin = frame.timing().presentation_timestamp().as_nanos();
                    let (offsets_ns, last_source_pts, last_sequence) =
                        validate_retained_frame_timing(input, &frame, clock_domain)?;
                    decoded_sources.push((
                        input,
                        vec![frame],
                        offsets_ns,
                        source_pts_origin,
                        last_source_pts,
                        last_sequence,
                        true,
                        clock_domain,
                        NativeVideoSourceKind::Retained,
                    ));
                }
                NativeResolvedSource::LiveFrame { input, frame } => {
                    let (offsets_ns, last_source_pts, last_sequence, source_clock_domain) =
                        validate_live_seed_timing(input, &frame)?;
                    decoded_sources.push((
                        input,
                        vec![frame],
                        offsets_ns,
                        last_source_pts,
                        last_source_pts,
                        last_sequence,
                        false,
                        source_clock_domain,
                        NativeVideoSourceKind::Live,
                    ));
                }
            }
        }

        let mut layouts = Vec::with_capacity(decoded_sources.len());
        for (input, frames, ..) in &decoded_sources {
            let dimensions = frames
                .first()
                .ok_or(NativeSourceError::InvalidTimeline { input: *input })?
                .payload()
                .dimensions();
            for frame in &frames[1..] {
                let actual = frame.payload().dimensions();
                if actual != dimensions {
                    return Err(NativeSourceError::DimensionMismatch {
                        input: *input,
                        expected_width: dimensions.width(),
                        expected_height: dimensions.height(),
                        actual_width: actual.width(),
                        actual_height: actual.height(),
                    }
                    .into());
                }
            }
            layouts.push((
                *input,
                dimensions.width(),
                dimensions.height(),
                frames.len(),
            ));
        }
        let (dimensions, retained_rgba16f_bytes) = validate_source_layouts(&layouts, limits)?;

        let mut registry = BTreeMap::new();
        for (
            input,
            frames,
            offsets_ns,
            source_pts_origin,
            last_source_pts,
            last_sequence,
            end_of_stream,
            source_clock_domain,
            kind,
        ) in decoded_sources
        {
            let mut normalized_frames = Vec::with_capacity(frames.len());
            for frame in &frames {
                normalized_frames.push(
                    self.normalizer
                        .normalize(&self.context, frame)
                        .await
                        .map_err(|error| NativeSourcePreflightError::Normalize { input, error })?,
                );
            }
            registry.insert(
                input,
                NativeVideoPrefix {
                    frames: normalized_frames,
                    offsets_ns,
                    source_pts_origin,
                    last_source_pts,
                    last_sequence,
                    clock_domain: source_clock_domain,
                    kind,
                    end_of_stream,
                    in_flight: None,
                    available_for_stinger: false,
                    pinned_for_stinger: false,
                },
            );
        }
        let worker = NativeDecodeWorker::spawn(decoders)?;
        Ok(NativeSourcePlayback {
            registry: NativeSourceRegistry {
                sources: registry,
                dimensions,
                retained_rgba16f_bytes,
                limits,
            },
            worker,
            failed: false,
        })
    }

    /// Synchronous daemon wrapper for bounded source-prefix preflight.
    ///
    /// # Errors
    ///
    /// Returns a typed, path-free source-bound, decode-contract, decode, or
    /// normalization failure.
    pub fn preflight_resolved_sources_blocking(
        &self,
        adapter: &Adapter,
        sources: impl IntoIterator<Item = (InputId, PathBuf)>,
        clock_domain: ClockDomainId,
        selector: StreamSelector,
        limits: NativeSourceLimits,
    ) -> Result<NativeSourceRegistry, NativeSourcePreflightError> {
        block_on(self.preflight_resolved_sources_local_blocking(
            adapter,
            sources,
            clock_domain,
            selector,
            limits,
        ))
    }

    /// Synchronous daemon wrapper for bounded source-playback preflight.
    ///
    /// # Errors
    ///
    /// Returns a typed, path-free source-bound preflight or worker-start
    /// failure.
    pub fn preflight_resolved_source_playback_blocking(
        &self,
        adapter: &Adapter,
        sources: impl IntoIterator<Item = (InputId, PathBuf)>,
        clock_domain: ClockDomainId,
        selector: StreamSelector,
        limits: NativeSourceLimits,
    ) -> Result<NativeSourcePlayback, NativeSourcePreflightError> {
        block_on(self.preflight_resolved_source_playback_local_blocking(
            adapter,
            sources,
            clock_domain,
            selector,
            limits,
        ))
    }

    /// Synchronous wrapper for mixed retained-frame/local-video preflight.
    ///
    /// # Errors
    ///
    /// Returns a typed, path-free source-bound preflight or worker-start
    /// failure.
    pub fn preflight_resolved_source_playback_mixed_blocking(
        &self,
        adapter: Option<&Adapter>,
        sources: impl IntoIterator<Item = NativeResolvedSource>,
        clock_domain: ClockDomainId,
        selector: StreamSelector,
        limits: NativeSourceLimits,
    ) -> Result<NativeSourcePlayback, NativeSourcePreflightError> {
        block_on(
            self.preflight_resolved_source_playback_mixed_local_blocking(
                adapter,
                sources,
                clock_domain,
                selector,
                limits,
            ),
        )
    }

    /// Drains completed CPU decode pages without waiting, normalizes complete
    /// pages on this runtime, evicts frames before each source's floor anchor,
    /// and schedules bounded low-watermark refill.
    ///
    /// The returned value is `true` only when every source is at EOS or has a
    /// latest rebased PTS at or beyond `deadline`. A non-EOS source is never
    /// considered safe merely because it has a last retained frame.
    ///
    /// # Errors
    ///
    /// Returns a typed, fatal source contract, decode, normalization, or worker
    /// failure. Playback remains failed after the first error.
    pub async fn service_source_playback(
        &self,
        playback: &mut NativeSourcePlayback,
        deadline: ClockTime,
    ) -> Result<bool, NativeSourcePlaybackError> {
        if playback.failed {
            return Err(NativeSourcePlaybackError::Failed);
        }
        let result = self
            .service_source_playback_inner(playback, deadline, None)
            .await;
        if result.is_err() {
            playback.failed = true;
        }
        result
    }

    /// Services one source against an independent clip-local deadline.
    ///
    /// # Errors
    ///
    /// Returns a typed failed-playback, missing-source, decode, normalization,
    /// worker, timeline, or resource-bound failure.
    pub async fn service_source_playback_for_input(
        &self,
        playback: &mut NativeSourcePlayback,
        input: InputId,
        deadline: ClockTime,
    ) -> Result<bool, NativeSourcePlaybackError> {
        if playback.failed {
            return Err(NativeSourcePlaybackError::Failed);
        }
        if !playback
            .registry
            .sources
            .get(&input)
            .is_some_and(|prefix| prefix.available_for_stinger)
        {
            return Err(NativeSourcePlaybackError::MissingSource { input });
        }
        let result = self
            .service_source_playback_inner(playback, deadline, Some(input))
            .await;
        if result.is_err() {
            playback.failed = true;
        }
        result
    }

    /// Replaces the retained GPU frame for one live CPU source.
    ///
    /// Source timing is preserved exactly. Updates must retain the source clock
    /// and advance both PTS and sequence, though queue-induced sequence gaps are
    /// accepted. The operation keeps one frame and therefore does not grow the
    /// registry's retained-byte charge.
    ///
    /// # Errors
    ///
    /// Returns a typed source-kind, timeline, dimension, normalization, or
    /// previously-failed playback error. Playback remains failed after an error.
    pub async fn ingest_live_video_frame(
        &self,
        playback: &mut NativeSourcePlayback,
        input: InputId,
        frame: CpuVideoFrame,
    ) -> Result<(), NativeSourcePlaybackError> {
        if playback.failed {
            return Err(NativeSourcePlaybackError::Failed);
        }
        let result = self
            .ingest_live_video_frame_inner(playback, input, frame)
            .await;
        if result.is_err() {
            playback.failed = true;
        }
        result
    }

    async fn ingest_live_video_frame_inner(
        &self,
        playback: &mut NativeSourcePlayback,
        input: InputId,
        frame: CpuVideoFrame,
    ) -> Result<(), NativeSourcePlaybackError> {
        let prefix = playback
            .registry
            .sources
            .get(&input)
            .ok_or(NativeSourcePlaybackError::SourceNotLive { input })?;
        if prefix.kind != NativeVideoSourceKind::Live {
            return Err(NativeSourcePlaybackError::SourceNotLive { input });
        }
        validate_live_update_timing(
            input,
            &frame,
            prefix.last_source_pts,
            prefix.last_sequence,
            prefix.clock_domain,
        )?;
        let (expected_width, expected_height) = playback
            .registry
            .dimensions
            .ok_or(NativeSourceError::InvalidTimeline { input })?;
        let dimensions = frame.payload().dimensions();
        if (dimensions.width(), dimensions.height()) != (expected_width, expected_height) {
            return Err(NativeSourceError::DimensionMismatch {
                input,
                expected_width,
                expected_height,
                actual_width: dimensions.width(),
                actual_height: dimensions.height(),
            }
            .into());
        }
        let timing = frame.timing();
        let normalized = self
            .normalizer
            .normalize(&self.context, &frame)
            .await
            .map_err(|error| NativeSourcePlaybackError::Normalize { input, error })?;
        let prefix = playback
            .registry
            .sources
            .get_mut(&input)
            .ok_or(NativeSourcePlaybackError::SourceNotLive { input })?;
        prefix.frames.clear();
        prefix.frames.push(normalized);
        prefix.offsets_ns.clear();
        prefix.offsets_ns.push(0);
        prefix.last_source_pts = timing.presentation_timestamp().as_nanos();
        prefix.last_sequence = timing.sequence().get();
        Ok(())
    }

    /// Synchronous daemon wrapper for [`Self::ingest_live_video_frame`].
    ///
    /// # Errors
    ///
    /// Returns the same typed failures as the asynchronous operation.
    pub fn ingest_live_video_frame_blocking(
        &self,
        playback: &mut NativeSourcePlayback,
        input: InputId,
        frame: CpuVideoFrame,
    ) -> Result<(), NativeSourcePlaybackError> {
        block_on(self.ingest_live_video_frame(playback, input, frame))
    }

    #[allow(clippy::too_many_lines)]
    async fn service_source_playback_inner(
        &self,
        playback: &mut NativeSourcePlayback,
        deadline: ClockTime,
        only_input: Option<InputId>,
    ) -> Result<bool, NativeSourcePlaybackError> {
        let mut completed = Vec::with_capacity(playback.registry.sources.len());
        loop {
            match playback.worker.results.try_recv() {
                Ok(result) => completed.push(result),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err(playback.worker.disconnected_error());
                }
            }
        }

        for completed in completed {
            let request = completed.request;
            let input = request.input;
            let window = completed
                .window
                .map_err(|error| NativeSourcePlaybackError::Decode { input, error })?;
            let prefix = playback
                .registry
                .sources
                .get(&input)
                .ok_or(NativeSourcePlaybackError::DecodeContract { input })?;
            if prefix.kind != NativeVideoSourceKind::Decoded || prefix.in_flight != Some(request) {
                return Err(NativeSourcePlaybackError::DecodeContract { input });
            }
            let requested = usize::try_from(request.count.get()).unwrap_or(usize::MAX);
            if window.frames.len() > requested
                || (window.frames.is_empty() && !window.end_of_stream)
                || (!window.end_of_stream && window.frames.len() != requested)
            {
                return Err(NativeSourcePlaybackError::DecodeContract { input });
            }

            if window.frames.is_empty() {
                if request.operation == NativeDecodeOperation::Restart {
                    return Err(NativeSourcePlaybackError::DecodeContract { input });
                }
                let prefix = playback
                    .registry
                    .sources
                    .get_mut(&input)
                    .ok_or(NativeSourcePlaybackError::DecodeContract { input })?;
                prefix.in_flight = None;
                prefix.end_of_stream = true;
                continue;
            }

            let restarting = request.operation == NativeDecodeOperation::Restart;
            let source_pts_origin = if restarting {
                window.frames[0]
                    .timing()
                    .presentation_timestamp()
                    .as_nanos()
            } else {
                prefix.source_pts_origin
            };
            let previous_pts = (!restarting).then_some(prefix.last_source_pts);
            let expected_sequence = if restarting {
                0
            } else {
                prefix
                    .last_sequence
                    .checked_add(1)
                    .ok_or(NativeSourceError::InvalidTimeline { input })?
            };
            let (offsets_ns, last_source_pts, last_sequence) = validate_page_timing(
                input,
                &window.frames,
                source_pts_origin,
                previous_pts,
                expected_sequence,
                prefix.clock_domain,
            )?;
            let (expected_width, expected_height) = playback
                .registry
                .dimensions
                .ok_or(NativeSourceError::InvalidTimeline { input })?;
            for frame in &window.frames {
                let dimensions = frame.payload().dimensions();
                if (dimensions.width(), dimensions.height()) != (expected_width, expected_height) {
                    return Err(NativeSourceError::DimensionMismatch {
                        input,
                        expected_width,
                        expected_height,
                        actual_width: dimensions.width(),
                        actual_height: dimensions.height(),
                    }
                    .into());
                }
            }
            let retained_frames = if restarting { 0 } else { prefix.frames.len() };
            if retained_frames.saturating_add(window.frames.len())
                > usize::try_from(playback.registry.limits.max_video_frames_per_source.get())
                    .unwrap_or(usize::MAX)
            {
                return Err(NativeSourceError::TooManyFrames {
                    input,
                    actual: retained_frames.saturating_add(window.frames.len()),
                    maximum: playback.registry.limits.max_video_frames_per_source.get(),
                }
                .into());
            }
            let frame_bytes = rgba16f_frame_bytes(input, expected_width, expected_height)?;
            let batch_bytes = frame_bytes
                .checked_mul(u64::try_from(window.frames.len()).unwrap_or(u64::MAX))
                .ok_or(NativeSourceError::RetainedBytesExceeded {
                    required: u64::MAX,
                    maximum: playback.registry.limits.max_retained_rgba16f_bytes,
                })?;
            let replaced_bytes = frame_bytes
                .checked_mul(u64::try_from(prefix.frames.len()).unwrap_or(u64::MAX))
                .ok_or(NativeSourceError::RetainedBytesExceeded {
                    required: u64::MAX,
                    maximum: playback.registry.limits.max_retained_rgba16f_bytes,
                })?;
            let retained_bytes = if restarting {
                playback
                    .registry
                    .retained_rgba16f_bytes
                    .checked_sub(replaced_bytes)
                    .ok_or(NativeSourcePlaybackError::DecodeContract { input })?
            } else {
                playback.registry.retained_rgba16f_bytes
            };
            let required_bytes = retained_bytes.checked_add(batch_bytes).ok_or(
                NativeSourceError::RetainedBytesExceeded {
                    required: u64::MAX,
                    maximum: playback.registry.limits.max_retained_rgba16f_bytes,
                },
            )?;
            if required_bytes > playback.registry.limits.max_retained_rgba16f_bytes {
                return Err(NativeSourceError::RetainedBytesExceeded {
                    required: required_bytes,
                    maximum: playback.registry.limits.max_retained_rgba16f_bytes,
                }
                .into());
            }

            let mut normalized = Vec::with_capacity(window.frames.len());
            for frame in &window.frames {
                normalized.push(
                    self.normalizer
                        .normalize(&self.context, frame)
                        .await
                        .map_err(|error| NativeSourcePlaybackError::Normalize { input, error })?,
                );
            }

            let prefix = playback
                .registry
                .sources
                .get_mut(&input)
                .ok_or(NativeSourcePlaybackError::DecodeContract { input })?;
            if restarting {
                prefix.frames.clear();
                prefix.offsets_ns.clear();
                prefix.source_pts_origin = source_pts_origin;
            }
            prefix.frames.append(&mut normalized);
            prefix.offsets_ns.extend(offsets_ns);
            prefix.last_source_pts = last_source_pts;
            prefix.last_sequence = last_sequence;
            prefix.end_of_stream = window.end_of_stream;
            prefix.in_flight = None;
            playback.registry.retained_rgba16f_bytes = required_bytes;
        }

        if let Some((width, height)) = playback.registry.dimensions {
            let Some(accounting_input) = playback.registry.sources.keys().next().copied() else {
                return Ok(true);
            };
            let frame_bytes = rgba16f_frame_bytes(accounting_input, width, height)?;
            for (input, prefix) in &mut playback.registry.sources {
                if only_input.is_some_and(|selected| selected != *input) {
                    continue;
                }
                let remove =
                    source_eviction_count(&prefix.offsets_ns, prefix.pinned_for_stinger, deadline);
                prefix.frames.drain(..remove);
                prefix.offsets_ns.drain(..remove);
                let removed_bytes = frame_bytes
                    .checked_mul(u64::try_from(remove).unwrap_or(u64::MAX))
                    .ok_or(NativeSourceError::RetainedBytesExceeded {
                        required: u64::MAX,
                        maximum: playback.registry.limits.max_retained_rgba16f_bytes,
                    })?;
                playback.registry.retained_rgba16f_bytes = playback
                    .registry
                    .retained_rgba16f_bytes
                    .checked_sub(removed_bytes)
                    .ok_or(NativeSourcePlaybackError::DecodeContract { input: *input })?;
            }

            let mut reserved_bytes = playback
                .registry
                .sources
                .values()
                .filter_map(|prefix| prefix.in_flight.map(|request| (prefix, request)))
                .try_fold(0_u64, |reserved, (prefix, request)| {
                    let reserved_frames = if request.operation == NativeDecodeOperation::Restart {
                        usize::try_from(request.count.get())
                            .unwrap_or(usize::MAX)
                            .saturating_sub(prefix.frames.len())
                    } else {
                        usize::try_from(request.count.get()).unwrap_or(usize::MAX)
                    };
                    frame_bytes
                        .checked_mul(u64::try_from(reserved_frames).unwrap_or(u64::MAX))
                        .and_then(|page| reserved.checked_add(page))
                })
                .ok_or(NativeSourceError::RetainedBytesExceeded {
                    required: u64::MAX,
                    maximum: playback.registry.limits.max_retained_rgba16f_bytes,
                })?;
            let inputs = playback
                .registry
                .sources
                .keys()
                .copied()
                .collect::<Vec<_>>();
            for input in inputs {
                if only_input.is_some_and(|selected| selected != input) {
                    continue;
                }
                let prefix = playback
                    .registry
                    .sources
                    .get(&input)
                    .ok_or(NativeSourcePlaybackError::DecodeContract { input })?;
                if prefix.kind != NativeVideoSourceKind::Decoded {
                    continue;
                }
                let restart = prefix.available_for_stinger
                    && prefix
                        .offsets_ns
                        .first()
                        .is_some_and(|first| *first > deadline.as_nanos());
                let allocated = playback
                    .registry
                    .retained_rgba16f_bytes
                    .checked_add(reserved_bytes)
                    .ok_or(NativeSourceError::RetainedBytesExceeded {
                        required: u64::MAX,
                        maximum: playback.registry.limits.max_retained_rgba16f_bytes,
                    })?;
                let source_bytes = frame_bytes
                    .checked_mul(u64::try_from(prefix.frames.len()).unwrap_or(u64::MAX))
                    .ok_or(NativeSourceError::RetainedBytesExceeded {
                        required: u64::MAX,
                        maximum: playback.registry.limits.max_retained_rgba16f_bytes,
                    })?;
                let allocated = if restart {
                    allocated
                        .checked_sub(source_bytes)
                        .ok_or(NativeSourcePlaybackError::DecodeContract { input })?
                } else {
                    allocated
                };
                let budget_frames = playback
                    .registry
                    .limits
                    .max_retained_rgba16f_bytes
                    .saturating_sub(allocated)
                    / frame_bytes;
                let count = if restart && prefix.in_flight.is_none() {
                    let count =
                        u64::from(playback.registry.limits.max_video_frames_per_source.get())
                            .min(budget_frames);
                    u32::try_from(count).ok().and_then(NonZeroU32::new)
                } else {
                    refill_page_size(
                        prefix.frames.len(),
                        prefix.in_flight.is_some(),
                        prefix.end_of_stream,
                        playback.registry.limits.max_video_frames_per_source.get(),
                        budget_frames,
                    )
                };
                let Some(count) = count else { continue };
                let operation = if restart {
                    NativeDecodeOperation::Restart
                } else {
                    NativeDecodeOperation::Continue
                };
                let request = NativeDecodeRequest {
                    input,
                    count,
                    operation,
                };
                let retained_frames = prefix.frames.len();
                let Some(sender) = playback.worker.requests.as_ref() else {
                    return Err(playback.worker.disconnected_error());
                };
                match sender.try_send(request) {
                    Ok(()) => {}
                    Err(TrySendError::Disconnected(_)) => {
                        return Err(playback.worker.disconnected_error());
                    }
                    Err(TrySendError::Full(_)) => {
                        return Err(NativeSourcePlaybackError::WorkerQueueFull);
                    }
                }
                playback
                    .registry
                    .sources
                    .get_mut(&input)
                    .ok_or(NativeSourcePlaybackError::DecodeContract { input })?
                    .in_flight = Some(request);
                let reserved_frames = if restart {
                    usize::try_from(count.get())
                        .unwrap_or(usize::MAX)
                        .saturating_sub(retained_frames)
                } else {
                    usize::try_from(count.get()).unwrap_or(usize::MAX)
                };
                let page_bytes = frame_bytes
                    .checked_mul(u64::try_from(reserved_frames).unwrap_or(u64::MAX))
                    .ok_or(NativeSourceError::RetainedBytesExceeded {
                        required: u64::MAX,
                        maximum: playback.registry.limits.max_retained_rgba16f_bytes,
                    })?;
                reserved_bytes = reserved_bytes.checked_add(page_bytes).ok_or(
                    NativeSourceError::RetainedBytesExceeded {
                        required: u64::MAX,
                        maximum: playback.registry.limits.max_retained_rgba16f_bytes,
                    },
                )?;
            }
        }

        match only_input {
            Some(input) => playback
                .registry
                .sources
                .get(&input)
                .ok_or(NativeSourcePlaybackError::MissingSource { input })
                .map(|prefix| prefix.covers_deadline(deadline)),
            None => Ok(playback
                .registry
                .sources
                .values()
                .all(|prefix| prefix.covers_deadline(deadline))),
        }
    }

    /// Synchronous wrapper for [`Self::service_source_playback`].
    ///
    /// # Errors
    ///
    /// Returns a typed, fatal source contract, decode, normalization, or worker
    /// failure.
    pub fn service_source_playback_blocking(
        &self,
        playback: &mut NativeSourcePlayback,
        deadline: ClockTime,
    ) -> Result<bool, NativeSourcePlaybackError> {
        block_on(self.service_source_playback(playback, deadline))
    }

    /// Synchronous wrapper for [`Self::service_source_playback_for_input`].
    ///
    /// # Errors
    ///
    /// Returns the same failures as
    /// [`Self::service_source_playback_for_input`].
    pub fn service_source_playback_for_input_blocking(
        &self,
        playback: &mut NativeSourcePlayback,
        input: InputId,
        deadline: ClockTime,
    ) -> Result<bool, NativeSourcePlaybackError> {
        block_on(self.service_source_playback_for_input(playback, input, deadline))
    }

    /// Renders the engine's authoritative frame from retained GPU source
    /// prefixes. Source frames are selected by rebased PTS at the exact output
    /// deadline, with the final retained frame held only after confirmed EOS.
    /// A frame without a secondary is rendered as
    /// `Cut(primary, primary)`; a frame with one is rendered using its supported
    /// transition kind and exact numerator and denominator, then applies the
    /// authoritative FTB interval endpoint. This method performs no decode,
    /// normalization, source upload, or CPU readback.
    ///
    /// # Errors
    ///
    /// Returns a typed missing-source, registry-dimension, invalid-mix, or
    /// native compositor failure.
    pub async fn render_frame_result(
        &self,
        registry: &NativeSourceRegistry,
        frame: &FrameResult,
    ) -> Result<NativeTexture, NativeSourceRenderError> {
        let plan = native_mix_plan(frame.program)?;
        let fade_to_black = native_fade_to_black_plan(frame.fade_to_black)?;
        let primary = registry_frame(registry, plan.primary, frame.deadline)?;
        let secondary = registry_frame(registry, plan.secondary, frame.deadline)?;
        let program = self
            .renderer
            .render(
                &self.context,
                plan.transition,
                primary.texture(),
                secondary.texture(),
            )
            .await
            .map_err(NativeSourceRenderError::Compositor)?;
        self.fade_to_black_renderer
            .render(&self.context, fade_to_black, &program)
            .await
            .map_err(NativeSourceRenderError::FadeToBlack)
    }

    async fn render_project_scenes(
        &self,
        registry: &NativeSourceRegistry,
        project: &NativeProjectPlan,
        frame: &FrameResult,
        execution: &mut NativeSceneExecution,
    ) -> Result<BTreeMap<SceneId, (SourceId, NativeTexture)>, NativeSourceRenderError> {
        let mut outputs: BTreeMap<SceneId, (SourceId, NativeTexture)> = BTreeMap::new();
        for scene in &project.scenes {
            if !execution.required.contains(&scene.id) {
                continue;
            }
            let sources = scene
                .sources
                .iter()
                .map(|(token, source)| {
                    let texture = match source {
                        NativeSceneSource::Leaf(input) => {
                            registry_frame(registry, *input, frame.deadline)?.texture()
                        }
                        NativeSceneSource::Scene(dependency) => {
                            &outputs
                                .get(dependency)
                                .ok_or(NativeSourceRenderError::MissingSource {
                                    input: frame.program.primary,
                                })?
                                .1
                        }
                    };
                    Ok(NativeSourceFrame::new(*token, texture))
                })
                .collect::<Result<Vec<_>, NativeSourceRenderError>>()?;
            let output = self
                .composition_renderer
                .render(&self.context, &scene.composition, &sources)
                .await
                .map_err(NativeSourceRenderError::SceneCompositor)?;
            drop(sources);
            outputs.insert(scene.id, (scene.output, output));
            for dependency in scene.scene_dependencies() {
                let consumers = execution
                    .remaining_consumers
                    .get_mut(&dependency)
                    .ok_or(NativeSourceRenderError::ResourceBounds)?;
                *consumers = consumers
                    .checked_sub(1)
                    .ok_or(NativeSourceRenderError::ResourceBounds)?;
                if *consumers == 0 && !execution.roots.contains(&dependency) {
                    outputs.remove(&dependency);
                }
            }
        }
        debug_assert!(outputs.keys().all(|scene| execution.roots.contains(scene)));
        Ok(outputs)
    }

    /// Derives the active scene roots from the authoritative transition, renders
    /// only their dependency closure once in dependency-first order, releases
    /// non-root outputs after their last consumer, then applies Cut, Fade, or
    /// Wipe followed by the authoritative FTB interval endpoint.
    ///
    /// # Errors
    ///
    /// Returns a typed missing source, scene-composition, or transition failure.
    pub async fn render_project_frame_result(
        &self,
        registry: &NativeSourceRegistry,
        project: &NativeProjectPlan,
        frame: &FrameResult,
    ) -> Result<NativeTexture, NativeSourceRenderError> {
        self.render_project_frame_result_with_stingers(registry, registry, project, frame)
            .await
    }

    /// Renders one authoritative frame using an independent clip-local Stinger
    /// registry while normal Program and Preview sources remain on the show
    /// timeline.
    ///
    /// # Errors
    ///
    /// Returns a typed project route, source, scene, Stinger compositor, or
    /// transition failure.
    pub async fn render_project_frame_result_with_stingers(
        &self,
        registry: &NativeSourceRegistry,
        stingers: &NativeSourceRegistry,
        project: &NativeProjectPlan,
        frame: &FrameResult,
    ) -> Result<NativeTexture, NativeSourceRenderError> {
        let plan = native_project_mix_plan(project, frame.program)?;
        let fade_to_black = native_fade_to_black_plan(frame.fade_to_black)?;
        let inputs = match plan {
            NativeProjectMixPlan::Transition(plan) => [plan.primary, plan.secondary, plan.primary],
            NativeProjectMixPlan::Stinger(plan) => [plan.program, plan.preview, plan.media],
        };
        let mut execution = project
            .scene_execution(&inputs)
            .ok_or(NativeSourceRenderError::ResourceBounds)?;
        let _frame_slot = self.begin_project_frame()?;
        let scene_outputs = self
            .render_project_scenes(registry, project, frame, &mut execution)
            .await?;
        let program = match plan {
            NativeProjectMixPlan::Transition(plan) => {
                let primary = project_texture(
                    registry,
                    project,
                    &scene_outputs,
                    plan.primary,
                    frame.deadline,
                )?;
                let secondary = project_texture(
                    registry,
                    project,
                    &scene_outputs,
                    plan.secondary,
                    frame.deadline,
                )?;
                self.renderer
                    .render(&self.context, plan.transition, primary, secondary)
                    .await
                    .map_err(NativeSourceRenderError::Compositor)?
            }
            NativeProjectMixPlan::Stinger(plan) => {
                let program = project_texture(
                    registry,
                    project,
                    &scene_outputs,
                    plan.program,
                    frame.deadline,
                )?;
                let preview = project_texture(
                    registry,
                    project,
                    &scene_outputs,
                    plan.preview,
                    frame.deadline,
                )?;
                let media = match project.video_route(plan.media) {
                    Some(NativeVideoRoute::Leaf(input)) => registry_stinger_frame(
                        stingers,
                        input,
                        project.frame_rate,
                        plan.frame.frame_index(),
                    )?
                    .texture(),
                    Some(NativeVideoRoute::Scene(_)) | None => {
                        return Err(NativeSourceRenderError::StingerSourceNotPreloaded {
                            input: plan.media,
                        });
                    }
                };
                self.stinger_renderer
                    .render(&self.context, plan.frame, program, preview, media)
                    .await
                    .map_err(NativeSourceRenderError::Stinger)?
            }
        };
        let output = self
            .fade_to_black_renderer
            .render(&self.context, fade_to_black, &program)
            .await
            .map_err(NativeSourceRenderError::FadeToBlack)?;
        self.finish_project_frame()?;
        Ok(output)
    }

    /// Synchronous daemon wrapper for one authoritative program render.
    ///
    /// # Errors
    ///
    /// Returns a typed, path-free source-registry or compositor failure.
    pub fn render_frame_result_blocking(
        &self,
        registry: &NativeSourceRegistry,
        frame: &FrameResult,
    ) -> Result<NativeTexture, NativeSourceRenderError> {
        block_on(self.render_frame_result(registry, frame))
    }

    /// Synchronous daemon wrapper for scene, Program-transition, and FTB realization.
    ///
    /// # Errors
    ///
    /// Returns a typed project route, source, scene-composition, or transition failure.
    pub fn render_project_frame_result_blocking(
        &self,
        registry: &NativeSourceRegistry,
        project: &NativeProjectPlan,
        frame: &FrameResult,
    ) -> Result<NativeTexture, NativeSourceRenderError> {
        block_on(self.render_project_frame_result(registry, project, frame))
    }

    /// Synchronous wrapper for
    /// [`Self::render_project_frame_result_with_stingers`].
    ///
    /// # Errors
    ///
    /// Returns the same failures as
    /// [`Self::render_project_frame_result_with_stingers`].
    pub fn render_project_frame_result_with_stingers_blocking(
        &self,
        registry: &NativeSourceRegistry,
        stingers: &NativeSourceRegistry,
        project: &NativeProjectPlan,
        frame: &FrameResult,
    ) -> Result<NativeTexture, NativeSourceRenderError> {
        block_on(self.render_project_frame_result_with_stingers(registry, stingers, project, frame))
    }

    /// Renders a GPU-resident Cut, Fade, or Wipe between canonical RGBA16-float
    /// working frames. This production operation performs no CPU readback.
    ///
    /// # Errors
    ///
    /// Returns a typed compositor or GPU validation failure.
    pub async fn render_transition(
        &self,
        plan: TransitionPlan,
        from: &NativeWorkingFrame,
        to: &NativeWorkingFrame,
    ) -> Result<NativeTexture, NativeMediaError> {
        self.renderer
            .render(&self.context, plan, from.texture(), to.texture())
            .await
            .map_err(Into::into)
    }

    /// Creates a reusable fixed-size SDR Program readback owner on this
    /// runtime's context.
    ///
    /// Nonzero dimensions are further validated against the selected native
    /// adapter's texture limits by the existing GPU target API.
    ///
    /// # Errors
    ///
    /// Returns a typed GPU target or SDR transform pipeline failure.
    pub async fn create_program_readback(
        &self,
        width: NonZeroU32,
        height: NonZeroU32,
    ) -> Result<NativeProgramReadback, NativeMediaError> {
        let target = self
            .context
            .create_rgba8_render_target(width.get(), height.get())
            .await?;
        let transform = NativeSdrOutputTransform::new(&self.context).await?;
        Ok(NativeProgramReadback { target, transform })
    }

    /// Synchronously creates a reusable fixed-size SDR Program readback owner.
    ///
    /// # Errors
    ///
    /// Returns a typed GPU target or SDR transform pipeline failure.
    pub fn create_program_readback_blocking(
        &self,
        width: NonZeroU32,
        height: NonZeroU32,
    ) -> Result<NativeProgramReadback, NativeMediaError> {
        block_on(self.create_program_readback(width, height))
    }

    /// Transforms canonical `Rgba16Float` Program light to explicit
    /// sRGB-encoded Rec.709 in the owner's reusable `Rgba8Unorm` target, then
    /// returns tightly packed RGBA8 pixels.
    ///
    /// Existing transform and readback APIs validate source format, source and
    /// owner context, target role, and dimensions. This correctness path polls
    /// and maps synchronously and may block for up to the native readback
    /// timeout. Its exclusive owner borrow keeps transform plus readback
    /// single-flight for the reusable target. It is not a zero-copy production
    /// encoder bridge.
    ///
    /// # Errors
    ///
    /// Returns a typed GPU ownership, format, submission, polling, mapping,
    /// timeout, or layout failure.
    pub async fn readback_program(
        &self,
        owner: &mut NativeProgramReadback,
        program: &NativeTexture,
    ) -> Result<DiagnosticReadback, NativeMediaError> {
        owner
            .transform
            .transform(&self.context, program, &owner.target)
            .await?;
        self.context
            .readback_rgba8(&owner.target)
            .await
            .map_err(Into::into)
    }

    /// Synchronous wrapper for [`Self::readback_program`].
    ///
    /// This blocking correctness path may wait for up to the native readback
    /// timeout and is not a zero-copy production encoder bridge.
    ///
    /// # Errors
    ///
    /// Returns a typed GPU ownership, format, submission, polling, mapping,
    /// timeout, or layout failure.
    pub fn readback_program_blocking(
        &self,
        owner: &mut NativeProgramReadback,
        program: &NativeTexture,
    ) -> Result<DiagnosticReadback, NativeMediaError> {
        block_on(self.readback_program(owner, program))
    }

    /// Returns portable adapter identification for diagnostics and tests.
    #[must_use]
    pub const fn diagnostic_adapter_info(&self) -> &NativeAdapterInfo {
        self.context.adapter_info()
    }

    /// Reads a native texture back to CPU memory for diagnostics and tests.
    /// Production preroll and rendering never call this method.
    ///
    /// # Errors
    ///
    /// Returns a typed GPU polling, mapping, or validation failure.
    pub async fn diagnostic_readback(
        &self,
        texture: &NativeTexture,
    ) -> Result<NativeTextureReadback, NativeMediaError> {
        self.context.readback(texture).await.map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroU128, process::Command, thread, time::Duration};

    use fm_clock::ClockDomainId as EngineClockDomainId;
    use fm_command::{CommandEnvelope, IdempotencyKey, Revision, RuntimeGeneration};
    use fm_engine::{
        Engine, EngineCommand, EngineManualTransitionKind, EngineManualTransitionPosition,
        ShowState,
    };
    #[cfg(target_os = "macos")]
    use fm_frame::{
        AlphaMode, ChromaLocation, ColorMetadata, ColorPrimaries, CpuVideoPlane,
        MatrixCoefficients, SignalRange, TransferFunction, VideoFrameMetadata,
    };
    use fm_frame::{
        Channel, ChannelLayout, CpuVideoPayload, MediaTimestamp, NormalizedDuration,
        NormalizedTimestamp, OriginalTimestamp, PixelFormat, SampleRate, SequenceNumber, TimeBase,
        VideoDimensions,
    };
    use fm_model::{
        Input, InputAudioStripState, InputGainMilliDb, Layer, LayerGeometry, ProjectSettings,
        Rgba8 as ModelRgba8, Scene as ModelScene, SimulatedAudio, SimulatedInput, SimulatedVideo,
        StingerConfig, StingerSlotNumber,
    };
    use fm_persistence::{ProjectPosition, ProjectStore, RuntimeRouting, StoredProject};
    use fm_scheduler::FrameNumber;
    use fm_switcher::{
        SwitcherCommand, SwitcherState, TBarPosition, TransitionKind as SwitcherTransitionKind,
    };
    use fm_types::{ColorMetadata as ModelColorMetadata, ProjectId, ScanMode, VideoFormat};
    #[cfg(target_os = "macos")]
    use half::f16;

    use super::*;

    fn input(value: u128) -> InputId {
        InputId::new(NonZeroU128::new(value).unwrap())
    }

    fn scene(value: u128) -> SceneId {
        SceneId::new(NonZeroU128::new(value).unwrap())
    }

    fn native_plan_project(width: u32, height: u32) -> Project {
        let frame_rate = FrameRate::new(30, 1).unwrap();
        Project::new(
            ProjectId::new(NonZeroU128::new(1).unwrap()),
            "native plan",
            ProjectSettings {
                frame_rate,
                video: VideoFormat {
                    dimensions: VideoDimensions::new(width, height).unwrap(),
                    frame_rate,
                    pixel_format: PixelFormat::Rgba8,
                    scan: ScanMode::Progressive,
                    color: ModelColorMetadata::default(),
                },
                audio: mono_audio_format(),
            },
        )
    }

    fn add_leaf(project: &mut Project, id: InputId) {
        project.add_input(Input {
            id,
            name: format!("leaf {id}"),
            kind: InputKind::Simulated(SimulatedInput::new(
                SimulatedVideo::Bars,
                SimulatedAudio::Silence,
            )),
            required_capabilities: Vec::new(),
        });
    }

    fn add_scene_input(
        project: &mut Project,
        id: InputId,
        scene_id: SceneId,
        audio_source: Option<InputId>,
    ) {
        project.add_input(Input {
            id,
            name: format!("scene input {id}"),
            kind: InputKind::Scene {
                scene_id,
                audio_source,
            },
            required_capabilities: Vec::new(),
        });
    }

    fn layer(source: SourceRef, z_order: i32) -> Layer {
        Layer {
            name: "layer".into(),
            source,
            enabled: true,
            geometry: LayerGeometry::new(0, 0, 4, 2, Rotation::Deg0),
            crop: None,
            mask: None,
            opacity: u8::MAX,
            z_order,
        }
    }

    fn add_scene(project: &mut Project, id: SceneId, layers: Vec<Layer>) {
        project.add_scene(ModelScene {
            id,
            name: format!("scene {id}"),
            background: ModelRgba8::OPAQUE_BLACK,
            layers,
        });
    }

    fn assert_sample_exact(actual: f32, expected: f32) {
        assert_eq!(actual.to_bits(), expected.to_bits());
    }

    fn retained_frame(
        clock_domain: ClockDomainId,
        sequence: u64,
        presentation_timestamp: i64,
    ) -> CpuVideoFrame {
        let timing = MediaTiming::new(
            OriginalTimestamp::new(
                MediaTimestamp::new(presentation_timestamp),
                TimeBase::new(1, 1_000_000_000).unwrap(),
            ),
            NormalizedTimestamp::from_nanos(presentation_timestamp),
            NormalizedDuration::from_nanos(1).unwrap(),
            clock_domain,
            SequenceNumber::new(sequence),
        )
        .unwrap();
        let payload =
            CpuVideoPayload::allocate(PixelFormat::Rgba8, VideoDimensions::new(1, 1).unwrap())
                .unwrap();
        CpuVideoFrame::new(timing, payload)
    }

    #[cfg(target_os = "macos")]
    fn colored_retained_frame(clock_domain: ClockDomainId, rgba: [u8; 4]) -> CpuVideoFrame {
        solid_retained_frame(clock_domain, rgba, 1, 1)
    }

    #[cfg(target_os = "macos")]
    fn solid_retained_frame(
        clock_domain: ClockDomainId,
        rgba: [u8; 4],
        width: u32,
        height: u32,
    ) -> CpuVideoFrame {
        let timing = MediaTiming::new(
            OriginalTimestamp::new(
                MediaTimestamp::new(0),
                TimeBase::new(1, 1_000_000_000).unwrap(),
            ),
            NormalizedTimestamp::from_nanos(0),
            NormalizedDuration::from_nanos(1).unwrap(),
            clock_domain,
            SequenceNumber::new(0),
        )
        .unwrap();
        let width = usize::try_from(width).unwrap();
        let height = usize::try_from(height).unwrap();
        let mut bytes = Vec::with_capacity(width * height * 4);
        for _ in 0..width * height {
            bytes.extend_from_slice(&rgba);
        }
        let payload = CpuVideoPayload::new(
            PixelFormat::Rgba8,
            VideoDimensions::new(
                u32::try_from(width).unwrap(),
                u32::try_from(height).unwrap(),
            )
            .unwrap(),
            vec![CpuVideoPlane::new(width * 4, bytes).unwrap()],
        )
        .unwrap();
        CpuVideoFrame::new(timing, payload)
            .with_metadata(VideoFrameMetadata::new(
                ColorMetadata {
                    primaries: ColorPrimaries::Bt709,
                    transfer: TransferFunction::Srgb,
                    matrix: MatrixCoefficients::Identity,
                    range: SignalRange::Full,
                    chroma_location: ChromaLocation::Center,
                },
                Some(AlphaMode::Straight),
            ))
            .unwrap()
    }

    #[cfg(target_os = "macos")]
    fn rgba16f_components(bytes: &[u8]) -> [f32; 4] {
        std::array::from_fn(|component| {
            let offset = component * 2;
            f16::from_bits(u16::from_le_bytes([bytes[offset], bytes[offset + 1]])).to_f32()
        })
    }

    #[cfg(target_os = "macos")]
    fn create_local_stinger_fixture(directory: &std::path::Path) -> std::path::PathBuf {
        let raw = directory.join("stinger.rgba");
        let output = directory.join("stinger.mkv");
        let mut frames = Vec::with_capacity(12 * 2 * 2 * 4);
        for frame in 0..12 {
            let color = if frame == 11 {
                [255, 255, 0, 255]
            } else {
                [0, 255, 0, u8::try_from(frame * 23).unwrap()]
            };
            for _ in 0..4 {
                frames.extend_from_slice(&color);
            }
        }
        std::fs::write(&raw, frames).unwrap();
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "rawvideo",
                "-pixel_format",
                "rgba",
                "-video_size",
                "2x2",
                "-framerate",
                "30",
                "-i",
            ])
            .arg(&raw)
            .args([
                "-frames:v",
                "12",
                "-vf",
                "setsar=1/1,setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=full",
                "-c:v",
                "ffv1",
                "-pix_fmt",
                "bgra",
                "-color_primaries",
                "bt709",
                "-color_trc",
                "iec61966-2-1",
                "-colorspace",
                "rgb",
                "-color_range",
                "pc",
            ])
            .arg(&output)
            .status()
            .unwrap();
        assert!(status.success());
        output
    }

    fn mono_audio_format() -> AudioFormat {
        AudioFormat {
            sample_rate: SampleRate::new(48_000).unwrap(),
            sample_format: SampleFormat::F32,
            channels: ChannelLayout::new(vec![Channel::Mono]).unwrap(),
        }
    }

    fn audio_block(
        sequence: u64,
        start_sample: u64,
        samples: &[f32],
        format: &AudioFormat,
        clock_domain: ClockDomainId,
    ) -> AudioBlock {
        let end_sample = start_sample + u64::try_from(samples.len()).unwrap();
        AudioBlock::new(
            output_audio_timing(
                sequence,
                start_sample,
                end_sample,
                format.sample_rate.hertz(),
                clock_domain,
            )
            .unwrap(),
            format.sample_rate,
            format.channels.clone(),
            vec![samples.to_vec()],
        )
        .unwrap()
    }

    struct TestAudioChunk {
        start_sample: u64,
        samples: Vec<f32>,
    }

    fn audio_chunk(start_sample: u64, samples: &[f32]) -> TestAudioChunk {
        TestAudioChunk {
            start_sample,
            samples: samples.to_vec(),
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn audio_source(chunks: Vec<TestAudioChunk>, end_of_stream: bool) -> NativeAudioSource {
        audio_source_at_master(&chunks, end_of_stream, 0)
    }

    fn audio_source_at_master(
        chunks: &[TestAudioChunk],
        end_of_stream: bool,
        master_origin_sample: u64,
    ) -> NativeAudioSource {
        let format = mono_audio_format();
        let clock_domain = ClockDomainId::new(NonZeroU128::new(9).unwrap());
        let first_sample = chunks.first().map_or(0, |chunk| chunk.start_sample);
        let first_timestamp =
            normalized_sample_endpoint(i128::from(first_sample), format.sample_rate.hertz())
                .unwrap();
        let mapping_domain = MappingClockDomainId::new(clock_domain.get());
        let mapping = ClockMapping::new(
            ClockSnapshot::new(mapping_domain, ClockTime::ZERO),
            ClockSnapshot::new(mapping_domain, ClockTime::ZERO),
            0,
        )
        .unwrap();
        let mut synchronizer = ClockMappedAudioSynchronizer::new(
            format.sample_rate,
            format.sample_rate,
            format.channels.clone(),
            mapping,
            AudioCadenceOrigin::new(
                NormalizedTimestamp::from_nanos(first_timestamp),
                first_sample,
            ),
            AudioCadenceOrigin::new(
                NormalizedTimestamp::from_nanos(
                    normalized_sample_endpoint(
                        i128::from(master_origin_sample),
                        format.sample_rate.hertz(),
                    )
                    .unwrap(),
                ),
                master_origin_sample,
            ),
            AudioSynchronizerLimits::default(),
        )
        .unwrap();
        let blocks = chunks
            .iter()
            .enumerate()
            .map(|(sequence, chunk)| {
                audio_block(
                    u64::try_from(sequence).unwrap(),
                    chunk.start_sample,
                    &chunk.samples,
                    &format,
                    clock_domain,
                )
            })
            .collect::<Vec<_>>();
        synchronizer.push_batch(&blocks).unwrap();
        let next_sample = chunks.last().map_or(first_sample, |chunk| {
            chunk.start_sample + u64::try_from(chunk.samples.len()).unwrap()
        });
        NativeAudioSource::decoded(
            synchronizer,
            AudioCadenceOrigin::new(NormalizedTimestamp::from_nanos(0), 0),
            ValidatedAudioPage {
                next_sample,
                next_sequence: u64::try_from(chunks.len()).unwrap(),
                charge: NativeAudioCharge::default(),
            },
            0,
            end_of_stream,
            0,
            0,
        )
    }

    fn frame_result(frame: u64, primary: InputId, secondary: Option<InputId>) -> FrameResult {
        frame_result_with_mix(frame, primary, secondary, u32::from(secondary.is_some()), 2)
    }

    fn frame_result_with_mix(
        frame: u64,
        primary: InputId,
        secondary: Option<InputId>,
        mix_numerator: u32,
        mix_denominator: u32,
    ) -> FrameResult {
        let mix_end_numerator = if secondary.is_some() {
            mix_numerator.saturating_add(1).min(mix_denominator)
        } else {
            0
        };
        frame_result_with_interval(
            frame,
            primary,
            secondary,
            mix_numerator,
            mix_denominator,
            mix_numerator,
            mix_end_numerator,
        )
    }

    fn frame_result_with_interval(
        frame: u64,
        primary: InputId,
        secondary: Option<InputId>,
        mix_numerator: u32,
        mix_denominator: u32,
        mix_start_numerator: u32,
        mix_end_numerator: u32,
    ) -> FrameResult {
        frame_result_with_transition_interval(
            frame,
            primary,
            secondary,
            secondary.map(|_| SwitcherTransitionKind::Fade),
            mix_numerator,
            mix_denominator,
            mix_start_numerator,
            mix_end_numerator,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn frame_result_with_transition_interval(
        frame: u64,
        primary: InputId,
        secondary: Option<InputId>,
        transition_kind: Option<SwitcherTransitionKind>,
        mix_numerator: u32,
        mix_denominator: u32,
        mix_start_numerator: u32,
        mix_end_numerator: u32,
    ) -> FrameResult {
        FrameResult {
            fade_to_black: fm_switcher::FadeToBlackFrame::LIVE,
            frame: FrameNumber::new(frame),
            deadline: ClockTime::ZERO,
            program: ProgramFrame {
                primary,
                secondary,
                transition_kind,
                mix_numerator,
                mix_denominator,
                mix_start_numerator,
                mix_end_numerator,
            },
            events: Vec::new(),
            revision: Revision::new(0),
            runtime_generation: RuntimeGeneration::new(0),
        }
    }

    fn audio_test_master(sources: &[(InputId, f32)], sink_blocks: usize) -> NativeMasterRuntime {
        let format = mono_audio_format();
        let mut mixer = MasterMixer::new(format.clone()).unwrap();
        let mut audio_sources = BTreeMap::new();
        let samples_per_source = 3 * 1_920;
        for &(input, sample) in sources {
            mixer
                .add_input(
                    input,
                    format.clone(),
                    ChannelMapping::identity(format.channels.clone()).unwrap(),
                    InputState {
                        follow_video: true,
                        ..InputState::default()
                    },
                )
                .unwrap();
            audio_sources.insert(
                input,
                audio_source(
                    vec![audio_chunk(0, &vec![sample; samples_per_source])],
                    true,
                ),
            );
        }
        let pending_mixer = mixer.clone();
        let scratch = NativeAudioScratch::new(
            1,
            fm_audio::MAX_SAMPLES_PER_BLOCK,
            &audio_sources,
            NativeAudioLimits::default().max_retained_blocks,
        );
        NativeMasterRuntime {
            format,
            frame_rate: FrameRate::new(25, 1).unwrap(),
            clock_domain: ClockDomainId::new(NonZeroU128::new(9).unwrap()),
            expected_next_frame: 0,
            cadence_origin_frame: 0,
            ready_frame: None,
            mixer,
            pending_mixer,
            sink: CollectingAudioSink::new(sink_blocks, OverflowPolicy::DropOldest).unwrap(),
            sources: audio_sources,
            worker: NativeAudioDecodeWorker::spawn(BTreeMap::new()).unwrap(),
            limits: NativeAudioLimits {
                sink_blocks,
                ..NativeAudioLimits::default()
            },
            scratch,
            audio_telemetry: NativeAudioTelemetry::default(),
            stinger_audio: None,
            collect_output: true,
            failed: false,
        }
    }

    fn silent_test_master(input: InputId, sink_blocks: usize) -> NativeMasterRuntime {
        let format = mono_audio_format();
        let mut mixer = MasterMixer::new(format.clone()).unwrap();
        mixer
            .add_input(
                input,
                format.clone(),
                ChannelMapping::identity(format.channels.clone()).unwrap(),
                InputState {
                    follow_video: true,
                    ..InputState::default()
                },
            )
            .unwrap();
        let pending_mixer = mixer.clone();
        let sources = BTreeMap::from([(input, NativeAudioSource::silence())]);
        let scratch = NativeAudioScratch::new(
            1,
            fm_audio::MAX_SAMPLES_PER_BLOCK,
            &sources,
            NativeAudioLimits::default().max_retained_blocks,
        );
        NativeMasterRuntime {
            format,
            frame_rate: FrameRate::new(25, 1).unwrap(),
            clock_domain: ClockDomainId::new(NonZeroU128::new(9).unwrap()),
            expected_next_frame: 0,
            cadence_origin_frame: 0,
            ready_frame: None,
            mixer,
            pending_mixer,
            sink: CollectingAudioSink::new(sink_blocks, OverflowPolicy::DropOldest).unwrap(),
            sources,
            worker: NativeAudioDecodeWorker::spawn(BTreeMap::new()).unwrap(),
            limits: NativeAudioLimits {
                sink_blocks,
                ..NativeAudioLimits::default()
            },
            scratch,
            audio_telemetry: NativeAudioTelemetry::default(),
            stinger_audio: None,
            collect_output: true,
            failed: false,
        }
    }

    #[test]
    fn native_project_plan_accepts_empty_scene_and_ignores_unreachable_scene() {
        let mut leaf_only = native_plan_project(4, 2);
        add_leaf(&mut leaf_only, input(9));
        let leaf_plan =
            NativeProjectPlan::compile(&leaf_only, NativeProjectLimits::default()).unwrap();
        assert_eq!(leaf_plan.peak_rgba16f_targets(), 3);
        assert_eq!(leaf_plan.transient_rgba16f_bytes(), 3 * 4 * 2 * 8);

        let mut project = native_plan_project(4, 2);
        add_scene_input(&mut project, input(1), scene(10), None);
        add_scene(&mut project, scene(10), Vec::new());
        add_scene(&mut project, scene(99), Vec::new());

        let plan = NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();

        assert_eq!(plan.scene_order().collect::<Vec<_>>(), vec![scene(10)]);
        assert_eq!(plan.active_layer_count(), 0);
        assert_eq!(plan.peak_rgba16f_targets(), 4);
        assert_eq!(plan.transient_rgba16f_bytes(), 4 * 4 * 2 * 8);
        assert_eq!(
            plan.video_route(input(1)),
            Some(NativeVideoRoute::Scene(scene(10)))
        );
        assert_eq!(plan.audio_route(input(1)), Some(NativeAudioRoute::Silence));
    }

    #[test]
    fn native_project_plan_orders_nested_shared_scenes_dependency_first() {
        let mut project = native_plan_project(4, 2);
        add_leaf(&mut project, input(1));
        add_scene_input(&mut project, input(2), scene(20), Some(input(1)));
        add_scene_input(&mut project, input(3), scene(30), Some(input(1)));
        add_scene(
            &mut project,
            scene(10),
            vec![layer(SourceRef::Input(input(1)), 0)],
        );
        add_scene(
            &mut project,
            scene(20),
            vec![layer(SourceRef::Scene(scene(10)), 0)],
        );
        add_scene(
            &mut project,
            scene(30),
            vec![
                layer(SourceRef::Scene(scene(10)), 0),
                layer(SourceRef::Scene(scene(10)), 1),
            ],
        );

        let plan = NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();

        assert_eq!(
            plan.scene_order().collect::<Vec<_>>(),
            vec![scene(10), scene(20), scene(30)]
        );
        assert_eq!(plan.active_layer_count(), 4);
        assert_eq!(plan.peak_rgba16f_targets(), 6);
        assert_eq!(plan.transient_rgba16f_bytes(), 6 * 4 * 2 * 8);
        assert_eq!(
            NativeProjectPlan::compile(
                &project,
                NativeProjectLimits {
                    max_transient_rgba16f_bytes: 383,
                    ..NativeProjectLimits::default()
                }
            ),
            Err(NativeProjectPlanError::TransientBytesExceeded {
                required: 384,
                maximum: 383,
            })
        );
        let primary_only = plan.scene_execution(&[input(2), input(2)]).unwrap();
        assert_eq!(
            primary_only.required,
            BTreeSet::from([scene(10), scene(20)])
        );
        assert!(!primary_only.required.contains(&scene(30)));
    }

    #[test]
    fn native_project_plan_accounts_for_three_independent_stinger_scene_roots() {
        let mut project = native_plan_project(4, 2);
        for (input_id, scene_id) in [
            (input(1), scene(10)),
            (input(2), scene(20)),
            (input(3), scene(30)),
        ] {
            add_scene_input(&mut project, input_id, scene_id, None);
            add_scene(&mut project, scene_id, Vec::new());
        }
        project.add_stinger(StingerConfig::new(
            StingerSlotNumber::new(1).unwrap(),
            input(3),
            true,
            1,
            fm_model::StingerAudioPolicy::Muted,
            fm_model::StingerMissingMediaFallback::Cut,
        ));

        let plan = NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();

        assert_eq!(plan.peak_rgba16f_targets(), 6);
        assert_eq!(plan.transient_rgba16f_bytes(), 6 * 4 * 2 * 8);
        let execution = plan
            .scene_execution(&[input(1), input(2), input(3)])
            .unwrap();
        assert_eq!(
            execution.required,
            BTreeSet::from([scene(10), scene(20), scene(30)])
        );
    }

    #[test]
    fn native_project_plan_source_tokens_do_not_narrow_high_u128_ids() {
        let low = input(7);
        let high = input((1_u128 << 64) + 7);
        let mut project = native_plan_project(4, 2);
        add_leaf(&mut project, low);
        add_leaf(&mut project, high);
        add_scene_input(&mut project, input(3), scene(10), Some(low));
        add_scene(
            &mut project,
            scene(10),
            vec![
                layer(SourceRef::Input(low), 0),
                layer(SourceRef::Input(high), 1),
            ],
        );

        let plan = NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();
        let sources = &plan.scenes[0].sources;

        assert_eq!(sources.len(), 2);
        assert!(
            sources
                .iter()
                .any(|(_, source)| *source == NativeSceneSource::Leaf(low))
        );
        assert!(
            sources
                .iter()
                .any(|(_, source)| *source == NativeSceneSource::Leaf(high))
        );
        assert_ne!(sources[0].0, sources[1].0);
        assert_eq!(plan.video_route(low), Some(NativeVideoRoute::Leaf(low)));
        assert_eq!(plan.video_route(high), Some(NativeVideoRoute::Leaf(high)));
    }

    #[test]
    fn native_project_plan_maps_scene_visual_model_exactly() {
        let mut project = native_plan_project(4, 2);
        add_leaf(&mut project, input(1));
        add_scene_input(&mut project, input(2), scene(10), Some(input(1)));
        project.add_scene(ModelScene {
            id: scene(10),
            name: "mapped".into(),
            background: ModelRgba8::new(12, 24, 36, 48),
            layers: vec![Layer {
                name: "mapped layer".into(),
                source: SourceRef::Input(input(1)),
                enabled: true,
                geometry: LayerGeometry::new(-7, 9, 3, 2, Rotation::Deg270),
                crop: Some(fm_model::CropRect::new(1, 0, 2, 2)),
                mask: Some(fm_model::RectMask::new(1, 0, 1, 2).inverted(true)),
                opacity: 123,
                z_order: -4,
            }],
        });

        let plan = NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();
        let composition = &plan.scenes[0].composition;
        let mapped = &composition.layers()[0];

        assert_eq!(
            composition.background(),
            CompositorRgba8::new(12, 24, 36, 48)
        );
        assert_eq!(mapped.z(), -4);
        assert_eq!(
            mapped.transform(),
            Transform::new(-7, 9, 3, 2, CompositorRotation::Deg270)
        );
        assert_eq!(
            mapped.crop(),
            Some(fm_compositor::CropRect::new(1, 0, 2, 2))
        );
        assert_eq!(
            mapped.mask(),
            Some(fm_compositor::RectMask::new(1, 0, 1, 2).inverted(true))
        );
        assert_eq!(mapped.opacity(), 123);
    }

    #[test]
    fn native_project_plan_admits_mask_bounds_without_changing_resource_accounting() {
        let build = |mask| {
            let mut project = native_plan_project(4, 2);
            add_leaf(&mut project, input(1));
            add_scene_input(&mut project, input(2), scene(10), Some(input(1)));
            project.add_scene(ModelScene {
                id: scene(10),
                name: "mask admission".into(),
                background: ModelRgba8::OPAQUE_BLACK,
                layers: vec![Layer {
                    name: "masked".into(),
                    source: SourceRef::Input(input(1)),
                    enabled: true,
                    geometry: LayerGeometry::new(1, 0, 3, 2, Rotation::Deg180),
                    crop: Some(fm_model::CropRect::new(1, 0, 3, 2)),
                    mask,
                    opacity: u8::MAX,
                    z_order: 0,
                }],
            });
            project
        };

        let unmasked =
            NativeProjectPlan::compile(&build(None), NativeProjectLimits::default()).unwrap();
        let masked = NativeProjectPlan::compile(
            &build(Some(fm_model::RectMask::new(1, 0, 1, 2))),
            NativeProjectLimits::default(),
        )
        .unwrap();
        assert_eq!(
            (
                masked.active_layer_count(),
                masked.peak_rgba16f_targets(),
                masked.transient_rgba16f_bytes(),
            ),
            (
                unmasked.active_layer_count(),
                unmasked.peak_rgba16f_targets(),
                unmasked.transient_rgba16f_bytes(),
            )
        );

        let source_token = masked.scenes[0].sources[0].0;
        let source = fm_compositor::ImageFrame::new(
            4,
            2,
            16,
            [
                CompositorRgba8::new(255, 0, 0, 255),
                CompositorRgba8::new(255, 255, 0, 255),
                CompositorRgba8::new(0, 0, 255, 255),
                CompositorRgba8::new(255, 0, 255, 255),
                CompositorRgba8::new(0, 255, 255, 255),
                CompositorRgba8::new(255, 255, 255, 255),
                CompositorRgba8::new(0, 255, 0, 255),
                CompositorRgba8::new(128, 128, 128, 255),
            ]
            .into_iter()
            .flat_map(fm_compositor::Rgba8::to_bytes)
            .collect(),
        )
        .unwrap();
        let output = fm_compositor::execute_cpu(
            &masked.scenes[0].composition,
            &[fm_compositor::CpuSourceFrame::new(source_token, &source)],
        )
        .unwrap();
        assert_eq!(
            (0..8)
                .map(|index| output.pixel(index % 4, index / 4).unwrap())
                .collect::<Vec<_>>(),
            vec![
                CompositorRgba8::new(0, 0, 0, 255),
                CompositorRgba8::new(0, 0, 0, 255),
                CompositorRgba8::new(0, 255, 0, 255),
                CompositorRgba8::new(0, 0, 0, 255),
                CompositorRgba8::new(0, 0, 0, 255),
                CompositorRgba8::new(0, 0, 0, 255),
                CompositorRgba8::new(0, 0, 255, 255),
                CompositorRgba8::new(0, 0, 0, 255),
            ]
        );

        for invalid in [
            fm_model::RectMask::new(0, 0, 0, 1),
            fm_model::RectMask::new(3, 0, 1, 1),
            fm_model::RectMask::new(u32::MAX, 0, 2, 1),
        ] {
            assert!(matches!(
                NativeProjectPlan::compile(&build(Some(invalid)), NativeProjectLimits::default()),
                Err(NativeProjectPlanError::Composition {
                    error: PlanError::InvalidMask { layer: 0 },
                    ..
                })
            ));
        }
    }

    #[test]
    fn native_project_plan_rejects_65_total_active_layers() {
        let mut project = native_plan_project(4, 2);
        add_leaf(&mut project, input(1));
        add_scene_input(&mut project, input(2), scene(10), Some(input(1)));
        add_scene(
            &mut project,
            scene(10),
            (0_i32..65)
                .map(|z| layer(SourceRef::Input(input(1)), z))
                .collect(),
        );

        assert_eq!(
            NativeProjectPlan::compile(&project, NativeProjectLimits::default()),
            Err(NativeProjectPlanError::TooManyActiveLayers {
                actual: 65,
                maximum: 64,
            })
        );
    }

    #[test]
    fn native_project_plan_routes_recursive_audio_without_collapsing_logical_inputs() {
        let mut project = native_plan_project(4, 2);
        add_leaf(&mut project, input(1));
        add_scene_input(&mut project, input(2), scene(20), Some(input(1)));
        add_scene_input(&mut project, input(3), scene(30), Some(input(2)));
        add_scene_input(&mut project, input(4), scene(40), None);
        for id in [20, 30, 40] {
            add_scene(&mut project, scene(id), Vec::new());
        }
        let plan = NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();

        assert_eq!(
            plan.audio_route(input(3)),
            Some(NativeAudioRoute::Leaf(input(1)))
        );
        assert_eq!(
            plan.audio_route(input(2)),
            Some(NativeAudioRoute::Leaf(input(1)))
        );
        assert_eq!(plan.audio_route(input(4)), Some(NativeAudioRoute::Silence));
        assert_ne!(input(2), input(3));
    }

    #[test]
    fn native_project_explicit_silence_renders_without_a_physical_source() {
        let mut project = native_plan_project(4, 2);
        add_scene_input(&mut project, input(1), scene(10), None);
        add_scene(&mut project, scene(10), Vec::new());
        let plan = NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();
        let mut master = NativeMasterRuntime::preflight_project_local_blocking(
            None,
            &[],
            &plan,
            &mono_audio_format(),
            FrameRate::new(30, 1).unwrap(),
            ClockDomainId::new(NonZeroU128::new(9).unwrap()),
            0,
            NativeAudioLimits::default(),
        )
        .unwrap();
        assert!(master.service_next_frame().unwrap());

        let block = master
            .render_project_frame_audio(&frame_result(0, input(1), None), &plan)
            .unwrap();

        assert_eq!(block.sample_count(), 1_600);
        assert!(block.planes()[0].iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn inactive_physical_strip_without_follow_video_remains_audible() {
        let active = input(1);
        let inactive = input(2);
        let mut project = native_plan_project(4, 2);
        add_leaf(&mut project, active);
        add_leaf(&mut project, inactive);
        assert!(project.set_input_audio_strip(
            inactive,
            InputAudioStripState {
                follow_video: false,
                ..InputAudioStripState::default()
            },
        ));
        let plan = NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();
        let mut master = audio_test_master(&[(active, 0.25), (inactive, 0.5)], 1);
        master.realize_project_audio(&plan).unwrap();

        assert!(master.service_next_frame().unwrap());
        let output = master
            .render_project_frame_audio(&frame_result(0, active, None), &plan)
            .unwrap();

        for sample in output.plane(0).unwrap() {
            assert_sample_exact(*sample, 0.75);
        }
    }

    #[test]
    fn inactive_scene_alias_without_follow_video_remains_audible() {
        let physical = input(1);
        let active = input(2);
        let alias = input(3);
        let mut project = native_plan_project(4, 2);
        add_leaf(&mut project, physical);
        add_leaf(&mut project, active);
        add_scene_input(&mut project, alias, scene(10), Some(physical));
        add_scene(&mut project, scene(10), Vec::new());
        assert!(project.set_input_audio_strip(
            alias,
            InputAudioStripState {
                follow_video: false,
                ..InputAudioStripState::default()
            },
        ));
        let plan = NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();
        let mut master = audio_test_master(&[(physical, 0.5), (active, 0.25)], 1);
        master.realize_project_audio(&plan).unwrap();

        assert!(master.service_next_frame().unwrap());
        let output = master
            .render_project_frame_audio(&frame_result(0, active, None), &plan)
            .unwrap();

        for sample in output.plane(0).unwrap() {
            assert_sample_exact(*sample, 0.75);
        }
    }

    #[test]
    fn project_store_restart_realizes_persisted_strips_in_master_output() {
        let active = input(1);
        let inactive = input(2);
        let mut project = native_plan_project(4, 2);
        add_leaf(&mut project, active);
        add_leaf(&mut project, inactive);
        assert!(project.set_input_audio_strip(
            inactive,
            InputAudioStripState {
                gain: InputGainMilliDb::new(-6_021).unwrap(),
                muted: false,
                follow_video: false,
            },
        ));
        let stored = StoredProject::from_project(
            project,
            RuntimeRouting::default(),
            ProjectPosition::default(),
            Vec::new(),
        )
        .unwrap();
        let root = std::env::temp_dir().join(format!(
            "freemixd-native-audio-restart-{}-{}.freemix",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        ProjectStore::new(&root).unwrap().save(&stored).unwrap();
        let restarted = ProjectStore::new(&root).unwrap().load().unwrap();
        let plan = NativeProjectPlan::compile(restarted.project(), NativeProjectLimits::default())
            .unwrap();
        let inactive_gain = native_input_state(&plan, inactive).unwrap().gain.linear();
        let mut master = audio_test_master(&[(active, 0.25), (inactive, 0.5)], 1);
        master.realize_project_audio(&plan).unwrap();

        assert!(master.service_next_frame().unwrap());
        let output = master
            .render_project_frame_audio(&frame_result(0, active, None), &plan)
            .unwrap();
        let expected = 0.25 + 0.5 * inactive_gain;
        for sample in output.plane(0).unwrap() {
            assert!((*sample - expected).abs() < 1.0e-6);
        }

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_leaf_aliases_keep_independent_strip_state_and_transition_gains() {
        let physical = input(1);
        let primary = input(2);
        let secondary = input(3);
        let mut project = native_plan_project(4, 2);
        add_leaf(&mut project, physical);
        add_scene_input(&mut project, primary, scene(10), Some(physical));
        add_scene_input(&mut project, secondary, scene(20), Some(physical));
        add_scene(&mut project, scene(10), Vec::new());
        add_scene(&mut project, scene(20), Vec::new());
        assert!(project.set_input_audio_strip(
            primary,
            InputAudioStripState {
                gain: InputGainMilliDb::new(-6_021).unwrap(),
                muted: false,
                follow_video: true,
            },
        ));
        assert!(project.set_input_audio_strip(
            secondary,
            InputAudioStripState {
                gain: InputGainMilliDb::UNITY,
                muted: false,
                follow_video: true,
            },
        ));
        let plan = NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();
        let primary_gain = native_input_state(&plan, primary).unwrap().gain.linear();
        let transition =
            frame_result_with_interval(0, primary, Some(secondary), 2_500, 10_000, 2_500, 2_500);
        let mut master = audio_test_master(&[(physical, 1.0)], 1);
        master.realize_project_audio(&plan).unwrap();

        assert!(master.service_next_frame().unwrap());
        let output = master
            .render_project_frame_audio(&transition, &plan)
            .unwrap();
        let expected = primary_gain * 0.75 + 0.25;
        for sample in output.plane(0).unwrap() {
            assert!((*sample - expected).abs() < 1.0e-6);
        }

        assert!(project.set_input_audio_strip(
            secondary,
            InputAudioStripState {
                gain: InputGainMilliDb::UNITY,
                muted: true,
                follow_video: true,
            },
        ));
        let muted_plan =
            NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();
        let mut muted_master = audio_test_master(&[(physical, 1.0)], 1);
        muted_master.realize_project_audio(&muted_plan).unwrap();
        assert!(muted_master.service_next_frame().unwrap());
        let muted = muted_master
            .render_project_frame_audio(&transition, &muted_plan)
            .unwrap();
        for sample in muted.plane(0).unwrap() {
            assert!((*sample - primary_gain * 0.75).abs() < 1.0e-6);
        }
    }

    #[test]
    fn native_project_applies_persisted_strip_state_immediately_and_transactionally() {
        let mut project = native_plan_project(4, 2);
        add_leaf(&mut project, input(1));
        add_scene_input(&mut project, input(2), scene(10), Some(input(1)));
        add_scene(&mut project, scene(10), Vec::new());
        let persisted = InputAudioStripState {
            gain: InputGainMilliDb::new(-12_000).unwrap(),
            muted: true,
            follow_video: false,
        };
        assert!(project.set_input_audio_strip(input(1), persisted));
        let alias_persisted = InputAudioStripState {
            gain: InputGainMilliDb::new(-6_000).unwrap(),
            muted: false,
            follow_video: true,
        };
        assert!(project.set_input_audio_strip(input(2), alias_persisted));
        let mut plan =
            NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();
        let source = NativeResolvedSource::RetainedFrame {
            input: input(1),
            frame: retained_frame(ClockDomainId::new(NonZeroU128::new(9).unwrap()), 0, 0),
        };
        let mut master = NativeMasterRuntime::preflight_project_local_blocking(
            None,
            &[source],
            &plan,
            &mono_audio_format(),
            FrameRate::new(30, 1).unwrap(),
            ClockDomainId::new(NonZeroU128::new(9).unwrap()),
            0,
            NativeAudioLimits::default(),
        )
        .unwrap();
        let realized = master.mixer.input_state(input(1)).unwrap();
        assert!(realized.muted);
        assert!(!realized.follow_video);
        assert!((realized.gain.db() + 12.0).abs() < 0.000_1);
        assert_eq!(
            master.mixer.current_linear_gain(input(1)),
            Some(realized.gain.linear())
        );
        let alias = master.mixer.input_state(input(2)).unwrap();
        assert!((alias.gain.db() + 6.0).abs() < 0.000_1);
        assert!(!alias.muted);
        assert!(alias.follow_video);

        let before = master.mixer.input_state(input(1));
        plan.audio_strips.remove(&input(1));
        assert!(matches!(
            master.apply_project_audio_strips(&plan),
            Err(NativeMasterError::MissingAudioRoute { input: missing }) if missing == input(1)
        ));
        assert_eq!(master.mixer.input_state(input(1)), before);
    }

    #[test]
    fn persisted_gain_mute_and_follow_video_control_generated_audio() {
        let mut project = native_plan_project(4, 2);
        add_leaf(&mut project, input(1));
        assert!(project.set_input_audio_strip(
            input(1),
            InputAudioStripState {
                gain: InputGainMilliDb::new(-6_021).unwrap(),
                muted: false,
                follow_video: true,
            },
        ));
        let plan = NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();
        let state = native_input_state(&plan, input(1)).unwrap();
        let format = mono_audio_format();
        let block = fm_audio::AudioBlock::from_planar(format.clone(), vec![vec![1.0; 4]]).unwrap();
        let mut mixer = MasterMixer::new(format.clone()).unwrap();
        mixer
            .add_input(
                input(1),
                format,
                ChannelMapping::identity(mono_audio_format().channels).unwrap(),
                state,
            )
            .unwrap();

        let inactive = mixer.mix(4, &[(input(1), &block)], None).unwrap();
        assert!(
            inactive
                .block
                .plane(0)
                .unwrap()
                .iter()
                .all(|sample| *sample == 0.0)
        );
        let selected = mixer.mix(4, &[(input(1), &block)], Some(input(1))).unwrap();
        assert!(
            selected
                .block
                .plane(0)
                .unwrap()
                .iter()
                .all(|sample| (*sample - state.gain.linear()).abs() < 0.000_1)
        );

        mixer
            .set_input_state(
                input(1),
                InputState {
                    muted: true,
                    ..state
                },
                0,
            )
            .unwrap();
        let muted = mixer.mix(4, &[(input(1), &block)], Some(input(1))).unwrap();
        assert!(
            muted
                .block
                .plane(0)
                .unwrap()
                .iter()
                .all(|sample| *sample == 0.0)
        );
    }

    #[test]
    fn native_project_plan_reports_scene_cycles_missing_resources_and_bounds() {
        let mut cyclic = native_plan_project(4, 2);
        add_scene_input(&mut cyclic, input(1), scene(10), None);
        add_scene(
            &mut cyclic,
            scene(10),
            vec![layer(SourceRef::Scene(scene(20)), 0)],
        );
        add_scene(
            &mut cyclic,
            scene(20),
            vec![layer(SourceRef::Scene(scene(10)), 0)],
        );
        assert!(matches!(
            NativeProjectPlan::compile(&cyclic, NativeProjectLimits::default()),
            Err(NativeProjectPlanError::SceneCycle { .. })
        ));

        let mut missing = native_plan_project(4, 2);
        add_scene_input(&mut missing, input(1), scene(10), None);
        add_scene(
            &mut missing,
            scene(10),
            vec![layer(SourceRef::Input(input(99)), 0)],
        );
        assert_eq!(
            NativeProjectPlan::compile(&missing, NativeProjectLimits::default()),
            Err(NativeProjectPlanError::MissingInput(input(99)))
        );

        let mut audio_cycle = native_plan_project(4, 2);
        add_scene_input(&mut audio_cycle, input(1), scene(10), Some(input(2)));
        add_scene_input(&mut audio_cycle, input(2), scene(20), Some(input(1)));
        add_scene(&mut audio_cycle, scene(10), Vec::new());
        add_scene(&mut audio_cycle, scene(20), Vec::new());
        assert!(matches!(
            NativeProjectPlan::compile(&audio_cycle, NativeProjectLimits::default()),
            Err(NativeProjectPlanError::AudioCycle { .. })
        ));

        let mut bounded = native_plan_project(4, 2);
        add_scene_input(&mut bounded, input(1), scene(10), None);
        add_scene(&mut bounded, scene(10), Vec::new());
        assert_eq!(
            NativeProjectPlan::compile(
                &bounded,
                NativeProjectLimits {
                    max_reachable_scenes: 0,
                    ..NativeProjectLimits::default()
                },
            ),
            Err(NativeProjectPlanError::TooManyReachableScenes {
                actual: 1,
                maximum: 0,
            })
        );
        assert_eq!(
            NativeProjectPlan::compile(
                &bounded,
                NativeProjectLimits {
                    max_transient_rgba16f_bytes: 191,
                    ..NativeProjectLimits::default()
                },
            ),
            Err(NativeProjectPlanError::TransientBytesExceeded {
                required: 256,
                maximum: 191,
            })
        );
    }

    #[test]
    fn aggregate_errors_preserve_typed_sources_without_paths() {
        let error = NativeMediaError::from(fm_codec_ffmpeg::Error::InputNotFound);
        assert!(error.source().is_some());
        assert_eq!(
            error.to_string(),
            "local media decode failed: input file was not found"
        );
    }

    #[test]
    fn program_readback_blocking_api_has_owned_private_target_contract() {
        let _: fn(
            &NativeMediaRuntime,
            NonZeroU32,
            NonZeroU32,
        ) -> Result<NativeProgramReadback, NativeMediaError> =
            NativeMediaRuntime::create_program_readback_blocking;
        let _: fn(
            &NativeMediaRuntime,
            &mut NativeProgramReadback,
            &NativeTexture,
        ) -> Result<DiagnosticReadback, NativeMediaError> =
            NativeMediaRuntime::readback_program_blocking;
    }

    #[test]
    fn resolved_source_accessors_preserve_full_width_ids() {
        let local_input = input(1);
        let retained_input = input((1_u128 << 96) + 1);
        let live_input = input((1_u128 << 112) + 1);
        let clock_domain = ClockDomainId::new(NonZeroU128::new(7).unwrap());
        let local = NativeResolvedSource::LocalVideo {
            input: local_input,
            path: PathBuf::from("video.mov"),
        };
        let retained = NativeResolvedSource::RetainedFrame {
            input: retained_input,
            frame: retained_frame(clock_domain, 0, -42),
        };
        let live = NativeResolvedSource::LiveFrame {
            input: live_input,
            frame: retained_frame(clock_domain, 41, -21),
        };

        assert_eq!(local.input(), local_input);
        assert_eq!(retained.input(), retained_input);
        assert_eq!(live.input(), live_input);
        assert!(matches!(local, NativeResolvedSource::LocalVideo { .. }));
        assert!(matches!(
            retained,
            NativeResolvedSource::RetainedFrame { .. }
        ));
        assert!(matches!(live, NativeResolvedSource::LiveFrame { .. }));
    }

    #[test]
    fn mixed_source_validation_precedes_adapter_use() {
        let duplicate = input((1_u128 << 80) + 3);
        let clock_domain = ClockDomainId::new(NonZeroU128::new(7).unwrap());
        let sources = vec![
            NativeResolvedSource::LocalVideo {
                input: duplicate,
                path: PathBuf::from("video.mov"),
            },
            NativeResolvedSource::RetainedFrame {
                input: duplicate,
                frame: retained_frame(clock_domain, 0, 0),
            },
        ];
        assert!(matches!(
            validate_resolved_sources(&sources, None, 2),
            Err(NativeSourcePreflightError::Source(
                NativeSourceError::DuplicateSource(input)
            )) if input == duplicate
        ));

        let local = input(4);
        let sources = [NativeResolvedSource::LocalVideo {
            input: local,
            path: PathBuf::from("video.mov"),
        }];
        assert!(matches!(
            validate_resolved_sources(&sources, None, 1),
            Err(NativeSourcePreflightError::CodecAdapterRequired { input }) if input == local
        ));

        let retained = [NativeResolvedSource::RetainedFrame {
            input: local,
            frame: retained_frame(clock_domain, 0, 0),
        }];
        assert!(validate_resolved_sources(&retained, None, 1).is_ok());
        let live = [NativeResolvedSource::LiveFrame {
            input: local,
            frame: retained_frame(clock_domain, 9, 10),
        }];
        assert!(validate_resolved_sources(&live, None, 1).is_ok());
    }

    #[test]
    fn retained_frame_timing_rebases_pts_and_requires_domain_and_sequence_zero() {
        let source = input(1);
        let clock_domain = ClockDomainId::new(NonZeroU128::new(7).unwrap());
        let other_domain = ClockDomainId::new(NonZeroU128::new(8).unwrap());
        let frame = retained_frame(clock_domain, 0, -42);
        assert_eq!(
            validate_retained_frame_timing(source, &frame, clock_domain),
            Ok((vec![0], -42, 0))
        );
        assert_eq!(
            validate_retained_frame_timing(
                source,
                &retained_frame(clock_domain, 1, 42),
                clock_domain
            ),
            Err(NativeSourceError::InvalidTimeline { input: source })
        );
        assert_eq!(
            validate_retained_frame_timing(
                source,
                &retained_frame(other_domain, 0, 42),
                clock_domain
            ),
            Err(NativeSourceError::InvalidTimeline { input: source })
        );
    }

    #[test]
    fn live_timing_preserves_source_clock_and_accepts_sequence_gaps() {
        let source = input(1);
        let clock_domain = ClockDomainId::new(NonZeroU128::new(7).unwrap());
        let other_domain = ClockDomainId::new(NonZeroU128::new(8).unwrap());
        let seed = retained_frame(clock_domain, 40, -42);
        assert_eq!(
            validate_live_seed_timing(source, &seed),
            Ok((vec![0], -42, 40, clock_domain))
        );
        assert_eq!(
            validate_live_update_timing(
                source,
                &retained_frame(clock_domain, 44, 10),
                -42,
                40,
                clock_domain,
            ),
            Ok(())
        );
        for frame in [
            retained_frame(clock_domain, 40, 10),
            retained_frame(clock_domain, 44, -42),
            retained_frame(other_domain, 44, 10),
        ] {
            assert_eq!(
                validate_live_update_timing(source, &frame, -42, 40, clock_domain),
                Err(NativeSourceError::InvalidTimeline { input: source })
            );
        }
    }

    #[test]
    fn source_validation_preserves_full_width_ids_and_charges_exact_bytes() {
        let low = input(1);
        let high = input((1_u128 << 64) + 1);
        let sources = [(low, 2, 3, 1), (high, 2, 3, 2)];
        let limits = NativeSourceLimits {
            max_media_inputs: 2,
            max_video_frames_per_source: NonZeroU32::new(2).unwrap(),
            max_retained_rgba16f_bytes: 144,
        };

        assert_eq!(
            validate_source_layouts(&sources, limits),
            Ok((Some((2, 3)), 144))
        );
        let ids = sources
            .into_iter()
            .map(|(id, _, _, _)| (id, true))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(ids.len(), 2);
        assert!(registered_source(&ids, low).is_ok());
        assert!(registered_source(&ids, high).is_ok());
    }

    #[test]
    fn source_validation_rejects_count_duplicates_budget_and_dimensions() {
        let first = input(1);
        let second = input(2);
        let limits = NativeSourceLimits {
            max_media_inputs: 2,
            max_video_frames_per_source: NonZeroU32::new(1).unwrap(),
            max_retained_rgba16f_bytes: 64,
        };

        assert_eq!(
            validate_source_layouts(
                &[(first, 2, 2, 1), (second, 2, 2, 1), (input(3), 2, 2, 1)],
                limits
            ),
            Err(NativeSourceError::TooManySources {
                actual: 3,
                maximum: 2
            })
        );
        assert_eq!(
            validate_source_layouts(&[(first, 2, 2, 1), (first, 2, 2, 1)], limits),
            Err(NativeSourceError::DuplicateSource(first))
        );
        assert_eq!(
            validate_source_layouts(&[(first, 2, 2, 0)], limits),
            Err(NativeSourceError::InvalidTimeline { input: first })
        );
        assert_eq!(
            validate_source_layouts(&[(first, 2, 2, 2)], limits),
            Err(NativeSourceError::TooManyFrames {
                input: first,
                actual: 2,
                maximum: 1
            })
        );
        assert_eq!(
            validate_source_layouts(&[(first, 3, 2, 1), (second, 3, 2, 1)], limits),
            Err(NativeSourceError::RetainedBytesExceeded {
                required: 96,
                maximum: 64
            })
        );
        assert_eq!(
            validate_source_layouts(&[(first, 2, 2, 1), (second, 3, 2, 1)], limits),
            Err(NativeSourceError::DimensionMismatch {
                input: second,
                expected_width: 2,
                expected_height: 2,
                actual_width: 3,
                actual_height: 2
            })
        );
    }

    #[test]
    fn source_validation_reports_checked_frame_charge_overflow() {
        let source = input(1);
        assert_eq!(
            validate_source_layouts(
                &[(source, u32::MAX, u32::MAX, 1)],
                NativeSourceLimits {
                    max_media_inputs: 1,
                    max_video_frames_per_source: NonZeroU32::MIN,
                    max_retained_rgba16f_bytes: u64::MAX,
                }
            ),
            Err(NativeSourceError::FrameByteSizeOverflow {
                input: source,
                width: u32::MAX,
                height: u32::MAX
            })
        );
    }

    #[test]
    fn missing_source_lookup_is_typed_and_uses_full_width_id() {
        let missing = input((1_u128 << 64) + 1);
        let sources = BTreeMap::<InputId, bool>::new();
        assert!(matches!(
            registered_source(&sources, missing),
            Err(NativeSourceRenderError::MissingSource { input }) if input == missing
        ));
    }

    #[test]
    fn program_frame_maps_exactly_to_cut_fade_or_wipe() {
        let primary = input(1);
        let secondary = input((1_u128 << 64) + 1);
        let cut = native_mix_plan(ProgramFrame {
            primary,
            secondary: None,
            transition_kind: None,
            mix_numerator: u32::MAX,
            mix_denominator: 0,
            mix_start_numerator: u32::MAX,
            mix_end_numerator: u32::MAX,
        })
        .unwrap();
        assert_eq!(cut.primary, primary);
        assert_eq!(cut.secondary, primary);
        assert_eq!(cut.transition.kind(), TransitionKind::Cut);
        assert_eq!(cut.transition.numerator(), 0);
        assert_eq!(cut.transition.denominator(), 1);

        let identical = native_mix_plan(ProgramFrame {
            primary,
            secondary: Some(primary),
            transition_kind: Some(SwitcherTransitionKind::Fade),
            mix_numerator: u32::MAX,
            mix_denominator: 0,
            mix_start_numerator: u32::MAX,
            mix_end_numerator: u32::MAX,
        })
        .unwrap();
        assert_eq!(identical.primary, primary);
        assert_eq!(identical.secondary, primary);
        assert_eq!(identical.transition.kind(), TransitionKind::Cut);

        let fade = native_mix_plan(ProgramFrame {
            primary,
            secondary: Some(secondary),
            transition_kind: Some(SwitcherTransitionKind::Fade),
            mix_numerator: 7,
            mix_denominator: 11,
            mix_start_numerator: 7,
            mix_end_numerator: 8,
        })
        .unwrap();
        assert_eq!(fade.primary, primary);
        assert_eq!(fade.secondary, secondary);
        assert_eq!(fade.transition.kind(), TransitionKind::Fade);
        assert_eq!(fade.transition.numerator(), 7);
        assert_eq!(fade.transition.denominator(), 11);
        assert!(matches!(
            native_mix_plan(ProgramFrame {
                primary,
                secondary: Some(secondary),
                transition_kind: Some(SwitcherTransitionKind::Fade),
                mix_numerator: 1,
                mix_denominator: 0,
                mix_start_numerator: 1,
                mix_end_numerator: 1,
            }),
            Err(NativeSourceRenderError::InvalidMix(
                TransitionError::ZeroDenominator
            ))
        ));

        let wipe = native_mix_plan(ProgramFrame {
            primary,
            secondary: Some(secondary),
            transition_kind: Some(SwitcherTransitionKind::Wipe),
            mix_numerator: 1,
            mix_denominator: 2,
            mix_start_numerator: 1,
            mix_end_numerator: 2,
        })
        .unwrap();
        assert_eq!(wipe.primary, primary);
        assert_eq!(wipe.secondary, secondary);
        assert_eq!(wipe.transition.kind(), TransitionKind::Wipe);
        assert_eq!(wipe.transition.numerator(), 1);
        assert_eq!(wipe.transition.denominator(), 2);
    }

    #[test]
    fn program_frame_preserves_slide_kind_and_progress() {
        let primary = input(1);
        let secondary = input((1_u128 << 64) + 1);
        let slide = native_mix_plan(ProgramFrame {
            primary,
            secondary: Some(secondary),
            transition_kind: Some(SwitcherTransitionKind::Slide),
            mix_numerator: 2,
            mix_denominator: 3,
            mix_start_numerator: 1,
            mix_end_numerator: 2,
        })
        .unwrap();
        assert_eq!(slide.primary, primary);
        assert_eq!(slide.secondary, secondary);
        assert_eq!(slide.transition.kind(), TransitionKind::Slide);
        assert_eq!(slide.transition.numerator(), 2);
        assert_eq!(slide.transition.denominator(), 3);
    }

    #[test]
    fn program_frame_preserves_zoom_and_rejects_stinger() {
        let primary = input(1);
        let secondary = input((1_u128 << 64) + 1);
        let zoom = native_mix_plan(ProgramFrame {
            primary,
            secondary: Some(secondary),
            transition_kind: Some(SwitcherTransitionKind::Zoom),
            mix_numerator: 1,
            mix_denominator: 2,
            mix_start_numerator: 1,
            mix_end_numerator: 2,
        })
        .unwrap();
        assert_eq!(zoom.primary, primary);
        assert_eq!(zoom.secondary, secondary);
        assert_eq!(zoom.transition.kind(), TransitionKind::Zoom);
        assert_eq!(zoom.transition.numerator(), 1);
        assert_eq!(zoom.transition.denominator(), 2);

        let stinger = SwitcherTransitionKind::Stinger(fm_switcher::StingerSlotId::new(1).unwrap());
        assert!(matches!(
            native_mix_plan(ProgramFrame {
                primary,
                secondary: Some(secondary),
                transition_kind: Some(stinger),
                mix_numerator: 1,
                mix_denominator: 2,
                mix_start_numerator: 1,
                mix_end_numerator: 2,
            }),
            Err(NativeSourceRenderError::UnsupportedTransition(actual))
                if actual == stinger
        ));
    }

    #[test]
    fn configured_project_stinger_selects_program_then_preview_at_exact_cut() {
        let program = input(1);
        let preview = input(2);
        let media = input(3);
        let slot = fm_switcher::StingerSlotId::new(1).unwrap();
        let mut project = native_plan_project(4, 2);
        for source in [program, preview, media] {
            add_leaf(&mut project, source);
        }
        project.add_stinger(StingerConfig::new(
            StingerSlotNumber::new(1).unwrap(),
            media,
            true,
            2,
            fm_model::StingerAudioPolicy::Muted,
            fm_model::StingerMissingMediaFallback::Cut,
        ));
        let project = NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();
        let frame = |frame_index| ProgramFrame {
            primary: program,
            secondary: Some(preview),
            transition_kind: Some(SwitcherTransitionKind::Stinger(slot)),
            mix_numerator: frame_index,
            mix_denominator: 4,
            mix_start_numerator: frame_index,
            mix_end_numerator: frame_index + 1,
        };

        let NativeProjectMixPlan::Stinger(before) =
            native_project_mix_plan(&project, frame(1)).unwrap()
        else {
            panic!("expected a configured Stinger plan");
        };
        assert_eq!(before.program, program);
        assert_eq!(before.preview, preview);
        assert_eq!(before.media, media);
        assert_eq!(before.frame.base(), fm_compositor::StingerBase::Program);

        let NativeProjectMixPlan::Stinger(at_cut) =
            native_project_mix_plan(&project, frame(2)).unwrap()
        else {
            panic!("expected a configured Stinger plan");
        };
        assert_eq!(at_cut.frame.base(), fm_compositor::StingerBase::Preview);
    }

    #[test]
    fn project_stinger_requires_configuration_and_valid_frame_position() {
        let program = input(1);
        let preview = input(2);
        let slot = fm_switcher::StingerSlotId::new(8).unwrap();
        let mut project = native_plan_project(4, 2);
        add_leaf(&mut project, program);
        add_leaf(&mut project, preview);
        let project = NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();
        let frame = ProgramFrame {
            primary: program,
            secondary: Some(preview),
            transition_kind: Some(SwitcherTransitionKind::Stinger(slot)),
            mix_numerator: 0,
            mix_denominator: 4,
            mix_start_numerator: 0,
            mix_end_numerator: 1,
        };

        assert!(matches!(
            native_project_mix_plan(&project, frame),
            Err(NativeSourceRenderError::MissingStingerConfiguration(actual))
                if actual == slot
        ));

        let mut invalid = native_plan_project(4, 2);
        add_leaf(&mut invalid, program);
        add_leaf(&mut invalid, preview);
        invalid.add_stinger(StingerConfig::new(
            StingerSlotNumber::new(8).unwrap(),
            preview,
            true,
            5,
            fm_model::StingerAudioPolicy::Muted,
            fm_model::StingerMissingMediaFallback::Cut,
        ));
        let invalid = NativeProjectPlan::compile(&invalid, NativeProjectLimits::default()).unwrap();
        assert!(matches!(
            native_project_mix_plan(&invalid, frame),
            Err(NativeSourceRenderError::InvalidStinger(
                StingerPlanError::CutPointOutOfRange {
                    cut_point_frame: 5,
                    frame_count: 4,
                }
            ))
        ));
    }

    #[test]
    fn program_frame_preserves_alpha_fade_kind_and_progress() {
        let primary = input(1);
        let secondary = input((1_u128 << 64) + 1);
        let plan = native_mix_plan(ProgramFrame {
            primary,
            secondary: Some(secondary),
            transition_kind: Some(SwitcherTransitionKind::AlphaFade),
            mix_numerator: 3,
            mix_denominator: 5,
            mix_start_numerator: 3,
            mix_end_numerator: 4,
        })
        .unwrap();
        assert_eq!(plan.primary, primary);
        assert_eq!(plan.secondary, secondary);
        assert_eq!(plan.transition.kind(), TransitionKind::AlphaFade);
        assert_eq!(plan.transition.numerator(), 3);
        assert_eq!(plan.transition.denominator(), 5);
    }

    #[test]
    fn prefix_selection_rebases_vfr_pts_and_holds_boundaries_and_end() {
        let source = input(1);
        let offsets = rebased_offsets(source, &[-20_000_000, 20_000_000, 100_000_000]).unwrap();
        assert_eq!(offsets, [0, 40_000_000, 120_000_000]);
        for (deadline, expected) in [
            (0, 0),
            (39_999_999, 0),
            (40_000_000, 1),
            (119_999_999, 1),
            (120_000_000, 2),
            (u64::MAX, 2),
        ] {
            assert_eq!(frame_index_at_deadline(&offsets, deadline), Some(expected));
        }
        assert_eq!(frame_index_at_deadline(&[], 0), None);
        assert_eq!(
            rebased_offsets(source, &[0, 0]),
            Err(NativeSourceError::InvalidTimeline { input: source })
        );
        assert_eq!(
            rebased_offsets(source, &[i64::MIN, i64::MAX]),
            Ok(vec![0, u64::MAX])
        );
    }

    fn source_playback_for_pin_test(
        kind: NativeVideoSourceKind,
        end_of_stream: bool,
    ) -> NativeSourcePlayback {
        let input = input(1);
        NativeSourcePlayback {
            registry: NativeSourceRegistry {
                sources: BTreeMap::from([(
                    input,
                    NativeVideoPrefix {
                        frames: Vec::new(),
                        offsets_ns: Vec::new(),
                        source_pts_origin: 0,
                        last_source_pts: 0,
                        last_sequence: 0,
                        clock_domain: ClockDomainId::new(NonZeroU128::new(1).unwrap()),
                        kind,
                        end_of_stream,
                        in_flight: None,
                        available_for_stinger: false,
                        pinned_for_stinger: false,
                    },
                )]),
                dimensions: None,
                retained_rgba16f_bytes: 0,
                limits: NativeSourceLimits::default(),
            },
            worker: NativeDecodeWorker::spawn(BTreeMap::new()).unwrap(),
            failed: false,
        }
    }

    #[test]
    fn stinger_pin_requires_bounded_eos_and_rejects_live_sources() {
        let source = input(1);
        let mut complete = source_playback_for_pin_test(NativeVideoSourceKind::Decoded, true);
        complete.pin_stinger_source(source).unwrap();
        assert!(complete.registry.sources[&source].pinned_for_stinger);

        let mut incomplete = source_playback_for_pin_test(NativeVideoSourceKind::Decoded, false);
        assert!(matches!(
            incomplete.pin_stinger_source(source),
            Err(NativeSourcePlaybackError::StingerSourceNotFullyPreloaded {
                input,
                retained_frames: 0,
                maximum_frames: 8,
            }) if input == source
        ));
        assert!(!incomplete.registry.sources[&source].pinned_for_stinger);

        let mut live = source_playback_for_pin_test(NativeVideoSourceKind::Live, false);
        assert!(matches!(
            live.pin_stinger_source(source),
            Err(NativeSourcePlaybackError::StingerSourceIsLive { input }) if input == source
        ));
    }

    #[test]
    fn clip_local_stinger_cadence_restarts_and_pinned_frames_never_evict() {
        let rate = FrameRate::new(25, 1).unwrap();
        let offsets = [0, 40_000_000, 80_000_000];
        for (frame, expected_deadline, expected_source_frame) in [
            (0, 39_999_999, 0),
            (1, 79_999_999, 1),
            (2, 119_999_999, 2),
            (3, 159_999_999, 2),
        ] {
            let deadline = stinger_frame_deadline(rate, frame).unwrap();
            assert_eq!(deadline.as_nanos(), expected_deadline);
            assert_eq!(
                frame_index_at_deadline(&offsets, deadline.as_nanos()),
                Some(expected_source_frame)
            );
        }
        assert_eq!(
            source_eviction_count(&offsets, true, ClockTime::from_nanos(u64::MAX)),
            0
        );
        assert_eq!(
            source_eviction_count(&offsets, false, ClockTime::from_nanos(u64::MAX)),
            2
        );
    }

    #[test]
    fn bounded_preflight_request_is_video_only() {
        let request = prefix_decode_request(
            ClockDomainId::new(NonZeroU128::new(1).unwrap()),
            StreamSelector::Best,
            NonZeroU32::new(8).unwrap(),
        );
        assert_eq!(request.video.unwrap().count.get(), 8);
        assert!(request.audio.is_none());
    }

    #[test]
    fn eviction_retains_floor_anchor_and_every_future_frame() {
        let offsets = [0, 40, 125, 210];
        assert_eq!(floor_anchor_eviction_count(&offsets, ClockTime::ZERO), 0);
        assert_eq!(
            floor_anchor_eviction_count(&offsets, ClockTime::from_nanos(124)),
            1
        );
        assert_eq!(
            floor_anchor_eviction_count(&offsets, ClockTime::from_nanos(125)),
            2
        );
        assert_eq!(
            floor_anchor_eviction_count(&offsets, ClockTime::from_nanos(u64::MAX)),
            3
        );
    }

    #[test]
    fn coverage_requires_latest_pts_until_eos() {
        assert!(!source_covers_deadline(None, false, ClockTime::ZERO));
        assert!(source_covers_deadline(
            Some(100),
            false,
            ClockTime::from_nanos(100)
        ));
        assert!(!source_covers_deadline(
            Some(100),
            false,
            ClockTime::from_nanos(101)
        ));
        assert!(source_covers_deadline(
            Some(100),
            true,
            ClockTime::from_nanos(u64::MAX)
        ));
    }

    #[test]
    fn refill_state_obeys_watermark_ring_budget_and_single_flight() {
        assert_eq!(
            refill_page_size(4, false, false, 8, u64::MAX),
            NonZeroU32::new(4)
        );
        assert_eq!(
            refill_page_size(3, false, false, 5, u64::MAX),
            NonZeroU32::new(2)
        );
        assert_eq!(refill_page_size(1, false, false, 8, 1), NonZeroU32::new(1));
        assert_eq!(refill_page_size(5, false, false, 8, 8), None);
        assert_eq!(refill_page_size(1, true, false, 8, 8), None);
        assert_eq!(refill_page_size(1, false, true, 8, 8), None);
        assert_eq!(refill_page_size(1, false, false, 8, 0), None);
    }

    #[test]
    fn retained_eos_uses_an_idle_worker_and_never_schedules_refill() {
        assert_eq!(refill_page_size(1, false, true, 8, u64::MAX), None);
        drop(NativeDecodeWorker::spawn(BTreeMap::new()).unwrap());
    }

    #[test]
    fn vfr_page_seams_preserve_origin_pts_and_global_sequence() {
        let source = input(1);
        let first =
            validate_timing_values(source, &[-20, 20, 100], &[0, 1, 2], -20, None, 0).unwrap();
        assert_eq!(first, (vec![0, 40, 120], 100, 2));
        let second =
            validate_timing_values(source, &[175, 260], &[3, 4], -20, Some(100), 3).unwrap();
        assert_eq!(second, (vec![195, 280], 260, 4));

        assert_eq!(
            validate_timing_values(source, &[100], &[3], -20, Some(100), 3),
            Err(NativeSourceError::InvalidTimeline { input: source })
        );
        assert_eq!(
            validate_timing_values(source, &[175], &[4], -20, Some(100), 3),
            Err(NativeSourceError::InvalidTimeline { input: source })
        );
    }

    #[test]
    fn absolute_audio_spans_are_exact_at_integer_and_fractional_rates() {
        let rate = FrameRate::new(25, 1).unwrap();
        assert_eq!(
            absolute_frame_sample_span(0, 48_000, rate).unwrap(),
            (0, 1_920)
        );
        assert_eq!(
            absolute_frame_sample_span(123, 48_000, rate).unwrap(),
            (236_160, 238_080)
        );

        let ntsc = FrameRate::new(60_000, 1_001).unwrap();
        assert_eq!(
            absolute_frame_sample_span(0, 48_000, ntsc).unwrap(),
            (0, 800)
        );
        assert_eq!(
            absolute_frame_sample_span(1, 48_000, ntsc).unwrap(),
            (800, 1_601)
        );
        assert_eq!(
            absolute_frame_sample_span(59_999, 48_000, ntsc).unwrap(),
            (48_047_199, 48_048_000)
        );
    }

    #[test]
    fn output_audio_timing_uses_absolute_samples_and_contiguous_normalized_endpoints() {
        let clock_domain = ClockDomainId::new(NonZeroU128::new(3).unwrap());
        let rate = FrameRate::new(60_000, 1_001).unwrap();
        let first_span = absolute_frame_sample_span(41, 48_000, rate).unwrap();
        let second_span = absolute_frame_sample_span(42, 48_000, rate).unwrap();
        let first =
            output_audio_timing(41, first_span.0, first_span.1, 48_000, clock_domain).unwrap();
        let second =
            output_audio_timing(42, second_span.0, second_span.1, 48_000, clock_domain).unwrap();

        assert_eq!(first.original_timestamp().timestamp().ticks(), 32_832);
        assert_eq!(
            first.original_timestamp().time_base(),
            TimeBase::new(1, 48_000).unwrap()
        );
        assert_eq!(first.sequence().get(), 41);
        assert_eq!(second.sequence().get(), 42);
        assert_eq!(
            first.presentation_timestamp().as_nanos()
                + i64::try_from(first.duration().as_nanos()).unwrap(),
            second.presentation_timestamp().as_nanos()
        );
    }

    #[test]
    fn audio_pages_validate_global_sequence_and_sample_continuity_transactionally() {
        let source_id = input(1);
        let format = mono_audio_format();
        let clock_domain = ClockDomainId::new(NonZeroU128::new(9).unwrap());
        let mut source = audio_source(
            vec![audio_chunk(100, &[1.0, 2.0]), audio_chunk(102, &[3.0])],
            false,
        );
        assert_eq!(source.next_sample, 103);
        assert_eq!(source.next_sequence, 2);

        let invalid = [audio_block(3, 103, &[4.0], &format, clock_domain)];
        assert!(matches!(
            validate_audio_page(source_id, &source, &invalid, clock_domain),
            Err(NativeMasterError::InvalidTimeline { input }) if input == source_id
        ));
        assert_eq!(source.next_sample, 103);
        assert_eq!(source.next_sequence, 2);
        let valid = [audio_block(2, 103, &[4.0], &format, clock_domain)];
        let page = validate_audio_page(source_id, &source, &valid, clock_domain).unwrap();
        commit_audio_page(&mut source, &valid, page).unwrap();
        assert_eq!(source.next_sample, 104);
        assert_eq!(
            source.synchronizer.unwrap().telemetry().accepted_blocks(),
            3
        );
    }

    #[test]
    fn synchronizer_render_crosses_audio_page_seams() {
        let mut source = audio_source(
            vec![
                audio_chunk(0, &[1.0, 2.0, 3.0]),
                audio_chunk(3, &[4.0, 5.0, 6.0, 7.0]),
            ],
            false,
        );
        let timing = output_audio_timing(
            0,
            0,
            6,
            48_000,
            ClockDomainId::new(NonZeroU128::new(9).unwrap()),
        )
        .unwrap();
        let synchronizer = source.synchronizer.as_mut().unwrap();
        let plan = synchronizer
            .plan_render(master_audio_interval(timing), 6)
            .unwrap();
        let mut output = [-1.0; 6];
        synchronizer
            .render_planned_into(plan, &mut [&mut output])
            .unwrap();
        for (actual, expected) in output.into_iter().zip([1.0, 2.0, 3.0, 4.0, 5.0, 6.0]) {
            assert_sample_exact(actual, expected);
        }
    }

    #[test]
    fn arbitrary_source_coordinate_maps_file_first_pts_to_master_zero() {
        let source_rate = fm_types::SampleRate::new(44_100).unwrap();
        let output_rate = fm_types::SampleRate::new(48_000).unwrap();
        let layout = fm_types::ChannelLayout::new(vec![fm_types::Channel::Mono]).unwrap();
        let source_format = AudioFormat {
            sample_rate: source_rate,
            sample_format: SampleFormat::F32,
            channels: layout.clone(),
        };
        let clock_domain = ClockDomainId::new(NonZeroU128::new(9).unwrap());
        let mapping_domain = MappingClockDomainId::new(clock_domain.get());
        let source_sample = 44_101;
        let source_timestamp =
            normalized_sample_endpoint(i128::from(source_sample), 44_100).unwrap();
        let mapping = ClockMapping::new(
            ClockSnapshot::new(
                mapping_domain,
                ClockTime::from_nanos(u64::try_from(source_timestamp).unwrap()),
            ),
            ClockSnapshot::new(mapping_domain, ClockTime::ZERO),
            0,
        )
        .unwrap();
        let mut synchronizer = ClockMappedAudioSynchronizer::new(
            source_rate,
            output_rate,
            layout,
            mapping,
            AudioCadenceOrigin::new(
                NormalizedTimestamp::from_nanos(source_timestamp),
                source_sample,
            ),
            AudioCadenceOrigin::new(NormalizedTimestamp::from_nanos(0), 0),
            AudioSynchronizerLimits::default(),
        )
        .unwrap();
        synchronizer
            .push(&audio_block(
                0,
                source_sample,
                &[0.25, 0.5, 0.75],
                &source_format,
                clock_domain,
            ))
            .unwrap();
        let timing = output_audio_timing(0, 0, 2, 48_000, clock_domain).unwrap();
        let plan = synchronizer
            .plan_render(master_audio_interval(timing), 2)
            .unwrap();
        let mut output = [0.0; 2];
        synchronizer
            .render_planned_into(plan, &mut [&mut output])
            .unwrap();
        assert_sample_exact(output[0], 0.25);
    }

    #[test]
    fn eos_straddle_synthesizes_contiguous_cadence_silence() {
        let source_id = input(1);
        let mut master = silent_test_master(source_id, 1);
        master.frame_rate = FrameRate::new(12_000, 1).unwrap();
        master
            .scratch
            .rendered
            .insert(source_id, vec![vec![0.0; fm_audio::MAX_SAMPLES_PER_BLOCK]]);
        master.sources.insert(
            source_id,
            audio_source(vec![audio_chunk(0, &[0.1, 0.2, 0.3])], true),
        );
        assert!(master.service_next_frame().unwrap());
        let block = master
            .render_frame_audio(&frame_result(0, source_id, None))
            .unwrap();
        assert_eq!(block.plane(0).unwrap(), &[0.1, 0.2, 0.3, 0.0]);
    }

    #[test]
    fn synchronizer_occupancy_aggregates_into_native_telemetry() {
        let source_id = input(1);
        let mut master = silent_test_master(source_id, 1);
        master.frame_rate = FrameRate::new(24_000, 1).unwrap();
        master
            .scratch
            .rendered
            .insert(source_id, vec![vec![0.0; fm_audio::MAX_SAMPLES_PER_BLOCK]]);
        master.sources.insert(
            source_id,
            audio_source(vec![audio_chunk(0, &[1.0, 2.0, 3.0, 4.0])], true),
        );
        assert_eq!(
            (
                master.retained_blocks(),
                master.retained_samples(),
                master.retained_bytes()
            ),
            (1, 4, 16)
        );
        assert!(master.service_next_frame().unwrap());
        master
            .render_frame(&frame_result(0, source_id, None))
            .unwrap();
        assert_eq!(master.retained_samples(), 2);
        assert_eq!(master.retained_bytes(), 8);
    }

    #[test]
    fn inactive_decoded_sources_advance_on_every_master_interval() {
        let active = input(1);
        let inactive = input(2);
        let mut master = audio_test_master(&[(active, 0.25), (inactive, -0.25)], 1);

        assert!(master.service_next_frame().unwrap());
        master
            .render_frame_audio(&frame_result(0, active, None))
            .unwrap();

        for source in [active, inactive] {
            let telemetry = master.sources[&source]
                .synchronizer
                .as_ref()
                .unwrap()
                .telemetry();
            assert_eq!(telemetry.rendered_intervals(), 1);
            assert_eq!(telemetry.rendered_samples(), 1_920);
            assert_eq!(telemetry.buffered_samples(), 3_840);
        }
    }

    #[test]
    fn multi_source_render_preflight_rolls_back_every_cursor() {
        let first = input(1);
        let second = input(2);
        let mut master = audio_test_master(&[(first, 0.25), (second, -0.25)], 1);
        master.sources.insert(
            second,
            audio_source(vec![audio_chunk(0, &[0.5; 32])], false),
        );
        master.ready_frame = Some((0, 0, 1_920));
        let retained = master.retained_samples();

        assert!(matches!(
            master.render_frame_audio(&frame_result(0, first, Some(second))),
            Err(NativeMasterError::Synchronizer(
                fm_audio::AudioSynchronizerError::NeedMoreInput { .. }
            ))
        ));
        assert_eq!(master.expected_next_frame(), 0);
        assert_eq!(master.retained_samples(), retained);
        for source in [first, second] {
            assert_eq!(
                master.sources[&source]
                    .synchronizer
                    .as_ref()
                    .unwrap()
                    .telemetry()
                    .rendered_intervals(),
                0
            );
        }
    }

    #[test]
    fn restored_decoded_master_starts_at_absolute_frame_cadence() {
        let source_id = input(1);
        let mut master = audio_test_master(&[(source_id, 0.25)], 1);
        let restored_frame = 2;
        let start_sample = absolute_frame_sample_span(restored_frame, 48_000, master.frame_rate)
            .unwrap()
            .0;
        master.expected_next_frame = restored_frame;
        master.sources.insert(
            source_id,
            audio_source_at_master(
                &[audio_chunk(0, &vec![0.25; 3 * 1_920])],
                true,
                start_sample,
            ),
        );

        assert!(master.service_next_frame().unwrap());
        let block = master
            .render_frame_audio(&frame_result(restored_frame, source_id, None))
            .unwrap();
        assert_eq!(
            block.timing().original_timestamp().timestamp().ticks(),
            3_840
        );
        assert_eq!(block.sample_count(), 1_920);
        for sample in block.plane(0).unwrap() {
            assert_sample_exact(*sample, 0.25);
        }
    }

    #[test]
    #[ignore = "requires FFmpeg and ffprobe"]
    fn deep_restored_44_1k_nonzero_pts_positions_without_pcm_prefix_and_preserves_phase() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("deep-restored.nut");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:size=64x48:rate=25:d=15",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=44100:duration=15",
                "-filter_complex",
                "[0:v]setpts=PTS+2/TB[v];[1:a]asetpts=PTS+2/TB[a]",
                "-map",
                "[v]",
                "-map",
                "[a]",
                "-c:v",
                "ffv1",
                "-c:a",
                "pcm_s16le",
                "-f",
                "nut",
            ])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let adapter = Adapter::new(fm_codec_ffmpeg::Config {
            allowed_root: Some(directory.path().to_owned()),
            ..fm_codec_ffmpeg::Config::default()
        })
        .unwrap();
        let source_id = input(1);
        let restored_frame = 251;
        let clock_domain = ClockDomainId::new(NonZeroU128::new(9).unwrap());
        let mut master = NativeMasterRuntime::preflight_local_blocking(
            Some(&adapter),
            &[NativeResolvedSource::LocalVideo {
                input: source_id,
                path,
            }],
            mono_audio_format(),
            FrameRate::new(25, 1).unwrap(),
            clock_domain,
            restored_frame,
            NativeAudioLimits::default(),
        )
        .unwrap();
        let telemetry = master.audio_telemetry();
        assert!(telemetry.positioned_samples > 400_000);
        assert!(master.retained_samples() <= NativeAudioLimits::default().max_samples_per_page);
        let synchronizer = master.sources[&source_id].synchronizer.as_ref().unwrap();
        assert!(synchronizer.source_origin().timestamp().as_nanos() > 10_000_000_000);

        assert!(master.service_next_frame().unwrap());
        let block = master
            .render_frame_audio(&frame_result(restored_frame, source_id, None))
            .unwrap();
        let first = block.plane(0).unwrap()[0];
        let second = block.plane(0).unwrap()[1];
        let expected_first = 0.125 * (std::f64::consts::TAU * 440.0 * 10.04).sin();
        let expected_second =
            0.125 * (std::f64::consts::TAU * 440.0 * (10.04 + 1.0 / 48_000.0)).sin();
        assert!((f64::from(first) - expected_first).abs() < 0.003);
        assert!((f64::from(second) - expected_second).abs() < 0.003);
    }

    #[test]
    #[ignore = "requires FFmpeg and ffprobe"]
    #[allow(clippy::too_many_lines)]
    fn media_timeline_offsets_trim_early_audio_and_pad_delayed_audio() {
        let directory = tempfile::tempdir().unwrap();
        for (name, video_pts, audio_pts, delayed, negative_origin) in [
            ("early.mov", "PTS+2/TB", "PTS+1.95/TB", false, false),
            ("delayed.mov", "PTS+2/TB", "PTS+2.05/TB", true, false),
            ("negative.mkv", "PTS", "PTS-0.05/TB", false, true),
        ] {
            let path = directory.path().join(name);
            let filter = format!(
                "[0:v]setpts={video_pts}[v];[1:a]asetpts={audio_pts},asetnsamples=n=441:p=1[a]"
            );
            let status = Command::new("ffmpeg")
                .args([
                    "-nostdin",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-copyts",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=black:size=64x48:rate=25:d=3",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:sample_rate=44100:duration=3",
                    "-filter_complex",
                    &filter,
                    "-map",
                    "[v]",
                    "-map",
                    "[a]",
                    "-c:v",
                    "mpeg4",
                    "-c:a",
                    "pcm_s16le",
                    "-avoid_negative_ts",
                    "disabled",
                ])
                .arg(&path)
                .status()
                .unwrap();
            assert!(status.success());
            let adapter = Adapter::new(fm_codec_ffmpeg::Config {
                allowed_root: Some(directory.path().to_owned()),
                ..fm_codec_ffmpeg::Config::default()
            })
            .unwrap();
            let source_id = input(1);
            let clock_domain = ClockDomainId::new(NonZeroU128::new(9).unwrap());
            let mut master = NativeMasterRuntime::preflight_local_blocking(
                Some(&adapter),
                &[NativeResolvedSource::LocalVideo {
                    input: source_id,
                    path,
                }],
                mono_audio_format(),
                FrameRate::new(25, 1).unwrap(),
                clock_domain,
                0,
                NativeAudioLimits::default(),
            )
            .unwrap();
            if negative_origin {
                assert!(
                    master.sources[&source_id]
                        .timeline_origin
                        .timestamp()
                        .as_nanos()
                        < 0
                );
            }
            assert!(master.service_next_frame().unwrap());
            let first = master
                .render_frame_audio(&frame_result(0, source_id, None))
                .unwrap();
            if delayed {
                let onset = master.sources[&source_id].audio_start_master_sample;
                let first_silence = usize::try_from(onset.min(1_920)).unwrap();
                assert!(
                    first.plane(0).unwrap()[..first_silence]
                        .iter()
                        .all(|sample| sample.abs() < 1.0e-7)
                );
                let mut ready = false;
                for _ in 0..1_000 {
                    if master.service_next_frame().unwrap() {
                        ready = true;
                        break;
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                assert!(ready);
                let second = master
                    .render_frame_audio(&frame_result(1, source_id, None))
                    .unwrap();
                if onset > 1_920 {
                    let second_silence = usize::try_from(onset - 1_920).unwrap();
                    assert!(
                        second.plane(0).unwrap()[..second_silence]
                            .iter()
                            .all(|sample| sample.abs() < 1.0e-7)
                    );
                }
                assert!(
                    second
                        .plane(0)
                        .unwrap()
                        .iter()
                        .any(|sample| sample.abs() > 0.01)
                );
                assert!(master.audio_telemetry().leading_silence_samples >= onset);
            } else {
                assert!(
                    first.plane(0).unwrap()[32..]
                        .iter()
                        .any(|sample| sample.abs() > 0.01)
                );
                assert_eq!(master.audio_telemetry().leading_silence_samples, 0);
            }

            let positioned_before_restart = master.audio_telemetry().positioned_samples;
            master.restart_clip_at(0).unwrap();
            let mut replay_ready = false;
            for _ in 0..1_000 {
                if master.service_next_frame().unwrap() {
                    replay_ready = true;
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            assert!(replay_ready);
            let replay = master
                .render_frame_audio(&frame_result(0, source_id, None))
                .unwrap();
            assert_eq!(replay.plane(0), first.plane(0));
            if !delayed {
                assert!(master.audio_telemetry().positioned_samples > positioned_before_restart);
            }
        }
    }

    #[test]
    fn uncovered_tiny_blocks_fail_at_the_per_source_ring_bound() {
        let source_id = input(1);
        let mut master = silent_test_master(source_id, 1);
        let chunks = (0..8)
            .map(|sample| audio_chunk(sample, &[1.0]))
            .collect::<Vec<_>>();
        master
            .sources
            .insert(source_id, audio_source(chunks, false));
        master.limits.max_blocks_per_source = NonZeroU32::new(8).unwrap();
        master.limits.max_blocks_per_page = NonZeroU32::new(4).unwrap();

        assert!(matches!(
            master.service_next_frame(),
            Err(NativeMasterError::BoundsExceeded)
        ));
    }

    #[test]
    fn only_uncovered_source_bypasses_watermark_and_reserves_actual_count() {
        let starved = input(1);
        let covered = input(2);
        let mut master = audio_test_master(&[(starved, 0.1), (covered, 0.2)], 1);
        master.sources.insert(
            starved,
            audio_source(
                (0..30).map(|sample| audio_chunk(sample, &[0.1])).collect(),
                false,
            ),
        );
        master.sources.insert(
            covered,
            audio_source(
                (0..9)
                    .map(|block| audio_chunk(block * 300, &[0.2; 300]))
                    .collect(),
                false,
            ),
        );

        assert!(!master.service_next_frame().unwrap());
        assert_eq!(
            master.sources[&starved]
                .in_flight
                .map(|reservation| reservation.count.get()),
            Some(2)
        );
        assert!(master.sources[&covered].in_flight.is_none());
        let telemetry = master.audio_telemetry();
        assert_eq!(telemetry.reservation_requests, 1);
        assert_eq!(telemetry.reserved_blocks, 2);
        assert_eq!(telemetry.peak_reserved_blocks, 2);
        assert_eq!(telemetry.source_stalls, 1);
    }

    #[test]
    fn urgent_high_id_reserves_before_low_id_speculation_at_exact_global_budget() {
        let speculative = input(1);
        let urgent = input(2);
        let mut master = audio_test_master(&[(speculative, 0.1), (urgent, 0.2)], 1);
        master.sources.insert(
            speculative,
            audio_source(vec![audio_chunk(0, &[0.1; 2_400])], false),
        );
        master.sources.insert(
            urgent,
            audio_source(
                (0..30).map(|sample| audio_chunk(sample, &[0.2])).collect(),
                false,
            ),
        );
        let retained_samples = 2_430;
        let speculative_reservation = NativeAudioLimits::default().max_samples_per_page;
        master.limits.max_retained_blocks = 47;
        master.limits.max_retained_samples = retained_samples + speculative_reservation;
        master.limits.max_retained_bytes =
            (retained_samples + speculative_reservation) * size_of::<f32>();

        assert!(!master.service_next_frame().unwrap());
        assert!(master.sources[&speculative].in_flight.is_none());
        assert_eq!(
            master.sources[&urgent]
                .in_flight
                .map(|reservation| reservation.charge),
            Some(NativeAudioCharge {
                blocks: 2,
                samples: 8_192,
                bytes: 8_192 * size_of::<f32>(),
            })
        );
        assert_eq!(
            master.limits.max_retained_blocks,
            master.retained_blocks() + 16
        );
        assert_eq!(
            master.limits.max_retained_samples,
            master.retained_samples() + speculative_reservation
        );
        assert_eq!(
            master.limits.max_retained_bytes,
            master.retained_bytes() + speculative_reservation * size_of::<f32>()
        );
        assert_eq!(master.audio_telemetry().reservation_requests, 1);
    }

    #[test]
    fn eos_padding_preflights_all_sources_before_any_commit() {
        let first = input(1);
        let second = input(2);
        let mut master = audio_test_master(&[(first, 0.1), (second, 0.2)], 1);
        master.frame_rate = FrameRate::new(12_000, 1).unwrap();
        master
            .sources
            .insert(first, audio_source(vec![audio_chunk(0, &[0.1; 3])], true));
        master
            .sources
            .insert(second, audio_source(vec![audio_chunk(0, &[0.2; 3])], true));
        master.limits.max_retained_samples = 7;
        master.limits.max_retained_bytes = 7 * size_of::<f32>();
        let before = [first, second].map(|input| {
            master.sources[&input]
                .synchronizer
                .as_ref()
                .unwrap()
                .telemetry()
        });

        assert!(matches!(
            master.service_next_frame(),
            Err(NativeMasterError::BoundsExceeded)
        ));
        for (input, expected) in [first, second].into_iter().zip(before) {
            assert_eq!(
                master.sources[&input]
                    .synchronizer
                    .as_ref()
                    .unwrap()
                    .telemetry(),
                expected
            );
        }
        assert_eq!(master.audio_telemetry().eos_padding_blocks, 0);
    }

    #[test]
    fn multi_block_eos_late_bound_failure_rolls_back_every_source() {
        let first = input(1);
        let second = input(2);
        let mut master = audio_test_master(&[(first, 0.1), (second, 0.2)], 1);
        master.frame_rate = FrameRate::new(8_000, 1).unwrap();
        master
            .sources
            .insert(first, audio_source(vec![audio_chunk(0, &[0.1])], true));
        master
            .sources
            .insert(second, audio_source(vec![audio_chunk(0, &[0.2])], true));
        master.limits.max_samples_per_page = 2;
        master.limits.max_retained_blocks = 8;
        master.limits.max_retained_samples = 11;
        master.limits.max_retained_bytes = 11 * size_of::<f32>();
        let before = [first, second].map(|input| {
            let source = &master.sources[&input];
            (
                source.next_sample,
                source.next_sequence,
                source.synchronizer.as_ref().unwrap().telemetry(),
            )
        });

        assert!(matches!(
            master.service_next_frame(),
            Err(NativeMasterError::BoundsExceeded)
        ));
        assert_eq!(master.scratch.padding_spans.len(), 6);
        for (input, expected) in [first, second].into_iter().zip(before) {
            let source = &master.sources[&input];
            assert_eq!(
                (
                    source.next_sample,
                    source.next_sequence,
                    source.synchronizer.as_ref().unwrap().telemetry(),
                ),
                expected
            );
        }
        assert_eq!(master.audio_telemetry().eos_padding_blocks, 0);
        assert_eq!(master.audio_telemetry().eos_padding_samples, 0);
    }

    #[test]
    fn eos_padding_reuses_preallocated_staging_for_multiple_blocks() {
        let source_id = input(1);
        let mut master = audio_test_master(&[(source_id, 0.1)], 1);
        master.frame_rate = FrameRate::new(8_000, 1).unwrap();
        master
            .sources
            .insert(source_id, audio_source(vec![audio_chunk(0, &[0.1])], true));
        master.limits.max_samples_per_page = 2;
        let spans_pointer = master.scratch.padding_spans.as_ptr();
        let spans_capacity = master.scratch.padding_spans.capacity();
        let sources_pointer = master.scratch.padding_sources.as_ptr();
        let sources_capacity = master.scratch.padding_sources.capacity();

        assert!(master.service_next_frame().unwrap());
        assert_eq!(master.scratch.padding_spans.len(), 3);
        assert_eq!(master.scratch.padding_spans.as_ptr(), spans_pointer);
        assert_eq!(master.scratch.padding_spans.capacity(), spans_capacity);
        assert_eq!(master.scratch.padding_sources.as_ptr(), sources_pointer);
        assert_eq!(master.scratch.padding_sources.capacity(), sources_capacity);
        assert_eq!(master.audio_telemetry().eos_padding_blocks, 3);
        assert_eq!(master.audio_telemetry().eos_padding_samples, 5);
    }

    #[test]
    fn runtime_reuses_per_source_mix_and_plan_scratch() {
        let active = input(1);
        let inactive = input(2);
        let mut master = audio_test_master(&[(active, 0.1), (inactive, 0.2)], 2);
        let active_planes = master.scratch.rendered[&active][0].as_ptr();
        let inactive_planes = master.scratch.rendered[&inactive][0].as_ptr();
        let mix = master.scratch.mix[0].as_ptr();
        let plan_capacity = master.scratch.plans.capacity();

        for frame in 0..2 {
            assert!(master.service_next_frame().unwrap());
            master
                .render_frame_audio(&frame_result(frame, active, None))
                .unwrap();
        }

        assert_eq!(master.scratch.rendered[&active][0].as_ptr(), active_planes);
        assert_eq!(
            master.scratch.rendered[&inactive][0].as_ptr(),
            inactive_planes
        );
        assert_eq!(master.scratch.mix[0].as_ptr(), mix);
        assert_eq!(master.scratch.plans.capacity(), plan_capacity);
    }

    #[test]
    fn audio_mix_plan_maps_cut_and_exact_fade_interval_endpoints() {
        let old = input(1);
        let new = input(2);
        assert_eq!(
            native_audio_mix_plan(ProgramFrame {
                primary: old,
                secondary: Some(new),
                transition_kind: Some(SwitcherTransitionKind::Fade),
                mix_numerator: 2,
                mix_denominator: 4,
                mix_start_numerator: 2,
                mix_end_numerator: 3,
            })
            .unwrap(),
            NativeAudioMixPlan {
                primary: old,
                primary_gain: SourceGain::new(2, 1, 4).unwrap(),
                secondary: Some((new, SourceGain::new(2, 3, 4).unwrap())),
            }
        );
        assert_eq!(
            native_audio_mix_plan(ProgramFrame {
                primary: new,
                secondary: None,
                transition_kind: None,
                mix_numerator: u32::MAX,
                mix_denominator: 0,
                mix_start_numerator: u32::MAX,
                mix_end_numerator: u32::MAX,
            })
            .unwrap(),
            NativeAudioMixPlan {
                primary: new,
                primary_gain: SourceGain::UNITY,
                secondary: None,
            }
        );
        assert!(
            native_audio_mix_plan(ProgramFrame {
                primary: old,
                secondary: Some(new),
                transition_kind: Some(SwitcherTransitionKind::Fade),
                mix_numerator: 1,
                mix_denominator: 0,
                mix_start_numerator: 1,
                mix_end_numerator: 1,
            })
            .is_err()
        );
        assert_eq!(
            native_audio_mix_plan(ProgramFrame {
                primary: old,
                secondary: Some(old),
                transition_kind: Some(SwitcherTransitionKind::Fade),
                mix_numerator: u32::MAX,
                mix_denominator: 0,
                mix_start_numerator: u32::MAX,
                mix_end_numerator: u32::MAX,
            })
            .unwrap(),
            NativeAudioMixPlan {
                primary: old,
                primary_gain: SourceGain::UNITY,
                secondary: None,
            }
        );
    }

    #[test]
    fn fade_to_black_video_plan_renders_the_exact_interval_endpoint() {
        let old = input(1);
        let new = input(2);
        let mut switcher = SwitcherState::new(vec![old, new], old, new).unwrap();
        switcher.request_fade_to_black(true, 4).unwrap();

        let first = native_fade_to_black_plan(switcher.fade_to_black_frame()).unwrap();
        assert_eq!(first.start().numerator(), 0);
        assert_eq!(first.start().denominator(), 65_535);
        assert_eq!(first.end().numerator(), 16_383);
        assert_eq!(first.end().denominator(), 65_535);
        assert_eq!(first.progress(), CompositorFadeToBlackPosition::BLACK);

        let _ = switcher.advance_frame_events();
        switcher.request_fade_to_black(false, 2).unwrap();
        let reverse = native_fade_to_black_plan(switcher.fade_to_black_frame()).unwrap();
        assert_eq!(reverse.start().numerator(), 16_383);
        assert_eq!(reverse.end().numerator(), 8_192);
        assert_eq!(reverse.progress(), CompositorFadeToBlackPosition::BLACK);
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn fade_to_black_audio_uses_master_sample_endpoint_convention() {
        let old = input(1);
        let new = input(2);
        let mut switcher = SwitcherState::new(vec![old, new], old, new).unwrap();
        switcher.request_fade_to_black(true, 2).unwrap();
        let forward = switcher.fade_to_black_frame();
        let mut planes = vec![vec![1.0; 4], vec![0.5; 4]];

        apply_fade_to_black_audio(&mut planes, 4, forward);

        let end_gain = f64::from(65_535 - 32_767) / 65_535.0;
        let expected = std::array::from_fn::<_, 4, _>(|sample| {
            let sample = u32::try_from(sample + 1).unwrap();
            (1.0 + (end_gain - 1.0) * f64::from(sample) / 4.0) as f32
        });
        for (actual, expected) in planes[0].iter().zip(expected) {
            assert!((actual - expected).abs() < 1.0e-6);
        }
        for (actual, expected) in planes[1].iter().zip(expected) {
            assert!((actual - expected * 0.5).abs() < 1.0e-6);
        }

        let _ = switcher.advance_frame_events();
        switcher.request_fade_to_black(false, 1).unwrap();
        let mut reverse = vec![vec![1.0; 4]];
        apply_fade_to_black_audio(&mut reverse, 4, switcher.fade_to_black_frame());
        assert!(reverse[0].windows(2).all(|samples| samples[0] < samples[1]));
        assert_sample_exact(reverse[0][3], 1.0);
    }

    #[test]
    fn native_master_output_applies_fade_to_black_after_program_mix() {
        let active = input(1);
        let preview = input(2);
        let mut project = native_plan_project(4, 2);
        add_leaf(&mut project, active);
        let plan = NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();
        let mut master = audio_test_master(&[(active, 1.0)], 2);
        master.realize_project_audio(&plan).unwrap();
        let mut switcher = SwitcherState::new(vec![active, preview], active, preview).unwrap();
        switcher.request_fade_to_black(true, 1).unwrap();

        assert!(master.service_next_frame().unwrap());
        let mut first = frame_result(0, active, None);
        first.fade_to_black = switcher.fade_to_black_frame();
        let fading = master.render_project_frame_audio(&first, &plan).unwrap();
        assert!(fading.plane(0).unwrap()[0] < 1.0);
        assert_sample_exact(fading.plane(0).unwrap()[fading.sample_count() - 1], 0.0);

        let _ = switcher.advance_frame_events();
        assert!(master.service_next_frame().unwrap());
        let mut second = frame_result(1, active, None);
        second.fade_to_black = switcher.fade_to_black_frame();
        let black = master.render_project_frame_audio(&second, &plan).unwrap();
        assert!(black.plane(0).unwrap().iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn wipe_audio_mix_plan_has_explicit_endpoints() {
        let old = input(1);
        let new = input(2);
        let plan = |start, end| {
            native_audio_mix_plan(ProgramFrame {
                primary: old,
                secondary: Some(new),
                transition_kind: Some(SwitcherTransitionKind::Wipe),
                mix_numerator: start,
                mix_denominator: 2,
                mix_start_numerator: start,
                mix_end_numerator: end,
            })
            .unwrap()
        };

        assert_eq!(
            plan(0, 1),
            NativeAudioMixPlan {
                primary: old,
                primary_gain: SourceGain::new(2, 1, 2).unwrap(),
                secondary: Some((new, SourceGain::new(0, 1, 2).unwrap())),
            }
        );
        assert_eq!(
            plan(1, 2),
            NativeAudioMixPlan {
                primary: old,
                primary_gain: SourceGain::new(1, 0, 2).unwrap(),
                secondary: Some((new, SourceGain::new(1, 2, 2).unwrap())),
            }
        );
    }

    #[test]
    fn wipe_t_bar_audio_plan_holds_reverses_and_reaches_manual_endpoints() {
        let old = input(1);
        let new = input(2);
        let mut switcher = SwitcherState::new(vec![old, new], old, new).unwrap();
        switcher
            .apply(SwitcherCommand::StartTBar {
                kind: SwitcherTransitionKind::Wipe,
            })
            .unwrap();
        switcher
            .apply(SwitcherCommand::SetTBarPosition(
                TBarPosition::new(8_000).unwrap(),
            ))
            .unwrap();
        assert_eq!(
            native_audio_mix_plan(switcher.program_frame()).unwrap(),
            NativeAudioMixPlan {
                primary: old,
                primary_gain: SourceGain::new(10_000, 2_000, 10_000).unwrap(),
                secondary: Some((new, SourceGain::new(0, 8_000, 10_000).unwrap())),
            }
        );

        assert_eq!(switcher.advance_frame(), None);
        assert_eq!(
            native_audio_mix_plan(switcher.program_frame()).unwrap(),
            NativeAudioMixPlan {
                primary: old,
                primary_gain: SourceGain::new(2_000, 2_000, 10_000).unwrap(),
                secondary: Some((new, SourceGain::new(8_000, 8_000, 10_000).unwrap())),
            }
        );

        switcher
            .apply(SwitcherCommand::SetTBarPosition(
                TBarPosition::new(2_500).unwrap(),
            ))
            .unwrap();
        assert_eq!(
            native_audio_mix_plan(switcher.program_frame()).unwrap(),
            NativeAudioMixPlan {
                primary: old,
                primary_gain: SourceGain::new(2_000, 7_500, 10_000).unwrap(),
                secondary: Some((new, SourceGain::new(8_000, 2_500, 10_000).unwrap())),
            }
        );

        assert_eq!(switcher.advance_frame(), None);
        switcher
            .apply(SwitcherCommand::SetTBarPosition(TBarPosition::END))
            .unwrap();
        assert_eq!(
            native_audio_mix_plan(switcher.program_frame()).unwrap(),
            NativeAudioMixPlan {
                primary: old,
                primary_gain: SourceGain::new(7_500, 0, 10_000).unwrap(),
                secondary: Some((new, SourceGain::new(2_500, 10_000, 10_000).unwrap())),
            }
        );

        switcher.apply(SwitcherCommand::CommitTBar).unwrap();
        assert_eq!(
            native_audio_mix_plan(switcher.program_frame()).unwrap(),
            NativeAudioMixPlan {
                primary: new,
                primary_gain: SourceGain::UNITY,
                secondary: None,
            }
        );
    }

    #[test]
    fn slide_and_zoom_audio_crossfade_while_stinger_fails_explicitly() {
        let old = input(1);
        let new = input(2);
        let program = |transition_kind| ProgramFrame {
            primary: old,
            secondary: Some(new),
            transition_kind,
            mix_numerator: 0,
            mix_denominator: 1,
            mix_start_numerator: 0,
            mix_end_numerator: 1,
        };

        assert_eq!(
            native_audio_mix_plan(program(Some(SwitcherTransitionKind::Slide))).unwrap(),
            NativeAudioMixPlan {
                primary: old,
                primary_gain: SourceGain::new(1, 0, 1).unwrap(),
                secondary: Some((new, SourceGain::new(0, 1, 1).unwrap())),
            }
        );

        assert_eq!(
            native_audio_mix_plan(program(Some(SwitcherTransitionKind::Zoom))).unwrap(),
            NativeAudioMixPlan {
                primary: old,
                primary_gain: SourceGain::new(1, 0, 1).unwrap(),
                secondary: Some((new, SourceGain::new(0, 1, 1).unwrap())),
            }
        );

        let stinger = SwitcherTransitionKind::Stinger(fm_switcher::StingerSlotId::new(1).unwrap());
        let Err(NativeMasterError::UnsupportedAudioTransition(actual)) =
            native_audio_mix_plan(program(Some(stinger)))
        else {
            panic!("expected unsupported audio transition {stinger:?}");
        };
        assert_eq!(actual, stinger);
        assert!(matches!(
            native_audio_mix_plan(program(None)),
            Err(NativeMasterError::MissingAudioTransitionKind)
        ));
    }

    #[test]
    fn configured_stinger_audio_policies_switch_base_at_the_video_cut() {
        let program = input(1);
        let preview = input(2);
        let media = input(3);
        let slot = fm_switcher::StingerSlotId::new(1).unwrap();
        let frame = |frame_index| ProgramFrame {
            primary: program,
            secondary: Some(preview),
            transition_kind: Some(SwitcherTransitionKind::Stinger(slot)),
            mix_numerator: frame_index,
            mix_denominator: 4,
            mix_start_numerator: frame_index,
            mix_end_numerator: frame_index + 1,
        };
        let compile = |audio_policy| {
            let mut project = native_plan_project(4, 2);
            for source in [program, preview, media] {
                add_leaf(&mut project, source);
            }
            project.add_stinger(StingerConfig::new(
                StingerSlotNumber::new(1).unwrap(),
                media,
                true,
                2,
                audio_policy,
                fm_model::StingerMissingMediaFallback::Cut,
            ));
            NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap()
        };

        let muted = compile(fm_model::StingerAudioPolicy::Muted);
        assert_eq!(
            native_project_audio_mix_plan(&muted, frame(1)).unwrap(),
            NativeAudioMixPlan {
                primary: program,
                primary_gain: SourceGain::UNITY,
                secondary: None,
            }
        );
        assert_eq!(
            native_project_audio_mix_plan(&muted, frame(2)).unwrap(),
            NativeAudioMixPlan {
                primary: preview,
                primary_gain: SourceGain::UNITY,
                secondary: None,
            }
        );

        let only = compile(fm_model::StingerAudioPolicy::StingerOnly);
        assert_eq!(
            native_project_audio_mix_plan(&only, frame(1)).unwrap(),
            NativeAudioMixPlan {
                primary: program,
                primary_gain: SourceGain::UNITY,
                secondary: None,
            }
        );
        assert_eq!(
            native_project_audio_mix_plan(&only, frame(2)).unwrap(),
            NativeAudioMixPlan {
                primary: preview,
                primary_gain: SourceGain::UNITY,
                secondary: None,
            }
        );

        let mixed = compile(fm_model::StingerAudioPolicy::MixWithProgram);
        assert_eq!(
            native_project_audio_mix_plan(&mixed, frame(1)).unwrap(),
            NativeAudioMixPlan {
                primary: program,
                primary_gain: SourceGain::UNITY,
                secondary: None,
            }
        );
        assert_eq!(
            native_project_audio_mix_plan(&mixed, frame(2)).unwrap(),
            NativeAudioMixPlan {
                primary: preview,
                primary_gain: SourceGain::UNITY,
                secondary: None,
            }
        );
    }

    #[test]
    fn native_master_realizes_each_configured_stinger_audio_policy() {
        let program = input(1);
        let preview = input(2);
        let media = input(3);
        let slot = fm_switcher::StingerSlotId::new(1).unwrap();
        let render = |audio_policy, frame_index| {
            let mut project = native_plan_project(4, 2);
            for source in [program, preview, media] {
                add_leaf(&mut project, source);
            }
            project.add_stinger(StingerConfig::new(
                StingerSlotNumber::new(1).unwrap(),
                media,
                true,
                2,
                audio_policy,
                fm_model::StingerMissingMediaFallback::Cut,
            ));
            let project =
                NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();
            let mut master = audio_test_master(&[(program, 0.1), (preview, 0.2), (media, 0.4)], 1);
            master.realize_project_audio(&project).unwrap();
            if audio_policy != fm_model::StingerAudioPolicy::Muted {
                master.stinger_audio = Some(NativeStingerAudioPlayback {
                    masters: BTreeMap::from([(
                        media,
                        Box::new(audio_test_master(&[(media, 0.4)], 1)),
                    )]),
                    silent_inputs: BTreeSet::new(),
                    active_trigger: None,
                    ready: None,
                });
            }
            let mut rendered = 0.0;
            for current in 0..=frame_index {
                let frame = frame_result_with_transition_interval(
                    u64::from(current),
                    program,
                    Some(preview),
                    Some(SwitcherTransitionKind::Stinger(slot)),
                    current,
                    4,
                    current,
                    current + 1,
                );
                assert!(master.service_project_next_frame(&frame, &project).unwrap());
                rendered = master
                    .render_project_frame_audio(&frame, &project)
                    .unwrap()
                    .plane(0)
                    .unwrap()[0];
            }
            rendered
        };
        let assert_close = |actual: f32, expected: f32| {
            assert!((actual - expected).abs() < 1.0e-6, "{actual} != {expected}");
        };

        assert_close(render(fm_model::StingerAudioPolicy::Muted, 1), 0.1);
        assert_close(render(fm_model::StingerAudioPolicy::Muted, 2), 0.2);
        assert_close(render(fm_model::StingerAudioPolicy::StingerOnly, 1), 0.4);
        assert_close(render(fm_model::StingerAudioPolicy::StingerOnly, 2), 0.4);
        assert_close(render(fm_model::StingerAudioPolicy::MixWithProgram, 1), 0.5);
        assert_close(render(fm_model::StingerAudioPolicy::MixWithProgram, 2), 0.6);
    }

    #[test]
    fn clip_local_mix_applies_the_master_clipping_policy_after_summing() {
        let format = mono_audio_format();
        let clock = ClockDomainId::new(NonZeroU128::new(9).unwrap());
        let clip = audio_block(0, 0, &[0.8], &format, clock);
        let mut clamped = vec![vec![0.8]];
        mix_clip_local_stinger_audio(
            &mut clamped,
            1,
            ModelStingerAudioPolicy::MixWithProgram,
            Some(&clip),
            &format,
        )
        .unwrap();
        apply_master_clipping(&mut clamped, 1, ClippingPolicy::Clamp);
        assert_eq!(clamped, vec![vec![1.0]]);

        let mut allowed = vec![vec![0.8]];
        mix_clip_local_stinger_audio(
            &mut allowed,
            1,
            ModelStingerAudioPolicy::MixWithProgram,
            Some(&clip),
            &format,
        )
        .unwrap();
        apply_master_clipping(&mut allowed, 1, ClippingPolicy::Allow);
        assert!((allowed[0][0] - 1.6).abs() < 1.0e-6);
    }

    #[test]
    fn clip_local_stinger_audio_reanchors_fractional_cadence_to_trigger_frame() {
        let program = input(1);
        let preview = input(2);
        let media = input(3);
        let slot = fm_switcher::StingerSlotId::new(1).unwrap();
        let rate = FrameRate::new(60_000, 1_001).unwrap();
        let mut project = native_plan_project(4, 2);
        for source in [program, preview, media] {
            add_leaf(&mut project, source);
        }
        project.add_stinger(StingerConfig::new(
            StingerSlotNumber::new(1).unwrap(),
            media,
            true,
            1,
            fm_model::StingerAudioPolicy::StingerOnly,
            fm_model::StingerMissingMediaFallback::Cut,
        ));
        let project = NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();
        let mut master = audio_test_master(&[(program, 0.1), (preview, 0.2), (media, 0.4)], 2);
        master.frame_rate = rate;
        master.realize_project_audio(&project).unwrap();
        let mut clip = audio_test_master(&[(media, 0.4)], 2);
        clip.frame_rate = rate;
        master.stinger_audio = Some(NativeStingerAudioPlayback {
            masters: BTreeMap::from([(media, Box::new(clip))]),
            silent_inputs: BTreeSet::new(),
            active_trigger: None,
            ready: None,
        });

        let idle = frame_result(0, program, None);
        assert!(master.service_project_next_frame(&idle, &project).unwrap());
        assert_eq!(
            master
                .render_project_frame_audio(&idle, &project)
                .unwrap()
                .sample_count(),
            800
        );

        let stinger = frame_result_with_transition_interval(
            1,
            program,
            Some(preview),
            Some(SwitcherTransitionKind::Stinger(slot)),
            0,
            2,
            0,
            1,
        );
        assert!(
            master
                .service_project_next_frame(&stinger, &project)
                .unwrap()
        );
        let rendered = master
            .render_project_frame_audio(&stinger, &project)
            .unwrap();
        assert_eq!(rendered.sample_count(), 801);
        assert!(
            rendered
                .plane(0)
                .unwrap()
                .iter()
                .all(|sample| (*sample - 0.4).abs() < 1.0e-6)
        );
    }

    #[test]
    fn stinger_audio_services_only_selected_media_and_reports_aggregate_retention() {
        let program = input(1);
        let preview = input(2);
        let selected = input(3);
        let unrelated = input(4);
        let slot = fm_switcher::StingerSlotId::new(1).unwrap();
        let mut project = native_plan_project(4, 2);
        for source in [program, preview, selected, unrelated] {
            add_leaf(&mut project, source);
        }
        project.add_stinger(StingerConfig::new(
            StingerSlotNumber::new(1).unwrap(),
            selected,
            true,
            1,
            fm_model::StingerAudioPolicy::StingerOnly,
            fm_model::StingerMissingMediaFallback::Cut,
        ));
        project.add_stinger(StingerConfig::new(
            StingerSlotNumber::new(2).unwrap(),
            unrelated,
            true,
            1,
            fm_model::StingerAudioPolicy::StingerOnly,
            fm_model::StingerMissingMediaFallback::Cut,
        ));
        let project = NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();
        let mut master = audio_test_master(
            &[
                (program, 0.1),
                (preview, 0.2),
                (selected, 0.3),
                (unrelated, 0.4),
            ],
            1,
        );
        master.realize_project_audio(&project).unwrap();
        let mut selected_master = audio_test_master(&[(selected, 0.3)], 1);
        selected_master.collect_output = false;
        let mut unrelated_master = audio_test_master(&[(unrelated, 0.4)], 1);
        unrelated_master.collect_output = false;
        master.stinger_audio = Some(NativeStingerAudioPlayback {
            masters: BTreeMap::from([
                (selected, Box::new(selected_master)),
                (unrelated, Box::new(unrelated_master)),
            ]),
            silent_inputs: BTreeSet::new(),
            active_trigger: None,
            ready: None,
        });
        assert!(master.retained_blocks() > master.retained_charge().blocks);

        let frame = frame_result_with_transition_interval(
            0,
            program,
            Some(preview),
            Some(SwitcherTransitionKind::Stinger(slot)),
            0,
            2,
            0,
            1,
        );
        assert!(master.service_project_next_frame(&frame, &project).unwrap());
        master.render_project_frame_audio(&frame, &project).unwrap();

        let playback = master.stinger_audio.as_ref().unwrap();
        assert_eq!(playback.masters[&selected].expected_next_frame(), 1);
        assert_eq!(playback.masters[&unrelated].expected_next_frame(), 0);
        assert_eq!(playback.masters[&selected].sink_len(), 0);
        assert_eq!(playback.masters[&unrelated].sink_len(), 0);
        assert_eq!(master.sink_len(), 1);
    }

    #[test]
    fn video_only_stinger_audio_uses_silence_without_splitting_retention() {
        let program = input(1);
        let preview = input(2);
        let media = input(3);
        let slot = fm_switcher::StingerSlotId::new(1).unwrap();
        let mut project = native_plan_project(4, 2);
        for source in [program, preview, media] {
            add_leaf(&mut project, source);
        }
        project.add_stinger(StingerConfig::new(
            StingerSlotNumber::new(1).unwrap(),
            media,
            true,
            1,
            fm_model::StingerAudioPolicy::StingerOnly,
            fm_model::StingerMissingMediaFallback::Cut,
        ));
        let project = NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();
        let clock = ClockDomainId::new(NonZeroU128::new(9).unwrap());
        let resolved = [program, preview, media].map(|input| NativeResolvedSource::RetainedFrame {
            input,
            frame: retained_frame(clock, 0, 0),
        });
        let limits = NativeAudioLimits::default();
        let mut master = NativeMasterRuntime::preflight_project_local_blocking(
            None,
            &resolved,
            &project,
            &mono_audio_format(),
            FrameRate::new(25, 1).unwrap(),
            clock,
            0,
            limits,
        )
        .unwrap();
        assert_eq!(master.limits, limits);
        let playback = master.stinger_audio.as_ref().unwrap();
        assert!(playback.masters.is_empty());
        assert_eq!(playback.silent_inputs, BTreeSet::from([media]));

        let frame = frame_result_with_transition_interval(
            0,
            program,
            Some(preview),
            Some(SwitcherTransitionKind::Stinger(slot)),
            0,
            2,
            0,
            1,
        );
        assert!(master.service_project_next_frame(&frame, &project).unwrap());
        let rendered = master.render_project_frame_audio(&frame, &project).unwrap();
        assert!(
            rendered
                .plane(0)
                .unwrap()
                .iter()
                .all(|sample| *sample == 0.0)
        );
    }

    #[test]
    fn fade_to_black_revision_change_does_not_restart_stinger_audio() {
        let program = input(1);
        let preview = input(2);
        let media = input(3);
        let slot = fm_switcher::StingerSlotId::new(1).unwrap();
        let mut project = native_plan_project(4, 2);
        for source in [program, preview, media] {
            add_leaf(&mut project, source);
        }
        project.add_stinger(StingerConfig::new(
            StingerSlotNumber::new(1).unwrap(),
            media,
            true,
            1,
            fm_model::StingerAudioPolicy::StingerOnly,
            fm_model::StingerMissingMediaFallback::Cut,
        ));
        let project = NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();
        let mut master = audio_test_master(&[(program, 0.1), (preview, 0.2), (media, 0.4)], 1);
        master.realize_project_audio(&project).unwrap();
        master.stinger_audio = Some(NativeStingerAudioPlayback {
            masters: BTreeMap::from([(media, Box::new(audio_test_master(&[(media, 0.4)], 1)))]),
            silent_inputs: BTreeSet::new(),
            active_trigger: None,
            ready: None,
        });
        let first = frame_result_with_transition_interval(
            0,
            program,
            Some(preview),
            Some(SwitcherTransitionKind::Stinger(slot)),
            0,
            2,
            0,
            1,
        );
        assert!(master.service_project_next_frame(&first, &project).unwrap());
        master.render_project_frame_audio(&first, &project).unwrap();

        let mut switcher = SwitcherState::new(vec![program, preview], program, preview).unwrap();
        switcher.request_fade_to_black(true, 1).unwrap();
        let mut second = frame_result_with_transition_interval(
            1,
            program,
            Some(preview),
            Some(SwitcherTransitionKind::Stinger(slot)),
            1,
            2,
            1,
            2,
        );
        second.revision = Revision::new(1);
        second.runtime_generation = RuntimeGeneration::new(1);
        second.fade_to_black = switcher.fade_to_black_frame();

        assert!(
            master
                .service_project_next_frame(&second, &project)
                .unwrap()
        );
        let rendered = master
            .render_project_frame_audio(&second, &project)
            .unwrap();
        let samples = rendered.plane(0).unwrap();
        assert!(samples[0] < 0.4);
        assert!(samples[0] > 0.0);
        assert!(samples[samples.len() - 1].abs() < 1.0e-7);
        assert_eq!(
            master.stinger_audio.as_ref().unwrap().masters[&media].expected_next_frame(),
            2
        );
    }

    #[test]
    fn engine_ticks_propagate_automatic_and_manual_intervals_to_audio_plans() {
        let old = input(1);
        let new = input(2);
        let inputs = vec![old, new];
        let frame_rate = FrameRate::new(25, 1).unwrap();
        let clock_domain = EngineClockDomainId::new(NonZeroU128::new(99).unwrap());
        let show = || ShowState::new("interval propagation", inputs.clone(), old, new).unwrap();

        let mut fade = Engine::new(show(), frame_rate, clock_domain);
        fade.execute(
            CommandEnvelope::new(
                "fade-command",
                IdempotencyKey::new("fade-intervals"),
                EngineCommand::Fade { duration_frames: 2 },
            ),
            0,
        )
        .unwrap();
        let first = native_audio_mix_plan(fade.tick().unwrap().program).unwrap();
        let second = native_audio_mix_plan(fade.tick().unwrap().program).unwrap();
        let after = native_audio_mix_plan(fade.tick().unwrap().program).unwrap();
        assert_eq!(
            first,
            NativeAudioMixPlan {
                primary: old,
                primary_gain: SourceGain::new(2, 1, 2).unwrap(),
                secondary: Some((new, SourceGain::new(0, 1, 2).unwrap())),
            }
        );
        assert_eq!(
            second,
            NativeAudioMixPlan {
                primary: old,
                primary_gain: SourceGain::new(1, 0, 2).unwrap(),
                secondary: Some((new, SourceGain::new(1, 2, 2).unwrap())),
            }
        );
        assert_eq!(
            after,
            NativeAudioMixPlan {
                primary: new,
                primary_gain: SourceGain::UNITY,
                secondary: None,
            }
        );

        let t_bar_frame = |end: u16| {
            let mut engine = Engine::new(show(), frame_rate, clock_domain);
            for (key, command) in [
                (
                    "manual-start",
                    EngineCommand::StartManualTransition {
                        kind: EngineManualTransitionKind::Fade,
                    },
                ),
                (
                    "manual-forward",
                    EngineCommand::SetManualTransitionPosition {
                        position: EngineManualTransitionPosition::new(8_000).unwrap(),
                    },
                ),
            ] {
                engine
                    .execute(
                        CommandEnvelope::new(key, IdempotencyKey::new(key), command),
                        0,
                    )
                    .unwrap();
                engine.tick().unwrap();
            }
            engine
                .execute(
                    CommandEnvelope::new(
                        "manual-end",
                        IdempotencyKey::new("manual-end"),
                        EngineCommand::SetManualTransitionPosition {
                            position: EngineManualTransitionPosition::new(end).unwrap(),
                        },
                    ),
                    0,
                )
                .unwrap();
            engine.tick().unwrap()
        };
        let held = native_audio_mix_plan(t_bar_frame(8_000).program).unwrap();
        let reversed = native_audio_mix_plan(t_bar_frame(2_500).program).unwrap();
        assert_eq!(
            held,
            NativeAudioMixPlan {
                primary: old,
                primary_gain: SourceGain::new(2_000, 2_000, 10_000).unwrap(),
                secondary: Some((new, SourceGain::new(8_000, 8_000, 10_000).unwrap())),
            }
        );
        assert_eq!(
            reversed,
            NativeAudioMixPlan {
                primary: old,
                primary_gain: SourceGain::new(2_000, 7_500, 10_000).unwrap(),
                secondary: Some((new, SourceGain::new(8_000, 2_500, 10_000).unwrap())),
            }
        );
    }

    #[test]
    fn manual_alpha_fade_engine_interval_reaches_native_video_and_audio_plans() {
        let old = input(1);
        let new = input(2);
        let frame_rate = FrameRate::new(25, 1).unwrap();
        let clock_domain = EngineClockDomainId::new(NonZeroU128::new(99).unwrap());
        let show = ShowState::new("manual alpha", vec![old, new], old, new).unwrap();
        let mut engine = Engine::new(show, frame_rate, clock_domain);
        for (key, command) in [
            (
                "manual-alpha-start",
                EngineCommand::StartManualTransition {
                    kind: EngineManualTransitionKind::AlphaFade,
                },
            ),
            (
                "manual-alpha-forward",
                EngineCommand::SetManualTransitionPosition {
                    position: EngineManualTransitionPosition::new(8_000).unwrap(),
                },
            ),
        ] {
            engine
                .execute(
                    CommandEnvelope::new(key, IdempotencyKey::new(key), command),
                    0,
                )
                .unwrap();
            let _ = engine.tick().unwrap();
        }
        engine
            .execute(
                CommandEnvelope::new(
                    "manual-alpha-reverse",
                    IdempotencyKey::new("manual-alpha-reverse"),
                    EngineCommand::SetManualTransitionPosition {
                        position: EngineManualTransitionPosition::new(6_250).unwrap(),
                    },
                ),
                0,
            )
            .unwrap();
        let frame = engine.tick().unwrap().program;

        assert_eq!(
            native_audio_mix_plan(frame).unwrap(),
            NativeAudioMixPlan {
                primary: old,
                primary_gain: SourceGain::new(2_000, 3_750, 10_000).unwrap(),
                secondary: Some((new, SourceGain::new(8_000, 6_250, 10_000).unwrap())),
            }
        );
        assert_eq!(
            native_mix_plan(frame).unwrap().transition.kind(),
            TransitionKind::AlphaFade
        );
    }

    #[test]
    fn identical_fade_sources_render_once_at_unity_without_poisoning_runtime() {
        let source = input(1);
        let mut master = audio_test_master(&[(source, 0.25)], 2);

        assert!(master.service_next_frame().unwrap());
        let output = master
            .render_frame_audio(&frame_result_with_interval(
                0,
                source,
                Some(source),
                u32::MAX,
                0,
                u32::MAX,
                u32::MAX,
            ))
            .unwrap();
        for sample in output.plane(0).unwrap() {
            assert_sample_exact(*sample, 0.25);
        }
        assert_eq!(master.expected_next_frame(), 1);

        assert!(master.service_next_frame().unwrap());
        master
            .render_frame_audio(&frame_result_with_mix(1, source, None, 0, 1))
            .unwrap();
        assert_eq!(master.expected_next_frame(), 2);
    }

    #[test]
    fn t_bar_master_audio_holds_reverses_and_accepts_irregular_ratios() {
        let old = input(1);
        let new = input(2);
        let mut master = audio_test_master(&[(old, 1.0), (new, -1.0)], 3);

        assert!(master.service_next_frame().unwrap());
        let held = master
            .render_frame_audio(&frame_result_with_interval(
                0,
                old,
                Some(new),
                7_500,
                10_000,
                7_500,
                7_500,
            ))
            .unwrap();
        for sample in held.plane(0).unwrap() {
            assert_sample_exact(*sample, -0.5);
        }

        assert!(master.service_next_frame().unwrap());
        let reversed = master
            .render_frame_audio(&frame_result_with_interval(
                1,
                old,
                Some(new),
                2_500,
                10_000,
                7_500,
                2_500,
            ))
            .unwrap();
        let reversed = reversed.plane(0).unwrap();
        assert!((reversed[0] - (-0.5 + 1.0 / 1_920.0)).abs() < 1.0e-6);
        assert_sample_exact(reversed[1_919], 0.5);

        assert!(master.service_next_frame().unwrap());
        let irregular = master
            .render_frame_audio(&frame_result_with_interval(
                2,
                old,
                Some(new),
                7_333,
                10_000,
                2_500,
                7_333,
            ))
            .unwrap();
        assert!((irregular.plane(0).unwrap()[1_919] - -0.4666).abs() < 1.0e-6);
    }

    #[test]
    fn linear_transition_master_audio_is_continuous_and_reaches_cut_endpoint() {
        let old = input(1);
        let new = input(2);
        for kind in [
            SwitcherTransitionKind::Fade,
            SwitcherTransitionKind::Wipe,
            SwitcherTransitionKind::AlphaFade,
        ] {
            let mut master = audio_test_master(&[(old, 1.0), (new, -1.0)], 3);
            let frame = |frame, primary, secondary, start, end, denominator| {
                frame_result_with_transition_interval(
                    frame,
                    primary,
                    secondary,
                    secondary.map(|_| kind),
                    start,
                    denominator,
                    start,
                    end,
                )
            };

            assert!(master.service_next_frame().unwrap());
            let first = master
                .render_frame_audio(&frame(0, old, Some(new), 0, 1, 2))
                .unwrap();
            assert!(master.service_next_frame().unwrap());
            let second = master
                .render_frame_audio(&frame(1, old, Some(new), 1, 2, 2))
                .unwrap();
            assert!(master.service_next_frame().unwrap());
            let cut = master
                .render_frame_audio(&frame(2, new, None, 0, 0, 1))
                .unwrap();

            let first = first.plane(0).unwrap();
            let second = second.plane(0).unwrap();
            let cut = cut.plane(0).unwrap();
            let step = 1.0 / 1_920.0;
            assert!((first[0] - (1.0 - step)).abs() < 1.0e-6);
            assert_sample_exact(first[1_919], 0.0);
            assert!((second[0] + step).abs() < 1.0e-6);
            assert_sample_exact(second[1_919], -1.0);
            assert_sample_exact(cut[0], -1.0);
            assert_sample_exact(cut[1_919], -1.0);
            assert!((second[0] - first[1_919] + step).abs() < 1.0e-6);
        }
    }

    #[test]
    fn fade_master_audio_uses_exact_fractional_cadence_intervals() {
        let old = input(1);
        let new = input(2);
        let mut master = audio_test_master(&[(old, 1.0), (new, -1.0)], 3);
        master.frame_rate = FrameRate::new(30_000, 1_001).unwrap();

        assert!(master.service_next_frame().unwrap());
        let first = master
            .render_frame_audio(&frame_result_with_mix(0, old, Some(new), 0, 2))
            .unwrap();
        assert!(master.service_next_frame().unwrap());
        let second = master
            .render_frame_audio(&frame_result_with_mix(1, old, Some(new), 1, 2))
            .unwrap();
        assert!(master.service_next_frame().unwrap());
        let cut = master
            .render_frame_audio(&frame_result_with_mix(2, new, None, 0, 1))
            .unwrap();

        assert_eq!(first.sample_count(), 1_601);
        assert_eq!(second.sample_count(), 1_602);
        assert_eq!(cut.sample_count(), 1_601);
        assert!((first.plane(0).unwrap()[0] - (1.0 - 1.0 / 1_601.0)).abs() < 1.0e-6);
        assert_sample_exact(first.plane(0).unwrap()[1_600], 0.0);
        assert!((second.plane(0).unwrap()[0] + 1.0 / 1_602.0).abs() < 1.0e-6);
        assert_sample_exact(second.plane(0).unwrap()[1_601], -1.0);
        assert_sample_exact(cut.plane(0).unwrap()[0], -1.0);
    }

    #[test]
    fn fake_audio_sink_is_bounded_and_reports_drop_oldest_telemetry() {
        let source_id = input(1);
        let mut master = silent_test_master(source_id, 1);
        assert!(master.service_next_frame().unwrap());
        master
            .render_frame(&frame_result(0, source_id, None))
            .unwrap();
        assert!(master.service_next_frame().unwrap());
        master
            .render_frame(&frame_result(1, source_id, None))
            .unwrap();

        assert_eq!(master.sink_len(), 1);
        let telemetry = master.sink_telemetry();
        assert_eq!(telemetry.received(), 2);
        assert_eq!(telemetry.accepted(), 2);
        assert_eq!(telemetry.dropped_oldest(), 1);
        assert_eq!(telemetry.high_watermark(), 1);
        let only = master.collected_audio().next().unwrap();
        assert_eq!(only.timing().sequence().get(), 1);
    }

    #[test]
    fn returned_master_audio_exactly_matches_sink_sequence_and_sample_span() {
        let source_id = input(1);
        let mut master = silent_test_master(source_id, 1);
        master.expected_next_frame = 123;

        assert!(master.service_next_frame().unwrap());
        let returned = master
            .render_frame_audio(&frame_result(123, source_id, None))
            .unwrap();

        assert_eq!(master.collected_audio().next(), Some(&returned));
        assert_eq!(returned.timing().sequence().get(), 123);
        assert_eq!(returned.sample_count(), 1_920);
        assert_eq!(master.expected_next_frame(), 124);
    }

    #[test]
    fn returned_master_audio_sink_failure_is_sticky_and_transactional() {
        let source_id = input(1);
        let secondary = input(2);
        let mut master = audio_test_master(&[(source_id, 1.0), (secondary, -1.0)], 1);
        master.sink = CollectingAudioSink::new(1, OverflowPolicy::Reject).unwrap();
        master
            .mixer
            .set_input_state(
                source_id,
                InputState {
                    gain: fm_audio::Gain::SILENCE,
                    follow_video: true,
                    ..InputState::default()
                },
                3_840,
            )
            .unwrap();

        assert!(master.service_next_frame().unwrap());
        let first = master
            .render_frame_audio(&frame_result_with_mix(0, source_id, Some(secondary), 0, 2))
            .unwrap();
        let gain_after_first = master.mixer.current_linear_gain(source_id);
        assert!((gain_after_first.unwrap() - 0.5).abs() < 1.0e-5);
        assert!(master.service_next_frame().unwrap());
        let ready = master.ready_frame;
        let source_before = master.sources[&source_id]
            .synchronizer
            .as_ref()
            .unwrap()
            .telemetry();
        let secondary_before = master.sources[&secondary]
            .synchronizer
            .as_ref()
            .unwrap()
            .telemetry();

        assert!(matches!(
            master.render_frame_audio(&frame_result_with_mix(1, source_id, Some(secondary), 1, 2,)),
            Err(NativeMasterError::SinkRejected)
        ));
        assert_eq!(master.expected_next_frame(), 1);
        assert_eq!(master.ready_frame, ready);
        assert_eq!(master.collected_audio().next(), Some(&first));
        assert_eq!(
            master.sources[&source_id]
                .synchronizer
                .as_ref()
                .unwrap()
                .telemetry(),
            source_before
        );
        assert_eq!(
            master.sources[&secondary]
                .synchronizer
                .as_ref()
                .unwrap()
                .telemetry(),
            secondary_before
        );
        assert_eq!(
            master.mixer.current_linear_gain(source_id),
            gain_after_first
        );
        assert!(matches!(
            master.render_frame_audio(&frame_result_with_mix(1, source_id, Some(secondary), 1, 2,)),
            Err(NativeMasterError::Failed)
        ));
        assert_eq!(master.expected_next_frame(), 1);
        assert_eq!(master.ready_frame, ready);
        assert_eq!(master.collected_audio().next(), Some(&first));
        assert_eq!(
            master.mixer.current_linear_gain(source_id),
            gain_after_first
        );
    }

    #[test]
    fn restored_master_cursor_services_absolute_frame_without_allocator_replay() {
        let source_id = input(1);
        let mut master = silent_test_master(source_id, 1);
        master.expected_next_frame = 123;

        assert!(master.service_next_frame().unwrap());
        master
            .render_frame(&frame_result(123, source_id, None))
            .unwrap();

        assert_eq!(master.expected_next_frame(), 124);
        let output = master.collected_audio().next().unwrap();
        assert_eq!(output.timing().sequence().get(), 123);
        assert_eq!(
            output.timing().original_timestamp().timestamp().ticks(),
            236_160
        );
        assert_eq!(output.sample_count(), 1_920);
    }

    #[test]
    fn decoder_and_worker_messages_are_send() {
        fn assert_send<T: Send>() {}

        assert_send::<LocalVideoDecoder>();
        assert_send::<LocalAudioDecoder>();
        assert_send::<NativeDecodeRequest>();
        assert_send::<NativeDecodeResult>();
        assert_send::<NativeAudioDecodeRequest>();
        assert_send::<NativeAudioDecodeResult>();
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires a native macOS Metal adapter"]
    fn native_metal_program_readback_is_tightly_packed_and_reusable() {
        let runtime = NativeMediaRuntime::new_blocking([NativeBackend::Metal]).unwrap();
        let mut owner = runtime
            .create_program_readback_blocking(
                NonZeroU32::new(3).unwrap(),
                NonZeroU32::new(2).unwrap(),
            )
            .unwrap();
        assert_eq!((owner.width(), owner.height()), (3, 2));

        let source = retained_frame(ClockDomainId::new(NonZeroU128::new(7).unwrap()), 0, 0)
            .with_metadata(VideoFrameMetadata::new(
                ColorMetadata {
                    primaries: ColorPrimaries::Bt709,
                    transfer: TransferFunction::Srgb,
                    matrix: MatrixCoefficients::Identity,
                    range: SignalRange::Full,
                    chroma_location: ChromaLocation::Center,
                },
                Some(AlphaMode::Straight),
            ))
            .unwrap();
        let working = block_on(runtime.normalizer.normalize(runtime.context(), &source)).unwrap();
        let first = runtime
            .readback_program_blocking(&mut owner, working.texture())
            .unwrap();
        let second = runtime
            .readback_program_blocking(&mut owner, working.texture())
            .unwrap();

        assert_eq!((first.width, first.height, first.stride), (3, 2, 12));
        assert_eq!(first.rgba.len(), 24);
        assert!(
            first
                .rgba
                .chunks_exact(4)
                .all(|pixel| pixel == [0, 0, 0, 255])
        );
        assert_eq!(second, first);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires a native macOS Metal adapter"]
    fn native_metal_project_stinger_changes_base_at_the_configured_cut() {
        let runtime = NativeMediaRuntime::new_blocking([NativeBackend::Metal]).unwrap();
        let clock_domain = ClockDomainId::new(NonZeroU128::new(77).unwrap());
        let program = input(1);
        let preview = input(2);
        let media = input(3);
        let playback = runtime
            .preflight_resolved_source_playback_mixed_blocking(
                None,
                [
                    NativeResolvedSource::RetainedFrame {
                        input: program,
                        frame: colored_retained_frame(clock_domain, [255, 0, 0, 255]),
                    },
                    NativeResolvedSource::RetainedFrame {
                        input: preview,
                        frame: colored_retained_frame(clock_domain, [0, 0, 255, 255]),
                    },
                    NativeResolvedSource::RetainedFrame {
                        input: media,
                        frame: colored_retained_frame(clock_domain, [0, 255, 0, 128]),
                    },
                ],
                clock_domain,
                StreamSelector::Best,
                NativeSourceLimits::default(),
            )
            .unwrap();
        let mut project = native_plan_project(1, 1);
        for source in [program, preview, media] {
            add_leaf(&mut project, source);
        }
        project.add_stinger(StingerConfig::new(
            StingerSlotNumber::new(1).unwrap(),
            media,
            true,
            1,
            fm_model::StingerAudioPolicy::Muted,
            fm_model::StingerMissingMediaFallback::Cut,
        ));
        let project = NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();
        let slot = fm_switcher::StingerSlotId::new(1).unwrap();
        let render = |frame_index| {
            let frame = frame_result_with_transition_interval(
                0,
                program,
                Some(preview),
                Some(SwitcherTransitionKind::Stinger(slot)),
                frame_index,
                2,
                frame_index,
                frame_index + 1,
            );
            let output = runtime
                .render_project_frame_result_blocking(playback.registry(), &project, &frame)
                .unwrap();
            block_on(runtime.diagnostic_readback(&output)).unwrap()
        };
        let before = rgba16f_components(&render(0).bytes);
        let at_cut = rgba16f_components(&render(1).bytes);
        assert!(
            before[0] > before[2],
            "Program red must be the initial base"
        );
        assert!(at_cut[2] > at_cut[0], "Preview blue must be the cut base");
        assert!(before[1] > 0.0 && at_cut[1] > 0.0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires FFmpeg, ffprobe, and a native macOS Metal adapter"]
    #[allow(clippy::too_many_lines)]
    fn native_metal_local_stinger_pages_restarts_and_preserves_normal_playback() {
        let directory = tempfile::tempdir().unwrap();
        let path = create_local_stinger_fixture(directory.path());
        let adapter = Adapter::new(fm_codec_ffmpeg::Config {
            allowed_root: Some(directory.path().to_owned()),
            ..fm_codec_ffmpeg::Config::default()
        })
        .unwrap();
        let runtime = NativeMediaRuntime::new_blocking([NativeBackend::Metal]).unwrap();
        let clock_domain = ClockDomainId::new(NonZeroU128::new(78).unwrap());
        let program = input(1);
        let preview = input(2);
        let media = input(3);
        let playback = runtime
            .preflight_resolved_source_playback_mixed_blocking(
                Some(&adapter),
                [
                    NativeResolvedSource::RetainedFrame {
                        input: program,
                        frame: solid_retained_frame(clock_domain, [255, 0, 0, 255], 2, 2),
                    },
                    NativeResolvedSource::RetainedFrame {
                        input: preview,
                        frame: solid_retained_frame(clock_domain, [0, 0, 255, 255], 2, 2),
                    },
                    NativeResolvedSource::LocalVideo {
                        input: media,
                        path: path.clone(),
                    },
                ],
                clock_domain,
                StreamSelector::Best,
                NativeSourceLimits::default(),
            )
            .unwrap();
        let mut stingers = runtime
            .preflight_resolved_source_playback_mixed_blocking(
                Some(&adapter),
                [NativeResolvedSource::LocalVideo { input: media, path }],
                clock_domain,
                StreamSelector::Best,
                NativeSourceLimits::default(),
            )
            .unwrap();
        stingers.enable_stinger_source(media).unwrap();
        assert_eq!(playback.registry.sources[&media].frames.len(), 8);
        assert_eq!(stingers.registry.sources[&media].frames.len(), 8);
        assert!(!stingers.registry.sources[&media].end_of_stream);
        let normal_offsets = playback.registry.sources[&media].offsets_ns.clone();

        let mut project = native_plan_project(2, 2);
        for source in [program, preview, media] {
            add_leaf(&mut project, source);
        }
        project.add_stinger(StingerConfig::new(
            StingerSlotNumber::new(1).unwrap(),
            media,
            true,
            6,
            fm_model::StingerAudioPolicy::Muted,
            fm_model::StingerMissingMediaFallback::Cut,
        ));
        let project = NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();
        let slot = fm_switcher::StingerSlotId::new(1).unwrap();
        let render = |frame_index, stingers: &NativeSourcePlayback| {
            let frame = frame_result_with_transition_interval(
                0,
                program,
                Some(preview),
                Some(SwitcherTransitionKind::Stinger(slot)),
                frame_index,
                12,
                frame_index,
                frame_index + 1,
            );
            runtime
                .render_project_frame_result_with_stingers_blocking(
                    playback.registry(),
                    stingers.registry(),
                    &project,
                    &frame,
                )
                .map(|output| block_on(runtime.diagnostic_readback(&output)).unwrap())
                .unwrap()
        };
        let mut rendered = Vec::new();
        for frame_index in 0..12 {
            let deadline =
                stinger_frame_deadline(FrameRate::new(30, 1).unwrap(), frame_index).unwrap();
            for _ in 0..10_000 {
                if runtime
                    .service_source_playback_for_input_blocking(&mut stingers, media, deadline)
                    .unwrap()
                {
                    break;
                }
                thread::sleep(std::time::Duration::from_millis(1));
            }
            assert!(
                stingers.registry.sources[&media].covers_deadline(deadline),
                "Stinger ring did not cover clip frame {frame_index}: offsets={:?}, eos={}, in_flight={:?}",
                stingers.registry.sources[&media].offsets_ns,
                stingers.registry.sources[&media].end_of_stream,
                stingers.registry.sources[&media].in_flight,
            );
            rendered.push(render(frame_index, &stingers));
        }
        assert!(stingers.registry.sources[&media].frames.len() <= 8);
        assert_eq!(playback.registry.sources[&media].offsets_ns, normal_offsets);

        let restart_deadline = stinger_frame_deadline(FrameRate::new(30, 1).unwrap(), 0).unwrap();
        for _ in 0..10_000 {
            if runtime
                .service_source_playback_for_input_blocking(&mut stingers, media, restart_deadline)
                .unwrap()
            {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(1));
        }
        let restarted = render(0, &stingers);
        let first = &rendered[0];
        let middle = &rendered[5];
        let last = &rendered[11];
        let first_components = rgba16f_components(&first.bytes);
        let middle_components = rgba16f_components(&middle.bytes);
        let last_components = rgba16f_components(&last.bytes);
        assert!(first_components[0] > first_components[2]);
        assert!(middle_components[1] > 0.0 && middle_components[2] > 0.0);
        assert!(last_components[0] > last_components[2] && last_components[1] > last_components[2]);
        assert_eq!(restarted.bytes, first.bytes);
    }
}
