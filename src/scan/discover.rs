//! Find Rust projects and measure their output dirs. Discovery walks in
//! parallel and honors ignore files, except gitignored `target/` dirs,
//! which still measure. Projects with `build.target-dir` /
//! `build.build-dir` in `.cargo/config.toml` report those dirs instead of
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

use ignore::{DirEntry, WalkBuilder, WalkState};
use rayon::prelude::*;

use crate::util::cpu_count;

use super::{
    TargetEntry,
    cargo_config::{DiscoveredEntry, Resolver},
    measure::{Measurement, measure_target},
};

/// Scan progress messages, sent from the background thread.
#[derive(Clone, Debug)]
pub enum ScanEvent {
    /// All project/output pairs found. Sizes are still unknown.
    Discovered(Vec<DiscoveredEntry>),
    /// One output dir measured.
    Measured(Measurement),
    /// Size walk finished, with the build-cache measurement if present.
    Done { build_cache: Option<TargetEntry> },
}

/// Shared walk state: config cache, known output dirs, manifest dirs.
struct Ctx {
    resolver: Mutex<Resolver>,
    /// Non-default output dirs to skip descending into. Defaults
    /// (`<proj>/target`) are pruned by name in `keep_entry`, so this stays
    /// empty in the common case. Best effort: entries racing registration
    /// still walk once, results stay correct.
    customs: RwLock<HashSet<PathBuf>>,
    /// Set once a custom dir registers. Lets `keep_entry` skip the lock
    /// entirely while no customs exist.
    has_customs: AtomicBool,
    manifests: Mutex<Vec<PathBuf>>,
}

/// Find project/output pairs without measuring them.
#[tracing::instrument(skip_all, fields(root = %root.display()))]
pub fn discover(root: &Path) -> Vec<DiscoveredEntry> {
    discover_with(root, Resolver::new())
}

/// Test seam: disk fixtures fully determine the result.
pub(crate) fn discover_with(root: &Path, mut resolver: Resolver) -> Vec<DiscoveredEntry> {
    let customs: HashSet<PathBuf> = resolver
        .outer_dirs(root)
        .into_iter()
        .filter(|d| d != root)
        .collect();
    let ctx = Arc::new(Ctx {
        resolver: Mutex::new(resolver),
        has_customs: AtomicBool::new(!customs.is_empty()),
        customs: RwLock::new(customs),
        manifests: Mutex::new(Vec::new()),
    });
    WalkBuilder::new(root)
        // Hidden dirs may hold projects; `.git` and `.cargo` are pruned below.
        .hidden(false)
        // Apply gitignores even outside a git checkout.
        .require_git(false)
        .threads(cpu_count())
        .filter_entry({
            let ctx = Arc::clone(&ctx);
            move |entry| keep_entry(entry, &ctx)
        })
        .build_parallel()
        .run(|| {
            let ctx = Arc::clone(&ctx);
            Box::new(move |result: Result<DirEntry, ignore::Error>| visit_entry(result, &ctx))
        });
    let Ctx {
        resolver,
        customs: _,
        has_customs: _,
        manifests,
    } = Arc::try_unwrap(ctx).map_err(|_| ()).expect("walk done");
    let manifests = manifests.into_inner().unwrap_or_default();
    let mut resolver = resolver.into_inner().unwrap_or_else(|_| Resolver::new());
    let mut entries = Vec::new();
    for manifest in &manifests {
        for entry in resolver.resolve(manifest) {
            if is_target_dir(&entry.target_dir) {
                entries.push(entry);
            }
        }
    }
    entries.sort_by(|a, b| {
        a.project_path
            .cmp(&b.project_path)
            .then(a.target_dir.cmp(&b.target_dir))
    });
    // One row per output dir. Workspace members and test fixtures often
    // resolve to the same shared `target/` (e.g. via a workspace-level
    // `build.target-dir`), which would repeat its size on every row and
    // inflate totals. The owning project sorts first as a path prefix,
    // so keep the first row per dir.
    let mut seen = HashSet::new();
    entries.retain(|e| seen.insert(e.target_dir.clone()));
    tracing::info!(count = entries.len(), "discovery complete");
    entries
}

/// Discover then measure, streaming progress over `tx`.
pub fn scan_stream(root: &Path, tx: mpsc::Sender<ScanEvent>) {
    scan_stream_with(root, tx, Resolver::new());
}

/// Test seam: disk fixtures fully determine the result.
pub(crate) fn scan_stream_with(root: &Path, tx: mpsc::Sender<ScanEvent>, resolver: Resolver) {
    let projects = discover_with(root, resolver);
    if tx.send(ScanEvent::Discovered(projects.clone())).is_err() {
        return;
    }
    projects.par_iter().for_each_with(tx.clone(), |tx, entry| {
        let m = measure_target(&entry.target_dir);
        let _ = tx.send(ScanEvent::Measured(m));
    });
    let build_cache = super::cache::build_cache_entry();
    let _ = tx.send(ScanEvent::Done { build_cache });
}

/// Whether the walker yields an entry and descends into it.
fn keep_entry(entry: &DirEntry, ctx: &Ctx) -> bool {
    // Never prune the scan root itself.
    if entry.depth() == 0 {
        return true;
    }
    // Skip known cargo output dirs before paying to enumerate them. The
    // check runs on dirs only: files under a kept tree are cheap, and a
    // pruned parent hides its whole subtree. Skipped entirely until a
    // custom dir registers, so the common case pays no lock here.
    if entry.file_type().is_some_and(|kind| kind.is_dir())
        && ctx.has_customs.load(Ordering::Relaxed)
        && is_under_customs(ctx, entry.path())
        && !is_manifest_dir(entry.path())
    {
        return false;
    }
    match entry.file_name().to_str() {
        Some(".git") | Some(".cargo") => false,
        // A bare `target/` without a sibling `Cargo.toml` may hide nested
        // workspaces, so only a project's own `target/` is pruned.
        Some("target") => !is_project_target(entry.path()),
        _ => true,
    }
}

fn is_under_customs(ctx: &Ctx, path: &Path) -> bool {
    ctx.customs.read().is_ok_and(|customs| {
        // Strict ancestor in the set: same as `path != c && path.starts_with(c)`
        // but O(depth) hashes instead of a scan over every known dir.
        path.ancestors().skip(1).any(|a| customs.contains(a))
    })
}

fn is_manifest_dir(dir: &Path) -> bool {
    dir.join("Cargo.toml").is_file()
}

/// Whether `dir` is a `target/` with a sibling `Cargo.toml`.
fn is_project_target(dir: &Path) -> bool {
    dir.parent()
        .map(|parent| parent.join("Cargo.toml").is_file())
        .unwrap_or(false)
}

fn visit_entry(result: Result<DirEntry, ignore::Error>, ctx: &Ctx) -> WalkState {
    let Ok(entry) = result else {
        return WalkState::Continue;
    };
    // Symlinked dirs are never projects, and the walker never descends into them.
    if entry.path_is_symlink() || !entry.file_type().is_some_and(|kind| kind.is_dir()) {
        return WalkState::Continue;
    }
    let dir = entry.path();
    if !is_manifest_dir(dir) {
        return WalkState::Continue;
    }
    // Fast path: a local `target/` is pruned by name and needs no config
    // I/O, so record the manifest without touching the resolver or customs
    // locks. Only custom output dirs register for walk pruning.
    let default_target = dir.join("target");
    if !is_target_dir(&default_target) {
        let customs_to_add: Vec<PathBuf> = ctx
            .resolver
            .lock()
            .map(|mut resolver| {
                resolver
                    .resolve(dir)
                    .into_iter()
                    .map(|e| e.target_dir)
                    .filter(|d| *d != default_target)
                    .collect()
            })
            .unwrap_or_default();
        if !customs_to_add.is_empty() {
            if let Ok(mut customs) = ctx.customs.write() {
                customs.extend(customs_to_add);
            }
            ctx.has_customs.store(true, Ordering::Relaxed);
        }
    }
    if let Ok(mut manifests) = ctx.manifests.lock() {
        manifests.push(dir.to_path_buf());
    }
    WalkState::Continue
}

/// Whether `path` is a real output dir. Symlinks never count.
fn is_target_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|md| md.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};

    /// Fixture-only discovery for tests: the machine's global cargo config
    /// and env must not leak rows into exact-count assertions.
    fn discover(root: &Path) -> Vec<DiscoveredEntry> {
        discover_with(root, Resolver::hermetic())
    }

    fn scan_stream(root: &Path, tx: std::sync::mpsc::Sender<ScanEvent>) {
        scan_stream_with(root, tx, Resolver::hermetic())
    }

    fn setup_tree(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("cargo-shepherd-test-{name}"));
        let _ = fs::remove_dir_all(&root);
        // Real project with a target dir.
        fs::create_dir_all(root.join("proj-a/target")).unwrap();
        fs::write(root.join("proj-a/Cargo.toml"), "[package]\n").unwrap();
        fs::write(root.join("proj-a/target/blob.bin"), "hello").unwrap();
        // Cargo.toml nested under .git must not be reported.
        fs::create_dir_all(root.join("proj-a/.git")).unwrap();
        fs::write(root.join("proj-a/.git/Cargo.toml"), "[package]\n").unwrap();
        // Bare target/ without Cargo.toml is not a project.
        fs::create_dir_all(root.join("loose-target/target")).unwrap();
        // Plain dir with no Cargo.toml at all.
        fs::create_dir_all(root.join("plain")).unwrap();
        root
    }

    fn projects_of(entries: &[DiscoveredEntry]) -> Vec<PathBuf> {
        let mut projects: Vec<PathBuf> = entries.iter().map(|e| e.project_path.clone()).collect();
        projects.sort();
        projects
    }

    #[test]
    fn finds_only_projects_with_cargo_toml_and_target() {
        let root = setup_tree("find");
        let projects = discover(&root);

        assert_eq!(projects_of(&projects), vec![root.join("proj-a")]);
        assert_eq!(projects[0].target_dir, root.join("proj-a/target"));
        // Disk usage, not apparent length: blocks for the file plus its dirs.
        let m = measure_target(&root.join("proj-a/target"));
        assert!(m.size >= 5);
        assert!(m.last_modified.is_some_and(|t| t > SystemTime::UNIX_EPOCH));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn custom_target_dir_found_without_local_target() {
        let root = std::env::temp_dir().join("cargo-shepherd-test-custom-target");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("proj/.cargo")).unwrap();
        fs::write(root.join("proj/Cargo.toml"), "[package]\n").unwrap();
        fs::write(
            root.join("proj/.cargo/config.toml"),
            "[build]\ntarget-dir = \"../shared-out\"\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("shared-out/debug")).unwrap();
        fs::write(root.join("shared-out/debug/blob.bin"), "hello").unwrap();

        let projects = discover(&root);
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].project_path, root.join("proj"));
        assert_eq!(projects[0].target_dir, root.join("shared-out"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_dir_adds_a_second_row() {
        let root = std::env::temp_dir().join("cargo-shepherd-test-build-dir");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("proj/.cargo")).unwrap();
        fs::write(root.join("proj/Cargo.toml"), "[package]\n").unwrap();
        fs::write(
            root.join("proj/.cargo/config.toml"),
            "[build]\ntarget-dir = \"../tout\"\nbuild-dir = \"../bout\"\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("tout")).unwrap();
        fs::write(root.join("tout/blob.bin"), "hello").unwrap();
        fs::create_dir_all(root.join("bout")).unwrap();
        fs::write(root.join("bout/blob.bin"), "world!").unwrap();

        let mut projects = discover(&root);
        projects.sort_by(|a, b| a.target_dir.cmp(&b.target_dir));
        assert_eq!(
            projects
                .iter()
                .map(|e| e.target_dir.clone())
                .collect::<Vec<_>>(),
            vec![root.join("bout"), root.join("tout")]
        );
        assert!(projects.iter().all(|e| e.project_path == root.join("proj")));
        let kinds: Vec<super::super::OutputKind> = projects.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&super::super::OutputKind::Target));
        assert!(kinds.contains(&super::super::OutputKind::Build));
        let _ = fs::remove_dir_all(&root);
    }
    #[test]
    fn workspace_members_sharing_one_target_collapse_to_one_row() {
        let root = std::env::temp_dir().join("cargo-shepherd-test-shared-target");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("proj/.cargo")).unwrap();
        fs::write(root.join("proj/Cargo.toml"), "[workspace]\n").unwrap();
        fs::write(
            root.join("proj/.cargo/config.toml"),
            "[build]\ntarget-dir = \"target\"\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("proj/target")).unwrap();
        fs::write(root.join("proj/target/blob.bin"), "hello").unwrap();
        // Member and fixture manifests with no local target/ resolve to
        // the same workspace target dir.
        fs::create_dir_all(root.join("proj/member")).unwrap();
        fs::write(root.join("proj/member/Cargo.toml"), "[package]\n").unwrap();
        fs::create_dir_all(root.join("proj/tests/fixture")).unwrap();
        fs::write(root.join("proj/tests/fixture/Cargo.toml"), "[package]\n").unwrap();

        let projects = discover(&root);
        assert_eq!(projects.len(), 1, "one row per output dir: {projects:?}");
        assert_eq!(projects[0].project_path, root.join("proj"));
        assert_eq!(projects[0].target_dir, root.join("proj/target"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_default_target_still_listed_next_to_custom() {
        let root = std::env::temp_dir().join("cargo-shepherd-test-stale-default");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("proj/.cargo")).unwrap();
        fs::write(root.join("proj/Cargo.toml"), "[package]\n").unwrap();
        fs::write(
            root.join("proj/.cargo/config.toml"),
            "[build]\ntarget-dir = \"/nowhere-custom-xyz\"\n",
        )
        .unwrap();
        // Leftover from before the config moved: still cleanable.
        fs::create_dir_all(root.join("proj/target")).unwrap();
        fs::write(root.join("proj/target/blob.bin"), "hello").unwrap();

        let projects = discover(&root);
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].target_dir, root.join("proj/target"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_custom_dir_reports_nothing() {
        let root = std::env::temp_dir().join("cargo-shepherd-test-missing-custom");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("proj/.cargo")).unwrap();
        fs::write(root.join("proj/Cargo.toml"), "[package]\n").unwrap();
        fs::write(
            root.join("proj/.cargo/config.toml"),
            "[build]\ntarget-dir = \"/nowhere-custom-xyz\"\n",
        )
        .unwrap();

        assert!(discover(&root).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn nested_workspace_member_is_found() {
        let root = std::env::temp_dir().join("cargo-shepherd-test-nested");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("workspace/member/target")).unwrap();
        fs::write(root.join("workspace/Cargo.toml"), "[workspace]\n").unwrap();
        fs::write(root.join("workspace/member/Cargo.toml"), "[package]\n").unwrap();
        fs::write(root.join("workspace/member/target/blob.bin"), "xy").unwrap();

        let projects = discover(&root);
        assert_eq!(projects_of(&projects), vec![root.join("workspace/member")]);
        // Sanity: fixture mtime is recent (within the last hour).
        let mtime = measure_target(&root.join("workspace/member/target"))
            .last_modified
            .expect("target exists");
        assert!(
            SystemTime::now()
                .duration_since(mtime)
                .unwrap_or(Duration::MAX)
                < Duration::from_secs(3600)
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn gitignored_dirs_are_skipped_but_gitignored_targets_still_measure() {
        let root = std::env::temp_dir().join("cargo-shepherd-test-gitignore");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".gitignore"), "ignored/\ntarget/\n").unwrap();
        // Real project whose `target/` matches the ignore file.
        fs::create_dir_all(root.join("proj-b/target")).unwrap();
        fs::write(root.join("proj-b/Cargo.toml"), "[package]\n").unwrap();
        fs::write(root.join("proj-b/target/blob.bin"), "hello").unwrap();
        // Fake project under a dir the ignore file prunes. Never even walked.
        fs::create_dir_all(root.join("ignored/ghost/target")).unwrap();
        fs::write(root.join("ignored/ghost/Cargo.toml"), "[package]\n").unwrap();
        fs::write(root.join("ignored/ghost/target/blob.bin"), "hello").unwrap();

        let projects = discover(&root);
        assert_eq!(projects_of(&projects), vec![root.join("proj-b")]);
        let m = measure_target(&root.join("proj-b/target"));
        assert!(m.size >= 5);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stream_delivers_discovery_before_measurements() {
        let root = setup_tree("stream");
        let (tx, rx) = std::sync::mpsc::channel();
        scan_stream(&root, tx);
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert!(matches!(
            &events[0],
            ScanEvent::Discovered(projects) if projects_of(projects) == vec![root.join("proj-a")]
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            ScanEvent::Measured(m)
                if m.target_dir == root.join("proj-a/target") && m.size >= 5
        )));
        assert!(matches!(events.last(), Some(ScanEvent::Done { .. })));
        let _ = fs::remove_dir_all(&root);
    }
}
