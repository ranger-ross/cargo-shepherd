//! Find Rust projects and measure their `target/` dirs. Discovery walks in
//! parallel and honors ignore files, except gitignored `target/` dirs,
//! which still measure, and linked git worktrees, which are walked even
//! from inside gitignored dirs.

mod cache;
mod cargo_config;
mod discover;
mod measure;

pub use cache::{build_cache_entry, build_cache_path};
pub use cargo_config::{DiscoveredEntry, OutputKind};
pub use discover::{ScanEvent, discover, scan_stream};
pub use measure::{Measurement, measure_target};

use std::{
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

#[derive(Clone, Debug)]
pub struct TargetEntry {
    /// Dir holding `Cargo.toml`.
    pub project_path: PathBuf,
    /// Measured artifact dir. Defaults to `project_path/target`; a
    /// `build.target-dir` / `build.build-dir` config points elsewhere.
    pub target_dir: PathBuf,
    /// Whether this dir came from `target-dir` or `build-dir`.
    pub kind: OutputKind,
    /// True when the dir came from `$CARGO_HOME/config.toml`.
    pub shared: bool,
    /// Disk usage of `target_dir` in bytes, `du` semantics. `None` while
    /// the size walk has not measured this entry yet.
    pub size: Option<u64>,
    /// Newest mtime under `target_dir`, or `None` while pending or deleted.
    pub last_modified: Option<SystemTime>,
    /// True while the target directory is queued for deletion or being deleted.
    pub is_under_deletion: bool,
}

impl TargetEntry {
    pub fn project_name(&self) -> String {
        self.project_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }

    /// PROJECT cell text. A linked git worktree reads as `main (tree)`;
    /// the TUI highlights the `(tree)` tag. Everything else is the dir name.
    pub fn display_name(&self) -> String {
        match self.worktree_names() {
            Some((main, tree)) => format!("{main} ({tree})"),
            None => self.project_name(),
        }
    }

    /// `(main project name, worktree name)` when a linked-worktree `.git`
    /// file (`gitdir: <main>/.git/worktrees/<tree>`) is found on
    /// `project_path` or an ancestor. Plain repos (`.git` dir), submodules
    /// (`.../modules/...`), and unreadable pointers read as `None`.
    pub fn worktree_names(&self) -> Option<(String, String)> {
        let mut dir = Some(self.project_path.as_path());
        while let Some(current) = dir {
            let git_path = current.join(".git");
            if git_path.is_dir() {
                return None;
            }
            if git_path.is_file() {
                return Self::parse_worktree_git_file(&git_path, current);
            }
            dir = current.parent();
        }
        None
    }

    fn parse_worktree_git_file(git_path: &Path, git_base: &Path) -> Option<(String, String)> {
        let text = std::fs::read_to_string(git_path).ok()?;
        let raw = text.strip_prefix("gitdir:")?.trim();
        if raw.is_empty() {
            return None;
        }
        let joined = if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            git_base.join(raw)
        };
        // Lexical cleanup so `..` in relative gitdirs cannot leak into names.
        let mut cleaned = PathBuf::new();
        for comp in joined.components() {
            match comp {
                Component::ParentDir => {
                    cleaned.pop();
                }
                Component::CurDir => {}
                _ => cleaned.push(comp),
            }
        }
        let mut comps = cleaned.components();
        let mut main = PathBuf::new();
        let mut found = false;
        for comp in comps.by_ref() {
            if matches!(comp, Component::Normal(name) if name.to_string_lossy() == ".git") {
                found = true;
                break;
            }
            main.push(comp);
        }
        if !found {
            return None;
        }
        let is_worktree = matches!(comps.next(), Some(Component::Normal(name)) if name.to_string_lossy() == "worktrees");
        let tree = match comps.next() {
            Some(Component::Normal(name)) => name.to_string_lossy().into_owned(),
            _ => return None,
        };
        if !is_worktree {
            return None;
        }
        let main_name = main.file_name()?.to_string_lossy().into_owned();
        if main_name.is_empty() {
            return None;
        }
        Some((main_name, tree))
    }

    /// Display name for a shared `$CARGO_HOME` dir, if this entry is one.
    pub fn shared_label(&self) -> Option<&'static str> {
        if !self.shared {
            return None;
        }
        match self.kind {
            OutputKind::Target => Some("Shared Target Dir"),
            OutputKind::Build => Some("Shared Build Dir"),
        }
    }
}

pub fn resolve_root(raw: &Path) -> PathBuf {
    std::fs::canonicalize(raw).unwrap_or_else(|_| raw.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn entry_at(dir: &Path) -> TargetEntry {
        TargetEntry {
            project_path: dir.to_path_buf(),
            target_dir: dir.join("target"),
            kind: OutputKind::Target,
            shared: false,
            size: None,
            last_modified: None,
            is_under_deletion: false,
        }
    }

    #[test]
    fn linked_worktree_labels_main_and_tree() {
        let root = std::env::temp_dir().join("cargo-storage-test-wt-label");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("trees/t1")).unwrap();
        fs::write(
            root.join("trees/t1/.git"),
            format!("gitdir: {}/main/.git/worktrees/t1\n", root.display()),
        )
        .unwrap();
        let entry = entry_at(&root.join("trees/t1"));
        assert_eq!(
            entry.worktree_names(),
            Some(("main".to_string(), "t1".to_string()))
        );
        // Main name comes from the gitdir, not the checkout dir.
        assert_eq!(entry.display_name(), "main (t1)");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn nested_crate_in_linked_worktree_labels_main_and_tree() {
        let root = std::env::temp_dir().join("cargo-storage-test-wt-nested");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("trees/t1/crates/foo")).unwrap();
        fs::write(
            root.join("trees/t1/.git"),
            format!("gitdir: {}/main/.git/worktrees/t1\n", root.display()),
        )
        .unwrap();
        let entry = entry_at(&root.join("trees/t1/crates/foo"));
        assert_eq!(
            entry.worktree_names(),
            Some(("main".to_string(), "t1".to_string()))
        );
        assert_eq!(entry.display_name(), "main (t1)");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn submodule_and_plain_dirs_keep_dir_name() {
        let root = std::env::temp_dir().join("cargo-storage-test-wt-plain");
        let _ = fs::remove_dir_all(&root);
        // Submodule-style pointer targets `modules/`, not `worktrees/`.
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
        // Plain repo: `.git` is a dir, unreadable as a pointer file.
        fs::create_dir_all(root.join("main/.git")).unwrap();
        fs::create_dir_all(root.join("bare")).unwrap();

        for (dir, name) in [
            (root.join("sub"), "sub"),
            (root.join("main"), "main"),
            (root.join("bare"), "bare"),
        ] {
            let entry = entry_at(&dir);
            assert_eq!(entry.worktree_names(), None);
            assert_eq!(entry.display_name(), name);
        }
        let _ = fs::remove_dir_all(&root);
    }
}
