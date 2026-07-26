use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use fm_persistence::{AssetResolveError, ProjectStore};

static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fm-persistence-assets-{}-{sequence}-{name}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn store(&self) -> ProjectStore {
        ProjectStore::new(self.0.join("show.freemix")).unwrap()
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn create_asset(store: &ProjectStore, key: &str) -> PathBuf {
    let path = store.assets_root().join(key);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"asset").unwrap();
    path
}

#[test]
fn resolves_nested_regular_file_beneath_assets_root() {
    let temp = TestDirectory::new("nested");
    let store = temp.store();
    let asset = create_asset(&store, "video/opening.mov");

    assert_eq!(
        store
            .resolve_asset_uri("asset://video/opening.mov")
            .unwrap(),
        fs::canonicalize(asset).unwrap()
    );
    assert_eq!(store.assets_root(), store.root().join("assets"));
}

#[test]
fn resolves_portable_unicode_filename() {
    let temp = TestDirectory::new("unicode");
    let store = temp.store();
    let asset = create_asset(&store, "stills/café.png");

    assert_eq!(
        store.resolve_asset_uri("asset://stills/café.png").unwrap(),
        fs::canonicalize(asset).unwrap()
    );
}

#[test]
fn rejects_invalid_uri_and_portable_path_tricks() {
    let temp = TestDirectory::new("invalid-uri");
    let store = temp.store();
    create_asset(&store, "valid.mov");
    let too_long = format!("asset://{}", "a".repeat(1017));
    let invalid = [
        "",
        "valid.mov",
        "file://valid.mov",
        "ASSET://valid.mov",
        "asset:/valid.mov",
        "asset://",
        "asset:///etc/passwd",
        "asset:////server/share/file.mov",
        "asset://.",
        "asset://..",
        "asset://./valid.mov",
        "asset://nested/../valid.mov",
        "asset://nested/./valid.mov",
        "asset://nested//valid.mov",
        "asset://nested/",
        "asset://..\\valid.mov",
        "asset://nested\\valid.mov",
        "asset://nested\\..\\valid.mov",
        "asset://C:/Windows/system.ini",
        "asset://C:relative.mov",
        "asset://nested/name:stream",
        "asset://\\\\server\\share\\file.mov",
        "asset://valid.mov?download=1",
        "asset://valid.mov#frame",
        "asset://%2e%2e/valid.mov",
        "asset://nested%2fvalid.mov",
        "asset://bad\0name.mov",
        "asset://bad\nname.mov",
        "asset://bad\u{7f}name.mov",
    ];

    for uri in invalid.into_iter().chain([too_long.as_str()]) {
        assert!(
            matches!(
                store.resolve_asset_uri(uri),
                Err(AssetResolveError::InvalidUri)
            ),
            "accepted {uri:?}"
        );
    }
}

#[test]
fn reports_missing_assets_root_and_file_without_creating_paths() {
    let temp = TestDirectory::new("missing");
    let store = temp.store();

    assert!(matches!(
        store.resolve_asset_uri("asset://missing.mov"),
        Err(AssetResolveError::AssetsRootUnavailable(_))
    ));
    assert!(!store.assets_root().exists());

    fs::create_dir_all(store.assets_root()).unwrap();
    assert!(matches!(
        store.resolve_asset_uri("asset://missing.mov"),
        Err(AssetResolveError::AssetUnavailable(_))
    ));
}

#[test]
fn rejects_directory_target() {
    let temp = TestDirectory::new("directory");
    let store = temp.store();
    fs::create_dir_all(store.assets_root().join("clips")).unwrap();

    assert!(matches!(
        store.resolve_asset_uri("asset://clips"),
        Err(AssetResolveError::NotRegularFile)
    ));
}

#[cfg(unix)]
#[test]
fn rejects_symlink_escape_after_canonicalization() {
    use std::os::unix::fs::symlink;

    let temp = TestDirectory::new("symlink-escape");
    let store = temp.store();
    fs::create_dir_all(store.assets_root()).unwrap();
    let outside = store.root().join("assets-escape").join("outside.mov");
    fs::create_dir_all(outside.parent().unwrap()).unwrap();
    fs::write(&outside, b"outside").unwrap();
    symlink(&outside, store.assets_root().join("escape.mov")).unwrap();

    assert!(matches!(
        store.resolve_asset_uri("asset://escape.mov"),
        Err(AssetResolveError::OutsideAssetsRoot)
    ));
}

#[test]
fn display_messages_do_not_expose_uri_or_filesystem_paths() {
    let temp = TestDirectory::new("secret-path-marker");
    let store = temp.store();
    let invalid = store
        .resolve_asset_uri("asset://secret-uri-marker/../file.mov")
        .unwrap_err();
    let missing_root = store
        .resolve_asset_uri("asset://secret-uri-marker.mov")
        .unwrap_err();

    for error in [&invalid, &missing_root] {
        let message = error.to_string();
        assert!(!message.contains("secret-uri-marker"));
        assert!(!message.contains("secret-path-marker"));
        assert!(!message.contains(temp.0.to_string_lossy().as_ref()));
    }
}
