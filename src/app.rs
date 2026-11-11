use crate::browser::FolderBrowser;
use crate::config::{Columns, Config, DiffView};
use crate::graph::{self, Layout};
use crate::search;
use crate::split::{self, Cell, CellKind, SplitRow};
use crate::syntax::{self, Highlighted};
use crate::timefmt;
use crate::repo::{
    BranchInfo, CommitDetail, CommitInfo, FileDiff, RefBadge, RefKind, RemoteBranchInfo, Repo, TagInfo,
};
use crate::worktree::{StashEntry, StatusEntry};
use eframe::egui::{self, Color32, RichText};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;

/// Maximum number of commits loaded per project.
const COMMIT_LIMIT: usize = 100_000;
const LANE_W: f32 = 22.0;
const ROW_H: f32 = 26.0;
/// Radius of the author avatar drawn on each commit.
const NODE_R: f32 = 9.0;
const PALETTE: [Color32; 8] = [
    Color32::from_rgb(0x4e, 0x9a, 0xe6),
    Color32::from_rgb(0xe6, 0x7e, 0x3c),
    Color32::from_rgb(0x5c, 0xb8, 0x5c),
    Color32::from_rgb(0xc2, 0x5c, 0xd6),
    Color32::from_rgb(0xe6, 0xc2, 0x3c),
    Color32::from_rgb(0x3c, 0xc4, 0xc4),
    Color32::from_rgb(0xe6, 0x5c, 0x6c),
    Color32::from_rgb(0x9a, 0x8c, 0xe6),
];

/// Everything computed in the background when a project is opened.
struct Snapshot {
    root: PathBuf,
    commits: Vec<CommitInfo>,
    layout: Layout,
    branches: Vec<BranchInfo>,
    remote_branches: Vec<RemoteBranchInfo>,
    tags: Vec<TagInfo>,
    refs: HashMap<String, Vec<RefBadge>>,
    head: Option<String>,
    remotes: Vec<String>,
}

fn load_snapshot(path: &Path) -> anyhow::Result<Snapshot> {
    let repo = Repo::open(path)?;
    let commits = repo.commits(COMMIT_LIMIT)?;
    Ok(Snapshot {
        root: repo.root(),
        layout: graph::layout(&commits),
        branches: repo.branches()?,
        remote_branches: repo.remote_branches()?,
        tags: repo.tags()?,
        refs: repo.refs()?,
        head: repo.current_branch(),
        remotes: repo.remotes().unwrap_or_default(),
        commits,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    History,
    Changes,
}

#[derive(Clone, Copy)]
enum PromptKind {
    Branch,
    Tag,
}

/// A small dialog asking for a name (branch or tag) to create at a commit.
struct Prompt {
    kind: PromptKind,
    commit: usize,
    text: String,
}

enum RowAction {
    Select(usize),
    NewBranch(usize),
    NewTag(usize),
    CheckoutDetached(usize),
    CopyHash(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SideTab {
    Branches,
    Tags,
    Stashes,
}

enum StashAction {
    Save,
    Apply(usize),
    Pop(usize),
    AskDrop(usize),
}

/// What the user asked for in the right-hand panel; applied after the frame is drawn.
#[derive(Default)]
struct SideActions {
    checkout: Option<String>,
    ask_delete: Option<String>,
    create_branch: Option<String>,
    /// (remote, branch name)
    checkout_remote: Option<(String, String)>,
    /// Local branch to publish / push.
    push_branch: Option<String>,
    /// Commit id to select and scroll to in the graph.
    jump_to: Option<String>,
    stash: Option<StashAction>,
}

enum ChangeAction {
    Stage(String),
    Unstage(String),
    StageAll,
    UnstageAll,
    Select(String, bool),
    Commit,
}

/// State of the commit search: results are highlighted in the full list, never filtered out.
#[derive(Default)]
struct Search {
    query: String,
    /// Commit indices matching the query, ascending.
    matches: Vec<usize>,
    /// `flags[i]` is true when commit `i` matches (empty when there is no query).
    flags: Vec<bool>,
    /// Position in `matches` of the result we are on.
    current: Option<usize>,
    /// Query the results above were computed for (to search incrementally).
    computed_for: String,
}

type TaskResult = Result<String, String>;

pub struct App {
    config: Config,
    config_path: Option<PathBuf>,
    /// Project to open on the first frame.
    pending_open: Option<String>,
    loading: Option<Receiver<Result<Snapshot, String>>>,
    snap: Option<Snapshot>,
    repo: Option<Repo>,
    error: Option<String>,
    search: Search,
    search_id: Option<egui::Id>,
    /// Commit to scroll into view on the next frame.
    scroll_to: Option<usize>,
    selected: Option<usize>,
    detail: Option<Result<CommitDetail, String>>,
    selected_file: usize,
    mode: Mode,
    status: Vec<StatusEntry>,
    /// (path, staged) of the file whose diff is shown in the Changes view.
    change_sel: Option<(String, bool)>,
    change_diff: Option<FileDiff>,
    commit_msg: String,
    new_branch: String,
    prompt: Option<Prompt>,
    /// Branch awaiting delete confirmation: (name, has commits not merged into HEAD).
    confirm_delete: Option<(String, bool)>,
    /// Network task running in the background (label, result channel).
    busy: Option<(String, Receiver<TaskResult>)>,
    /// Last result to show in the status bar: (text, is_error).
    message: Option<(String, bool)>,
    /// Commit to re-select once a reload finishes.
    reselect: Option<String>,
    was_focused: bool,
    browser: Option<FolderBrowser>,
    show_organizer: bool,
    /// Highlighting of the file diff on screen: finished result and job in flight (uid, dark, ...).
    /// Whole-file diff of the file on screen: (commit id, path, diff, was cut). Computed on demand.
    full_cache: Option<(String, String, FileDiff, bool)>,
    /// Side-by-side rows of the whole-file diff on screen, keyed by that diff's `uid`.
    split_cache: Option<(u64, Vec<SplitRow>)>,
    /// Scroll the diff to its first change on the next frame (entering whole-file view, new file).
    scroll_to_change: bool,
    hl_ready: Option<(u64, bool, Arc<Highlighted>)>,
    hl_pending: Option<(u64, bool, Receiver<Highlighted>)>,
    side_tab: SideTab,
    stashes: Vec<StashEntry>,
    stash_msg: String,
    stash_untracked: bool,
    confirm_drop_stash: Option<usize>,
    confirm_clear: bool,
    /// Recent projects whose folder no longer exists (refreshed, not checked every frame).
    missing: HashSet<PathBuf>,
}

impl App {
    pub fn new() -> Self {
        let config_path = Config::default_path();
        let config = config_path.as_deref().map(Config::load).unwrap_or_default();
        let start = std::env::args()
            .nth(1)
            .or_else(|| config.last().map(|p| p.to_string_lossy().into_owned()))
            .unwrap_or_else(|| ".".into());
        Self {
            config,
            config_path,
            pending_open: Some(start),
            loading: None,
            snap: None,
            repo: None,
            error: None,
            search: Search::default(),
            search_id: None,
            scroll_to: None,
            selected: None,
            detail: None,
            selected_file: 0,
            mode: Mode::History,
            status: Vec::new(),
            change_sel: None,
            change_diff: None,
            commit_msg: String::new(),
            new_branch: String::new(),
            prompt: None,
            confirm_delete: None,
            busy: None,
            message: None,
            reselect: None,
            was_focused: true,
            browser: None,
            show_organizer: false,
            full_cache: None,
            split_cache: None,
            scroll_to_change: false,
            hl_ready: None,
            hl_pending: None,
            side_tab: SideTab::Branches,
            stashes: Vec::new(),
            stash_msg: String::new(),
            stash_untracked: true,
            confirm_drop_stash: None,
            confirm_clear: false,
            missing: HashSet::new(),
        }
    }

    /// Starts loading `path` in a background thread so the UI never freezes.
    fn open(&mut self, ctx: &egui::Context, path: String) {
        self.error = None;
        self.snap = None;
        self.repo = None;
        self.selected = None;
        self.detail = None;
        self.change_sel = None;
        self.change_diff = None;
        self.status.clear();
        self.search = Search::default();
        self.spawn_load(ctx, path);
    }

    /// Reloads the current project but keeps showing the old data (and selection) meanwhile.
    fn refresh(&mut self, ctx: &egui::Context) {
        if self.loading.is_some() {
            return;
        }
        if let Some(snap) = &self.snap {
            self.reselect = self.selected.map(|i| snap.commits[i].id.clone());
            let path = snap.root.to_string_lossy().into_owned();
            self.spawn_load(ctx, path);
        }
    }

    fn spawn_load(&mut self, ctx: &egui::Context, path: String) {
        let (tx, rx) = mpsc::channel();
        self.loading = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let res = load_snapshot(Path::new(&path)).map_err(|e| e.to_string());
            let _ = tx.send(res);
            ctx.request_repaint();
        });
    }

    fn poll_loading(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.loading else { return };
        let res = match rx.try_recv() {
            Ok(res) => res,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("le chargement a échoué".to_string()),
        };
        self.loading = None;
        match res {
            Ok(snap) => {
                self.repo = Repo::open(&snap.root).ok();
                self.config.add_recent(snap.root.clone());
                self.save_config();
                self.refresh_missing();
                let name = snap.root.file_name().map(|n| n.to_string_lossy().into_owned());
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
                    "{} — Git Viewer",
                    name.unwrap_or_default()
                )));
                self.snap = Some(snap);
                self.update_search(false);
                self.refresh_status();
                if let Some(id) = self.reselect.take() {
                    let idx = self.snap.as_ref().and_then(|s| s.commits.iter().position(|c| c.id == id));
                    match idx {
                        Some(i) => self.select(i),
                        None => {
                            self.selected = None;
                            self.detail = None;
                        }
                    }
                }
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn refresh_missing(&mut self) {
        self.missing = self.config.recent.iter().filter(|p| !p.is_dir()).cloned().collect();
    }

    fn open_browser(&mut self) {
        let start = self
            .snap
            .as_ref()
            .and_then(|s| s.root.parent().map(Path::to_path_buf))
            .or_else(|| self.config.last().and_then(|p| p.parent().map(Path::to_path_buf)));
        self.browser = Some(FolderBrowser::new(start.as_deref()));
    }

    /// Syntax highlighting for `file`, computed in a background thread. Returns `None` until it is
    /// ready (the diff is drawn without colors meanwhile) or when the file has nothing to color.
    fn highlights_for(&mut self, ctx: &egui::Context, dark: bool, file: &FileDiff) -> Option<Arc<Highlighted>> {
        if file.binary || file.lines.is_empty() {
            return None;
        }
        if let Some((uid, d, rx)) = self.hl_pending.take() {
            match rx.try_recv() {
                Ok(res) => self.hl_ready = Some((uid, d, Arc::new(res))),
                Err(TryRecvError::Empty) => self.hl_pending = Some((uid, d, rx)),
                Err(TryRecvError::Disconnected) => {}
            }
        }
        if let Some((uid, d, res)) = &self.hl_ready {
            if *uid == file.uid && *d == dark {
                return Some(res.clone());
            }
        }
        let in_flight = self.hl_pending.as_ref().is_some_and(|(u, d, _)| *u == file.uid && *d == dark);
        if !in_flight {
            let (tx, rx) = mpsc::channel();
            self.hl_pending = Some((file.uid, dark, rx));
            let path = file.path.clone();
            let lines: Vec<(char, String)> = file.lines.iter().map(|l| (l.origin, l.text.clone())).collect();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let refs: Vec<(char, &str)> = lines.iter().map(|(o, t)| (*o, t.as_str())).collect();
                let _ = tx.send(syntax::analyze(&path, &refs, dark));
                ctx.request_repaint();
            });
        }
        None
    }

    fn save_config(&self) {
        if let Some(p) = &self.config_path {
            if let Err(e) = self.config.save(p) {
                eprintln!("impossible d'enregistrer la configuration : {e}");
            }
        }
    }

    /// Recomputes the results for the current query. With `jump`, goes to the first result at or
    /// after the selected commit (so typing more letters keeps you where you are if it still matches).
    fn update_search(&mut self, jump: bool) {
        let query = self.search.query.trim().to_string();
        let previous = std::mem::take(&mut self.search.computed_for);
        let previous_matches = std::mem::take(&mut self.search.matches);
        self.search.flags.clear();
        self.search.current = None;
        let Some(snap) = &self.snap else { return };
        if query.is_empty() {
            return;
        }
        let previous_arg = Some((previous.as_str(), previous_matches.as_slice()));
        self.search.matches = search::find_matches(&snap.commits, &query, previous_arg);
        let mut flags = vec![false; snap.commits.len()];
        for &i in &self.search.matches {
            flags[i] = true;
        }
        self.search.flags = flags;
        self.search.computed_for = query;
        if jump {
            let pos = search::first_at_or_after(&self.search.matches, self.selected.unwrap_or(0));
            self.goto_match(pos);
        }
    }

    fn goto_match(&mut self, pos: Option<usize>) {
        let Some(pos) = pos else { return };
        let idx = self.search.matches[pos];
        self.search.current = Some(pos);
        self.mode = Mode::History;
        self.select(idx);
        self.scroll_to = Some(idx);
    }

    fn step_search(&mut self, delta: isize) {
        let pos = search::step(self.search.matches.len(), self.search.current, delta);
        self.goto_match(pos);
    }

    fn refresh_status(&mut self) {
        let Some(repo) = &self.repo else { return };
        self.stashes = repo.stash_list().unwrap_or_default();
        match repo.status() {
            Ok(st) => self.status = st,
            Err(e) => self.message = Some((format!("statut illisible : {e}"), true)),
        }
        // Keep the shown diff in sync with the file's current state.
        match self.change_sel.clone() {
            Some((path, staged)) if self.status.iter().any(|e| e.path == path) => {
                self.load_change_diff(&path, staged)
            }
            _ => {
                self.change_sel = None;
                self.change_diff = None;
            }
        }
    }

    fn load_change_diff(&mut self, path: &str, staged: bool) {
        let Some(repo) = &self.repo else { return };
        let files = if staged { repo.diff_staged() } else { repo.diff_unstaged() };
        self.change_sel = Some((path.to_string(), staged));
        self.change_diff = files.ok().and_then(|f| f.into_iter().find(|d| d.path == path));
    }

    /// Runs a quick local operation on the UI thread and reports its result.
    fn local_op(
        &mut self,
        ctx: &egui::Context,
        reload_history: bool,
        f: impl FnOnce(&Repo) -> anyhow::Result<String>,
    ) {
        let Some(repo) = &self.repo else { return };
        match f(repo) {
            Ok(msg) => {
                self.message = (!msg.is_empty()).then_some((msg, false));
                if reload_history {
                    self.refresh(ctx);
                }
            }
            Err(e) => self.message = Some((format!("{e:#}"), true)),
        }
        self.refresh_status();
    }

    /// Runs a network operation in a background thread.
    fn remote_op(
        &mut self,
        ctx: &egui::Context,
        label: &str,
        f: impl FnOnce(&Repo) -> anyhow::Result<String> + Send + 'static,
    ) {
        let Some(snap) = &self.snap else { return };
        if self.busy.is_some() {
            return;
        }
        let root = snap.root.clone();
        let (tx, rx) = mpsc::channel();
        self.busy = Some((label.to_string(), rx));
        self.message = None;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let res = Repo::open(&root).and_then(|r| f(&r)).map_err(|e| format!("{e:#}"));
            let _ = tx.send(res);
            ctx.request_repaint();
        });
    }

    fn poll_busy(&mut self, ctx: &egui::Context) {
        let Some((label, rx)) = &self.busy else { return };
        let res = match rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("l'opération a été interrompue".into()),
        };
        let label = label.clone();
        self.busy = None;
        self.message = Some(match res {
            Ok(m) if m.is_empty() => (format!("{label} : terminé"), false),
            Ok(m) => (format!("{label} : {m}"), false),
            Err(e) => (format!("{label} : {e}"), true),
        });
        self.refresh(ctx);
    }

    fn select(&mut self, idx: usize) {
        let (Some(snap), Some(repo)) = (&self.snap, &self.repo) else { return };
        self.selected = Some(idx);
        self.selected_file = 0;
        self.scroll_to_change = true;
        self.detail = Some(repo.commit_detail(&snap.commits[idx].id).map_err(|e| e.to_string()));
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Keep relative times fresh even when nothing else happens.
        ctx.request_repaint_after(std::time::Duration::from_secs(60));
        if let Some(path) = self.pending_open.take() {
            self.refresh_missing();
            self.open(&ctx, path);
        }
        self.poll_loading(&ctx);
        self.poll_busy(&ctx);

        // Shortcuts: Ctrl+F focuses the search, F3 / Shift+F3 move between results.
        let mut shortcut_step: Option<isize> = None;
        ctx.input_mut(|i| {
            if i.consume_key(egui::Modifiers::COMMAND, egui::Key::F) {
                if let Some(id) = self.search_id {
                    ctx.memory_mut(|m| m.request_focus(id));
                }
            }
            if i.consume_key(egui::Modifiers::SHIFT, egui::Key::F3) {
                shortcut_step = Some(-1);
            } else if i.consume_key(egui::Modifiers::NONE, egui::Key::F3) {
                shortcut_step = Some(1);
            }
        });

        // Files may have changed while the window was in the background.
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        if focused && !self.was_focused {
            self.refresh_status();
        }
        self.was_focused = focused;

        let mut to_open: Option<String> = None;
        let mut side = SideActions::default();
        let mut remote_action: Option<&'static str> = None;

        let mut search_changed = false;
        let mut search_step: Option<isize> = None;
        let mut open_organizer = false;
        let mut reorder: Option<(PathBuf, PathBuf, bool)> = None;
        let mut open_browser = false;
        egui::Panel::top("menubar").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("Projets", |ui| {
                    if ui.button("📂 Ouvrir un dossier…").clicked() {
                        open_browser = true;
                        ui.close();
                    }
                    ui.menu_button("Récents", |ui| {
                        let current = self.snap.as_ref().map(|s| s.root.as_path());
                        recent_items(ui, &self.config, &self.missing, current, &mut to_open, &mut reorder);
                    });
                    ui.separator();
                    if ui.button("Organiser les projets…").clicked() {
                        open_organizer = true;
                        ui.close();
                    }
                });
            });
        });
        egui::Panel::top("top").show(ui, |ui| {
            ui.horizontal(|ui| {
                // Project name, top left: a dropdown to switch between the recent projects.
                let name = self.snap.as_ref().map(|s| project_label(&s.root));
                let title = RichText::new(format!("{} ▾", name.unwrap_or_else(|| "Git Viewer".into())))
                    .strong()
                    .size(16.0);
                ui.menu_button(title, |ui| {
                    let current = self.snap.as_ref().map(|s| s.root.as_path());
                    recent_items(ui, &self.config, &self.missing, current, &mut to_open, &mut reorder);
                    ui.separator();
                    if ui.button("📂 Ouvrir un dossier…").clicked() {
                        open_browser = true;
                        ui.close();
                    }
                    if ui.button("Organiser les projets…").clicked() {
                        open_organizer = true;
                        ui.close();
                    }
                });
                ui.separator();
                if ui.button("⟳").on_hover_text("Recharger").clicked() {
                    self.refresh(&ctx);
                    self.refresh_status();
                }
                ui.separator();
                let n_changes = self.status.len();
                ui.selectable_value(&mut self.mode, Mode::History, "Historique");
                ui.selectable_value(
                    &mut self.mode,
                    Mode::Changes,
                    if n_changes > 0 { format!("Modifications ({n_changes})") } else { "Modifications".into() },
                );
                ui.separator();
                let can_remote = self.snap.as_ref().is_some_and(|s| !s.remotes.is_empty())
                    && self.busy.is_none();
                ui.add_enabled_ui(can_remote, |ui| {
                    if ui.button("⬇ Fetch").clicked() {
                        remote_action = Some("fetch");
                    }
                    if ui.button("⬇ Pull").on_hover_text("Fetch + fast-forward").clicked() {
                        remote_action = Some("pull");
                    }
                    if ui.button("⬆ Push").on_hover_text("Pousse la branche courante").clicked() {
                        remote_action = Some("push");
                    }
                });
                ui.separator();
                ui.label("🔍");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.search.query)
                        .hint_text("rechercher (Ctrl+F)")
                        .desired_width(180.0),
                );
                self.search_id = Some(resp.id);
                if resp.changed() {
                    search_changed = true;
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    search_step = Some(if ui.input(|i| i.modifiers.shift) { -1 } else { 1 });
                    resp.request_focus(); // keep typing / pressing Enter
                }
                if resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    self.search.query.clear();
                    search_changed = true;
                }
                if ui.small_button("▲").on_hover_text("Résultat précédent (Maj+Entrée, Maj+F3)").clicked() {
                    search_step = Some(-1);
                }
                if ui.small_button("▼").on_hover_text("Résultat suivant (Entrée, F3)").clicked() {
                    search_step = Some(1);
                }
                if !self.search.query.trim().is_empty() {
                    let n = self.search.matches.len();
                    if n == 0 {
                        ui.colored_label(Color32::from_rgb(0xe6, 0x5c, 0x6c), "aucun résultat");
                    } else {
                        let cur = self.search.current.map_or("–".to_string(), |c| (c + 1).to_string());
                        ui.weak(format!("{cur}/{n}"));
                    }
                }
                if self.loading.is_some() {
                    ui.spinner();
                } else if let Some(s) = &self.snap {
                    let n = s.commits.len();
                    if n >= COMMIT_LIMIT {
                        ui.weak(format!("{n} commits (limite atteinte)"));
                    } else {
                        ui.weak(format!("{n} commits"));
                    }
                }
            });
        });

        if let Some((from, to, after)) = reorder {
            self.config.reorder(&from, &to, after);
            self.save_config();
        }
        if open_browser {
            self.open_browser();
        }
        if open_organizer {
            self.refresh_missing();
            self.show_organizer = true;
        }

        egui::Panel::bottom("statusbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if let Some((label, _)) = &self.busy {
                    ui.spinner();
                    ui.label(format!("{label}…"));
                } else if let Some((msg, is_err)) = &self.message {
                    let color = if *is_err { Color32::from_rgb(0xe6, 0x5c, 0x6c) } else { Color32::from_rgb(0x5c, 0xb8, 0x5c) };
                    ui.colored_label(color, msg);
                    if ui.small_button("✕").clicked() {
                        self.message = None;
                    }
                } else if let Some(head) = self.snap.as_ref().and_then(|s| s.head.as_ref()) {
                    ui.weak(format!("branche : {head}"));
                } else if self.snap.is_some() {
                    ui.weak("HEAD détaché");
                }
            });
        });

        egui::Panel::left("refs_panel").resizable(true).default_size(270.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.side_tab, SideTab::Branches, "Branches");
                let n_tags = self.snap.as_ref().map_or(0, |s| s.tags.len());
                ui.selectable_value(&mut self.side_tab, SideTab::Tags, format!("Tags ({n_tags})"));
                let n = self.stashes.len();
                ui.selectable_value(
                    &mut self.side_tab,
                    SideTab::Stashes,
                    if n > 0 { format!("Stashes ({n})") } else { "Stashes".into() },
                );
            });
            ui.separator();
            match self.side_tab {
                SideTab::Branches => self.branches_tab(ui, &mut side),
                SideTab::Tags => self.tags_tab(ui, &mut side),
                SideTab::Stashes => self.stashes_tab(ui, &mut side),
            }
        });
        let SideActions {
            checkout,
            ask_delete,
            create_branch,
            checkout_remote,
            push_branch,
            jump_to,
            stash: stash_action,
        } = side;

        let mut change_actions: Vec<ChangeAction> = Vec::new();

        if self.mode == Mode::History && self.detail.is_some() {
            egui::Panel::bottom("detail")
                .resizable(true)
                .default_size(320.0)
                .show(ui, |ui| self.detail_ui(ui));
        }

        let mut row_action = None;
        let scroll_to = self.scroll_to.take();
        let mut save_columns = false;
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(e) = &self.error {
                ui.colored_label(Color32::RED, e);
            } else if self.snap.is_none() {
                if self.loading.is_some() {
                    ui.centered_and_justified(|ui| ui.spinner());
                }
            } else if self.mode == Mode::Changes {
                self.changes_ui(ui, &mut change_actions);
            } else if let Some(snap) = &self.snap {
                if snap.commits.is_empty() {
                    ui.weak("Aucun commit.");
                } else {
                    let (action, resized) = draw_graph(
                        ui,
                        snap,
                        self.selected,
                        &self.search,
                        scroll_to,
                        &mut self.config.columns,
                    );
                    row_action = action;
                    save_columns |= resized;
                }
            }
        });

        // ---- organizer window
        if self.show_organizer {
            enum Org {
                Pin(PathBuf),
                Up(PathBuf),
                Down(PathBuf),
                Remove(PathBuf),
                SortByName,
                PruneMissing,
                AskClear,
            }
            let mut act: Option<Org> = None;
            let mut open = true;
            let rows: Vec<PathBuf> = self.config.ordered_recent().into_iter().cloned().collect();
            egui::Window::new("Organiser les projets")
                .open(&mut open)
                .default_size([520.0, 380.0])
                .show(&ctx, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Trier A → Z").on_hover_text("Ordre alphabétique (une fois)").clicked() {
                            act = Some(Org::SortByName);
                        }
                        if ui
                            .add_enabled(!self.missing.is_empty(), egui::Button::new("Retirer les introuvables"))
                            .clicked()
                        {
                            act = Some(Org::PruneMissing);
                        }
                        if ui
                            .add_enabled(rows.len() > self.config.pinned.len(), egui::Button::new("Vider la liste"))
                            .on_hover_text("Les projets épinglés sont conservés")
                            .clicked()
                        {
                            act = Some(Org::AskClear);
                        }
                    });
                    ui.weak("📌 épinglé : reste en haut de la liste. ↑ ↓ réordonnent dans chaque groupe.");
                    ui.separator();
                    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                        if rows.is_empty() {
                            ui.weak("Aucun projet récent.");
                        }
                        for p in &rows {
                            let pinned = self.config.is_pinned(p);
                            let missing = self.missing.contains(p);
                            ui.horizontal(|ui| {
                                if ui.selectable_label(pinned, "📌").on_hover_text(if pinned { "Désépingler" } else { "Épingler" }).clicked() {
                                    act = Some(Org::Pin(p.clone()));
                                }
                                if ui.small_button("↑").clicked() {
                                    act = Some(Org::Up(p.clone()));
                                }
                                if ui.small_button("↓").clicked() {
                                    act = Some(Org::Down(p.clone()));
                                }
                                if ui.small_button("✕").on_hover_text("Oublier ce projet").clicked() {
                                    act = Some(Org::Remove(p.clone()));
                                }
                                let label = if missing {
                                    RichText::new(format!("{} (introuvable)", project_label(p))).color(Color32::from_rgb(0xe6, 0x5c, 0x6c))
                                } else {
                                    RichText::new(project_label(p))
                                };
                                if ui.add_enabled(!missing, egui::Button::new(label).frame(false)).on_hover_text(p.to_string_lossy()).clicked() {
                                    to_open = Some(p.to_string_lossy().into_owned());
                                }
                                ui.weak(p.to_string_lossy());
                            });
                        }
                    });
                });
            if !open {
                self.show_organizer = false;
            }
            if let Some(a) = act {
                match a {
                    Org::Pin(p) => self.config.toggle_pin(&p),
                    Org::Up(p) => self.config.move_recent(&p, true),
                    Org::Down(p) => self.config.move_recent(&p, false),
                    Org::Remove(p) => self.config.remove_recent(&p),
                    Org::SortByName => self.config.sort_recent_by_name(),
                    Org::PruneMissing => {
                        let n = self.config.prune_missing(|p| p.is_dir());
                        self.message = Some((format!("{n} projet(s) introuvable(s) retiré(s)"), false));
                    }
                    Org::AskClear => self.confirm_clear = true,
                }
                self.refresh_missing();
                self.save_config();
            }
        }
        if self.confirm_clear {
            let (mut yes, mut no) = (false, false);
            egui::Window::new("Vider la liste ?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(&ctx, |ui| {
                    let n = self.config.recent.len() - self.config.pinned.len();
                    ui.label(format!("Oublier {n} projet(s) récent(s) ?"));
                    ui.weak("Les dossiers ne sont pas supprimés, et les projets épinglés sont conservés.");
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        yes = ui.button(RichText::new("Vider").color(Color32::from_rgb(0xe6, 0x5c, 0x6c))).clicked();
                        no = ui.button("Annuler").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape));
                    });
                });
            if yes {
                self.config.clear_unpinned();
                self.confirm_clear = false;
                self.refresh_missing();
                self.save_config();
            } else if no {
                self.confirm_clear = false;
            }
        }

        if save_columns {
            self.save_config(); // a column was resized: remember it
        }

        // ---- folder picker
        if let Some(browser) = &mut self.browser {
            let mut open = true;
            if let Some(dir) = browser.show(&ctx, &mut open) {
                to_open = Some(dir.to_string_lossy().into_owned());
            }
            if !open {
                self.browser = None;
            }
        }

        // ---- prompt dialog (new branch / tag at a commit)
        if let Some(mut prompt) = self.prompt.take() {
            let title = match prompt.kind {
                PromptKind::Branch => "Nouvelle branche ici",
                PromptKind::Tag => "Nouveau tag ici",
            };
            let (mut ok, mut cancel) = (false, false);
            egui::Window::new(title).collapsible(false).resizable(false).show(&ctx, |ui| {
                let r = ui.text_edit_singleline(&mut prompt.text);
                r.request_focus();
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    ok = true;
                }
                ui.horizontal(|ui| {
                    ok |= ui.button("Créer").clicked();
                    cancel = ui.button("Annuler").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape));
                });
            });
            if ok && !prompt.text.trim().is_empty() {
                let id = self.snap.as_ref().map(|s| s.commits[prompt.commit].id.clone());
                let name = prompt.text.trim().to_string();
                if let Some(id) = id {
                    match prompt.kind {
                        PromptKind::Branch => self.local_op(&ctx, true, |r| {
                            r.create_branch(&name, Some(&id))?;
                            Ok(format!("branche « {name} » créée"))
                        }),
                        PromptKind::Tag => self.local_op(&ctx, true, |r| {
                            r.create_tag(&name, &id)?;
                            Ok(format!("tag « {name} » créé"))
                        }),
                    }
                }
            } else if !cancel && !ok {
                self.prompt = Some(prompt);
            }
        }

        // ---- branch deletion: ask first
        if let Some(name) = ask_delete {
            let unmerged = self
                .repo
                .as_ref()
                .map(|r| r.is_merged(&name).map(|m| !m).unwrap_or(true))
                .unwrap_or(true);
            self.confirm_delete = Some((name, unmerged));
        }
        if let Some((name, unmerged)) = self.confirm_delete.clone() {
            let (mut yes, mut no) = (false, false);
            egui::Window::new("Supprimer la branche ?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(&ctx, |ui| {
                    ui.label(format!("Supprimer la branche locale « {name} » ?"));
                    if unmerged {
                        ui.colored_label(
                            Color32::from_rgb(0xe6, 0x7e, 0x3c),
                            "⚠ Elle contient des commits qui ne sont pas fusionnés dans HEAD : \
                             ils ne seront plus accessibles depuis aucune branche.",
                        );
                    } else {
                        ui.weak("Ses commits sont déjà dans HEAD, rien ne sera perdu.");
                    }
                    ui.weak("La branche distante éventuelle n'est pas touchée.");
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        yes = ui
                            .button(RichText::new("Supprimer").color(Color32::from_rgb(0xe6, 0x5c, 0x6c)))
                            .clicked();
                        no = ui.button("Annuler").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape));
                    });
                });
            if yes {
                self.confirm_delete = None;
                self.local_op(&ctx, true, |r| {
                    r.delete_branch(&name)?;
                    Ok(format!("branche « {name} » supprimée"))
                });
            } else if no {
                self.confirm_delete = None;
            }
        }

        // ---- search
        if search_changed {
            self.update_search(true);
        }
        if let Some(d) = search_step.or(shortcut_step) {
            self.step_search(d);
        }

        // ---- apply deferred actions
        match row_action {
            Some(RowAction::Select(i)) => self.select(i),
            Some(RowAction::NewBranch(i)) => {
                self.prompt = Some(Prompt { kind: PromptKind::Branch, commit: i, text: String::new() })
            }
            Some(RowAction::NewTag(i)) => {
                self.prompt = Some(Prompt { kind: PromptKind::Tag, commit: i, text: String::new() })
            }
            Some(RowAction::CheckoutDetached(i)) => {
                if let Some(id) = self.snap.as_ref().map(|s| s.commits[i].id.clone()) {
                    self.local_op(&ctx, true, |r| {
                        r.checkout_commit(&id)?;
                        Ok(format!("HEAD détaché sur {}", &id[..7]))
                    });
                }
            }
            Some(RowAction::CopyHash(i)) => {
                if let Some(s) = &self.snap {
                    ctx.copy_text(s.commits[i].id.clone());
                }
            }
            None => {}
        }

        for action in change_actions {
            match action {
                ChangeAction::Stage(p) => self.local_op(&ctx, false, |r| r.stage(&p).map(|_| String::new())),
                ChangeAction::Unstage(p) => self.local_op(&ctx, false, |r| r.unstage(&p).map(|_| String::new())),
                ChangeAction::StageAll => self.local_op(&ctx, false, |r| r.stage_all().map(|_| String::new())),
                ChangeAction::UnstageAll => self.local_op(&ctx, false, |r| r.unstage_all().map(|_| String::new())),
                ChangeAction::Select(p, staged) => self.load_change_diff(&p, staged),
                ChangeAction::Commit => {
                    let msg = self.commit_msg.clone();
                    let mut ok = false;
                    self.local_op(&ctx, true, |r| {
                        let id = r.commit(&msg)?;
                        ok = true;
                        Ok(format!("commit {} créé", &id.to_string()[..7]))
                    });
                    if ok {
                        self.commit_msg.clear();
                    }
                }
            }
        }

        if let Some((remote, name)) = checkout_remote {
            self.local_op(&ctx, true, |r| {
                r.checkout_remote_branch(&remote, &name)?;
                Ok(format!("basculé sur « {name} » (suit {remote}/{name})"))
            });
        }
        if let Some(id) = jump_to {
            let idx = self.snap.as_ref().and_then(|s| s.commits.iter().position(|c| c.id == id));
            match idx {
                Some(i) => {
                    self.mode = Mode::History;
                    self.select(i);
                    self.scroll_to = Some(i);
                }
                None => {
                    self.message = Some(("ce commit n'est pas dans la liste chargée".into(), true));
                }
            }
        }
        match stash_action {
            Some(StashAction::Save) => {
                let (msg, untracked) = (self.stash_msg.clone(), self.stash_untracked);
                let mut ok = false;
                self.local_op(&ctx, false, |r| {
                    r.stash_save(&msg, untracked)?;
                    ok = true;
                    Ok("modifications mises de côté".into())
                });
                if ok {
                    self.stash_msg.clear();
                }
            }
            Some(StashAction::Apply(i)) => self.local_op(&ctx, false, |r| {
                r.stash_apply(i)?;
                Ok("stash appliqué (conservé dans la liste)".into())
            }),
            Some(StashAction::Pop(i)) => self.local_op(&ctx, false, |r| {
                r.stash_pop(i)?;
                Ok("stash appliqué et supprimé".into())
            }),
            Some(StashAction::AskDrop(i)) => self.confirm_drop_stash = Some(i),
            None => {}
        }
        if let Some(i) = self.confirm_drop_stash {
            let (mut yes, mut no) = (false, false);
            egui::Window::new("Supprimer le stash ?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(&ctx, |ui| {
                    let msg = self.stashes.iter().find(|e| e.index == i).map_or("", |e| e.message.as_str());
                    ui.label(format!("Supprimer définitivement stash@{{{i}}} ?"));
                    ui.weak(msg);
                    ui.colored_label(
                        Color32::from_rgb(0xe6, 0x7e, 0x3c),
                        "⚠ Les modifications qu'il contient seront perdues.",
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        yes = ui.button(RichText::new("Supprimer").color(Color32::from_rgb(0xe6, 0x5c, 0x6c))).clicked();
                        no = ui.button("Annuler").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape));
                    });
                });
            if yes {
                self.confirm_drop_stash = None;
                self.local_op(&ctx, false, |r| {
                    r.stash_drop(i)?;
                    Ok(format!("stash@{{{i}}} supprimé"))
                });
            } else if no {
                self.confirm_drop_stash = None;
            }
        }
        if let Some(name) = checkout {
            self.local_op(&ctx, true, |r| {
                r.checkout_branch(&name)?;
                Ok(format!("basculé sur « {name} »"))
            });
        }
        if let Some(name) = create_branch {
            self.local_op(&ctx, true, |r| {
                r.create_branch(&name, None)?;
                r.checkout_branch(&name)?;
                Ok(format!("branche « {name} » créée"))
            });
            self.new_branch.clear();
        }
        let default_remote: Option<String> = self
            .snap
            .as_ref()
            .and_then(|s| s.remotes.iter().find(|r| *r == "origin").or(s.remotes.first()).cloned());
        if let Some(branch) = push_branch {
            match default_remote.clone() {
                Some(remote) => self.remote_op(&ctx, "push", move |r| {
                    r.push(&remote, &branch)?;
                    Ok(format!("« {branch} » publiée sur {remote}"))
                }),
                None => self.message = Some(("aucun remote configuré : ajoute-en un avec git remote add".into(), true)),
            }
        }
        if let Some(action) = remote_action {
            let remote = default_remote.unwrap_or_default();
            let branch = self.snap.as_ref().and_then(|s| s.head.clone());
            match action {
                "fetch" => self.remote_op(&ctx, "fetch", move |r| r.fetch(&remote).map(|_| String::new())),
                "pull" => self.remote_op(&ctx, "pull", |r| r.pull()),
                _ => match branch {
                    Some(b) => self.remote_op(&ctx, "push", move |r| {
                        r.push(&remote, &b)?;
                        Ok(format!("« {b} » publiée sur {remote}"))
                    }),
                    None => self.message = Some(("push impossible : HEAD est détaché".into(), true)),
                },
            }
        }
        if let Some(p) = to_open {
            self.open(&ctx, p);
        }
    }
}

impl App {
    fn branches_tab(&mut self, ui: &mut egui::Ui, side: &mut SideActions) {
        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.new_branch)
                    .hint_text("nouvelle branche")
                    .desired_width(150.0),
            );
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.button("＋").on_hover_text("Créer à partir de HEAD et basculer").clicked() || enter)
                && !self.new_branch.trim().is_empty()
            {
                side.create_branch = Some(self.new_branch.trim().to_string());
            }
        });
        let Some(s) = &self.snap else { return };

        // Top panel: what exists on the remotes. Bottom panel: local branches.
        egui::Panel::top("remote_branches")
            .resizable(true)
            .default_size(ui.available_height() * 0.45)
            .show(ui, |ui| {
                ui.strong("Branches distantes");
                egui::ScrollArea::vertical().id_salt("remote_scroll").auto_shrink(false).show(ui, |ui| {
                    if s.remote_branches.is_empty() {
                        ui.weak("Aucune branche distante (fais un fetch).");
                    }
                    let mut remotes: Vec<&str> = s.remote_branches.iter().map(|b| b.remote.as_str()).collect();
                    remotes.dedup(); // already sorted by remote
                    for remote in remotes {
                        egui::CollapsingHeader::new(remote)
                            .id_salt(("remote", remote))
                            .default_open(true)
                            .show(ui, |ui| {
                                for b in s.remote_branches.iter().filter(|b| b.remote == remote) {
                                    let r = ui
                                        .selectable_label(false, &b.name)
                                        .on_hover_text("Clic : aller au commit · Double-clic : basculer");
                                    if r.double_clicked() {
                                        side.checkout_remote = Some((b.remote.clone(), b.name.clone()));
                                    } else if r.clicked() {
                                        side.jump_to = Some(b.tip.clone());
                                    }
                                    r.context_menu(|ui| {
                                        if ui.button("Aller au commit").clicked() {
                                            side.jump_to = Some(b.tip.clone());
                                            ui.close();
                                        }
                                        if ui.button("Basculer (crée la branche locale)").clicked() {
                                            side.checkout_remote = Some((b.remote.clone(), b.name.clone()));
                                            ui.close();
                                        }
                                    });
                                }
                            });
                    }
                });
            });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.strong("Branches locales");
            egui::ScrollArea::vertical().id_salt("local_scroll").auto_shrink(false).show(ui, |ui| {
                if s.branches.is_empty() {
                    ui.weak("Aucune branche locale.");
                }
                for b in &s.branches {
                    ui.horizontal(|ui| {
                        let r = ui
                            .selectable_label(b.is_head, &b.name)
                            .on_hover_text("Clic : aller au commit · Double-clic : basculer");
                        if r.double_clicked() && !b.is_head {
                            side.checkout = Some(b.name.clone());
                        } else if r.clicked() {
                            side.jump_to = Some(b.tip.clone());
                        }
                        r.context_menu(|ui| {
                            if ui.button("Aller au commit").clicked() {
                                side.jump_to = Some(b.tip.clone());
                                ui.close();
                            }
                            if ui.add_enabled(!b.is_head, egui::Button::new("Basculer")).clicked() {
                                side.checkout = Some(b.name.clone());
                                ui.close();
                            }
                            let label = if b.has_upstream { "Pousser" } else { "Publier sur le remote" };
                            if ui.button(label).clicked() {
                                side.push_branch = Some(b.name.clone());
                                ui.close();
                            }
                            if ui.add_enabled(!b.is_head, egui::Button::new("Supprimer…")).clicked() {
                                side.ask_delete = Some(b.name.clone());
                                ui.close();
                            }
                        });
                        if !b.has_upstream {
                            if ui.small_button("⬆").on_hover_text("Branche non publiée : la publier sur le remote").clicked() {
                                side.push_branch = Some(b.name.clone());
                            }
                        }
                        if !b.is_head && ui.small_button("✕").on_hover_text("Supprimer la branche").clicked() {
                            side.ask_delete = Some(b.name.clone());
                        }
                    });
                }
            });
        });
    }

    fn tags_tab(&mut self, ui: &mut egui::Ui, side: &mut SideActions) {
        let Some(s) = &self.snap else { return };
        if s.tags.is_empty() {
            ui.weak("Aucun tag. Clic droit sur un commit pour en créer un.");
            return;
        }
        egui::ScrollArea::vertical().id_salt("tags_scroll").auto_shrink(false).show(ui, |ui| {
            for t in &s.tags {
                let r = ui.selectable_label(false, &t.name).on_hover_text("Clic : aller au commit");
                if r.clicked() {
                    side.jump_to = Some(t.tip.clone());
                }
                r.context_menu(|ui| {
                    if ui.button("Aller au commit").clicked() {
                        side.jump_to = Some(t.tip.clone());
                        ui.close();
                    }
                    if ui.button("Copier le nom").clicked() {
                        ui.ctx().copy_text(t.name.clone());
                        ui.close();
                    }
                });
            }
        });
    }

    fn stashes_tab(&mut self, ui: &mut egui::Ui, side: &mut SideActions) {
        ui.add(
            egui::TextEdit::singleline(&mut self.stash_msg)
                .hint_text("message (facultatif)")
                .desired_width(f32::INFINITY),
        );
        ui.checkbox(&mut self.stash_untracked, "inclure les fichiers non suivis");
        let dirty = !self.status.is_empty();
        if ui
            .add_enabled(dirty, egui::Button::new("Mettre de côté les modifications"))
            .on_disabled_hover_text("Aucune modification à mettre de côté")
            .clicked()
        {
            side.stash = Some(StashAction::Save);
        }
        ui.separator();
        if self.stashes.is_empty() {
            ui.weak("Aucun stash.");
        }
        egui::ScrollArea::vertical().id_salt("stash_scroll").auto_shrink(false).show(ui, |ui| {
            for e in &self.stashes {
                ui.group(|ui| {
                    ui.label(RichText::new(format!("stash@{{{}}}", e.index)).monospace().weak());
                    ui.add(egui::Label::new(&e.message).wrap());
                    ui.horizontal(|ui| {
                        if ui.small_button("Appliquer").on_hover_text("Applique et garde le stash").clicked() {
                            side.stash = Some(StashAction::Apply(e.index));
                        }
                        if ui.small_button("Pop").on_hover_text("Applique puis supprime le stash").clicked() {
                            side.stash = Some(StashAction::Pop(e.index));
                        }
                        if ui.small_button("Supprimer…").clicked() {
                            side.stash = Some(StashAction::AskDrop(e.index));
                        }
                    });
                });
            }
        });
    }

    fn changes_ui(&mut self, ui: &mut egui::Ui, actions: &mut Vec<ChangeAction>) {
        let staged: Vec<&StatusEntry> = self.status.iter().filter(|e| e.staged.is_some()).collect();
        let unstaged: Vec<&StatusEntry> = self.status.iter().filter(|e| e.unstaged.is_some()).collect();
        let selected = self.change_sel.clone();

        egui::Panel::left("changes_list").resizable(true).default_size(380.0).show(ui, |ui| {
            // Commit box first, so it stays visible below the lists.
            egui::Panel::bottom("commit_box").show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.commit_msg)
                        .hint_text("Message de commit")
                        .desired_rows(4)
                        .desired_width(f32::INFINITY),
                );
                let can = !staged.is_empty() && !self.commit_msg.trim().is_empty();
                if ui
                    .add_enabled(can, egui::Button::new(format!("Commiter ({} fichier(s))", staged.len())))
                    .clicked()
                {
                    actions.push(ChangeAction::Commit);
                }
            });
            egui::ScrollArea::vertical().id_salt("changes").auto_shrink(false).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong(format!("Indexées ({})", staged.len()));
                    if !staged.is_empty() && ui.small_button("Tout désindexer").clicked() {
                        actions.push(ChangeAction::UnstageAll);
                    }
                });
                for e in &staged {
                    change_row(ui, e, e.staged.unwrap(), true, &selected, actions);
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.strong(format!("Non indexées ({})", unstaged.len()));
                    if !unstaged.is_empty() && ui.small_button("Tout indexer").clicked() {
                        actions.push(ChangeAction::StageAll);
                    }
                });
                for e in &unstaged {
                    change_row(ui, e, e.unstaged.unwrap(), false, &selected, actions);
                }
                if self.status.is_empty() {
                    ui.weak("Aucune modification.");
                }
            });
        });
        // Taken out of `self` so the highlighter (which needs `&mut self`) can run meanwhile.
        let diff = self.change_diff.take();
        egui::CentralPanel::default().show(ui, |ui| match &diff {
            Some(f) if f.binary => {
                ui.weak("Fichier binaire");
            }
            Some(f) => {
                let hl = self.highlights_for(ui.ctx(), ui.visuals().dark_mode, f);
                diff_ui(ui, f, hl.as_deref(), None);
            }
            None => {
                ui.weak("Sélectionne un fichier pour voir ses modifications.");
            }
        });
        self.change_diff = diff;
    }
}

/// `"src/app/main.rs"` → `("main.rs", "src/app")`; a path without folder gives an empty folder.
fn split_path(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((dir, name)) => (name, dir),
        None => (path, ""),
    }
}

/// Text of a file row: the file name first, its folder dimmed after it. When the row is too
/// narrow it is the end of the folder that gets cut, never the file name.
fn file_label(ui: &egui::Ui, path: &str) -> egui::text::LayoutJob {
    let (name, dir) = split_path(path);
    let font = egui::TextStyle::Body.resolve(ui.style());
    let mut job = egui::text::LayoutJob::default();
    job.append(
        name,
        0.0,
        egui::TextFormat { font_id: font.clone(), color: ui.visuals().text_color(), ..Default::default() },
    );
    if !dir.is_empty() {
        job.append(
            &format!("  {dir}"),
            0.0,
            egui::TextFormat { font_id: font, color: ui.visuals().weak_text_color(), ..Default::default() },
        );
    }
    job
}

fn change_row(
    ui: &mut egui::Ui,
    e: &StatusEntry,
    code: char,
    staged: bool,
    selected: &Option<(String, bool)>,
    actions: &mut Vec<ChangeAction>,
) {
    let color = match code {
        'A' | '?' => Color32::from_rgb(0x5c, 0xb8, 0x5c),
        'D' => Color32::from_rgb(0xe6, 0x5c, 0x6c),
        'U' => Color32::from_rgb(0xe6, 0x7e, 0x3c),
        _ => Color32::from_rgb(0xe6, 0xc2, 0x3c),
    };
    ui.horizontal(|ui| {
        let (btn, tip) = if staged { ("−", "Désindexer") } else { ("+", "Indexer") };
        if ui.small_button(btn).on_hover_text(tip).clicked() {
            actions.push(if staged { ChangeAction::Unstage(e.path.clone()) } else { ChangeAction::Stage(e.path.clone()) });
        }
        ui.label(RichText::new(code.to_string()).monospace().color(color));
        let is_sel = selected.as_ref().is_some_and(|(p, s)| *p == e.path && *s == staged);
        let label = file_label(ui, &e.path);
        let r = ui.add(egui::Button::selectable(is_sel, label).truncate()).on_hover_text(&e.path);
        if r.clicked() {
            actions.push(ChangeAction::Select(e.path.clone(), staged));
        }
    });
}

impl App {
    fn detail_ui(&mut self, ui: &mut egui::Ui) {
        // Take the detail out so the closures below can freely borrow `self`.
        let Some(detail) = self.detail.take() else { return };
        let mut selected_file = self.selected_file;
        match &detail {
            Err(e) => {
                ui.colored_label(Color32::RED, e);
            }
            Ok(d) => {
                ui.horizontal_wrapped(|ui| {
                    ui.monospace(&d.id[..d.id.len().min(10)]).on_hover_text(&d.id);
                    ui.label(format!("{} <{}>", d.author, d.email));
                    ui.weak(format!("{} ({})", timefmt::relative_to_now(d.time), timefmt::absolute(d.time)));
                    if !d.parents.is_empty() {
                        let ps: Vec<_> = d.parents.iter().map(|p| &p[..7]).collect();
                        ui.weak(format!("parent{} : {}", if ps.len() > 1 { "s" } else { "" }, ps.join(", ")));
                    }
                });
                egui::ScrollArea::vertical().id_salt("msg").max_height(90.0).show(ui, |ui| {
                    ui.label(RichText::new(&d.message).strong());
                });
                ui.separator();

                egui::Panel::left("files").resizable(true).default_size(300.0).show(ui, |ui| {
                    ui.weak(format!("{} fichier(s) modifié(s)", d.files.len()));
                    // Vertical scrolling only: a horizontal scroll bar would cover the last file. Long
                    // paths are cut with "…" instead (the full path is in the tooltip).
                    egui::ScrollArea::vertical().id_salt("filelist").auto_shrink(false).show(ui, |ui| {
                        for (i, f) in d.files.iter().enumerate() {
                            let color = match f.status {
                                'A' => Color32::from_rgb(0x5c, 0xb8, 0x5c),
                                'D' => Color32::from_rgb(0xe6, 0x5c, 0x6c),
                                _ => Color32::from_rgb(0xe6, 0xc2, 0x3c),
                            };
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(f.status.to_string()).monospace().color(color));
                                ui.label(
                                    RichText::new(format!("+{} −{}", f.additions, f.deletions)).monospace().weak(),
                                );
                                let label = file_label(ui, &f.path);
                                let r = ui
                                    .add(egui::Button::selectable(selected_file == i, label).truncate())
                                    .on_hover_text(&f.path);
                                if r.clicked() {
                                    selected_file = i;
                                }
                            });
                        }
                    });
                });
                egui::CentralPanel::default().show(ui, |ui| {
                    let Some(file) = d.files.get(selected_file) else { return };

                    // Changed lines, whole file, or previous / next version side by side.
                    let mut view = self.config.diff_view;
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut view, DiffView::Hunks, "Lignes modifiées");
                        ui.selectable_value(&mut view, DiffView::Full, "Fichier entier");
                        ui.selectable_value(&mut view, DiffView::Split, "Côte à côte");
                    });
                    if view != self.config.diff_view {
                        self.config.diff_view = view;
                        self.save_config();
                        self.scroll_to_change = true;
                    }
                    let full_view = view != DiffView::Hunks;
                    if file.binary {
                        ui.weak("Fichier binaire");
                        return;
                    }

                    // The whole-file diff is computed when asked for, and kept while it is on screen.
                    let (mut full, mut cut) = (None, false);
                    if full_view {
                        let cached = self.full_cache.take().filter(|(c, p, ..)| *c == d.id && *p == file.path);
                        match cached {
                            Some((_, _, f, c)) => (full, cut) = (Some(f), c),
                            None => match self
                                .repo
                                .as_ref()
                                .map(|r| r.file_diff_full(&d.id, &file.path, file.old_path.as_deref()))
                            {
                                Some(Ok((f, c))) => (full, cut) = (Some(f), c),
                                Some(Err(e)) => {
                                    ui.colored_label(Color32::from_rgb(0xe6, 0x5c, 0x6c), format!("{e:#}"));
                                }
                                None => {}
                            },
                        }
                    }
                    let shown = full.as_ref().unwrap_or(file);
                    let want_scroll = std::mem::take(&mut self.scroll_to_change);
                    let hl = self.highlights_for(ui.ctx(), ui.visuals().dark_mode, shown);
                    if view == DiffView::Split {
                        let (uid, rows) = match self.split_cache.take() {
                            Some((uid, rows)) if uid == shown.uid => (uid, rows),
                            _ => (shown.uid, split::split_rows(&shown.lines)),
                        };
                        let scroll = want_scroll.then(|| split::first_change_row(&rows)).flatten();
                        split_ui(ui, shown, hl.as_deref(), &rows, scroll);
                        self.split_cache = Some((uid, rows));
                    } else {
                        let scroll = want_scroll.then(|| full.as_ref().and_then(first_change_line)).flatten();
                        diff_ui(ui, shown, hl.as_deref(), scroll);
                    }
                    if d.truncated || cut {
                        ui.colored_label(Color32::YELLOW, "Diff tronqué (fichier ou commit très volumineux).");
                    }
                    if let Some(f) = full {
                        self.full_cache = Some((d.id.clone(), file.path.clone(), f, cut));
                    }
                });
            }
        }
        // Another file selected: show its first change.
        if selected_file != self.selected_file {
            self.scroll_to_change = true;
        }
        self.selected_file = selected_file;
        self.detail = Some(detail);
    }
}

/// Side-by-side view: the previous version of the file on the left, the new one on the right,
/// rows aligned. Both sides scroll together since they are the same rows.
fn split_ui(ui: &mut egui::Ui, file: &FileDiff, hl: Option<&Highlighted>, rows: &[SplitRow], scroll_to: Option<usize>) {
    ui.horizontal(|ui| {
        ui.strong(&file.path);
        if let Some(old) = &file.old_path {
            ui.weak(format!("(renommé depuis {old})"));
        }
        if let Some(lang) = hl.and_then(|h| h.language) {
            ui.weak(lang);
        }
    });
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 2.0;
    let mut area = egui::ScrollArea::vertical().id_salt("split").auto_shrink(false);
    if let Some(row) = scroll_to {
        area = area.vertical_scroll_offset(row as f32 * row_h);
    }
    area.show_rows(ui, row_h, rows.len(), |ui, range| {
        let width = ui.available_width();
        for row in &rows[range] {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(width, row_h), egui::Sense::hover());
            let p = ui.painter_at(rect);
            match row {
                SplitRow::Separator => {
                    p.text(rect.center(), egui::Align2::CENTER_CENTER, "⋯", font.clone(), ui.visuals().weak_text_color());
                }
                SplitRow::Pair { left, right } => {
                    let half = (width - 1.0) / 2.0;
                    let left_rect = egui::Rect::from_min_size(rect.min, egui::vec2(half, row_h));
                    let right_rect = egui::Rect::from_min_size(rect.min + egui::vec2(half + 1.0, 0.0), egui::vec2(half, row_h));
                    paint_cell(ui, &p, left_rect, left.as_ref(), file, hl, &font);
                    paint_cell(ui, &p, right_rect, right.as_ref(), file, hl, &font);
                    p.vline(rect.left() + half + 0.5, rect.y_range(), ui.visuals().widgets.noninteractive.bg_stroke);
                }
            }
        }
    });
}

/// One side of a split row: line number in a gutter, then the (colored) text cut at the cell's edge.
fn paint_cell(
    ui: &egui::Ui,
    p: &egui::Painter,
    rect: egui::Rect,
    cell: Option<&Cell>,
    file: &FileDiff,
    hl: Option<&Highlighted>,
    font: &egui::FontId,
) {
    const GUTTER: f32 = 46.0;
    let Some(cell) = cell else {
        // Nothing on this side: the other side has lines that do not exist here.
        p.rect_filled(rect, 0.0, ui.visuals().faint_bg_color);
        return;
    };
    let bg = match cell.kind {
        CellKind::Removed => Color32::from_rgba_unmultiplied(248, 81, 73, 45),
        CellKind::Added => Color32::from_rgba_unmultiplied(46, 160, 67, 45),
        CellKind::Context => Color32::TRANSPARENT,
    };
    p.rect_filled(rect, 0.0, bg);
    let mid = rect.center().y;
    p.text(
        egui::pos2(rect.left() + GUTTER - 6.0, mid),
        egui::Align2::RIGHT_CENTER,
        cell.number.to_string(),
        font.clone(),
        ui.visuals().weak_text_color(),
    );

    let text_color = ui.visuals().text_color();
    let line = &file.lines[cell.line];
    let fmt = |color: Color32| egui::TextFormat { font_id: font.clone(), color, ..Default::default() };
    let mut job = egui::text::LayoutJob::default();
    match hl.and_then(|h| h.lines.get(cell.line)).filter(|spans| !spans.is_empty()) {
        Some(spans) => {
            for s in spans {
                job.append(&s.text, 0.0, fmt(Color32::from_rgb(s.color[0], s.color[1], s.color[2])));
            }
        }
        None => job.append(&line.text.replace('\t', "    "), 0.0, fmt(text_color)),
    }
    let galley = p.layout_job(job);
    let clip = egui::Rect::from_min_max(egui::pos2(rect.left() + GUTTER, rect.top()), rect.max);
    p.with_clip_rect(clip).galley(egui::pos2(rect.left() + GUTTER, mid - galley.size().y / 2.0), galley, text_color);
}

/// Index of the line to scroll to so the first change is visible with a little context above it.
fn first_change_line(file: &FileDiff) -> Option<usize> {
    let first = file.lines.iter().position(|l| matches!(l.origin, '+' | '-'))?;
    Some(first.saturating_sub(3))
}

/// `scroll_to`: line index to bring to the top of the view (once).
fn diff_ui(ui: &mut egui::Ui, file: &FileDiff, hl: Option<&Highlighted>, scroll_to: Option<usize>) {
    ui.horizontal(|ui| {
        ui.strong(&file.path);
        if let Some(lang) = hl.and_then(|h| h.language) {
            ui.weak(lang);
        }
    });
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
    let text_color = ui.visuals().text_color();
    let weak = ui.visuals().weak_text_color();
    let mut area = egui::ScrollArea::both().id_salt("diff").auto_shrink(false);
    if let Some(line) = scroll_to {
        area = area.vertical_scroll_offset(line as f32 * row_h);
    }
    area.show_rows(
        ui,
        row_h,
        file.lines.len(),
        |ui, range| {
            for idx in range {
                let l = &file.lines[idx];
                let (prefix, prefix_color, bg) = match l.origin {
                    '+' => ("+ ", Color32::from_rgb(0x5c, 0xb8, 0x5c), Color32::from_rgba_unmultiplied(46, 160, 67, 45)),
                    '-' => ("- ", Color32::from_rgb(0xe6, 0x5c, 0x6c), Color32::from_rgba_unmultiplied(248, 81, 73, 45)),
                    'H' => ("", weak, Color32::from_rgba_unmultiplied(56, 139, 253, 45)),
                    _ => ("  ", weak, Color32::TRANSPARENT),
                };
                let fmt = |color: Color32| egui::TextFormat {
                    font_id: font.clone(),
                    color,
                    background: bg,
                    ..Default::default()
                };
                let mut job = egui::text::LayoutJob::default();
                if l.origin == 'H' {
                    job.append(&l.text, 0.0, fmt(text_color));
                } else {
                    job.append(prefix, 0.0, fmt(prefix_color));
                    match hl.and_then(|h| h.lines.get(idx)).filter(|spans| !spans.is_empty()) {
                        Some(spans) => {
                            for s in spans {
                                job.append(&s.text, 0.0, fmt(Color32::from_rgb(s.color[0], s.color[1], s.color[2])));
                            }
                        }
                        None => job.append(&l.text.replace('\t', "    "), 0.0, fmt(text_color)),
                    }
                }
                ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Extend));
            }
        },
    );
}

/// Paints a highlight behind the first occurrence of `query` in `text` drawn at (`x`, `mid`).
fn highlight_span(p: &egui::Painter, x: f32, mid: f32, text: &str, query: &str, font: &egui::FontId) {
    let Some(r) = search::find_ci(text, query) else { return };
    let width = |s: &str| p.layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE).size();
    let before = width(&text[..r.start]).x;
    let found = width(&text[r.clone()]);
    let rect = egui::Rect::from_min_size(
        egui::pos2(x + before - 1.0, mid - found.y / 2.0),
        egui::vec2(found.x + 2.0, found.y),
    );
    p.rect_filled(rect, 2.0, Color32::from_rgba_unmultiplied(255, 200, 0, 140));
}

/// Room taken at the end of a project row by the drag grip (16 px) and the spacing before it.
const GRIP_ROW_W: f32 = 26.0;

/// The recent projects as menu entries (pinned first); the open one is ticked, missing ones are
/// greyed out. Clicking one stores its path in `to_open`. Entries can be dragged to reorder them
/// within their group (pinned / not pinned): the drop is reported in `reorder` as
/// `(dragged, target, after_target)`.
fn recent_items(
    ui: &mut egui::Ui,
    config: &Config,
    missing: &HashSet<PathBuf>,
    current: Option<&Path>,
    to_open: &mut Option<String>,
    reorder: &mut Option<(PathBuf, PathBuf, bool)>,
) {
    if config.recent.is_empty() {
        ui.weak("Aucun projet récent");
    }
    let label_of = |p: &PathBuf| {
        let tick = if current == Some(p.as_path()) { "✔ " } else { "" };
        let pin = if config.is_pinned(p) { "📌 " } else { "" };
        let gone = if missing.contains(p) { " (introuvable)" } else { "" };
        format!("{tick}{pin}{}{gone}", project_label(p))
    };

    // Make the menu a little wider than its widest project name (plus button padding, the grip
    // and some air), instead of whatever width the first rows happened to need.
    let font = egui::TextStyle::Button.resolve(ui.style());
    let color = ui.visuals().text_color();
    let widest = config
        .recent
        .iter()
        .map(|p| ui.painter().layout_no_wrap(label_of(p), font.clone(), color).size().x)
        .fold(0.0_f32, f32::max);
    // Every row has exactly this button width: never "whatever space the popup offers", which
    // is very wide and made the menu huge.
    let btn_w = (widest + 2.0 * ui.spacing().button_padding.x + 12.0).min(500.0 - GRIP_ROW_W);
    ui.set_min_width(btn_w + GRIP_ROW_W);

    for p in config.ordered_recent() {
        let is_missing = missing.contains(p);
        let label = label_of(p);

        let row_id = ui.unique_id().with(("recent_row", p));
        let (mut clicked, mut grip) = (false, None);
        let row = ui
            .horizontal(|ui| {
                // The project name opens the project; only the grip at the end starts a drag.
                let btn = ui
                    .add_enabled(!is_missing, egui::Button::new(label).min_size(egui::vec2(btn_w, 0.0)))
                    .on_hover_text(p.to_string_lossy());
                clicked = btn.clicked();
                let handle = ui.dnd_drag_source(row_id, p.clone(), |ui| {
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                    let color = ui.visuals().weak_text_color();
                    for dx in [-2.5, 2.5] {
                        for dy in [-4.0, 0.0, 4.0] {
                            ui.painter().circle_filled(rect.center() + egui::vec2(dx, dy), 1.2, color);
                        }
                    }
                });
                grip = Some(handle.response);
            })
            .response;
        if let Some(g) = grip {
            let _ = g.on_hover_cursor(egui::CursorIcon::Grab).on_hover_text("Glisser pour réorganiser");
        }
        if clicked {
            *to_open = Some(p.to_string_lossy().into_owned());
            ui.close();
        }
        // While dragging, the grip alone would be a tiny square: show the project name too.
        if ui.ctx().is_being_dragged(row_id) {
            if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                egui::Area::new(row_id.with("ghost"))
                    .order(egui::Order::Tooltip)
                    .fixed_pos(pos + egui::vec2(14.0, 8.0))
                    .interactable(false)
                    .show(ui.ctx(), |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| ui.label(project_label(p)));
                    });
            }
        }

        // Something is being dragged over this row: show where it would land.
        if let Some(from) = row.dnd_hover_payload::<PathBuf>() {
            if *from != *p && config.is_pinned(&from) == config.is_pinned(p) {
                let pointer_y = ui.input(|i| i.pointer.interact_pos()).map_or(row.rect.center().y, |pos| pos.y);
                let after = pointer_y > row.rect.center().y;
                let y = if after { row.rect.bottom() } else { row.rect.top() };
                ui.painter().hline(row.rect.x_range(), y, egui::Stroke::new(2.0, ui.visuals().selection.stroke.color));
                if row.dnd_release_payload::<PathBuf>().is_some() {
                    *reorder = Some(((*from).clone(), p.clone(), after));
                }
            }
        }
    }
}

/// Folder name of a project, or the full path when it has none (e.g. a drive root).
fn project_label(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string_lossy().into_owned())
}

fn ref_colors(b: &RefBadge) -> (Color32, Color32) {
    // (background, text)
    match (b.kind, b.is_head, b.local) {
        (RefKind::Tag, ..) => (Color32::from_rgb(0xb8, 0x8a, 0x1c), Color32::BLACK),
        (RefKind::Branch, true, _) => (Color32::from_rgb(0x2e, 0x7d, 0x32), Color32::WHITE),
        (RefKind::Branch, _, true) => (Color32::from_rgb(0x3a, 0x6e, 0x9e), Color32::WHITE),
        // Remote-only branch.
        (RefKind::Branch, _, false) => (Color32::from_rgb(0x5a, 0x5a, 0x7a), Color32::WHITE),
    }
}

const ICON_W: f32 = 14.0;


/// Minimum room kept for the commit message before a right-hand column is hidden.
const MIN_MSG_W: f32 = 150.0;

/// A small monitor: the branch exists locally.
fn paint_local_icon(p: &egui::Painter, c: egui::Pos2, color: Color32) {
    let stroke = egui::Stroke::new(1.3, color);
    let screen = egui::Rect::from_center_size(c - egui::vec2(0.0, 1.0), egui::vec2(10.0, 7.0));
    p.rect_stroke(screen, 1.5, stroke, egui::StrokeKind::Inside);
    p.line_segment([c + egui::vec2(-4.5, 4.5), c + egui::vec2(4.5, 4.5)], stroke);
}

/// A small cloud: the branch exists on a remote.
fn paint_remote_icon(p: &egui::Painter, c: egui::Pos2, color: Color32) {
    p.circle_filled(c + egui::vec2(-3.0, 1.0), 3.0, color);
    p.circle_filled(c + egui::vec2(0.0, -1.0), 3.5, color);
    p.circle_filled(c + egui::vec2(3.5, 1.0), 2.5, color);
    p.rect_filled(egui::Rect::from_min_max(c + egui::vec2(-3.0, 1.0), c + egui::vec2(3.5, 3.5)), 0.0, color);
}

/// Horizontal layout of one row of the commit list.
#[derive(Debug, Clone, Copy, PartialEq)]
struct RowGeometry {
    /// Where the commit message starts (after the refs column and the graph).
    msg_start: f32,
    time_left: f32,
    author_left: f32,
    /// A right-hand column disappears entirely when it would leave less than `MIN_MSG_W`
    /// for the message; the author goes first, then the date.
    show_time: bool,
    show_author: bool,
}

fn row_geometry(left: f32, right: f32, cols: &Columns, graph_w: f32) -> RowGeometry {
    let msg_start = left + cols.refs + graph_w + 8.0;
    let time_left = right - 8.0 - cols.time;
    let author_left = time_left - 12.0 - cols.author;
    let show_time = time_left >= msg_start + MIN_MSG_W;
    RowGeometry {
        msg_start,
        time_left,
        author_left,
        show_time,
        show_author: show_time && author_left >= msg_start + MIN_MSG_W,
    }
}

/// A draggable vertical border in the header. Returns (horizontal drag in points, drag finished).
fn column_handle(ui: &mut egui::Ui, hp: &egui::Painter, hrect: egui::Rect, x: f32, id: &str) -> (f32, bool) {
    let handle = egui::Rect::from_center_size(egui::pos2(x, hrect.center().y), egui::vec2(8.0, hrect.height()));
    let resp = ui.interact(handle, ui.unique_id().with(id), egui::Sense::drag());
    let active = resp.hovered() || resp.dragged();
    if active {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
    let stroke = if active {
        ui.visuals().widgets.active.bg_stroke
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke
    };
    hp.line_segment([egui::pos2(x, hrect.top() + 2.0), egui::pos2(x, hrect.bottom() - 2.0)], stroke);
    (if resp.dragged() { resp.drag_delta().x } else { 0.0 }, resp.drag_stopped())
}

/// Draws the commit list with its graph; search results are highlighted.
/// Returns the user's action on a row, and whether a column resize just finished.
fn draw_graph(
    ui: &mut egui::Ui,
    snap: &Snapshot,
    selected: Option<usize>,
    search: &Search,
    scroll_to: Option<usize>,
    cols: &mut Columns,
) -> (Option<RowAction>, bool) {
    let graph_w = snap.layout.width.max(1) as f32 * LANE_W;
    let query = search.query.trim();
    let mut action = None;
    let mut resized = false;
    let now = chrono::Local::now();
    ui.spacing_mut().item_spacing.y = 0.0;

    // Column header: the borders can be dragged to resize the columns.
    let (hrect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 20.0), egui::Sense::hover());
    let right_edge = hrect.right() - ui.spacing().scroll.allocated_width();
    let hp = ui.painter_at(hrect);
    let weak = ui.visuals().weak_text_color();
    let f = egui::FontId::proportional(12.0);
    let label = |x: f32, text: &str| {
        hp.text(egui::pos2(x, hrect.center().y), egui::Align2::LEFT_CENTER, text, f.clone(), weak);
    };

    // Branches / tags column (its right border sits left of the graph).
    let (dx, done) = column_handle(ui, &hp, hrect, hrect.left() + cols.refs, "refs_handle");
    cols.refs = (cols.refs + dx).clamp(Columns::REFS_RANGE.0, Columns::REFS_RANGE.1);
    resized |= done;
    // Right-hand columns: dragging a border to the left widens the column on its right.
    let g = row_geometry(hrect.left(), right_edge, cols, graph_w);
    if g.show_time {
        let (dx, done) = column_handle(ui, &hp, hrect, g.time_left - 6.0, "time_handle");
        cols.time = (cols.time - dx).clamp(Columns::TIME_RANGE.0, Columns::TIME_RANGE.1);
        resized |= done;
    }
    if g.show_author {
        let (dx, done) = column_handle(ui, &hp, hrect, g.author_left - 6.0, "author_handle");
        cols.author = (cols.author - dx).clamp(Columns::AUTHOR_RANGE.0, Columns::AUTHOR_RANGE.1);
        resized |= done;
    }
    // Labels use the geometry after this frame's drags.
    let g = row_geometry(hrect.left(), right_edge, cols, graph_w);
    label(hrect.left() + 6.0, "Branches / tags");
    label(hrect.left() + cols.refs + 8.0, "Graphe");
    label(g.msg_start, "Message");
    if g.show_author {
        label(g.author_left, "Auteur");
    }
    if g.show_time {
        label(g.time_left, "Date");
    }
    let rw = cols.refs;
    let mut area = egui::ScrollArea::vertical().auto_shrink(false);
    if let Some(i) = scroll_to {
        // Put the wanted row in the middle of the view.
        area = area.vertical_scroll_offset((i as f32 * ROW_H - ui.available_height() / 2.0 + ROW_H / 2.0).max(0.0));
    }
    area.show_rows(ui, ROW_H, snap.commits.len(), |ui, range| {
        for pos in range {
            let i = pos;
            let (commit, row) = (&snap.commits[i], &snap.layout.rows[i]);
            let (rect, resp) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), ROW_H),
                egui::Sense::click(),
            );
            if resp.clicked() {
                action = Some(RowAction::Select(i));
            }
            resp.context_menu(|ui| {
                if ui.button("Créer une branche ici…").clicked() {
                    action = Some(RowAction::NewBranch(i));
                    ui.close();
                }
                if ui.button("Créer un tag ici…").clicked() {
                    action = Some(RowAction::NewTag(i));
                    ui.close();
                }
                if ui.button("Checkout (HEAD détaché)").clicked() {
                    action = Some(RowAction::CheckoutDetached(i));
                    ui.close();
                }
                if ui.button("Copier le hash").clicked() {
                    action = Some(RowAction::CopyHash(i));
                    ui.close();
                }
            });
            let p = ui.painter_at(rect);
            if selected == Some(i) {
                p.rect_filled(rect, 0.0, ui.visuals().selection.bg_fill);
            } else if resp.hovered() {
                p.rect_filled(rect, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
            }
            let is_match = search.flags.get(i).copied().unwrap_or(false);
            if is_match {
                let current = search.current.is_some_and(|c| search.matches[c] == i);
                let alpha = if current { 80 } else { 32 };
                p.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(255, 200, 0, alpha));
            }
            let mid = rect.center().y;

            // Branch and tag labels in the left column, linked to the node by a thin line.
            let font = egui::FontId::proportional(13.0);
            let small = egui::FontId::proportional(11.0);
            let text_color = ui.visuals().text_color();
            let weak = ui.visuals().weak_text_color();
            if let Some(labels) = snap.refs.get(&commit.id) {
                let col_rect = egui::Rect::from_min_max(rect.min, egui::pos2(rect.left() + rw - 4.0, rect.max.y));
                let pc = p.with_clip_rect(col_rect);
                let limit = rect.left() + rw - 8.0;
                let mut tx = rect.left() + 6.0;
                for (n, l) in labels.iter().enumerate() {
                    let (bg, fg) = ref_colors(l);
                    let g = p.layout_no_wrap(l.name.clone(), small.clone(), fg);
                    let icons = (l.kind == RefKind::Branch) as usize as f32
                        * (l.local as usize + l.has_remote() as usize) as f32;
                    let w = g.size().x + 10.0 + icons * ICON_W;
                    let remaining = labels.len() - n;
                    let reserve = if remaining > 1 { 26.0 } else { 0.0 };
                    if n > 0 && tx + w + reserve > limit {
                        pc.text(egui::pos2(tx, mid), egui::Align2::LEFT_CENTER, format!("+{remaining}"), small.clone(), weak);
                        tx += 24.0;
                        break;
                    }
                    let r = egui::Rect::from_min_size(egui::pos2(tx, mid - 8.0), egui::vec2(w, 16.0));
                    pc.rect_filled(r, 4.0, bg);
                    let mut ix = tx + 5.0;
                    if l.kind == RefKind::Branch && l.local {
                        paint_local_icon(&pc, egui::pos2(ix + 5.0, mid), fg);
                        ix += ICON_W;
                    }
                    if l.has_remote() {
                        paint_remote_icon(&pc, egui::pos2(ix + 5.0, mid), fg);
                        ix += ICON_W;
                    }
                    pc.galley(egui::pos2(ix, mid - g.size().y / 2.0), g, fg);
                    tx += w + 4.0;
                }
                {
                    let node_x = rect.left() + rw + row.col as f32 * LANE_W + LANE_W / 2.0;
                    let from = tx.min(limit).max(rect.left() + 6.0);
                    p.line_segment(
                        [egui::pos2(from, mid), egui::pos2(node_x, mid)],
                        egui::Stroke::new(1.5, PALETTE[row.color % PALETTE.len()]),
                    );
                }
                if resp.hover_pos().is_some_and(|hp| col_rect.contains(hp)) {
                    let lines: Vec<String> = labels
                        .iter()
                        .map(|l| match (l.kind, l.local, l.has_remote()) {
                            (RefKind::Tag, ..) => format!("{} (tag)", l.name),
                            (_, true, true) => format!("{} — local + {}", l.name, l.remotes.join(", ")),
                            (_, true, false) => format!("{} — local seulement", l.name),
                            _ => format!("{} — remote seulement ({})", l.name, l.remotes.join(", ")),
                        })
                        .collect();
                    resp.clone().on_hover_text(lines.join("\n"));
                }
            }

            {
                let x = |col: usize| rect.left() + rw + col as f32 * LANE_W + LANE_W / 2.0;
                let (top, bottom) = (rect.top(), rect.bottom());
                let stroke = |c: usize| egui::Stroke::new(2.0, PALETTE[c % PALETTE.len()]);
                for &(c, color) in &row.incoming {
                    p.line_segment([egui::pos2(x(c), top), egui::pos2(x(row.col), mid)], stroke(color));
                }
                for pass in &row.passing {
                    p.line_segment(
                        [egui::pos2(x(pass.from), top), egui::pos2(x(pass.to), bottom)],
                        stroke(pass.color),
                    );
                }
                for &(c, color) in &row.outgoing {
                    p.line_segment([egui::pos2(x(row.col), mid), egui::pos2(x(c), bottom)], stroke(color));
                }
                // Avatar: the author's initials, ringed with the color of the branch lane.
                let center = egui::pos2(x(row.col), mid);
                let fill = Color32::from(egui::ecolor::Hsva::new(crate::avatar::hue(&commit.author), 0.5, 0.5, 1.0));
                p.circle_filled(center, NODE_R, fill);
                p.circle_stroke(center, NODE_R - 1.0, egui::Stroke::new(2.0, PALETTE[row.color % PALETTE.len()]));
                p.text(
                    center,
                    egui::Align2::CENTER_CENTER,
                    crate::avatar::initials(&commit.author),
                    egui::FontId::proportional(9.0),
                    Color32::WHITE,
                );
            }

            // Right side: date column, then author column (fixed widths, so every row lines up).
            // Each column disappears entirely when the window is too narrow to keep room for the
            // graph and the start of the message. The date is only written on the first row of a
            // run of commits sharing the same label ("hier", "il y a 2 heures"…).
            let RowGeometry { msg_start, time_left, author_left, show_time, show_author } =
                row_geometry(rect.left(), rect.right(), cols, graph_w);

            if show_time {
                let time_label = timefmt::relative_from(&now, commit.time);
                let same_as_previous = pos > 0
                    && timefmt::relative_from(&now, snap.commits[pos - 1].time) == time_label;
                if !same_as_previous {
                    let g = p.layout_no_wrap(time_label, font.clone(), weak);
                    p.galley(egui::pos2(rect.right() - 8.0 - g.size().x, mid - g.size().y / 2.0), g, weak);
                }
                if resp.hover_pos().is_some_and(|hp| hp.x >= time_left) {
                    resp.clone().on_hover_text(timefmt::absolute(commit.time));
                }
            }
            if show_author {
                let col = egui::Rect::from_min_max(
                    egui::pos2(author_left, rect.top()),
                    egui::pos2(author_left + cols.author, rect.bottom()),
                );
                let cp = p.with_clip_rect(col);
                if is_match {
                    highlight_span(&cp, author_left, mid, &commit.author, query, &font);
                }
                cp.text(
                    egui::pos2(author_left, mid),
                    egui::Align2::LEFT_CENTER,
                    &commit.author,
                    font.clone(),
                    weak,
                );
            }

            let summary_right = if show_author {
                author_left - 12.0
            } else if show_time {
                time_left - 12.0
            } else {
                rect.right() - 8.0
            };
            let clip = egui::Rect::from_min_max(rect.min, egui::pos2(summary_right, rect.max.y));
            let cp = p.with_clip_rect(clip);
            if is_match {
                highlight_span(&cp, msg_start, mid, &commit.summary, query, &font);
            }
            cp.text(
                egui::pos2(msg_start, mid),
                egui::Align2::LEFT_CENTER,
                &commit.summary,
                font,
                text_color,
            );
        }
    });
    (action, resized)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols() -> Columns {
        Columns { refs: 200.0, author: 140.0, time: 130.0 }
    }

    #[test]
    fn wide_window_shows_every_column() {
        let g = row_geometry(0.0, 1400.0, &cols(), 100.0);
        assert!(g.show_time && g.show_author);
        assert_eq!(g.msg_start, 200.0 + 100.0 + 8.0);
        assert_eq!(g.time_left, 1400.0 - 8.0 - 130.0);
        assert_eq!(g.author_left, g.time_left - 12.0 - 140.0);
        assert!(g.author_left >= g.msg_start + MIN_MSG_W);
    }

    #[test]
    fn author_is_hidden_before_the_date() {
        // msg_start = 308; time shown needs time_left >= 458, author needs author_left >= 458.
        // time_left = right - 138 ; author_left = right - 290.
        let g = row_geometry(0.0, 740.0, &cols(), 100.0);
        assert!(g.show_time && !g.show_author, "{g:?}"); // author_left = 450 < 458
        let g = row_geometry(0.0, 748.0, &cols(), 100.0);
        assert!(g.show_time && g.show_author, "{g:?}"); // author_left = 458: exactly enough
    }

    #[test]
    fn date_is_hidden_when_the_window_is_very_narrow() {
        let g = row_geometry(0.0, 500.0, &cols(), 100.0);
        assert!(!g.show_time && !g.show_author);
    }

    #[test]
    fn resizing_columns_moves_the_thresholds() {
        let narrow_author = Columns { author: 60.0, ..cols() };
        // Same window: with a 140 px author column it is hidden, with 60 px it fits.
        assert!(!row_geometry(0.0, 700.0, &cols(), 100.0).show_author);
        assert!(row_geometry(0.0, 700.0, &narrow_author, 100.0).show_author);
    }

    #[test]
    fn a_wider_graph_pushes_the_message_and_hides_columns_sooner() {
        let a = row_geometry(0.0, 900.0, &cols(), 50.0);
        let b = row_geometry(0.0, 900.0, &cols(), 400.0);
        assert!(a.show_author && !b.show_author);
        assert_eq!(b.msg_start - a.msg_start, 350.0);
    }

    #[test]
    fn project_label_is_the_folder_name_or_the_whole_path() {
        assert_eq!(project_label(Path::new("/home/user/dev/sports-scope")), "sports-scope");
        assert_eq!(project_label(Path::new("/home/user/dev/sports-scope/")), "sports-scope");
        assert_eq!(project_label(Path::new("/")), "/");
    }

    /// A one-file diff taken from a real (temporary) repository.
    fn sample_diff(name: &str, content: &str) -> FileDiff {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let raw = git2::Repository::init(dir.path()).unwrap();
        let mut cfg = raw.config().unwrap();
        cfg.set_str("user.name", "T").unwrap();
        cfg.set_str("user.email", "t@t").unwrap();
        std::fs::write(dir.path().join(name), content).unwrap();
        let repo = Repo::open(dir.path()).unwrap();
        repo.stage(name).unwrap();
        let id = repo.commit("add").unwrap();
        repo.commit_detail(&id.to_string()).unwrap().files.remove(0)
    }

    /// Polls `highlights_for` like the UI does, frame after frame, until the result arrives.
    fn wait_for_highlights(app: &mut App, file: &FileDiff, dark: bool) -> Option<Arc<Highlighted>> {
        let ctx = egui::Context::default();
        for _ in 0..1000 {
            if let Some(h) = app.highlights_for(&ctx, dark, file) {
                return Some(h);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    }

    #[test]
    fn highlights_arrive_through_the_same_path_as_the_ui() {
        let file = sample_diff("main.rs", "fn main() {\n    let s = \"hi\"; // note\n}\n");
        let mut app = App::new();
        let h = wait_for_highlights(&mut app, &file, true).expect("no highlighting after 10 s");
        assert_eq!(h.language, Some("Rust"));
        let colored: Vec<_> = h.lines.iter().filter(|l| !l.is_empty()).collect();
        assert!(colored.len() >= 3, "{:?}", h.lines);
        let colors: std::collections::HashSet<_> = colored.iter().flat_map(|l| l.iter().map(|s| s.color)).collect();
        assert!(colors.len() >= 3, "only {colors:?}");
    }

    #[test]
    fn highlights_follow_the_file_and_the_theme() {
        let rs = sample_diff("a.rs", "fn a() {}\n");
        let py = sample_diff("b.py", "def b():\n    pass\n");
        let mut app = App::new();
        let h1 = wait_for_highlights(&mut app, &rs, true).unwrap();
        let h2 = wait_for_highlights(&mut app, &py, true).unwrap();
        assert_eq!(h1.language, Some("Rust"));
        assert_eq!(h2.language, Some("Python")); // not the cached Rust result
        let light = wait_for_highlights(&mut app, &py, false).unwrap();
        assert_ne!(h2.lines, light.lines); // theme change recomputes
    }

    /// Renders `diff_ui` for real (headless egui) and returns the distinct text colors painted.
    fn rendered_text_colors(file: &FileDiff, hl: Option<&Highlighted>) -> std::collections::HashSet<Color32> {
        let ctx = egui::Context::default();
        let mut colors = std::collections::HashSet::new();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 600.0))),
            ..Default::default()
        };
        // Two frames: the first one lays everything out.
        for _ in 0..2 {
            colors.clear();
            let mut out = ctx.run_ui(input.clone(), |ui| {
                egui::CentralPanel::default().show(ui, |ui| diff_ui(ui, file, hl, None));
            });
            out.textures_delta.clear(); // we never upload textures here
            for clipped in &out.shapes {
                collect_text_colors(&clipped.shape, &mut colors);
            }
        }
        colors
    }

    fn collect_text_colors(shape: &egui::epaint::Shape, out: &mut std::collections::HashSet<Color32>) {
        match shape {
            egui::epaint::Shape::Text(t) => match t.override_text_color {
                // An override replaces every section color when painting.
                Some(c) => {
                    out.insert(c);
                }
                None => {
                    for section in &t.galley.job.sections {
                        out.insert(section.format.color);
                    }
                }
            },
            egui::epaint::Shape::Vec(v) => v.iter().for_each(|s| collect_text_colors(s, out)),
            _ => {}
        }
    }

    #[test]
    fn diff_view_paints_syntax_colors_when_highlights_are_available() {
        let file = sample_diff("main.rs", "fn main() {\n    let s = \"hi\"; // note\n}\n");
        let mut app = App::new();
        let hl = wait_for_highlights(&mut app, &file, true).unwrap();
        let plain = rendered_text_colors(&file, None);
        let colored = rendered_text_colors(&file, Some(&hl));
        assert!(colored.len() > plain.len() + 1, "plain: {plain:?}\ncolored: {colored:?}");
    }

    /// Runs one frame of the whole application, headless, and returns the text colors painted.
    fn run_app_frame(
        ctx: &egui::Context,
        app: &mut App,
        frame: &mut eframe::Frame,
    ) -> std::collections::HashSet<Color32> {
        use eframe::App as _;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))),
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| app.ui(ui, frame));
        out.textures_delta.clear();
        let mut colors = std::collections::HashSet::new();
        for clipped in &out.shapes {
            collect_text_colors(&clipped.shape, &mut colors);
        }
        colors
    }

    #[test]
    fn whole_app_shows_syntax_colors_in_the_diff_of_a_selected_commit() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let raw = git2::Repository::init(dir.path()).unwrap();
        let mut cfg = raw.config().unwrap();
        cfg.set_str("user.name", "Antoine Jaquet").unwrap();
        cfg.set_str("user.email", "t@t").unwrap();
        let repo = Repo::open(dir.path()).unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {\n    println!(\"one\");\n}\n").unwrap();
        repo.stage("main.rs").unwrap();
        repo.commit("first").unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {\n    let s = \"two\"; // changed\n}\n").unwrap();
        repo.stage("main.rs").unwrap();
        repo.commit("second").unwrap();

        let ctx = egui::Context::default();
        let mut frame = eframe::Frame::_new_kittest();
        let mut app = App::new();
        app.config_path = None; // never touch the real configuration
        app.pending_open = Some(dir.path().to_string_lossy().into_owned());

        // 1. The project loads (in a background thread).
        for _ in 0..500 {
            run_app_frame(&ctx, &mut app, &mut frame);
            if app.snap.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(app.snap.is_some(), "project did not load: {:?}", app.error);

        // 2. Select the newest commit, as a click on its row does.
        app.select(0);
        assert!(matches!(app.detail, Some(Ok(_))), "no commit detail");

        // 3. Frames go by; the highlighting thread finishes and the diff gets colored.
        let baseline = run_app_frame(&ctx, &mut app, &mut frame);
        let mut best = baseline.clone();
        for _ in 0..500 {
            let colors = run_app_frame(&ctx, &mut app, &mut frame);
            if colors.len() > best.len() {
                best = colors;
            }
            if app.hl_ready.is_some() && best.len() >= baseline.len() + 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(app.hl_ready.is_some(), "highlighting never completed (pending: {})", app.hl_pending.is_some());
        assert!(
            best.len() >= baseline.len() + 2,
            "diff stayed uncolored.\nbaseline: {baseline:?}\nbest: {best:?}"
        );
    }

    /// Manual check on a real repository, through the whole app:
    /// `GV_BENCH_REPO=/path cargo test real_repo_whole_app -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_repo_whole_app_colors() {
        let path = std::env::var("GV_BENCH_REPO").expect("set GV_BENCH_REPO");
        let ctx = egui::Context::default();
        let mut frame = eframe::Frame::_new_kittest();
        let mut app = App::new();
        app.config_path = None;
        app.pending_open = Some(path);
        for _ in 0..3000 {
            run_app_frame(&ctx, &mut app, &mut frame);
            if app.snap.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(app.snap.is_some(), "did not load: {:?}", app.error);

        let (mut files_seen, mut files_colored) = (0, 0);
        for commit in 0..40 {
            app.select(commit);
            let Some(Ok(detail)) = &app.detail else { continue };
            let n_files = detail.files.len();
            for f in 0..n_files {
                app.selected_file = f;
                let (path, binary, n_lines) = {
                    let Some(Ok(d)) = &app.detail else { break };
                    (d.files[f].path.clone(), d.files[f].binary, d.files[f].lines.len())
                };
                if binary || n_lines == 0 {
                    continue;
                }
                files_seen += 1;
                let start = std::time::Instant::now();
                let uid = {
                    let Some(Ok(d)) = &app.detail else { break };
                    d.files[f].uid
                };
                // Frames go by until the highlighting of *this* file is available.
                let mut result = None;
                for _ in 0..1500 {
                    run_app_frame(&ctx, &mut app, &mut frame);
                    if let Some((u, _, h)) = &app.hl_ready {
                        if *u == uid {
                            result = Some(h.clone());
                            break;
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                let (status, lang, colored_lines, distinct) = match &result {
                    None => ("TIMEOUT", "-", 0, 0),
                    Some(h) => {
                        let colored = h.lines.iter().filter(|l| !l.is_empty()).count();
                        let distinct: std::collections::HashSet<_> =
                            h.lines.iter().flatten().map(|s| s.color).collect();
                        let status = match (h.language, colored) {
                            (None, _) => "plain",
                            (Some(_), 0) => "EMPTY",
                            _ => "OK",
                        };
                        (status, h.language.unwrap_or("-"), colored, distinct.len())
                    }
                };
                files_colored += (status == "OK") as usize;
                println!(
                    "{status:7} commit {commit:>2} {path} [{lang}] {n_lines} lines: {colored_lines} colored, {distinct} distinct colors, {:?}",
                    start.elapsed()
                );
            }
        }
        println!("{files_colored}/{files_seen} files colored");
    }

    #[test]
    fn split_path_separates_the_file_name_from_its_folder() {
        assert_eq!(split_path("src/app/main.rs"), ("main.rs", "src/app"));
        assert_eq!(split_path("README.md"), ("README.md", ""));
        assert_eq!(split_path("a/b"), ("b", "a"));
        assert_eq!(split_path(""), ("", ""));
    }

    /// Height of the rows of the files list when it is drawn in a narrow column.
    fn narrow_list_rows(paths: &[&str]) -> Vec<f32> {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200.0, 400.0))),
            ..Default::default()
        };
        let mut heights = Vec::new();
        for _ in 0..2 {
            heights.clear();
            let mut out = ctx.run_ui(input.clone(), |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    for p in paths {
                        let label = file_label(ui, p);
                        let r = ui.add(egui::Button::selectable(false, label).truncate());
                        heights.push(r.rect.height());
                        assert!(r.rect.right() <= 200.0 + 1.0, "{p} overflows: {:?}", r.rect);
                    }
                });
            });
            out.textures_delta.clear();
        }
        heights
    }

    #[test]
    fn long_file_names_are_cut_to_the_column_instead_of_widening_it() {
        let long = "app/javascript/components/with/a/really/long/folder/name/ActivityStatsOverview.vue";
        let rows = narrow_list_rows(&["a.rs", long, "b.rs"]);
        // Every row stays on a single line of the same height, whatever the path length.
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|h| (h - rows[0]).abs() < 0.5), "{rows:?}");
    }

    fn diff_with(origins: &str) -> FileDiff {
        FileDiff {
            uid: 0,
            path: "f".into(),
            old_path: None,
            status: 'M',
            additions: 0,
            deletions: 0,
            binary: false,
            lines: origins
                .chars()
                .map(|origin| crate::repo::DiffLine { origin, text: String::new() })
                .collect(),
        }
    }

    #[test]
    fn scroll_target_is_the_first_change_with_context_above() {
        assert_eq!(first_change_line(&diff_with("H     +  ")), Some(3)); // change at 6 → 3 above
        assert_eq!(first_change_line(&diff_with("H -")), Some(0)); // never below zero
        assert_eq!(first_change_line(&diff_with("H   ")), None); // nothing changed
        assert_eq!(first_change_line(&diff_with("")), None);
    }

    #[test]
    fn whole_app_toggles_between_changed_lines_and_the_whole_file() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let raw = git2::Repository::init(dir.path()).unwrap();
        let mut cfg = raw.config().unwrap();
        cfg.set_str("user.name", "T").unwrap();
        cfg.set_str("user.email", "t@t").unwrap();
        let repo = Repo::open(dir.path()).unwrap();
        let body = |changed: &str| -> String {
            (1..=50).map(|i| if i == 25 { format!("{changed}\n") } else { format!("line {i}\n") }).collect()
        };
        std::fs::write(dir.path().join("f.txt"), body("before")).unwrap();
        repo.stage("f.txt").unwrap();
        repo.commit("first").unwrap();
        std::fs::write(dir.path().join("f.txt"), body("after")).unwrap();
        repo.stage("f.txt").unwrap();
        repo.commit("second").unwrap();

        let ctx = egui::Context::default();
        let mut frame = eframe::Frame::_new_kittest();
        let mut app = App::new();
        app.config_path = None;
        app.pending_open = Some(dir.path().to_string_lossy().into_owned());
        for _ in 0..500 {
            run_app_frame(&ctx, &mut app, &mut frame);
            if app.snap.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        app.select(0);
        app.config.diff_view = DiffView::Hunks;
        run_app_frame(&ctx, &mut app, &mut frame);
        assert!(app.full_cache.is_none(), "changed-lines view must not compute the whole file");

        // Switch to the whole-file view: its diff is computed, cached, and has all 50 lines.
        app.config.diff_view = DiffView::Full;
        run_app_frame(&ctx, &mut app, &mut frame);
        let (id, uid) = {
            let (id, path, diff, cut) = app.full_cache.as_ref().expect("whole-file diff not computed");
            assert_eq!(path, "f.txt");
            assert!(!cut);
            assert_eq!(id.len(), 40);
            assert_eq!(diff.lines.iter().filter(|l| l.origin == ' ').count(), 49);
            (id.clone(), diff.uid)
        };

        // Staying on the same file reuses the cache instead of recomputing.
        run_app_frame(&ctx, &mut app, &mut frame);
        assert_eq!(app.full_cache.as_ref().unwrap().2.uid, uid);

        // Another commit: the cache is replaced by that commit's version of the file.
        app.select(1);
        run_app_frame(&ctx, &mut app, &mut frame);
        let (id2, _, diff2, _) = app.full_cache.as_ref().unwrap();
        assert_ne!(*id2, id);
        assert_eq!(diff2.additions, 50); // root commit: the file was added
    }

    #[test]
    fn whole_app_side_by_side_puts_previous_and_next_version_on_the_same_row() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let raw = git2::Repository::init(dir.path()).unwrap();
        let mut cfg = raw.config().unwrap();
        cfg.set_str("user.name", "T").unwrap();
        cfg.set_str("user.email", "t@t").unwrap();
        let repo = Repo::open(dir.path()).unwrap();
        let body = |changed: &str| -> String {
            (1..=40).map(|i| if i == 20 { format!("let v = \"{changed}\";\n") } else { format!("let a{i} = {i};\n") }).collect()
        };
        std::fs::write(dir.path().join("f.rs"), body("before")).unwrap();
        repo.stage("f.rs").unwrap();
        repo.commit("first").unwrap();
        std::fs::write(dir.path().join("f.rs"), body("after")).unwrap();
        repo.stage("f.rs").unwrap();
        repo.commit("second").unwrap();

        let ctx = egui::Context::default();
        let mut frame = eframe::Frame::_new_kittest();
        let mut app = App::new();
        app.config_path = None;
        app.pending_open = Some(dir.path().to_string_lossy().into_owned());
        for _ in 0..500 {
            run_app_frame(&ctx, &mut app, &mut frame);
            if app.snap.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        app.select(0);
        app.config.diff_view = DiffView::Split;
        for _ in 0..50 {
            run_app_frame(&ctx, &mut app, &mut frame);
            if app.hl_ready.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        run_app_frame(&ctx, &mut app, &mut frame);

        let (_, rows) = app.split_cache.as_ref().expect("side-by-side rows not computed");
        assert_eq!(rows.len(), 40); // the whole file, one row per line
        let changed = rows
            .iter()
            .find(|r| matches!(r, SplitRow::Pair { left: Some(l), .. } if l.kind == CellKind::Removed))
            .expect("no changed row");
        match changed {
            SplitRow::Pair { left: Some(l), right: Some(r) } => {
                assert_eq!((l.number, r.number), (20, 20)); // same line number on both sides
                assert_eq!((l.kind, r.kind), (CellKind::Removed, CellKind::Added));
            }
            other => panic!("{other:?}"),
        }
        // Both versions of the file are visible at once in the painted output.
        let colors = run_app_frame(&ctx, &mut app, &mut frame);
        assert!(colors.len() >= 4, "side-by-side view painted almost no colors: {colors:?}");
    }
}
