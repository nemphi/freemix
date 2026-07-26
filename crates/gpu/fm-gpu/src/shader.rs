use crate::DeviceProfile;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShaderStage {
    Vertex,
    Fragment,
    Compute,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShaderLanguage {
    Wgsl,
    Glsl,
    MetalShadingLanguage,
    SpirV,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShaderSource {
    Text(String),
    Binary(Vec<u8>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShaderDescriptor {
    pub label: String,
    pub stage: ShaderStage,
    pub language: ShaderLanguage,
    pub entry_point: String,
    pub source: ShaderSource,
}

impl ShaderDescriptor {
    #[must_use]
    pub fn new(
        label: impl Into<String>,
        stage: ShaderStage,
        language: ShaderLanguage,
        entry_point: impl Into<String>,
        source: ShaderSource,
    ) -> Self {
        Self {
            label: label.into(),
            stage,
            language,
            entry_point: entry_point.into(),
            source,
        }
    }

    /// Checks only portable descriptor structure and source representation.
    ///
    /// # Errors
    ///
    /// Returns an error for empty fields, empty source, or a text/binary source
    /// that does not match the declared language.
    pub fn validate_contract(&self) -> Result<(), ShaderError> {
        if self.label.trim().is_empty() {
            return Err(ShaderError::EmptyLabel);
        }
        if self.entry_point.trim().is_empty() {
            return Err(ShaderError::EmptyEntryPoint);
        }
        match (&self.source, self.language) {
            (ShaderSource::Text(source), _) if source.is_empty() => Err(ShaderError::EmptySource),
            (ShaderSource::Binary(source), _) if source.is_empty() => Err(ShaderError::EmptySource),
            (ShaderSource::Binary(source), ShaderLanguage::SpirV) if source.len() % 4 != 0 => {
                Err(ShaderError::InvalidBinaryLength)
            }
            (ShaderSource::Binary(_), ShaderLanguage::SpirV)
            | (
                ShaderSource::Text(_),
                ShaderLanguage::Wgsl | ShaderLanguage::Glsl | ShaderLanguage::MetalShadingLanguage,
            ) => Ok(()),
            _ => Err(ShaderError::SourceLanguageMismatch),
        }
    }
}

/// Strength of the validation represented by metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShaderValidationLevel {
    /// Portable descriptor checks only; no parser, compiler, or GPU was used.
    ContractOnly,
    /// Validation supplied by an external parser or compiler implementation.
    Language,
    /// Validation supplied by a concrete device backend.
    Device,
}

/// Auditable metadata supplied by a shader validator implementation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShaderValidationMetadata {
    pub validator: String,
    pub validator_version: String,
    pub level: ShaderValidationLevel,
    pub source_fingerprint: u64,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedShader {
    descriptor: ShaderDescriptor,
    metadata: ShaderValidationMetadata,
}

impl ValidatedShader {
    #[must_use]
    pub const fn descriptor(&self) -> &ShaderDescriptor {
        &self.descriptor
    }

    #[must_use]
    pub const fn metadata(&self) -> &ShaderValidationMetadata {
        &self.metadata
    }
}

/// Seam for real language/backend validators supplied by higher layers.
pub trait ShaderValidator {
    /// Validates a descriptor and records exactly what kind of validation ran.
    ///
    /// # Errors
    ///
    /// Returns structural, language, or backend-specific validation errors.
    fn validate(
        &self,
        descriptor: ShaderDescriptor,
        profile: &DeviceProfile,
    ) -> Result<ValidatedShader, ShaderError>;
}

/// Deterministic validator that explicitly performs contract checks only.
#[derive(Clone, Copy, Debug, Default)]
pub struct ContractShaderValidator;

impl ShaderValidator for ContractShaderValidator {
    fn validate(
        &self,
        descriptor: ShaderDescriptor,
        _profile: &DeviceProfile,
    ) -> Result<ValidatedShader, ShaderError> {
        descriptor.validate_contract()?;
        let source_fingerprint = fingerprint(&descriptor.source);
        Ok(ValidatedShader {
            descriptor,
            metadata: ShaderValidationMetadata {
                validator: "fm-gpu-contract".to_owned(),
                validator_version: env!("CARGO_PKG_VERSION").to_owned(),
                level: ShaderValidationLevel::ContractOnly,
                source_fingerprint,
                warnings: Vec::new(),
            },
        })
    }
}

fn fingerprint(source: &ShaderSource) -> u64 {
    let bytes = match source {
        ShaderSource::Text(source) => source.as_bytes(),
        ShaderSource::Binary(source) => source,
    };
    // FNV-1a is stable across platforms and sufficient for metadata identity;
    // this value is not presented as a cryptographic digest.
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShaderError {
    EmptyLabel,
    EmptyEntryPoint,
    EmptySource,
    SourceLanguageMismatch,
    InvalidBinaryLength,
    ValidationFailed { validator: String, message: String },
}

impl std::fmt::Display for ShaderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyLabel => formatter.write_str("shader label is empty"),
            Self::EmptyEntryPoint => formatter.write_str("shader entry point is empty"),
            Self::EmptySource => formatter.write_str("shader source is empty"),
            Self::SourceLanguageMismatch => {
                formatter.write_str("shader source representation does not match its language")
            }
            Self::InvalidBinaryLength => {
                formatter.write_str("SPIR-V source length is not a multiple of four bytes")
            }
            Self::ValidationFailed { validator, message } => {
                write!(formatter, "shader validator {validator} failed: {message}")
            }
        }
    }
}

impl std::error::Error for ShaderError {}
