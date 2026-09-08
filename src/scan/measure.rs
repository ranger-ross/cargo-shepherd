use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// A fresh size reading for one watched directory, without a full rescan.
#[derive(Clone, Debug)]
pub struct Measurement {
    pub target_dir: PathBuf,
    pub size: u64,
    /// Newest mtime found, or `None` when the dir is gone.
    pub last_modified: Option<SystemTime>,
}

#[tracing::instrument(skip_all, fields(target = %target_dir.display()))]
pub fn measure_target(target_dir: &Path) -> Measurement {
    let (size, last_modified) = recursive_scan_target(target_dir);
    Measurement {
        target_dir: target_dir.to_path_buf(),
        size,
        last_modified,
    }
}

/// Recursively measure disk usage and newest mtime in one parallel walk.
///
/// Size matches `du`: allocated-block sizes, each inode counted once. Missing
/// paths measure empty with no timestamp, unreadable subtrees add nothing.
/// Symlinks are never followed but count their own blocks, like before.
#[tracing::instrument(skip_all, fields(path = %path.as_ref().display()))]
pub(super) fn recursive_scan_target<T: AsRef<Path>>(path: T) -> (u64, Option<SystemTime>) {
    let path = path.as_ref();
    if !path.exists() || path.is_symlink() {
        return (0, None);
    }
    let state = ScanState::default();
    // Seed the floor with the root's own mtime and blocks: the serial walk
    // counted the root entry too, so an existing-but-empty dir still
    // reports a real timestamp instead of the epoch.
    if let Ok(md) = std::fs::symlink_metadata(path) {
        let mut acc = (0u64, 0u64);
        state.claim(&md, &mut acc);
        state.publish(acc);
    }
    // One rayon task per directory, sharing the global pool with the
    // cross-target fan-out. Files stat inline in the task that lists
    // their dir; only subdirs spawn, so flat `deps/`-style dirs stay
    // local and deep trees fan out by depth 2-3.
    rayon::scope(|s| scan_dir(path.to_path_buf(), s, &state));
    state.finish()
}

/// Shared accumulators for one target walk. Each directory task batches
/// its files locally and publishes one add plus one max, so 16 threads
/// never ping-pong a cache line per file. The mutex only sees files
/// with extra hard links.
#[derive(Default)]
struct ScanState {
    total: AtomicU64,
    newest_ns: AtomicU64,
    #[cfg(unix)]
    seen: Mutex<HashSet<(u64, u64)>>,
}

impl ScanState {
    /// Fold one entry into a task-local `(total, newest)`. Returns
    /// `false` for a repeat hard link, which counts nothing.
    fn claim(&self, md: &std::fs::Metadata, acc: &mut (u64, u64)) -> bool {
        // Only inodes that can repeat need dedup: files with extra links.
        // Plain files and dirs skip the set entirely.
        #[cfg(unix)]
        {
            let nlink = if md.is_dir() { 1 } else { md.nlink() };
            if nlink > 1
                && !self
                    .seen
                    .lock()
                    .is_ok_and(|mut seen| seen.insert((md.dev(), md.ino())))
            {
                return false;
            }
        }
        #[cfg(unix)]
        let size = md.blocks() * 512;
        #[cfg(not(unix))]
        let size = md.len();
        acc.0 += size;
        acc.1 = acc.1.max(mtime_ns(md));
        true
    }

    fn publish(&self, acc: (u64, u64)) {
        self.total.fetch_add(acc.0, Ordering::Relaxed);
        self.newest_ns.fetch_max(acc.1, Ordering::Relaxed);
    }

    fn finish(&self) -> (u64, Option<SystemTime>) {
        (
            self.total.load(Ordering::Relaxed),
            Some(
                SystemTime::UNIX_EPOCH
                    + Duration::from_nanos(self.newest_ns.load(Ordering::Relaxed)),
            ),
        )
    }
}

fn scan_dir<'a>(dir: PathBuf, scope: &rayon::Scope<'a>, state: &'a ScanState) {
    let Ok(read_dir) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut acc = (0u64, 0u64);
    for entry in read_dir.flatten() {
        if entry.file_type().is_err() {
            continue;
        }
        // lstat: symlinks record their own inode but are never followed.
        let Ok(md) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !state.claim(&md, &mut acc) {
            continue;
        }
        if md.is_dir() {
            scope.spawn(move |s| scan_dir(entry.path(), s, state));
        }
    }
    state.publish(acc);
}

fn mtime_ns(md: &std::fs::Metadata) -> u64 {
    md.modified()
        .ok()
        .and_then(|st| st.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::SystemTime;

    // Dedup relies on inode identity, which only Unix exposes.
    #[cfg(unix)]
    #[test]
    fn hardlinked_file_counts_once_like_du() {
        let root = std::env::temp_dir().join("cargo-storage-test-hardlink");
        let _ = fs::remove_dir_all(&root);
        let target = root.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("a.bin"), "1234").unwrap();
        fs::write(target.join("b.bin"), "123456").unwrap();

        let (alone, _) = recursive_scan_target(&target);
        assert!(alone > 0);
        // Second link to the same inode, which `du` counts as nothing extra.
        fs::hard_link(target.join("a.bin"), target.join("a-link.bin")).unwrap();
        let (deduped, mtime) = recursive_scan_target(&target);
        assert_eq!(deduped, alone);
        assert!(mtime.is_some_and(|t| t > SystemTime::UNIX_EPOCH));
        // Missing path degrades to empty with no timestamp.
        assert_eq!(recursive_scan_target(root.join("nope")), (0, None));
        let _ = fs::remove_dir_all(&root);
    }
    #[cfg(unix)]
    #[test]
    fn symlinks_and_ignores_do_not_skew_measure() {
        let root = std::env::temp_dir().join("cargo-storage-test-measure-parity");
        let _ = fs::remove_dir_all(&root);
        let target = root.join("target");
        fs::create_dir_all(target.join("sub")).unwrap();
        fs::write(target.join("sub/a.bin"), "12345678").unwrap();
        let (baseline, _) = recursive_scan_target(&target);
        assert!(baseline > 0);
        // Valid link, dangling link, and linked dir: never followed, but a
        // link inode counts its own blocks like `du` (long targets spill
        // out of the inode, so this is `>=`, not `==`).
        std::os::unix::fs::symlink(target.join("sub/a.bin"), target.join("link.bin")).unwrap();
        std::os::unix::fs::symlink(root.join("nope"), target.join("dangling.bin")).unwrap();
        std::os::unix::fs::symlink(target.join("sub"), target.join("linkdir")).unwrap();
        let (with_links, _) = recursive_scan_target(&target);
        assert!(with_links >= baseline);

        // Ignore files never prune: gitignored outputs still count.
        fs::write(target.join(".gitignore"), "*.bin\n").unwrap();
        let (with_ignore, _) = recursive_scan_target(&target);
        assert!(with_ignore >= baseline);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn newest_mtime_tracks_deep_writes() {
        let root = std::env::temp_dir().join("cargo-storage-test-measure-mtime");
        let _ = fs::remove_dir_all(&root);
        let deep = root.join("target/a/b");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("a.bin"), "1234").unwrap();
        fs::write(deep.join("b.bin"), "12345678").unwrap();
        // Pin to the inode mtime instead of the wall clock.
        let expected = fs::symlink_metadata(deep.join("b.bin"))
            .unwrap()
            .modified()
            .unwrap();
        let (_, mtime) = recursive_scan_target(root.join("target"));
        assert_eq!(mtime, Some(expected));
        let _ = fs::remove_dir_all(&root);
    }
}
