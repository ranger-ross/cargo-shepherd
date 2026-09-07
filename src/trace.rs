use std::path::PathBuf;

use tracing_chrome::ChromeLayerBuilder;
use tracing_subscriber::prelude::*;

/// Env var gating Perfetto-compatible trace output.
pub const ENV_VAR: &str = "CARGO_STORAGE_TRACING";
/// Trace path used when the env var enables tracing without naming a file.
pub const DEFAULT_PATH: &str = "cargo-storage-trace.json";

/// Held for the program lifetime. Dropping it flushes the trace file.
pub struct TraceGuard {
    _guard: tracing_chrome::FlushGuard,
    pub path: PathBuf,
}

/// Enable Chrome-trace output
pub fn init() -> Option<TraceGuard> {
    let raw = std::env::var(ENV_VAR).ok()?;
    let value = raw.trim();
    if value.is_empty() || is_falsy(value) {
        return None;
    }
    let path = if is_truthy(value) {
        PathBuf::from(DEFAULT_PATH)
    } else {
        PathBuf::from(value)
    };
    let (layer, guard) = ChromeLayerBuilder::new()
        .file(path.clone())
        .include_args(true)
        .include_locations(true)
        .build();
    let _ = tracing_subscriber::registry().with(layer).try_init();
    eprintln!("CARGO_STORAGE_TRACING: writing trace to {}", path.display());
    Some(TraceGuard {
        _guard: guard,
        path,
    })
}

fn is_truthy(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn is_falsy(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}
