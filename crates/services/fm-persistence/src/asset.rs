use std::{error::Error, fmt, fs, io, path::PathBuf};

use fm_model::InputKind;
use fm_types::InputId;

use crate::{ProjectStore, StoredProject};

const ASSETS_DIRECTORY: &str = "assets";
const ASSET_URI_PREFIX: &str = "asset://";
const MAX_ASSET_URI_BYTES: usize = 1024;

impl ProjectStore {
    /// Returns the assets directory beneath the project bundle root.
    #[must_use]
    pub fn assets_root(&self) -> PathBuf {
        self.root().join(ASSETS_DIRECTORY)
    }

    /// Resolves an `asset://` URI to a canonical regular file within the project.
    ///
    /// Resolution does not create the project or assets directories. Asset keys
    /// use `/` separators on every platform and do not support percent decoding.
    ///
    /// # Errors
    ///
    /// Returns an error if the URI is invalid, either path cannot be resolved,
    /// the resolved target escapes the assets directory, or the target is not a
    /// regular file.
    pub fn resolve_asset_uri(&self, uri: &str) -> Result<PathBuf, AssetResolveError> {
        let key = validate_asset_uri(uri)?;
        let assets_root = fs::canonicalize(self.assets_root())
            .map_err(AssetResolveError::AssetsRootUnavailable)?;
        if !fs::metadata(&assets_root)
            .map_err(AssetResolveError::AssetsRootUnavailable)?
            .is_dir()
        {
            return Err(AssetResolveError::AssetsRootUnavailable(io::Error::new(
                io::ErrorKind::NotADirectory,
                "assets root is not a directory",
            )));
        }

        let candidate =
            fs::canonicalize(assets_root.join(key)).map_err(AssetResolveError::AssetUnavailable)?;
        if !candidate.starts_with(&assets_root) {
            return Err(AssetResolveError::OutsideAssetsRoot);
        }
        if !fs::metadata(&candidate)
            .map_err(AssetResolveError::AssetUnavailable)?
            .is_file()
        {
            return Err(AssetResolveError::NotRegularFile);
        }

        Ok(candidate)
    }

    /// Reports media inputs whose project asset cannot be resolved.
    #[must_use]
    pub fn audit_assets(&self, project: &StoredProject) -> Vec<AssetAuditIssue> {
        let mut issues = project
            .project()
            .inputs()
            .iter()
            .filter_map(|input| {
                let InputKind::Media { asset_uri } = &input.kind else {
                    return None;
                };
                let reason = match self.resolve_asset_uri(asset_uri) {
                    Ok(_) => return None,
                    Err(AssetResolveError::InvalidUri) => AssetAuditReason::InvalidUri,
                    Err(
                        AssetResolveError::AssetsRootUnavailable(_)
                        | AssetResolveError::AssetUnavailable(_),
                    ) => AssetAuditReason::MissingAsset,
                    Err(AssetResolveError::OutsideAssetsRoot) => {
                        AssetAuditReason::OutsideAssetsRoot
                    }
                    Err(AssetResolveError::NotRegularFile) => AssetAuditReason::NotRegularFile,
                };
                Some(AssetAuditIssue {
                    input_id: input.id,
                    reason,
                })
            })
            .collect::<Vec<_>>();
        issues.sort_unstable_by_key(|issue| issue.input_id);
        issues
    }
}

/// A project input with an unresolved media asset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AssetAuditIssue {
    pub input_id: InputId,
    pub reason: AssetAuditReason,
}

/// A stable reason that a project media asset cannot be used.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetAuditReason {
    InvalidUri,
    MissingAsset,
    OutsideAssetsRoot,
    NotRegularFile,
}

fn validate_asset_uri(uri: &str) -> Result<&str, AssetResolveError> {
    if uri.len() > MAX_ASSET_URI_BYTES {
        return Err(AssetResolveError::InvalidUri);
    }
    let Some(key) = uri.strip_prefix(ASSET_URI_PREFIX) else {
        return Err(AssetResolveError::InvalidUri);
    };
    if key.is_empty()
        || key.starts_with('/')
        || key.contains(['\\', '\0', '?', '#', '%', ':'])
        || key.chars().any(char::is_control)
        || key
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        return Err(AssetResolveError::InvalidUri);
    }

    Ok(key)
}

/// A failure to resolve a project asset URI.
#[derive(Debug)]
pub enum AssetResolveError {
    /// The URI does not use the supported strict `asset://` syntax.
    InvalidUri,
    /// The project assets directory could not be resolved as a directory.
    AssetsRootUnavailable(io::Error),
    /// The requested asset could not be resolved or inspected.
    AssetUnavailable(io::Error),
    /// The requested asset resolves outside the project assets directory.
    OutsideAssetsRoot,
    /// The requested asset exists but is not a regular file.
    NotRegularFile,
}

impl fmt::Display for AssetResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidUri => "invalid project asset URI",
            Self::AssetsRootUnavailable(_) => "project assets root is unavailable",
            Self::AssetUnavailable(_) => "project asset is unavailable",
            Self::OutsideAssetsRoot => "project asset resolves outside the assets root",
            Self::NotRegularFile => "project asset is not a regular file",
        })
    }
}

impl Error for AssetResolveError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::AssetsRootUnavailable(error) | Self::AssetUnavailable(error) => Some(error),
            Self::InvalidUri | Self::OutsideAssetsRoot | Self::NotRegularFile => None,
        }
    }
}
