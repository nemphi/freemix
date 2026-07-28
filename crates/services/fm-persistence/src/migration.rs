use std::{fs::File, io::Read};

use crate::{CURRENT_SCHEMA_VERSION, MAX_MANIFEST_BYTES, ProjectStore, StoreError, json};

const V2_SCHEMA_VERSION: u32 = 2;
const V3_SCHEMA_VERSION: u32 = 3;
const V4_SCHEMA_VERSION: u32 = 4;
const V5_SCHEMA_VERSION: u32 = 5;
const V3_DEFAULTS: [&str; 8] = [
    "settings.frame_rate=60000/1001",
    "settings.video=1920x1080/nv12/progressive/bt709",
    "settings.audio=48000/f32/stereo",
    "inputs.kind=deterministic_simulated",
    "scenes=[]",
    "audio_buses=[]",
    "outputs=[]",
    "restart_policy=never",
];
const V4_DEFAULTS: [&str; 5] = [
    "scenes.background=rgba8(0,0,0,255)",
    "scenes.layers.geometry=canvas_identity",
    "scenes.layers.crop=null",
    "scenes.layers.opacity=255",
    "scenes.layers.z_order=0",
];
const V5_DEFAULTS: [&str; 1] = ["runtime.manual_transitions=inactive"];
const V6_DEFAULTS: [&str; 1] = ["scenes.layers.mask=null"];

/// Summary of an explicitly completed manifest migration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationReport {
    from_schema: u32,
    to_schema: u32,
    defaulted_fields: Vec<&'static str>,
}

impl MigrationReport {
    #[must_use]
    pub const fn from_schema(&self) -> u32 {
        self.from_schema
    }

    #[must_use]
    pub const fn to_schema(&self) -> u32 {
        self.to_schema
    }

    #[must_use]
    pub fn defaulted_fields(&self) -> &[&'static str] {
        &self.defaulted_fields
    }
}

impl ProjectStore {
    /// Explicitly migrates a schema-v2 manifest to the canonical schema.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed data, the wrong schema, validation,
    /// size-limit, or filesystem failures.
    pub fn migrate_v2(&self) -> Result<MigrationReport, StoreError> {
        let source = self.read_legacy_manifest()?;
        let project = json::decode_v2(&source).map_err(StoreError::from_decode)?;
        self.save(&project)?;
        Ok(MigrationReport {
            from_schema: V2_SCHEMA_VERSION,
            to_schema: CURRENT_SCHEMA_VERSION,
            defaulted_fields: V3_DEFAULTS
                .into_iter()
                .chain(V4_DEFAULTS)
                .chain(V5_DEFAULTS)
                .chain(V6_DEFAULTS)
                .collect(),
        })
    }

    /// Explicitly migrates a schema-v3 manifest to the canonical schema.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed data, the wrong schema, validation,
    /// size-limit, or filesystem failures.
    pub fn migrate_v3(&self) -> Result<MigrationReport, StoreError> {
        let source = self.read_legacy_manifest()?;
        let project = json::decode_v3(&source).map_err(StoreError::from_decode)?;
        self.save(&project)?;
        Ok(MigrationReport {
            from_schema: V3_SCHEMA_VERSION,
            to_schema: CURRENT_SCHEMA_VERSION,
            defaulted_fields: V4_DEFAULTS
                .into_iter()
                .chain(V5_DEFAULTS)
                .chain(V6_DEFAULTS)
                .collect(),
        })
    }

    /// Explicitly migrates a schema-v4 manifest to the canonical schema.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed data, the wrong schema, validation,
    /// size-limit, or filesystem failures.
    pub fn migrate_v4(&self) -> Result<MigrationReport, StoreError> {
        let source = self.read_legacy_manifest()?;
        let project = json::decode_v4(&source).map_err(StoreError::from_decode)?;
        self.save(&project)?;
        Ok(MigrationReport {
            from_schema: V4_SCHEMA_VERSION,
            to_schema: CURRENT_SCHEMA_VERSION,
            defaulted_fields: V5_DEFAULTS.into_iter().chain(V6_DEFAULTS).collect(),
        })
    }

    /// Explicitly migrates a schema-v5 manifest to the canonical schema.
    ///
    /// Exact desired and realized manual-transition state is preserved while
    /// every existing layer receives the explicit no-mask default.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed data, the wrong schema, validation,
    /// size-limit, or filesystem failures.
    pub fn migrate_v5(&self) -> Result<MigrationReport, StoreError> {
        let source = self.read_legacy_manifest()?;
        let project = json::decode_v5(&source).map_err(StoreError::from_decode)?;
        self.save(&project)?;
        Ok(MigrationReport {
            from_schema: V5_SCHEMA_VERSION,
            to_schema: CURRENT_SCHEMA_VERSION,
            defaulted_fields: V6_DEFAULTS.to_vec(),
        })
    }

    fn read_legacy_manifest(&self) -> Result<String, StoreError> {
        let mut file = File::open(self.manifest_path()).map_err(StoreError::Io)?;
        let size = file.metadata().map_err(StoreError::Io)?.len();
        if size > MAX_MANIFEST_BYTES {
            return Err(StoreError::ManifestTooLarge {
                size,
                maximum: MAX_MANIFEST_BYTES,
            });
        }
        let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or_default());
        file.by_ref()
            .take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(StoreError::Io)?;
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(StoreError::ManifestTooLarge {
                size: bytes.len() as u64,
                maximum: MAX_MANIFEST_BYTES,
            });
        }
        String::from_utf8(bytes).map_err(|error| {
            StoreError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        })
    }
}
