use std::{path::PathBuf, time::Instant};

use ratatui::widgets::TableState;
use regex::Regex;

use crate::scan::{DiscoveredEntry, Measurement, TargetEntry, build_cache_path};

pub struct App {
    pub root: PathBuf,
    /// Kept sorted by `target_dir`. Sorting and filtering only affect `visible_indices()`.
    pub entries: Vec<TargetEntry>,
    pub build_cache: Option<TargetEntry>,
    pub build_cache_path: Option<PathBuf>,
    pub total_size: u64,
    pub table_state: TableState,
    pub scanning: bool,
    pub sort: SortKey,
    pub loading_start: Instant,
    /// Visible table rows from the last render. Paging follows it.
    pub page_len: usize,
    /// True once the user moves selection. Suppresses the auto-jump to top.
    pub navigated: bool,
    /// True while the user is typing a filter pattern.
    pub filtering: bool,
    pub filter_text: String,
    /// Last filter that compiled. Matches against name and path.
    pub filter_regex: Option<Regex>,
    /// First line of the latest regex error, if the text does not compile.
    pub filter_error: Option<String>,
    /// Last delete failure, cleared on the next successful delete.
    pub delete_error: Option<String>,
}

impl App {
    pub fn new(root: PathBuf) -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));
        Self {
            root,
            entries: Vec::new(),
            build_cache: None,
            build_cache_path: build_cache_path(),
            total_size: 0,
            table_state,
            scanning: true,
            sort: SortKey::default(),
            loading_start: Instant::now(),
            page_len: 10,
            navigated: false,
            filtering: false,
            filter_text: String::new(),
            filter_regex: None,
            filter_error: None,
            delete_error: None,
        }
    }

    /// Merge the results of a scan into `entries`, keeping them sorted.
    ///
    /// Incoming `DiscoveredEntry` items are deduplicated on `target_path`.
    ///
    /// Current `TargetEntry` and incoming `DiscoveredEntry` items are joined on `target_path`.
    /// If a `DiscoveredEntry` has *no* matching `TargetEntry`, it is simply added as new.
    /// If a `DiscoveredEntry` *does* have an existing `TargetEntry`, metadata e.g. `size` are kept.
    /// If a `TargetEntry` no longer has a matching `DiscoveredEntry`, it is removed.
    pub fn set_discovered(&mut self, discovered: Vec<impl Into<DiscoveredEntry>>) {
        // Dedup incoming entries.
        let mut discovered: Vec<DiscoveredEntry> = discovered.into_iter().map(Into::into).collect();
        discovered.sort_by(|a, b| a.target_dir.cmp(&b.target_dir));
        discovered.dedup_by(|a, b| a.target_dir == b.target_dir);

        // Merge incoming entries, remove stale entries, and keep metadata where possible.
        let mut old_entries = std::mem::take(&mut self.entries).into_iter().peekable();
        let mut discovered = discovered.into_iter().peekable();
        let mut entries = Vec::with_capacity(old_entries.len() + discovered.len());
        loop {
            match (old_entries.peek(), discovered.peek()) {
                (Some(old), Some(found)) => match old.target_dir.cmp(&found.target_dir) {
                    std::cmp::Ordering::Less => {
                        // The target no longer appears in discovery.
                        old_entries.next();
                    }
                    std::cmp::Ordering::Greater => {
                        let found = discovered.next().expect("peeked entry exists");
                        entries.push(TargetEntry {
                            project_path: found.project_path,
                            target_dir: found.target_dir,
                            kind: found.kind,
                            shared: found.shared,
                            size: None,
                            last_modified: None,
                        });
                    }
                    std::cmp::Ordering::Equal => {
                        let mut entry = old_entries.next().expect("peeked entry exists");
                        let found = discovered.next().expect("peeked entry exists");
                        entry.project_path = found.project_path;
                        entry.kind = found.kind;
                        entry.shared = found.shared;
                        entries.push(entry);
                    }
                },
                (None, Some(_)) => {
                    let found = discovered.next().expect("peeked entry exists");
                    entries.push(TargetEntry {
                        project_path: found.project_path,
                        target_dir: found.target_dir,
                        kind: found.kind,
                        shared: found.shared,
                        size: None,
                        last_modified: None,
                    });
                }
                (Some(_), None) => {
                    // The remaining targets no longer appear in discovery.
                    break;
                }
                (None, None) => break,
            }
        }
        self.total_size = entries.iter().filter_map(|e| e.size).sum();
        self.entries = entries;
        self.scanning = true;
        self.clamp_selection();
        if !self.navigated {
            self.top_unmarked();
        }
    }

    /// End the size walk. Measured rows keep their sizes; entries still
    /// pending show `Loading...` until the poller measures them.
    pub fn finish_scan(&mut self, build_cache: Option<TargetEntry>) {
        self.build_cache = build_cache;
        self.scanning = false;
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        // Keep selection in bounds after rescan.
        let visible = self.visible_indices().len();
        if visible == 0 {
            self.table_state.select(None);
        } else {
            let selected = self.table_state.selected().unwrap_or(0);
            self.table_state.select(Some(selected.min(visible - 1)));
        }
    }

    pub fn cycle_sort(&mut self) {
        self.sort = self.sort.next();
    }

    /// Replace the filter text; an invalid pattern keeps the last good one.
    pub fn set_filter(&mut self, text: String) {
        self.filter_text = text;
        if self.filter_text.is_empty() {
            self.filter_regex = None;
            self.filter_error = None;
        } else {
            match Regex::new(&self.filter_text) {
                Ok(re) => {
                    self.filter_regex = Some(re);
                    self.filter_error = None;
                }
                Err(e) => {
                    self.filter_error = Some(
                        e.to_string()
                            .lines()
                            .next()
                            .unwrap_or("invalid regex")
                            .to_string(),
                    );
                }
            }
        }
        self.top();
    }

    pub fn visible_indices(&self) -> Vec<usize> {
        let mut visible: Vec<usize> = match &self.filter_regex {
            None => (0..self.entries.len()).collect(),
            Some(re) => self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| {
                    re.is_match(&e.project_name())
                        || re.is_match(&e.display_name())
                        || re.is_match(&e.project_path.to_string_lossy())
                        || re.is_match(&e.target_dir.to_string_lossy())
                })
                .map(|(i, _)| i)
                .collect(),
        };
        visible.sort_by(|&a, &b| self.compare_entries(&self.entries[a], &self.entries[b]));
        visible
    }

    /// The row at a position in the sorted, filtered table.
    pub fn visible_entry(&self, position: usize) -> Option<&TargetEntry> {
        self.visible_indices()
            .get(position)
            .and_then(|&index| self.entries.get(index))
    }

    #[tracing::instrument(skip_all)]
    pub fn apply_measurements(&mut self, measurements: &[Measurement]) {
        if measurements.is_empty() {
            return;
        }
        let selected_path = self
            .table_state
            .selected()
            .and_then(|i| self.visible_indices().get(i).copied())
            .and_then(|i| self.entries.get(i))
            .map(|e| e.target_dir.clone());
        for m in measurements {
            if let Some(entry) = self
                .entries
                .iter_mut()
                .find(|e| e.target_dir == m.target_dir)
            {
                entry.size = Some(m.size);
                entry.last_modified = m.last_modified;
            } else if let Some(cache) = &mut self.build_cache
                && cache.project_path == m.target_dir
            {
                cache.size = Some(m.size);
                cache.last_modified = m.last_modified;
            } else if Some(&m.target_dir) == self.build_cache_path.as_ref()
                && m.last_modified.is_some()
            {
                // The build cache arrived after startup, so give it a row now.
                // The poller tracks it from the next reset.
                self.build_cache = Some(TargetEntry {
                    project_path: m.target_dir.clone(),
                    target_dir: m.target_dir.clone(),
                    kind: crate::scan::OutputKind::Target,
                    shared: false,
                    size: Some(m.size),
                    last_modified: m.last_modified,
                });
            }
        }
        self.total_size = self.entries.iter().filter_map(|e| e.size).sum();
        if !self.navigated {
            // Sizes churn the order while the walk runs. Stay glued to the
            // top until the user takes over.
            self.top_unmarked();
        } else if let Some(path) = selected_path {
            let visible = self.visible_indices();
            let pos = visible
                .iter()
                .position(|&i| self.entries.get(i).is_some_and(|e| e.target_dir == path));
            self.table_state
                .select(pos.or_else(|| visible.first().map(|_| 0)));
        }
    }

    /// Start a load and restart the shimmer sweep.
    pub fn begin_scan(&mut self) {
        self.scanning = true;
        self.loading_start = Instant::now();
        self.navigated = false;
    }

    pub fn next(&mut self) {
        self.navigated = true;
        let visible = self.visible_indices().len();
        if visible == 0 {
            return;
        }
        let i = self.table_state.selected().unwrap_or(0);
        self.table_state.select(Some((i + 1) % visible));
    }

    pub fn previous(&mut self) {
        self.navigated = true;
        let visible = self.visible_indices().len();
        if visible == 0 {
            return;
        }
        let i = self.table_state.selected().unwrap_or(0);
        self.table_state
            .select(Some(i.checked_sub(1).unwrap_or(visible - 1)));
    }

    pub fn top(&mut self) {
        self.navigated = true;
        if !self.visible_indices().is_empty() {
            self.table_state.select(Some(0));
        }
    }

    pub fn bottom(&mut self) {
        self.navigated = true;
        let visible = self.visible_indices().len();
        if visible > 0 {
            self.table_state.select(Some(visible - 1));
        }
    }

    /// Jump forward one screen. Clamps at the end, never wraps.
    pub fn page_down(&mut self) {
        self.navigated = true;
        let visible = self.visible_indices().len();
        if visible == 0 {
            return;
        }
        let i = self.table_state.selected().unwrap_or(0);
        self.table_state
            .select(Some((i + self.page_len.max(1)).min(visible - 1)));
    }

    /// Jump back one screen. Clamps at the start, never wraps.
    pub fn page_up(&mut self) {
        self.navigated = true;
        if self.visible_indices().is_empty() {
            return;
        }
        let i = self.table_state.selected().unwrap_or(0);
        self.table_state
            .select(Some(i.saturating_sub(self.page_len.max(1))));
    }

    /// Delete the selected project's `target/` dir. A missing dir
    /// counts as deleted. Failures surface in `delete_error`.
    /// Selection moves to the row below. Deleting the last row
    /// keeps selection. A lone row keeps selection.
    pub fn delete_selected(&mut self) {
        self.navigated = true;
        let visible = self.visible_indices();
        let sel = self.table_state.selected();
        let entry_idx = sel.and_then(|i| visible.get(i).copied());
        let Some(entry_idx) = entry_idx else { return };
        let Some(target_dir) = self.entries.get(entry_idx).map(|e| e.target_dir.clone()) else {
            return;
        };
        // Neighbor below by identity, so the resort below cannot lose it.
        // No row below keeps the current selection.
        let neighbor_idx = match sel {
            Some(i) => visible.get(i + 1).copied(),
            None => None,
        };
        let neighbor = neighbor_idx.and_then(|i| self.entries.get(i).map(|e| e.target_dir.clone()));
        match std::fs::remove_dir_all(&target_dir) {
            Ok(()) => self.delete_error = None,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.delete_error = None,
            Err(e) => {
                self.delete_error = Some(format!("delete {}: {e}", target_dir.display()));
                return;
            }
        }
        if let Some(entry) = self.entries.iter_mut().find(|e| e.target_dir == target_dir) {
            entry.size = Some(0);
            entry.last_modified = None;
        }
        self.total_size = self.entries.iter().filter_map(|e| e.size).sum();
        if neighbor.is_none() {
            return;
        }
        let visible = self.visible_indices();
        let pos = visible.iter().position(|&i| {
            self.entries
                .get(i)
                .is_some_and(|e| Some(&e.target_dir) == neighbor.as_ref())
        });
        self.table_state
            .select(pos.or_else(|| visible.first().map(|_| 0)));
    }

    /// Select the first row without counting as user navigation, so the
    /// scan can settle on top while the user is idle.
    fn top_unmarked(&mut self) {
        if self.visible_indices().is_empty() {
            self.table_state.select(None);
        } else {
            self.table_state.select(Some(0));
        }
    }

    fn compare_entries(&self, a: &TargetEntry, b: &TargetEntry) -> std::cmp::Ordering {
        match self.sort {
            // Pending rows float above measured ones: big dirs take
            // longest to measure, so they stay visible while loading.
            SortKey::Size => match (a.size, b.size) {
                (Some(x), Some(y)) => y.cmp(&x),
                (Some(_), None) => std::cmp::Ordering::Greater,
                (None, Some(_)) => std::cmp::Ordering::Less,
                (None, None) => a
                    .project_path
                    .cmp(&b.project_path)
                    .then(a.target_dir.cmp(&b.target_dir)),
            },
            SortKey::Modified => {
                match (&a.last_modified, &b.last_modified) {
                    // Pending and deleted dirs have no timestamp. They sink.
                    (None, None) => a
                        .project_path
                        .cmp(&b.project_path)
                        .then(a.target_dir.cmp(&b.target_dir)),
                    (None, _) => std::cmp::Ordering::Greater,
                    (_, None) => std::cmp::Ordering::Less,
                    (Some(x), Some(y)) => y
                        .cmp(x)
                        .then(a.project_path.cmp(&b.project_path))
                        .then(a.target_dir.cmp(&b.target_dir)),
                }
            }
            SortKey::Name => a
                .project_path
                .cmp(&b.project_path)
                .then(a.target_dir.cmp(&b.target_dir)),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortKey {
    #[default]
    Size,
    Modified,
    Name,
}

impl SortKey {
    pub fn next(self) -> Self {
        match self {
            Self::Size => Self::Modified,
            Self::Modified => Self::Name,
            Self::Name => Self::Size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn app_with_entries() -> App {
        let mut app = App::new(PathBuf::from("."));
        app.set_discovered(vec![PathBuf::from("proj-big"), PathBuf::from("proj-small")]);
        app.apply_measurements(&[
            Measurement {
                target_dir: PathBuf::from("proj-big/target"),
                size: 100,
                last_modified: Some(SystemTime::UNIX_EPOCH),
            },
            Measurement {
                target_dir: PathBuf::from("proj-small/target"),
                size: 10,
                last_modified: Some(SystemTime::UNIX_EPOCH),
            },
        ]);
        app.finish_scan(None);
        // Simulate settled state: the user has taken over, so later
        // measurements follow the selection instead of pinning to top.
        app.navigated = true;
        app
    }

    #[test]
    fn measurements_update_size_and_keep_selection_on_project() {
        let mut app = app_with_entries();
        // Select proj-small (index 1 after size-desc sort).
        app.table_state.select(Some(1));
        // proj-small grows past proj-big. Order flips but selection follows it.
        app.apply_measurements(&[Measurement {
            target_dir: PathBuf::from("proj-small/target"),
            size: 200,
            last_modified: Some(SystemTime::UNIX_EPOCH),
        }]);
        assert_eq!(
            app.visible_entry(0).unwrap().project_path,
            PathBuf::from("proj-small")
        );
        assert_eq!(app.total_size, 300);
        let selected = app
            .table_state
            .selected()
            .and_then(|i| app.visible_entry(i))
            .unwrap();
        assert_eq!(selected.project_path, PathBuf::from("proj-small"));
        assert_eq!(selected.size, Some(200));
    }

    #[test]
    fn unknown_measurement_is_ignored() {
        let mut app = app_with_entries();
        app.apply_measurements(&[Measurement {
            target_dir: PathBuf::from("gone/target"),
            size: 999,
            last_modified: Some(SystemTime::UNIX_EPOCH),
        }]);
        assert_eq!(app.total_size, 110);
    }

    #[test]
    fn discovered_rows_stay_pending_until_measured() {
        let mut app = App::new(PathBuf::from("."));
        app.set_discovered(vec![PathBuf::from("proj-b"), PathBuf::from("proj-a")]);
        assert!(app.scanning);
        assert_eq!(app.total_size, 0);
        assert!(app.entries.iter().all(|e| e.size.is_none()));
        // One measurement fills its row; pending rows float above, totals skip them.
        app.apply_measurements(&[Measurement {
            target_dir: PathBuf::from("proj-a/target"),
            size: 50,
            last_modified: Some(SystemTime::UNIX_EPOCH),
        }]);
        assert!(app.scanning);
        assert_eq!(app.total_size, 50);
        assert_eq!(app.visible_entry(0).unwrap().size, None);
        assert_eq!(
            app.visible_entry(1).unwrap().project_path,
            PathBuf::from("proj-a")
        );
        assert_eq!(app.visible_entry(1).unwrap().size, Some(50));
        // Finishing keeps still-pending rows pending for the poller.
        app.finish_scan(None);
        assert!(!app.scanning);
        assert_eq!(app.visible_entry(0).unwrap().size, None);
    }
    #[test]
    fn fresh_discovery_jumps_to_top() {
        let mut app = app_with_entries();
        // Sitting mid-list when a rescan lands: the new dirs take over.
        app.table_state.select(Some(1));
        app.begin_scan();
        app.set_discovered(vec![PathBuf::from("proj-small"), PathBuf::from("proj-big")]);
        assert_eq!(app.table_state.selected(), Some(0));
    }

    #[test]
    fn measurements_pin_to_top_until_taken_over() {
        let mut app = App::new(PathBuf::from("."));
        app.set_discovered(vec![PathBuf::from("proj-a"), PathBuf::from("proj-b")]);
        // proj-b measures first but pending proj-a floats above it;
        // selection stays glued to the top until the user takes over.
        app.apply_measurements(&[Measurement {
            target_dir: PathBuf::from("proj-b/target"),
            size: 200,
            last_modified: Some(SystemTime::UNIX_EPOCH),
        }]);
        assert_eq!(
            app.visible_entry(0).unwrap().project_path,
            PathBuf::from("proj-a")
        );
        assert_eq!(app.table_state.selected(), Some(0));
    }
    #[test]
    fn pending_rows_float_above_measured_in_size_order() {
        let mut app = App::new(PathBuf::from("."));
        app.set_discovered(vec![PathBuf::from("proj-a"), PathBuf::from("proj-b")]);
        app.apply_measurements(&[Measurement {
            target_dir: PathBuf::from("proj-b/target"),
            size: 10,
            last_modified: Some(SystemTime::UNIX_EPOCH),
        }]);
        assert_eq!(
            app.visible_entry(0).unwrap().project_path,
            PathBuf::from("proj-a")
        );
        assert_eq!(app.visible_entry(0).unwrap().size, None);
        assert_eq!(
            app.visible_entry(1).unwrap().project_path,
            PathBuf::from("proj-b")
        );
        // Once everything measures, pure size-desc takes over.
        app.apply_measurements(&[Measurement {
            target_dir: PathBuf::from("proj-a/target"),
            size: 200,
            last_modified: Some(SystemTime::UNIX_EPOCH),
        }]);
        assert_eq!(
            app.visible_entry(0).unwrap().project_path,
            PathBuf::from("proj-a")
        );
        assert_eq!(
            app.visible_entry(1).unwrap().project_path,
            PathBuf::from("proj-b")
        );
    }

    #[test]
    fn navigation_suppresses_auto_top() {
        let mut app = App::new(PathBuf::from("."));
        app.set_discovered(vec![PathBuf::from("proj-a"), PathBuf::from("proj-b")]);
        app.next();
        assert_eq!(app.table_state.selected(), Some(1));
        // A measurement re-sorts but follows the user's row now. Measured
        // proj-a sinks below pending proj-b, selection follows proj-b up.
        app.apply_measurements(&[Measurement {
            target_dir: PathBuf::from("proj-a/target"),
            size: 200,
            last_modified: Some(SystemTime::UNIX_EPOCH),
        }]);
        assert_eq!(
            app.visible_entry(0).unwrap().project_path,
            PathBuf::from("proj-b")
        );
        assert_eq!(app.table_state.selected(), Some(0));
    }

    #[test]
    fn deleted_and_recreated_dir_zeroes_then_restores_row() {
        use crate::scan::measure_target;
        let root = std::env::temp_dir().join("cargo-storage-test-recreate");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("proj/target")).unwrap();
        let mut app = App::new(root.clone());
        app.set_discovered(vec![root.join("proj")]);
        app.apply_measurements(&[Measurement {
            target_dir: root.join("proj/target"),
            size: 1,
            last_modified: Some(SystemTime::UNIX_EPOCH),
        }]);
        app.finish_scan(None);
        let target = root.join("proj/target");

        // Deletion zeroes the row and clears its timestamp.
        std::fs::remove_dir_all(&target).unwrap();
        app.apply_measurements(&[measure_target(&target)]);
        assert_eq!(app.entries[0].size, Some(0));
        assert_eq!(app.entries[0].last_modified, None);

        // Recreation restores the row with a real timestamp.
        std::fs::create_dir_all(target.join("debug")).unwrap();
        std::fs::write(target.join("debug/a.bin"), "1234").unwrap();
        app.apply_measurements(&[measure_target(&target)]);
        assert!(app.entries[0].size.is_some_and(|s| s > 0));
        assert!(app.entries[0].last_modified.is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn first_cache_measurement_creates_entry() {
        use crate::scan::measure_target;
        let root = std::env::temp_dir().join("cargo-storage-test-cache-arrival");
        let _ = std::fs::remove_dir_all(&root);
        let mut app = app_with_entries();
        app.build_cache = None;
        app.build_cache_path = Some(root.join("build-cache"));
        assert!(app.build_cache.is_none());

        // Nothing there yet: no row.
        app.apply_measurements(&[measure_target(&root.join("build-cache"))]);
        assert!(app.build_cache.is_none());

        // The cache appears: a row is created.
        std::fs::create_dir_all(root.join("build-cache/content")).unwrap();
        std::fs::write(root.join("build-cache/content/a.bin"), "12345678").unwrap();
        app.apply_measurements(&[measure_target(&root.join("build-cache"))]);
        let cache = app.build_cache.as_ref().expect("cache row created");
        assert!(cache.size.is_some_and(|s| s > 0));
    }

    #[test]
    fn filter_alternation_narrows_to_matches() {
        let mut app = app_with_entries();
        app.set_filter("big|zzz".to_string());
        assert_eq!(app.visible_indices(), vec![1]);
        app.set_filter("proj-".to_string());
        assert_eq!(app.visible_indices().len(), 2);
    }

    #[test]
    fn filter_matches_path_substring() {
        let mut app = App::new(PathBuf::from("."));
        app.set_discovered(vec![PathBuf::from("ws/member-a"), PathBuf::from("other")]);
        app.set_filter("member".to_string());
        assert_eq!(app.visible_indices(), vec![0]);
    }

    #[test]
    fn invalid_filter_keeps_last_good() {
        let mut app = app_with_entries();
        app.set_filter("big".to_string());
        assert_eq!(app.visible_indices(), vec![0]);
        app.set_filter("big(".to_string());
        assert!(app.filter_error.is_some());
        assert_eq!(app.visible_indices(), vec![0]);
        app.set_filter(String::new());
        assert!(app.filter_error.is_none());
        assert!(app.filter_regex.is_none());
        assert_eq!(app.visible_indices().len(), 2);
    }

    #[test]
    fn filter_resets_selection_and_nav_wraps_visible() {
        let mut app = app_with_entries();
        app.table_state.select(Some(1));
        app.set_filter("small".to_string());
        assert_eq!(app.visible_indices(), vec![1]);
        assert_eq!(app.table_state.selected(), Some(0));
        app.next();
        assert_eq!(app.table_state.selected(), Some(0));
        app.previous();
        assert_eq!(app.table_state.selected(), Some(0));
    }

    #[test]
    fn delete_selected_removes_target_and_zeroes_row() {
        let root = std::env::temp_dir().join("cargo-storage-test-delete");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("proj/target")).unwrap();
        std::fs::write(root.join("proj/target/a.bin"), "1234").unwrap();
        let mut app = App::new(root.clone());
        app.set_discovered(vec![root.join("proj")]);
        app.apply_measurements(&[Measurement {
            target_dir: root.join("proj/target"),
            size: 4,
            last_modified: Some(SystemTime::UNIX_EPOCH),
        }]);
        app.finish_scan(None);
        app.delete_selected();
        assert!(!root.join("proj/target").exists());
        assert_eq!(app.entries[0].size, Some(0));
        assert_eq!(app.entries[0].last_modified, None);
        assert_eq!(app.total_size, 0);
        assert!(app.delete_error.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_selected_follows_filtered_selection() {
        let root = std::env::temp_dir().join("cargo-storage-test-delete-filter");
        let _ = std::fs::remove_dir_all(&root);
        for proj in ["proj-a", "proj-b"] {
            std::fs::create_dir_all(root.join(proj).join("target")).unwrap();
            std::fs::write(root.join(proj).join("target/a.bin"), "1234").unwrap();
        }
        let mut app = App::new(root.clone());
        app.set_discovered(vec![root.join("proj-a"), root.join("proj-b")]);
        app.finish_scan(None);
        app.set_filter("proj-b".to_string());
        assert_eq!(app.visible_indices().len(), 1);
        app.delete_selected();
        assert!(!root.join("proj-b/target").exists());
        assert!(root.join("proj-a/target").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_selected_moves_down_or_keeps_last() {
        let selected_project = |app: &App| {
            app.table_state
                .selected()
                .and_then(|i| app.visible_indices().get(i).copied())
                .and_then(|i| app.entries.get(i))
                .map(|e| e.project_path.clone())
                .expect("selection kept")
        };
        // Deleting the first row moves down. Fake paths miss on disk,
        // which counts as deleted.
        let mut app = app_with_entries();
        app.table_state.select(Some(0));
        app.delete_selected();
        assert_eq!(selected_project(&app), PathBuf::from("proj-small"));
        // Deleting a middle row moves down, not up.
        let mut app = App::new(PathBuf::from("."));
        app.set_discovered(vec![
            PathBuf::from("proj-big"),
            PathBuf::from("proj-mid"),
            PathBuf::from("proj-small"),
        ]);
        app.apply_measurements(&[
            Measurement {
                target_dir: PathBuf::from("proj-big/target"),
                size: 100,
                last_modified: Some(SystemTime::UNIX_EPOCH),
            },
            Measurement {
                target_dir: PathBuf::from("proj-mid/target"),
                size: 50,
                last_modified: Some(SystemTime::UNIX_EPOCH),
            },
            Measurement {
                target_dir: PathBuf::from("proj-small/target"),
                size: 10,
                last_modified: Some(SystemTime::UNIX_EPOCH),
            },
        ]);
        app.finish_scan(None);
        app.navigated = true;
        app.table_state.select(Some(1));
        app.delete_selected();
        assert_eq!(selected_project(&app), PathBuf::from("proj-small"));
        // Deleting the last row keeps selection on the deleted row.
        let mut app = app_with_entries();
        app.table_state.select(Some(1));
        app.delete_selected();
        assert_eq!(app.table_state.selected(), Some(1));
        assert_eq!(selected_project(&app), PathBuf::from("proj-small"));
        // A lone row keeps selection.
        let mut solo = App::new(PathBuf::from("."));
        solo.set_discovered(vec![PathBuf::from("only")]);
        solo.delete_selected();
        assert_eq!(solo.table_state.selected(), Some(0));
    }

    #[test]
    fn paging_jumps_one_screen_and_clamps() {
        let mut app = App::new(PathBuf::from("."));
        let projects: Vec<PathBuf> = (0..30)
            .map(|i| PathBuf::from(format!("proj-{i:02}")))
            .collect();
        app.set_discovered(projects);
        app.finish_scan(None);
        app.page_len = 10;
        app.table_state.select(Some(0));
        app.page_down();
        assert_eq!(app.table_state.selected(), Some(10));
        app.page_down();
        assert_eq!(app.table_state.selected(), Some(20));
        app.page_down();
        assert_eq!(app.table_state.selected(), Some(29));
        app.page_up();
        assert_eq!(app.table_state.selected(), Some(19));
        app.page_up();
        app.page_up();
        assert_eq!(app.table_state.selected(), Some(0));
    }
}
