use std::{fs::File, io::Read};

use crate::{CURRENT_SCHEMA_VERSION, MAX_MANIFEST_BYTES, ProjectStore, StoreError, json};

const V2_SCHEMA_VERSION: u32 = 2;
const V3_SCHEMA_VERSION: u32 = 3;
const V4_SCHEMA_VERSION: u32 = 4;
const V5_SCHEMA_VERSION: u32 = 5;
const V6_SCHEMA_VERSION: u32 = 6;
const V7_SCHEMA_VERSION: u32 = 7;
const V8_SCHEMA_VERSION: u32 = 8;
const V9_SCHEMA_VERSION: u32 = 9;
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
const V7_DEFAULTS: [&str; 1] =
    ["input_audio_strips=per-input gain_milli_db=0/muted=false/follow_video=true"];
const V8_DEFAULTS: [&str; 1] = ["runtime.fade_to_black=live"];
const V10_DEFAULTS: [&str; 1] = ["stingers=[]"];

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
                .chain(V7_DEFAULTS)
                .chain(V8_DEFAULTS)
                .chain(V10_DEFAULTS)
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
                .chain(V7_DEFAULTS)
                .chain(V8_DEFAULTS)
                .chain(V10_DEFAULTS)
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
            defaulted_fields: V5_DEFAULTS
                .into_iter()
                .chain(V6_DEFAULTS)
                .chain(V7_DEFAULTS)
                .chain(V8_DEFAULTS)
                .chain(V10_DEFAULTS)
                .collect(),
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
            defaulted_fields: V6_DEFAULTS
                .into_iter()
                .chain(V7_DEFAULTS)
                .chain(V8_DEFAULTS)
                .chain(V10_DEFAULTS)
                .collect(),
        })
    }

    /// Explicitly migrates a schema-v6 manifest to the current schema.
    ///
    /// Exact masks and desired/realized manual-transition state are preserved.
    /// Every input receives the previously hard-coded native daemon strip
    /// behavior: unity gain, unmuted, and follow-video enabled.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed data, the wrong schema, validation,
    /// size-limit, or filesystem failures.
    pub fn migrate_v6(&self) -> Result<MigrationReport, StoreError> {
        let source = self.read_legacy_manifest()?;
        let project = json::decode_v6(&source).map_err(StoreError::from_decode)?;
        self.save(&project)?;
        Ok(MigrationReport {
            from_schema: V6_SCHEMA_VERSION,
            to_schema: CURRENT_SCHEMA_VERSION,
            defaulted_fields: V7_DEFAULTS
                .into_iter()
                .chain(V8_DEFAULTS)
                .chain(V10_DEFAULTS)
                .collect(),
        })
    }

    /// Explicitly migrates a schema-v7 manifest to the current schema.
    ///
    /// Exact project audio-strip and manual-transition state is preserved.
    /// Fade-to-Black defaults to the settled live endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed data, the wrong schema, validation,
    /// size-limit, or filesystem failures.
    pub fn migrate_v7(&self) -> Result<MigrationReport, StoreError> {
        let source = self.read_legacy_manifest()?;
        let project = json::decode_v7(&source).map_err(StoreError::from_decode)?;
        self.save(&project)?;
        Ok(MigrationReport {
            from_schema: V7_SCHEMA_VERSION,
            to_schema: CURRENT_SCHEMA_VERSION,
            defaulted_fields: V8_DEFAULTS.into_iter().chain(V10_DEFAULTS).collect(),
        })
    }

    /// Explicitly migrates a schema-v8 manifest to the current schema.
    ///
    /// Project, routing, manual-transition, Fade-to-Black, and receipt state
    /// are preserved exactly. Schema v9 extends the set of manual-transition
    /// kinds without adding a defaulted field.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed data, the wrong schema, validation,
    /// size-limit, or filesystem failures.
    pub fn migrate_v8(&self) -> Result<MigrationReport, StoreError> {
        let source = self.read_legacy_manifest()?;
        let project = json::decode_v8(&source).map_err(StoreError::from_decode)?;
        self.save(&project)?;
        Ok(MigrationReport {
            from_schema: V8_SCHEMA_VERSION,
            to_schema: CURRENT_SCHEMA_VERSION,
            defaulted_fields: V10_DEFAULTS.to_vec(),
        })
    }

    /// Explicitly migrates a schema-v9 manifest to the current schema.
    ///
    /// Existing project and runtime state are preserved exactly and the new
    /// Stinger slot collection defaults to empty.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed data, the wrong schema, validation,
    /// size-limit, or filesystem failures.
    pub fn migrate_v9(&self) -> Result<MigrationReport, StoreError> {
        let source = self.read_legacy_manifest()?;
        let project = json::decode_v9(&source).map_err(StoreError::from_decode)?;
        self.save(&project)?;
        Ok(MigrationReport {
            from_schema: V9_SCHEMA_VERSION,
            to_schema: CURRENT_SCHEMA_VERSION,
            defaulted_fields: V10_DEFAULTS.to_vec(),
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
