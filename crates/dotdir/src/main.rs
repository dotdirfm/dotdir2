mod app;
mod archive;
mod column_scroll;
mod commands;
mod extension_logs;
mod hover;
mod icon_theme;
mod lsp;
mod media;
mod media_native;
mod model;
mod operations;
mod panels;
mod syntax;
mod terminal;
mod theme;
mod ui;
mod vscode;

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
            let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(700.), px(420.))),
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
