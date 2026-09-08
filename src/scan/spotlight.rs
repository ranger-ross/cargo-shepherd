//! macOS Spotlight discovery for `Cargo.toml` files.
//!
//! Queries the metadata index for `Cargo.toml` files under `root`, filters
//! hits to match walk semantics (gitignores, `.git`/`.cargo` pruning, custom
//! output dirs), and records accepted manifest dirs. When hits are accepted,
//! [`super::run_collect`] skips the main filesystem walk unless
//! `CARGO_STORAGE_SPOTLIGHT_WALK=1` requests a full walk as well.

use std::path::{Component, Path};
use std::sync::Arc;

use mdquery_rs::{MDQueryBuilder, MDQueryScope};

use super::{
    Ctx, build_ignore_matcher, is_symlink_dir, record_manifest_dir, record_repo_if_present,
    spotlight_path_ok,
};

/// Env var disabling the Spotlight prefetch (`0`, `false`, `no`, `off`).
pub(crate) const ENV_DISABLE: &str = "CARGO_STORAGE_SPOTLIGHT";
/// Env var forcing a filesystem walk after Spotlight (`1`, `true`, `yes`).
pub(crate) const ENV_WALK: &str = "CARGO_STORAGE_SPOTLIGHT_WALK";

/// Query Spotlight for `Cargo.toml` under `root` and record accepted manifests.
/// Returns `true` when at least one candidate was accepted.
pub(crate) fn collect(root: &Path, ctx: &Arc<Ctx>) -> bool {
    if spotlight_disabled() {
        tracing::debug!("spotlight prefetch disabled by {ENV_DISABLE}");
        return false;
    }
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let query = MDQueryBuilder::default()
        .name_is("Cargo.toml")
        .build(vec![MDQueryScope::Custom(root.clone())], None);
    let Ok(query) = query else {
        tracing::debug!("spotlight query build failed");
        return false;
    };
    let Ok(results) = query.execute() else {
        tracing::debug!("spotlight query failed");
        return false;
    };
    if results.is_empty() {
        tracing::debug!("spotlight returned no Cargo.toml files");
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
        tracing::debug!(count = indexed, "spotlight hits were all filtered out");
        return false;
    }
    tracing::info!(indexed, accepted, "spotlight discovery complete");
    true
}

/// Whether a full filesystem walk is requested via `CARGO_STORAGE_SPOTLIGHT_WALK`.
pub(crate) fn walk_requested() -> bool {
    match std::env::var(ENV_WALK).as_deref() {
        Ok("0") | Ok("false") | Ok("no") | Ok("off") => false,
        Ok(value) if value.eq_ignore_ascii_case("false") || value.eq_ignore_ascii_case("no") => {
            false
        }
        Ok(value) if value.eq_ignore_ascii_case("off") => false,
        Ok("") => false,
        Ok(value) if is_truthy(value) => true,
        Ok(_) => false,
        Err(_) => false,
    }
}

/// Whether the Spotlight prefetch is disabled via `CARGO_STORAGE_SPOTLIGHT`.
fn spotlight_disabled() -> bool {
    match std::env::var(ENV_DISABLE).as_deref() {
        Ok("0") | Ok("false") | Ok("no") | Ok("off") => true,
        Ok(value)
            if value.eq_ignore_ascii_case("false")
                || value.eq_ignore_ascii_case("no")
                || value.eq_ignore_ascii_case("off") =>
        {
            true
        }
        _ => false,
    }
}

/// Whether `path` contains a normal path component equal to `name`.
fn path_has_component(path: &Path, name: &str) -> bool {
    path.components()
        .any(|c| matches!(c, Component::Normal(s) if s == name))
}

fn is_truthy(value: &str) -> bool {
    matches!(value, "1" | "true" | "yes" | "on" | "TRUE" | "YES" | "ON")
        || value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("yes")
        || value.eq_ignore_ascii_case("on")
}
