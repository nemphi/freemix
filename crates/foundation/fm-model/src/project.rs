use fm_types::{
    AudioFormat, BusId, FrameRate, InputId, InputOrderError, OutputId, ProjectId, RenameInputError,
    SceneId, VideoFormat, validate_input_name, validate_input_order,
};

use crate::{EntityRef, ValidationError, ValidationErrorKind, validation::validate_project};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoveInputError {
    UnknownInput(InputId),
    DomainReference(InputId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoveSceneError {
    UnknownScene(SceneId),
    InputReference { input: InputId, scene: SceneId },
    LayerReference { owner: SceneId, source: SceneId },
    OutputReference { output: OutputId, scene: SceneId },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenameSceneError {
    UnknownScene(SceneId),
    EmptyName,
    DuplicateName,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DuplicateSceneInputError {
    UnknownSourceScene(SceneId),
    DuplicateSceneId(SceneId),
    DuplicateInputId(InputId),
    EmptySceneName,
    DuplicateSceneName,
    EmptyInputName,
    InputNameTooLong,
    DuplicateInputName,
    InvalidProject(Vec<ValidationError>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AddSceneInputError {
    DuplicateSceneId(SceneId),
    DuplicateInputId(InputId),
    EmptySceneName,
    DuplicateSceneName,
    EmptyInputName,
    InputNameTooLong,
    DuplicateInputName,
    InvalidProject(Vec<ValidationError>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NewSceneInputError {
    DuplicateSceneId(SceneId),
    DuplicateInputId(InputId),
    EmptySceneName,
    DuplicateSceneName,
    EmptyInputName,
    InputNameTooLong,
    DuplicateInputName,
}

impl Project {
    fn validate_new_scene_input(
        &self,
        scene: SceneId,
        scene_name: &str,
        input: InputId,
        input_name: &str,
    ) -> Result<(), NewSceneInputError> {
        if self.scenes.iter().any(|candidate| candidate.id == scene) {
            return Err(NewSceneInputError::DuplicateSceneId(scene));
        }
        if self.inputs.iter().any(|candidate| candidate.id == input) {
            return Err(NewSceneInputError::DuplicateInputId(input));
        }
        if scene_name.trim().is_empty() {
            return Err(NewSceneInputError::EmptySceneName);
        }
        if self
            .scenes
            .iter()
            .any(|candidate| candidate.name.eq_ignore_ascii_case(scene_name))
        {
            return Err(NewSceneInputError::DuplicateSceneName);
        }
        if input_name.trim().is_empty() {
            return Err(NewSceneInputError::EmptyInputName);
        }
        if input_name.len() > fm_types::MAX_INPUT_NAME_BYTES {
            return Err(NewSceneInputError::InputNameTooLong);
        }
        if self
            .inputs
            .iter()
            .any(|candidate| candidate.name == input_name)
        {
            return Err(NewSceneInputError::DuplicateInputName);
        }
        Ok(())
    }
}

macro_rules! map_new_scene_input_error {
    ($error:expr, $target:ident) => {
        match $error {
            NewSceneInputError::DuplicateSceneId(value) => $target::DuplicateSceneId(value),
            NewSceneInputError::DuplicateInputId(value) => $target::DuplicateInputId(value),
            NewSceneInputError::EmptySceneName => $target::EmptySceneName,
            NewSceneInputError::DuplicateSceneName => $target::DuplicateSceneName,
            NewSceneInputError::EmptyInputName => $target::EmptyInputName,
            NewSceneInputError::InputNameTooLong => $target::InputNameTooLong,
            NewSceneInputError::DuplicateInputName => $target::DuplicateInputName,
        }
    };
}

impl From<NewSceneInputError> for AddSceneInputError {
    fn from(error: NewSceneInputError) -> Self {
        map_new_scene_input_error!(error, AddSceneInputError)
    }
}

impl From<NewSceneInputError> for DuplicateSceneInputError {
    fn from(error: NewSceneInputError) -> Self {
        map_new_scene_input_error!(error, DuplicateSceneInputError)
    }
}

impl std::fmt::Display for DuplicateSceneInputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSourceScene(scene) => write!(formatter, "unknown source scene {scene}"),
            Self::DuplicateSceneId(scene) => write!(formatter, "scene {scene} already exists"),
            Self::DuplicateInputId(input) => write!(formatter, "input {input} already exists"),
            Self::EmptySceneName => formatter.write_str("scene name must not be empty"),
            Self::DuplicateSceneName => formatter.write_str("scene name is already in use"),
            Self::EmptyInputName => formatter.write_str("input name must not be empty"),
            Self::InputNameTooLong => write!(
                formatter,
                "input name must not exceed {} bytes",
                fm_types::MAX_INPUT_NAME_BYTES
            ),
            Self::DuplicateInputName => formatter.write_str("input name is already in use"),
            Self::InvalidProject(errors) => write!(formatter, "invalid project ({})", errors.len()),
        }
    }
}

impl std::error::Error for DuplicateSceneInputError {}

impl std::fmt::Display for AddSceneInputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateSceneId(scene) => write!(formatter, "scene {scene} already exists"),
            Self::DuplicateInputId(input) => write!(formatter, "input {input} already exists"),
            Self::EmptySceneName => formatter.write_str("scene name must not be empty"),
            Self::DuplicateSceneName => formatter.write_str("scene name is already in use"),
            Self::EmptyInputName => formatter.write_str("input name must not be empty"),
            Self::InputNameTooLong => write!(
                formatter,
                "input name must not exceed {} bytes",
                fm_types::MAX_INPUT_NAME_BYTES
            ),
            Self::DuplicateInputName => formatter.write_str("input name is already in use"),
            Self::InvalidProject(errors) => write!(formatter, "invalid project ({})", errors.len()),
        }
    }
}

impl std::error::Error for AddSceneInputError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplaceInputError {
    UnknownInput(InputId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelinkMediaInputError {
    UnknownInput(InputId),
    NotMediaInput(InputId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetOutputRouteError {
    UnknownOutput(OutputId),
    UnknownScene(SceneId),
    UnknownBus(BusId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddAudioBusError {
    DuplicateId(BusId),
    DuplicateName,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddOutputError {
    DuplicateId(OutputId),
    DuplicateName,
    UnknownScene(SceneId),
    UnknownBus(BusId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoveOutputError {
    UnknownOutput(OutputId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoveAudioBusError {
    UnknownBus(BusId),
    OutputReference { bus: BusId, output: OutputId },
    OutgoingSend { bus: BusId, destination: BusId },
    IncomingSend { bus: BusId, source: BusId },
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
    MissingInput(InputId),
    MissingScene(SceneId),
    SourceCycle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SceneInputAudioSourceError {
    MissingTargetInput(InputId),
    NonSceneTargetInput(InputId),
    MissingSourceInput(InputId),
    SourceCycle,
}

impl std::fmt::Display for SceneInputAudioSourceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTargetInput(input) => write!(formatter, "missing target input {input}"),
            Self::NonSceneTargetInput(input) => {
                write!(formatter, "target input {input} is not a scene input")
            }
            Self::MissingSourceInput(input) => write!(formatter, "missing source input {input}"),
            Self::SourceCycle => {
                formatter.write_str("scene input audio source would create a cycle")
            }
        }
    }
}

impl std::error::Error for SceneInputAudioSourceError {}

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
            Self::MissingInput(input) => write!(formatter, "missing source input {input}"),
            Self::MissingScene(scene) => write!(formatter, "missing source scene {scene}"),
            Self::SourceCycle => formatter.write_str("scene layer source would create a cycle"),
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

impl std::fmt::Display for RelinkMediaInputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownInput(input) => write!(formatter, "unknown input {input}"),
            Self::NotMediaInput(input) => write!(formatter, "input {input} is not a media input"),
        }
    }
}

impl std::error::Error for RelinkMediaInputError {}

impl std::fmt::Display for SetOutputRouteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownOutput(output) => write!(formatter, "unknown output {output}"),
            Self::UnknownScene(scene) => write!(formatter, "unknown scene {scene}"),
            Self::UnknownBus(bus) => write!(formatter, "unknown audio bus {bus}"),
        }
    }
}

impl std::error::Error for SetOutputRouteError {}

impl std::fmt::Display for AddAudioBusError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateId(id) => write!(formatter, "duplicate audio bus {id}"),
            Self::DuplicateName => formatter.write_str("duplicate audio bus name"),
        }
    }
}

impl std::error::Error for AddAudioBusError {}

impl std::fmt::Display for AddOutputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateId(id) => write!(formatter, "duplicate output {id}"),
            Self::DuplicateName => formatter.write_str("duplicate output name"),
            Self::UnknownScene(scene) => write!(formatter, "unknown scene {scene}"),
            Self::UnknownBus(bus) => write!(formatter, "unknown audio bus {bus}"),
        }
    }
}

impl std::error::Error for AddOutputError {}

impl std::fmt::Display for RemoveOutputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownOutput(output) => write!(formatter, "unknown output {output}"),
        }
    }
}

impl std::error::Error for RemoveOutputError {}

impl std::fmt::Display for RemoveAudioBusError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownBus(bus) => write!(formatter, "unknown audio bus {bus}"),
            Self::OutputReference { bus, output } => {
                write!(formatter, "audio bus {bus} is used by output {output}")
            }
            Self::OutgoingSend { bus, destination } => {
                write!(
                    formatter,
                    "audio bus {bus} sends to audio bus {destination}"
                )
            }
            Self::IncomingSend { bus, source } => {
                write!(formatter, "audio bus {source} sends to audio bus {bus}")
            }
        }
    }
}

impl std::error::Error for RemoveAudioBusError {}

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

impl std::fmt::Display for RemoveSceneError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownScene(scene) => write!(formatter, "unknown scene {scene}"),
            Self::InputReference { input, scene } => {
                write!(formatter, "input {input} references scene {scene}")
            }
            Self::LayerReference { owner, source } => {
                write!(formatter, "scene {owner} layer references scene {source}")
            }
            Self::OutputReference { output, scene } => {
                write!(formatter, "output {output} references scene {scene}")
            }
        }
    }
}

impl std::error::Error for RemoveSceneError {}

impl std::fmt::Display for RenameSceneError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownScene(scene) => write!(formatter, "scene {scene} does not exist"),
            Self::EmptyName => formatter.write_str("scene name must not be empty"),
            Self::DuplicateName => formatter.write_str("scene name is already in use"),
        }
    }
}

impl std::error::Error for RenameSceneError {}

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

    pub fn relink_media_input(
        &mut self,
        input: InputId,
        asset_uri: String,
    ) -> Result<(), RelinkMediaInputError> {
        let index = self
            .inputs
            .iter()
            .position(|candidate| candidate.id == input)
            .ok_or(RelinkMediaInputError::UnknownInput(input))?;
        if !matches!(self.inputs[index].kind, InputKind::Media { .. }) {
            return Err(RelinkMediaInputError::NotMediaInput(input));
        }
        let InputKind::Media { asset_uri: current } = &mut self.inputs[index].kind else {
            unreachable!("media input kind was validated above");
        };
        *current = asset_uri;
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

    pub fn add_scene_input_checked(
        &mut self,
        scene: SceneId,
        scene_name: String,
        input: InputId,
        input_name: String,
    ) -> Result<(), AddSceneInputError> {
        self.validate_new_scene_input(scene, &scene_name, input, &input_name)
            .map_err(AddSceneInputError::from)?;
        let mut candidate = self.clone();
        candidate.add_scene(Scene {
            id: scene,
            name: scene_name,
            background: Rgba8::OPAQUE_BLACK,
            layers: Vec::new(),
        });
        candidate.add_input(Input {
            id: input,
            name: input_name,
            kind: InputKind::Scene {
                scene_id: scene,
                audio_source: None,
            },
            required_capabilities: Vec::new(),
        });
        candidate
            .validate()
            .map_err(AddSceneInputError::InvalidProject)?;
        *self = candidate;
        Ok(())
    }

    pub fn duplicate_scene_input_checked(
        &mut self,
        source_scene: SceneId,
        new_scene: SceneId,
        scene_name: String,
        new_input: InputId,
        input_name: String,
    ) -> Result<(), DuplicateSceneInputError> {
        let source = self
            .scenes
            .iter()
            .find(|scene| scene.id == source_scene)
            .ok_or(DuplicateSceneInputError::UnknownSourceScene(source_scene))?;
        self.validate_new_scene_input(new_scene, &scene_name, new_input, &input_name)
            .map_err(DuplicateSceneInputError::from)?;
        let mut candidate = self.clone();
        let mut scene = source.clone();
        scene.id = new_scene;
        scene.name = scene_name;
        candidate.scenes.push(scene);
        candidate.add_input(Input {
            id: new_input,
            name: input_name,
            kind: InputKind::Scene {
                scene_id: new_scene,
                audio_source: None,
            },
            required_capabilities: Vec::new(),
        });
        candidate
            .validate()
            .map_err(DuplicateSceneInputError::InvalidProject)?;
        *self = candidate;
        Ok(())
    }

    pub fn rename_scene(&mut self, scene: SceneId, name: String) -> Result<(), RenameSceneError> {
        let index = self
            .scenes
            .iter()
            .position(|candidate| candidate.id == scene)
            .ok_or(RenameSceneError::UnknownScene(scene))?;
        if name.trim().is_empty() {
            return Err(RenameSceneError::EmptyName);
        }
        if self
            .scenes
            .iter()
            .enumerate()
            .any(|(candidate_index, candidate)| {
                candidate_index != index && candidate.name.eq_ignore_ascii_case(&name)
            })
        {
            return Err(RenameSceneError::DuplicateName);
        }
        self.scenes[index].name = name;
        Ok(())
    }

    pub fn remove_scene(&mut self, scene: SceneId) -> Result<(), RemoveSceneError> {
        if !self.scenes.iter().any(|candidate| candidate.id == scene) {
            return Err(RemoveSceneError::UnknownScene(scene));
        }
        if let Some(input) = self.inputs.iter().find_map(|input| {
            matches!(input.kind, InputKind::Scene { scene_id, .. } if scene_id == scene)
                .then_some(input.id)
        }) {
            return Err(RemoveSceneError::InputReference { input, scene });
        }
        if let Some((owner, source)) = self.scenes.iter().find_map(|owner| {
            owner.layers.iter().find_map(|layer| {
                matches!(layer.source, SourceRef::Scene(source) if source == scene)
                    .then_some((owner.id, scene))
            })
        }) {
            return Err(RemoveSceneError::LayerReference { owner, source });
        }
        if let Some(output) = self
            .outputs
            .iter()
            .find(|output| output.video_source == scene)
            .map(|output| output.id)
        {
            return Err(RemoveSceneError::OutputReference { output, scene });
        }
        self.scenes.retain(|candidate| candidate.id != scene);
        Ok(())
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

    pub fn set_scene_layer_source(
        &mut self,
        scene: SceneId,
        index: usize,
        source: SourceRef,
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
        match source {
            SourceRef::Input(input)
                if !self.inputs.iter().any(|candidate| candidate.id == input) =>
            {
                return Err(SceneLayerError::MissingInput(input));
            }
            SourceRef::Scene(source_scene)
                if !self
                    .scenes
                    .iter()
                    .any(|candidate| candidate.id == source_scene) =>
            {
                return Err(SceneLayerError::MissingScene(source_scene));
            }
            SourceRef::Input(_) | SourceRef::Scene(_) => {}
        }
        let mut cycle_probe = self.clone();
        cycle_probe.scenes[scene_index].layers[index].source = source;
        cycle_probe.scenes[scene_index].layers =
            vec![cycle_probe.scenes[scene_index].layers[index].clone()];
        if cycle_probe.validate().err().is_some_and(|errors| {
            errors.iter().any(|error| {
                error.kind == ValidationErrorKind::Cycle
                    && error.entity == Some(EntityRef::Scene(scene))
                    && error.field == "layers.source"
            })
        }) {
            return Err(SceneLayerError::SourceCycle);
        }
        self.scenes[scene_index].layers[index].source = source;
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

    pub fn add_audio_bus_checked(&mut self, bus: AudioBus) -> Result<(), AddAudioBusError> {
        if self
            .audio_buses
            .iter()
            .any(|candidate| candidate.id == bus.id)
        {
            return Err(AddAudioBusError::DuplicateId(bus.id));
        }
        if self
            .audio_buses
            .iter()
            .any(|candidate| candidate.name.eq_ignore_ascii_case(&bus.name))
        {
            return Err(AddAudioBusError::DuplicateName);
        }
        self.audio_buses.push(bus);
        Ok(())
    }

    pub fn add_output(&mut self, output: Output) {
        self.outputs.push(output);
    }

    pub fn add_output_checked(&mut self, output: Output) -> Result<(), AddOutputError> {
        if self
            .outputs
            .iter()
            .any(|candidate| candidate.id == output.id)
        {
            return Err(AddOutputError::DuplicateId(output.id));
        }
        if self
            .outputs
            .iter()
            .any(|candidate| candidate.name.eq_ignore_ascii_case(&output.name))
        {
            return Err(AddOutputError::DuplicateName);
        }
        if !self
            .scenes
            .iter()
            .any(|candidate| candidate.id == output.video_source)
        {
            return Err(AddOutputError::UnknownScene(output.video_source));
        }
        if !self
            .audio_buses
            .iter()
            .any(|candidate| candidate.id == output.audio_source)
        {
            return Err(AddOutputError::UnknownBus(output.audio_source));
        }
        self.outputs.push(output);
        Ok(())
    }

    pub fn set_output_route(
        &mut self,
        output: OutputId,
        scene: SceneId,
        bus: BusId,
    ) -> Result<(), SetOutputRouteError> {
        let output_index = self
            .outputs
            .iter()
            .position(|candidate| candidate.id == output)
            .ok_or(SetOutputRouteError::UnknownOutput(output))?;
        if !self.scenes.iter().any(|candidate| candidate.id == scene) {
            return Err(SetOutputRouteError::UnknownScene(scene));
        }
        if !self.audio_buses.iter().any(|candidate| candidate.id == bus) {
            return Err(SetOutputRouteError::UnknownBus(bus));
        }
        self.outputs[output_index].video_source = scene;
        self.outputs[output_index].audio_source = bus;
        Ok(())
    }

    pub fn remove_output(&mut self, output: OutputId) -> Result<(), RemoveOutputError> {
        if !self.outputs.iter().any(|candidate| candidate.id == output) {
            return Err(RemoveOutputError::UnknownOutput(output));
        }
        self.outputs.retain(|candidate| candidate.id != output);
        Ok(())
    }

    pub fn remove_audio_bus(&mut self, bus: BusId) -> Result<(), RemoveAudioBusError> {
        if !self.audio_buses.iter().any(|candidate| candidate.id == bus) {
            return Err(RemoveAudioBusError::UnknownBus(bus));
        }
        if let Some(output) = self
            .outputs
            .iter()
            .find(|output| output.audio_source == bus)
        {
            return Err(RemoveAudioBusError::OutputReference {
                bus,
                output: output.id,
            });
        }
        if let Some(send) = self
            .audio_buses
            .iter()
            .find(|candidate| candidate.id == bus)
            .and_then(|candidate| candidate.sends.first())
        {
            return Err(RemoveAudioBusError::OutgoingSend {
                bus,
                destination: send.destination,
            });
        }
        if let Some(source) = self.audio_buses.iter().find(|candidate| {
            candidate.id != bus && candidate.sends.iter().any(|send| send.destination == bus)
        }) {
            return Err(RemoveAudioBusError::IncomingSend {
                bus,
                source: source.id,
            });
        }
        self.audio_buses.retain(|candidate| candidate.id != bus);
        Ok(())
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

    pub fn set_scene_input_audio_source(
        &mut self,
        scene_input: InputId,
        audio_source: Option<InputId>,
    ) -> Result<(), SceneInputAudioSourceError> {
        let target = self
            .inputs
            .iter()
            .find(|input| input.id == scene_input)
            .ok_or(SceneInputAudioSourceError::MissingTargetInput(scene_input))?;
        if !matches!(target.kind, InputKind::Scene { .. }) {
            return Err(SceneInputAudioSourceError::NonSceneTargetInput(scene_input));
        }
        if let Some(source) = audio_source
            && !self.inputs.iter().any(|input| input.id == source)
        {
            return Err(SceneInputAudioSourceError::MissingSourceInput(source));
        }
        let mut candidate = self.clone();
        let target = candidate
            .inputs
            .iter_mut()
            .find(|input| input.id == scene_input)
            .expect("validated target input exists in candidate");
        let InputKind::Scene {
            audio_source: current,
            ..
        } = &mut target.kind
        else {
            unreachable!("validated target input is a scene input");
        };
        *current = audio_source;
        if candidate.validate().is_err_and(|errors| {
            errors.iter().any(|error| {
                error.entity == Some(EntityRef::Input(scene_input))
                    && error.field == "kind.scene.audio_source"
                    && matches!(
                        error.kind,
                        ValidationErrorKind::SelfReference | ValidationErrorKind::Cycle
                    )
            })
        }) {
            return Err(SceneInputAudioSourceError::SourceCycle);
        }
        *self = candidate;
        Ok(())
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
