//! Named container volumes as extra scan roots.
//!
//! Listing goes through the `docker` (or `podman`) CLI so remote daemons,
//! rootless storage, and `DOCKER_HOST` keep working. Only mountpoints that
//! are local dirs are walked. The rest are reported as warnings.

use std::path::PathBuf;
use std::process::Command;

/// One named volume and its host-side mountpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DockerVolume {
    pub name: String,
    pub mountpoint: PathBuf,
}

/// One walk root. `volume` is the volume name for container roots,
/// `None` for the plain host root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanRoot {
    pub path: PathBuf,
    pub volume: Option<String>,
}

impl ScanRoot {
    pub fn host(path: PathBuf) -> Self {
        Self { path, volume: None }
    }
}

impl From<PathBuf> for ScanRoot {
    fn from(path: PathBuf) -> Self {
        Self::host(path)
    }
}

/// List all named volumes via the container runtime.
pub fn list_volumes() -> eyre::Result<Vec<DockerVolume>> {
    let rt = select_runtime()?;
    let ls = Command::new(rt)
        .args(["volume", "ls", "-q"])
        .output()
        .map_err(|e| eyre::eyre!("running `{rt} volume ls`: {e}"))?;
    if !ls.status.success() {
        let detail = String::from_utf8_lossy(&ls.stderr).trim().to_string();
        eyre::bail!("`{rt} volume ls` failed: {detail}");
    }
    let ls_text = String::from_utf8_lossy(&ls.stdout).into_owned();
    let names: Vec<&str> = ls_text
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let mut cmd = Command::new(rt);
    cmd.args([
        "volume",
        "inspect",
        "--format",
        "{{.Name}}\t{{.Mountpoint}}",
    ]);
    for name in &names {
        cmd.arg(name);
    }
    let out = cmd
        .output()
        .map_err(|e| eyre::eyre!("running `{rt} volume inspect`: {e}"))?;
    if !out.status.success() {
        let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
        eyre::bail!("`{rt} volume inspect` failed: {detail}");
    }
    Ok(parse_inspect_output(&String::from_utf8_lossy(&out.stdout)))
}

/// Split volume roots into walkable dirs and human-readable warnings.
/// Non-local (remote daemon, Desktop VM) and unreadable mountpoints land
/// in warnings so one bad volume never fails the whole scan.
pub fn usable_roots(volumes: &[DockerVolume]) -> (Vec<ScanRoot>, Vec<String>) {
    let mut roots = Vec::new();
    let mut skipped = Vec::new();
    for vol in volumes {
        if vol.mountpoint.is_dir() {
            roots.push(ScanRoot {
                path: vol.mountpoint.clone(),
                volume: Some(vol.name.clone()),
            });
            continue;
        }
        skipped.push(skip_reason(vol));
    }
    (roots, skipped)
}

fn skip_reason(vol: &DockerVolume) -> String {
    match std::fs::symlink_metadata(&vol.mountpoint) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => format!(
            "--docker: skipping volume `{}`: permission denied on {} (try sudo)",
            vol.name,
            vol.mountpoint.display()
        ),
        Err(e) => format!(
            "--docker: skipping volume `{}`: cannot access {} ({e}); \
             the daemon may be remote",
            vol.name,
            vol.mountpoint.display()
        ),
        Ok(_) => format!(
            "--docker: skipping volume `{}`: not a directory: {}",
            vol.name,
            vol.mountpoint.display()
        ),
    }
}

/// Prefer docker, fall back to podman only when docker is not installed.
/// A present-but-broken docker (daemon down) reports its own error rather
/// than silently returning podman's volumes.
fn select_runtime() -> eyre::Result<&'static str> {
    if have_binary("docker") {
        return Ok("docker");
    }
    if have_binary("podman") {
        return Ok("podman");
    }
    eyre::bail!("--docker needs a container runtime: neither `docker` nor `podman` found in PATH")
}

fn have_binary(rt: &str) -> bool {
    Command::new(rt).arg("--version").output().is_ok()
}

/// Parse `volume inspect --format` lines. Malformed lines are skipped.
fn parse_inspect_output(text: &str) -> Vec<DockerVolume> {
    let mut volumes = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (name, mount) = match line.split_once('\t').or_else(|| line.split_once(' ')) {
            Some(pair) => pair,
            None => continue,
        };
        let (name, mount) = (name.trim(), mount.trim());
        if name.is_empty() || mount.is_empty() {
            continue;
        }
        volumes.push(DockerVolume {
            name: name.to_string(),
            mountpoint: PathBuf::from(mount),
        });
    }
    volumes.sort_by(|a, b| a.name.cmp(&b.name));
    volumes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tabbed_inspect_lines_in_name_order() {
        let out = parse_inspect_output(
            "zeta\t/var/lib/docker/volumes/zeta/_data\n\
             alpha\t/var/lib/docker/volumes/alpha/_data\n",
        );
        assert_eq!(
            out.iter().map(|v| v.name.clone()).collect::<Vec<_>>(),
            vec!["alpha".to_string(), "zeta".to_string()]
        );
        assert_eq!(
            out[0].mountpoint,
            PathBuf::from("/var/lib/docker/volumes/alpha/_data")
        );
    }

    #[test]
    fn skips_blank_and_malformed_lines() {
        let out = parse_inspect_output("lonely-name\n\nok\t/data\n  \n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "ok");
    }

    #[test]
    fn usable_roots_keeps_local_dirs_and_warns_on_missing() {
        let root = std::env::temp_dir().join("cargo-shepherd-test-docker-roots");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("vol-a/_data")).unwrap();
        let volumes = vec![
            DockerVolume {
                name: "vol-a".to_string(),
                mountpoint: root.join("vol-a/_data"),
            },
            DockerVolume {
                name: "gone".to_string(),
                mountpoint: root.join("gone/_data"),
            },
        ];
        let (roots, skipped) = usable_roots(&volumes);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].volume.as_deref(), Some("vol-a"));
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].contains("gone"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
