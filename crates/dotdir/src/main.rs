#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

mod app;
mod archive;
mod archive_extensions;
mod archive_ops;
mod archive_passwords;
mod archive_typescript;
mod column_scroll;
mod commands;
mod diagnostics;
mod extension_logs;
mod filesystems;
mod folder_sizes;
mod hex_file;
mod hex_view;
mod hover;
mod icon_theme;
mod keystrokes;
mod lsp;
mod marketplace;
mod media;
#[cfg(any(target_os = "windows", target_os = "linux"))]
mod media_mpv;
mod media_native;
mod model;
mod open_vsx;
mod open_vsx_media;
mod operations;
mod panel_menu;
mod panels;
mod quick_actions;
mod remote_connections;
mod remote_transfer;
mod selection_inspector;
mod syntax;
mod terminal;
mod theme;
mod ui;
mod vscode;
mod vscode_nls;
mod vscode_theme;
mod workspace;

use gpui_kit::{
    assets::{Assets, icon_assets},
    *,
};
use std::borrow::Cow;

icon_assets!(ExtraIcons, [Lightbulb]);

struct DotDirAssets;

impl AssetSource for DotDirAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match ExtraIcons.load(path)? {
            Some(icon) => Ok(Some(icon)),
            None => Assets.load(path),
        }
    }

    fn list(&self, prefix: &str) -> Result<Vec<SharedString>> {
        let mut paths = Assets.list(prefix)?;
        paths.extend(ExtraIcons.list(prefix)?);
        Ok(paths)
    }
}

fn main() {
    let initial = std::env::args_os().nth(1).map(std::path::PathBuf::from);
    let config = std::env::var_os("DOTDIR_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            dirs::config_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("dev.dotdir.native")
        });
    gpui_kit::application()
        .with_assets(DotDirAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            syntax::register_perl();
            commands::bind(cx, &config.join("keybindings.json"));
            cx.set_quit_mode(QuitMode::LastWindowClosed);
            let workspace = workspace::Workspace::load(config.join("settings.json"), cx);
            let mut sessions = workspace.startup_sessions();
            if let (Some(session), Some(path)) = (sessions.first_mut(), initial) {
                session.open_initial_path(path);
            }
            for session in sessions {
                if let Err(error) = workspace::open_window(workspace.clone(), session, None, cx) {
                    eprintln!("Could not restore DotDir window: {error:#}");
                }
            }
            if cx.windows().is_empty() {
                cx.quit();
            } else {
                cx.activate(true);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::{AssetSource as _, DotDirAssets, quick_actions};
    use gpui_kit::assets::IconName;

    #[test]
    fn editor_gutter_and_default_icons_are_bundled() {
        for path in [quick_actions::ICON_PATH, IconName::Close.path().as_ref()] {
            assert!(DotDirAssets.load(path).unwrap().is_some(), "{path}");
        }
    }
}
