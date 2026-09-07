//! Find Rust projects and measure their `target/` dirs. Discovery walks in
//! parallel and honors ignore files, except gitignored `target/` dirs,
//! which still measure.

mod cache;
mod cargo_config;
mod discover;
mod docker;
mod measure;

pub use cache::{build_cache_entry, build_cache_path};
pub use cargo_config::{DiscoveredEntry, OutputKind};
pub use discover::{ScanEvent, discover_roots, scan_stream_roots};
pub use docker::{ScanRoot, list_volumes, usable_roots};
pub use measure::{Measurement, measure_target};

use std::{
    path::{Path, PathBuf},
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
    /// Containing volume name for `--docker` hits, else `None`.
    pub volume: Option<String>,
    /// Disk usage of `target_dir` in bytes, `du` semantics. `None` while
    /// the size walk has not measured this entry yet.
    pub size: Option<u64>,
    /// Newest mtime under `target_dir`, or `None` while pending or deleted.
    pub last_modified: Option<SystemTime>,
}

impl TargetEntry {
    pub fn project_name(&self) -> String {
        self.project_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
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

/// Walk roots for a scan: the host root plus locally accessible volume
/// mountpoints when `include_docker` is set. Skipped volumes warn on
/// stderr. Daemon failures bail so `--docker` never silently scans host only.
pub fn resolve_scan_roots(host: &Path, include_docker: bool) -> eyre::Result<Vec<ScanRoot>> {
    let mut roots = vec![ScanRoot::host(resolve_root(host))];
    if !include_docker {
        return Ok(roots);
    }
    let volumes = list_volumes()?;
    if volumes.is_empty() {
        eprintln!("--docker: no container volumes found");
        return Ok(roots);
    }
    let (mut usable, skipped) = usable_roots(&volumes);
    for line in &skipped {
        eprintln!("{line}");
    }
    for root in &mut usable {
        root.path = resolve_root(&root.path);
    }
    roots.append(&mut usable);
    Ok(roots)
}
