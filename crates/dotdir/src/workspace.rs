use crate::{
    app::{DotDir, TabContent},
    archive::{Archive, SavedLocation},
    archive_extensions::BUNDLED as ARCHIVE_FORMATS,
    extension_logs::{ERROR_LOG_NAME, ExtensionLogs},
    filesystems::ProviderPath,
    hex_view::ByteViewMode,
    icon_theme::IconTheme,
    model::{Presentation, Settings, save_json},
    theme::Palette,
    vscode_theme::ImportedTheme,
};
use gpui_kit::*;
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, collections::BTreeMap, path::PathBuf, rc::Rc, sync::Arc};

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum SavedTab {
    Directory {
        path: SavedLocation,
    },
    File {
        path: SavedLocation,
        readonly: bool,
        byte_mode: Option<ByteViewMode>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct SavedPane {
    pub tabs: Vec<SavedTab>,
    pub active: usize,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub(crate) struct WindowPlacement {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    maximized: bool,
}

impl WindowPlacement {
    pub fn capture(window: &Window) -> Self {
        let state = window.window_bounds();
        let bounds = state.get_bounds();
        Self {
            x: bounds.origin.x.into(),
            y: bounds.origin.y.into(),
            width: bounds.size.width.into(),
            height: bounds.size.height.into(),
            maximized: matches!(state, WindowBounds::Maximized(_)),
        }
    }

    pub fn cascaded(self) -> Self {
        Self {
            x: self.x + 28.,
            y: self.y + 28.,
            maximized: false,
            ..self
        }
    }

    fn bounds(self) -> Bounds<Pixels> {
        Bounds::new(
            point(px(self.x), px(self.y)),
            size(px(self.width), px(self.height)),
        )
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct WindowSession {
    pub id: u64,
    pub panes: [SavedPane; 2],
    pub active: usize,
    pub presentations: [Presentation; 2],
    pub split: f32,
    pub show_hidden: bool,
    pub font_size: f32,
    pub terminal_height: f32,
    pub terminal_visible: bool,
    pub panels_visible: bool,
    pub placement: Option<WindowPlacement>,
}

impl WindowSession {
    fn migrate(settings: &Settings) -> Self {
        let pane = |paths: &[SavedLocation], active| SavedPane {
            tabs: paths
                .iter()
                .cloned()
                .map(|path| SavedTab::Directory { path })
                .collect(),
            active,
        };
        Self {
            id: 1,
            panes: [
                pane(&settings.left, settings.left_active),
                pane(&settings.right, settings.right_active),
            ],
            active: settings.active_pane,
            presentations: [settings.presentation(0), settings.presentation(1)],
            split: settings.panel_split,
            show_hidden: settings.show_hidden,
            font_size: settings.font_size,
            terminal_height: settings.terminal_height,
            terminal_visible: false,
            panels_visible: true,
            placement: None,
        }
    }

    pub fn open_initial_path(&mut self, path: PathBuf) {
        let directory = |path| SavedPane {
            tabs: vec![SavedTab::Directory { path }],
            active: 0,
        };
        if let Some(uri) = path
            .to_str()
            .and_then(|path| ProviderPath::parse(path).ok().flatten())
        {
            self.panes[0] = directory(SavedLocation::Provider { uri });
            self.active = 0;
        } else if path.is_dir() {
            self.panes = [
                directory(SavedLocation::Local(path.clone())),
                directory(SavedLocation::Local(path)),
            ];
        } else if path.is_file()
            && let Some(format) = ARCHIVE_FORMATS.for_path(&path)
        {
            let path = path.canonicalize().unwrap_or(path);
            if let Some(parent) = path.parent() {
                self.panes[1] = directory(SavedLocation::Local(parent.into()));
            }
            if let Ok(root) = Archive::mount_location(path.into(), format) {
                self.panes[0] = directory(SavedLocation::from(&root));
            }
        }
    }

    pub fn apply_layout(&self, settings: &mut Settings) {
        settings.active_pane = self.active.min(1);
        settings.presentations = Some(self.presentations);
        settings.panel_split = self.split.clamp(0., 1.);
        settings.show_hidden = self.show_hidden;
        settings.font_size = self.font_size.clamp(10., 24.);
        settings.terminal_height = self.terminal_height.clamp(100., 700.);
    }
}

#[derive(Clone)]
pub(crate) struct SharedState {
    pub settings: Settings,
    pub palette: Palette,
    pub color_theme_name: Option<String>,
    pub icon_theme: Option<Arc<IconTheme>>,
    pub revision: u64,
    pub appearance_revision: u64,
    pub extensions_revision: u64,
}

#[derive(Clone)]
pub(crate) struct Workspace(Rc<RefCell<State>>);

struct State {
    settings_path: PathBuf,
    shared: SharedState,
    logs: ExtensionLogs,
    sessions: BTreeMap<u64, WindowSession>,
    live: BTreeMap<u64, WeakEntity<DotDir>>,
    next_id: u64,
    color_request: u64,
    icon_request: u64,
}

impl Workspace {
    pub fn load(settings_path: PathBuf, cx: &mut App) -> Self {
        let settings = Settings::load(&settings_path);
        let logs = ExtensionLogs::with_error_file(&settings_path.with_file_name(ERROR_LOG_NAME));
        let mut palette = Palette::named(&settings.theme);
        let mut color_theme_name = None;
        if let Some(source) = settings.color_theme.clone() {
            match ImportedTheme::load(source) {
                Ok(theme) => {
                    theme.apply(cx);
                    palette = theme.palette;
                    color_theme_name = Some(theme.name);
                }
                Err(error) => logs
                    .source("dotdir", "DotDir")
                    .error(&format!("Color theme: {error:#}")),
            }
        }
        if color_theme_name.is_none() {
            palette.apply(cx);
        }
        let icon_theme =
            settings
                .icon_theme
                .as_ref()
                .and_then(|path| match IconTheme::load(path) {
                    Ok(theme) => Some(Arc::new(theme)),
                    Err(error) => {
                        logs.source("dotdir", "DotDir")
                            .error(&format!("Icon theme: {error:#}"));
                        None
                    }
                });
        let sessions: BTreeMap<_, _> = std::fs::read(settings_path.with_file_name("windows.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Vec<WindowSession>>(&bytes).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|session| (session.id, session))
            .collect();
        let sessions = if sessions.is_empty() {
            BTreeMap::from([(1, WindowSession::migrate(&settings))])
        } else {
            sessions
        };
        let next_id = sessions.last_key_value().map_or(1, |(id, _)| id + 1);
        Self(Rc::new(RefCell::new(State {
            settings_path,
            shared: SharedState {
                settings,
                palette,
                color_theme_name,
                icon_theme,
                revision: 0,
                appearance_revision: 0,
                extensions_revision: 0,
            },
            logs,
            sessions,
            live: BTreeMap::new(),
            next_id,
            color_request: 0,
            icon_request: 0,
        })))
    }

    pub fn settings_path(&self) -> PathBuf {
        self.0.borrow().settings_path.clone()
    }

    pub fn logs(&self) -> ExtensionLogs {
        self.0.borrow().logs.clone()
    }

    pub fn startup_sessions(&self) -> Vec<WindowSession> {
        self.0.borrow().sessions.values().cloned().collect()
    }

    pub fn snapshot(&self) -> SharedState {
        self.0.borrow().shared.clone()
    }

    pub fn changes_since(&self, revision: u64) -> Option<SharedState> {
        let state = self.0.borrow();
        (state.shared.revision != revision).then(|| state.shared.clone())
    }

    pub fn update_settings(&self, f: impl FnOnce(&mut Settings)) -> anyhow::Result<()> {
        let mut state = self.0.borrow_mut();
        let mut settings = state.shared.settings.clone();
        f(&mut settings);
        settings.save(&state.settings_path)?;
        state.shared.settings = settings;
        state.shared.revision += 1;
        Ok(())
    }

    pub fn next_color_request(&self) -> u64 {
        let mut state = self.0.borrow_mut();
        state.color_request += 1;
        state.color_request
    }

    pub fn is_current_color_request(&self, request: u64) -> bool {
        self.0.borrow().color_request == request
    }

    pub fn next_icon_request(&self) -> u64 {
        let mut state = self.0.borrow_mut();
        state.icon_request += 1;
        state.icon_request
    }

    pub fn is_current_icon_request(&self, request: u64) -> bool {
        self.0.borrow().icon_request == request
    }

    pub fn set_icon_theme(&self, theme: Option<IconTheme>) -> anyhow::Result<()> {
        self.update_settings(|settings| {
            settings.icon_theme = theme.as_ref().map(|theme| theme.root.clone())
        })?;
        self.0.borrow_mut().shared.icon_theme = theme.map(Arc::new);
        Ok(())
    }

    pub fn use_builtin_theme(&self, id: &str, cx: &mut App) -> anyhow::Result<()> {
        self.update_settings(|settings| {
            settings.theme = id.into();
            settings.color_theme = None;
        })?;
        let palette = Palette::named(id);
        {
            let mut state = self.0.borrow_mut();
            state.shared.palette = palette;
            state.shared.color_theme_name = None;
            state.shared.appearance_revision += 1;
        }
        palette.apply(cx);
        Ok(())
    }

    pub fn use_imported_theme(&self, theme: ImportedTheme, cx: &mut App) -> anyhow::Result<()> {
        self.update_settings(|settings| settings.color_theme = Some(theme.source.clone()))?;
        theme.apply(cx);
        let mut state = self.0.borrow_mut();
        state.shared.palette = theme.palette;
        state.shared.color_theme_name = Some(theme.name);
        state.shared.appearance_revision += 1;
        Ok(())
    }

    pub fn extensions_changed(&self) {
        let mut state = self.0.borrow_mut();
        state.shared.revision += 1;
        state.shared.extensions_revision += 1;
    }

    pub fn new_id(&self) -> u64 {
        let mut state = self.0.borrow_mut();
        let id = state.next_id;
        state.next_id += 1;
        id
    }

    pub fn other_windows(&self, id: u64) -> Vec<WeakEntity<DotDir>> {
        self.0
            .borrow()
            .live
            .iter()
            .filter_map(|(&owner, window)| (owner != id).then(|| window.clone()))
            .collect()
    }

    pub fn save_window(&self, session: WindowSession) -> anyhow::Result<()> {
        let mut state = self.0.borrow_mut();
        state.sessions.insert(session.id, session);
        state.save_sessions()
    }

    pub fn close_window(&self, id: u64) -> anyhow::Result<()> {
        let mut state = self.0.borrow_mut();
        state.live.remove(&id);
        if !state.live.is_empty() {
            state.sessions.remove(&id);
        }
        state.save_sessions()
    }
}

impl State {
    fn save_sessions(&self) -> anyhow::Result<()> {
        let sessions = self.sessions.values().collect::<Vec<_>>();
        save_json(
            &self.settings_path.with_file_name("windows.json"),
            &sessions,
        )
    }
}

pub(crate) fn open_window(
    workspace: Workspace,
    session: WindowSession,
    extra: Option<TabContent>,
    cx: &mut App,
) -> Result<AnyWindowHandle> {
    let stored = session.placement.map(WindowPlacement::bounds);
    let display = stored
        .and_then(|bounds| {
            cx.displays()
                .into_iter()
                .find(|display| display.visible_bounds().contains(&bounds.center()))
        })
        .or_else(|| cx.primary_display());
    let preferred = size(px(1200.), px(800.));
    let min = size(px(700.), px(420.));
    let bounds = if let Some(display) = &display {
        let area = display.visible_bounds();
        let max_size = area.size;
        let min_size = min.min(&max_size);
        let size = stored.map_or(max_size.map(|axis| axis * 0.9).min(&preferred), |bounds| {
            bounds.size.max(&min_size).min(&max_size)
        });
        let origin = stored.map_or(Bounds::centered_at(area.center(), size).origin, |bounds| {
            point(
                bounds
                    .origin
                    .x
                    .clamp(area.left(), area.right() - size.width),
                bounds
                    .origin
                    .y
                    .clamp(area.top(), area.bottom() - size.height),
            )
        });
        Bounds::new(origin, size)
    } else {
        stored.unwrap_or_else(|| Bounds::centered(None, preferred, cx))
    };
    let placement = if session.placement.is_some_and(|saved| saved.maximized) {
        WindowBounds::Maximized(bounds)
    } else {
        WindowBounds::Windowed(bounds)
    };
    let id = session.id;
    let (handle, view) = gpui_kit::open_window(
        WindowOptions {
            window_bounds: Some(placement),
            window_min_size: Some(min.min(&bounds.size)),
            display_id: display.map(|display| display.id()),
            titlebar: Some(TitlebarOptions {
                title: Some("DotDir".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        cx,
        {
            let workspace = workspace.clone();
            move |window, cx| cx.new(|cx| DotDir::new(workspace, session, extra, window, cx))
        },
    )?;
    workspace.0.borrow_mut().live.insert(id, view.downgrade());
    view.update(cx, |view, cx| view.persist(cx));
    Ok(handle)
}
