use crate::{
    column_scroll::ColumnScroll,
    commands::{COMMANDS, CommandState, RunCommand},
    icon_theme::IconTheme,
    media::{MediaEvent, MediaFilter, MediaViewer},
    model::*,
    operations::*,
    terminal::Terminal,
    theme::Palette,
};
use dotdir_core::{
    copy::{CancelToken, ConflictResolution, CopyEvent, CopyOptions},
    search::{FileSearchEvent, FileSearchRequest},
    watch::FsWatcher,
};
use dotdir_extension_host::{ExtensionHost, ExtensionReply, LoadedExtension, PanelNode};
use gpui_kit::{
    component::input::{EditorState, InputEvent, InputState},
    *,
};
use serde_json::json;
use std::{
    cell::Cell,
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

pub(crate) enum Message {
    IconTheme(u64, anyhow::Result<Box<IconTheme>>),
    Directory(u64, u64, anyhow::Result<Vec<Entry>>),
    Watch(u64),
    Opened {
        request: u64,
        pane: usize,
        path: PathBuf,
        readonly: bool,
        tab: Option<u64>,
        result: anyhow::Result<Vec<u8>>,
    },
    Saved {
        id: u64,
        text: String,
        result: anyhow::Result<()>,
    },
    Search(u64, FileSearchEvent),
    Extension(anyhow::Result<LoadedExtension>),
    ExtensionReply {
        request: u64,
        extension: String,
        command: String,
        title: String,
        result: anyhow::Result<ExtensionReply>,
    },
    Simple(anyhow::Result<()>),
}

impl DotDir {
    pub fn accept_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.dialog_input.read(cx).value().to_string();
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
        let Some(Modal::Prompt { kind, .. }) = self.modal.take() else {
            return;
        };
        let result = (|| -> anyhow::Result<()> {
            match kind.clone() {
                PromptKind::IconTheme => {
                    self.load_icon_theme(self.resolve_path(value.trim()), cx);
                }
                PromptKind::Location => {
                    let p = self.resolve_path(&value);
                    if !p.is_dir() {
                        anyhow::bail!("Directory does not exist");
                    }
                    self.navigate(self.active, p, window, cx);
                }
                PromptKind::Mkdir => {
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
                PromptKind::NewFile => {
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
                PromptKind::Transfer(kind, sources) => {
                    let destination = self.resolve_path(&value);
                    if !destination.is_dir() {
                        anyhow::bail!("Destination directory does not exist");
                    }
                    let id = self.id();
                    self.operation = Some(Operation::start(
                        id,
                        kind,
                        sources,
                        destination,
                        self.copy_options.clone(),
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
                title: "Check your input".into(),
                description: String::new(),
            });
            self.modal_error = Some(e.to_string());
            self.dialog_input.update(cx, |s, cx| s.focus(window, cx));
        } else if !self.shutting_down {
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
                        | "left"
                        | "right"
                        | "first"
                        | "last"
                        | "mark-down"
                        | "mark-up"
                        | "escape"
                )
            })
            .map(|c| (c.id.to_string(), c.title.to_string(), c.key.to_string()))
            .chain(self.extensions.iter().flat_map(|e| {
                e.contributions
                    .iter()
                    .filter(|c| c.viewer.is_none())
                    .map(|c| (c.id.clone(), c.title.clone(), "Extension".into()))
            }))
            .filter(|(id, title, _)| {
                query
                    .split_whitespace()
                    .all(|q| title.to_lowercase().contains(q) || id.to_lowercase().contains(q))
            })
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
        CommandState {
            panels_visible: self.panels_visible,
            directory: directory.is_some(),
            entry: entry.is_some(),
            real_entry: entry.is_some_and(|entry| !entry.parent),
            file: entry.is_some_and(|entry| !entry.directory && !entry.parent),
            items: directory.is_some_and(|d| d.entries.iter().any(|e| !e.parent)),
            selection: directory
                .is_some_and(|d| !d.marked.is_empty() || d.focused().is_some_and(|e| !e.parent)),
            parent: directory.is_some_and(|d| d.path.parent().is_some()),
            can_save: self.panels_visible && document.is_some_and(|d| !d.readonly && !d.saving),
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
                self.navigate(self.active, path, window, cx);
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
        let mut roots = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../extensions")];
        if let Some(bundle) = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.join("../Resources/extensions")))
        {
            roots.push(bundle);
        }
        if let Some(base) = self.settings_path.parent() {
            roots.push(base.join("extensions"));
        }
        let mut seen = std::collections::HashSet::new();
        for root in roots {
            if let Ok(entries) = std::fs::read_dir(root) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.join("extension.json").is_file() {
                        continue;
                    }
                    if !seen.insert(path.canonicalize().unwrap_or(path.clone())) {
                        continue;
                    }
                    let rx = self.extension_host.load(path);
                    let tx = self.tx.clone();
                    std::thread::spawn(move || {
                        if let Ok(result) = rx.recv() {
                            let _ = tx.send(Message::Extension(result));
                        }
                    });
                }
            }
        }
    }
    pub fn extension_context(&self) -> serde_json::Value {
        let source = if self.directory(self.active).is_some() {
            self.active
        } else {
            1 - self.active
        };
        let selected = self
            .directory(source)
            .map(Directory::selected)
            .unwrap_or_default();
        json!({"path":self.cwd(source),"oppositePath":self.cwd(1-source),"selected":selected.iter().map(|e|json!({"name":e.name,"path":e.path,"size":e.size,"directory":e.directory})).collect::<Vec<_>>()})
    }
    pub fn invoke_extension(
        &mut self,
        extension: String,
        command: String,
        title: String,
        event: serde_json::Value,
        _cx: &mut Context<Self>,
    ) {
        self.extension_request += 1;
        let request = self.extension_request;
        let rx = self.extension_host.invoke(
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
            let path = self.resolve_path(path);
            if path.is_dir() {
                self.navigate(self.active, path, window, cx);
            } else {
                self.error("Directory does not exist", cx);
                return;
            }
        } else {
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
        for _ in 0..200 {
            let Ok(message) = self.rx.try_recv() else {
                break;
            };
            changed = true;
            match message {
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
                Message::Directory(id, generation, result) => {
                    let path = if let Some(d) = self.directory_by_id(id) {
                        d.apply(generation, result);
                        Some(d.path.clone())
                    } else {
                        None
                    };
                    if let (Some(w), Some(path)) = (&self.watcher, path) {
                        w.remove(&id.to_string());
                        w.add(&id.to_string(), &path.to_string_lossy());
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
                Message::Opened {
                    request,
                    pane,
                    path,
                    readonly,
                    tab,
                    result,
                } => {
                    let current = match tab {
                        Some(id) => self.panes[pane]
                            .tabs
                            .iter()
                            .any(|tab| tab.id() == id && tab.accepts_load(request)),
                        None => self.open_request[pane] == request,
                    };
                    if !current {
                        continue;
                    }
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
                            } else {
                                self.error(e, cx);
                            }
                        }
                        Ok(bytes) => {
                            let (text, readonly) = match String::from_utf8(bytes.clone()) {
                                Ok(text) => (text, readonly),
                                Err(_) => {
                                    let text = bytes
                                        .chunks(16)
                                        .enumerate()
                                        .map(|(i, row)| {
                                            format!(
                                                "{:08x}  {:48}  {}",
                                                i * 16,
                                                row.iter()
                                                    .map(|b| format!("{b:02x} "))
                                                    .collect::<String>(),
                                                row.iter()
                                                    .map(|b| if b.is_ascii_graphic() {
                                                        *b as char
                                                    } else {
                                                        '.'
                                                    })
                                                    .collect::<String>()
                                            )
                                        })
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    (text, true)
                                }
                            };
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
                                let language = language_for(&path);
                                let input = cx.new(|cx| {
                                    EditorState::new(window, cx)
                                        .language(language)
                                        .default_value(text)
                                });
                                let subscription =
                                    cx.subscribe(&input, move |this, _, event: &InputEvent, cx| {
                                        if matches!(event, InputEvent::Change)
                                            && let Some(doc) = this.documents.get(&id)
                                        {
                                            let previous = doc.dirty_cache.replace(None);
                                            // The editor redraws itself. Rebuild the surrounding
                                            // panes only when their modified indicator changes.
                                            let dirty = doc.dirty(cx);
                                            if dirty {
                                                for tab in this.panes.iter_mut().flat_map(|pane| &mut pane.tabs) {
                                                    if matches!(tab.content, TabContent::Document(doc) if doc == id) { tab.make_permanent(); }
                                                }
                                            }
                                            if previous != Some(dirty) {
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
                                        saved: bytes,
                                        dirty_cache: Cell::new(None),
                                        readonly,
                                        saving: false,
                                        error: None,
                                        _subscription: subscription,
                                    },
                                );
                                id
                            };
                            self.install_content(pane, TabContent::Document(id), tab, window, cx);
                            if tab.is_none() {
                                self.status.clear();
                            }
                        }
                    }
                }
                Message::Saved { id, text, result } => {
                    if let Some(d) = self.documents.get_mut(&id) {
                        d.saving = false;
                        match result {
                            Ok(()) => {
                                d.saved = text.into_bytes();
                                d.dirty_cache.set(None);
                                d.error = None;
                                self.status = format!("Saved {}", d.path.display());
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
                Message::Extension(result) => match result {
                    Ok(e) => {
                        if let Some(i) = self
                            .extensions
                            .iter()
                            .position(|x| x.manifest.id == e.manifest.id)
                        {
                            self.extensions[i] = e;
                        } else {
                            self.extensions.push(e);
                        }
                    }
                    Err(e) => self.extension_errors.push(e.to_string()),
                },
                Message::ExtensionReply {
                    request,
                    extension,
                    command,
                    title,
                    result,
                } => {
                    if request != self.extension_request {
                        continue;
                    }
                    match result {
                        Err(e) => self.error(format!("Extension: {e}"), cx),
                        Ok(reply) => {
                            if let Some(message) = reply.message {
                                self.status = message;
                            }
                            if let Some(node) = reply.panel {
                                let existing=self.panes.iter().enumerate().find_map(|(p,pane)|pane.tabs.iter().position(|t|matches!(&t.content,TabContent::Panel{extension:e,command:c,..} if e==&extension && c==&command)).map(|i|(p,i)));
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
            self.reload(id);
            changed = true;
        }
        let mut settled = None;
        let mut focus_conflict = false;
        if let Some(op) = &mut self.operation {
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
        self.sync_previews(window, cx);
        if self
            .terminals
            .iter()
            .any(|t| t.dirty.swap(false, Ordering::Relaxed))
        {
            changed = true;
        }
        if changed {
            cx.notify();
            window.refresh();
        }
    }
}
fn language_for(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "rs" => "rust",
        "js" | "jsx" | "mjs" => "javascript",
        "ts" | "tsx" => "typescript",
        "json" | "jsonc" => "json",
        "toml" => "toml",
        "css" => "css",
        "md" => "markdown",
        "sh" | "zsh" | "bash" => "bash",
        _ => "plain_text",
    }
}
pub(crate) struct Document {
    pub id: u64,
    pub path: PathBuf,
    pub input: Entity<EditorState>,
    pub saved: Vec<u8>,
    // Invalidated by text changes and successful saves, never by cursor movement
    // or redraws. Keep undo-to-saved detection without allocating the full text.
    dirty_cache: Cell<Option<bool>>,
    pub readonly: bool,
    pub saving: bool,
    pub error: Option<String>,
    pub _subscription: Subscription,
}
impl Document {
    pub fn dirty(&self, cx: &App) -> bool {
        if self.readonly {
            return false;
        }
        if let Some(dirty) = self.dirty_cache.get() {
            return dirty;
        }
        let text = self.input.read(cx).text();
        let mut offset = 0;
        let matches = text.len() == self.saved.len()
            && text.chunks().all(|chunk| {
                let end = offset + chunk.len();
                let matches = chunk.as_bytes() == &self.saved[offset..end];
                offset = end;
                matches
            });
        self.dirty_cache.set(Some(!matches));
        !matches
    }
}
pub(crate) enum TabContent {
    Pending {
        id: u64,
        path: Option<PathBuf>,
        error: Option<String>,
    },
    Directory(DirectoryBrowser),
    Document(u64),
    Image {
        id: u64,
        path: PathBuf,
    },
    Media {
        id: u64,
        path: PathBuf,
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
}
impl TabContent {
    pub fn id(&self) -> u64 {
        match self {
            Self::Directory(d) => d.id,
            Self::Document(id) => *id,
            Self::Panel { id, .. }
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
    preview_path: Option<PathBuf>,
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
        }
    }
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn make_permanent(&mut self) {
        self.preview = false;
        self.preview_source = None;
    }
    fn follows_changed_file(&self, source: u64, path: &Option<PathBuf>) -> bool {
        self.preview && self.preview_source == Some(source) && &self.preview_path != path
    }
    fn begin_preview_load(&mut self, path: Option<PathBuf>, request: u64) {
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
impl Pane {
    fn opposite_tab(&self, opposite: &Pane) -> usize {
        self.tabs
            .get(self.active)
            .filter(|tab| tab.preview)
            .and_then(|tab| tab.preview_source)
            .and_then(|source| opposite.tabs.iter().position(|tab| tab.id() == source))
            .unwrap_or(opposite.active)
    }
    fn remove_previews(&mut self) {
        let active = self.tabs.get(self.active).map(Tab::id);
        self.tabs.retain(|tab| !tab.preview);
        self.active = active
            .and_then(|id| self.tabs.iter().position(|tab| tab.id() == id))
            .unwrap_or_else(|| self.active.min(self.tabs.len().saturating_sub(1)));
    }
}
#[derive(Clone)]
pub(crate) enum PromptKind {
    IconTheme,
    Location,
    Mkdir,
    Rename(PathBuf),
    NewFile,
    Transfer(OperationKind, Vec<PathBuf>),
    Trash(Vec<PathBuf>),
    CloseTab { pane: usize, id: u64 },
    Quit,
}
pub(crate) enum Modal {
    Prompt {
        kind: PromptKind,
        title: String,
        description: String,
    },
    Palette,
    Settings,
    Extensions,
    Search,
    Conflict {
        source: PathBuf,
        destination: PathBuf,
        reply: mpsc::Sender<ConflictResolution>,
    },
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
    pub panel_scrolls: HashMap<u64, ColumnScroll>,
    pub active: usize,
    pub documents: HashMap<u64, Document>,
    pub settings: Settings,
    pub settings_path: PathBuf,
    pub palette: Palette,
    pub icon_theme: Option<IconTheme>,
    pub icon_theme_request: u64,
    pub command_line: Entity<InputState>,
    pub dialog_input: Entity<InputState>,
    pub search_content: Entity<InputState>,
    pub modal: Option<Modal>,
    pub modal_error: Option<String>,
    pub palette_cursor: usize,
    pub operation: Option<Operation>,
    pub copy_options: CopyOptions,
    pub terminals: Vec<Terminal>,
    pub terminal_active: usize,
    pub terminal_visible: bool,
    pub panels_visible: bool,
    pub extensions: Vec<LoadedExtension>,
    pub extension_host: ExtensionHost,
    pub extension_errors: Vec<String>,
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
        if let Some(path) = initial
            && path.is_dir()
        {
            settings.left = vec![path.clone()];
            settings.right = vec![path];
        }
        let palette = Palette::named(&settings.theme);
        palette.apply(&settings.theme, cx);
        let (tx, rx) = mpsc::channel();
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
        let search_content =
            cx.new(|cx| InputState::new(window, cx).placeholder("Containing text (optional)"));
        let subscriptions = vec![
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
                        cx.notify();
                        window.refresh();
                    }
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &search_content,
                window,
                |_, _, _: &InputEvent, window, cx| {
                    cx.notify();
                    window.refresh();
                },
            ),
            cx.on_release(|this, _| {
                this.persist();
                if let Some(s) = &this.search {
                    s.token.cancel();
                }
            }),
        ];
        let mut next_id = 1;
        let panes = [settings.left.clone(), settings.right.clone()].map(|paths| Pane {
            tabs: paths
                .into_iter()
                .map(|path| {
                    let d = DirectoryBrowser::new(next_id, path);
                    next_id += 1;
                    Tab::new(TabContent::Directory(d))
                })
                .collect(),
            active: 0,
        });
        let cache = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("dev.dotdir.native/quickjs");
        let host = ExtensionHost::new(cache);
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
            panel_scrolls: HashMap::new(),
            active: settings.active_pane,
            documents: HashMap::new(),
            palette,
            icon_theme: None,
            icon_theme_request: 0,
            command_line,
            dialog_input,
            search_content,
            modal: None,
            modal_error: None,
            palette_cursor: 0,
            operation: None,
            copy_options: CopyOptions::default(),
            terminals: vec![],
            terminal_active: 0,
            terminal_visible: false,
            panels_visible: true,
            extensions: vec![],
            extension_host: host,
            extension_errors: vec![],
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
            settings_path,
            settings,
            _subscriptions: subscriptions,
            shutting_down: false,
            quit_pending: false,
        };
        this.panes[0].active = this.settings.left_active.min(this.panes[0].tabs.len() - 1);
        this.panes[1].active = this.settings.right_active.min(this.panes[1].tabs.len() - 1);
        let ids = this
            .panes
            .iter()
            .flat_map(|p| p.tabs.iter().map(Tab::id))
            .collect::<Vec<_>>();
        for id in ids {
            this.reload(id);
        }
        this.load_extensions();
        if let Some(path) = this.settings.icon_theme.clone() {
            this.load_icon_theme(path, cx);
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
    pub fn cwd(&self, pane: usize) -> PathBuf {
        match self.panes[pane]
            .tabs
            .get(self.panes[pane].active)
            .map(|tab| &tab.content)
        {
            Some(TabContent::Directory(d)) => d.current().path.clone(),
            Some(TabContent::Document(id)) => self
                .documents
                .get(id)
                .and_then(|d| d.path.parent().map(PathBuf::from))
                .unwrap_or_default(),
            Some(TabContent::Image { path, .. } | TabContent::Media { path, .. }) => {
                path.parent().map(PathBuf::from).unwrap_or_default()
            }
            _ => self.panes[pane]
                .tabs
                .iter()
                .find_map(|t| {
                    if let TabContent::Directory(d) = &t.content {
                        Some(d.current().path.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| dirs::home_dir().unwrap_or_default()),
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
    pub fn reload(&mut self, id: u64) {
        let hidden = self.settings.show_hidden;
        if let Some(d) = self.directory_by_id(id) {
            d.generation += 1;
            d.loading = true;
            let generation = d.generation;
            let path = d.path.clone();
            let sort = d.sort;
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(Message::Directory(
                    id,
                    generation,
                    read_directory(&path, hidden, sort),
                ));
            });
        }
    }
    pub fn refresh_all(&mut self) {
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
            .map(|d| d.id)
            .collect::<Vec<_>>();
        for id in ids {
            self.reload(id);
        }
    }
    pub fn navigate(
        &mut self,
        pane: usize,
        path: PathBuf,
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
            } else if columns && path.parent() == Some(browser.current().path.as_path()) {
                if let Some(index) = browser
                    .current()
                    .entries
                    .iter()
                    .position(|e| e.path == path)
                {
                    browser.current_mut().cursor = index;
                }
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
        self.active = pane;
        if let Some(id) = self.active_document() {
            if let Some(d) = self.documents.get(&id) {
                d.input.update(cx, |s, cx| s.focus(window, cx));
            }
        } else if let Some(TabContent::Media { view, .. }) = self.panes[pane]
            .tabs
            .get(self.panes[pane].active)
            .map(|tab| &tab.content)
        {
            window.focus(&view.read(cx).focus_handle(cx), cx);
        } else {
            self.focus.focus(window, cx);
        }
        // A newly mounted pane/editor is not in the dispatch tree until the
        // next frame. Restore focus after that tree replaces a closed dialog.
        let view = cx.weak_entity();
        window.on_next_frame(move |window, cx| {
            let _ = view.update(cx, |this, cx| {
                if this.active != pane || this.modal.is_some() {
                    return;
                }
                if let Some(id) = this.active_document() {
                    if let Some(d) = this.documents.get(&id) {
                        d.input.update(cx, |s, cx| s.focus(window, cx));
                    }
                } else if let Some(TabContent::Media { view, .. }) = this.panes[pane]
                    .tabs
                    .get(this.panes[pane].active)
                    .map(|tab| &tab.content)
                {
                    window.focus(&view.read(cx).focus_handle(cx), cx);
                } else {
                    this.focus.focus(window, cx);
                }
            });
        });
        cx.notify();
    }
    fn focus_modal(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.modal_focus.focus(window, cx);
        let view = cx.weak_entity();
        window.on_next_frame(move |window, cx| {
            let _ = view.update(cx, |this, cx| {
                let input = match &this.modal {
                    Some(Modal::Palette | Modal::Search) => true,
                    Some(Modal::Prompt { kind, .. }) => !matches!(
                        kind,
                        PromptKind::Trash(_) | PromptKind::CloseTab { .. } | PromptKind::Quit
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
        self.focus_pane(pane, window, cx);
        self.persist();
    }
    pub fn layout_for(&self, pane: usize, window: &Window) -> PanelLayout {
        let height: f32 = window.viewport_size().height.into();
        let width: f32 = window.viewport_size().width.into();
        PanelLayout::new(
            self.settings.presentation(pane),
            width / 2. - 10.,
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
            if entry.directory {
                self.navigate(pane, entry.path, window, cx);
            } else {
                self.open_document(pane, entry.path, false, window, cx);
            }
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
        let has_input = !matches!(
            &kind,
            PromptKind::Trash(_) | PromptKind::CloseTab { .. } | PromptKind::Quit
        );
        self.modal_error = None;
        self.modal = Some(Modal::Prompt {
            kind,
            title: title.into(),
            description,
        });
        self.dialog_input.update(cx, |s, cx| {
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
    pub fn show_modal(&mut self, modal: Modal, window: &mut Window, cx: &mut Context<Self>) {
        let has_input = matches!(&modal, Modal::Palette | Modal::Search);
        self.modal = Some(modal);
        self.modal_error = None;
        self.palette_cursor = 0;
        self.dialog_input.update(cx, |s, cx| {
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
    pub fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Modal::Conflict { reply, .. }) = self.modal.take() {
            let _ = reply.send(ConflictResolution::Cancel);
            if let Some(op) = &self.operation {
                op.token.cancel();
            }
        }
        if let Some(search) = &self.search {
            search.token.cancel();
        }
        self.modal_error = None;
        self.focus_pane(self.active, window, cx);
    }
    pub fn error(&mut self, e: impl ToString, cx: &mut Context<Self>) {
        self.status = e.to_string();
        cx.notify();
    }
    pub fn run(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal.is_some() && !matches!(id, "escape" | "quit" | "palette") {
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
        let (rows, columns) = self.layout(window);
        match id {
            "keep-preview" => {
                if let Some((pane, index)) = self.preview_target() {
                    self.panes[pane].tabs[index].make_permanent();
                }
            }
            "up" | "down" | "left" | "right" | "page-up" | "page-down" | "mark-down"
            | "mark-up" => {
                let layout = self.layout_for(self.active, window);
                if layout.presentation == Presentation::Columns && matches!(id, "left" | "right") {
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
                    if let Some(d) = self.directory_mut(self.active) {
                        layout.move_cursor(d, id);
                    }
                    self.update_column_preview(self.active);
                    self.reveal_column(self.active, true);
                }
            }
            "first" | "last" => {
                if let Some(d) = self.directory_mut(self.active) {
                    d.reset_icon_scroll();
                    d.cursor = if id == "first" {
                        0
                    } else {
                        d.entries.len().saturating_sub(1)
                    };
                    d.ensure_visible(rows, columns);
                }
                self.update_column_preview(self.active);
                self.reveal_column(self.active, true);
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
                    if e.directory {
                        self.navigate(self.active, e.path, window, cx);
                    } else {
                        let path = e.path;
                        std::thread::spawn(move || {
                            let _ = open::that(path);
                        });
                    }
                }
            }
            "parent" => {
                let cwd = self.cwd(self.active);
                if let Some(p) = cwd.parent() {
                    self.navigate(self.active, p.into(), window, cx);
                }
            }
            "home" => {
                if let Some(p) = dirs::home_dir() {
                    self.navigate(self.active, p, window, cx);
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
                if let Some(d) = self.directory_mut(self.active) {
                    d.toggle_mark();
                    if id == "mark-next" {
                        d.move_cursor(1, false, rows, columns);
                    }
                }
            }
            "select-all" => {
                if let Some(d) = self.directory_mut(self.active) {
                    d.select_all();
                }
            }
            "new-tab" => {
                self.open_request[self.active] += 1;
                let id = self.id();
                let path = self.cwd(self.active);
                let p = &mut self.panes[self.active];
                p.tabs
                    .push(Tab::new(TabContent::Directory(DirectoryBrowser::new(
                        id, path,
                    ))));
                p.active = p.tabs.len() - 1;
                self.reload(id);
                self.focus_pane(self.active, window, cx);
                self.persist();
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
            "location" => self.prompt(
                PromptKind::Location,
                "Go to directory",
                "Enter an absolute path or a path relative to this panel.".into(),
                self.cwd(self.active).to_string_lossy().into_owned(),
                window,
                cx,
            ),
            "mkdir" => self.prompt(
                PromptKind::Mkdir,
                "Make directory",
                self.cwd(self.active).display().to_string(),
                String::new(),
                window,
                cx,
            ),
            "new-file" => self.prompt(
                PromptKind::NewFile,
                "Open / Create File",
                self.cwd(self.active).display().to_string(),
                String::new(),
                window,
                cx,
            ),
            "rename" if self.active_document().is_none() => {
                if let Some(e) = self
                    .directory(self.active)
                    .and_then(Directory::focused)
                    .filter(|e| !e.parent)
                    .cloned()
                {
                    self.prompt(
                        PromptKind::Rename(e.path),
                        "Rename",
                        e.name.clone(),
                        e.name,
                        window,
                        cx,
                    );
                }
            }
            "rename" | "save" => {
                if let Some(id) = self.active_document() {
                    self.save(id, cx);
                }
            }
            "view" | "edit" | "quick-view" | "quick-edit" => {
                if let Some(e) = self
                    .directory(self.active)
                    .and_then(Directory::focused)
                    .filter(|e| !e.directory)
                    .cloned()
                {
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
                                DirectoryBrowser::new(id, cwd),
                            )));
                            self.panes[pane].active = 0;
                            self.reload(id);
                        }
                    }
                    self.open_document(
                        pane,
                        e.path,
                        id.ends_with("view") || id == "view",
                        window,
                        cx,
                    );
                }
            }
            "copy" | "move" | "trash" => {
                if self.operation.is_some() {
                    self.error("Wait for the current operation to finish", cx);
                    return;
                }
                let paths = self
                    .directory(self.active)
                    .map(Directory::selected)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|e| e.path)
                    .collect::<Vec<_>>();
                if paths.is_empty() {
                    return;
                }
                let title = if id == "copy" {
                    "Copy"
                } else if id == "move" {
                    "Move"
                } else {
                    "Move to Trash"
                };
                let desc = format!(
                    "{} item(s) from {}",
                    paths.len(),
                    self.cwd(self.active).display()
                );
                let kind = if id == "trash" {
                    PromptKind::Trash(paths)
                } else {
                    PromptKind::Transfer(
                        if id == "copy" {
                            OperationKind::Copy
                        } else {
                            OperationKind::Move
                        },
                        paths,
                    )
                };
                self.prompt(
                    kind,
                    title,
                    desc,
                    self.cwd(1 - self.active).display().to_string(),
                    window,
                    cx,
                );
            }
            "sync" => {
                let path = self.cwd(self.active);
                self.navigate(1 - self.active, path, window, cx);
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
                if self.modal.is_some() {
                    self.dismiss(window, cx);
                } else if self.active_document().is_some() {
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
        let index = if let Some(index) = self.panes[pane].tabs.iter().position(|tab| tab.preview) {
            self.panes[pane].tabs[index].preview_source = Some(source_id);
            index
        } else {
            let id = self.id();
            let mut tab = Tab::new(TabContent::Pending {
                id,
                path: None,
                error: None,
            });
            tab.preview = true;
            tab.preview_source = Some(source_id);
            self.panes[pane].tabs.push(tab);
            self.panes[pane].tabs.len() - 1
        };
        self.panes[pane].active = index;
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
        self.documents.retain(|id, _| used.contains(id));
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
        let existing = if let TabContent::Document(id) = &content {
            self.panes[pane]
                .tabs
                .iter()
                .position(|tab| matches!(tab.content, TabContent::Document(doc) if doc == *id))
        } else {
            None
        };
        let index = if let Some(index) = existing {
            self.panes[pane].tabs[index].make_permanent();
            index
        } else {
            self.panes[pane].tabs.push(Tab::new(content));
            self.panes[pane].tabs.len() - 1
        };
        self.panes[pane].active = index;
        self.focus_pane(pane, window, cx);
    }
    pub fn open_document(
        &mut self,
        pane: usize,
        path: PathBuf,
        readonly: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_document_into(pane, path, readonly, None, window, cx);
    }
    fn open_document_into(
        &mut self,
        pane: usize,
        path: PathBuf,
        readonly: bool,
        tab: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = path.canonicalize().unwrap_or(path);
        if readonly {
            let viewer = self
                .extensions
                .iter()
                .flat_map(|e| &e.contributions)
                .filter_map(|c| c.viewer.as_ref())
                .filter_map(|v| {
                    MediaFilter::new(&v.patterns)
                        .ok()
                        .filter(|f| f.matches(&path))
                        .map(|f| (v.priority, f))
                })
                .max_by_key(|(priority, _)| *priority);
            if let Some((_, filter)) = viewer {
                if tab.is_none() {
                    self.open_request[pane] += 1;
                }
                let id = tab.unwrap_or_else(|| self.id());
                let source = if tab.is_some() { 1 - pane } else { self.active };
                let sort = self.directory(source).map(|d| d.sort).unwrap_or_default();
                let source_directory = self
                    .directory(source)
                    .filter(|d| d.path.canonicalize().ok().as_deref() == path.parent())
                    .map(|d| (d.id, d.path.clone()));
                let view = cx.new(|cx| {
                    MediaViewer::new(
                        path.clone(),
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
                        MediaEvent::Close => this.request_close_tab(pane, id, window, cx),
                        MediaEvent::Changed => {
                            let new_path = view.read(cx).path.clone();
                            let following = this.panes[pane]
                                .tabs
                                .iter_mut()
                                .find(|t| t.id() == id)
                                .is_some_and(|target| {
                                    if let TabContent::Media { path, .. } = &mut target.content {
                                        *path = new_path.clone();
                                    }
                                    target.preview
                                });
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
                            if following
                                && let Some(target) = this.panes[pane]
                                    .tabs
                                    .iter_mut()
                                    .find(|target| target.id() == id)
                            {
                                // Keep the source spelling (e.g. /tmp versus /private/tmp).
                                // Decoder status changes must not restart the same preview.
                                target.preview_path = selected_path.or(Some(new_path));
                            }
                            if this.active == pane && this.modal.is_none() {
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
            && matches!(
                path.extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_lowercase()
                    .as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico"
            )
        {
            if tab.is_none() {
                self.open_request[pane] += 1;
            }
            let id = self.id();
            self.install_content(pane, TabContent::Image { id, path }, tab, window, cx);
            return;
        }
        if let Some(doc) = self.documents.values_mut().find(|d| d.path == path) {
            let id = doc.id;
            if !readonly {
                doc.readonly = false;
            }
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
        if tab.is_none() {
            self.status = format!("Opening {}…", path.display());
        }
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<Vec<u8>> {
                if std::fs::metadata(&path)?.len() > 16 * 1024 * 1024 {
                    anyhow::bail!(
                        "Files over 16 MiB require the streaming viewer (not yet ported)"
                    );
                }
                Ok(std::fs::read(&path)?)
            })();
            let _ = tx.send(Message::Opened {
                request,
                pane,
                path,
                readonly,
                tab,
                result,
            });
        });
    }
    pub fn save(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(d) = self.documents.get_mut(&id) {
            if d.readonly || d.saving {
                return;
            }
            d.saving = true;
            d.error = None;
            let (path, expected, text) = (
                d.path.clone(),
                d.saved.clone(),
                d.input.read(cx).value().to_string(),
            );
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let result = save_document(&path, &expected, &text);
                let _ = tx.send(Message::Saved { id, text, result });
            });
            cx.notify();
        }
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
                if let TabContent::Document(id) = tab.content {
                    self.documents.get(&id)
                } else {
                    None
                }
            });
        if let Some(doc) = document {
            if doc.saving {
                self.error("Wait for the save to finish", cx);
                return;
            }
            if doc.dirty(cx) {
                self.prompt(
                    PromptKind::CloseTab { pane, id },
                    "Unsaved changes",
                    format!("Discard changes to {}?", doc.path.display()),
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
        let cwd = self.cwd(pane);
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
            p.tabs.remove(i);
            if p.active > i {
                p.active -= 1;
            }
            p.active = p.active.min(p.tabs.len().saturating_sub(1));
        }
        self.release_unused_documents();
        if let Some(w) = &self.watcher {
            w.remove(&id.to_string());
        }
        if self.panes[pane].tabs.is_empty() {
            let id = self.id();
            self.panes[pane]
                .tabs
                .push(Tab::new(TabContent::Directory(DirectoryBrowser::new(
                    id, cwd,
                ))));
            self.reload(id);
        }
        self.focus_pane(pane, window, cx);
        self.persist();
    }
    pub fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.modal, Some(Modal::Conflict { .. })) {
            self.error("Resolve or cancel the file conflict before closing", cx);
            return;
        }
        if self.documents.values().any(|d| d.saving) {
            self.error("Wait for pending saves before closing", cx);
            return;
        }
        if self.documents.values().any(|d| d.dirty(cx)) || self.operation.is_some() {
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
        let paths = |p: &Pane| {
            p.tabs
                .iter()
                .filter_map(|t| {
                    if let TabContent::Directory(d) = &t.content {
                        Some(d.current().path.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        self.settings.left = paths(&self.panes[0]);
        self.settings.right = paths(&self.panes[1]);
        for (i, pane) in self.panes.iter().enumerate() {
            let index = pane
                .tabs
                .iter()
                .take(pane.active)
                .filter(|t| matches!(&t.content, TabContent::Directory(_)))
                .count();
            if i == 0 {
                self.settings.left_active = index;
            } else {
                self.settings.right_active = index;
            }
        }
        self.settings.active_pane = self.active;
        if let Err(e) = self.settings.save(&self.settings_path) {
            eprintln!("Cannot save session: {e}");
        }
    }
}

#[cfg(test)]
mod preview_tests {
    use super::{Pane, Tab, TabContent};
    use std::path::PathBuf;

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
    }

    #[test]
    fn preview_tracks_only_its_source_and_preserves_identity_across_files() {
        let mut tab = preview();
        let first = Some(PathBuf::from("/tmp/first.txt"));
        assert!(tab.follows_changed_file(3, &first));
        assert!(!tab.follows_changed_file(4, &first));
        tab.begin_preview_load(first.clone(), 20);
        tab.content = TabContent::Document(30);
        assert!(tab.preview);
        assert_eq!(tab.id(), 10);
        assert!(!tab.follows_changed_file(3, &first));
        let next = Some(PathBuf::from("/tmp/next.png"));
        assert!(tab.follows_changed_file(3, &next));
        tab.begin_preview_load(next.clone(), 21);
        tab.content = TabContent::Image {
            id: 31,
            path: next.unwrap(),
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
        let file = Some(PathBuf::from("/tmp/first.txt"));
        firm.begin_preview_load(file.clone(), 20);
        firm.make_permanent();
        assert!(!firm.preview);
        assert_eq!(firm.preview_source, None);
        assert_eq!(firm.preview_path, file);
        assert!(firm.accepts_load(20));
        assert!(!firm.follows_changed_file(3, &Some(PathBuf::from("/tmp/next.txt"))));
        // A new preview has independent request state; it cannot cancel the firm tab.
        let mut next = preview();
        next.begin_preview_load(Some(PathBuf::from("/tmp/next.txt")), 21);
        assert!(firm.accepts_load(20));
        assert!(!firm.accepts_load(21));
        assert!(next.preview);
    }
}
