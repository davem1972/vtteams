//! A scratch directory for a test that is removed when the test is done.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A uniquely named directory under the temporary directory (process id and a process-wide counter, so tests running
/// in parallel never share one). It is deleted, with everything in it, when the value is dropped.
#[derive(Debug)]
pub struct TestDir {
    path: PathBuf,
}

impl TestDir {
    /// The path only, for code that creates the directory itself when it needs it. Anything made there is still
    /// removed on drop.
    pub fn reserve(label: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("teams-client-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        Self { path }
    }

    /// An empty directory that exists.
    pub fn new(label: &str) -> Self {
        let dir = Self::reserve(label);
        std::fs::create_dir_all(&dir.path).unwrap();
        dir
    }
}

impl std::ops::Deref for TestDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_is_unique_and_gone_after_it_is_dropped() {
        let a = TestDir::new("testdir");
        let b = TestDir::new("testdir");
        assert_ne!(*a, *b);
        std::fs::write(a.join("file"), "x").unwrap();
        let path = a.to_path_buf();
        assert!(path.exists());
        drop(a);
        assert!(!path.exists());
        assert!(b.exists());
    }

    #[test]
    fn a_reserved_directory_is_not_created_until_needed() {
        let dir = TestDir::reserve("testdir");
        assert!(!dir.exists());
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        let path = dir.to_path_buf();
        drop(dir);
        assert!(!path.exists());
    }
}
