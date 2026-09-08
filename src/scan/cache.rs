use std::path::{Path, PathBuf};

use super::{TargetEntry, measure::recursive_scan_target};

/// Path of the unstable cargo build cache (`$CARGO_HOME/build-cache`).
pub fn build_cache_path() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("CARGO_HOME")
        && !home.trim().is_empty()
    {
        return Some(PathBuf::from(home).join("build-cache"));
    }
    std::env::var("HOME")
        .ok()
        .filter(|home| !home.trim().is_empty())
        .map(|home| PathBuf::from(home).join(".cargo").join("build-cache"))
}

#[tracing::instrument]
pub fn build_cache_entry() -> Option<TargetEntry> {
    build_cache_path().and_then(|path| build_cache_entry_at(&path))
}

/// Measure the build cache at an explicit path.
pub fn build_cache_entry_at(path: &Path) -> Option<TargetEntry> {
    if !path.is_dir() || path.is_symlink() {
        return None;
    }
    let (size, last_modified) = recursive_scan_target(path);
    Some(TargetEntry {
        project_path: path.to_path_buf(),
        target_dir: path.to_path_buf(),
        kind: super::OutputKind::Target,
        shared: false,
        size: Some(size),
        last_modified,
        is_under_deletion: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::SystemTime;

    #[test]
    fn build_cache_entry_measures_present_dir() {
        let root = std::env::temp_dir().join("cargo-storage-test-build-cache");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("build-cache/content")).unwrap();
        fs::write(root.join("build-cache/content/blob.bin"), "12345678").unwrap();

        let entry = build_cache_entry_at(&root.join("build-cache")).expect("cache dir exists");
        assert_eq!(entry.project_path, root.join("build-cache"));
        assert!(entry.size.is_some_and(|s| s >= 8));
        assert!(
            entry
                .last_modified
                .is_some_and(|t| t > SystemTime::UNIX_EPOCH)
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_cache_entry_missing_dir_is_none() {
        let root = std::env::temp_dir().join("cargo-storage-test-build-cache-missing");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        assert!(build_cache_entry_at(&root.join("build-cache")).is_none());
        // A file at the cache path is not a cache either.
        fs::write(root.join("build-cache"), "not a dir").unwrap();
        assert!(build_cache_entry_at(&root.join("build-cache")).is_none());
        let _ = fs::remove_dir_all(&root);
    }
}
