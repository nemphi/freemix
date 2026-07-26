use std::{
    error::Error,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{ProjectValidationError, StoredProject, json};

const MANIFEST_NAME: &str = "project.json";
/// Maximum supported on-disk manifest size, in bytes (4 MiB).
pub const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn next_temp_sequence() -> u64 {
    TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

/// A durable project directory ending in `.freemix`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectStore {
    root: PathBuf,
}

impl ProjectStore {
    /// Creates a store handle without touching the filesystem.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidRoot`] unless `root` is named `.freemix`
    /// or has the `.freemix` extension.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        let is_bundle = root.file_name().is_some_and(|name| name == ".freemix")
            || root
                .extension()
                .is_some_and(|extension| extension == "freemix");
        if !is_bundle {
            return Err(StoreError::InvalidRoot { root });
        }
        Ok(Self { root })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn manifest_path(&self) -> PathBuf {
        self.root.join(MANIFEST_NAME)
    }

    /// Atomically replaces the durable manifest after validation.
    ///
    /// The temporary file is created beside the manifest, flushed with
    /// `sync_all`, then renamed into place.
    ///
    /// # Errors
    ///
    /// Returns validation or filesystem errors. A failed validation performs
    /// no filesystem mutation. Manifests larger than
    /// [`MAX_MANIFEST_BYTES`] are rejected.
    pub fn save(&self, project: &StoredProject) -> Result<(), StoreError> {
        project.validate().map_err(StoreError::Validation)?;
        let manifest = json::encode(project);
        enforce_manifest_size(manifest.len().try_into().unwrap_or(u64::MAX))?;

        let root_existed = self.root.try_exists().map_err(StoreError::Io)?;
        fs::create_dir_all(&self.root).map_err(StoreError::Io)?;
        if !root_existed {
            sync_directory(parent_directory(&self.root))?;
        }

        let temp_path = self.temp_path();
        let mut guard = TempGuard::new(temp_path.clone());
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(StoreError::Io)?;
        file.write_all(manifest.as_bytes())
            .map_err(StoreError::Io)?;
        file.sync_all().map_err(StoreError::Io)?;
        drop(file);
        fs::rename(&temp_path, self.manifest_path()).map_err(StoreError::Io)?;
        guard.disarm();
        sync_directory(&self.root)?;
        Ok(())
    }

    /// Reads, strictly parses, and fully validates the manifest.
    ///
    /// # Errors
    ///
    /// Returns filesystem, syntax, size-limit, or project validation errors.
    /// No partial project is returned. At most [`MAX_MANIFEST_BYTES`] plus one
    /// byte is read, including if the file grows while it is being loaded.
    pub fn load(&self) -> Result<StoredProject, StoreError> {
        let file = File::open(self.manifest_path()).map_err(StoreError::Io)?;
        let size = file.metadata().map_err(StoreError::Io)?.len();
        enforce_manifest_size(size)?;

        let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or_default());
        file.take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(StoreError::Io)?;
        enforce_manifest_size(bytes.len().try_into().unwrap_or(u64::MAX))?;
        let source = String::from_utf8(bytes)
            .map_err(|error| StoreError::Io(io::Error::new(io::ErrorKind::InvalidData, error)))?;
        json::decode(&source).map_err(|error| match error {
            json::DecodeError::Syntax { offset, message } => {
                StoreError::MalformedManifest { offset, message }
            }
            json::DecodeError::Validation(error) => StoreError::Validation(error),
        })
    }

    fn temp_path(&self) -> PathBuf {
        let sequence = next_temp_sequence();
        self.root.join(format!(
            ".{MANIFEST_NAME}.tmp-{}-{sequence}",
            std::process::id()
        ))
    }
}

fn enforce_manifest_size(size: u64) -> Result<(), StoreError> {
    if size > MAX_MANIFEST_BYTES {
        Err(StoreError::ManifestTooLarge {
            size,
            maximum: MAX_MANIFEST_BYTES,
        })
    } else {
        Ok(())
    }
}

fn parent_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(unix)]
pub(crate) fn sync_directory(path: &Path) -> Result<(), StoreError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(StoreError::Io)
}

#[cfg(not(unix))]
pub(crate) fn sync_directory(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

struct TempGuard {
    path: PathBuf,
    armed: bool,
}

impl TempGuard {
    const fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug)]
pub enum StoreError {
    InvalidRoot { root: PathBuf },
    ManifestTooLarge { size: u64, maximum: u64 },
    Io(std::io::Error),
    MalformedManifest { offset: usize, message: String },
    Validation(ProjectValidationError),
    Journal(crate::JournalError),
}

impl StoreError {
    pub(crate) fn from_decode(error: json::DecodeError) -> Self {
        match error {
            json::DecodeError::Syntax { offset, message } => {
                Self::MalformedManifest { offset, message }
            }
            json::DecodeError::Validation(error) => Self::Validation(error),
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRoot { root } => {
                write!(
                    formatter,
                    "project root `{}` is not a .freemix directory",
                    root.display()
                )
            }
            Self::ManifestTooLarge { size, maximum } => write!(
                formatter,
                "manifest is {size} bytes, exceeding the {maximum}-byte maximum"
            ),
            Self::Io(error) => error.fmt(formatter),
            Self::MalformedManifest { offset, message } => {
                write!(formatter, "malformed manifest at byte {offset}: {message}")
            }
            Self::Validation(error) => error.fmt(formatter),
            Self::Journal(error) => error.fmt(formatter),
        }
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Validation(error) => Some(error),
            Self::Journal(error) => Some(error),
            Self::InvalidRoot { .. }
            | Self::ManifestTooLarge { .. }
            | Self::MalformedManifest { .. } => None,
        }
    }
}
