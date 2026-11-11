//! A small in-app folder picker (works the same on Windows, Linux and WSL, with no system dialog).

use eframe::egui;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_repo: bool,
}

/// Sub-folders of `dir`, sorted case-insensitively. Hidden folders are skipped unless asked for.
pub fn list_dirs(dir: &Path, show_hidden: bool) -> std::io::Result<Vec<DirEntry>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        // `metadata` follows symlinks, so a link to a folder counts as a folder.
        if !std::fs::metadata(&path).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let is_repo = path.join(".git").exists();
        out.push(DirEntry { name, path, is_repo });
    }
    out.sort_by_key(|e| e.name.to_lowercase());
    Ok(out)
}

#[cfg(windows)]
fn drives() -> Vec<PathBuf> {
    ('A'..='Z')
        .map(|c| PathBuf::from(format!("{c}:\\")))
        .filter(|p| p.exists())
        .collect()
}

#[cfg(not(windows))]
fn drives() -> Vec<PathBuf> {
    Vec::new()
}

pub struct FolderBrowser {
    current: PathBuf,
    path_text: String,
    entries: Vec<DirEntry>,
    error: Option<String>,
    show_hidden: bool,
}

impl FolderBrowser {
    /// Starts at `start`, or at the nearest existing parent, or at the home folder.
    pub fn new(start: Option<&Path>) -> Self {
        let start = start
            .and_then(|p| p.ancestors().find(|a| a.is_dir()))
            .map(Path::to_path_buf)
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        let mut b = Self {
            current: start.clone(),
            path_text: String::new(),
            entries: Vec::new(),
            error: None,
            show_hidden: false,
        };
        b.go_to(start);
        b
    }

    fn go_to(&mut self, dir: PathBuf) {
        match list_dirs(&dir, self.show_hidden) {
            Ok(entries) => {
                self.current = dunce::canonicalize(&dir).unwrap_or(dir);
                self.path_text = self.current.to_string_lossy().into_owned();
                self.entries = entries;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{} : {e}", dir.display())),
        }
    }

    /// Shows the dialog. Returns `Some(path)` when the user picks a folder, and sets
    /// `*open = false` when the dialog is closed.
    pub fn show(&mut self, ctx: &egui::Context, open: &mut bool) -> Option<PathBuf> {
        let mut picked = None;
        let mut goto: Option<PathBuf> = None;
        let mut window_open = true;
        egui::Window::new("Ouvrir un dossier")
            .open(&mut window_open)
            .default_size([520.0, 460.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("⬆").on_hover_text("Dossier parent").clicked() {
                        goto = self.current.parent().map(Path::to_path_buf);
                    }
                    if ui.button("🏠").on_hover_text("Dossier personnel").clicked() {
                        goto = dirs::home_dir();
                    }
                    for d in drives() {
                        if ui.button(d.to_string_lossy()).clicked() {
                            goto = Some(d);
                        }
                    }
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.path_text).desired_width(f32::INFINITY),
                    );
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        goto = Some(PathBuf::from(self.path_text.trim()));
                    }
                });
                if ui.checkbox(&mut self.show_hidden, "Afficher les dossiers cachés").changed() {
                    goto = Some(self.current.clone());
                }
                if let Some(e) = &self.error {
                    ui.colored_label(egui::Color32::from_rgb(0xe6, 0x5c, 0x6c), e);
                }
                ui.separator();
                egui::Panel::bottom("browser_actions").show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Ouvrir ce dossier").clicked() {
                            picked = Some(self.current.clone());
                        }
                        if self.current.join(".git").exists() {
                            ui.weak("● dépôt git");
                        }
                    });
                });
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    if self.entries.is_empty() {
                        ui.weak("Aucun sous-dossier.");
                    }
                    for e in &self.entries {
                        ui.horizontal(|ui| {
                            let label = if e.is_repo { format!("📁 {}  ●", e.name) } else { format!("📁 {}", e.name) };
                            if ui.selectable_label(false, label).clicked() {
                                goto = Some(e.path.clone());
                            }
                            if e.is_repo && ui.small_button("Ouvrir").clicked() {
                                picked = Some(e.path.clone());
                            }
                        });
                    }
                });
            });
        if let Some(dir) = goto {
            self.go_to(dir);
        }
        if picked.is_some() || !window_open {
            *open = false;
        }
        picked
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tree() -> TempDir {
        let d = TempDir::new().unwrap();
        for sub in ["beta", "Alpha", ".hidden", "repo/.git", "file_not_dir_parent"] {
            std::fs::create_dir_all(d.path().join(sub)).unwrap();
        }
        std::fs::write(d.path().join("a_file.txt"), "x").unwrap();
        d
    }

    #[test]
    fn lists_only_folders_sorted_case_insensitively_without_hidden() {
        let d = tree();
        let names: Vec<_> =
            list_dirs(d.path(), false).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["Alpha", "beta", "file_not_dir_parent", "repo"]);
    }

    #[test]
    fn hidden_folders_can_be_shown() {
        let d = tree();
        let names: Vec<_> = list_dirs(d.path(), true).unwrap().into_iter().map(|e| e.name).collect();
        assert!(names.contains(&".hidden".to_string()));
    }

    #[test]
    fn git_repositories_are_flagged() {
        let d = tree();
        let entries = list_dirs(d.path(), false).unwrap();
        assert!(entries.iter().find(|e| e.name == "repo").unwrap().is_repo);
        assert!(!entries.iter().find(|e| e.name == "beta").unwrap().is_repo);
    }

    #[test]
    fn unreadable_or_missing_folder_is_an_error() {
        let d = TempDir::new().unwrap();
        assert!(list_dirs(&d.path().join("nope"), false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_a_folder_counts_as_a_folder() {
        let d = tree();
        std::os::unix::fs::symlink(d.path().join("beta"), d.path().join("link")).unwrap();
        let names: Vec<_> = list_dirs(d.path(), false).unwrap().into_iter().map(|e| e.name).collect();
        assert!(names.contains(&"link".to_string()));
    }

    #[test]
    fn browser_starts_at_nearest_existing_ancestor() {
        let d = TempDir::new().unwrap();
        let b = FolderBrowser::new(Some(&d.path().join("gone/deeper")));
        assert_eq!(b.current, dunce::canonicalize(d.path()).unwrap());
    }

    #[test]
    fn go_to_a_missing_folder_keeps_the_current_one_and_reports_an_error() {
        let d = TempDir::new().unwrap();
        let mut b = FolderBrowser::new(Some(d.path()));
        let before = b.current.clone();
        b.go_to(d.path().join("missing"));
        assert_eq!(b.current, before);
        assert!(b.error.is_some());
    }
}
