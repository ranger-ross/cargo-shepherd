use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use ignore::{DirEntry, WalkBuilder};

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
#[tracing::instrument(skip_all, fields(path = %path.as_ref().display()))]
pub(super) fn recursive_scan_target<T: AsRef<Path>>(path: T) -> (u64, Option<SystemTime>) {
    let path = path.as_ref();
    if !path.exists() || path.is_symlink() {
        return (0, None);
    }
    // Serial walk with an inline fold: no channel, no extra threads. Targets
    // already measure in parallel, so per-target pools would only multiply
    // threads and buffer whole file lists in memory.
    #[cfg(unix)]
    let mut seen = HashSet::new();
    let mut total = 0u64;
    // Floor at the dir's own mtime, so an existing-but-empty dir still
    // reports a real timestamp instead of the epoch.
    let mut newest = std::fs::symlink_metadata(path)
        .and_then(|md| md.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let walk = WalkBuilder::new(path)
        .hidden(false)
        .require_git(false)
        .standard_filters(false)
        .build();
    for result in walk {
        let Ok(entry) = result else { continue };
        let Some(rec) = record_entry(&entry) else {
            continue;
        };
        // Only inodes that can repeat need dedup: files with extra links.
        // Plain files and dirs skip the set entirely.
        #[cfg(unix)]
        if rec.nlink > 1 && !seen.insert((rec.dev, rec.ino)) {
            continue;
        }
        total += rec.size;
        newest = newest.max(SystemTime::UNIX_EPOCH + Duration::from_nanos(rec.mtime_ns));
    }
    (total, Some(newest))
}

struct EntryRec {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    nlink: u64,
    size: u64,
    mtime_ns: u64,
}

fn record_entry(entry: &DirEntry) -> Option<EntryRec> {
    // lstat: a symlink records its own inode but is never followed.
    let md = std::fs::symlink_metadata(entry.path()).ok()?;
    Some(EntryRec {
        #[cfg(unix)]
        dev: md.dev(),
        #[cfg(unix)]
        ino: md.ino(),
        // Dirs are visited exactly once (symlinks are never followed and
        // dirs cannot hardlink), so they need no dedup. Only files with
        // extra links enter the set.
        #[cfg(unix)]
        nlink: if md.is_dir() { 1 } else { md.nlink() },
        size: md.blocks() * 512,
        #[cfg(not(unix))]
        size: md.len(),
        mtime_ns: md
            .modified()
            .ok()
            .and_then(|st| st.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0),
    })
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
