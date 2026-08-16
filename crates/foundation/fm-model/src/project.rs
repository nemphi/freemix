use fm_types::{
    AudioFormat, BusId, FrameRate, InputId, InputOrderError, OutputId, ProjectId, RenameInputError,
    SceneId, VideoFormat, validate_input_name, validate_input_order,
};

use crate::{ValidationError, validation::validate_project};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoveInputError {
    UnknownInput(InputId),
    DomainReference(InputId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplaceInputError {
    UnknownInput(InputId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddSceneLayerError {
    UnknownScene(SceneId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetSceneBackgroundError {
    UnknownScene(SceneId),
    NotPremultiplied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SceneLayerError {
    UnknownScene(SceneId),
    LayerIndexOutOfRange {
        scene: SceneId,
        index: usize,
        length: usize,
    },
}

impl std::fmt::Display for SceneLayerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownScene(scene) => write!(formatter, "unknown scene {scene}"),
            Self::LayerIndexOutOfRange {
                scene,
                index,
                length,
            } => write!(
                formatter,
                "layer index {index} out of range for scene {scene} with {length} layers"
            ),
        }
    }
}

impl std::error::Error for SceneLayerError {}

impl std::fmt::Display for AddSceneLayerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownScene(scene) => write!(formatter, "unknown scene {scene}"),
        }
    }
}

impl std::error::Error for AddSceneLayerError {}

impl std::fmt::Display for SetSceneBackgroundError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownScene(scene) => write!(formatter, "unknown scene {scene}"),
            Self::NotPremultiplied => formatter.write_str("scene background must be premultiplied"),
        }
    }
}

impl std::error::Error for SetSceneBackgroundError {}

impl std::fmt::Display for ReplaceInputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownInput(input) => write!(formatter, "unknown input {input}"),
        }
    }
}

impl std::error::Error for ReplaceInputError {}

impl std::fmt::Display for RemoveInputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownInput(input) => write!(formatter, "unknown input {input}"),
            Self::DomainReference(input) => {
                write!(formatter, "input {input} has a domain reference")
            }
        }
    }
}

impl std::error::Error for RemoveInputError {}

pub const CURRENT_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(17);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SchemaVersion(u32);

impl SchemaVersion {
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Project {
    schema_version: SchemaVersion,
    id: ProjectId,
    name: String,
    settings: ProjectSettings,
    inputs: Vec<Input>,
    input_audio_strips: Vec<InputAudioStrip>,
    scenes: Vec<Scene>,
    audio_buses: Vec<AudioBus>,
    outputs: Vec<Output>,
    main_mix: Option<MainMix>,
    stingers: Vec<StingerConfig>,
    restart_policy: RestartPolicy,
}

impl Project {
    #[must_use]
    pub fn new(id: ProjectId, name: impl Into<String>, settings: ProjectSettings) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            id,
            name: name.into(),
            settings,
            inputs: Vec::new(),
            input_audio_strips: Vec::new(),
            scenes: Vec::new(),
            audio_buses: Vec::new(),
            outputs: Vec::new(),
            main_mix: None,
            stingers: Vec::new(),
            restart_policy: RestartPolicy::default(),
        }
    }

    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    #[must_use]
    pub const fn id(&self) -> ProjectId {
        self.id
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn settings(&self) -> &ProjectSettings {
        &self.settings
    }

    #[must_use]
    pub fn inputs(&self) -> &[Input] {
        &self.inputs
    }

    #[must_use]
    pub fn input_audio_strips(&self) -> &[InputAudioStrip] {
        &self.input_audio_strips
    }

    #[must_use]
    pub fn input_audio_strip(&self, input: InputId) -> Option<InputAudioStripState> {
        self.input_audio_strips
            .iter()
            .find(|strip| strip.input == input)
            .map(|strip| strip.state)
    }

    #[must_use]
    pub fn scenes(&self) -> &[Scene] {
        &self.scenes
    }

    #[must_use]
    pub fn audio_buses(&self) -> &[AudioBus] {
        &self.audio_buses
    }

    #[must_use]
    pub fn outputs(&self) -> &[Output] {
        &self.outputs
    }

    #[must_use]
    pub const fn main_mix(&self) -> Option<MainMix> {
        self.main_mix
    }

    #[must_use]
    pub fn stingers(&self) -> &[StingerConfig] {
        &self.stingers
    }

    #[must_use]
    pub const fn restart_policy(&self) -> RestartPolicy {
        self.restart_policy
    }

    pub fn add_input(&mut self, input: Input) {
        self.input_audio_strips.push(InputAudioStrip {
            input: input.id,
            state: InputAudioStripState::default(),
        });
        self.inputs.push(input);
    }

    pub fn remove_input(&mut self, input: InputId) -> Result<(), RemoveInputError> {
        if !self.inputs.iter().any(|candidate| candidate.id == input) {
            return Err(RemoveInputError::UnknownInput(input));
        }
        let referenced = self.main_mix.is_some_and(|mix| {
            mix.desired_program == input || mix.desired_preview == input
        }) || self.stingers.iter().any(|stinger| stinger.media_input == input)
            || self.scenes.iter().any(|scene| {
                scene.layers.iter().any(|layer| layer.source == SourceRef::Input(input))
            })
            || self.inputs.iter().any(|candidate| {
                matches!(candidate.kind, InputKind::Scene { audio_source: Some(source), .. } if source == input)
            });
        if referenced {
            return Err(RemoveInputError::DomainReference(input));
        }
        self.inputs.retain(|candidate| candidate.id != input);
        self.input_audio_strips.retain(|strip| strip.input != input);
        Ok(())
    }

    /// Renames one input while preserving the exact supplied text.
    ///
    /// # Errors
    ///
    /// Returns [`RenameInputError`] when the input is unknown or the name is
    /// blank, too long, or already used by another input.
    pub fn rename_input(&mut self, input: InputId, name: String) -> Result<(), RenameInputError> {
        let index = self
            .inputs
            .iter()
            .position(|candidate| candidate.id == input)
            .ok_or(RenameInputError::UnknownInput(input))?;
        validate_input_name(&name)?;
        if self
            .inputs
            .iter()
            .enumerate()
            .any(|(candidate_index, candidate)| candidate_index != index && candidate.name == name)
        {
            return Err(RenameInputError::DuplicateName);
        }
        self.inputs[index].name = name;
        Ok(())
    }

    pub fn replace_input_source(
        &mut self,
        input: InputId,
        kind: InputKind,
        required_capabilities: Vec<String>,
    ) -> Result<(), ReplaceInputError> {
        let candidate = self
            .inputs
            .iter_mut()
            .find(|candidate| candidate.id == input)
            .ok_or(ReplaceInputError::UnknownInput(input))?;
        candidate.kind = kind;
        candidate.required_capabilities = required_capabilities;
        Ok(())
    }

    pub fn reorder_inputs(&mut self, inputs: Vec<InputId>) -> Result<(), InputOrderError> {
        let current = self.inputs.iter().map(|input| input.id).collect::<Vec<_>>();
        validate_input_order(&current, &inputs)?;
        let reordered = inputs
            .iter()
            .map(|input| {
                self.inputs
                    .iter()
                    .find(|candidate| candidate.id == *input)
                    .expect("validated input order contains only project inputs")
                    .clone()
            })
            .collect();
        self.inputs = reordered;
        Ok(())
    }

    /// Replaces the persisted audio strip for an existing input.
    ///
    /// Returns `false` when `input` is not part of this project.
    pub fn set_input_audio_strip(&mut self, input: InputId, state: InputAudioStripState) -> bool {
        let Some(strip) = self
            .input_audio_strips
            .iter_mut()
            .find(|strip| strip.input == input)
        else {
            return false;
        };
        strip.state = state;
        true
    }

    pub fn add_scene(&mut self, scene: Scene) {
        self.scenes.push(scene);
    }

    pub fn set_scene_background(
        &mut self,
        scene: SceneId,
        background: Rgba8,
    ) -> Result<(), SetSceneBackgroundError> {
        if !background.is_premultiplied() {
            return Err(SetSceneBackgroundError::NotPremultiplied);
        }
        self.scenes
            .iter_mut()
            .find(|candidate| candidate.id == scene)
            .ok_or(SetSceneBackgroundError::UnknownScene(scene))?
            .background = background;
        Ok(())
    }

    pub fn add_layer_to_scene(
        &mut self,
        scene: SceneId,
        layer: Layer,
    ) -> Result<(), AddSceneLayerError> {
        self.scenes
            .iter_mut()
            .find(|candidate| candidate.id == scene)
            .ok_or(AddSceneLayerError::UnknownScene(scene))?
            .layers
            .push(layer);
        Ok(())
    }

    pub fn remove_layer_from_scene(
        &mut self,
        scene: SceneId,
        index: usize,
    ) -> Result<Layer, SceneLayerError> {
        let target = self
            .scenes
            .iter_mut()
            .find(|candidate| candidate.id == scene)
            .ok_or(SceneLayerError::UnknownScene(scene))?;
        if index >= target.layers.len() {
            return Err(SceneLayerError::LayerIndexOutOfRange {
                scene,
                index,
                length: target.layers.len(),
            });
        }
        Ok(target.layers.remove(index))
    }

    pub fn set_scene_layer_z_order(
        &mut self,
        scene: SceneId,
        index: usize,
        z_order: i32,
    ) -> Result<(), SceneLayerError> {
        let target = self
            .scenes
            .iter_mut()
            .find(|candidate| candidate.id == scene)
            .ok_or(SceneLayerError::UnknownScene(scene))?;
        let length = target.layers.len();
        let layer = target
            .layers
            .get_mut(index)
            .ok_or(SceneLayerError::LayerIndexOutOfRange {
                scene,
                index,
                length,
            })?;
        layer.z_order = z_order;
        Ok(())
    }

    pub fn set_scene_layer_appearance(
        &mut self,
        scene: SceneId,
        index: usize,
        enabled: bool,
        opacity: u8,
    ) -> Result<(), SceneLayerError> {
        let scene_index = self
            .scenes
            .iter()
            .position(|candidate| candidate.id == scene)
            .ok_or(SceneLayerError::UnknownScene(scene))?;
        let length = self.scenes[scene_index].layers.len();
        if index >= length {
            return Err(SceneLayerError::LayerIndexOutOfRange {
                scene,
                index,
                length,
            });
        }
        let layer = &mut self.scenes[scene_index].layers[index];
        layer.enabled = enabled;
        layer.opacity = opacity;
        Ok(())
    }

    pub fn set_scene_layer_geometry(
        &mut self,
        scene: SceneId,
        index: usize,
        geometry: LayerGeometry,
    ) -> Result<(), SceneLayerError> {
        let target = self
            .scenes
            .iter_mut()
            .find(|candidate| candidate.id == scene)
            .ok_or(SceneLayerError::UnknownScene(scene))?;
        let length = target.layers.len();
        let layer = target
            .layers
            .get_mut(index)
            .ok_or(SceneLayerError::LayerIndexOutOfRange {
                scene,
                index,
                length,
            })?;
        layer.geometry = geometry;
        Ok(())
    }

    pub fn set_scene_layer_crop(
        &mut self,
        scene: SceneId,
        index: usize,
        crop: Option<CropRect>,
    ) -> Result<(), SceneLayerError> {
        let target = self
            .scenes
            .iter_mut()
            .find(|candidate| candidate.id == scene)
            .ok_or(SceneLayerError::UnknownScene(scene))?;
        let length = target.layers.len();
        let layer = target
            .layers
            .get_mut(index)
            .ok_or(SceneLayerError::LayerIndexOutOfRange {
                scene,
                index,
                length,
            })?;
        layer.crop = crop;
        Ok(())
    }

    pub fn set_scene_layer_mask(
        &mut self,
        scene: SceneId,
        index: usize,
        mask: Option<RectMask>,
    ) -> Result<(), SceneLayerError> {
        let target = self
            .scenes
            .iter_mut()
            .find(|candidate| candidate.id == scene)
            .ok_or(SceneLayerError::UnknownScene(scene))?;
        let length = target.layers.len();
        let layer = target
            .layers
            .get_mut(index)
            .ok_or(SceneLayerError::LayerIndexOutOfRange {
                scene,
                index,
                length,
            })?;
        layer.mask = mask;
        Ok(())
    }

    pub fn add_audio_bus(&mut self, bus: AudioBus) {
        self.audio_buses.push(bus);
    }

    pub fn add_output(&mut self, output: Output) {
        self.outputs.push(output);
    }

    pub fn set_main_mix(&mut self, main_mix: MainMix) {
        self.main_mix = Some(main_mix);
    }

    pub fn add_stinger(&mut self, stinger: StingerConfig) {
        self.stingers.push(stinger);
    }

    /// Inserts or replaces one Stinger slot while preserving slot order.
    pub fn set_stinger(&mut self, stinger: StingerConfig) {
        if let Some(configured) = self
            .stingers
            .iter_mut()
            .find(|configured| configured.slot == stinger.slot)
        {
            *configured = stinger;
        } else {
            self.stingers.push(stinger);
        }
    }

    /// Removes one configured Stinger slot.
    pub fn remove_stinger(&mut self, slot: StingerSlotNumber) -> Option<StingerConfig> {
        let index = self
            .stingers
            .iter()
            .position(|configured| configured.slot == slot)?;
        Some(self.stingers.remove(index))
    }

    #[must_use]
    pub fn with_main_mix(mut self, main_mix: MainMix) -> Self {
        self.set_main_mix(main_mix);
        self
    }

    pub fn set_restart_policy(&mut self, restart_policy: RestartPolicy) {
        self.restart_policy = restart_policy;
    }

    #[must_use]
    pub const fn with_restart_policy(mut self, restart_policy: RestartPolicy) -> Self {
        self.restart_policy = restart_policy;
        self
    }

    /// Validates names, references, capability keys, and graph cycles.
    ///
    /// # Errors
    ///
    /// Returns every discovered [`ValidationError`] so a caller can present one
    /// complete preflight report.
    pub fn validate(&self) -> Result<(), Vec<ValidationError>> {
        let errors = validate_project(self);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectSettings {
    pub frame_rate: FrameRate,
    pub video: VideoFormat,
    pub audio: AudioFormat,
}

impl ProjectSettings {
    pub const MIN_FRAME_RATE_FPS: u32 = 1;
    pub const MAX_FRAME_RATE_FPS: u32 = 240;
    pub const MAX_VIDEO_WIDTH: u32 = 8_192;
    pub const MAX_VIDEO_HEIGHT: u32 = 8_192;
    pub const MIN_AUDIO_SAMPLE_RATE_HZ: u32 = 8_000;
    pub const MAX_AUDIO_SAMPLE_RATE_HZ: u32 = 384_000;
    pub const MAX_AUDIO_CHANNELS: usize = 8;

    #[must_use]
    pub fn new(frame_rate: FrameRate, output: OutputFormat) -> Self {
        Self {
            frame_rate,
            video: output.video,
            audio: output.audio,
        }
    }

    #[must_use]
    pub fn output_format(&self) -> OutputFormat {
        OutputFormat {
            video: self.video,
            audio: self.audio.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputFormat {
    pub video: VideoFormat,
    pub audio: AudioFormat,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Input {
    pub id: InputId,
    pub name: String,
    pub kind: InputKind,
    pub required_capabilities: Vec<String>,
}

/// Exact persisted gain for an input strip, in one-thousandth of a decibel.
///
/// The integer representation is stable across JSON round trips and excludes
/// non-finite floating-point values by construction.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct InputGainMilliDb(i32);

impl InputGainMilliDb {
    pub const MIN: i32 = -96_000;
    pub const MAX: i32 = 24_000;
    pub const UNITY: Self = Self(0);

    #[must_use]
    pub const fn new(value: i32) -> Option<Self> {
        if value >= Self::MIN && value <= Self::MAX {
            Some(Self(value))
        } else {
            None
        }
    }

    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }
}

/// Exact persisted stereo balance in one-hundredth of a percent.
///
/// `-10000` is full left, zero is centered, and `10000` is full right.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct InputBalanceBasisPoints(i32);

impl InputBalanceBasisPoints {
    pub const MIN: i32 = -10_000;
    pub const MAX: i32 = 10_000;
    pub const CENTER: Self = Self(0);

    #[must_use]
    pub const fn new(value: i32) -> Option<Self> {
        if value >= Self::MIN && value <= Self::MAX {
            Some(Self(value))
        } else {
            None
        }
    }

    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }
}

/// Exact nonnegative per-input delay in 48 kHz samples.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputDelaySamples(u32);

impl InputDelaySamples {
    pub const MAX: u32 = 48_000;
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn new(value: u32) -> Option<Self> {
        if value <= Self::MAX {
            Some(Self(value))
        } else {
            None
        }
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Persisted user controls for one input's Master mixer strip.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputAudioStripState {
    pub gain: InputGainMilliDb,
    pub balance: InputBalanceBasisPoints,
    pub delay_samples: InputDelaySamples,
    pub muted: bool,
    pub soloed: bool,
    pub follow_video: bool,
}

impl Default for InputAudioStripState {
    fn default() -> Self {
        Self {
            gain: InputGainMilliDb::UNITY,
            balance: InputBalanceBasisPoints::CENTER,
            delay_samples: InputDelaySamples::ZERO,
            muted: false,
            soloed: false,
            follow_video: true,
        }
    }
}

/// Input identity paired with its persisted strip state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputAudioStrip {
    pub input: InputId,
    pub state: InputAudioStripState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputKind {
    Color,
    Media {
        asset_uri: String,
    },
    /// Exact adapter-qualified source identity from platform discovery.
    Device {
        stable_key: String,
    },
    Network {
        endpoint: String,
    },
    Scene {
        scene_id: SceneId,
        audio_source: Option<InputId>,
    },
    Simulated(SimulatedInput),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SimulatedInput {
    pub video: SimulatedVideo,
    pub audio: SimulatedAudio,
}

impl SimulatedInput {
    #[must_use]
    pub const fn new(video: SimulatedVideo, audio: SimulatedAudio) -> Self {
        Self { video, audio }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SimulatedVideo {
    Solid(SolidColor),
    Bars,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SolidColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: u8,
}

impl SolidColor {
    #[must_use]
    pub const fn new(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        Self {
            red,
            green,
            blue,
            alpha,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SimulatedAudio {
    Silence,
    Sine { frequency_hz: u32 },
}

impl SimulatedAudio {
    pub const MIN_SINE_FREQUENCY_HZ: u32 = 1;
    pub const MAX_SINE_FREQUENCY_HZ: u32 = 20_000;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MainMix {
    pub desired_program: InputId,
    pub desired_preview: InputId,
}

/// One-based operator-facing Stinger slot number.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StingerSlotNumber(u8);

impl StingerSlotNumber {
    pub const COUNT: usize = 8;

    #[must_use]
    pub const fn new(number: u8) -> Option<Self> {
        if number >= 1 && number <= 8 {
            Some(Self(number))
        } else {
            None
        }
    }

    #[must_use]
    pub const fn number(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StingerAudioPolicy {
    Muted,
    StingerOnly,
    MixWithProgram,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StingerMissingMediaFallback {
    Cut,
    Fade,
    KeepProgram,
}

/// Durable configuration for one of the eight Stinger slots.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StingerConfig {
    pub slot: StingerSlotNumber,
    pub media_input: InputId,
    pub preload: bool,
    pub cut_point_frames: u32,
    pub audio_policy: StingerAudioPolicy,
    pub missing_media_fallback: StingerMissingMediaFallback,
}

impl StingerConfig {
    #[must_use]
    pub const fn new(
        slot: StingerSlotNumber,
        media_input: InputId,
        preload: bool,
        cut_point_frames: u32,
        audio_policy: StingerAudioPolicy,
        missing_media_fallback: StingerMissingMediaFallback,
    ) -> Self {
        Self {
            slot,
            media_input,
            preload,
            cut_point_frames,
            audio_policy,
            missing_media_fallback,
        }
    }
}

impl MainMix {
    #[must_use]
    pub const fn new(desired_program: InputId, desired_preview: InputId) -> Self {
        Self {
            desired_program,
            desired_preview,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RestartPolicy {
    #[default]
    Never,
    OnFailure {
        max_attempts: u8,
    },
    Always,
}

impl RestartPolicy {
    pub const MAX_RESTART_ATTEMPTS: u8 = 100;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Scene {
    pub id: SceneId,
    pub name: String,
    pub background: Rgba8,
    pub layers: Vec<Layer>,
}

impl Scene {
    /// Maximum layers accepted by the compositor for one execution plan.
    ///
    /// Persisted scenes are intentionally not bounded by this value: schema v3
    /// allowed larger scene lists, so storage must preserve them.
    /// Rendering code must enforce this limit when realizing a scene.
    pub const MAX_RENDERED_LAYERS: usize = 64;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Layer {
    pub name: String,
    pub source: SourceRef,
    pub enabled: bool,
    pub geometry: LayerGeometry,
    pub crop: Option<CropRect>,
    pub mask: Option<RectMask>,
    pub opacity: u8,
    pub z_order: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LayerGeometry {
    pub translation_x: i32,
    pub translation_y: i32,
    pub width: u32,
    pub height: u32,
    pub rotation: Rotation,
}

impl LayerGeometry {
    #[must_use]
    pub const fn new(
        translation_x: i32,
        translation_y: i32,
        width: u32,
        height: u32,
        rotation: Rotation,
    ) -> Self {
        Self {
            translation_x,
            translation_y,
            width,
            height,
            rotation,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Rotation {
    #[default]
    Deg0,
    Deg90,
    Deg180,
    Deg270,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CropRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl CropRect {
    #[must_use]
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// A hard-edged rectangular mask in half-open post-crop source space.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RectMask {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub invert: bool,
}

impl RectMask {
    #[must_use]
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
            invert: false,
        }
    }

    #[must_use]
    pub const fn inverted(mut self, invert: bool) -> Self {
        self.invert = invert;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rgba8 {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: u8,
}

impl Rgba8 {
    pub const OPAQUE_BLACK: Self = Self::new(0, 0, 0, u8::MAX);

    #[must_use]
    pub const fn new(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        Self {
            red,
            green,
            blue,
            alpha,
        }
    }

    #[must_use]
    pub const fn is_premultiplied(self) -> bool {
        self.red <= self.alpha && self.green <= self.alpha && self.blue <= self.alpha
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceRef {
    Input(InputId),
    Scene(SceneId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioBus {
    pub id: BusId,
    pub name: String,
    pub sends: Vec<BusSend>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BusSend {
    pub destination: BusId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Output {
    pub id: OutputId,
    pub name: String,
    pub video_source: SceneId,
    pub audio_source: BusId,
    pub startup: StartupPolicy,
    pub required_capabilities: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StartupPolicy {
    #[default]
    Stopped,
    ReconcileDesiredState,
}
