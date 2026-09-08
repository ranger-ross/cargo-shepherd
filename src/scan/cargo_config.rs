//! Resolve cargo output dirs from `.cargo/config.toml` files.
//!
//! We purposefully do not follow the full Cargo config resolving rules
//! as that be really slow. We just do our best to semi handle the common
//! usecases while balancing the performance.

use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
};

/// Hierarchical resolver for target-dir / build-dir
pub struct Resolver {
    /// Lowest precedence: `$CARGO_HOME/config.toml`.
    home: Option<ConfigFile>,
    cache: HashMap<PathBuf, Option<ConfigFile>>,
    env_target: Option<PathBuf>,
    env_build: Option<PathBuf>,
    cargo_home: PathBuf,
}

impl Resolver {
    pub fn new() -> Self {
        let cargo_home = cargo_home();
        let home = config_in(&cargo_home).and_then(|path| read_config(&path));
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            home,
            cache: HashMap::new(),
            env_target: env_dir("CARGO_BUILD_TARGET_DIR")
                .or_else(|| env_dir("CARGO_TARGET_DIR"))
                .map(|p| absolutize(&cwd, &p)),
            env_build: env_dir("CARGO_BUILD_BUILD_DIR").map(|p| absolutize(&cwd, &p)),
            cargo_home,
        }
    }

    /// Test seam: no global or env layers, so disk fixtures fully
    /// determine the result regardless of the machine's cargo setup.
    #[cfg(test)]
    pub fn hermetic() -> Self {
        Self {
            home: None,
            cache: HashMap::new(),
            env_target: None,
            env_build: None,
            cargo_home: cargo_home(),
        }
    }

    /// Test seam: `$CARGO_HOME` layer with only `build.build-dir` set.
    #[cfg(test)]
    pub(crate) fn with_home_build_dir(build_dir: impl Into<String>) -> Self {
        let mut r = Self::hermetic();
        r.home = Some(ConfigFile {
            base: PathBuf::from("/home/user/.cargo"),
            target_dir: None,
            build_dir: Some(build_dir.into()),
        });
        r
    }

    /// Candidate output dirs for a manifest dir, most specific last.
    ///
    /// Always includes the default `<manifest>/target` when it differs, so
    /// stale dirs left behind by a moved config still surface for cleanup.
    /// Callers keep the candidates that exist on disk.
    ///
    /// Fast path: when the default `<manifest>/target` is a real dir, skip
    /// the ancestor `.cargo` probe chain entirely. That walk is file I/O per
    /// distinct ancestor, while the default case only needs one metadata
    /// probe. In-memory overrides (`$CARGO_HOME`, env) still apply. A project
    /// with both a local `target/` and a file-configured custom dir reports
    /// the local dir only.
    pub fn resolve(&mut self, manifest_dir: &Path) -> Vec<DiscoveredEntry> {
        let default_target = manifest_dir.join("target");
        // One probe, symlinks excluded like discovery's `is_target_dir`.
        let has_default = std::fs::symlink_metadata(&default_target).is_ok_and(|md| md.is_dir());
        let mut target = None;
        let mut target_base = None;
        let mut target_shared = false;
        let mut build = None;
        let mut build_base = None;
        let mut build_shared = false;
        if let Some(home) = &self.home {
            target = home.target_dir.clone();
            target_base = Some(home.base.clone());
            target_shared = home.target_dir.is_some();
            build = home.build_dir.clone();
            build_base = Some(home.base.clone());
            build_shared = home.build_dir.is_some();
        }
        // Ancestors from the filesystem root down: deeper configs win.
        // Skipped when the default exists: the common case pays no config I/O.
        if !has_default {
            let chain: Vec<PathBuf> = manifest_dir.ancestors().map(|a| a.to_path_buf()).collect();
            for dir in chain.iter().rev() {
                // The global file is already the lowest-precedence layer.
                // Re-applying it here would also clear the shared flag.
                if dir.join(".cargo") == self.cargo_home {
                    continue;
                }
                if let Some(cfg) = self.config_for(dir) {
                    // Clone out before touching other cache entries.
                    let (t, b, base) = (
                        cfg.target_dir.clone(),
                        cfg.build_dir.clone(),
                        cfg.base.clone(),
                    );
                    if t.is_some() {
                        target = t;
                        target_base = Some(base.clone());
                        target_shared = false;
                    }
                    if b.is_some() {
                        build = b;
                        build_base = Some(base);
                        build_shared = false;
                    }
                }
            }
        }
        let manifest = manifest_dir.to_path_buf();
        let mut out = vec![DiscoveredEntry::new(
            manifest.clone(),
            default_target.clone(),
            OutputKind::Target,
            false,
        )];
        let push =
            |out: &mut Vec<DiscoveredEntry>, dir: PathBuf, kind: OutputKind, shared: bool| {
                if !out.iter().any(|d| d.target_dir == dir) {
                    out.push(DiscoveredEntry::new(manifest.clone(), dir, kind, shared));
                }
            };
        if let Some(raw) = self.env_target.clone() {
            push(&mut out, raw, OutputKind::Target, false);
        } else if let Some(raw) = target
            && let Some(base) = target_base
        {
            push(
                &mut out,
                absolutize(&base, Path::new(&raw)),
                OutputKind::Target,
                target_shared,
            );
        }
        // `build.build-dir` defaults to the target dir, so only an explicit
        // value that templates cleanly and lands elsewhere adds a row.
        // Env wins over file configs, so an env dir is never shared.
        let (build, env_build) = match self.env_build.clone() {
            Some(dir) => (Some(dir), true),
            None => (
                build.and_then(|raw| {
                    build_base.and_then(|base| {
                        expand_build_dir(&raw, &base, manifest_dir, &self.cargo_home)
                    })
                }),
                false,
            ),
        };
        if let Some(dir) = build {
            push(&mut out, dir, OutputKind::Build, build_shared && !env_build);
        }
        out
    }

    /// Outer (scan-root level) candidates for pre-seeding walk pruning.
    /// Best effort: deeper configs may still override these per project.
    pub fn outer_dirs(&mut self, dir: &Path) -> Vec<PathBuf> {
        self.resolve(dir)
            .into_iter()
            .map(|e| e.target_dir)
            .collect()
    }

    fn config_for(&mut self, dir: &Path) -> Option<&ConfigFile> {
        if !self.cache.contains_key(dir) {
            let parsed = config_in_cargo_dir(dir).and_then(|path| read_config(&path));
            self.cache.insert(dir.to_path_buf(), parsed);
        }
        self.cache.get(dir).and_then(|opt| opt.as_ref())
    }

    #[cfg(test)]
    fn cached_files(&self) -> usize {
        self.cache.values().filter(|c| c.is_some()).count()
    }
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new()
    }
}

/// `.cargo/config.toml` under `dir`, honoring the same precedence.
fn config_in_cargo_dir(dir: &Path) -> Option<PathBuf> {
    config_in(&dir.join(".cargo"))
}

struct ConfigFile {
    /// Dir whose `.cargo/` held the file.
    base: PathBuf,
    target_dir: Option<String>,
    build_dir: Option<String>,
}

/// Which config key produced an output dir.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputKind {
    /// Default `target/` or `build.target-dir`.
    #[default]
    Target,
    /// `build.build-dir`.
    Build,
}

/// One project/output pair found by discovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredEntry {
    /// Dir holding `Cargo.toml`.
    pub project_path: PathBuf,
    /// Artifact dir: default `target/` or a configured dir.
    pub target_dir: PathBuf,
    /// Whether this dir came from `target-dir` or `build-dir`.
    pub kind: OutputKind,
    /// True when the dir came from `$CARGO_HOME/config.toml`.
    pub shared: bool,
}

impl DiscoveredEntry {
    pub fn new(project_path: PathBuf, target_dir: PathBuf, kind: OutputKind, shared: bool) -> Self {
        Self {
            project_path,
            target_dir,
            kind,
            shared,
        }
    }
}

/// Bare manifest dirs default to a sibling `target/`.
impl From<PathBuf> for DiscoveredEntry {
    fn from(project_path: PathBuf) -> Self {
        let target_dir = project_path.join("target");
        Self {
            project_path,
            target_dir,
            kind: OutputKind::Target,
            shared: false,
        }
    }
}

/// `$CARGO_HOME/config.toml`, or the legacy extensionless sibling.
/// Extensionless wins when both exist, matching cargo.
fn config_in(dir: &Path) -> Option<PathBuf> {
    let bare = dir.join("config");
    if bare.is_file() {
        return Some(bare);
    }
    let toml = dir.join("config.toml");
    toml.is_file().then_some(toml)
}

/// Read and parse the global or per-dir config file.
fn read_config(path: &Path) -> Option<ConfigFile> {
    let text = std::fs::read_to_string(path).ok()?;
    let (target_dir, build_dir) = parse_build_dirs(&text);
    if target_dir.is_none() && build_dir.is_none() {
        return None;
    }
    // Base is the parent of `.cargo`, or the home dir itself for the
    // global file which sits directly in `$CARGO_HOME`.
    let base = path
        .parent()
        .and_then(|d| {
            (d.file_name().is_some_and(|n| n == ".cargo"))
                .then(|| d.parent())
                .flatten()
        })
        .or_else(|| path.parent())
        .map(Path::to_path_buf)?;
    Some(ConfigFile {
        base,
        target_dir,
        build_dir,
    })
}

/// Extract `build.target-dir` / `build.build-dir` from config text.
/// Only the exact `[build]` table counts; `[build.x]` ends it.
fn parse_build_dirs(text: &str) -> (Option<String>, Option<String>) {
    let mut target_dir = None;
    let mut build_dir = None;
    let mut in_build = false;
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_build = section_is_build(line);
            continue;
        }
        if !in_build {
            continue;
        }
        let Some((key, rhs)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "target-dir" => target_dir = parse_string_value(rhs),
            "build-dir" => build_dir = parse_string_value(rhs),
            _ => {}
        }
    }
    (target_dir, build_dir)
}

fn section_is_build(line: &str) -> bool {
    line.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .is_some_and(|s| s.trim() == "build")
}

/// Parse a TOML basic (`".."`) or literal (`'..'`) string, dropping any
/// trailing comment. Anything else is not a path value.
fn parse_string_value(rhs: &str) -> Option<String> {
    let rhs = strip_comment(rhs).trim();
    if let Some(inner) = rhs.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = inner.chars();
        loop {
            match chars.next()? {
                '"' => return Some(out),
                '\\' => match chars.next()? {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    c => out.push(c),
                },
                c => out.push(c),
            }
        }
    }
    if let Some(inner) = rhs.strip_prefix('\'') {
        return inner.split('\'').next().map(str::to_string);
    }
    None
}

/// Cut a `#` comment, ignoring `#` inside quotes.
fn strip_comment(rhs: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    for (i, c) in rhs.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {
                if quote == Some('"') && c == '\\' {
                    escaped = true;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '#' => return rhs[..i].trim_end(),
                _ => {}
            },
        }
    }
    rhs
}

fn expand_build_dir(
    raw: &str,
    base: &Path,
    manifest_dir: &Path,
    cargo_home: &Path,
) -> Option<PathBuf> {
    if raw.contains("{workspace-path-hash}") {
        return None;
    }
    let expanded = raw
        .replace("{workspace-root}", &manifest_dir.to_string_lossy())
        .replace("{cargo-cache-home}", &cargo_home.to_string_lossy());
    if expanded.contains('{') {
        return None;
    }
    // Plain relative paths resolve against the config's dir, like other
    // config paths. Templates already expanded absolute stay as-is.
    Some(absolutize(base, Path::new(&expanded)))
}

fn absolutize(base: &Path, value: &Path) -> PathBuf {
    if value.is_absolute() {
        normalize(value)
    } else {
        normalize(&base.join(value))
    }
}

/// Lexical `.` / `..` cleanup without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

fn cargo_home() -> PathBuf {
    crate::config::cargo_home()
}

fn env_dir(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("cargo-storage-test-cfg-{name}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// Expected path for a `/unix-absolute` fixture. On Windows `/x` is
    /// drive-relative, so production joins it onto the config base's
    /// drive; resolve expectations the same way.
    fn fixture_dir(base: &Path, abs: &str) -> PathBuf {
        absolutize(base, Path::new(abs))
    }

    #[test]
    fn parses_quoted_values_and_ignores_other_tables() {
        let (target, build) = parse_build_dirs(
            "[build]\ntarget-dir = \"shared/target\" # keep\n[other]\ntarget-dir = \"nope\"\n",
        );
        assert_eq!(target.as_deref(), Some("shared/target"));
        assert_eq!(build, None);
    }

    #[test]
    fn parses_literal_strings_and_subtables_end_build() {
        let (target, build) = parse_build_dirs(
            "[build]\nbuild-dir = 'out/build'\n[build.x]\ntarget-dir = \"nope\"\n",
        );
        assert_eq!(target, None);
        assert_eq!(build.as_deref(), Some("out/build"));
    }

    #[test]
    fn ignores_hash_inside_quotes() {
        let (target, _) = parse_build_dirs("[build]\ntarget-dir = \"we#ird/target\"\n");
        assert_eq!(target.as_deref(), Some("we#ird/target"));
    }

    #[test]
    fn relative_resolves_against_cargo_parent() {
        let root = test_root("relative");
        fs::create_dir_all(root.join("proj/.cargo")).unwrap();
        fs::write(
            root.join("proj/.cargo/config.toml"),
            "[build]\ntarget-dir = \"../shared-target\"\n",
        )
        .unwrap();
        let mut r = Resolver::new();
        // Config above the temp root cannot interfere: resolve only the leaf.
        let dirs = r.resolve(&root.join("proj"));
        assert!(
            dirs.iter()
                .any(|e| e.target_dir == normalize(&root.join("shared-target")))
        );
        assert!(
            dirs.iter()
                .any(|e| e.target_dir == root.join("proj/target"))
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn deeper_config_wins_over_ancestor() {
        let root = test_root("precedence");
        fs::create_dir_all(root.join(".cargo")).unwrap();
        fs::write(
            root.join(".cargo/config.toml"),
            "[build]\ntarget-dir = \"/outer\"\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("proj/.cargo")).unwrap();
        fs::write(
            root.join("proj/.cargo/config.toml"),
            "[build]\ntarget-dir = \"/inner\"\n",
        )
        .unwrap();
        let mut r = Resolver::new();
        let dirs = r.resolve(&root.join("proj"));
        assert!(
            dirs.iter()
                .any(|e| e.target_dir == fixture_dir(&root.join("proj"), "/inner"))
        );
        assert!(
            !dirs
                .iter()
                .any(|e| e.target_dir == fixture_dir(&root, "/outer"))
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn extensionless_config_wins_and_unknown_template_skips_build() {
        let root = test_root("legacy-template");
        fs::create_dir_all(root.join("proj/.cargo")).unwrap();
        fs::write(
            root.join("proj/.cargo/config"),
            "[build]\ntarget-dir = \"/bare\"\nbuild-dir = \"out/{workspace-path-hash}\"\n",
        )
        .unwrap();
        fs::write(
            root.join("proj/.cargo/config.toml"),
            "[build]\ntarget-dir = \"/toml\"\n",
        )
        .unwrap();
        let mut r = Resolver::new();
        let dirs = r.resolve(&root.join("proj"));
        assert!(
            dirs.iter()
                .any(|e| e.target_dir == fixture_dir(&root.join("proj"), "/bare"))
        );
        assert!(
            !dirs
                .iter()
                .any(|e| e.target_dir == fixture_dir(&root.join("proj"), "/toml"))
        );
        assert_eq!(dirs.len(), 2, "hash template adds no row: {dirs:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_dir_templates_expand() {
        let root = test_root("templates");
        fs::create_dir_all(root.join("proj/.cargo")).unwrap();
        fs::write(
            root.join("proj/.cargo/config.toml"),
            "[build]\nbuild-dir = \"{workspace-root}/bdir\"\n",
        )
        .unwrap();
        let mut r = Resolver::new();
        let dirs = r.resolve(&root.join("proj"));
        assert!(dirs.iter().any(|e| e.target_dir == root.join("proj/bdir")));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn many_projects_share_one_parse() {
        let root = test_root("shared-parse");
        fs::create_dir_all(root.join(".cargo")).unwrap();
        fs::write(
            root.join(".cargo/config.toml"),
            "[build]\ntarget-dir = \"/shared\"\n",
        )
        .unwrap();
        for proj in ["a", "b", "c"] {
            fs::create_dir_all(root.join(proj)).unwrap();
        }
        let mut r = Resolver::new();
        for proj in ["a", "b", "c"] {
            let dirs = r.resolve(&root.join(proj));
            assert!(
                dirs.iter()
                    .any(|e| e.target_dir == fixture_dir(&root, "/shared"))
            );
        }
        assert_eq!(r.cached_files(), 1, "one distinct .cargo dir parsed once");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn default_target_skips_ancestor_config() {
        let root = test_root("fast-path");
        fs::create_dir_all(root.join("proj/target")).unwrap();
        fs::create_dir_all(root.join("proj/.cargo")).unwrap();
        fs::write(
            root.join("proj/.cargo/config.toml"),
            "[build]\ntarget-dir = \"/custom-fast-xyz\"\n",
        )
        .unwrap();
        let mut r = Resolver::new();
        let dirs = r.resolve(&root.join("proj"));
        assert!(
            dirs.iter()
                .any(|e| e.target_dir == root.join("proj/target"))
        );
        assert!(
            !dirs
                .iter()
                .any(|e| e.target_dir == fixture_dir(&root.join("proj"), "/custom-fast-xyz"))
        );
        assert_eq!(r.cached_files(), 0, "default target avoids config I/O");
        let _ = fs::remove_dir_all(&root);
    }

    fn home_resolver(target: Option<&str>, build: Option<&str>) -> Resolver {
        let mut r = Resolver::new();
        r.cache.clear();
        r.env_target = None;
        r.env_build = None;
        r.home = Some(ConfigFile {
            base: PathBuf::from("/home/user/.cargo"),
            target_dir: target.map(str::to_string),
            build_dir: build.map(str::to_string),
        });
        r
    }

    #[test]
    fn home_target_dir_marks_shared() {
        let root = test_root("home-shared");
        fs::create_dir_all(root.join("proj/target")).unwrap();
        let mut r = home_resolver(Some("/shared-target"), None);
        let dirs = r.resolve(&root.join("proj"));
        let shared = dirs
            .iter()
            .find(|e| e.target_dir == Path::new("/shared-target"))
            .expect("home target resolves");
        assert!(shared.shared);
        assert_eq!(shared.kind, OutputKind::Target);
        let local = dirs
            .iter()
            .find(|e| e.target_dir == root.join("proj/target"))
            .expect("default target stays");
        assert!(!local.shared);
        let _ = fs::remove_dir_all(&root);
    }
    #[test]
    fn ancestor_walk_does_not_reapply_global_config() {
        // cargo_home sits inside the test root, so the manifest's ancestor
        // chain passes through its parent. Re-applying the global file
        // there would clear the shared flag.
        let root = test_root("home-reapply");
        fs::create_dir_all(root.join(".cargo")).unwrap();
        fs::write(
            root.join(".cargo/config.toml"),
            "[build]\ntarget-dir = \"/shared-target\"\nbuild-dir = \"/shared-build\"\n",
        )
        .unwrap();
        // No local target: the full ancestor chain runs.
        fs::create_dir_all(root.join("proj")).unwrap();
        let mut r = Resolver {
            home: Some(ConfigFile {
                base: root.clone(),
                target_dir: Some("/shared-target".to_string()),
                build_dir: Some("/shared-build".to_string()),
            }),
            cache: HashMap::new(),
            env_target: None,
            env_build: None,
            cargo_home: root.join(".cargo"),
        };
        let dirs = r.resolve(&root.join("proj"));
        for want in ["/shared-target", "/shared-build"] {
            let entry = dirs
                .iter()
                .find(|e| e.target_dir == fixture_dir(&root, want))
                .expect("home dir resolves");
            assert!(entry.shared, "{want} stays shared");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn home_build_dir_marks_shared() {
        let root = test_root("home-shared-build");
        fs::create_dir_all(root.join("proj/target")).unwrap();
        let mut r = home_resolver(None, Some("/shared-build"));
        let dirs = r.resolve(&root.join("proj"));
        let shared = dirs
            .iter()
            .find(|e| e.target_dir == Path::new("/shared-build"))
            .expect("home build resolves");
        assert!(shared.shared);
        assert_eq!(shared.kind, OutputKind::Build);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn project_override_clears_shared() {
        let root = test_root("home-override");
        fs::create_dir_all(root.join("proj/.cargo")).unwrap();
        fs::write(
            root.join("proj/.cargo/config.toml"),
            "[build]\ntarget-dir = \"/inner\"\n",
        )
        .unwrap();
        let mut r = home_resolver(Some("/outer"), None);
        let dirs = r.resolve(&root.join("proj"));
        assert!(
            dirs.iter()
                .any(|e| e.target_dir == fixture_dir(&root.join("proj"), "/inner"))
        );
        let inner = dirs
            .iter()
            .find(|e| e.target_dir == fixture_dir(&root.join("proj"), "/inner"))
            .expect("project target resolves");
        assert!(!inner.shared);
        assert!(!dirs.iter().any(|e| e.target_dir == Path::new("/outer")));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn env_target_wins_and_is_not_shared() {
        let root = test_root("home-env");
        fs::create_dir_all(root.join("proj/target")).unwrap();
        let mut r = home_resolver(Some("/shared-target"), Some("/shared-build"));
        r.env_target = Some(PathBuf::from("/env-target"));
        r.env_build = Some(PathBuf::from("/env-build"));
        let dirs = r.resolve(&root.join("proj"));
        assert!(
            !dirs
                .iter()
                .any(|e| e.target_dir == Path::new("/shared-target"))
        );
        assert!(
            !dirs
                .iter()
                .any(|e| e.target_dir == Path::new("/shared-build"))
        );
        for want in ["/env-target", "/env-build"] {
            let entry = dirs
                .iter()
                .find(|e| e.target_dir == Path::new(want))
                .expect("env dir resolves");
            assert!(!entry.shared);
        }
        let _ = fs::remove_dir_all(&root);
    }
}
