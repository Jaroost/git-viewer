//! Persistent settings: the list of recently opened projects.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const MAX_RECENT: usize = 15;

/// How the file of a commit is shown.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffView {
    /// The whole file, changes marked in place.
    Full,
    /// Previous version on the left, new version on the right.
    Split,
    /// Changed lines with a few lines of context.
    // Any unknown value (e.g. from a newer version) falls back to this one instead of making the
    // whole configuration unreadable. `#[serde(other)]` has to be on the last variant.
    #[default]
    #[serde(other)]
    Hunks,
}

/// Widths (in points) of the resizable columns of the commit list.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Columns {
    /// Branches / tags column, left of the graph.
    pub refs: f32,
    pub author: f32,
    pub time: f32,
}

impl Columns {
    pub const REFS_RANGE: (f32, f32) = (80.0, 600.0);
    pub const AUTHOR_RANGE: (f32, f32) = (60.0, 400.0);
    pub const TIME_RANGE: (f32, f32) = (60.0, 300.0);

    /// Forces every width into its allowed range; NaN and infinities fall back to the default.
    pub fn clamped(self) -> Self {
        let d = Self::default();
        let fix = |v: f32, def: f32, (lo, hi): (f32, f32)| {
            if v.is_finite() { v.clamp(lo, hi) } else { def }
        };
        Self {
            refs: fix(self.refs, d.refs, Self::REFS_RANGE),
            author: fix(self.author, d.author, Self::AUTHOR_RANGE),
            time: fix(self.time, d.time, Self::TIME_RANGE),
        }
    }
}

impl Default for Columns {
    fn default() -> Self {
        Self { refs: 200.0, author: 140.0, time: 130.0 }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Most recently opened first.
    pub recent: Vec<PathBuf>,
    /// Pinned projects always stay above the others in the list.
    pub pinned: Vec<PathBuf>,
    pub columns: Columns,
    pub diff_view: DiffView,
}

impl Config {
    /// `<config dir>/git-viewer/config.json` (e.g. `~/.config` on Linux, `%APPDATA%` on Windows).
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("git-viewer").join("config.json"))
    }

    /// Loads the config; a missing or corrupt file gives the default config.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<Self>(&s).ok())
            .map(|mut c| {
                c.columns = c.columns.clamped();
                let recent = c.recent.clone();
                c.pinned.retain(|p| recent.contains(p));
                c
            })
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        // Write then rename so a crash never leaves a half-written file.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }

    /// Puts `project` first, removing any previous occurrence. When the list is full the
    /// oldest *unpinned* project is dropped; pinned ones are never evicted.
    pub fn add_recent(&mut self, project: PathBuf) {
        self.recent.retain(|p| *p != project);
        self.recent.insert(0, project);
        while self.recent.len() > MAX_RECENT {
            // Never evict the project that was just added (index 0) nor a pinned one.
            match (1..self.recent.len()).rev().find(|&i| !self.is_pinned(&self.recent[i])) {
                Some(i) => {
                    self.recent.remove(i);
                }
                None => break, // everything else is pinned: let the list grow
            }
        }
    }

    pub fn remove_recent(&mut self, project: &Path) {
        self.recent.retain(|p| p != project);
        self.pinned.retain(|p| p != project);
    }

    pub fn is_pinned(&self, project: &Path) -> bool {
        self.pinned.iter().any(|p| p == project)
    }

    pub fn toggle_pin(&mut self, project: &Path) {
        if self.is_pinned(project) {
            self.pinned.retain(|p| p != project);
        } else if self.recent.iter().any(|p| p == project) {
            self.pinned.push(project.to_path_buf());
        }
    }

    /// Projects as displayed: pinned first, then the others, each group in list order.
    pub fn ordered_recent(&self) -> Vec<&PathBuf> {
        let (pinned, others): (Vec<_>, Vec<_>) = self.recent.iter().partition(|p| self.is_pinned(p));
        pinned.into_iter().chain(others).collect()
    }

    /// Moves a project one step up or down *within its group* (pinned or not).
    pub fn move_recent(&mut self, project: &Path, up: bool) {
        let pinned = self.is_pinned(project);
        let group: Vec<usize> = (0..self.recent.len())
            .filter(|&i| self.is_pinned(&self.recent[i]) == pinned)
            .collect();
        let Some(pos) = group.iter().position(|&i| self.recent[i] == project) else { return };
        let other = if up { pos.checked_sub(1) } else { (pos + 1 < group.len()).then_some(pos + 1) };
        if let Some(other) = other {
            self.recent.swap(group[pos], group[other]);
        }
    }

    /// Drag-and-drop: moves `from` just before (or, with `after`, just after) `to`.
    /// Only reorders inside a group: a pinned project cannot be dropped among the unpinned ones
    /// (that would silently change its pinned state), and unknown paths change nothing.
    pub fn reorder(&mut self, from: &Path, to: &Path, after: bool) {
        if from == to || self.is_pinned(from) != self.is_pinned(to) {
            return;
        }
        let Some(from_idx) = self.recent.iter().position(|p| p == from) else { return };
        if !self.recent.iter().any(|p| p == to) {
            return;
        }
        let item = self.recent.remove(from_idx);
        let to_idx = self.recent.iter().position(|p| p == to).expect("checked above");
        self.recent.insert(if after { to_idx + 1 } else { to_idx }, item);
    }

    /// Alphabetical order (case-insensitive, by folder name), applied once.
    pub fn sort_recent_by_name(&mut self) {
        let key = |p: &PathBuf| {
            p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()
        };
        self.recent.sort_by_key(key);
    }

    /// Forgets the projects for which `exists` is false.
    pub fn prune_missing(&mut self, exists: impl Fn(&Path) -> bool) -> usize {
        let before = self.recent.len();
        self.recent.retain(|p| exists(p));
        self.pinned.retain(|p| self.recent.contains(p));
        before - self.recent.len()
    }

    /// Empties the list but keeps the pinned projects.
    pub fn clear_unpinned(&mut self) {
        let pinned = std::mem::take(&mut self.pinned);
        self.recent.retain(|p| pinned.contains(p));
        self.pinned = pinned;
    }

    pub fn last(&self) -> Option<&PathBuf> {
        self.recent.first()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn missing_file_gives_default() {
        let dir = TempDir::new().unwrap();
        assert_eq!(Config::load(&dir.path().join("nope.json")), Config::default());
    }

    #[test]
    fn corrupt_file_gives_default() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, "{ not json").unwrap();
        assert_eq!(Config::load(&p), Config::default());
    }

    #[test]
    fn unknown_or_missing_fields_are_tolerated() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, r#"{"future_option": 1}"#).unwrap();
        assert_eq!(Config::load(&p), Config::default());
    }

    #[test]
    fn save_then_load_roundtrips_and_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("sub/dir/config.json");
        let mut c = Config::default();
        c.add_recent(PathBuf::from("/a"));
        c.add_recent(PathBuf::from("/b"));
        c.save(&p).unwrap();
        assert_eq!(Config::load(&p), c);
    }

    #[test]
    fn reopening_a_project_moves_it_first_without_duplicate() {
        let mut c = Config::default();
        for p in ["/a", "/b", "/c"] {
            c.add_recent(PathBuf::from(p));
        }
        c.add_recent(PathBuf::from("/a"));
        assert_eq!(c.recent, [PathBuf::from("/a"), "/c".into(), "/b".into()]);
        assert_eq!(c.last(), Some(&PathBuf::from("/a")));
    }

    #[test]
    fn recent_list_is_capped() {
        let mut c = Config::default();
        for i in 0..(MAX_RECENT + 5) {
            c.add_recent(PathBuf::from(format!("/p{i}")));
        }
        assert_eq!(c.recent.len(), MAX_RECENT);
        assert_eq!(c.recent[0], PathBuf::from(format!("/p{}", MAX_RECENT + 4)));
    }

    #[test]
    fn remove_recent_forgets_a_project() {
        let mut c = Config::default();
        c.add_recent(PathBuf::from("/a"));
        c.add_recent(PathBuf::from("/b"));
        c.remove_recent(Path::new("/a"));
        assert_eq!(c.recent, [PathBuf::from("/b")]);
    }

    #[test]
    fn column_widths_roundtrip() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        let c = Config { columns: Columns { refs: 250.0, author: 99.0, time: 111.0 }, ..Default::default() };
        c.save(&p).unwrap();
        assert_eq!(Config::load(&p).columns, c.columns);
    }

    #[test]
    fn old_config_without_columns_gets_default_widths() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, r#"{"recent": ["/a"]}"#).unwrap();
        let c = Config::load(&p);
        assert_eq!(c.columns, Columns::default());
        assert_eq!(c.recent, [PathBuf::from("/a")]);
    }

    #[test]
    fn absurd_column_widths_are_clamped_on_load() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, r#"{"columns": {"refs": 99999, "author": -5, "time": 130}}"#).unwrap();
        let c = Config::load(&p).columns;
        assert_eq!(c.refs, Columns::REFS_RANGE.1);
        assert_eq!(c.author, Columns::AUTHOR_RANGE.0);
        assert_eq!(c.time, 130.0);
    }

    #[test]
    fn non_finite_widths_fall_back_to_defaults() {
        let c = Columns { refs: f32::NAN, author: f32::INFINITY, time: 100.0 }.clamped();
        assert_eq!(c.refs, Columns::default().refs);
        assert_eq!(c.author, Columns::default().author);
        assert_eq!(c.time, 100.0);
    }

    // ------------------------------------------------------------ organising

    fn cfg(paths: &[&str]) -> Config {
        let mut c = Config::default();
        for p in paths.iter().rev() {
            c.add_recent(PathBuf::from(p)); // so that `paths[0]` ends up first
        }
        c
    }

    fn names(v: Vec<&PathBuf>) -> Vec<String> {
        v.iter().map(|p| p.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn pinned_projects_are_listed_first_in_their_own_order() {
        let mut c = cfg(&["/a", "/b", "/c", "/d"]);
        c.toggle_pin(Path::new("/c"));
        c.toggle_pin(Path::new("/b"));
        assert_eq!(names(c.ordered_recent()), ["/b", "/c", "/a", "/d"]);
        c.toggle_pin(Path::new("/b")); // unpin
        assert_eq!(names(c.ordered_recent()), ["/c", "/a", "/b", "/d"]);
    }

    #[test]
    fn cannot_pin_an_unknown_project() {
        let mut c = cfg(&["/a"]);
        c.toggle_pin(Path::new("/nope"));
        assert!(c.pinned.is_empty());
    }

    #[test]
    fn opening_a_project_does_not_unpin_or_demote_pinned_ones() {
        let mut c = cfg(&["/a", "/b", "/c"]);
        c.toggle_pin(Path::new("/c"));
        c.add_recent(PathBuf::from("/b"));
        assert_eq!(names(c.ordered_recent()), ["/c", "/b", "/a"]);
    }

    #[test]
    fn full_list_evicts_oldest_unpinned_never_pinned() {
        let mut c = Config::default();
        c.add_recent(PathBuf::from("/keep"));
        c.toggle_pin(Path::new("/keep")); // oldest entry, but pinned
        for i in 0..MAX_RECENT + 3 {
            c.add_recent(PathBuf::from(format!("/p{i}")));
        }
        assert_eq!(c.recent.len(), MAX_RECENT);
        assert!(c.recent.contains(&PathBuf::from("/keep")));
        assert!(!c.recent.contains(&PathBuf::from("/p0")));
    }

    #[test]
    fn list_of_only_pinned_projects_may_exceed_the_cap() {
        let mut c = Config::default();
        for i in 0..MAX_RECENT + 2 {
            let p = PathBuf::from(format!("/p{i}"));
            c.add_recent(p.clone());
            c.toggle_pin(&p);
        }
        assert_eq!(c.recent.len(), MAX_RECENT + 2);
    }

    #[test]
    fn move_up_and_down_stay_within_the_group() {
        let mut c = cfg(&["/a", "/b", "/c", "/d"]);
        c.toggle_pin(Path::new("/c"));
        // view: /c | /a /b /d
        c.move_recent(Path::new("/d"), true);
        assert_eq!(names(c.ordered_recent()), ["/c", "/a", "/d", "/b"]);
        c.move_recent(Path::new("/a"), false);
        assert_eq!(names(c.ordered_recent()), ["/c", "/d", "/a", "/b"]);
        // Edges do nothing; pinned never swaps with an unpinned one.
        c.move_recent(Path::new("/c"), true);
        c.move_recent(Path::new("/c"), false);
        c.move_recent(Path::new("/b"), false);
        assert_eq!(names(c.ordered_recent()), ["/c", "/d", "/a", "/b"]);
    }

    #[test]
    fn move_of_unknown_project_is_a_no_op() {
        let mut c = cfg(&["/a", "/b"]);
        c.move_recent(Path::new("/zzz"), true);
        assert_eq!(names(c.ordered_recent()), ["/a", "/b"]);
    }

    #[test]
    fn sort_by_name_ignores_case_and_parent_folders() {
        let mut c = cfg(&["/x/Zeta", "/a/beta", "/m/Alpha"]);
        c.sort_recent_by_name();
        assert_eq!(names(c.ordered_recent()), ["/m/Alpha", "/a/beta", "/x/Zeta"]);
    }

    #[test]
    fn prune_missing_drops_unknown_folders_and_their_pins() {
        let mut c = cfg(&["/here", "/gone", "/also-here"]);
        c.toggle_pin(Path::new("/gone"));
        let removed = c.prune_missing(|p| p != Path::new("/gone"));
        assert_eq!(removed, 1);
        assert_eq!(names(c.ordered_recent()), ["/here", "/also-here"]);
        assert!(c.pinned.is_empty());
    }

    #[test]
    fn clear_unpinned_keeps_only_pinned_projects() {
        let mut c = cfg(&["/a", "/b", "/c"]);
        c.toggle_pin(Path::new("/b"));
        c.clear_unpinned();
        assert_eq!(names(c.ordered_recent()), ["/b"]);
        assert!(c.is_pinned(Path::new("/b")));
    }

    #[test]
    fn removing_a_project_also_unpins_it() {
        let mut c = cfg(&["/a", "/b"]);
        c.toggle_pin(Path::new("/a"));
        c.remove_recent(Path::new("/a"));
        assert!(c.pinned.is_empty() && c.recent == [PathBuf::from("/b")]);
    }

    #[test]
    fn pins_and_order_survive_a_save_and_load_but_stray_pins_are_dropped() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        let mut c = cfg(&["/a", "/b"]);
        c.toggle_pin(Path::new("/b"));
        c.save(&p).unwrap();
        assert_eq!(Config::load(&p), c);

        std::fs::write(&p, r#"{"recent": ["/a"], "pinned": ["/a", "/not-in-recent"]}"#).unwrap();
        assert_eq!(Config::load(&p).pinned, [PathBuf::from("/a")]);
    }

    #[test]
    fn reorder_moves_before_or_after_the_target() {
        let mut c = cfg(&["/a", "/b", "/c", "/d"]);
        c.reorder(Path::new("/d"), Path::new("/b"), false); // before /b
        assert_eq!(names(c.ordered_recent()), ["/a", "/d", "/b", "/c"]);
        c.reorder(Path::new("/a"), Path::new("/c"), true); // after /c
        assert_eq!(names(c.ordered_recent()), ["/d", "/b", "/c", "/a"]);
        c.reorder(Path::new("/c"), Path::new("/d"), false); // to the very top
        assert_eq!(names(c.ordered_recent()), ["/c", "/d", "/b", "/a"]);
    }

    #[test]
    fn reorder_works_across_items_of_the_other_group() {
        // recent order is a, p, b, c with p pinned: the view is p | a b c.
        let mut c = cfg(&["/a", "/p", "/b", "/c"]);
        c.toggle_pin(Path::new("/p"));
        c.reorder(Path::new("/c"), Path::new("/a"), false);
        assert_eq!(names(c.ordered_recent()), ["/p", "/c", "/a", "/b"]);
        c.reorder(Path::new("/a"), Path::new("/b"), true);
        assert_eq!(names(c.ordered_recent()), ["/p", "/c", "/b", "/a"]);
    }

    #[test]
    fn reorder_refuses_to_cross_between_pinned_and_unpinned() {
        let mut c = cfg(&["/a", "/b", "/c"]);
        c.toggle_pin(Path::new("/a"));
        let before = c.clone();
        c.reorder(Path::new("/a"), Path::new("/c"), true); // pinned onto unpinned
        c.reorder(Path::new("/c"), Path::new("/a"), false); // unpinned onto pinned
        assert_eq!(c, before);
    }

    #[test]
    fn reorder_ignores_unknown_paths_and_self_drops_without_losing_anything() {
        let mut c = cfg(&["/a", "/b"]);
        let before = c.clone();
        c.reorder(Path::new("/a"), Path::new("/a"), true);
        c.reorder(Path::new("/zzz"), Path::new("/a"), true);
        c.reorder(Path::new("/a"), Path::new("/zzz"), true); // target unknown: /a must stay
        assert_eq!(c, before);
    }

    #[test]
    fn diff_view_choice_is_remembered_and_defaults_to_changed_lines() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        assert_eq!(Config::load(&p).diff_view, DiffView::Hunks); // no file yet
        std::fs::write(&p, r#"{"recent": []}"#).unwrap();
        assert_eq!(Config::load(&p).diff_view, DiffView::Hunks); // old config without the option
        for view in [DiffView::Full, DiffView::Split, DiffView::Hunks] {
            Config { diff_view: view, ..Default::default() }.save(&p).unwrap();
            assert_eq!(Config::load(&p).diff_view, view);
        }
    }

    #[test]
    fn unknown_diff_view_does_not_make_the_config_unreadable() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, r#"{"recent": ["/a"], "diff_view": "three_way"}"#).unwrap();
        let c = Config::load(&p);
        assert_eq!(c.diff_view, DiffView::Hunks);
        assert_eq!(c.recent, [PathBuf::from("/a")]); // the rest of the config survives
        // The old boolean option of a previous version is simply ignored.
        std::fs::write(&p, r#"{"recent": ["/b"], "full_file_diff": true}"#).unwrap();
        assert_eq!(Config::load(&p).recent, [PathBuf::from("/b")]);
    }
}
