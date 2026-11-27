use crate::{
    archive::{Archive, Location, SavedLocation},
    archive_extensions::BUNDLED as ARCHIVE_FORMATS,
    archive_passwords::{ArchivePasswords, PASSWORD_REQUIRED, PasswordRequest, UnlockCancelled},
    archive_typescript,
    column_scroll::ColumnScroll,
    commands::{COMMANDS, CommandState, RunCommand},
    diagnostics::{DiagnosticDecorations, DiagnosticStyles},
    extension_logs::{ERROR_LOG_NAME, ExtensionLogs},
    filesystems::{Filesystems, LOG_SCHEME, ProviderPath},
    folder_sizes::{FolderSizeEvent, FolderSizes},
    hex_file::{self, OpenedFile},
    hex_view::{ByteViewMode, HexBuffer, HexView},
    hover::HoverHints,
    icon_theme::IconTheme,
    keystrokes::PresentationKeystrokes,
    lsp::{self, LanguageServers},
    media::{MediaEvent, MediaFilter, MediaViewer, VIEWER_PRIORITY, image_format},
    model::*,
    open_vsx,
    operations::*,
    panel_menu::{JumpStep, PanelMenu},
    quick_actions::QuickActions,
    selection_inspector::{self, SelectionSnapshot},
    syntax::language_for,
    terminal::Terminal,
    theme::Palette,
    vscode,
    vscode_theme::{ImportedTheme, ThemeSource},
};
use anyhow::Context as _;
use dotdir_archive_api::{ArchiveFormat, OptionKind, OptionValue, Password};
use dotdir_core::{
    copy::{self, CancelToken, ConflictPolicy, ConflictResolution, CopyEvent, CopyOptions},
    search::{FileSearchEvent, FileSearchRequest},
    watch::FsWatcher,
};
use dotdir_extension_host::{ExtensionHost, ExtensionReply, LoadedExtension, PanelNode};
use gpui_kit::{
    component::input::{EditorState, GoToDefinition, InputEvent, InputState, ShowCompletions},
    *,
};
use serde_json::json;
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

const VSCODE_RUNNING: &str = "Running VS Code extension…";

fn extension_cache_directory() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("dev.dotdir.native")
}

pub(crate) enum Message {
    Market(open_vsx::Event),
    IconTheme(u64, anyhow::Result<Box<IconTheme>>),
    ColorTheme(u64, anyhow::Result<Box<ImportedTheme>>),
    Lsp(lsp::Notice),
    MountedRoots(Vec<PathBuf>),
    Directory(u64, u64, anyhow::Result<Vec<Entry>>),
    FolderSize(FolderSizeEvent),
    Watch(u64),
    ArchivePassword(PasswordRequest),
    Opened {
        request: u64,
        pane: usize,
        path: Location,
        readonly: bool,
        tab: Option<u64>,
        byte_mode: ByteViewMode,
        result: anyhow::Result<OpenedFile>,
    },
    Saved {
        id: u64,
        version: u64,
        text: String,
        result: anyhow::Result<()>,
    },
    ArchiveFileCreated {
        pane: usize,
        directory: Location,
        path: Location,
    },
    Search(u64, FileSearchEvent),
    Extension(anyhow::Result<LoadedExtension>),
    VsCode(vscode::Event),
    ExtensionReply {
        request: u64,
        target: Option<u64>,
        extension: String,
        command: String,
        title: String,
        result: anyhow::Result<ExtensionReply>,
    },
    Simple(anyhow::Result<()>),
}

fn read_password(input: &InputState) -> Password {
    let text = input.text();
    let mut password = Password::new(Vec::with_capacity(text.len()));
    for chunk in text.chunks() {
        password.extend_from_slice(chunk.as_bytes());
    }
    password
}

fn quote_copy_destination(path: &str) -> String {
    format!("\"{}\"", path.replace('"', "\"\""))
}

fn parse_copy_destinations(value: &str) -> anyhow::Result<Vec<String>> {
    let mut rest = value.trim_start();
    let mut paths = Vec::new();
    loop {
        let path = if let Some(quoted) = rest.strip_prefix('"') {
            rest = quoted;
            let mut path = String::new();
            loop {
                let (part, tail) = rest
                    .split_once('"')
                    .context("Close the quoted destination path")?;
                path.push_str(part);
                if let Some(escaped) = tail.strip_prefix('"') {
                    path.push('"');
                    rest = escaped;
                } else {
                    rest = tail.trim_start();
                    break;
                }
            }
            path
        } else {
            let end = rest.find(';').unwrap_or(rest.len());
            let (path, tail) = rest.split_at(end);
            rest = tail;
            path.trim().to_owned()
        };
        anyhow::ensure!(!path.is_empty(), "Enter a directory for each destination");
        paths.push(path);
        if rest.is_empty() {
            return Ok(paths);
        }
        rest = rest
            .strip_prefix(';')
            .context("Separate destination directories with semicolons")?
            .trim_start();
    }
}

impl DotDir {
    pub fn accept_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.modal, Some(Modal::ArchivePassword { .. })) {
            if let Some(Modal::ArchivePassword { request, input }) = self.modal.take() {
                let password = read_password(input.read(cx));
                input.update(cx, |input, cx| input.set_value("", window, cx));
                let _ = request.reply.send(password);
                self.focus_pane(self.active, window, cx);
                cx.notify();
            }
            return;
        }
        let value = self.dialog_input.read(cx).value().to_string();
        if matches!(self.modal, Some(Modal::CreateArchive(_))) {
            self.accept_create_archive(&value, window, cx);
            return;
        }
        if matches!(self.modal, Some(Modal::OpenVsx)) {
            self.search_marketplace(&value, 0, cx);
            return;
        }
        if let Some(Modal::QuickPick { request, .. }) = &self.modal {
            if let Some(&index) = request.matches(&value).get(self.palette_cursor) {
                self.accept_quick_pick(index, window, cx);
            }
            return;
        }
        if matches!(self.modal, Some(Modal::Palette)) {
            let commands = self.filtered_commands(cx);
            if let Some((id, _, _)) = commands.get(self.palette_cursor).cloned() {
                self.dismiss(window, cx);
                self.run(&id, window, cx);
            }
            return;
        }
        if matches!(self.modal, Some(Modal::Search)) {
            self.begin_search(window, cx);
            return;
        }
        if matches!(self.modal, Some(Modal::Conflict { .. })) {
            let choice = match self.palette_cursor {
                0 => ConflictResolution::Overwrite,
                1 => ConflictResolution::Skip,
                2 => ConflictResolution::OverwriteAll,
                3 => ConflictResolution::SkipAll,
                _ => ConflictResolution::Cancel,
            };
            if let Some(Modal::Conflict { reply, .. }) = self.modal.take() {
                let _ = reply.send(choice);
            }
            self.focus_pane(self.active, window, cx);
            return;
        }
        if !matches!(self.modal, Some(Modal::Prompt { .. })) {
            return;
        }
        let Some(Modal::Prompt {
            kind,
            title,
            description,
        }) = self.modal.take()
        else {
            return;
        };
        let result = (|| -> anyhow::Result<()> {
            match kind.clone() {
                PromptKind::IconTheme => {
                    self.load_icon_theme(self.resolve_path(value.trim()), cx);
                }
                PromptKind::ColorTheme => {
                    anyhow::ensure!(
                        !value.trim().is_empty(),
                        "Enter a theme file or extension directory"
                    );
                    self.choose_color_theme(self.resolve_path(value.trim()), window, cx)?;
                }
                PromptKind::ArchiveCache => {
                    let minutes = value
                        .trim()
                        .parse::<u64>()
                        .map_err(|_| anyhow::anyhow!("Enter a whole number of minutes, or 0"))?;
                    self.settings.archive_password_cache_minutes = minutes;
                    self.filesystems.archive_passwords.set_minutes(minutes);
                    self.persist();
                }
                PromptKind::Location => {
                    let location = self.resolve_directory(&value, true)?;
                    self.navigate(self.active, location, window, cx);
                }
                PromptKind::Mkdir(directory) | PromptKind::NewFile(directory)
                    if matches!(directory, Location::Archive { .. }) =>
                {
                    let name = value.trim();
                    let folder = matches!(kind, PromptKind::Mkdir(_));
                    crate::archive_ops::entry_path(name, folder)?;
                    anyhow::ensure!(
                        self.operation.is_none(),
                        "Wait for the current operation to finish"
                    );
                    let tx = self.tx.clone();
                    let pane = self.active;
                    let owner = directory.clone();
                    self.operation = Some(Operation::create_entry(
                        self.id(),
                        directory,
                        name.into(),
                        folder,
                        self.filesystems.clone(),
                        move |path| {
                            let _ = tx.send(Message::ArchiveFileCreated {
                                pane,
                                directory: owner,
                                path,
                            });
                        },
                    ));
                }
                PromptKind::Mkdir(_) => {
                    if !valid_name(value.trim()) {
                        anyhow::bail!("Enter one valid directory name");
                    }
                    let path = self.cwd(self.active).join(value.trim());
                    let tx = self.tx.clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(Message::Simple(
                            std::fs::create_dir(path).map_err(Into::into),
                        ));
                    });
                }
                PromptKind::Rename(source) => {
                    if !valid_name(&value) {
                        anyhow::bail!("Enter one valid file name");
                    }
                    let tx = self.tx.clone();
                    std::thread::spawn(move || {
                        let result = dotdir_core::move_op::rename_item(&source, &value)
                            .map(|_| ())
                            .map_err(Into::into);
                        let _ = tx.send(Message::Simple(result));
                    });
                }
                PromptKind::NewFile(_) => {
                    if value.trim().is_empty() {
                        anyhow::bail!("Enter a file path");
                    }
                    let path = self.resolve_path(&value);
                    if !path.exists() {
                        std::fs::OpenOptions::new()
                            .create_new(true)
                            .write(true)
                            .open(&path)?;
                    }
                    self.open_document(self.active, path, false, window, cx);
                }
                PromptKind::Transfer {
                    kind,
                    sources,
                    archive_destination,
                } => {
                    anyhow::ensure!(
                        self.operation.is_none(),
                        "Wait for the current operation to finish"
                    );
                    if value.trim().is_empty() {
                        anyhow::bail!("Enter a destination directory");
                    }
                    let local_copy = matches!(kind, OperationKind::Copy)
                        && sources.iter().all(|source| source.local().is_some());
                    let archive_destination =
                        archive_destination.filter(|path| value.trim() == path.display());
                    let mut options = self.copy_options.clone();
                    let id = self.id();
                    self.operation = if local_copy && archive_destination.is_none() {
                        let values = if self.copy_multiple {
                            parse_copy_destinations(&value)?
                        } else {
                            vec![value.trim().to_owned()]
                        };
                        let destinations = values
                            .iter()
                            .map(|value| self.resolve_path(value))
                            .map(|path| {
                                anyhow::ensure!(
                                    path.is_dir(),
                                    "Destination directory does not exist: {}",
                                    path.display()
                                );
                                Ok(path)
                            })
                            .collect::<anyhow::Result<Vec<_>>>()?;
                        if self.copy_use_filter {
                            let pattern = self.copy_filter.read(cx).value();
                            anyhow::ensure!(
                                !pattern.trim().is_empty(),
                                "Enter a file filter pattern"
                            );
                            copy::validate_copy_filter(pattern.trim())?;
                            options.filter = Some(pattern.trim().to_owned());
                        }
                        let sources = sources
                            .into_iter()
                            .map(|source| source.local().unwrap().to_path_buf())
                            .collect();
                        Some(Operation::copy(id, sources, destinations, options))
                    } else {
                        let destination = if let Some(path) = archive_destination {
                            path
                        } else {
                            let path = self.resolve_path(value.trim());
                            anyhow::ensure!(path.is_dir(), "Destination directory does not exist");
                            path.into()
                        };
                        if !self.transfer_supports_policy(
                            kind,
                            &sources,
                            destination.archive_member().is_some(),
                            options.conflict_policy,
                        ) {
                            options.conflict_policy = ConflictPolicy::Ask;
                        }
                        Some(Operation::transfer(
                            id,
                            kind,
                            sources,
                            destination,
                            options,
                            self.filesystems.clone(),
                        ))
                    };
                }
                PromptKind::DeleteArchive(sources) => {
                    anyhow::ensure!(
                        self.operation.is_none(),
                        "Wait for the current operation to finish"
                    );
                    self.operation = Some(Operation::delete_entries(
                        self.id(),
                        sources,
                        self.filesystems.clone(),
                    ));
                }
                PromptKind::Trash(paths) => {
                    let id = self.id();
                    self.operation = Some(Operation::start(
                        id,
                        OperationKind::Trash,
                        paths,
                        PathBuf::new(),
                        CopyOptions::default(),
                    ));
                }
                PromptKind::CloseTab { pane, id } => self.close_tab(pane, id, window, cx),
                PromptKind::Quit => {
                    self.persist();
                    if let Some(op) = &self.operation {
                        op.token.cancel();
                        self.quit_pending = true;
                        self.status = "Cancelling the operation before closing…".into();
                    } else {
                        self.shutting_down = true;
                        window.remove_window();
                        cx.quit();
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            self.modal = Some(Modal::Prompt {
                kind,
                title,
                description,
            });
            self.modal_error = Some(e.to_string());
            self.dialog_input.update(cx, |s, cx| s.focus(window, cx));
        } else if !self.shutting_down && self.modal.is_none() {
            self.focus_pane(self.active, window, cx);
        }
        cx.notify();
    }
    pub fn resolve_path(&self, value: &str) -> PathBuf {
        if value == "~" {
            return dirs::home_dir().unwrap_or_default();
        }
        if let Some(rest) = value.strip_prefix("~/") {
            return dirs::home_dir().unwrap_or_default().join(rest);
        }
        let p = PathBuf::from(value);
        if p.is_absolute() {
            p
        } else {
            self.cwd(self.active).join(p)
        }
    }

    pub(crate) fn resolve_directory(
        &self,
        value: &str,
        allow_archive: bool,
    ) -> anyhow::Result<Location> {
        if let Some(path) = ProviderPath::parse(value)? {
            self.filesystems.check(&path)?;
            return Ok(Location::Provider(path));
        }
        if let Location::Provider(path) = self.pane_location(self.active)
            && value != "~"
            && !value.starts_with("~/")
            && !Path::new(value).is_absolute()
        {
            self.filesystems.check(&path)?;
            return Ok(Location::Provider(path.resolve(value)?));
        }
        let path = self.resolve_path(value);
        if path.is_dir() {
            Ok(path.into())
        } else if allow_archive
            && path.is_file()
            && let Some(format) = ARCHIVE_FORMATS.for_path(&path)
        {
            Archive::mount_location(path.into(), format)
        } else {
            anyhow::bail!(if allow_archive {
                "Directory or supported archive does not exist"
            } else {
                "Directory does not exist"
            });
        }
    }

    pub fn filtered_commands(&self, cx: &App) -> Vec<(String, String, String)> {
        let query = self.dialog_input.read(cx).value().to_lowercase();
        let state = self.command_state(cx);
        COMMANDS
            .iter()
            .filter(|command| command.available(&state))
            .filter(|c| {
                !matches!(
                    c.id,
                    "up" | "down"
                        | "left-panel-menu"
                        | "right-panel-menu"
                        | "left"
                        | "right"
                        | "first"
                        | "last"
                        | "mark-down"
                        | "mark-up"
                        | "mark-left"
                        | "mark-right"
                        | "mark-first"
                        | "mark-last"
                        | "mark-page-up"
                        | "mark-page-down"
                        | "escape"
                )
            })
            .map(|c| {
                (
                    c.id.to_string(),
                    c.title(&state).to_string(),
                    c.key.to_string(),
                    None,
                )
            })
            .chain(self.extensions.iter().flat_map(|e| {
                e.contributions
                    .iter()
                    .filter(|c| c.viewer.is_none())
                    .map(|c| (c.id.clone(), c.title.clone(), "Extension".into(), None))
            }))
            .chain(self.vscode_commands().map(|(_, command)| {
                (
                    command.id.clone(),
                    command.title.clone(),
                    "VS Code".into(),
                    command.original_title.as_deref(),
                )
            }))
            .filter(|(id, title, _, original)| {
                let id = id.to_lowercase();
                let title = title.to_lowercase();
                let original = original.map(str::to_lowercase);
                query.split_whitespace().all(|q| {
                    title.contains(q)
                        || id.contains(q)
                        || original.as_ref().is_some_and(|title| title.contains(q))
                })
            })
            .map(|(id, title, shortcut, _)| (id, title, shortcut))
            .collect()
    }
    pub fn command_state(&self, cx: &App) -> CommandState {
        let tab = self.panes[self.active]
            .tabs
            .get(self.panes[self.active].active)
            .map(|tab| &tab.content);
        let directory = self.directory(self.active).filter(|_| self.panels_visible);
        let entry = directory.and_then(Directory::focused);
        let document = self
            .active_document()
            .and_then(|id| self.documents.get(&id));
        let location = self.pane_location(self.active);
        CommandState {
            panels_visible: self.panels_visible,
            directory: directory.is_some(),
            archive: matches!(location, Location::Archive { .. }),
            archive_editable: entry.is_some_and(|entry| {
                entry.path.archive_member().is_some() && entry.path.editable() && !entry.symlink
            }),
            archive_writable: matches!(location, Location::Archive { .. }) && location.editable(),
            opposite_archive_writable: self.opposite_archive_destination().is_some(),
            archive_creation: ARCHIVE_FORMATS.creators().next().is_some(),
            provider: matches!(location, Location::Provider(_)),
            entry: entry.is_some(),
            real_entry: entry.is_some_and(|entry| !entry.parent),
            file: entry.is_some_and(|entry| !entry.directory && !entry.parent),
            byte_viewer: self.panels_visible
                && self
                    .active_hex()
                    .is_some_and(|(_, view)| view.read(cx).readonly),
            items: directory.is_some_and(|d| d.entries.iter().any(|e| !e.parent)),
            selection: directory
                .is_some_and(|d| !d.marked.is_empty() || d.focused().is_some_and(|e| !e.parent)),
            parent: directory.is_some_and(|d| d.path.parent().is_some()),
            can_save: self.panels_visible
                && (document.is_some_and(|d| !d.readonly && !d.saving)
                    || self.active_hex().is_some_and(|(_, view)| {
                        let view = view.read(cx);
                        !view.readonly && !view.buffer.read(cx).saving
                    })),
            line_wrap: document.and_then(|doc| doc.line_wrap),
            show_keystrokes: self.keystrokes.enabled,
            language_server: document.is_some_and(|d| d.lsp.is_some()),
            has_tab: self.panels_visible && tab.is_some(),
            multiple_tabs: self.panels_visible && self.panes[self.active].tabs.len() > 1,
            preview: self.preview_target().is_some(),
            operation_idle: self.operation.is_none(),
            media: match tab {
                Some(TabContent::Media { view, .. }) if self.panels_visible => {
                    Some(view.read(cx).command_state())
                }
                _ => None,
            },
        }
    }
    pub fn command_available(&self, id: &str, cx: &App) -> bool {
        if let Some(command) = COMMANDS.iter().find(|command| command.id == id) {
            return command.available(&self.command_state(cx));
        }
        if id.starts_with("vscode:") {
            return self.vscode_commands().any(|(_, command)| command.id == id);
        }
        // API v1 extension commands are global; viewer registrations aren't
        // executable commands. They still obey the shared modal execution gate.
        self.extensions.iter().any(|extension| {
            extension
                .contributions
                .iter()
                .any(|command| command.id == id && command.viewer.is_none())
        })
    }
    pub fn begin_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(s) = &self.search {
            s.token.cancel();
        }
        let id = self.id();
        let token = CancelToken::new();
        let request = FileSearchRequest {
            start_path: self.cwd(self.active).display().to_string(),
            ignore_dirs_enabled: true,
            ignore_dirs: vec![".git".into(), "node_modules".into(), "target".into()],
            file_pattern: self.dialog_input.read(cx).value().to_string(),
            content_pattern: self.search_content.read(cx).value().to_string(),
            recursive: true,
            follow_symlinks: false,
            shell_patterns: true,
            case_sensitive_file_name: false,
            whole_words: false,
            regex: false,
            case_sensitive_content: false,
            all_charsets: false,
            first_hit: false,
            skip_hidden: !self.settings.show_hidden,
        };
        let cancel = token.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = dotdir_core::search::search_files(&request, &cancel, |e| {
                let _ = tx.send(Message::Search(id, e));
            });
            if let Err(e) = result {
                let _ = tx.send(Message::Search(
                    id,
                    FileSearchEvent::Error {
                        message: e.to_string(),
                        found: 0,
                    },
                ));
            }
        });
        self.search = Some(SearchState {
            id,
            token,
            results: vec![],
            done: false,
            count: 0,
            error: None,
            cursor: 0,
        });
        cx.notify();
        window.refresh();
    }
    pub fn open_search_result(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(path) = self
            .search
            .as_ref()
            .and_then(|s| s.results.get(index))
            .cloned()
        {
            self.dismiss(window, cx);
            if path.is_dir() {
                self.navigate(self.active, path.into(), window, cx);
            } else if let Some(parent) = path.parent() {
                let name = path.file_name().map(|s| s.to_string_lossy().into_owned());
                self.navigate(self.active, parent.into(), window, cx);
                if let Some(d) = self.directory_mut(self.active) {
                    d.restore_name = name;
                }
            }
        }
    }
    pub fn load_extensions(&mut self) {
        let mut filesystems = self
            .filesystems
            .rebuild_providers(self.extension_logs.clone());
        let directory = self.settings_path.with_file_name("extensions");
        self.load_vscode_extensions(&directory);
        self.reset_vscode_host();
        let mut paths = std::fs::read_dir(directory)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        paths.sort_unstable();
        let mut seen = std::collections::HashSet::new();
        for path in paths {
            let manifest_path = path.join("extension.json");
            if !manifest_path.is_file() || !seen.insert(path.canonicalize().unwrap_or(path.clone()))
            {
                continue;
            }
            if let Some(manifest) = std::fs::read(manifest_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .filter(|manifest| manifest["runtime"].as_str() == Some("node"))
            {
                let result = serde_json::from_value(manifest)
                    .map_err(Into::into)
                    .and_then(|package| {
                        filesystems.register(
                            package,
                            &path,
                            self.settings.vscode.node.clone(),
                            self.extension_logs.clone(),
                        )
                    });
                if let Err(error) = result {
                    self.report_extension_error(format!("{}: {error:#}", path.display()));
                }
                continue;
            }
            let rx = self
                .extension_host
                .get_or_insert_with(|| {
                    ExtensionHost::new(extension_cache_directory().join("quickjs"))
                })
                .load(path);
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                if let Ok(result) = rx.recv() {
                    let _ = tx.send(Message::Extension(result));
                }
            });
        }
        self.filesystems = filesystems;
    }

    fn report_extension_error(&mut self, error: String) {
        self.extension_logs
            .source("dotdir", "DotDir extensions")
            .error(&error);
        self.extension_errors.push(error);
    }

    pub(crate) fn load_vscode_extensions(&mut self, directory: &Path) {
        self.vscode_extensions = vscode::discover(directory, &self.settings.vscode);
        for extension in &self.vscode_extensions {
            self.extension_logs.register(&extension.id);
        }
    }

    pub fn browse_extension_logs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dismiss(window, cx);
        self.navigate(
            self.active,
            Location::Provider(ProviderPath::root(LOG_SCHEME)),
            window,
            cx,
        );
    }

    fn vscode_commands(
        &self,
    ) -> impl Iterator<Item = (&vscode::Extension, &vscode::ExtensionCommand)> {
        let document = self.vscode_document().filter(|_| {
            self.vscode_host.pending.is_none() && self.vscode_language_command.is_none()
        });
        self.vscode_extensions
            .iter()
            .filter(move |extension| {
                document.is_some() && self.settings.vscode.enabled(&extension.id)
            })
            .flat_map(move |extension| {
                let language_session = document
                    .and_then(|document| document.lsp.as_deref())
                    .filter(|buffer| buffer.extension_id() == extension.id);
                extension
                    .commands
                    .iter()
                    .filter(move |command| {
                        !extension.provides_language()
                            || language_session
                                .is_some_and(|buffer| buffer.command_available(&command.command))
                    })
                    .map(move |command| (extension, command))
            })
    }

    pub fn refresh_vscode_language_servers(&mut self, reload: bool, cx: &mut Context<Self>) {
        if reload {
            self.language_servers.forget_extension_sessions();
        }
        let documents = self
            .documents
            .values()
            .filter_map(|document| {
                if document.readonly {
                    return None;
                }
                let desired = if let Some(path) = document.path.local() {
                    vscode::language_extension(&self.vscode_extensions, &self.settings.vscode, path)
                } else {
                    archive_typescript::extension(
                        &document.path,
                        &self.vscode_extensions,
                        &self.settings.vscode,
                    )
                }
                .map(|(extension, _)| extension.id.as_str());
                let current = document.lsp.as_ref().map(|buffer| buffer.extension_id());
                (desired != current || (reload && current.is_some())).then_some(document.id)
            })
            .collect::<Vec<_>>();
        if !documents.is_empty() {
            self.dismiss_hover(cx);
        }
        for &id in &documents {
            self.documents
                .get_mut(&id)
                .unwrap()
                .clear_language_server(None, cx);
        }
        for id in documents {
            self.enable_language_server(id, cx);
        }
    }

    fn vscode_document(&self) -> Option<&Document> {
        if !self.panels_visible {
            return None;
        }
        self.documents
            .get(&self.active_document()?)
            .filter(|document| !document.readonly && document.path.local().is_some())
    }

    pub fn toggle_vscode_extension(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.settings.vscode.enabled(id) {
            self.settings
                .vscode
                .enabled
                .retain(|entry| !entry.eq_ignore_ascii_case(id));
            self.reset_vscode_host();
        } else {
            self.settings.vscode.enabled.push(id.into());
        }
        self.refresh_vscode_language_servers(false, cx);
        self.persist();
        cx.notify();
    }

    pub(crate) fn reset_vscode_host(&mut self) {
        self.vscode_host.reset();
        self.vscode_language_command = None;
        if self.status == VSCODE_RUNNING {
            self.status.clear();
        }
    }

    fn invoke_vscode(&mut self, command_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.id();
        let Some(document) = self.vscode_document() else {
            return;
        };
        let Some((extension, command)) = self
            .vscode_commands()
            .find(|(_, command)| command.id == command_id)
        else {
            return;
        };
        let editor = document.input.read(cx);
        let text = editor.text().clone();
        let selection = editor.selected_range();
        let reversed = editor.is_selection_reversed();
        let extension_id = extension.id.clone();
        let command_name = command.command.clone();
        if extension.provides_language() {
            let Some(buffer) = &document.lsp else {
                return;
            };
            let invocation = (id, buffer.session());
            let pane = self.active;
            let origin = document.id;
            let version = document.version;
            let (anchor, active) = if reversed {
                (selection.end, selection.start)
            } else {
                (selection.start, selection.end)
            };
            match buffer.execute_command(&command.command, &text, anchor, active) {
                Ok(reply) => {
                    self.vscode_language_command = Some(invocation);
                    self.status = VSCODE_RUNNING.into();
                    cx.spawn_in(window, async move |this, cx| {
                        let result = reply
                            .recv()
                            .await
                            .map_err(|_| anyhow::anyhow!("Language server disconnected"))
                            .and_then(|result| result)
                            .and_then(|value| {
                                Ok(serde_json::from_value::<
                                    Option<vscode::LanguageCommandResult>,
                                >(value)?)
                            });
                        let _ = this.update_in(cx, |this, window, cx| {
                            if this.vscode_language_command != Some(invocation) {
                                return;
                            }
                            this.vscode_language_command = None;
                            if this.status == VSCODE_RUNNING {
                                this.status.clear();
                            }
                            match result {
                                Err(error) => this.report_vscode_error(
                                    &extension_id,
                                    &command_name,
                                    error,
                                    cx,
                                ),
                                Ok(Some(action))
                                    if this.modal.is_none()
                                        && this.active == pane
                                        && this.active_document() == Some(origin)
                                        && this.documents[&origin].version == version =>
                                {
                                    match action {
                                        vscode::LanguageCommandResult::GoToLocation {
                                            location,
                                        } => {
                                            if let Some(path) = lsp::uri_to_path(&location.uri) {
                                                this.open_source_location(
                                                    pane,
                                                    path,
                                                    Some(location.range.start),
                                                    window,
                                                    cx,
                                                );
                                            } else {
                                                this.report_vscode_error(
                                                    &extension_id,
                                                    &command_name,
                                                    anyhow::anyhow!(
                                                        "Cannot open source location {}",
                                                        location.uri.as_str()
                                                    ),
                                                    cx,
                                                );
                                            }
                                        }
                                        vscode::LanguageCommandResult::Status { message } => {
                                            this.status = message
                                        }
                                    }
                                }
                                Ok(_) => {}
                            }
                            cx.notify();
                        });
                    })
                    .detach();
                }
                Err(error) => self.report_vscode_error(&extension_id, &command_name, error, cx),
            }
            cx.notify();
            return;
        }
        let snapshot = vscode::Snapshot::new(
            document.path.local().expect("local editor"),
            document.version,
            &text,
            selection,
            reversed,
        );
        let document_id = document.id;
        let invocation = extension.invocation(command, &self.settings.vscode, id, snapshot);
        match self
            .vscode_host
            .run(&self.settings.vscode, invocation, document_id, text)
        {
            Ok(()) => self.status = VSCODE_RUNNING.into(),
            Err(error) => self.report_vscode_error(&extension_id, &command_name, error, cx),
        }
        cx.notify();
    }

    fn report_vscode_error(
        &mut self,
        extension: &str,
        command: &str,
        error: anyhow::Error,
        cx: &mut Context<Self>,
    ) {
        self.extension_logs
            .source(extension, "VS Code commands")
            .error(&format!("{command}: {error:#}"));
        self.status = format!("VS Code extension: {error}");
        cx.notify();
    }

    fn handle_vscode(&mut self, event: vscode::Event, window: &mut Window, cx: &mut Context<Self>) {
        if !self.vscode_host.is_current(event.process) {
            return;
        }
        let reply = match event.reply {
            Ok(reply) => reply,
            Err(error) => {
                let pending = self.vscode_host.pending.take();
                self.vscode_host.reset();
                if let Some(pending) = pending {
                    self.report_vscode_error(&pending.extension, &pending.command, error, cx);
                } else {
                    self.extension_logs
                        .source("dotdir", "VS Code commands")
                        .error(&format!("{error:#}"));
                }
                return;
            }
        };
        match reply {
            vscode::Reply::Log { extension, message } => {
                self.extension_logs
                    .source(&extension, "VS Code commands")
                    .append(&message);
            }
            vscode::Reply::Edit {
                id,
                serial,
                version,
                edits,
            } => {
                let updated = self.apply_vscode_edit(id, version, edits, window, cx);
                if updated.is_none() && self.status == VSCODE_RUNNING {
                    self.status = "VS Code extension edit rejected because the buffer changed or the edit was invalid".into();
                }
                if let Err(error) = self.vscode_host.acknowledge(id, serial, updated) {
                    self.error(error, cx);
                }
            }
            vscode::Reply::Done { id, error } => {
                if let Some(pending) = self.vscode_host.pending.take_if(|pending| pending.id == id)
                {
                    if let Some(error) = error {
                        self.report_vscode_error(
                            &pending.extension,
                            &pending.command,
                            error.into(),
                            cx,
                        );
                    } else if self.status == VSCODE_RUNNING {
                        self.status.clear();
                    }
                }
            }
        }
    }

    fn apply_vscode_edit(
        &mut self,
        id: u64,
        version: u64,
        edits: Vec<vscode::Edit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<vscode::Snapshot> {
        let pending = self.vscode_host.pending.as_mut()?;
        if id != pending.id || version != pending.version {
            return None;
        }
        let document = self
            .documents
            .get(&pending.document)
            .filter(|doc| !doc.readonly)?;
        let path = document.path.local()?;
        document.input.update(cx, |editor, cx| {
            if editor.text() != &pending.text {
                return None;
            }
            let batch = vscode::EditBatch::prepare(editor.text(), edits)?;
            if batch.apply(editor, window, cx) {
                pending.version += 1;
                pending.text = editor.text().clone();
            }
            Some(vscode::Snapshot::new(
                path,
                pending.version,
                editor.text(),
                editor.selected_range(),
                editor.is_selection_reversed(),
            ))
        })
    }
    pub(crate) fn extension_selection(&self) -> (usize, Vec<Entry>) {
        let source = if self.directory(self.active).is_some() {
            self.active
        } else {
            1 - self.active
        };
        let selected = self
            .directory(source)
            .map(Directory::selected)
            .unwrap_or_default();
        (source, selected)
    }
    pub fn extension_context(&self) -> serde_json::Value {
        let (source, selected) = self.extension_selection();
        json!({"path":self.pane_location(source).display(),"oppositePath":self.pane_location(1-source).display(),"selected":selected.iter().map(|e|json!({"name":e.name,"path":e.path.display(),"size":e.size,"directory":e.directory})).collect::<Vec<_>>()})
    }
    pub fn invoke_extension(
        &mut self,
        extension: String,
        command: String,
        title: String,
        event: serde_json::Value,
        _cx: &mut Context<Self>,
    ) {
        let Some(host) = &self.extension_host else {
            return;
        };
        let target = (!event.is_null())
            .then(|| {
                let pane = &self.panes[self.active];
                let tab = &pane.tabs[pane.active];
                matches!(&tab.content, TabContent::Panel { extension: e, command: c, .. }
                if e == &extension && c == &command)
                .then(|| tab.id())
            })
            .flatten();
        self.extension_request += 1;
        let request = self.extension_request;
        let rx = host.invoke(
            extension.clone(),
            command.clone(),
            self.extension_context(),
            event,
        );
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            if let Ok(result) = rx.recv() {
                let _ = tx.send(Message::ExtensionReply {
                    request,
                    target,
                    extension,
                    command,
                    title,
                    result,
                });
            }
        });
    }
    pub fn ensure_terminal(&mut self, cx: &mut Context<Self>) {
        if self.terminals.is_empty() {
            self.new_terminal(cx);
        }
    }
    pub fn new_terminal(&mut self, cx: &mut Context<Self>) {
        let id = self.id();
        match Terminal::spawn(id, &self.cwd(self.active), None) {
            Ok(t) => {
                self.terminals.push(t);
                self.terminal_active = self.terminals.len() - 1;
            }
            Err(e) => self.error(e, cx),
        }
    }
    pub fn execute_shell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let command = self.command_line.read(cx).value().to_string();
        if command.trim().is_empty() {
            return;
        }
        if command == "cd" || command.starts_with("cd ") {
            let path = if command == "cd" {
                "~"
            } else {
                command[3..].trim()
            };
            match self.resolve_directory(path, false) {
                Ok(path) => self.navigate(self.active, path, window, cx),
                Err(error) => {
                    self.error(error, cx);
                    return;
                }
            }
        } else {
            if matches!(self.pane_location(self.active), Location::Provider(_)) {
                self.error("Shell commands require a local directory", cx);
                return;
            }
            self.ensure_terminal(cx);
            self.terminal_visible = true;
            if let Some(t) = self.terminals.get_mut(self.terminal_active) {
                let _ = t.write(format!("{command}\r").as_bytes());
            }
            self.terminal_focus.focus(window, cx);
        }
        self.command_line
            .update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }
    pub fn terminal_key(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = &event.keystroke.key;
        let m = event.keystroke.modifiers;
        if m.platform && key == "v" {
            if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text())
                && let Some(t) = self.terminals.get_mut(self.terminal_active)
            {
                let _ = t.paste(&text);
            }
            cx.stop_propagation();
            return;
        }
        if m.platform {
            return;
        }
        let text = match key.as_str() {
            "enter" => "\r".into(),
            "backspace" => "\x7f".into(),
            "tab" => "\t".into(),
            "escape" => "\x1b".into(),
            "up" => "\x1b[A".into(),
            "down" => "\x1b[B".into(),
            "right" => "\x1b[C".into(),
            "left" => "\x1b[D".into(),
            "home" => "\x1b[H".into(),
            "end" => "\x1b[F".into(),
            "delete" => "\x1b[3~".into(),
            "pageup" => "\x1b[5~".into(),
            "pagedown" => "\x1b[6~".into(),
            k if m.control && k.len() == 1 => {
                String::from_utf8(vec![k.as_bytes()[0].to_ascii_lowercase() & 0x1f])
                    .unwrap_or_default()
            }
            _ => event.keystroke.key_char.clone().unwrap_or_default(),
        };
        if !text.is_empty() {
            if let Some(t) = self.terminals.get_mut(self.terminal_active) {
                let text = if m.alt { format!("\x1b{text}") } else { text };
                if let Err(e) = t.write(text.as_bytes()) {
                    self.error(e, cx);
                }
            }
            cx.stop_propagation();
        }
    }
    pub fn pane_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.modified() {
            return;
        }
        if let Some(text) = &event.keystroke.key_char {
            if text.chars().any(char::is_control) || text == " " {
                return;
            }
            if self.last_typeahead.elapsed() > Duration::from_millis(900) {
                self.typeahead.clear();
            }
            self.last_typeahead = Instant::now();
            self.typeahead.push_str(text);
            let query = self.typeahead.to_lowercase();
            if let Some(d) = self.directory_mut(self.active)
                && let Some(index) = d
                    .entries
                    .iter()
                    .position(|e| e.name.to_lowercase().starts_with(&query))
            {
                d.cursor = index;
            }
            self.update_column_preview(self.active);
            self.reveal_column(self.active, true);
            cx.notify();
        }
    }
    pub fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut changed = false;
        if let Some(pending) = self.vscode_host.expire() {
            self.report_vscode_error(
                &pending.extension,
                &pending.command,
                anyhow::anyhow!("Command timed out"),
                cx,
            );
        }
        for _ in 0..200 {
            let Ok(message) = self.rx.try_recv() else {
                break;
            };
            changed = true;
            match message {
                Message::Market(event) => self.handle_marketplace(event, cx),
                Message::MountedRoots(roots) => self.mounted_roots = roots,
                Message::IconTheme(request, result) => {
                    if request != self.icon_theme_request {
                        continue;
                    }
                    match result {
                        Ok(theme) => {
                            self.status = format!("File icons: {} {}", theme.name, theme.version);
                            self.settings.icon_theme = Some(theme.root.clone());
                            self.icon_theme = Some(*theme);
                            self.persist();
                        }
                        Err(error) => self.error(format!("Icon theme: {error:#}"), cx),
                    }
                }
                Message::ColorTheme(request, result) => {
                    if request != self.color_theme_request {
                        continue;
                    }
                    match result {
                        Ok(theme) => {
                            theme.apply(cx);
                            self.palette = theme.palette;
                            self.refresh_diagnostic_decorations(cx);
                            self.dismiss_hover(cx);
                            self.status = format!("Color theme: {}", theme.name);
                            self.color_theme_name = Some(theme.name);
                            self.settings.color_theme = Some(theme.source);
                            self.persist();
                        }
                        Err(error) => self.error(format!("Color theme: {error:#}"), cx),
                    }
                }
                Message::Lsp(notice) => match notice {
                    lsp::Notice::CommandsChanged => {}
                    lsp::Notice::QuickPick {
                        session,
                        request,
                        reply,
                    } => {
                        if self.modal.is_none()
                            && self
                                .vscode_language_command
                                .is_some_and(|(_, owner)| owner == session)
                        {
                            self.show_modal(
                                Modal::QuickPick {
                                    request,
                                    action: QuickPickAction::Extension { session, reply },
                                },
                                window,
                                cx,
                            );
                        }
                    }
                    lsp::Notice::QuickPickClosed { session } => {
                        self.dismiss_quick_pick(session, window, cx);
                    }
                    lsp::Notice::Ready { session, document } => {
                        if let Some(doc) = self.documents.get_mut(&document)
                            && let Some(buffer) = doc
                                .lsp
                                .as_ref()
                                .filter(|buffer| buffer.session() == session)
                            && doc
                                .lsp_status
                                .as_deref()
                                .is_some_and(|status| status.starts_with("Starting "))
                        {
                            doc.lsp_status = Some(buffer.server_name().into());
                        }
                    }
                    lsp::Notice::Diagnostics {
                        session,
                        document,
                        revision,
                        items,
                    } => {
                        if let Some(doc) = self.documents.get_mut(&document)
                            && let Some(buffer) = doc.lsp.as_ref().filter(|buffer| {
                                buffer.session() == session && buffer.revision() == revision
                            })
                        {
                            let name = buffer.server_name();
                            doc.lsp_status = Some(match items.len() {
                                0 => name.into(),
                                1 => format!("{name} · 1 diagnostic"),
                                count => format!("{name} · {count} diagnostics"),
                            });
                            buffer.set_diagnostics(items);
                            doc.refresh_diagnostic_decorations(cx);
                            self.refresh_hover_diagnostics(document);
                        }
                    }
                    lsp::Notice::Failed { session, error } => {
                        self.dismiss_quick_pick(session, window, cx);
                        for doc in self.documents.values_mut().filter(|doc| {
                            doc.lsp
                                .as_ref()
                                .is_some_and(|buffer| buffer.session() == session)
                        }) {
                            doc.clear_language_server(Some(error.clone()), cx);
                        }
                    }
                    lsp::Notice::CodeActionFailed {
                        session,
                        document,
                        error,
                    } => {
                        if self
                            .documents
                            .get(&document)
                            .and_then(|doc| doc.lsp.as_ref())
                            .is_some_and(|buffer| buffer.session() == session)
                        {
                            self.status = format!("Code action: {error}");
                        }
                    }
                },
                Message::Directory(id, generation, result) => {
                    let Some(directory) = self
                        .directory_by_id(id)
                        .filter(|d| d.generation == generation)
                    else {
                        continue;
                    };
                    directory.apply(generation, result);
                    let local = directory.path.local().map(PathBuf::from);
                    if let Some(w) = &self.watcher {
                        w.remove(&id.to_string());
                        if let Some(path) = local {
                            w.add(&id.to_string(), &path.to_string_lossy());
                        }
                    }
                    let mut old_ids = vec![];
                    for pane in &mut self.panes {
                        for tab in &mut pane.tabs {
                            if let TabContent::Directory(browser) = &mut tab.content {
                                old_ids.extend(browser.columns.iter().map(|d| d.id));
                                browser.reconcile();
                            }
                        }
                    }
                    self.unwatch_removed_columns(old_ids);
                }
                Message::Watch(id) => {
                    self.pending_watch.entry(id).or_insert_with(Instant::now);
                }
                Message::FolderSize(event) => {
                    if let Err(error) = self.folder_sizes.update(event)
                        && !error.is::<UnlockCancelled>()
                    {
                        self.error(format!("{error:#}"), cx);
                    }
                }
                Message::ArchivePassword(request) => self.password_requests.push_back(request),
                Message::Opened {
                    request,
                    pane,
                    path,
                    readonly,
                    tab,
                    byte_mode,
                    result,
                } => {
                    let pane = match tab {
                        Some(id) => {
                            let Some((pane, index)) = self.tab_position(id) else {
                                continue;
                            };
                            if !self.panes[pane].tabs[index].accepts_load(request) {
                                continue;
                            }
                            pane
                        }
                        None if self.open_request[pane] == request => pane,
                        None => continue,
                    };
                    let source_position = if tab.is_none() {
                        self.source_jump[pane]
                            .take()
                            .and_then(|(reply, position)| (request == reply).then_some(position))
                    } else {
                        None
                    };
                    match result {
                        Err(e) => {
                            if let Some(id) = tab {
                                if let Some(TabContent::Pending { error, .. }) = self.panes[pane]
                                    .tabs
                                    .iter_mut()
                                    .find(|tab| tab.id() == id)
                                    .map(|tab| &mut tab.content)
                                {
                                    *error = Some(e.to_string());
                                }
                            } else if e.is::<UnlockCancelled>() {
                                self.status = e.to_string();
                            } else {
                                self.error(e, cx);
                            }
                        }
                        Ok(OpenedFile::Image(format, bytes)) => {
                            let id = self.id();
                            self.install_content(
                                pane,
                                TabContent::Image {
                                    id,
                                    path,
                                    image: Some(Arc::new(Image::from_bytes(format, bytes))),
                                },
                                tab,
                                window,
                                cx,
                            );
                            if tab.is_none() {
                                self.status.clear();
                            }
                        }
                        Ok(OpenedFile::Hex(file)) => {
                            let buffer = self
                                .panes
                                .iter()
                                .flat_map(|pane| &pane.tabs)
                                .find_map(|tab| match &tab.content {
                                    TabContent::Hex {
                                        path: existing,
                                        view,
                                        ..
                                    } if *existing == path => Some(view.read(cx).buffer.clone()),
                                    _ => None,
                                })
                                .unwrap_or_else(|| cx.new(|_| HexBuffer::new(file)));
                            let id = tab.unwrap_or_else(|| self.id());
                            let content =
                                self.hex_content(id, path, buffer, readonly, byte_mode, window, cx);
                            self.install_content(pane, content, tab, window, cx);
                            if tab.is_none() {
                                self.status.clear();
                            }
                        }
                        Ok(OpenedFile::Text(text)) => {
                            // A second pane may have completed the same canonical path first.
                            let id = if let Some(document) =
                                self.documents.values_mut().find(|d| d.path == path)
                            {
                                if !readonly {
                                    document.readonly = false;
                                }
                                document.id
                            } else {
                                let id = self.id();
                                let language = language_for(path.name_path());
                                let bytes = text.as_bytes().to_vec();
                                let input = cx.new(|cx| {
                                    EditorState::new(window, cx)
                                        .language(language)
                                        .soft_wrap(true)
                                        .default_value(text)
                                });
                                let subscription =
                                    cx.subscribe_in(&input, window, move |this, input, event: &InputEvent, window, cx| {
                                        if matches!(event, InputEvent::Change)
                                            && let Some(doc) = this.documents.get_mut(&id)
                                        {
                                            let was_dirty = doc.dirty();
                                            doc.version += 1;
                                            doc.clear_diagnostic_decorations(cx);
                                            if let Some(view) = &doc.inactive_view {
                                                let text = input.read(cx).value();
                                                view.input.update(cx, |editor, cx| {
                                                    let scroll = editor.scroll_offset();
                                                    editor.set_value(text, window, cx);
                                                    editor.set_scroll_offset(scroll, cx);
                                                });
                                            }
                                            let text = input.read(cx).text().clone();
                                            if doc.path.archive_member().is_some() && !doc.readonly {
                                                this.language_servers.archive_text(&doc.path, Some(text.clone()));
                                            }
                                            if let Some(buffer) = &doc.lsp {
                                                buffer.change(text);
                                            }
                                            if this.hover.is_for_document(id) && this.hover.clear() {
                                                cx.notify();
                                            }
                                            // The editor redraws itself. Rebuild the surrounding
                                            // panes only when their modified indicator changes.
                                            if !was_dirty && doc.dirty() {
                                                for tab in this.panes.iter_mut().flat_map(|pane| &mut pane.tabs) {
                                                    if matches!(tab.content, TabContent::Document(doc) if doc == id) { tab.make_permanent(); }
                                                }
                                                cx.notify();
                                            }
                                        }
                                    });
                                self.documents.insert(
                                    id,
                                    Document {
                                        id,
                                        path,
                                        input,
                                        inactive_view: None,
                                        diagnostics: DiagnosticDecorations::default(),
                                        saved: bytes,
                                        version: 1,
                                        saved_version: 1,
                                        readonly,
                                        line_wrap: Some(true),
                                        saving: false,
                                        error: None,
                                        lsp: None,
                                        lsp_status: None,
                                        _subscription: subscription,
                                    },
                                );
                                id
                            };
                            self.enable_language_server(id, cx);
                            self.install_content(pane, TabContent::Document(id), tab, window, cx);
                            if let Some(position) = source_position {
                                self.reveal_source_position(id, position, window, cx);
                            }
                            if tab.is_none() {
                                self.status.clear();
                            }
                        }
                    }
                }
                Message::Saved {
                    id,
                    version,
                    text,
                    result,
                } => {
                    if let Some(d) = self.documents.get_mut(&id) {
                        d.saving = false;
                        match result {
                            Ok(()) => {
                                if let Some(buffer) = &d.lsp {
                                    buffer.save();
                                }
                                d.saved = text.into_bytes();
                                d.saved_version = version;
                                // An edit and its Undo can both finish while a save is pending.
                                d.reconcile_saved_text(cx);
                                d.error = None;
                                self.status = format!("Saved {}", d.path.display());
                                if d.path.archive_member().is_some()
                                    && let Some(host) = d.path.host_path().map(Path::to_path_buf)
                                {
                                    self.refresh_directories(|path| {
                                        path.host_path() == Some(host.as_path())
                                            || path.local() == host.parent()
                                    });
                                }
                            }
                            Err(e) => {
                                d.error = Some(e.to_string());
                                self.status = e.to_string();
                            }
                        }
                    }
                }
                Message::Search(id, event) => {
                    if let Some(s) = self.search.as_mut().filter(|s| s.id == id) {
                        match event {
                            FileSearchEvent::Match { r#match: m } => {
                                s.count += 1;
                                if s.results.len() < 10000 {
                                    s.results.push(m.path.into());
                                }
                            }
                            FileSearchEvent::Done { found }
                            | FileSearchEvent::Cancelled { found } => {
                                s.count = found;
                                s.done = true;
                            }
                            FileSearchEvent::Error { message, .. } => {
                                s.error = Some(message);
                                s.done = true;
                            }
                        }
                    }
                }
                Message::Extension(result) => {
                    match result {
                        Ok(e) => {
                            self.extension_logs.register(&e.manifest.id);
                            if let Some(i) = self
                                .extensions
                                .iter()
                                .position(|x| x.manifest.id == e.manifest.id)
                            {
                                self.extensions[i] = e;
                            } else {
                                self.extensions.push(e);
                            }
                            self.refresh_directories(|path| matches!(path, Location::Provider(address) if address.is_log()));
                        }
                        Err(e) => self.report_extension_error(e.to_string()),
                    }
                }
                Message::VsCode(event) => self.handle_vscode(event, window, cx),
                Message::ExtensionReply {
                    request,
                    target,
                    extension,
                    command,
                    title,
                    result,
                } => {
                    if request != self.extension_request {
                        continue;
                    }
                    match result {
                        Err(e) => {
                            self.extension_logs.append(&extension, &e.to_string());
                            self.error(format!("Extension: {e}"), cx);
                        }
                        Ok(reply) => {
                            if let Some(message) = reply.message {
                                self.extension_logs.append(&extension, &message);
                                self.status = message;
                            }
                            if let Some(node) = reply.panel {
                                let existing = if let Some(target) = target {
                                    let Some(position) = self.tab_position(target) else {
                                        continue;
                                    };
                                    Some(position)
                                } else {
                                    self.panes.iter().enumerate().find_map(|(pane, tabs)| {
                                        tabs.tabs
                                            .iter()
                                            .position(|tab| {
                                                matches!(&tab.content, TabContent::Panel {
                                                extension: e, command: c, ..
                                            } if e == &extension && c == &command)
                                            })
                                            .map(|index| (pane, index))
                                    })
                                };
                                if let Some((p, i)) = existing {
                                    if let TabContent::Panel { node: n, .. } =
                                        &mut self.panes[p].tabs[i].content
                                    {
                                        *n = node;
                                    }
                                    self.panes[p].active = i;
                                    self.focus_pane(p, window, cx);
                                } else {
                                    let id = self.id();
                                    let pane = 1 - self.active;
                                    self.panes[pane].tabs.push(Tab::new(TabContent::Panel {
                                        id,
                                        title,
                                        extension,
                                        command,
                                        node,
                                    }));
                                    self.panes[pane].active = self.panes[pane].tabs.len() - 1;
                                    self.focus_pane(pane, window, cx);
                                }
                            }
                        }
                    }
                }
                Message::ArchiveFileCreated {
                    pane,
                    directory,
                    path,
                } => {
                    if self.pane_location(pane) == directory {
                        self.open_document_into(pane, path, false, None, window, cx);
                    }
                }
                Message::Simple(result) => {
                    if let Err(e) = result {
                        self.error(e, cx);
                    }
                    self.refresh_all();
                }
            }
        }
        let ready = self
            .pending_watch
            .iter()
            .filter(|(_, t)| t.elapsed() > Duration::from_millis(100))
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in ready {
            self.pending_watch.remove(&id);
            self.reload_directory(id, false);
            changed = true;
        }
        let mut settled = None;
        let mut focus_conflict = false;
        if let Some(op) = &mut self.operation
            && !matches!(self.modal, Some(Modal::ArchivePassword { .. }))
        {
            for _ in 0..100 {
                let Ok(event) = op.events.try_recv() else {
                    break;
                };
                changed = true;
                match event {
                    OperationEvent::Progress(CopyEvent::Progress(p)) => {
                        op.fraction = if p.bytes_total > 0 {
                            p.bytes_copied as f32 / p.bytes_total as f32
                        } else {
                            0.
                        };
                        op.progress = format!(
                            "{} / {} files · {} / {} · {}",
                            p.files_done,
                            p.files_total,
                            human_size(p.bytes_copied),
                            human_size(p.bytes_total),
                            p.current_file
                        );
                    }
                    OperationEvent::Progress(CopyEvent::Error { message }) => op.progress = message,
                    OperationEvent::Conflict {
                        source,
                        destination,
                        reply,
                    } => {
                        self.palette_cursor = 0;
                        self.modal = Some(Modal::Conflict {
                            source,
                            destination,
                            reply,
                        });
                        focus_conflict = true;
                    }
                    OperationEvent::Settled(result) => settled = Some(result),
                    _ => {}
                }
            }
        }
        if focus_conflict {
            self.focus_modal(window, cx);
        }
        if let Some(result) = settled {
            let cancelled = self
                .operation
                .as_ref()
                .is_some_and(|o| o.token.is_cancelled());
            self.operation = None;
            if matches!(self.modal, Some(Modal::Conflict { .. })) {
                self.modal = None;
                self.focus_pane(self.active, window, cx);
            }
            self.status = match result {
                Ok(()) => {
                    if cancelled {
                        "Operation cancelled; completed changes are retained".into()
                    } else {
                        "Operation completed".into()
                    }
                }
                Err(e) => e,
            };
            self.refresh_all();
            if self.quit_pending {
                self.persist();
                self.shutting_down = true;
                window.remove_window();
                cx.quit();
                return;
            }
        }
        self.filesystems.archive_passwords.expire();
        if matches!(self.modal.as_ref(), Some(Modal::ArchivePassword { request, .. }) if request.cancel.is_cancelled())
        {
            self.dismiss(window, cx);
            changed = true;
        }
        if self.modal.is_none() && !self.quit_pending {
            while let Some(request) = self.password_requests.pop_front() {
                if request.cancel.is_cancelled() {
                    continue;
                }
                let input = cx.new(|cx| InputState::new(window, cx).masked(true));
                self.modal = Some(Modal::ArchivePassword { request, input });
                self.modal_error = None;
                self.focus_modal(window, cx);
                changed = true;
                break;
            }
        }
        self.sync_previews(window, cx);
        if self
            .terminals
            .iter()
            .any(|t| t.dirty.swap(false, Ordering::Relaxed))
        {
            changed = true;
        }
        let logs_changed =
            matches!(self.modal, Some(Modal::ExtensionLogs)) && self.extension_logs.take_changed();
        if logs_changed
            && self.extension_log_scroll.offset().y + self.extension_log_scroll.max_offset().y
                <= px(4.)
        {
            self.extension_log_scroll.scroll_to_bottom();
        }
        changed |= self.update_quick_actions(window, cx);
        if changed || logs_changed {
            cx.notify();
            window.refresh();
        }
    }
}
struct InactiveEditor {
    input: Entity<EditorState>,
    diagnostics: DiagnosticDecorations,
}

pub(crate) struct Document {
    pub id: u64,
    pub path: Location,
    pub input: Entity<EditorState>,
    inactive_view: Option<InactiveEditor>,
    pub(crate) diagnostics: DiagnosticDecorations,
    pub saved: Vec<u8>,
    version: u64,
    saved_version: u64,
    pub readonly: bool,
    line_wrap: Option<bool>,
    pub saving: bool,
    pub error: Option<String>,
    pub lsp: Option<Rc<lsp::Buffer>>,
    pub lsp_status: Option<String>,
    pub _subscription: Subscription,
}
impl Document {
    pub(crate) fn inactive_editor(&self) -> Option<&Entity<EditorState>> {
        self.inactive_view.as_ref().map(|view| &view.input)
    }

    fn diagnostic_styles(&self, cx: &App) -> DiagnosticStyles {
        self.lsp
            .as_ref()
            .and_then(|buffer| {
                let diagnostics = buffer.diagnostics();
                diagnostics
                    .as_ref()
                    .map(|set| DiagnosticStyles::new(self.input.read(cx).text(), set, cx))
            })
            .unwrap_or_default()
    }

    fn refresh_diagnostic_decorations(&mut self, cx: &mut App) {
        let styles = self.diagnostic_styles(cx);
        if let Some(view) = &mut self.inactive_view {
            view.diagnostics.set(&view.input, styles.clone(), cx);
        }
        self.diagnostics.set(&self.input, styles, cx);
    }

    fn clear_diagnostic_decorations(&mut self, cx: &mut App) {
        self.diagnostics.clear(cx);
        if let Some(view) = &mut self.inactive_view {
            view.diagnostics.clear(cx);
        }
    }

    fn clear_language_server(&mut self, status: Option<String>, cx: &mut App) {
        self.lsp = None;
        self.lsp_status = status;
        self.input.update(cx, |state, cx| {
            let lsp = state.lsp_mut();
            lsp.completion_provider = None;
            lsp.hover_provider = None;
            lsp.definition_provider = None;
            lsp.code_action_providers.clear();
            lsp.show_document = None;
            cx.notify();
        });
        self.clear_diagnostic_decorations(cx);
    }

    pub fn dirty(&self) -> bool {
        !self.readonly && self.version != self.saved_version
    }

    fn reconcile_saved_text(&mut self, cx: &App) -> bool {
        if !self.dirty() {
            return false;
        }
        let text = self.input.read(cx).text();
        if text.len() != self.saved.len() {
            return false;
        }
        let mut remaining = self.saved.as_slice();
        for chunk in text.chunks() {
            let Some(rest) = remaining.strip_prefix(chunk.as_bytes()) else {
                return false;
            };
            remaining = rest;
        }
        self.saved_version = self.version;
        true
    }
}
pub(crate) enum TabContent {
    Pending {
        id: u64,
        path: Option<Location>,
        error: Option<String>,
    },
    Directory(DirectoryBrowser),
    Document(u64),
    Hex {
        id: u64,
        path: Location,
        view: Entity<HexView>,
        _subscription: Subscription,
    },
    Image {
        id: u64,
        path: Location,
        image: Option<Arc<Image>>,
    },
    Media {
        id: u64,
        path: Location,
        view: Entity<MediaViewer>,
        _subscription: Subscription,
    },
    Panel {
        id: u64,
        title: String,
        extension: String,
        command: String,
        node: PanelNode,
    },
    SelectionInspector {
        id: u64,
        snapshot: SelectionSnapshot,
    },
}
impl TabContent {
    pub fn id(&self) -> u64 {
        match self {
            Self::Directory(d) => d.id,
            Self::Document(id) => *id,
            Self::Panel { id, .. }
            | Self::SelectionInspector { id, .. }
            | Self::Hex { id, .. }
            | Self::Image { id, .. }
            | Self::Media { id, .. }
            | Self::Pending { id, .. } => *id,
        }
    }
}
/// Tab identity and presentation state survive replacement of preview content.
pub(crate) struct Tab {
    pub content: TabContent,
    pub preview: bool,
    pub preview_source: Option<u64>,
    id: u64,
    load_request: u64,
    preview_path: Option<Location>,
    source_position: Option<lsp_types::Position>,
}
impl Tab {
    pub fn new(content: TabContent) -> Self {
        Self {
            id: content.id(),
            content,
            preview: false,
            preview_source: None,
            load_request: 0,
            preview_path: None,
            source_position: None,
        }
    }
    pub fn id(&self) -> u64 {
        self.id
    }
    fn with_id(id: u64, content: TabContent) -> Self {
        Self {
            id,
            ..Self::new(content)
        }
    }
    pub fn make_permanent(&mut self) {
        self.preview = false;
        self.preview_source = None;
    }
    fn follows_changed_file(&self, source: u64, path: &Option<Location>) -> bool {
        self.preview && self.preview_source == Some(source) && &self.preview_path != path
    }
    fn is_definition_preview(&self) -> bool {
        self.preview && self.preview_source.is_none()
    }
    fn begin_preview_load(&mut self, path: Option<Location>, request: u64) {
        self.preview_path = path.clone();
        self.load_request = request;
        self.content = TabContent::Pending {
            id: self.id,
            path,
            error: None,
        };
    }
    fn accepts_load(&self, request: u64) -> bool {
        self.load_request == request
    }
}
pub(crate) struct Pane {
    pub tabs: Vec<Tab>,
    pub active: usize,
}

#[derive(Default)]
pub(crate) struct TabStripScroll {
    pub handle: ScrollHandle,
    last_selected: Option<(usize, u64)>,
    pub manually_scrolled: bool,
    pub return_timer: Option<Task<()>>,
    return_motion: Option<(Pixels, Instant)>,
}

impl TabStripScroll {
    pub fn manual_scroll(&mut self) {
        self.manually_scrolled = true;
        self.return_timer = None;
        self.return_motion = None;
    }

    pub fn return_to_active(&mut self) {
        self.manually_scrolled = false;
        self.return_timer = None;
        self.return_motion = Some((self.handle.offset().x, Instant::now()));
    }

    pub fn follow(&mut self, index: usize, id: u64, reduce_motion: bool, window: &mut Window) {
        if self.last_selected != Some((index, id)) {
            self.last_selected = Some((index, id));
            self.manually_scrolled = false;
            self.return_timer = None;
            self.return_motion = None;
        }
        if self.manually_scrolled {
            return;
        }
        if !reduce_motion
            && let Some((from, started)) = self.return_motion
            && let Some(tab) = self.handle.bounds_for_item(index)
        {
            let viewport = self.handle.bounds();
            let mut offset = self.handle.offset();
            let x = offset.x;
            let left = viewport.left() - tab.left();
            let right = viewport.right() - tab.right();
            let target = if left > right {
                left
            } else {
                x.clamp(left, right)
            }
            .clamp(-self.handle.max_offset().x, px(0.));
            if (target - x).abs() > px(0.5) {
                let progress = (started.elapsed().as_secs_f32() / 0.22).min(1.);
                offset.x = from + (target - from) * (1. - (1. - progress).powi(3));
                self.handle.set_offset(offset);
                if progress < 1. {
                    window.request_animation_frame();
                    return;
                }
            }
        }
        self.return_motion = None;
        self.handle.scroll_to_item(index);
    }
}

impl Pane {
    fn restore(paths: &[SavedLocation], saved_active: usize, next_id: &mut u64) -> Self {
        let mut tabs = Vec::new();
        let mut active = 0;
        for (index, path) in paths.iter().enumerate() {
            if let Some(location) = path.clone().restore(&ARCHIVE_FORMATS) {
                if index <= saved_active {
                    active = tabs.len();
                }
                tabs.push(Tab::new(TabContent::Directory(DirectoryBrowser::new(
                    *next_id, location,
                ))));
                *next_id += 1;
            }
        }
        if tabs.is_empty() {
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
            tabs.push(Tab::new(TabContent::Directory(DirectoryBrowser::new(
                *next_id,
                home.into(),
            ))));
            *next_id += 1;
        }
        Self { tabs, active }
    }

    fn saved_directories(&self) -> (Vec<SavedLocation>, usize) {
        let mut paths = Vec::new();
        let mut active = 0;
        for (index, tab) in self.tabs.iter().enumerate() {
            if let TabContent::Directory(browser) = &tab.content {
                if index <= self.active {
                    active = paths.len();
                }
                paths.push(SavedLocation::from(&browser.current().path));
            }
        }
        (paths, active)
    }

    fn insert_after_active(&mut self, tab: Tab) {
        let index = (self.active + 1).min(self.tabs.len());
        self.tabs.insert(index, tab);
        self.active = index;
    }

    fn remove_tab(&mut self, index: usize) -> Tab {
        let closed = self.tabs.remove(index);
        let file_tab = matches!(
            closed.content,
            TabContent::Document(_)
                | TabContent::Hex { .. }
                | TabContent::Image { .. }
                | TabContent::Media { .. }
                | TabContent::Pending { .. }
        );
        if index < self.active || (index == self.active && file_tab) {
            self.active = self.active.saturating_sub(1);
        }
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        closed
    }

    fn reorder_tab(&mut self, from: usize, slot: usize) -> usize {
        let slot = slot - usize::from(slot > from);
        let tab = self.tabs.remove(from);
        self.tabs.insert(slot, tab);
        slot
    }

    fn opposite_tab(&self, opposite: &Pane) -> usize {
        self.tabs
            .get(self.active)
            .filter(|tab| tab.preview)
            .and_then(|tab| tab.preview_source)
            .and_then(|source| opposite.tabs.iter().position(|tab| tab.id() == source))
            .unwrap_or(opposite.active)
    }
    fn remove_previews(&mut self) {
        let preceding = self
            .tabs
            .iter()
            .take(self.active)
            .filter(|tab| !tab.preview)
            .count();
        let closing_active = self.tabs.get(self.active).is_some_and(|tab| tab.preview);
        self.tabs.retain(|tab| !tab.preview);
        self.active = preceding
            .saturating_sub(usize::from(closing_active))
            .min(self.tabs.len().saturating_sub(1));
    }
}
#[derive(Clone)]
pub(crate) enum PromptKind {
    IconTheme,
    ColorTheme,
    ArchiveCache,
    Location,
    Mkdir(Location),
    Rename(PathBuf),
    NewFile(Location),
    Transfer {
        kind: OperationKind,
        sources: Vec<Location>,
        archive_destination: Option<Location>,
    },
    Trash(Vec<PathBuf>),
    DeleteArchive(Vec<Location>),
    CloseTab {
        pane: usize,
        id: u64,
    },
    Quit,
}
pub(crate) enum Modal {
    Prompt {
        kind: PromptKind,
        title: String,
        description: String,
    },
    Palette,
    CreateArchive(ArchiveDialog),
    ArchivePassword {
        request: PasswordRequest,
        input: Entity<InputState>,
    },
    QuickPick {
        request: vscode::QuickPick,
        action: QuickPickAction,
    },
    PanelMenu(PanelMenu),
    Settings,
    Extensions,
    OpenVsx,
    ExtensionLogs,
    Search,
    Conflict {
        source: PathBuf,
        destination: PathBuf,
        reply: mpsc::Sender<ConflictResolution>,
    },
}

pub(crate) struct ArchiveDialog {
    sources: Vec<Location>,
    pub format: &'static dyn ArchiveFormat,
    pub options: Vec<ArchiveOption>,
}

pub(crate) enum ArchiveOption {
    Choice(&'static str),
    Text(Entity<InputState>),
}

impl ArchiveDialog {
    pub fn selection_count(&self) -> usize {
        self.sources.len()
    }

    fn output_suffix(&self) -> &'static str {
        let creation = self.format.creation().unwrap();
        creation
            .fields
            .iter()
            .zip(&self.options)
            .find_map(|(field, option)| {
                let (OptionKind::Choice(choices), ArchiveOption::Choice(value)) =
                    (&field.kind, option)
                else {
                    return None;
                };
                choices.iter().find(|choice| choice.value == *value)?.suffix
            })
            .unwrap_or(creation.suffix)
    }

    fn clear_secrets(&self, window: &mut Window, cx: &mut Context<DotDir>) {
        for (field, option) in self
            .format
            .creation()
            .unwrap()
            .fields
            .iter()
            .zip(&self.options)
        {
            if let (OptionKind::Secret, ArchiveOption::Text(input)) = (&field.kind, option) {
                input.update(cx, |input, cx| input.set_value("", window, cx));
            }
        }
    }

    fn fields(
        format: &'static dyn ArchiveFormat,
        window: &mut Window,
        cx: &mut Context<DotDir>,
    ) -> Vec<ArchiveOption> {
        format
            .creation()
            .unwrap()
            .fields
            .iter()
            .map(|field| match field.kind {
                OptionKind::Choice(_) => ArchiveOption::Choice(field.default),
                OptionKind::Text | OptionKind::Secret => ArchiveOption::Text(cx.new(|cx| {
                    InputState::new(window, cx)
                        .masked(matches!(field.kind, OptionKind::Secret))
                        .default_value(field.default)
                })),
            })
            .collect()
    }
}

fn strip_archive_suffix<'a>(name: &'a str, suffix: &str) -> Option<&'a str> {
    let dot = name.len().checked_sub(suffix.len() + 1)?;
    (name.as_bytes()[dot] == b'.'
        && name.as_bytes()[dot + 1..].eq_ignore_ascii_case(suffix.as_bytes()))
    .then(|| &name[..dot])
}
pub(crate) enum QuickPickAction {
    Extension {
        session: u64,
        reply: mpsc::Sender<usize>,
    },
    ColorTheme(Vec<ThemeSource>),
}
pub(crate) struct SearchState {
    pub id: u64,
    pub token: CancelToken,
    pub results: Vec<PathBuf>,
    pub done: bool,
    pub count: u32,
    pub error: Option<String>,
    pub cursor: usize,
}
pub(crate) struct DotDir {
    pub focus: FocusHandle,
    pub terminal_focus: FocusHandle,
    pub modal_focus: FocusHandle,
    pub panes: [Pane; 2],
    pub tab_strip_scrolls: [TabStripScroll; 2],
    pub panel_scrolls: HashMap<u64, ColumnScroll>,
    pub folder_sizes: FolderSizes,
    pub active: usize,
    pub documents: HashMap<u64, Document>,
    pub settings: Settings,
    pub settings_path: PathBuf,
    pub palette: Palette,
    pub color_theme_name: Option<String>,
    color_theme_request: u64,
    pub icon_theme: Option<IconTheme>,
    pub icon_theme_request: u64,
    pub command_line: Entity<InputState>,
    pub dialog_input: Entity<InputState>,
    pub jump_input: Entity<InputState>,
    pub search_content: Entity<InputState>,
    pub modal: Option<Modal>,
    pub modal_error: Option<String>,
    pub palette_cursor: usize,
    pub mounted_roots: Vec<PathBuf>,
    pub panel_menu_scroll: ScrollHandle,
    pub operation: Option<Operation>,
    pub copy_options: CopyOptions,
    pub copy_multiple: bool,
    pub copy_use_filter: bool,
    pub copy_filter: Entity<InputState>,
    pub transfer_dropdown_open: Option<&'static str>,
    transfer_prompt_id: u64,
    pub terminals: Vec<Terminal>,
    pub terminal_active: usize,
    pub terminal_visible: bool,
    pub panels_visible: bool,
    pub extensions: Vec<LoadedExtension>,
    extension_host: Option<ExtensionHost>,
    pub vscode_extensions: Vec<vscode::Extension>,
    vscode_host: vscode::Host,
    vscode_language_command: Option<(u64, u64)>,
    language_servers: LanguageServers,
    pub hover: HoverHints,
    pub(crate) keystrokes: PresentationKeystrokes,
    pub(crate) quick_actions: QuickActions,
    source_jump: [Option<(u64, lsp_types::Position)>; 2],
    pub extension_errors: Vec<String>,
    pub extension_logs: ExtensionLogs,
    filesystems: Filesystems,
    password_requests: VecDeque<PasswordRequest>,
    pub extension_log_scroll: ScrollHandle,
    pub(crate) market: open_vsx::Market,
    pub(crate) market_search_task: Option<Task<()>>,
    pub search: Option<SearchState>,
    pub status: String,
    pub tx: mpsc::Sender<Message>,
    rx: mpsc::Receiver<Message>,
    pub watcher: Option<FsWatcher>,
    pub pending_watch: HashMap<u64, Instant>,
    pub next_id: u64,
    pub open_request: [u64; 2],
    pub extension_request: u64,
    pub last_typeahead: Instant,
    pub typeahead: String,
    shift_selection: Option<ShiftSelection>,
    pub _subscriptions: Vec<Subscription>,
    pub shutting_down: bool,
    pub quit_pending: bool,
}
impl DotDir {
    pub fn new(
        settings_path: PathBuf,
        initial: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut settings = Settings::load(&settings_path);
        if let Some(path) = initial {
            if let Some(path) = path
                .to_str()
                .and_then(|path| ProviderPath::parse(path).ok().flatten())
            {
                settings.left = vec![SavedLocation::Provider { uri: path }];
                settings.active_pane = 0;
                settings.left_active = 0;
            } else if path.is_dir() {
                settings.left = vec![SavedLocation::Local(path.clone())];
                settings.right = vec![SavedLocation::Local(path)];
                settings.left_active = 0;
                settings.right_active = 0;
            } else if path.is_file()
                && let Some(format) = ARCHIVE_FORMATS.for_path(&path)
            {
                let path = path.canonicalize().unwrap_or(path);
                if let Some(parent) = path.parent() {
                    settings.right = vec![SavedLocation::Local(parent.into())];
                    settings.right_active = 0;
                }
                if let Ok(root) = Archive::mount_location(path.into(), format) {
                    settings.left = vec![SavedLocation::from(&root)];
                    settings.left_active = 0;
                }
            }
        }
        let palette = Palette::named(&settings.theme);
        palette.apply(cx);
        let (tx, rx) = mpsc::channel();
        let password_tx = tx.clone();
        let passwords =
            ArchivePasswords::new(settings.archive_password_cache_minutes, move |request| {
                let _ = password_tx.send(Message::ArchivePassword(request));
            });
        let lsp_tx = tx.clone();
        let extension_logs =
            ExtensionLogs::with_error_file(&settings_path.with_file_name(ERROR_LOG_NAME));
        let language_servers = LanguageServers::new(
            settings_path
                .parent()
                .unwrap_or(Path::new("."))
                .to_path_buf(),
            extension_logs.clone(),
            move |notice| {
                let _ = lsp_tx.send(Message::Lsp(notice));
            },
        );
        let watch_tx = tx.clone();
        let watcher = FsWatcher::new(Arc::new(move |id, _, _| {
            if let Ok(id) = id.parse() {
                let _ = watch_tx.send(Message::Watch(id));
            }
        }))
        .ok();
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let command_line = cx.new(|cx| InputState::new(window, cx).placeholder(""));
        let dialog_input = cx.new(|cx| InputState::new(window, cx));
        let copy_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("e.g. *.rs or src/**/*.rs"));
        let jump_input = cx.new(|cx| InputState::new(window, cx).placeholder("Type filename…"));
        let search_content =
            cx.new(|cx| InputState::new(window, cx).placeholder("Containing text (optional)"));
        let subscriptions = vec![
            Self::observe_presentation_keystrokes(window, cx),
            cx.subscribe_in(&command_line, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.execute_shell(window, cx);
                }
            }),
            cx.subscribe_in(
                &dialog_input,
                window,
                |this, _, event, window, cx| match event {
                    InputEvent::PressEnter { .. } => {}
                    InputEvent::Change => {
                        this.palette_cursor = 0;
                        this.modal_error = None;
                        if matches!(this.modal, Some(Modal::OpenVsx)) {
                            this.schedule_market_search(cx);
                        }
                        cx.notify();
                        window.refresh();
                    }
                    _ => {}
                },
            ),
            cx.subscribe_in(&jump_input, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.select_jump_match(JumpStep::First, window, cx);
                }
            }),
            cx.subscribe_in(
                &search_content,
                window,
                |_, _, _: &InputEvent, window, cx| {
                    cx.notify();
                    window.refresh();
                },
            ),
            cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() {
                    this.end_shift_selection();
                    if this.keystrokes.clear() {
                        cx.notify();
                    }
                }
            }),
            cx.on_release(|this, _| {
                this.persist();
                if let Some(s) = &this.search {
                    s.token.cancel();
                }
            }),
        ];
        let mut next_id = 1;
        let panes = [
            Pane::restore(&settings.left, settings.left_active, &mut next_id),
            Pane::restore(&settings.right, settings.right_active, &mut next_id),
        ];
        let cache = extension_cache_directory();
        let vscode_tx = tx.clone();
        let vscode_host =
            vscode::Host::new(cache.join("node"), extension_logs.clone(), move |event| {
                let _ = vscode_tx.send(Message::VsCode(event));
            });
        let weak = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            weak.update(cx, |this, cx| {
                if this.shutting_down {
                    return true;
                }
                this.request_quit(window, cx);
                false
            })
            .unwrap_or(true)
        });
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(33))
                    .await;
                if this
                    .update_in(cx, |this, window, cx| this.tick(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let mut this = Self {
            focus,
            terminal_focus: cx.focus_handle(),
            modal_focus: cx.focus_handle(),
            panes,
            tab_strip_scrolls: Default::default(),
            panel_scrolls: HashMap::new(),
            active: settings.active_pane,
            documents: HashMap::new(),
            palette,
            color_theme_name: None,
            color_theme_request: 0,
            icon_theme: None,
            icon_theme_request: 0,
            command_line,
            dialog_input,
            jump_input,
            search_content,
            modal: None,
            modal_error: None,
            palette_cursor: 0,
            mounted_roots: vec![],
            panel_menu_scroll: ScrollHandle::new(),
            operation: None,
            copy_options: CopyOptions::default(),
            copy_multiple: false,
            copy_use_filter: false,
            copy_filter,
            transfer_dropdown_open: None,
            transfer_prompt_id: 0,
            terminals: vec![],
            terminal_active: 0,
            terminal_visible: false,
            panels_visible: true,
            extensions: vec![],
            extension_host: None,
            vscode_extensions: vec![],
            vscode_host,
            vscode_language_command: None,
            language_servers,
            hover: HoverHints::default(),
            keystrokes: PresentationKeystrokes::default(),
            quick_actions: QuickActions::default(),
            source_jump: [None, None],
            extension_errors: vec![],
            filesystems: {
                let mut filesystems = Filesystems::new(extension_logs.clone());
                filesystems.archive_passwords = Arc::new(passwords);
                filesystems
            },
            password_requests: VecDeque::new(),
            folder_sizes: FolderSizes::default(),
            extension_logs,
            extension_log_scroll: ScrollHandle::new(),
            market: open_vsx::Market::default(),
            market_search_task: None,
            search: None,
            status: String::new(),
            tx,
            rx,
            watcher,
            pending_watch: HashMap::new(),
            next_id,
            open_request: [0; 2],
            extension_request: 0,
            last_typeahead: Instant::now(),
            typeahead: String::new(),
            shift_selection: None,
            settings_path,
            settings,
            _subscriptions: subscriptions,
            shutting_down: false,
            quit_pending: false,
        };
        this.load_extensions();
        let ids = this
            .panes
            .iter()
            .flat_map(|p| p.tabs.iter().map(Tab::id))
            .collect::<Vec<_>>();
        let active_directory = this.directory(this.active).map(|dir| dir.id);
        for id in ids {
            this.reload_directory(id, Some(id) == active_directory);
        }
        if let Some(path) = this.settings.icon_theme.clone() {
            this.load_icon_theme(path, cx);
        }
        if let Some(source) = this.settings.color_theme.clone() {
            this.load_color_theme(source, cx);
        }
        this.focus_pane(this.active, window, cx);
        this
    }
    pub fn load_icon_theme(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.icon_theme_request += 1;
        let request = self.icon_theme_request;
        self.status = "Loading file icon theme…".into();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Message::IconTheme(
                request,
                IconTheme::load(&path).map(Box::new),
            ));
        });
        cx.notify();
    }
    pub fn prompt_icon_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self
            .settings
            .icon_theme
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.prompt(
            PromptKind::IconTheme,
            "Load file icon theme",
            "Enter the extension directory containing package.json and its icon assets.".into(),
            value,
            window,
            cx,
        );
    }
    pub fn clear_icon_theme(&mut self, cx: &mut Context<Self>) {
        self.icon_theme_request += 1;
        self.icon_theme = None;
        self.settings.icon_theme = None;
        self.status = "Using built-in file icons".into();
        self.persist();
        cx.notify();
    }

    pub fn prompt_color_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self
            .settings
            .color_theme
            .as_ref()
            .map(|source| source.path.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.prompt(
            PromptKind::ColorTheme,
            "Load VS Code Color Theme",
            "Enter a theme file or an unpacked VS Code extension directory.".into(),
            value,
            window,
            cx,
        );
    }

    pub(crate) fn choose_color_theme(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let mut sources = match &self.settings.color_theme {
            Some(source) if source.path == path => vec![source.clone()],
            _ => ThemeSource::discover(&path, self.settings.vscode.locale.as_deref())?,
        };
        if sources.len() == 1 {
            self.load_color_theme(sources.pop().unwrap(), cx);
        } else {
            let items = sources
                .iter()
                .map(|source| vscode::QuickPickItem {
                    label: source.label.clone().unwrap_or_else(|| {
                        source
                            .path
                            .file_stem()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into()
                    }),
                    description: None,
                    detail: None,
                    separator: false,
                })
                .collect();
            self.show_modal(
                Modal::QuickPick {
                    request: vscode::QuickPick {
                        title: Some("VS Code Color Themes".into()),
                        place_holder: None,
                        items,
                        match_on_description: false,
                        match_on_detail: false,
                    },
                    action: QuickPickAction::ColorTheme(sources),
                },
                window,
                cx,
            );
        }
        Ok(())
    }

    fn load_color_theme(&mut self, source: ThemeSource, cx: &mut Context<Self>) {
        self.color_theme_request += 1;
        let request = self.color_theme_request;
        self.status = "Loading color theme…".into();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Message::ColorTheme(
                request,
                ImportedTheme::load(source).map(Box::new),
            ));
        });
        cx.notify();
    }

    pub fn use_builtin_theme(&mut self, id: &str, cx: &mut Context<Self>) {
        self.color_theme_request += 1;
        self.color_theme_name = None;
        self.settings.color_theme = None;
        self.settings.theme = id.into();
        self.palette = Palette::named(id);
        self.palette.apply(cx);
        self.refresh_diagnostic_decorations(cx);
        self.dismiss_hover(cx);
        self.persist();
        cx.notify();
    }

    fn refresh_diagnostic_decorations(&mut self, cx: &mut App) {
        for doc in self.documents.values_mut() {
            doc.refresh_diagnostic_decorations(cx);
        }
    }

    pub fn id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
    pub fn directory(&self, pane: usize) -> Option<&Directory> {
        match self.panes[pane]
            .tabs
            .get(self.panes[pane].active)
            .map(|tab| &tab.content)
        {
            Some(TabContent::Directory(d)) => Some(d.current()),
            _ => None,
        }
    }
    pub fn directory_mut(&mut self, pane: usize) -> Option<&mut Directory> {
        let active = self.panes[pane].active;
        match self.panes[pane]
            .tabs
            .get_mut(active)
            .map(|tab| &mut tab.content)
        {
            Some(TabContent::Directory(d)) => Some(d.current_mut()),
            _ => None,
        }
    }
    pub fn directory_by_id(&mut self, id: u64) -> Option<&mut Directory> {
        self.panes
            .iter_mut()
            .flat_map(|p| &mut p.tabs)
            .find_map(|t| match &mut t.content {
                TabContent::Directory(d) => d.columns.iter_mut().find(|d| d.id == id),
                _ => None,
            })
    }
    pub(crate) fn pane_location(&self, pane: usize) -> Location {
        match self.panes[pane]
            .tabs
            .get(self.panes[pane].active)
            .map(|tab| &tab.content)
        {
            Some(TabContent::Directory(d)) => d.current().path.clone(),
            Some(TabContent::Document(id)) => self
                .documents
                .get(id)
                .and_then(|d| d.path.parent())
                .unwrap_or_else(|| PathBuf::new().into()),
            Some(
                TabContent::Image { path, .. }
                | TabContent::Hex { path, .. }
                | TabContent::Media { path, .. },
            ) => path.parent().unwrap_or_else(|| PathBuf::new().into()),
            _ => self.panes[pane]
                .tabs
                .iter()
                .find_map(|t| match &t.content {
                    TabContent::Directory(d) => Some(d.current().path.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().into()),
        }
    }
    fn opposite_archive_destination(&self) -> Option<&Location> {
        self.directory(1 - self.active)
            .map(|dir| &dir.path)
            .filter(|path| matches!(path, Location::Archive { .. }) && path.editable())
    }
    pub fn cwd(&self, pane: usize) -> PathBuf {
        let location = self.pane_location(pane);
        if let Some(local) = location.local() {
            local.into()
        } else {
            location
                .host_path()
                .and_then(Path::parent)
                .map(PathBuf::from)
                .unwrap_or_else(|| dirs::home_dir().unwrap_or_default())
        }
    }
    pub fn active_document(&self) -> Option<u64> {
        match self.panes[self.active]
            .tabs
            .get(self.panes[self.active].active)
            .map(|tab| &tab.content)
        {
            Some(TabContent::Document(id)) => Some(*id),
            _ => None,
        }
    }

    pub(crate) fn active_hex(&self) -> Option<(&Location, &Entity<HexView>)> {
        match &self.panes[self.active]
            .tabs
            .get(self.panes[self.active].active)?
            .content
        {
            TabContent::Hex { path, view, .. } => Some((path, view)),
            _ => None,
        }
    }

    pub(crate) fn file_changes<'a>(
        &'a self,
        content: &'a TabContent,
        cx: &App,
    ) -> Option<(&'a Location, bool, bool)> {
        match content {
            TabContent::Document(id) => self
                .documents
                .get(id)
                .map(|doc| (&doc.path, doc.saving, doc.dirty())),
            TabContent::Hex { path, view, .. } => {
                let buffer = view.read(cx).buffer.read(cx);
                Some((path, buffer.saving, buffer.dirty()))
            }
            _ => None,
        }
    }
    pub fn reload(&mut self, id: u64) {
        self.reload_directory(id, true);
    }

    fn reload_directory(&mut self, id: u64, interactive: bool) {
        let hidden = self.settings.show_hidden;
        let filesystems = if interactive {
            self.filesystems.archive_operation()
        } else {
            self.filesystems
                .without_password_prompts()
                .archive_operation()
        };
        if let Some(d) = self.directory_by_id(id) {
            d.generation += 1;
            d.loading = true;
            let generation = d.generation;
            let path = d.path.clone();
            let sort = d.sort;
            self.folder_sizes.invalidate_unwatched(|folder| {
                folder == &path || folder.parent().as_ref() == Some(&path)
            });
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(Message::Directory(
                    id,
                    generation,
                    read_location(&path, &filesystems, hidden, sort),
                ));
            });
        }
    }
    pub fn refresh_all(&mut self) {
        self.folder_sizes.invalidate_unwatched(|_| true);
        self.refresh_directories(|_| true);
    }

    pub fn refresh_provider_directories(&mut self) {
        self.folder_sizes
            .invalidate_unwatched(|path| matches!(path, Location::Provider(_)));
        self.refresh_directories(|path| matches!(path, Location::Provider(_)));
    }

    fn refresh_directories(&mut self, matches: impl Fn(&Location) -> bool) {
        let ids = self
            .panes
            .iter()
            .flat_map(|p| &p.tabs)
            .filter_map(|tab| {
                if let TabContent::Directory(browser) = &tab.content {
                    Some(&browser.columns)
                } else {
                    None
                }
            })
            .flatten()
            .filter(|d| matches(&d.path))
            .map(|d| d.id)
            .collect::<Vec<_>>();
        for id in ids {
            self.reload_directory(id, false);
        }
    }
    pub fn navigate(
        &mut self,
        pane: usize,
        path: Location,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_request[pane] += 1;
        self.active = pane;
        let columns = self.settings.presentation(pane) == Presentation::Columns;
        let next_id = self.id();
        let old_ids = self
            .browser(pane)
            .map(|b| b.columns.iter().map(|d| d.id).collect::<Vec<_>>())
            .unwrap_or_default();
        let mut reload = None;
        if let Some(browser) = self.browser_mut(pane) {
            if columns && let Some(index) = browser.columns.iter().position(|d| d.path == path) {
                browser.active = index;
            } else if columns
                && path.parent().as_ref() == Some(&browser.current().path)
                && let Some(index) = browser
                    .current()
                    .entries
                    .iter()
                    .position(|entry| entry.directory && !entry.parent && entry.path == path)
            {
                browser.current_mut().cursor = index;
                reload = browser.preview(next_id);
                if browser
                    .columns
                    .get(browser.active + 1)
                    .is_some_and(|d| d.path == path)
                {
                    browser.active += 1;
                }
            } else {
                browser.collapse();
                browser.current_mut().navigate(path);
                reload = Some(browser.current().id);
            }
        } else {
            self.panes[pane]
                .tabs
                .push(Tab::new(TabContent::Directory(DirectoryBrowser::new(
                    next_id, path,
                ))));
            self.panes[pane].active = self.panes[pane].tabs.len() - 1;
            reload = Some(next_id);
        }
        self.unwatch_removed_columns(old_ids);
        if let Some(id) = reload {
            self.reload(id);
        }
        self.reveal_column(pane, false);
        self.focus_pane(pane, window, cx);
        self.persist();
        cx.notify();
    }
    pub fn focus_pane(&mut self, pane: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.end_shift_selection();
        self.active = pane;
        self.focus_active_content(window, cx);
        // A newly mounted pane/editor is not in the dispatch tree until the
        // next frame. Restore focus after that tree replaces a closed dialog.
        let view = cx.weak_entity();
        window.on_next_frame(move |window, cx| {
            let _ = view.update(cx, |this, cx| {
                if this.active != pane || this.modal.is_some() {
                    return;
                }
                this.focus_active_content(window, cx);
            });
        });
        cx.notify();
    }

    fn focus_active_content(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pane = &mut self.panes[self.active];
        let Some(tab) = pane.tabs.get_mut(pane.active) else {
            self.focus.focus(window, cx);
            return;
        };
        match tab.content {
            TabContent::Document(id) => {
                if let Some(position) = tab.source_position.take() {
                    self.reveal_source_position(id, position, window, cx);
                } else if let Some(d) = self.documents.get(&id) {
                    d.input.update(cx, |s, cx| s.focus(window, cx));
                }
            }
            TabContent::Media { ref view, .. } => {
                window.focus(&view.read(cx).focus_handle(cx), cx);
            }
            TabContent::Hex { ref view, .. } => {
                let focus = view.read(cx).focus_handle().clone();
                if !focus.contains_focused(window, cx) {
                    focus.focus(window, cx);
                }
            }
            _ => self.focus.focus(window, cx),
        }
    }

    pub fn focus_pane_from_click(
        &mut self,
        pane: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left || self.active == pane || self.modal.is_some() {
            return;
        }
        let mirror = self.active_document().and_then(|id| {
            if !matches!(self.panes[pane].tabs[self.panes[pane].active].content,
                TabContent::Document(document) if document == id)
            {
                return None;
            }
            self.documents[&id]
                .inactive_editor()
                .filter(|mirror| mirror.read(cx).input_bounds().contains(&event.position))
                .map(|mirror| (id, mirror.clone()))
        });
        self.focus_pane(pane, window, cx);
        if let Some((id, mirror)) = mirror {
            let app = cx.weak_entity();
            // Let the editor under the pointer place its caret before the
            // editable view takes over this pane.
            window.defer(cx, move |window, cx| {
                let _ = app.update(cx, |this, cx| {
                    if this.active == pane && this.active_document() == Some(id) {
                        let position = mirror.read(cx).cursor_position();
                        this.documents[&id].input.update(cx, |editor, cx| {
                            editor.set_cursor_position(position, window, cx);
                        });
                    }
                });
            });
        }
    }
    fn focus_modal(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.modal_focus.focus(window, cx);
        let view = cx.weak_entity();
        window.on_next_frame(move |window, cx| {
            let _ = view.update(cx, |this, cx| {
                if matches!(&this.modal, Some(Modal::PanelMenu(menu)) if menu.is_jump()) {
                    this.jump_input.update(cx, |s, cx| s.focus(window, cx));
                    return;
                }
                if let Some(Modal::ArchivePassword { input, .. }) = &this.modal {
                    input.update(cx, |input, cx| input.focus(window, cx));
                    return;
                }
                let input = match &this.modal {
                    Some(
                        Modal::Palette | Modal::Search | Modal::OpenVsx | Modal::CreateArchive(_),
                    ) => true,
                    Some(Modal::Prompt { kind, .. }) => !matches!(
                        kind,
                        PromptKind::Trash(_)
                            | PromptKind::DeleteArchive(_)
                            | PromptKind::CloseTab { .. }
                            | PromptKind::Quit
                    ),
                    None => return,
                    _ => false,
                };
                if input {
                    this.dialog_input.update(cx, |s, cx| s.focus(window, cx));
                } else {
                    this.modal_focus.focus(window, cx);
                }
            });
        });
    }
    pub fn set_tab(
        &mut self,
        pane: usize,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_request[pane] += 1;
        self.panes[pane].active = index.min(self.panes[pane].tabs.len() - 1);
        if let Some(directory) = self.directory(pane)
            && directory.error.as_deref() == Some(PASSWORD_REQUIRED)
        {
            self.reload(directory.id);
        }
        self.focus_pane(pane, window, cx);
        self.persist();
    }

    fn tab_position(&self, id: u64) -> Option<(usize, usize)> {
        self.panes.iter().enumerate().find_map(|(pane, tabs)| {
            tabs.tabs
                .iter()
                .position(|tab| tab.id() == id)
                .map(|index| (pane, index))
        })
    }

    pub fn drop_tab(
        &mut self,
        id: u64,
        destination: usize,
        slot: usize,
        duplicate: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal.is_some() {
            return;
        }
        let Some((source, index)) = self.tab_position(id) else {
            return;
        };
        let slot = slot.min(self.panes[destination].tabs.len());
        if source == destination {
            let index = self.panes[source].reorder_tab(index, slot);
            self.set_tab(source, index, window, cx);
            return;
        }

        if duplicate {
            self.insert_duplicated_tab(source, index, destination, slot, window, cx);
            return;
        }
        let fallback = self.pane_location(source);
        let mut tab = self.panes[source].remove_tab(index);
        tab.make_permanent();
        self.open_request[source] += 1;
        self.ensure_directory_tab(source, fallback);
        // A preview in the destination can no longer follow a source that
        // now lives beside it in the same pane.
        for preview in &mut self.panes[destination].tabs {
            if preview.preview_source == Some(id) {
                preview.make_permanent();
            }
        }
        self.panes[destination].tabs.insert(slot, tab);
        self.set_tab(destination, slot, window, cx);
        self.reveal_column(destination, false);
    }

    pub(crate) fn duplicate_panel_tab(
        &mut self,
        pane: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel = &self.panes[pane];
        self.insert_duplicated_tab(pane, panel.active, pane, panel.tabs.len(), window, cx);
    }

    fn insert_duplicated_tab(
        &mut self,
        source: usize,
        index: usize,
        destination: usize,
        slot: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (tab, load) = self.duplicate_tab(source, index, window, cx);
        let tab_id = tab.id();
        let directories = match &tab.content {
            TabContent::Directory(browser) => {
                browser.columns.iter().map(|column| column.id).collect()
            }
            _ => Vec::new(),
        };
        self.panes[destination].tabs.insert(slot, tab);
        self.set_tab(destination, slot, window, cx);
        for id in directories {
            self.reload(id);
        }
        self.reveal_column(destination, false);
        if let Some(path) = load {
            self.open_document_into(destination, path, true, Some(tab_id), window, cx);
        }
    }

    fn open_directory_tab(
        &mut self,
        pane: usize,
        path: Location,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.id();
        let panel = &mut self.panes[pane];
        panel
            .tabs
            .push(Tab::new(TabContent::Directory(DirectoryBrowser::new(
                id, path,
            ))));
        let index = panel.tabs.len() - 1;
        self.set_tab(pane, index, window, cx);
        self.reload(id);
    }

    pub(crate) fn open_panel_location(
        &mut self,
        pane: usize,
        path: Location,
        new_tab: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if new_tab {
            self.open_directory_tab(pane, path, window, cx);
            return;
        }
        let index = self.panes[pane].active;
        let tab = &self.panes[pane].tabs[index];
        match tab.content {
            TabContent::Directory(_) => self.navigate(pane, path, window, cx),
            _ if self
                .file_changes(&tab.content, cx)
                .is_some_and(|(_, saving, dirty)| saving || dirty) =>
            {
                self.open_directory_tab(pane, path, window, cx);
            }
            _ => {
                let tab_id = tab.id();
                let id = self.id();
                let browser = DirectoryBrowser::new(id, path);
                self.panes[pane].tabs[index] = Tab::with_id(tab_id, TabContent::Directory(browser));
                self.release_unused_documents();
                self.set_tab(pane, index, window, cx);
                self.reload(id);
            }
        }
    }

    fn duplicate_tab(
        &mut self,
        pane: usize,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Tab, Option<Location>) {
        let id = self.id();
        let (content, load) = match &self.panes[pane].tabs[index].content {
            TabContent::Directory(browser) => {
                let mut browser = browser.clone();
                browser.id = id;
                for (index, column) in browser.columns.iter_mut().enumerate() {
                    column.id = if index == 0 { id } else { self.id() };
                }
                (TabContent::Directory(browser), None)
            }
            TabContent::Document(document) => (TabContent::Document(*document), None),
            TabContent::Hex { path, view, .. } => {
                let view = view.read(cx);
                (
                    self.hex_content(
                        id,
                        path.clone(),
                        view.buffer.clone(),
                        view.readonly,
                        view.mode(),
                        window,
                        cx,
                    ),
                    None,
                )
            }
            TabContent::SelectionInspector { snapshot, .. } => (
                TabContent::SelectionInspector {
                    id,
                    snapshot: snapshot.clone(),
                },
                None,
            ),
            TabContent::Image { path, image, .. } => (
                TabContent::Image {
                    id,
                    path: path.clone(),
                    image: image.clone(),
                },
                None,
            ),
            TabContent::Panel {
                title,
                extension,
                command,
                node,
                ..
            } => (
                TabContent::Panel {
                    id,
                    title: title.clone(),
                    extension: extension.clone(),
                    command: command.clone(),
                    node: node.clone(),
                },
                None,
            ),
            TabContent::Pending { path, error, .. } => (
                TabContent::Pending {
                    id,
                    path: path.clone(),
                    error: error.clone(),
                },
                path.clone(),
            ),
            TabContent::Media { path, .. } => {
                let path = Some(path.clone());
                (
                    TabContent::Pending {
                        id,
                        path: path.clone(),
                        error: None,
                    },
                    path,
                )
            }
        };
        (Tab::with_id(id, content), load)
    }

    /// A GPUI editor's layout and focus handle belong to one mounted surface.
    /// When a buffer is visible in both panes, the focused pane owns the editor
    /// and the other pane paints a separate view of the same text.
    pub fn editor_for_pane(
        &mut self,
        pane: usize,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<EditorState>, bool) {
        let shared = self.panes.iter().all(|pane| {
            matches!(pane.tabs[pane.active].content, TabContent::Document(document) if document == id)
        });
        let doc = self.documents.get_mut(&id).expect("document tab");
        if !shared {
            doc.inactive_view = None;
        }
        if !shared || self.active == pane {
            return (doc.input.clone(), doc.readonly);
        }
        if doc.inactive_view.is_none() {
            let text = doc.input.read(cx).value();
            let language = language_for(doc.path.name_path());
            let input = cx.new(|cx| {
                EditorState::new(window, cx)
                    .language(language)
                    .soft_wrap(doc.line_wrap.unwrap_or(true))
                    .default_value(text)
            });
            let mut diagnostics = DiagnosticDecorations::default();
            diagnostics.set(&input, doc.diagnostic_styles(cx), cx);
            doc.inactive_view = Some(InactiveEditor { input, diagnostics });
        }
        let view = doc.inactive_view.as_ref().unwrap();
        (view.input.clone(), true)
    }
    pub fn layout_for(&self, pane: usize, window: &Window) -> PanelLayout {
        let height: f32 = window.viewport_size().height.into();
        let width: f32 = window.viewport_size().width.into();
        PanelLayout::new(
            self.settings.presentation(pane),
            width / 2. - 10. - crate::ui::PANEL_MARGIN,
            height
                - 218.
                - if self.operation.is_some() { 30. } else { 0. }
                - if self.terminal_visible {
                    self.settings.terminal_height
                } else {
                    0.
                },
        )
    }
    pub fn layout(&self, window: &Window) -> (usize, usize) {
        let layout = self.layout_for(self.active, window);
        (layout.rows, layout.columns)
    }
    pub fn browser(&self, pane: usize) -> Option<&DirectoryBrowser> {
        match self.panes[pane]
            .tabs
            .get(self.panes[pane].active)
            .map(|tab| &tab.content)
        {
            Some(TabContent::Directory(browser)) => Some(browser),
            _ => None,
        }
    }
    pub fn browser_mut(&mut self, pane: usize) -> Option<&mut DirectoryBrowser> {
        let index = self.panes[pane].active;
        match self.panes[pane]
            .tabs
            .get_mut(index)
            .map(|tab| &mut tab.content)
        {
            Some(TabContent::Directory(browser)) => Some(browser),
            _ => None,
        }
    }
    fn unwatch_removed_columns(&mut self, ids: Vec<u64>) {
        for id in ids {
            if self.directory_by_id(id).is_none()
                && let Some(w) = &self.watcher
            {
                w.remove(&id.to_string());
            }
        }
    }
    pub fn reveal_column(&mut self, pane: usize, preview: bool) {
        if self.settings.presentation(pane) != Presentation::Columns {
            return;
        }
        if let Some(browser) = self.browser(pane) {
            let id = browser.id;
            let index = (browser.active + usize::from(preview)).min(browser.columns.len() - 1);
            self.panel_scrolls.entry(id).or_default().reveal(index);
        }
    }
    pub fn update_column_preview(&mut self, pane: usize) {
        if self.settings.presentation(pane) != Presentation::Columns {
            return;
        }
        let next_id = self.id();
        let Some(browser) = self.browser_mut(pane) else {
            return;
        };
        let old_ids = browser.columns.iter().map(|d| d.id).collect();
        let reload = browser.preview(next_id);
        self.unwatch_removed_columns(old_ids);
        if let Some(id) = reload {
            self.reload(id);
        }
    }
    pub fn set_presentation(
        &mut self,
        pane: usize,
        presentation: Presentation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut modes = [self.settings.presentation(0), self.settings.presentation(1)];
        if modes[pane] != presentation
            && let Some(directory) = self.directory_mut(pane)
        {
            directory.reset_icon_scroll();
        }
        modes[pane] = presentation;
        self.settings.presentations = Some(modes);
        let old_ids = self
            .browser(pane)
            .map(|b| b.columns.iter().map(|d| d.id).collect())
            .unwrap_or_default();
        if presentation != Presentation::Columns
            && let Some(browser) = self.browser_mut(pane)
        {
            browser.collapse();
            let id = browser.id;
            if let Some(scroll) = self.panel_scrolls.get_mut(&id) {
                scroll.cancel();
            }
        }
        self.unwatch_removed_columns(old_ids);
        if presentation == Presentation::Columns {
            self.update_column_preview(pane);
            self.reveal_column(pane, true);
        }
        if self.modal.is_none() {
            self.focus_pane(pane, window, cx);
        }
        self.persist();
        cx.notify();
        window.refresh();
    }
    pub fn select_panel_entry(
        &mut self,
        pane: usize,
        column: usize,
        index: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active = pane;
        let Some(browser) = self.browser_mut(pane) else {
            return;
        };
        if column >= browser.columns.len() {
            return;
        }
        browser.active = column;
        let directory = browser.current_mut();
        if index >= directory.entries.len() {
            return;
        }
        let previous = directory.cursor;
        directory.cursor = index;
        let marking = event.modifiers.platform || event.modifiers.control || event.modifiers.shift;
        if event.modifiers.shift {
            for entry in &directory.entries[previous.min(index)..=previous.max(index)] {
                if !entry.parent {
                    directory.marked.insert(entry.path.clone());
                }
            }
        } else if marking {
            directory.toggle_mark();
        }
        let entry = directory.focused().cloned();
        if !marking {
            self.update_column_preview(pane);
            self.reveal_column(pane, true);
        }
        self.focus_pane(pane, window, cx);
        if event.click_count == 2
            && !marking
            && let Some(entry) = entry
        {
            self.open_entry(pane, entry, false, window, cx);
        }
        self.persist();
        cx.notify();
    }
    pub fn prompt(
        &mut self,
        kind: PromptKind,
        title: &str,
        description: String,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(&kind, PromptKind::Transfer { .. }) {
            self.transfer_prompt_id += 1;
            self.transfer_dropdown_open = None;
            self.copy_multiple = false;
            self.copy_use_filter = false;
            self.copy_options.conflict_policy = ConflictPolicy::Ask;
            self.copy_filter
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        let has_input = !matches!(
            &kind,
            PromptKind::Trash(_)
                | PromptKind::DeleteArchive(_)
                | PromptKind::CloseTab { .. }
                | PromptKind::Quit
        );
        self.modal_error = None;
        self.modal = Some(Modal::Prompt {
            kind,
            title: title.into(),
            description,
        });
        self.dialog_input.update(cx, |s, cx| {
            s.set_placeholder("", window, cx);
            s.set_value(value, window, cx);
            if has_input {
                s.focus(window, cx);
            }
        });
        if !has_input {
            self.focus_modal(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn transfer_supports_policy(
        &self,
        kind: OperationKind,
        sources: &[Location],
        archive_target: bool,
        policy: ConflictPolicy,
    ) -> bool {
        match policy {
            ConflictPolicy::Append => !matches!(kind, OperationKind::Move),
            ConflictPolicy::Resume => {
                matches!(kind, OperationKind::Copy)
                    && !archive_target
                    && sources.iter().all(|source| source.local().is_some())
            }
            _ => true,
        }
    }

    pub(crate) fn set_copy_multiple(
        &mut self,
        multiple: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = self.dialog_input.read(cx).value();
        let value = if multiple && (value.contains(';') || value.contains('"')) {
            quote_copy_destination(&value)
        } else if !multiple {
            parse_copy_destinations(&value)
                .ok()
                .and_then(|paths| paths.into_iter().next())
                .unwrap_or_else(|| value.to_string())
        } else {
            value.to_string()
        };
        self.copy_multiple = multiple;
        self.dialog_input
            .update(cx, |input, cx| input.set_value(value, window, cx));
        cx.notify();
    }

    pub(crate) fn choose_transfer_destination(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.transfer_prompt_id;
        let multiple = self.copy_multiple;
        let dialog = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple,
            prompt: Some("Choose Destination".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(result) = dialog.await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                if this.transfer_prompt_id != id
                    || !matches!(
                        this.modal,
                        Some(Modal::Prompt {
                            kind: PromptKind::Transfer { .. },
                            ..
                        })
                    )
                {
                    return;
                }
                match result {
                    Ok(Some(paths)) if !paths.is_empty() => {
                        let value = if multiple {
                            paths
                                .iter()
                                .map(|path| quote_copy_destination(&path.to_string_lossy()))
                                .collect::<Vec<_>>()
                                .join("; ")
                        } else {
                            paths[0].to_string_lossy().into_owned()
                        };
                        this.dialog_input.update(cx, |input, cx| {
                            input.set_value(value, window, cx);
                            input.focus(window, cx);
                        });
                    }
                    Ok(_) => {}
                    Err(error) => this.modal_error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn prompt_create_archive(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(directory) = self.directory(self.active) else {
            return;
        };
        let Some(format) = ARCHIVE_FORMATS.creators().next() else {
            return;
        };
        let sources: Vec<_> = directory
            .selected()
            .into_iter()
            .map(|entry| entry.path)
            .collect();
        if sources.is_empty() {
            return;
        }
        let name = if sources.len() == 1 {
            sources[0].label()
        } else {
            directory.path.label()
        };
        let name = if valid_name(&name) {
            name
        } else {
            "Archive".into()
        };
        let filename = format!("{name}.{}", format.creation().unwrap().suffix);
        let path = self.cwd(self.active).join(filename);
        self.show_modal(
            Modal::CreateArchive(ArchiveDialog {
                sources,
                format,
                options: ArchiveDialog::fields(format, window, cx),
            }),
            window,
            cx,
        );
        self.dialog_input.update(cx, |input, cx| {
            input.set_value(path.to_string_lossy().into_owned(), window, cx);
        });
    }

    pub(crate) fn select_archive_format(
        &mut self,
        format: &'static dyn ArchiveFormat,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::CreateArchive(dialog)) = &mut self.modal else {
            return;
        };
        if dialog.format.info().id == format.info().id {
            return;
        }
        let old_suffix = dialog.output_suffix();
        dialog.clear_secrets(window, cx);
        dialog.format = format;
        dialog.options = ArchiveDialog::fields(format, window, cx);
        let new_suffix = dialog.output_suffix();
        self.update_archive_suffix(old_suffix, new_suffix, window, cx);
        self.modal_error = None;
        cx.notify();
    }

    pub(crate) fn select_archive_option(
        &mut self,
        index: usize,
        value: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::CreateArchive(dialog)) = &mut self.modal else {
            return;
        };
        let old_suffix = dialog.output_suffix();
        let Some(option @ ArchiveOption::Choice(_)) = dialog.options.get_mut(index) else {
            return;
        };
        *option = ArchiveOption::Choice(value);
        let new_suffix = dialog.output_suffix();
        self.update_archive_suffix(old_suffix, new_suffix, window, cx);
        self.modal_error = None;
        cx.notify();
    }

    fn update_archive_suffix(
        &self,
        old: &str,
        new: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if old != new
            && let Some(stem) = strip_archive_suffix(&self.dialog_input.read(cx).value(), old)
        {
            let path = format!("{stem}.{new}");
            self.dialog_input.update(cx, |input, cx| {
                input.set_value(path, window, cx);
            });
        }
    }

    fn accept_create_archive(&mut self, value: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::CreateArchive(dialog)) = &self.modal else {
            return;
        };
        let result = (|| -> anyhow::Result<_> {
            anyhow::ensure!(
                self.operation.is_none(),
                "Wait for the current operation to finish"
            );
            let mut path = self.resolve_path(value.trim());
            anyhow::ensure!(
                !value.trim().is_empty()
                    && !value.trim().ends_with(std::path::is_separator)
                    && path.file_name().is_some()
                    && !path.is_dir(),
                "Enter a file name or path for the archive"
            );
            let creation = dialog.format.creation().unwrap();
            let suffix = dialog.output_suffix();
            let name = path.file_name().unwrap().to_string_lossy();
            let stem = dialog
                .format
                .info()
                .suffixes
                .iter()
                .filter_map(|ending| strip_archive_suffix(&name, ending))
                .min_by_key(|stem| stem.len())
                .unwrap_or(&name);
            anyhow::ensure!(!stem.is_empty(), "Enter a name before the .{suffix} suffix");
            if strip_archive_suffix(&name, suffix).is_none() {
                path.set_file_name(format!("{stem}.{suffix}"));
            }
            anyhow::ensure!(
                path.parent().is_some_and(Path::is_dir),
                "Destination directory does not exist"
            );
            match path.symlink_metadata() {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
                Ok(_) => anyhow::bail!("Archive already exists: {}", path.display()),
            }
            let options = creation
                .fields
                .iter()
                .zip(&dialog.options)
                .map(|(field, value)| {
                    let value = match value {
                        ArchiveOption::Choice(value) => (*value).into(),
                        ArchiveOption::Text(input) if matches!(field.kind, OptionKind::Secret) => {
                            OptionValue::Secret(read_password(input.read(cx)))
                        }
                        ArchiveOption::Text(input) => {
                            OptionValue::Text(input.read(cx).value().to_string())
                        }
                    };
                    (field.id, value)
                })
                .collect();
            Ok((path, dialog.format, dialog.sources.clone(), options))
        })();
        match result {
            Ok((path, format, sources, options)) => {
                self.dismiss(window, cx);
                self.operation = Some(Operation::create_archive(
                    self.id(),
                    sources,
                    path,
                    format,
                    options,
                    self.filesystems.clone(),
                ));
            }
            Err(error) => self.modal_error = Some(error.to_string()),
        }
        cx.notify();
    }

    pub fn show_modal(&mut self, modal: Modal, window: &mut Window, cx: &mut Context<Self>) {
        let placeholder = if matches!(&modal, Modal::OpenVsx) {
            "Search Open VSX"
        } else {
            ""
        };
        let has_input = matches!(
            &modal,
            Modal::Palette
                | Modal::Search
                | Modal::OpenVsx
                | Modal::QuickPick { .. }
                | Modal::CreateArchive(_)
        );
        self.modal = Some(modal);
        self.modal_error = None;
        self.palette_cursor = 0;
        self.dialog_input.update(cx, |s, cx| {
            s.set_placeholder(placeholder, window, cx);
            s.set_value("", window, cx);
            if has_input {
                s.focus(window, cx);
            }
        });
        if !has_input {
            self.focus_modal(window, cx);
        }
        cx.notify();
    }

    pub fn accept_quick_pick(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.modal, Some(Modal::QuickPick { .. })) {
            return;
        }
        let Some(Modal::QuickPick { action, .. }) = self.modal.take() else {
            return;
        };
        self.dismiss(window, cx);
        match action {
            QuickPickAction::Extension { reply, .. } => {
                let _ = reply.send(index);
            }
            QuickPickAction::ColorTheme(sources) => {
                if let Some(source) = sources.into_iter().nth(index) {
                    self.load_color_theme(source, cx);
                }
            }
        }
    }

    fn dismiss_quick_pick(&mut self, session: u64, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(&self.modal, Some(Modal::QuickPick { action: QuickPickAction::Extension { session: owner, .. }, .. }) if *owner == session)
        {
            self.dismiss(window, cx);
        }
    }

    pub fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.modal.take() {
            Some(Modal::Conflict { reply, .. }) => {
                let _ = reply.send(ConflictResolution::Cancel);
                if let Some(op) = &self.operation {
                    op.token.cancel();
                }
            }
            Some(Modal::ArchivePassword { request, input }) => {
                request.cancel.cancel();
                input.update(cx, |input, cx| input.set_value("", window, cx));
            }
            Some(Modal::CreateArchive(dialog)) => dialog.clear_secrets(window, cx),
            Some(Modal::OpenVsx) => self.clear_market_media(cx),
            _ => {}
        }
        if let Some(search) = &self.search {
            search.token.cancel();
        }
        self.modal_error = None;
        self.focus_pane(self.active, window, cx);
    }
    pub fn error(&mut self, e: impl ToString, cx: &mut Context<Self>) {
        self.status = e.to_string();
        self.extension_logs
            .source("dotdir", "DotDir")
            .error(&self.status);
        cx.notify();
    }
    pub(crate) fn end_shift_selection(&mut self) {
        self.shift_selection = None;
    }

    pub fn run(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let switches_panel_menu = matches!(self.modal, Some(Modal::PanelMenu(_)))
            && matches!(id, "left-panel-menu" | "right-panel-menu");
        if self.modal.is_some()
            && !switches_panel_menu
            && !matches!(id, "escape" | "quit" | "palette")
        {
            return;
        }
        if !self.command_available(id, cx) {
            return;
        }
        if self.modal.is_none()
            && let Some(TabContent::Media { view, .. }) = self.panes[self.active]
                .tabs
                .get(self.panes[self.active].active)
                .map(|tab| &tab.content)
        {
            let view = view.clone();
            if view.update(cx, |view, cx| view.command(id, window, cx)) {
                return;
            }
        }
        let movement = id.strip_prefix("mark-").unwrap_or(id);
        let is_movement = matches!(
            movement,
            "up" | "down" | "left" | "right" | "page-up" | "page-down" | "first" | "last"
        );
        let selecting = is_movement && movement != id;
        if !selecting {
            self.end_shift_selection();
        }
        match id {
            selection_inspector::INSPECT => self.open_selection_inspector(window, cx),
            selection_inspector::SUMMARY => self.status = self.selection_snapshot().summary(),
            "left-panel-menu" => self.open_panel_menu(0, window, cx),
            "right-panel-menu" => self.open_panel_menu(1, window, cx),
            "keep-preview" => {
                if let Some((pane, index)) = self.preview_target() {
                    self.panes[pane].tabs[index].make_permanent();
                }
            }
            _ if is_movement => {
                let layout = self.layout_for(self.active, window);
                let horizontal = matches!(movement, "left" | "right");
                if selecting
                    && horizontal
                    && matches!(
                        layout.presentation,
                        Presentation::Columns | Presentation::Details
                    )
                {
                    return;
                }
                if layout.presentation == Presentation::Columns && horizontal {
                    let pane = self.active;
                    if id == "right" {
                        self.update_column_preview(pane);
                    }
                    if let Some(browser) = self.browser_mut(pane) {
                        browser.active = if id == "left" {
                            browser.active.saturating_sub(1)
                        } else {
                            (browser.active + 1).min(browser.columns.len() - 1)
                        };
                    }
                    self.reveal_column(pane, false);
                    self.persist();
                } else {
                    let selection = self.shift_selection;
                    if let Some(directory) = self.directory_mut(self.active) {
                        if selecting {
                            self.shift_selection =
                                layout.extend_selection(directory, movement, selection);
                        } else {
                            layout.move_cursor(directory, movement);
                        }
                    }
                    self.update_column_preview(self.active);
                    self.reveal_column(self.active, true);
                }
            }
            "switch" => {
                let pane = 1 - self.active;
                let index = self.panes[self.active].opposite_tab(&self.panes[pane]);
                self.set_tab(pane, index, window, cx);
            }
            "open" => {
                if let Some(e) = self
                    .directory(self.active)
                    .and_then(Directory::focused)
                    .cloned()
                {
                    self.open_entry(self.active, e, true, window, cx);
                }
            }
            "parent" => {
                if let Some(path) = self.directory(self.active).and_then(|d| d.path.parent()) {
                    self.navigate(self.active, path, window, cx);
                }
            }
            "home" => {
                if let Some(p) = dirs::home_dir() {
                    self.navigate(self.active, p.into(), window, cx);
                }
            }
            "refresh" => {
                if let Some(d) = self.directory(self.active) {
                    self.reload(d.id);
                }
            }
            "hidden" => {
                self.settings.show_hidden = !self.settings.show_hidden;
                self.refresh_all();
                self.persist();
            }
            "brief" | "view-brief" | "view-icons" | "view-details" | "view-columns" => {
                let mode = match id {
                    "view-icons" => Presentation::Icons,
                    "view-details" => Presentation::Details,
                    "view-columns" => Presentation::Columns,
                    "brief" if self.settings.presentation(self.active) == Presentation::Brief => {
                        Presentation::Details
                    }
                    _ => Presentation::Brief,
                };
                self.set_presentation(self.active, mode, window, cx);
            }
            "sort-name" | "sort-size" | "sort-date" | "sort-extension" => {
                if let Some(d) = self.directory_mut(self.active) {
                    d.sort = match id {
                        "sort-size" => SortOrder::Size,
                        "sort-date" => SortOrder::Modified,
                        "sort-extension" => SortOrder::Extension,
                        _ => SortOrder::Name,
                    };
                    let id = d.id;
                    self.reload(id);
                }
            }
            "mark" | "mark-next" => {
                let layout = self.layout_for(self.active, window);
                if let Some(d) = self.directory_mut(self.active) {
                    d.toggle_mark();
                    if id == "mark-next" {
                        d.move_cursor(1);
                        layout.ensure_visible(d);
                    }
                }
            }
            "select-all" => {
                if let Some(d) = self.directory_mut(self.active) {
                    d.select_all();
                }
            }
            "new-tab" => {
                let path = self.pane_location(self.active);
                self.open_directory_tab(self.active, path, window, cx);
            }
            "next-tab" => {
                let p = &self.panes[self.active];
                let i = (p.active + 1) % p.tabs.len();
                self.set_tab(self.active, i, window, cx);
            }
            "close-tab" => {
                let p = &self.panes[self.active];
                let id = p.tabs[p.active].id();
                self.request_close_tab(self.active, id, window, cx);
            }
            "location" => {
                let location = self.pane_location(self.active);
                let path = if matches!(location, Location::Provider(_)) {
                    location.display()
                } else {
                    self.cwd(self.active).display().to_string()
                };
                self.prompt(
                    PromptKind::Location,
                    "Go to directory",
                    "Enter a directory, archive, or filesystem URI.".into(),
                    path,
                    window,
                    cx,
                );
            }
            "mkdir" => self.prompt(
                PromptKind::Mkdir(self.pane_location(self.active)),
                "Make directory",
                self.pane_location(self.active).display(),
                String::new(),
                window,
                cx,
            ),
            "new-file" => self.prompt(
                PromptKind::NewFile(self.pane_location(self.active)),
                "Open / Create File",
                self.pane_location(self.active).display(),
                String::new(),
                window,
                cx,
            ),
            "create-archive" => self.prompt_create_archive(window, cx),
            "rename" if self.directory(self.active).is_some() => {
                if let Some(e) = self
                    .directory(self.active)
                    .and_then(Directory::focused)
                    .filter(|e| !e.parent)
                    .and_then(|e| Some((e.path.local()?.to_path_buf(), e.name.clone())))
                {
                    self.prompt(
                        PromptKind::Rename(e.0),
                        "Rename",
                        e.1.clone(),
                        e.1,
                        window,
                        cx,
                    );
                }
            }
            "rename" | "save" => {
                if let Some(id) = self.active_document() {
                    self.save(id, cx);
                } else if let Some((path, view)) = self.active_hex()
                    && let Some(path) = path.local().map(Path::to_path_buf)
                    && !view.read(cx).readonly
                {
                    let buffer = view.read(cx).buffer.clone();
                    buffer.update(cx, |buffer, cx| buffer.save(path, cx));
                }
            }
            "definition" | "completions" => {
                if let Some(doc) = self
                    .active_document()
                    .and_then(|id| self.documents.get(&id))
                {
                    let focus = doc.input.read(cx).focus_handle(cx);
                    if id == "definition" {
                        focus.dispatch_action(&GoToDefinition, window, cx);
                    } else {
                        focus.dispatch_action(&ShowCompletions, window, cx);
                    }
                }
            }
            "view-hex" | "view-dump" | "edit-hex" => {
                let mode = if id == "view-dump" {
                    ByteViewMode::Dump
                } else {
                    ByteViewMode::Hex
                };
                if let Some((_, view)) =
                    self.active_hex().filter(|(_, view)| view.read(cx).readonly)
                {
                    view.clone()
                        .update(cx, |view, cx| view.set_mode(mode, window, cx));
                }
                if let Some(path) = self
                    .directory(self.active)
                    .and_then(Directory::focused)
                    .filter(|entry| !entry.directory)
                    .map(|entry| entry.path.clone())
                {
                    self.open_file_into(
                        self.active,
                        path,
                        id != "edit-hex",
                        None,
                        Some(mode),
                        window,
                        cx,
                    );
                }
            }
            "view" | "edit" | "quick-view" | "quick-edit" => {
                if let Some(e) = self
                    .directory(self.active)
                    .and_then(Directory::focused)
                    .cloned()
                {
                    if e.directory {
                        if id == "view" && !e.parent {
                            let request = self.id();
                            self.folder_sizes.start(
                                request,
                                e.path,
                                self.filesystems.archive_operation(),
                                self.tx.clone(),
                            );
                        }
                        cx.notify();
                        return;
                    }
                    let pane = if id.starts_with("quick-") {
                        1 - self.active
                    } else {
                        self.active
                    };
                    if id == "quick-view" {
                        self.start_preview(self.active, window, cx);
                        return;
                    }
                    if id == "quick-edit" {
                        let cwd = self.cwd(pane);
                        self.panes[pane].remove_previews();
                        self.release_unused_documents();
                        if self.panes[pane].tabs.is_empty() {
                            let id = self.id();
                            self.panes[pane].tabs.push(Tab::new(TabContent::Directory(
                                DirectoryBrowser::new(id, cwd.into()),
                            )));
                            self.panes[pane].active = 0;
                            self.reload(id);
                        }
                    }
                    if id == "view" && ARCHIVE_FORMATS.for_path(e.path.name_path()).is_some() {
                        self.open_entry(pane, e, false, window, cx);
                    } else {
                        self.open_document_into(pane, e.path, id == "view", None, window, cx);
                    }
                }
            }
            "copy" | "move" | "trash" => {
                let Some(directory) = self.directory(self.active) else {
                    return;
                };
                let sources: Vec<_> = directory
                    .selected()
                    .into_iter()
                    .map(|entry| entry.path)
                    .collect();
                if sources.is_empty() {
                    return;
                }
                let from_archive = matches!(directory.path, Location::Archive { .. });
                let description = format!(
                    "{} item(s) from {}",
                    sources.len(),
                    directory.path.display()
                );
                if id == "trash" {
                    let (kind, title, description) = if from_archive {
                        (
                            PromptKind::DeleteArchive(sources),
                            "Delete Archive Entries",
                            format!(
                                "{description}
Selected entries will be permanently removed from the archive."
                            ),
                        )
                    } else {
                        (
                            PromptKind::Trash(
                                sources
                                    .into_iter()
                                    .filter_map(|path| path.local().map(Path::to_path_buf))
                                    .collect(),
                            ),
                            "Move to Trash",
                            description,
                        )
                    };
                    self.prompt(kind, title, description, String::new(), window, cx);
                } else {
                    let archive_destination = self.opposite_archive_destination().cloned();
                    let destination = archive_destination
                        .as_ref()
                        .map(Location::display)
                        .or_else(|| {
                            self.pane_location(1 - self.active)
                                .local()
                                .map(|path| path.display().to_string())
                        })
                        .unwrap_or_default();
                    let kind = match id {
                        "move" => OperationKind::Move,
                        _ if from_archive && archive_destination.is_none() => {
                            OperationKind::Extract
                        }
                        _ => OperationKind::Copy,
                    };
                    self.prompt(
                        PromptKind::Transfer {
                            kind,
                            sources,
                            archive_destination,
                        },
                        kind.title(),
                        description,
                        destination,
                        window,
                        cx,
                    );
                }
            }
            "sync" => {
                if let Some(path) = self.directory(self.active).map(|d| d.path.clone()) {
                    self.navigate(1 - self.active, path, window, cx);
                }
            }
            "toggle-line-wrap" => {
                let Some(doc) = self
                    .active_document()
                    .and_then(|id| self.documents.get_mut(&id))
                else {
                    return;
                };
                let Some(wrapped) = doc.line_wrap else {
                    return;
                };
                doc.line_wrap = Some(!wrapped);
                for editor in std::iter::once(&doc.input).chain(doc.inactive_editor()) {
                    editor.update(cx, |state, cx| state.set_soft_wrap(!wrapped, window, cx));
                }
                self.dismiss_hover(cx);
            }
            "toggle-keystrokes" => {
                self.keystrokes.enabled = !self.keystrokes.enabled;
                self.keystrokes.clear();
            }
            "palette" => self.show_modal(Modal::Palette, window, cx),
            "settings" => self.show_modal(Modal::Settings, window, cx),
            "extensions" => self.show_modal(Modal::Extensions, window, cx),
            "find" => self.show_modal(Modal::Search, window, cx),
            "terminal" => {
                self.terminal_visible = !self.terminal_visible;
                if self.terminal_visible {
                    self.ensure_terminal(cx);
                    self.terminal_focus.focus(window, cx);
                } else {
                    self.focus_pane(self.active, window, cx);
                }
            }
            "panels" => {
                self.panels_visible = !self.panels_visible;
                if !self.panels_visible {
                    self.terminal_visible = true;
                    self.ensure_terminal(cx);
                    self.terminal_focus.focus(window, cx);
                } else {
                    self.focus_pane(self.active, window, cx);
                }
            }
            "command-line" => self.command_line.update(cx, |s, cx| s.focus(window, cx)),
            "escape" => {
                if self.modal.is_none() && self.dismiss_hover(cx) {
                    return;
                }
                if self.modal.is_some() {
                    self.dismiss(window, cx);
                } else if let Some(path) = self
                    .directory(self.active)
                    .and_then(Directory::focused)
                    .map(|entry| entry.path.clone())
                    && self.folder_sizes.cancel_for(&path)
                {
                    cx.notify();
                    return;
                } else if self.active_document().is_some() || self.active_hex().is_some() {
                    let p = &self.panes[self.active];
                    self.request_close_tab(self.active, p.tabs[p.active].id(), window, cx);
                } else {
                    self.typeahead.clear();
                    if let Some(d) = self.directory_mut(self.active) {
                        d.marked.clear();
                    }
                    self.focus_pane(self.active, window, cx);
                }
            }
            "quit" => self.request_quit(window, cx),
            id if id.starts_with("vscode:") => self.invoke_vscode(id, window, cx),
            _ => {
                if let Some((ext, c)) = self.extensions.iter().find_map(|e| {
                    e.contributions
                        .iter()
                        .find(|c| c.id == id)
                        .map(|c| (e.manifest.id.clone(), c.clone()))
                }) {
                    self.invoke_extension(ext, c.id, c.title, json!(null), cx);
                }
            }
        }
        cx.notify();
    }
    pub fn on_command(
        &mut self,
        command: &RunCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Media actions now bubble to this shared dispatcher. Resolve the pane
        // owning focus, including a native video view, before validating it.
        if self.modal.is_none() && self.panels_visible {
            for (pane, panel) in self.panes.iter().enumerate() {
                if let Some(TabContent::Media { view, .. }) =
                    panel.tabs.get(panel.active).map(|tab| &tab.content)
                    && view.read(cx).focus_handle(cx).contains_focused(window, cx)
                {
                    self.active = pane;
                    break;
                }
            }
        }
        self.run(&command.id, window, cx);
    }
    /// Prefer the focused preview, otherwise the one following this source tab.
    fn preview_target(&self) -> Option<(usize, usize)> {
        let pane = &self.panes[self.active];
        let tab = pane.tabs.get(pane.active)?;
        if tab.preview {
            return Some((self.active, pane.active));
        }
        self.panes[1 - self.active]
            .tabs
            .iter()
            .position(|target| target.preview && target.preview_source == Some(tab.id()))
            .map(|index| (1 - self.active, index))
    }
    fn start_preview(&mut self, source: usize, window: &mut Window, cx: &mut Context<Self>) {
        let source_id = self.panes[source].tabs[self.panes[source].active].id();
        let pane = 1 - source;
        if let Some(index) = self.panes[pane]
            .tabs
            .iter()
            .position(|tab| tab.preview && tab.preview_source.is_some())
        {
            self.panes[pane].tabs[index].preview_source = Some(source_id);
            self.panes[pane].active = index;
        } else {
            let id = self.id();
            let mut tab = Tab::new(TabContent::Pending {
                id,
                path: None,
                error: None,
            });
            tab.preview = true;
            tab.preview_source = Some(source_id);
            self.panes[pane].insert_after_active(tab);
        }
        self.sync_previews(window, cx);
        cx.notify();
    }
    fn sync_previews(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.panels_visible {
            return;
        }
        for pane in 0..2 {
            let source = 1 - pane;
            let Some(source_tab) = self.panes[source].tabs.get(self.panes[source].active) else {
                continue;
            };
            let source_id = source_tab.id();
            if !self.panes[pane]
                .tabs
                .iter()
                .any(|tab| tab.preview && tab.preview_source == Some(source_id))
            {
                continue;
            }
            let path = self
                .directory(source)
                .and_then(Directory::focused)
                .filter(|entry| !entry.directory && !entry.parent)
                .map(|entry| entry.path.clone());
            // A switched-away source tab pauses following; it does not clear its preview.
            let targets = self.panes[pane]
                .tabs
                .iter()
                .filter(|tab| tab.follows_changed_file(source_id, &path))
                .map(Tab::id)
                .collect::<Vec<_>>();
            for id in targets {
                let request = self.id();
                if let Some(tab) = self.panes[pane].tabs.iter_mut().find(|tab| tab.id() == id) {
                    tab.begin_preview_load(path.clone(), request);
                }
                self.release_unused_documents();
                if let Some(path) = path.clone() {
                    self.open_document_into(pane, path, true, Some(id), window, cx);
                }
                cx.notify();
            }
        }
    }
    fn release_unused_documents(&mut self) {
        let used = self
            .panes
            .iter()
            .flat_map(|pane| &pane.tabs)
            .filter_map(|tab| {
                if let TabContent::Document(id) = tab.content {
                    Some(id)
                } else {
                    None
                }
            })
            .collect::<std::collections::HashSet<_>>();
        self.documents.retain(|id, document| {
            let retained = used.contains(id);
            if !retained && !document.readonly {
                self.language_servers.archive_text(&document.path, None);
            }
            retained
        });
    }
    fn install_content(
        &mut self,
        pane: usize,
        content: TabContent,
        target: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = target {
            let refocus = self.active == pane
                && self.modal.is_none()
                && self.panes[pane]
                    .tabs
                    .get(self.panes[pane].active)
                    .is_some_and(|tab| tab.id() == id)
                && match self.panes[pane]
                    .tabs
                    .get(self.panes[pane].active)
                    .map(|tab| &tab.content)
                {
                    Some(TabContent::Document(id)) => {
                        self.documents.get(id).is_some_and(|document| {
                            document
                                .input
                                .read(cx)
                                .focus_handle(cx)
                                .contains_focused(window, cx)
                        })
                    }
                    Some(TabContent::Media { view, .. }) => {
                        view.read(cx).focus_handle(cx).contains_focused(window, cx)
                    }
                    Some(TabContent::Hex { view, .. }) => {
                        view.read(cx).focus_handle().contains_focused(window, cx)
                    }
                    _ => self.focus.contains_focused(window, cx),
                };
            if let Some(tab) = self.panes[pane].tabs.iter_mut().find(|tab| tab.id() == id) {
                tab.content = content;
            }
            self.release_unused_documents();
            if refocus {
                self.focus_pane(pane, window, cx);
            }
            cx.notify();
            return;
        }
        let existing =
            self.panes[pane]
                .tabs
                .iter()
                .position(|tab| match (&tab.content, &content) {
                    (TabContent::Document(existing), TabContent::Document(id)) => existing == id,
                    (
                        TabContent::Hex {
                            path: existing_path,
                            view: existing,
                            ..
                        },
                        TabContent::Hex { path, view, .. },
                    ) => {
                        existing_path == path
                            && existing.read(cx).readonly == view.read(cx).readonly
                    }
                    _ => false,
                });
        if let Some(index) = existing {
            self.panes[pane].tabs[index].make_permanent();
            self.panes[pane].active = index;
        } else {
            let tab = if matches!(content, TabContent::Document(_)) {
                Tab::with_id(self.id(), content)
            } else {
                Tab::new(content)
            };
            self.panes[pane].insert_after_active(tab);
        }
        self.focus_pane(pane, window, cx);
    }

    fn hex_content(
        &self,
        id: u64,
        path: Location,
        buffer: Entity<HexBuffer>,
        readonly: bool,
        mode: ByteViewMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TabContent {
        let readonly = readonly || path.local().is_none();
        let view =
            cx.new(|cx| HexView::new(buffer.clone(), readonly, mode, self.palette, window, cx));
        let subscription = cx.observe(&buffer, move |this, buffer, cx| {
            if !readonly
                && buffer.read(cx).dirty()
                && let Some((pane, index)) = this.tab_position(id)
            {
                this.panes[pane].tabs[index].make_permanent();
            }
            cx.notify();
        });
        TabContent::Hex {
            id,
            path,
            view,
            _subscription: subscription,
        }
    }
    pub fn open_document(
        &mut self,
        pane: usize,
        path: PathBuf,
        readonly: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_document_into(pane, path.into(), readonly, None, window, cx);
    }
    fn open_entry(
        &mut self,
        pane: usize,
        entry: Entry,
        external: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if entry.directory {
            self.navigate(pane, entry.path, window, cx);
        } else if let Some(format) = ARCHIVE_FORMATS.for_path(entry.path.name_path()) {
            match Archive::mount_location(entry.path, format) {
                Ok(root) => self.navigate(pane, root, window, cx),
                Err(error) => self.error(error, cx),
            }
        } else if let Some(path) = entry.path.local().filter(|_| external) {
            let path = path.to_path_buf();
            std::thread::spawn(move || {
                let _ = open::that(path);
            });
        } else {
            let readonly = entry.path.archive_member().is_some();
            self.open_document_into(pane, entry.path, readonly, None, window, cx);
        }
    }
    fn open_document_into(
        &mut self,
        pane: usize,
        path: Location,
        readonly: bool,
        tab: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_file_into(pane, path, readonly, tab, None, window, cx);
    }

    fn open_file_into(
        &mut self,
        pane: usize,
        path: Location,
        readonly: bool,
        tab: Option<u64>,
        byte_mode: Option<ByteViewMode>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = match path {
            Location::Local(path) => Location::Local(path.canonicalize().unwrap_or(path)),
            archive => archive,
        };
        let readonly = readonly || !path.editable();
        if let Some(mode) = byte_mode
            && tab.is_none()
            && let Some((index, view)) =
                self.panes[pane]
                    .tabs
                    .iter()
                    .enumerate()
                    .find_map(|(index, tab)| match &tab.content {
                        TabContent::Hex {
                            path: existing,
                            view,
                            ..
                        } if *existing == path
                            && view.read(cx).readonly == (readonly || path.local().is_none()) =>
                        {
                            Some((index, view.clone()))
                        }
                        _ => None,
                    })
        {
            view.update(cx, |view, cx| view.set_mode(mode, window, cx));
            self.set_tab(pane, index, window, cx);
            return;
        }
        if readonly && byte_mode.is_none() {
            let bundled = MediaFilter::bundled();
            let builtin = bundled
                .matches(path.name_path())
                .then(|| (VIEWER_PRIORITY, bundled.clone()));
            let extensions = self
                .extensions
                .iter()
                .flat_map(|e| &e.contributions)
                .filter_map(|c| c.viewer.as_ref())
                .filter_map(|v| {
                    MediaFilter::new(&v.patterns)
                        .ok()
                        .filter(|f| f.matches(path.name_path()))
                        .map(|f| (v.priority, f))
                });
            let viewer = builtin
                .into_iter()
                .chain(extensions)
                .max_by_key(|(priority, _)| *priority);
            if let Some((_, filter)) = viewer {
                if tab.is_none() {
                    self.open_request[pane] += 1;
                }
                let id = tab.unwrap_or_else(|| self.id());
                let source = if tab.is_some() { 1 - pane } else { self.active };
                let sort = self.directory(source).map(|d| d.sort).unwrap_or_default();
                let parent = path.parent();
                let source_directory = self
                    .directory(source)
                    .filter(|d| {
                        Some(&d.path) == parent.as_ref()
                            || path.local().and_then(Path::parent).is_some_and(|parent| {
                                d.path
                                    .local()
                                    .and_then(|directory| directory.canonicalize().ok())
                                    .as_deref()
                                    == Some(parent)
                            })
                    })
                    .map(|d| (d.id, d.path.clone()));
                let view = cx.new(|cx| {
                    MediaViewer::new(
                        path.clone(),
                        self.filesystems.clone(),
                        filter,
                        self.settings.show_hidden,
                        sort,
                        window,
                        cx,
                    )
                });
                let subscription = cx.subscribe_in(
                    &view,
                    window,
                    move |this, view, event, window, cx| match event {
                        MediaEvent::AvailabilityChanged => cx.notify(),
                        MediaEvent::Close => {
                            if let Some((pane, index)) = this.tab_position(id)
                                && matches!(&this.panes[pane].tabs[index].content,
                                    TabContent::Media { view: current, .. } if current == view)
                            {
                                this.request_close_tab(pane, id, window, cx);
                            }
                        }
                        MediaEvent::Changed => {
                            let Some((pane, index)) = this.tab_position(id) else {
                                return;
                            };
                            let target = &mut this.panes[pane].tabs[index];
                            let TabContent::Media {
                                path,
                                view: current,
                                ..
                            } = &mut target.content
                            else {
                                return;
                            };
                            if current.entity_id() != view.entity_id() {
                                return;
                            }
                            let new_path = view.read(cx).path.clone();
                            *path = new_path.clone();
                            let following = target.preview;
                            let (rows, columns) = this.layout(window);
                            let mut selected_path = None;
                            if (tab.is_none() || following)
                                && let Some((id, original_path)) = &source_directory
                                && let Some(directory) = this.directory_by_id(*id)
                                && &directory.path == original_path
                                && let Some(index) = directory
                                    .entries
                                    .iter()
                                    .position(|e| e.path.file_name() == new_path.file_name())
                            {
                                directory.cursor = index;
                                directory.ensure_visible(rows, columns);
                                selected_path = Some(directory.entries[index].path.clone());
                            }
                            if following {
                                // Keep the source spelling (e.g. /tmp versus /private/tmp).
                                // Decoder status changes must not restart the same preview.
                                this.panes[pane].tabs[index].preview_path =
                                    selected_path.or(Some(new_path));
                            }
                            if this.active == pane
                                && this.panes[pane].active == index
                                && this.modal.is_none()
                            {
                                this.focus_pane(pane, window, cx);
                            }
                            cx.notify();
                        }
                    },
                );
                self.install_content(
                    pane,
                    TabContent::Media {
                        id,
                        path,
                        view,
                        _subscription: subscription,
                    },
                    tab,
                    window,
                    cx,
                );
                return;
            }
        }
        if readonly
            && byte_mode.is_none()
            && path.local().is_some()
            && image_format(&path).is_some()
        {
            if tab.is_none() {
                self.open_request[pane] += 1;
            }
            let id = self.id();
            self.install_content(
                pane,
                TabContent::Image {
                    id,
                    path,
                    image: None,
                },
                tab,
                window,
                cx,
            );
            return;
        }
        if byte_mode.is_none()
            && let Some(doc) = self.documents.values_mut().find(|d| d.path == path)
        {
            let id = doc.id;
            if !readonly {
                doc.readonly = false;
            }
            self.enable_language_server(id, cx);
            if tab.is_none() {
                self.open_request[pane] += 1;
            }
            self.install_content(pane, TabContent::Document(id), tab, window, cx);
            return;
        }
        if tab.is_none() {
            self.open_request[pane] += 1;
        }
        let request = if let Some(id) = tab {
            let request = self.id();
            if let Some(target) = self.panes[pane]
                .tabs
                .iter_mut()
                .find(|target| target.id() == id)
            {
                target.load_request = request;
            }
            request
        } else {
            self.open_request[pane]
        };
        let tx = self.tx.clone();
        let filesystems = self.filesystems.clone();
        if tab.is_none() {
            self.status = format!("Opening {}…", path.display());
        }
        std::thread::spawn(move || {
            let result = hex_file::open(&path, readonly, byte_mode.is_some(), &filesystems);
            let _ = tx.send(Message::Opened {
                request,
                pane,
                path,
                readonly,
                tab,
                byte_mode: byte_mode.unwrap_or_default(),
                result,
            });
        });
    }
    fn enable_language_server(&mut self, id: u64, cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        let Some(doc) = self
            .documents
            .get(&id)
            .filter(|doc| !doc.readonly && doc.lsp.is_none())
        else {
            return;
        };
        let path = doc.path.clone();
        let text = doc.input.read(cx).text().clone();
        let buffer = if let Some(local) = path.local() {
            vscode::language_extension(&self.vscode_extensions, &self.settings.vscode, local)
                .and_then(|(extension, language)| {
                    self.language_servers.attach_extension(
                        id,
                        local,
                        text,
                        language,
                        extension,
                        &self.settings.vscode,
                    )
                })
        } else {
            self.language_servers
                .archive_text(&path, Some(text.clone()));
            archive_typescript::extension(&path, &self.vscode_extensions, &self.settings.vscode)
                .and_then(|(extension, language)| {
                    match self.language_servers.attach_archive(
                        id,
                        &path,
                        text,
                        extension,
                        language,
                        &self.settings.vscode,
                        self.filesystems.clone(),
                    ) {
                        Ok(buffer) => Some(buffer),
                        Err(error) => {
                            self.documents.get_mut(&id).unwrap().lsp_status =
                                Some(format!("{error:#}"));
                            None
                        }
                    }
                })
        };
        let Some(buffer) = buffer else {
            return;
        };
        let doc = self.documents.get_mut(&id).unwrap();
        doc.lsp_status = Some(format!("Starting {}…", buffer.server_name()));
        doc.lsp = Some(buffer.clone());
        let provider = Rc::new(lsp::EditorProvider(buffer));
        let buffer = Rc::downgrade(&provider.0);
        let code_actions = doc.path.local().is_some();
        doc.input.update(cx, |state, _| {
            let lsp = state.lsp_mut();
            lsp.completion_provider = Some(provider.clone());
            if code_actions {
                lsp.code_action_providers.push(provider.clone());
            }
            lsp.definition_provider = Some(provider);
            lsp.show_document = Some(Rc::new(move |params, window, cx| {
                let Some(destination) = buffer
                    .upgrade()
                    .and_then(|buffer| buffer.source_location(&params.uri))
                else {
                    return false;
                };
                if path == destination {
                    return false;
                }
                let target = params.selection.map(|range| range.start);
                let weak = weak.clone();
                window.defer(cx, move |window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        let pane = this.active;
                        if this.active_document() != Some(id) {
                            return;
                        }
                        this.open_definition_preview(pane, destination, target, window, cx);
                    });
                });
                true
            }));
        });
    }

    pub(crate) fn open_source_location(
        &mut self,
        pane: usize,
        path: PathBuf,
        position: Option<lsp_types::Position>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_source_at(pane, path.into(), position, window, cx);
    }

    fn open_definition_preview(
        &mut self,
        pane: usize,
        path: Location,
        position: Option<lsp_types::Position>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = match path {
            Location::Local(path) => Location::Local(path.canonicalize().unwrap_or(path)),
            path => path,
        };
        let existing = self.panes[pane].tabs.iter().position(|tab| {
            matches!(tab.content, TabContent::Document(id) if self.documents[&id].path == path)
        });
        if let Some(index) = existing {
            self.panes[pane].tabs[index].source_position = position;
            self.set_tab(pane, index, window, cx);
            return;
        }
        let index = if let Some(index) = self.panes[pane]
            .tabs
            .iter()
            .position(Tab::is_definition_preview)
        {
            index
        } else {
            let id = self.id();
            let mut tab = Tab::new(TabContent::Pending {
                id,
                path: None,
                error: None,
            });
            tab.preview = true;
            self.panes[pane].insert_after_active(tab);
            self.panes[pane].active
        };
        let tab = &mut self.panes[pane].tabs[index];
        tab.begin_preview_load(Some(path.clone()), 0);
        tab.source_position = position;
        let id = tab.id();
        self.release_unused_documents();
        self.set_tab(pane, index, window, cx);
        self.open_document_into(pane, path, false, Some(id), window, cx);
    }

    fn open_source_at(
        &mut self,
        pane: usize,
        path: Location,
        position: Option<lsp_types::Position>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = match path {
            Location::Local(path) => Location::Local(path.canonicalize().unwrap_or(path)),
            path => path,
        };
        self.open_document_into(pane, path.clone(), false, None, window, cx);
        if let Some(position) = position {
            let selected = &self.panes[pane].tabs[self.panes[pane].active];
            if let TabContent::Document(id) = selected.content
                && self.documents[&id].path == path
            {
                self.reveal_source_position(id, position, window, cx);
            } else {
                self.source_jump[pane] = Some((self.open_request[pane], position));
            }
        }
    }

    fn reveal_source_position(
        &self,
        id: u64,
        position: lsp_types::Position,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(doc) = self.documents.get(&id) {
            doc.input.update(cx, |editor, cx| {
                let position = lsp::editor_position(editor.text(), position);
                editor.unfold_at(position, cx);
                editor.set_cursor_position(position, window, cx);
            });
        }
    }

    pub fn reconcile_after_history(&self, id: u64, cx: &mut Context<Self>) {
        let Some(document) = self.documents.get(&id) else {
            return;
        };
        let before = document.version;
        let view = cx.weak_entity();
        // This listener runs during action capture. Queue behind the change events
        // emitted by the editor during the subsequent action dispatch.
        cx.defer(move |cx| {
            cx.defer(move |cx| {
                let _ = view.update(cx, |this, cx| {
                    if let Some(document) = this.documents.get_mut(&id)
                        && document.version != before
                        && document.reconcile_saved_text(cx)
                    {
                        cx.notify();
                    }
                });
            });
        });
    }

    pub fn save(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(d) = self.documents.get_mut(&id) {
            if d.readonly || d.saving {
                return;
            }
            if !d.path.editable() {
                return;
            }
            d.saving = true;
            d.error = None;
            let version = d.version;
            let (path, expected, text) = (
                d.path.clone(),
                d.saved.clone(),
                d.input.read(cx).value().to_string(),
            );
            let filesystems = self.filesystems.clone();
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let result = if let Some(local) = path.local() {
                    save_document(local, &expected, &text)
                } else {
                    path.save_archive_member(&expected, &text, &filesystems)
                };
                let _ = tx.send(Message::Saved {
                    id,
                    version,
                    text,
                    result,
                });
            });
            cx.notify();
        }
    }
    fn has_other_hex_editor(&self, id: u64, buffer: &Entity<HexBuffer>, cx: &App) -> bool {
        self.panes.iter().flat_map(|pane| &pane.tabs).any(|tab| {
            if tab.id() == id {
                return false;
            }
            if let TabContent::Hex { view, .. } = &tab.content {
                let view = view.read(cx);
                !view.readonly && view.buffer == *buffer
            } else {
                false
            }
        })
    }

    pub fn request_close_tab(
        &mut self,
        pane: usize,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let document = self.panes[pane]
            .tabs
            .iter()
            .find(|tab| tab.id() == id)
            .and_then(|tab| {
                if let TabContent::Hex { view, .. } = &tab.content {
                    let view = view.read(cx);
                    if view.readonly || self.has_other_hex_editor(id, &view.buffer, cx) {
                        return None;
                    }
                }
                self.file_changes(&tab.content, cx)
            });
        if let Some((path, saving, dirty)) = document {
            if saving {
                self.error("Wait for the save to finish", cx);
                return;
            }
            if dirty {
                self.prompt(
                    PromptKind::CloseTab { pane, id },
                    "Unsaved changes",
                    format!("Discard changes to {}?", path.display()),
                    String::new(),
                    window,
                    cx,
                );
                return;
            }
        }
        self.close_tab(pane, id, window, cx);
    }
    pub fn close_tab(&mut self, pane: usize, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.open_request[pane] += 1;
        let fallback = Location::from(self.cwd(pane));
        let p = &mut self.panes[pane];
        if let Some(i) = p.tabs.iter().position(|t| t.id() == id) {
            if let TabContent::Directory(browser) = &p.tabs[i].content {
                if let Some(watcher) = &self.watcher {
                    for directory in &browser.columns {
                        watcher.remove(&directory.id.to_string());
                    }
                }
                self.panel_scrolls.remove(&browser.id);
            }
            let closed = p.remove_tab(i);
            if let TabContent::Hex { view, .. } = closed.content {
                let view = view.read(cx);
                if !view.readonly
                    && view.buffer.read(cx).dirty()
                    && !self.has_other_hex_editor(id, &view.buffer, cx)
                {
                    let buffer = view.buffer.clone();
                    buffer.update(cx, |buffer, cx| buffer.discard(cx));
                }
            }
        }
        self.release_unused_documents();
        if let Some(w) = &self.watcher {
            w.remove(&id.to_string());
        }
        self.ensure_directory_tab(pane, fallback);
        self.focus_pane(pane, window, cx);
        self.persist();
    }
    fn ensure_directory_tab(&mut self, pane: usize, fallback: Location) {
        if self.panes[pane].tabs.is_empty() {
            let id = self.id();
            let browser = DirectoryBrowser::new(id, fallback);
            self.panes[pane]
                .tabs
                .push(Tab::new(TabContent::Directory(browser)));
            self.reload(id);
        }
    }
    pub fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.modal, Some(Modal::Conflict { .. })) {
            self.error("Resolve or cancel the file conflict before closing", cx);
            return;
        }
        let mut documents = self
            .panes
            .iter()
            .flat_map(|pane| &pane.tabs)
            .filter_map(|tab| self.file_changes(&tab.content, cx));
        if documents.clone().any(|(_, saving, _)| saving) {
            self.error("Wait for pending saves before closing", cx);
            return;
        }
        if documents.any(|(_, _, dirty)| dirty) || self.operation.is_some() {
            self.prompt(
                PromptKind::Quit,
                "Close DotDir",
                "Discard unsaved buffers and cancel any active operation?".into(),
                String::new(),
                window,
                cx,
            );
        } else {
            self.persist();
            self.shutting_down = true;
            window.remove_window();
            cx.quit();
        }
    }
    pub fn persist(&mut self) {
        (self.settings.left, self.settings.left_active) = self.panes[0].saved_directories();
        (self.settings.right, self.settings.right_active) = self.panes[1].saved_directories();
        self.settings.active_pane = self.active;
        if let Err(e) = self.settings.save(&self.settings_path) {
            eprintln!("Cannot save session: {e}");
        }
    }
}

#[cfg(test)]
mod tab_tests {
    use super::{
        DirectoryBrowser, Pane, SavedLocation, Tab, TabContent, parse_copy_destinations,
        quote_copy_destination,
    };
    use std::path::PathBuf;

    #[test]
    fn copy_destination_paths_allow_quoted_separators_and_quotes() {
        let path = "/tmp/has; a \"quote\"";
        let value = format!("folder one; {}", quote_copy_destination(path));
        assert_eq!(
            parse_copy_destinations(&value).unwrap(),
            ["folder one", path]
        );
        assert!(parse_copy_destinations("one; ;three").is_err());
        assert!(parse_copy_destinations("\"one;two").is_err());
    }

    fn preview() -> Tab {
        let mut tab = Tab::new(TabContent::Pending {
            id: 10,
            path: None,
            error: None,
        });
        tab.preview = true;
        tab.preview_source = Some(3);
        tab
    }

    #[test]
    fn file_tabs_open_next_to_the_active_tab_and_close_back_to_it() {
        let file = |id| Tab::new(TabContent::Document(id));
        let mut pane = Pane {
            tabs: vec![file(1), file(2), file(3)],
            active: 1,
        };
        pane.insert_after_active(file(4));
        assert_eq!(
            pane.tabs.iter().map(Tab::id).collect::<Vec<_>>(),
            [1, 2, 4, 3]
        );
        assert_eq!(pane.tabs[pane.active].id(), 4);
        pane.remove_tab(pane.active);
        assert_eq!(pane.tabs[pane.active].id(), 2);
        pane.remove_tab(0);
        assert_eq!(pane.tabs[pane.active].id(), 2);
        pane.remove_tab(pane.active);
        assert_eq!(pane.tabs[pane.active].id(), 3);
    }

    #[test]
    fn restored_tab_selection_skips_missing_directories_without_shifting_forward() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let selected = temp.path().join("selected");
        let last = temp.path().join("last");
        for path in [&first, &selected, &last] {
            std::fs::create_dir(path).unwrap();
        }
        let paths = [
            first.clone(),
            temp.path().join("missing"),
            selected.clone(),
            last,
        ]
        .map(SavedLocation::Local);
        let mut next_id = 1;
        let pane = Pane::restore(&paths, 2, &mut next_id);
        assert_eq!(pane.tabs.len(), 3);
        let active_path = |pane: &Pane| match &pane.tabs[pane.active].content {
            TabContent::Directory(browser) => browser.current().path.local().unwrap().to_path_buf(),
            _ => panic!("Expected a directory tab"),
        };
        assert_eq!(active_path(&pane), selected);
        let pane = Pane::restore(&paths, 1, &mut next_id);
        assert_eq!(active_path(&pane), first);
    }

    #[test]
    fn saving_with_a_file_tab_active_keeps_the_preceding_directory_selected() {
        let folder = |id, path: &str| {
            Tab::new(TabContent::Directory(DirectoryBrowser::new(
                id,
                PathBuf::from(path).into(),
            )))
        };
        let mut pane = Pane {
            tabs: vec![
                folder(1, "first"),
                folder(2, "selected"),
                Tab::new(TabContent::Document(3)),
                folder(4, "last"),
            ],
            active: 2,
        };
        let (paths, active) = pane.saved_directories();
        assert_eq!(paths.len(), 3);
        assert_eq!(active, 1);
        assert!(
            matches!(&paths[active], SavedLocation::Local(path) if path == &PathBuf::from("selected"))
        );
        pane.active = 1;
        assert_eq!(pane.saved_directories().1, active);
    }

    #[test]
    fn drag_reorder_resolves_slots_on_both_sides_of_the_original_tab() {
        let mut pane = Pane {
            tabs: [1, 2, 3, 4]
                .map(|id| Tab::new(TabContent::Document(id)))
                .into(),
            active: 1,
        };
        assert_eq!(pane.reorder_tab(1, 4), 3);
        assert_eq!(pane.reorder_tab(3, 0), 0);
        assert_eq!(pane.reorder_tab(0, 1), 0);
        assert_eq!(
            pane.tabs.iter().map(Tab::id).collect::<Vec<_>>(),
            [2, 1, 3, 4]
        );
    }

    #[test]
    fn switching_from_a_preview_restores_its_source_tab_and_handles_a_closed_source() {
        let source = |id| Tab::new(TabContent::Document(id));
        let mut origin = Pane {
            tabs: vec![source(3), source(4)],
            active: 1,
        };
        let mut opposite = Pane {
            tabs: vec![preview()],
            active: 0,
        };
        assert_eq!(opposite.opposite_tab(&origin), 0);
        origin.tabs.remove(0);
        origin.active = 0;
        assert_eq!(opposite.opposite_tab(&origin), 0);
        opposite.tabs[0].make_permanent();
        origin.tabs.push(source(5));
        origin.active = 1;
        assert_eq!(opposite.opposite_tab(&origin), 1);
    }

    #[test]
    fn quick_edit_removes_temporary_previews_and_preserves_firm_tabs() {
        let mut firm = preview();
        firm.id = 11;
        firm.make_permanent();
        let mut pane = Pane {
            tabs: vec![preview(), firm, Tab::new(TabContent::Document(12))],
            active: 1,
        };
        pane.remove_previews();
        assert_eq!(
            pane.tabs.iter().map(Tab::id).collect::<Vec<_>>(),
            vec![11, 12]
        );
        assert_eq!(pane.active, 0);
        assert!(!pane.tabs.iter().any(|tab| tab.preview));
        let mut only_preview = Pane {
            tabs: vec![preview()],
            active: 0,
        };
        only_preview.remove_previews();
        assert!(only_preview.tabs.is_empty());
        assert_eq!(only_preview.active, 0);

        let mut between_tabs = Pane {
            tabs: vec![
                Tab::new(TabContent::Document(11)),
                preview(),
                Tab::new(TabContent::Document(12)),
            ],
            active: 1,
        };
        between_tabs.remove_previews();
        between_tabs.insert_after_active(Tab::new(TabContent::Document(13)));
        assert_eq!(
            between_tabs.tabs.iter().map(Tab::id).collect::<Vec<_>>(),
            [11, 13, 12]
        );
    }

    #[test]
    fn preview_tracks_only_its_source_and_preserves_identity_across_files() {
        let mut tab = preview();
        let first = Some(PathBuf::from("/tmp/first.txt").into());
        assert!(tab.follows_changed_file(3, &first));
        assert!(!tab.follows_changed_file(4, &first));
        tab.begin_preview_load(first.clone(), 20);
        tab.content = TabContent::Document(30);
        assert!(tab.preview);
        assert_eq!(tab.id(), 10);
        assert!(!tab.follows_changed_file(3, &first));
        let next = Some(PathBuf::from("/tmp/next.png").into());
        assert!(tab.follows_changed_file(3, &next));
        tab.begin_preview_load(next.clone(), 21);
        tab.content = TabContent::Image {
            id: 31,
            path: next.unwrap(),
            image: None,
        };
        assert_eq!(tab.id(), 10);
        assert!(tab.preview);
        assert!(!tab.accepts_load(20));
        assert!(tab.accepts_load(21));
        assert!(tab.follows_changed_file(3, &None));
        tab.begin_preview_load(None, 22);
        assert!(!tab.accepts_load(21));
    }

    #[test]
    fn keeping_a_pending_preview_freezes_its_file_without_cancelling_its_load() {
        let mut firm = preview();
        let file = Some(PathBuf::from("/tmp/first.txt").into());
        firm.begin_preview_load(file.clone(), 20);
        firm.make_permanent();
        assert!(!firm.preview);
        assert_eq!(firm.preview_source, None);
        assert_eq!(firm.preview_path, file);
        assert!(firm.accepts_load(20));
        assert!(!firm.follows_changed_file(3, &Some(PathBuf::from("/tmp/next.txt").into())));
        // A new preview has independent request state; it cannot cancel the firm tab.
        let mut next = preview();
        next.begin_preview_load(Some(PathBuf::from("/tmp/next.txt").into()), 21);
        assert!(firm.accepts_load(20));
        assert!(!firm.accepts_load(21));
        assert!(next.preview);
    }
}
