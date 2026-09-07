use std::{
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant, SystemTime},
};

use rayon::prelude::*;

use crate::{
    app::App,
    scan::{self, Measurement},
};

/// Unknown dirs poll fast until they prove idle.
const UNKNOWN_INTERVAL: Duration = Duration::from_secs(10);
/// Consecutive unchanged Unknown polls before relaxing to SemiDormant.
const UNKNOWN_QUIET_LIMIT: u32 = 6;
/// Changed or deleted dirs poll fastest and never relax.
const ACTIVE_INTERVAL: Duration = Duration::from_secs(3);
/// Semi-dormant dirs proved idle once. They poll slowly.
const SEMI_INTERVAL: Duration = Duration::from_secs(30);
/// Consecutive unchanged SemiDormant polls before going Dormant.
const SEMI_QUIET_LIMIT: u32 = 6;
/// Dormant dirs poll slowest until a change wakes them.
const DORMANT_INTERVAL: Duration = Duration::from_secs(60);
/// mtime age past which a dir skips Unknown and starts Dormant.
const STALE_AFTER: Duration = Duration::from_secs(30 * 24 * 3600);

/// Owns poll tiers and the background re-measure pipeline.
pub struct Poller {
    tracked: Vec<Tracked>,
    measure_tx: mpsc::Sender<Vec<Measurement>>,
    measure_rx: mpsc::Receiver<Vec<Measurement>>,
    in_flight: bool,
}

impl Poller {
    pub fn new() -> Self {
        let (measure_tx, measure_rx) = mpsc::channel();
        Self {
            tracked: Vec::new(),
            measure_tx,
            measure_rx,
            in_flight: false,
        }
    }

    /// Rebuild tracking from fresh scan results.
    pub fn reset(&mut self, app: &App) {
        self.reset_at(app, Instant::now());
    }

    fn reset_at(&mut self, app: &App, now: Instant) {
        while self.measure_rx.try_recv().is_ok() {}
        self.in_flight = false;
        let mut dirs: Vec<(PathBuf, Option<u64>, Option<SystemTime>)> = app
            .entries
            .iter()
            .map(|e| (e.target_dir.clone(), e.size, e.last_modified))
            .collect();
        // The build cache rides along: measured entry when present, else the
        // prospective path so its arrival is picked up like any change.
        match &app.build_cache {
            Some(cache) => dirs.push((cache.project_path.clone(), cache.size, cache.last_modified)),
            None => {
                if let Some(path) = &app.build_cache_path {
                    dirs.push((path.clone(), None, None));
                }
            }
        }
        // Stagger first polls across one interval instead of one spike.
        let wall = SystemTime::now();
        let n = dirs.len().max(1) as u64;
        self.tracked = dirs
            .into_iter()
            .enumerate()
            .map(|(i, (target_dir, size, last_modified))| {
                let stale = last_modified
                    .is_some_and(|m| wall.duration_since(m).is_ok_and(|age| age >= STALE_AFTER));
                let tier = if stale {
                    Tier::Dormant
                } else {
                    Tier::Unknown(0)
                };
                let interval_ms = tier.interval().as_millis() as u64;
                Tracked {
                    target_dir,
                    tier,
                    next_due: now + Duration::from_millis((i as u64 + 1) * interval_ms / n),
                    last_size: size,
                    last_modified,
                }
            })
            .collect();
    }

    /// Measure due dirs in the background; fold finished ones into the app.
    pub fn poll(&mut self, app: &mut App) {
        let now = Instant::now();
        while let Ok(measurements) = self.measure_rx.try_recv() {
            self.in_flight = false;
            self.apply(measurements, app, now);
        }
        if self.in_flight {
            return;
        }
        let due = self.due_targets(now);
        if due.is_empty() {
            return;
        }
        let tx = self.measure_tx.clone();
        self.in_flight = true;
        std::thread::spawn(move || {
            let measurements: Vec<Measurement> = due
                .par_iter()
                .map(|dir| scan::measure_target(dir))
                .collect();
            let _ = tx.send(measurements);
        });
    }

    fn due_targets(&self, now: Instant) -> Vec<PathBuf> {
        self.tracked
            .iter()
            .filter(|t| t.next_due <= now)
            .map(|t| t.target_dir.clone())
            .collect()
    }

    /// Fold finished measurements into tiers and the app.
    fn apply(&mut self, measurements: Vec<Measurement>, app: &mut App, now: Instant) {
        app.apply_measurements(&measurements);
        for m in measurements {
            let Some(t) = self
                .tracked
                .iter_mut()
                .find(|t| t.target_dir == m.target_dir)
            else {
                continue;
            };
            // Deleted: go Active so a rebuild is caught within one short
            // interval. The zeroed size is not new activity, so the old
            // baseline stays for the recreated tree to compare against.
            // A missing path costs just an `exists` check, so the 3s
            // cadence is free while it stays gone.
            if m.last_modified.is_none() {
                t.tier = Tier::Active;
                t.next_due = now + t.tier.interval();
                continue;
            }
            if Some(m.size) != t.last_size || m.last_modified != t.last_modified {
                t.tier = Tier::Active;
                t.last_size = Some(m.size);
                t.last_modified = m.last_modified;
            } else {
                t.tier = match t.tier {
                    Tier::Unknown(c) if c + 1 >= UNKNOWN_QUIET_LIMIT => Tier::SemiDormant(0),
                    Tier::Unknown(c) => Tier::Unknown(c + 1),
                    Tier::Active => Tier::Active,
                    Tier::SemiDormant(c) if c + 1 >= SEMI_QUIET_LIMIT => Tier::Dormant,
                    Tier::SemiDormant(c) => Tier::SemiDormant(c + 1),
                    Tier::Dormant => Tier::Dormant,
                };
            }
            t.next_due = now + t.tier.interval();
        }
    }
}

impl Default for Poller {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    Unknown(u32),
    Active,
    SemiDormant(u32),
    Dormant,
}

impl Tier {
    fn interval(self) -> Duration {
        match self {
            Tier::Unknown(_) => UNKNOWN_INTERVAL,
            Tier::Active => ACTIVE_INTERVAL,
            Tier::SemiDormant(_) => SEMI_INTERVAL,
            Tier::Dormant => DORMANT_INTERVAL,
        }
    }
}

struct Tracked {
    target_dir: PathBuf,
    tier: Tier,
    next_due: Instant,
    last_size: Option<u64>,
    last_modified: Option<SystemTime>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Backing dirs for cases. Created (never written), so live dirs
    /// measure `Some` and the never-created `proj-gone` measures `None`.
    fn test_root() -> PathBuf {
        std::env::temp_dir().join("cargo-storage-test-poll")
    }

    fn target_dir(proj: &str) -> PathBuf {
        test_root().join(proj).join("target")
    }

    fn app_with_targets() -> App {
        let root = test_root();
        for proj in ["proj-a", "proj-b"] {
            std::fs::create_dir_all(root.join(proj).join("target")).unwrap();
        }
        let mut app = App::new(root);
        app.set_discovered(vec![test_root().join("proj-a"), test_root().join("proj-b")]);
        app.apply_measurements(&[measurement("proj-a", 100), measurement("proj-b", 50)]);
        app.finish_scan(None);
        // No build-cache path in tests: only the two targets are tracked.
        app.build_cache_path = None;
        app
    }

    fn missing_measurement(proj: &str) -> Measurement {
        Measurement {
            target_dir: target_dir(proj),
            size: 0,
            last_modified: None,
        }
    }

    fn measurement(proj: &str, size: u64) -> Measurement {
        static FRESH: std::sync::LazyLock<SystemTime> =
            std::sync::LazyLock::new(|| SystemTime::now() - Duration::from_secs(3600));
        Measurement {
            target_dir: target_dir(proj),
            size,
            last_modified: Some(*FRESH),
        }
    }

    fn stale_measurement(proj: &str, size: u64) -> Measurement {
        Measurement {
            target_dir: target_dir(proj),
            size,
            last_modified: Some(SystemTime::UNIX_EPOCH),
        }
    }

    fn tier_of(poller: &Poller, proj: &str) -> Tier {
        poller
            .tracked
            .iter()
            .find(|t| t.target_dir == target_dir(proj))
            .expect("dir tracked")
            .tier
    }

    #[test]
    fn change_promotes_to_active_and_active_never_relaxes() {
        let app = app_with_targets();
        let mut poller = Poller::new();
        let now = Instant::now();
        poller.reset_at(&app, now);
        assert_eq!(tier_of(&poller, "proj-a"), Tier::Unknown(0));

        // Growth promotes to Active on a 3s cadence.
        let mut app2 = app_with_targets();
        poller.apply(vec![measurement("proj-a", 200)], &mut app2, now);
        assert_eq!(tier_of(&poller, "proj-a"), Tier::Active);

        // Quiet polls never demote Active, per spec.
        for _ in 0..10 {
            let mut app2 = app_with_targets();
            poller.apply(vec![measurement("proj-a", 200)], &mut app2, now);
        }
        assert_eq!(tier_of(&poller, "proj-a"), Tier::Active);
    }

    #[test]
    fn unknown_relaxes_after_six_quiet_polls() {
        let app = app_with_targets();
        let mut poller = Poller::new();
        let now = Instant::now();
        poller.reset_at(&app, now);
        let mut app2 = app_with_targets();
        for i in 0..5 {
            poller.apply(vec![measurement("proj-a", 100)], &mut app2, now);
            assert_eq!(tier_of(&poller, "proj-a"), Tier::Unknown(i + 1));
        }
        poller.apply(vec![measurement("proj-a", 100)], &mut app2, now);
        assert_eq!(tier_of(&poller, "proj-a"), Tier::SemiDormant(0));
    }

    #[test]
    fn semidormant_relaxes_to_dormant_and_change_wakes() {
        let app = app_with_targets();
        let mut poller = Poller::new();
        let now = Instant::now();
        poller.reset_at(&app, now);
        let mut app2 = app_with_targets();
        // 6 quiet for SemiDormant, 6 more quiet for Dormant.
        for _ in 0..12 {
            poller.apply(vec![measurement("proj-a", 100)], &mut app2, now);
        }
        assert_eq!(tier_of(&poller, "proj-a"), Tier::Dormant);

        // Any change wakes straight to Active, from any tier.
        poller.apply(vec![measurement("proj-a", 101)], &mut app2, now);
        assert_eq!(tier_of(&poller, "proj-a"), Tier::Active);
    }

    #[test]
    fn due_targets_follow_tier_intervals() {
        let app = app_with_targets();
        let mut poller = Poller::new();
        let start = Instant::now();
        poller.reset_at(&app, start);
        // Staggered first polls: all due within one Unknown interval, none at once.
        assert!(poller.due_targets(start).is_empty());
        assert_eq!(poller.due_targets(start + UNKNOWN_INTERVAL).len(), 2);

        // Promote one to Active: due again 3s after its change, silent one is not.
        let mut app2 = app_with_targets();
        let change_at = start + UNKNOWN_INTERVAL;
        poller.apply(vec![measurement("proj-a", 200)], &mut app2, change_at);
        // Active honors its 3s interval from the change: at +2s only the
        // stagger-due Unknown is ready; at +3s the changed dir joins.
        assert_eq!(
            poller.due_targets(change_at + Duration::from_secs(2)),
            vec![target_dir("proj-b")]
        );
        assert!(
            poller
                .due_targets(change_at + ACTIVE_INTERVAL)
                .contains(&target_dir("proj-a"))
        );
    }

    #[test]
    fn missing_dir_goes_active_until_recreation() {
        // `proj-gone` is tracked but never created on disk.
        let root = test_root();
        let mut app = App::new(root.clone());
        app.set_discovered(vec![root.join("proj-gone")]);
        app.apply_measurements(&[measurement("proj-gone", 100)]);
        app.finish_scan(None);
        app.build_cache_path = None;
        let mut poller = Poller::new();
        let now = Instant::now();
        poller.reset_at(&app, now);
        // Deletion promotes to Active so a rebuild is caught fast, and
        // the row zeroes with no timestamp.
        poller.apply(vec![missing_measurement("proj-gone")], &mut app, now);
        assert_eq!(app.entries[0].size, Some(0));
        assert!(app.entries[0].last_modified.is_none());
        assert_eq!(tier_of(&poller, "proj-gone"), Tier::Active);
        // Still gone: stays Active on the fast cadence, baseline kept.
        poller.apply(vec![missing_measurement("proj-gone")], &mut app, now);
        assert_eq!(tier_of(&poller, "proj-gone"), Tier::Active);
        // Rebuild differs from the kept baseline: still Active, resurveyed.
        poller.apply(vec![measurement("proj-gone", 50)], &mut app, now);
        assert_eq!(tier_of(&poller, "proj-gone"), Tier::Active);
    }

    #[test]
    fn reset_restarts_tiers_with_scan_as_baseline() {
        let app = app_with_targets();
        let mut poller = Poller::new();
        let now = Instant::now();
        poller.reset_at(&app, now);
        let mut app2 = app_with_targets();
        poller.apply(vec![measurement("proj-a", 200)], &mut app2, now);
        assert_eq!(tier_of(&poller, "proj-a"), Tier::Active);
        // Manual rescan restarts at Unknown with fresh baselines.
        poller.reset_at(&app2, now);
        assert_eq!(tier_of(&poller, "proj-a"), Tier::Unknown(0));
    }

    #[test]
    fn stale_dir_skips_unknown_straight_to_dormant() {
        let root = test_root();
        std::fs::create_dir_all(root.join("proj-old").join("target")).unwrap();
        let mut app = App::new(root.clone());
        app.set_discovered(vec![root.join("proj-old")]);
        app.apply_measurements(&[stale_measurement("proj-old", 100)]);
        app.finish_scan(None);
        app.build_cache_path = None;
        let mut poller = Poller::new();
        let now = Instant::now();
        poller.reset_at(&app, now);
        assert_eq!(tier_of(&poller, "proj-old"), Tier::Dormant);
        // First poll rides the slow cadence, skipping the Unknown burst.
        assert!(poller.due_targets(now + UNKNOWN_INTERVAL).is_empty());
        assert_eq!(poller.due_targets(now + DORMANT_INTERVAL).len(), 1);
        // A change still wakes it straight to Active.
        poller.apply(vec![stale_measurement("proj-old", 200)], &mut app, now);
        assert_eq!(tier_of(&poller, "proj-old"), Tier::Active);
    }
}
