//! Where credentials live between runs.
//!
//! The backends never pick a location or touch the disk themselves: they are handed a [`CredentialStore`], so an
//! application can keep credentials in a file ([`FileStore`]), an OS keyring, memory ([`MemoryStore`]) or anywhere else.

use crate::error::StoreError;
#[cfg(any(feature = "graph", feature = "teams"))]
use serde::Serialize;
#[cfg(any(feature = "graph", feature = "teams"))]
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A place to keep one opaque blob of credentials. Implementations decide how it is protected.
pub trait CredentialStore: Send + Sync {
    /// The stored bytes, or `None` when nothing has been stored.
    ///
    /// # Errors
    /// [`StoreError`] when the storage exists but cannot be read.
    fn load(&self) -> Result<Option<Vec<u8>>, StoreError>;

    /// Replace the stored bytes. Implementations should not leave a half-written value behind on failure.
    ///
    /// # Errors
    /// [`StoreError`] when the value cannot be stored.
    fn save(&self, bytes: &[u8]) -> Result<(), StoreError>;

    /// Forget the stored value. Returns whether there was one.
    ///
    /// # Errors
    /// [`StoreError`] when the value exists but cannot be removed.
    fn clear(&self) -> Result<bool, StoreError>;
}

/// Create `path` and any missing parents so that only the current user can use them: mode 0700 on Unix. Directories
/// that already exist are left as they are. On Windows new directories inherit the access list of their parent, which
/// for a per-user location such as `%APPDATA%` is the user's alone.
///
/// # Errors
/// The underlying I/O error if a directory cannot be created.
pub fn create_private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Credentials in one file. Writes go to a temporary file first and are renamed into place, so a crash cannot leave
/// a truncated file, and on Unix the file is readable by its owner only (on Windows a per-user location such as
/// `%APPDATA%` is expected).
#[derive(Debug, Clone)]
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    /// A store backed by `path`; parent directories are created on the first save.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The file this store uses.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn io(&self, action: &'static str, path: &Path, source: std::io::Error) -> StoreError {
        StoreError::Io {
            action,
            path: path.to_path_buf(),
            source,
        }
    }
}

impl CredentialStore for FileStore {
    fn load(&self) -> Result<Option<Vec<u8>>, StoreError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(self.io("read", &self.path, e)),
        }
    }

    fn save(&self, bytes: &[u8]) -> Result<(), StoreError> {
        use std::io::Write;
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            create_private_dir(dir).map_err(|e| self.io("create", dir, e))?;
        }
        // A name unique to this process, so two instances saving at once cannot interleave in one temporary file.
        let tmp = self
            .path
            .with_extension(format!("{}.tmp", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let written = options
            .open(&tmp)
            .and_then(|mut file| file.write_all(bytes).and_then(|()| file.sync_all()));
        if let Err(source) = written {
            let _ = std::fs::remove_file(&tmp); // best effort: leave no half-written file behind
            return Err(self.io("write", &tmp, source));
        }
        std::fs::rename(&tmp, &self.path).map_err(|source| {
            let _ = std::fs::remove_file(&tmp);
            self.io("replace", &self.path, source)
        })
    }

    fn clear(&self) -> Result<bool, StoreError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(self.io("remove", &self.path, e)),
        }
    }
}

/// Credentials held in memory only: nothing survives the process. Useful for tests and one-shot tools.
#[derive(Debug, Default)]
pub struct MemoryStore {
    value: Mutex<Option<Vec<u8>>>,
}

impl MemoryStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    // The guarded value is replaced wholesale, so it is valid at every point even if a holder panicked.
    fn value(&self) -> std::sync::MutexGuard<'_, Option<Vec<u8>>> {
        self.value
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl CredentialStore for MemoryStore {
    fn load(&self) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self.value().clone())
    }

    fn save(&self, bytes: &[u8]) -> Result<(), StoreError> {
        *self.value() = Some(bytes.to_vec());
        Ok(())
    }

    fn clear(&self) -> Result<bool, StoreError> {
        Ok(self.value().take().is_some())
    }
}

/// The format version written into stored credential documents. A document without a version is version 1.
#[cfg(any(feature = "graph", feature = "teams"))]
pub(crate) const FORMAT_VERSION: u32 = 1;

/// Serde default for the `version` field of stored documents.
#[cfg(any(feature = "graph", feature = "teams"))]
pub(crate) fn format_version() -> u32 {
    FORMAT_VERSION
}

/// Refuse documents written by a newer format than this build understands.
#[cfg(any(feature = "graph", feature = "teams"))]
pub(crate) fn check_version(found: u32) -> Result<(), StoreError> {
    if found > FORMAT_VERSION {
        Err(StoreError::UnsupportedVersion {
            found,
            supported: FORMAT_VERSION,
        })
    } else {
        Ok(())
    }
}

/// Load and parse a JSON document from `store`.
#[cfg(any(feature = "graph", feature = "teams"))]
pub(crate) fn load_json<T: DeserializeOwned>(
    store: &dyn CredentialStore,
) -> Result<Option<T>, StoreError> {
    let Some(bytes) = store.load()? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| StoreError::Corrupt {
            source: Box::new(e),
        })
}

/// Serialise `value` as JSON into `store`.
#[cfg(any(feature = "graph", feature = "teams"))]
pub(crate) fn save_json<T: Serialize>(
    store: &dyn CredentialStore,
    value: &T,
) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| StoreError::Encode {
        what: "credentials",
        source: Box::new(e),
    })?;
    store.save(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_store_round_trips_replaces_and_clears() {
        let dir = crate::testdir::TestDir::new("store");
        let store = FileStore::new(dir.join("nested").join("creds.json"));
        assert_eq!(store.load().unwrap(), None);
        store.save(b"one").unwrap();
        store.save(b"two").unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some(&b"two"[..]));
        let leftovers = std::fs::read_dir(store.path().parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0, "temp file renamed away");
        assert!(store.clear().unwrap());
        assert!(!store.clear().unwrap());
    }

    #[test]
    fn memory_store_behaves_like_a_file_store() {
        let store = MemoryStore::new();
        assert_eq!(store.load().unwrap(), None);
        store.save(b"x").unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some(&b"x"[..]));
        assert!(store.clear().unwrap() && !store.clear().unwrap());
    }

    #[cfg(any(feature = "graph", feature = "teams"))]
    #[test]
    fn corrupt_json_is_a_typed_error_not_a_missing_value() {
        let store = MemoryStore::new();
        store.save(b"{not json").unwrap();
        let err = load_json::<serde_json::Value>(&store).unwrap_err();
        assert!(matches!(err, StoreError::Corrupt { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn credential_files_are_readable_by_their_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        // Only the path is reserved: the store makes the folder, as it does for a first sign-in. A folder the test made
        // itself would have the platform's default mode (0755 with the usual umask) and prove nothing about the store.
        let dir = crate::testdir::TestDir::reserve("mode");
        let store = FileStore::new(dir.join("creds.json"));
        store.save(b"secret").unwrap();
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = std::fs::metadata(&*dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            dir_mode, 0o700,
            "the directory made for the file is private too"
        );
    }

    #[test]
    fn private_directories_are_created_with_their_parents_and_existing_ones_are_untouched() {
        // The root is reserved, not made, so that `create_private_dir` makes all three folders (see above).
        let root = crate::testdir::TestDir::reserve("private");
        let leaf = root.join("a").join("b");
        create_private_dir(&leaf).unwrap();
        assert!(leaf.is_dir());
        create_private_dir(&leaf).unwrap(); // already there: not an error
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for dir in [&*root, &root.join("a"), &leaf] {
                let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o700, "{}", dir.display());
            }
            // A folder that is already there keeps whatever mode it had, whatever that is.
            let existing = crate::testdir::TestDir::new("existing");
            std::fs::set_permissions(&*existing, std::fs::Permissions::from_mode(0o750)).unwrap();
            create_private_dir(&existing).unwrap();
            let mode = std::fs::metadata(&*existing).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o750, "an existing folder is left as it was");
        }
    }
}
