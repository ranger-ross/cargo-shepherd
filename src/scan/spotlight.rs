//! macOS Spotlight fast path for `Cargo.toml` discovery.
//!
//! Replaces the parallel filesystem walk when the metadata index can answer
//! `name == Cargo.toml` quickly. Results are filtered to match walk semantics
//! (gitignores, `.git`/`.cargo` pruning, custom output dirs). On query failure,
//! an empty index, or `CARGO_STORAGE_SPOTLIGHT=0`, discovery falls back to walk.

use std::path::{Component, Path};
use std::sync::Arc;

use mdquery_rs::{MDQueryBuilder, MDQueryScope};

use super::{
    Ctx, build_ignore_matcher, is_symlink_dir, record_manifest_dir, record_repo_if_present,
    spotlight_path_ok,
};

/// Env var disabling the Spotlight fast path (`0`, `false`, `no`, `off`).
pub(crate) const ENV_DISABLE: &str = "CARGO_STORAGE_SPOTLIGHT";

/// Collect manifest dirs under `root` via Spotlight. Returns `true` when the
/// index was queried and at least one candidate was accepted.
pub(crate) fn collect(root: &Path, ctx: &Arc<Ctx>) -> bool {
    if spotlight_disabled() {
        tracing::debug!("spotlight discovery disabled by {ENV_DISABLE}");
        return false;
    }
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let query = MDQueryBuilder::default()
        .name_is("Cargo.toml")
        .build(vec![MDQueryScope::Custom(root.clone())], None);
    let Ok(query) = query else {
        tracing::debug!("spotlight query build failed, falling back to walk");
        return false;
    };
    let Ok(results) = query.execute() else {
        tracing::debug!("spotlight query failed, falling back to walk");
        return false;
    };
    if results.is_empty() {
        tracing::debug!("spotlight returned no Cargo.toml files, falling back to walk");
        return false;
    }

    let indexed = results.len();
    let mut cargo_tomls: Vec<_> = results.into_iter().filter_map(|item| item.path()).collect();
    // Shallow paths first so custom output dirs register before descendants.
    cargo_tomls.sort_by_key(|p| p.components().count());
    let mut matcher = build_ignore_matcher(&root);
    let mut accepted = 0usize;
    for cargo_toml in cargo_tomls {
        let Some(manifest_dir) = cargo_toml.parent() else {
            continue;
        };
        if path_has_component(manifest_dir, ".git") || path_has_component(manifest_dir, ".cargo") {
            continue;
        }
        if is_symlink_dir(manifest_dir) {
            continue;
        }
        if !manifest_dir.starts_with(&root) {
            continue;
        }
        if !spotlight_path_ok(&root, manifest_dir, ctx, &mut matcher) {
            continue;
        }
        for ancestor in manifest_dir.ancestors() {
            if ancestor.starts_with(&root) {
                record_repo_if_present(ancestor, ctx);
            }
        }
        record_manifest_dir(manifest_dir, ctx);
        accepted += 1;
    }
    if accepted == 0 {
        tracing::debug!(
            count = indexed,
            "spotlight hits were all filtered out, falling back to walk"
        );
        return false;
    }
    tracing::info!(indexed, accepted, "spotlight discovery complete");
    true
}

fn spotlight_disabled() -> bool {
    match std::env::var(ENV_DISABLE).as_deref() {
        Ok("0") | Ok("false") | Ok("no") | Ok("off") | Ok("FALSE") | Ok("OFF") => true,
        Ok(value) if value.eq_ignore_ascii_case("false") || value.eq_ignore_ascii_case("no") => {
            true
        }
        _ => false,
    }
}

fn path_has_component(path: &Path, name: &str) -> bool {
    path.components()
        .any(|c| matches!(c, Component::Normal(s) if s == name))
}
