#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

mod app;
mod archive;
mod archive_extensions;
mod archive_ops;
mod archive_passwords;
mod archive_typescript;
mod column_scroll;
mod commands;
mod diagnostic_ruler;
mod extension_logs;
mod filesystems;
mod folder_sizes;
mod hex_file;
mod hex_view;
mod hover;
mod icon_theme;
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
mod selection_inspector;
mod syntax;
mod terminal;
mod theme;
mod ui;
mod vscode;
mod vscode_nls;
mod vscode_theme;

use gpui_kit::*;
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
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            syntax::register_perl();
            commands::bind(cx, &config.join("keybindings.json"));
            let preferred_size = size(px(1200.), px(800.));
            let display = cx.primary_display();
            let bounds = match &display {
                Some(display) => {
                    let work_area = display.visible_bounds();
                    // Leave room for the native window frame around the client area.
                    let available = work_area.size.map(|axis| axis * 0.9);
                    Bounds::centered_at(work_area.center(), preferred_size.min(&available))
                }
                None => Bounds::centered(None, preferred_size, cx),
            };
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(700.), px(420.)).min(&bounds.size)),
                    display_id: display.map(|display| display.id()),
                    titlebar: Some(TitlebarOptions {
                        title: Some("DotDir".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                cx,
                move |window, cx| {
                    cx.new(|cx| app::DotDir::new(config.join("settings.json"), initial, window, cx))
                },
            )
            .expect("open DotDir window");
            cx.activate(true);
        });
}
