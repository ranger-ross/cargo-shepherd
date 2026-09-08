//! User config from `$CARGO_HOME/cargo-storage.toml`, loaded with figment.
//!
//! Current keys:
//! ```toml
//! [discovery]
//! worktrees = true
//! ```
//!
//! Every key is also settable as `CARGO_STORAGE_<SECTION>_<KEY>`, e.g.
//! `CARGO_STORAGE_DISCOVERY_WORKTREES=false`, which wins over the file.

use std::path::{Path, PathBuf};

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};

/// Top-level config. Unknown keys are ignored by the extract step.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub discovery: DiscoveryConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryConfig {
    #[serde(default = "default_worktrees")]
    pub worktrees: bool,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            worktrees: default_worktrees(),
        }
    }
}

fn default_worktrees() -> bool {
    true
}

impl Config {
    /// Load from `$CARGO_HOME/cargo-storage.toml` with `CARGO_STORAGE_*`
    /// env overrides. Missing file, missing keys, or parse errors fall
    /// back to defaults.
    pub fn load() -> Self {
        Self::load_from(&config_path())
    }

    pub(crate) fn load_from(path: &Path) -> Self {
        Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::file(path))
            .merge(Env::prefixed("CARGO_STORAGE_").split("_"))
            .extract()
            .unwrap_or_default()
    }
}

/// Effective `$CARGO_HOME`. Honors `CARGO_HOME`, then `~/.cargo`.
pub fn cargo_home() -> PathBuf {
    if let Ok(home) = std::env::var("CARGO_HOME")
        && !home.trim().is_empty()
    {
        return PathBuf::from(home);
    }
    std::env::var("HOME")
        .ok()
        .filter(|home| !home.trim().is_empty())
        .map(|home| PathBuf::from(home).join(".cargo"))
        .unwrap_or_else(|| PathBuf::from(".cargo"))
}

/// `$CARGO_HOME/cargo-storage.toml`.
pub fn config_path() -> PathBuf {
    cargo_home().join("cargo-storage.toml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Env is process-global, so every config test serializes here and
    /// declares the `CARGO_STORAGE_DISCOVERY_WORKTREES` value it needs.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_worktrees_env(value: Option<&str>, f: impl FnOnce()) {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let key = "CARGO_STORAGE_DISCOVERY_WORKTREES";
        let prior = std::env::var_os(key);
        match value {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
        f();
        match prior {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
        drop(guard);
    }

    fn write_config(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("cargo-storage.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cargo-storage-config-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_gives_defaults() {
        with_worktrees_env(None, || {
            let dir = tmpdir("missing");
            let cfg = Config::load_from(&dir.join("cargo-storage.toml"));
            assert!(cfg.discovery.worktrees);
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn worktrees_flag_round_trips() {
        with_worktrees_env(None, || {
            let dir = tmpdir("flag");
            let path = write_config(&dir, "[discovery]\nworktrees = false\n");
            assert!(!Config::load_from(&path).discovery.worktrees);
            let path = write_config(&dir, "[discovery]\nworktrees = true\n");
            assert!(Config::load_from(&path).discovery.worktrees);
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn missing_key_falls_back_to_enabled() {
        with_worktrees_env(None, || {
            let dir = tmpdir("key");
            let path = write_config(&dir, "[discovery]\n");
            assert!(Config::load_from(&path).discovery.worktrees);
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn env_disables_worktrees() {
        with_worktrees_env(Some("false"), || {
            let dir = tmpdir("env-off");
            let cfg = Config::load_from(&dir.join("cargo-storage.toml"));
            assert!(!cfg.discovery.worktrees);
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn env_wins_over_file() {
        with_worktrees_env(Some("true"), || {
            let dir = tmpdir("env-wins");
            let path = write_config(&dir, "[discovery]\nworktrees = false\n");
            assert!(Config::load_from(&path).discovery.worktrees);
            let _ = std::fs::remove_dir_all(&dir);
        });
    }
}
