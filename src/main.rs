mod app;
mod avatar;
mod browser;
mod config;
mod graph;

mod repo;
mod search;
mod split;
mod syntax;
mod timefmt;
mod worktree;

use eframe::egui;

/// Under WSLg, the Wayland backend leaves stale white borders after a resize
/// or maximize. Prefer X11 (XWayland) there. Set `GIT_VIEWER_WAYLAND=1` to opt out.
#[cfg(target_os = "linux")]
fn prefer_x11_on_wsl() {
    let is_wsl = std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::fs::read_to_string("/proc/version")
            .is_ok_and(|v| v.to_lowercase().contains("microsoft"));
    if is_wsl
        && std::env::var_os("DISPLAY").is_some()
        && std::env::var_os("GIT_VIEWER_WAYLAND").is_none()
    {
        // SAFETY: called first thing in `main`, before any other thread exists.
        unsafe { std::env::remove_var("WAYLAND_DISPLAY") };
    }
}

fn main() -> eframe::Result {
    #[cfg(target_os = "linux")]
    prefer_x11_on_wsl();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1200.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native("Git Viewer", options, Box::new(|_cc| Ok(Box::new(app::App::new()))))
}
