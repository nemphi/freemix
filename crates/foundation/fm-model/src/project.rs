use fm_types::{AudioFormat, BusId, FrameRate, InputId, OutputId, ProjectId, SceneId, VideoFormat};

use crate::{ValidationError, validation::validate_project};

pub const CURRENT_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(6);
pub const OLDEST_SUPPORTED_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(2);
pub const SUPPORTED_SCHEMA_VERSIONS: [SchemaVersion; 5] = [
    CURRENT_SCHEMA_VERSION,
    SchemaVersion::new(5),
    SchemaVersion::new(4),
    SchemaVersion::new(3),
    OLDEST_SUPPORTED_SCHEMA_VERSION,
];

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

    #[must_use]
    pub const fn is_supported(self) -> bool {
        self.0 >= OLDEST_SUPPORTED_SCHEMA_VERSION.0 && self.0 <= CURRENT_SCHEMA_VERSION.0
    }

    #[must_use]
    pub const fn requires_migration(self) -> bool {
        self.is_supported() && self.0 != CURRENT_SCHEMA_VERSION.0
    }
}

/// A decoded project representation together with the schema that describes it.
///
/// `T` deliberately has no serialization constraints. Persistence adapters can
/// use an intermediate representation appropriate to their format and migrate
/// it into [`Project`] separately.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationInput<T> {
    schema_version: SchemaVersion,
    representation: T,
}

impl<T> MigrationInput<T> {
    #[must_use]
    pub const fn new(schema_version: SchemaVersion, representation: T) -> Self {
        Self {
            schema_version,
            representation,
        }
    }

    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    #[must_use]
    pub const fn representation(&self) -> &T {
        &self.representation
    }

    #[must_use]
    pub fn into_representation(self) -> T {
        self.representation
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Project {
    schema_version: SchemaVersion,
    id: ProjectId,
    name: String,
    settings: ProjectSettings,
    inputs: Vec<Input>,
    scenes: Vec<Scene>,
    audio_buses: Vec<AudioBus>,
    outputs: Vec<Output>,
    main_mix: Option<MainMix>,
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
            scenes: Vec::new(),
            audio_buses: Vec::new(),
            outputs: Vec::new(),
            main_mix: None,
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
    pub const fn restart_policy(&self) -> RestartPolicy {
        self.restart_policy
    }

    pub fn add_input(&mut self, input: Input) {
        self.inputs.push(input);
    }

    pub fn add_scene(&mut self, scene: Scene) {
        self.scenes.push(scene);
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
    /// allowed larger scene lists, so storage and migration must preserve them.
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
