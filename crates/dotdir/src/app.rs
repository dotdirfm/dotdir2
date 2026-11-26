use crate::{
    archive::{Archive, Location, SavedLocation, is_zip},
    column_scroll::ColumnScroll,
    commands::{COMMANDS, CommandState, RunCommand},
    extension_logs::ExtensionLogs,
    hover::HoverHints,
    icon_theme::IconTheme,
    lsp::{self, LanguageServers},
    media::{MediaEvent, MediaFilter, MediaViewer},
    model::*,
    operations::*,
    syntax::language_for,
    terminal::Terminal,
    theme::Palette,
    vscode,
};
use dotdir_core::{
    copy::{CancelToken, ConflictResolution, CopyEvent, CopyOptions},
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
    cell::Cell,
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

const VSCODE_RUNNING: &str = "Running VS Code extension…";

pub(crate) enum Message {
    IconTheme(u64, anyhow::Result<Box<IconTheme>>),
    Lsp(lsp::Notice),
    Directory(u64, u64, anyhow::Result<Vec<Entry>>),
    Watch(u64),
    Opened {
        request: u64,
        pane: usize,
        path: Location,
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
                    let location = if p.is_dir() {
                        p.into()
                    } else if p.is_file() && is_zip(&p) {
                        Archive::mount(p)
                    } else {
                        anyhow::bail!("Directory or ZIP archive does not exist");
                    };
                    self.navigate(self.active, location, window, cx);
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
                    if value.trim().is_empty() {
                        anyhow::bail!("Enter a local destination directory");
                    }
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
                PromptKind::Extract(archive, members) => {
                    if value.trim().is_empty() {
                        anyhow::bail!("Enter a local destination directory");
                    }
                    let destination = self.resolve_path(value.trim());
                    if !destination.is_dir() {
                        anyhow::bail!("Destination directory does not exist");
                    }
                    let id = self.id();
                    self.operation = Some(Operation::extract(
                        id,
                        archive,
                        members,
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
        let vscode_available =
            self.vscode_document().is_some() && self.vscode_host.pending.is_none();
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
            .map(|c| {
                (
                    c.id.to_string(),
                    c.title(&state).to_string(),
                    c.key.to_string(),
                )
            })
            .chain(self.extensions.iter().flat_map(|e| {
                e.contributions
                    .iter()
                    .filter(|c| c.viewer.is_none())
                    .map(|c| (c.id.clone(), c.title.clone(), "Extension".into()))
            }))
            .chain(
                self.vscode_commands()
                    .filter(|_| vscode_available)
                    .map(|(_, command)| {
                        (command.id.clone(), command.title.clone(), "VS Code".into())
                    }),
            )
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
            archive: self.archive_active(self.active),
            entry: entry.is_some(),
            real_entry: entry.is_some_and(|entry| !entry.parent),
            file: entry.is_some_and(|entry| !entry.directory && !entry.parent),
            items: directory.is_some_and(|d| d.entries.iter().any(|e| !e.parent)),
            selection: directory
                .is_some_and(|d| !d.marked.is_empty() || d.focused().is_some_and(|e| !e.parent)),
            parent: directory.is_some_and(|d| d.path.parent().is_some()),
            can_save: self.panels_visible && document.is_some_and(|d| !d.readonly && !d.saving),
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
            return self.vscode_document().is_some()
                && self.vscode_host.pending.is_none()
                && self.vscode_commands().any(|(_, command)| command.id == id);
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
        let mut roots = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../extensions")];
        if let Some(bundle) = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.join("../Resources/extensions")))
        {
            roots.push(bundle);
        }
        if let Some(base) = self.settings_path.parent() {
            let directory = base.join("extensions");
            self.vscode_extensions = vscode::discover(&directory);
            self.reset_vscode_host();
            roots.push(directory);
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

    fn vscode_commands(
        &self,
    ) -> impl Iterator<Item = (&vscode::Extension, &vscode::ExtensionCommand)> {
        self.vscode_extensions
            .iter()
            .filter(|extension| self.settings.vscode.enabled(&extension.id))
            .flat_map(|extension| {
                extension
                    .commands
                    .iter()
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
                let path = document.path.local().filter(|_| !document.readonly)?;
                let desired = vscode::language_extension(
                    &self.vscode_extensions,
                    &self.settings.vscode,
                    path,
                )
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

    fn reset_vscode_host(&mut self) {
        self.vscode_host.reset();
        if self.status == VSCODE_RUNNING {
            self.status.clear();
        }
    }

    fn invoke_vscode(&mut self, command_id: &str, cx: &mut Context<Self>) {
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
        let snapshot = vscode::Snapshot::new(
            document.path.local().expect("local editor"),
            document.version.get(),
            &text,
            editor.selected_range(),
            editor.is_selection_reversed(),
        );
        let document_id = document.id;
        let invocation = extension.invocation(command, &self.settings.vscode, id, snapshot);
        match self
            .vscode_host
            .run(&self.settings.vscode, invocation, document_id, text)
        {
            Ok(()) => self.status = VSCODE_RUNNING.into(),
            Err(error) => {
                self.extension_logs
                    .append("VS Code commands", &format!("{error:#}"));
                self.error(error, cx);
            }
        }
        cx.notify();
    }

    fn handle_vscode(&mut self, event: vscode::Event, window: &mut Window, cx: &mut Context<Self>) {
        if !self.vscode_host.is_current(event.process) {
            return;
        }
        let reply = match event.reply {
            Ok(reply) => reply,
            Err(error) => {
                self.extension_logs
                    .append("VS Code commands", &format!("{error:#}"));
                let running = self.vscode_host.pending.is_some();
                self.vscode_host.reset();
                if running {
                    self.error(error, cx);
                }
                return;
            }
        };
        match reply {
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
                if self
                    .vscode_host
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.id == id)
                {
                    self.vscode_host.pending = None;
                    if let Some(error) = error {
                        self.extension_logs.append("VS Code commands", &error);
                        self.error(format!("VS Code extension: {error}"), cx);
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
            let unchanged = editor
                .text()
                .slice(batch.range.clone())
                .bytes()
                .eq(batch.text.bytes());
            if !unchanged {
                let selection = editor.selected_range();
                let reversed = editor.is_selection_reversed();
                let translated = batch.translate(selection.start)..batch.translate(selection.end);
                editor.unmark_text(window, cx);
                editor.set_selected_range(batch.range, cx);
                editor.replace(batch.text, window, cx);
                editor.set_selected_range_with_direction(translated, reversed, cx);
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
            let path = self.resolve_path(path);
            if path.is_dir() {
                self.navigate(self.active, path.into(), window, cx);
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
        if self.vscode_host.expire() {
            self.extension_logs
                .append("VS Code commands", "VS Code extension command timed out");
            self.error("VS Code extension command timed out", cx);
        }
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
                Message::Lsp(notice) => match notice {
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
                            doc.input.update(cx, |state, cx| {
                                let text = state.text().clone();
                                if let Some(diagnostics) = state.diagnostics_mut() {
                                    diagnostics.reset(&text);
                                    diagnostics.extend(items);
                                    cx.notify();
                                }
                            });
                        }
                    }
                    lsp::Notice::Failed { session, error } => {
                        for doc in self.documents.values_mut().filter(|doc| {
                            doc.lsp
                                .as_ref()
                                .is_some_and(|buffer| buffer.session() == session)
                        }) {
                            doc.clear_language_server(Some(error.clone()), cx);
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
                Message::Opened {
                    request,
                    pane,
                    path,
                    readonly,
                    tab,
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
                            } else {
                                self.error(e, cx);
                            }
                        }
                        Ok(bytes) => {
                            if readonly
                                && path.local().is_none()
                                && let Some(format) = image_format(&path)
                            {
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
                                continue;
                            }
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
                                if !readonly && std::str::from_utf8(&document.saved).is_ok() {
                                    document.readonly = false;
                                }
                                document.id
                            } else {
                                let id = self.id();
                                let language = language_for(path.name_path());
                                let input = cx.new(|cx| {
                                    EditorState::new(window, cx)
                                        .language(language)
                                        .default_value(text)
                                });
                                let subscription =
                                    cx.subscribe_in(&input, window, move |this, input, event: &InputEvent, window, cx| {
                                        if matches!(event, InputEvent::Change)
                                            && let Some(doc) = this.documents.get(&id)
                                        {
                                            doc.version.set(doc.version.get() + 1);
                                            if let Some(view) = &doc.inactive_view {
                                                let text = input.read(cx).value();
                                                view.update(cx, |editor, cx| {
                                                    let scroll = editor.scroll_offset();
                                                    editor.set_value(text, window, cx);
                                                    editor.set_scroll_offset(scroll, cx);
                                                });
                                            }
                                            if let Some(buffer) = &doc.lsp {
                                                buffer.change(input.read(cx).text().clone());
                                            }
                                            if this.hover.is_for_document(id) && this.hover.clear() {
                                                cx.notify();
                                            }
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
                                        inactive_view: None,
                                        saved: bytes,
                                        version: Cell::new(1),
                                        dirty_cache: Cell::new(None),
                                        readonly,
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
                Message::Saved { id, text, result } => {
                    if let Some(d) = self.documents.get_mut(&id) {
                        d.saving = false;
                        match result {
                            Ok(()) => {
                                if let Some(buffer) = &d.lsp {
                                    buffer.save();
                                }
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
                    Err(e) => {
                        let error = e.to_string();
                        self.extension_logs.append("DotDir extensions", &error);
                        self.extension_errors.push(error);
                    }
                },
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
        let logs_changed =
            matches!(self.modal, Some(Modal::ExtensionLogs)) && self.extension_logs.take_changed();
        if logs_changed
            && self.extension_log_scroll.offset().y + self.extension_log_scroll.max_offset().y
                <= px(4.)
        {
            self.extension_log_scroll.scroll_to_bottom();
        }
        if changed || logs_changed {
            cx.notify();
            window.refresh();
        }
    }
}
pub(crate) struct Document {
    pub id: u64,
    pub path: Location,
    pub input: Entity<EditorState>,
    pub inactive_view: Option<Entity<EditorState>>,
    pub saved: Vec<u8>,
    version: Cell<u64>,
    // Invalidated by text changes and successful saves, never by cursor movement
    // or redraws. Keep undo-to-saved detection without allocating the full text.
    dirty_cache: Cell<Option<bool>>,
    pub readonly: bool,
    pub saving: bool,
    pub error: Option<String>,
    pub lsp: Option<Rc<lsp::Buffer>>,
    pub lsp_status: Option<String>,
    pub _subscription: Subscription,
}
impl Document {
    fn clear_language_server(&mut self, status: Option<String>, cx: &mut App) {
        self.lsp = None;
        self.lsp_status = status;
        self.input.update(cx, |state, cx| {
            let lsp = state.lsp_mut();
            lsp.completion_provider = None;
            lsp.hover_provider = None;
            lsp.definition_provider = None;
            lsp.show_document = None;
            if let Some(diagnostics) = state.diagnostics_mut() {
                diagnostics.clear();
            }
            cx.notify();
        });
    }

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
        path: Option<Location>,
        error: Option<String>,
    },
    Directory(DirectoryBrowser),
    Document(u64),
    Image {
        id: u64,
        path: Location,
        image: Option<Arc<Image>>,
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
    preview_path: Option<Location>,
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
impl Pane {
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
    Location,
    Mkdir,
    Rename(PathBuf),
    NewFile,
    Transfer(OperationKind, Vec<PathBuf>),
    Extract(Arc<Archive>, Vec<PathBuf>),
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
    ExtensionLogs,
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
    pub vscode_extensions: Vec<vscode::Extension>,
    vscode_host: vscode::Host,
    language_servers: LanguageServers,
    pub hover: HoverHints,
    source_jump: [Option<(u64, lsp_types::Position)>; 2],
    pub extension_errors: Vec<String>,
    pub extension_logs: ExtensionLogs,
    pub extension_log_scroll: ScrollHandle,
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
        if let Some(path) = initial {
            if path.is_dir() {
                settings.left = vec![SavedLocation::Local(path.clone())];
                settings.right = vec![SavedLocation::Local(path)];
            } else if path.is_file() && is_zip(&path) {
                let path = path.canonicalize().unwrap_or(path);
                if let Some(parent) = path.parent() {
                    settings.right = vec![SavedLocation::Local(parent.into())];
                }
                settings.left = vec![SavedLocation::from(&Archive::mount(path))];
            }
        }
        let palette = Palette::named(&settings.theme);
        palette.apply(&settings.theme, cx);
        let (tx, rx) = mpsc::channel();
        let lsp_tx = tx.clone();
        let extension_logs = ExtensionLogs::default();
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
                    let d = DirectoryBrowser::new(next_id, path.into());
                    next_id += 1;
                    Tab::new(TabContent::Directory(d))
                })
                .collect(),
            active: 0,
        });
        let cache = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("dev.dotdir.native");
        let host = ExtensionHost::new(cache.join("quickjs"));
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
            vscode_extensions: vec![],
            vscode_host,
            language_servers,
            hover: HoverHints::default(),
            source_jump: [None, None],
            extension_errors: vec![],
            extension_logs,
            extension_log_scroll: ScrollHandle::new(),
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
    fn pane_location(&self, pane: usize) -> Location {
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
            Some(TabContent::Image { path, .. }) => {
                path.parent().unwrap_or_else(|| PathBuf::new().into())
            }
            Some(TabContent::Media { path, .. }) => path
                .parent()
                .map(Location::from)
                .unwrap_or_else(|| PathBuf::new().into()),
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
    pub fn archive_active(&self, pane: usize) -> bool {
        self.pane_location(pane).local().is_none()
    }
    pub fn cwd(&self, pane: usize) -> PathBuf {
        let location = self.pane_location(pane);
        if let Some(local) = location.local() {
            local.into()
        } else {
            location
                .host_path()
                .parent()
                .map(PathBuf::from)
                .unwrap_or_default()
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
                    read_location(&path, hidden, sort),
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
                .inactive_view
                .as_ref()
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

        let (tab, load) = if duplicate {
            self.duplicate_tab(source, index)
        } else {
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
            (tab, None)
        };

        let tab_id = tab.id();
        let directories = if duplicate {
            match &tab.content {
                TabContent::Directory(browser) => {
                    browser.columns.iter().map(|column| column.id).collect()
                }
                _ => Vec::new(),
            }
        } else {
            Vec::new()
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

    fn duplicate_tab(&mut self, pane: usize, index: usize) -> (Tab, Option<Location>) {
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
                let path = Some(Location::from(path.clone()));
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
        let view = doc.inactive_view.get_or_insert_with(|| {
            let text = doc.input.read(cx).value();
            let language = language_for(doc.path.name_path());
            cx.new(|cx| {
                EditorState::new(window, cx)
                    .language(language)
                    .default_value(text)
            })
        });
        (view.clone(), true)
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
                let path = self.pane_location(self.active);
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
                "Enter a directory or ZIP archive path.".into(),
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
                                DirectoryBrowser::new(id, cwd.into()),
                            )));
                            self.panes[pane].active = 0;
                            self.reload(id);
                        }
                    }
                    if id == "view"
                        && let Some(path) = e.path.local().filter(|path| is_zip(path))
                    {
                        self.navigate(
                            pane,
                            Archive::mount(path.canonicalize().unwrap_or_else(|_| path.into())),
                            window,
                            cx,
                        );
                    } else {
                        self.open_document_into(pane, e.path, id == "view", None, window, cx);
                    }
                }
            }
            "copy" | "move" | "trash" => {
                if self.operation.is_some() {
                    self.error("Wait for the current operation to finish", cx);
                    return;
                }
                let Some(directory) = self.directory(self.active) else {
                    return;
                };
                let selected = directory.selected();
                let description = format!(
                    "{} item(s) from {}",
                    selected.len(),
                    directory.path.display()
                );
                let destination = if self.archive_active(1 - self.active) {
                    String::new()
                } else {
                    self.cwd(1 - self.active).display().to_string()
                };
                if let Location::Zip { archive, .. } = &directory.path {
                    let members = selected
                        .into_iter()
                        .filter_map(|entry| match entry.path {
                            Location::Zip { member, .. } => Some(member),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    if !members.is_empty() {
                        self.prompt(
                            PromptKind::Extract(archive.clone(), members),
                            "Extract ZIP",
                            description,
                            destination,
                            window,
                            cx,
                        );
                    }
                } else {
                    let paths = selected
                        .into_iter()
                        .filter_map(|entry| entry.path.local().map(PathBuf::from))
                        .collect::<Vec<_>>();
                    if paths.is_empty() {
                        return;
                    }
                    let (kind, title) = match id {
                        "trash" => (PromptKind::Trash(paths), "Move to Trash"),
                        "move" => (PromptKind::Transfer(OperationKind::Move, paths), "Move"),
                        _ => (PromptKind::Transfer(OperationKind::Copy, paths), "Copy"),
                    };
                    self.prompt(kind, title, description, destination, window, cx);
                }
            }
            "sync" => {
                if let Some(path) = self.directory(self.active).map(|d| d.path.clone()) {
                    self.navigate(1 - self.active, path, window, cx);
                }
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
            id if id.starts_with("vscode:") => self.invoke_vscode(id, cx),
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
        if let Some(index) = self.panes[pane].tabs.iter().position(|tab| tab.preview) {
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
        } else if let Some(path) = entry.path.local().filter(|path| is_zip(path)) {
            self.navigate(
                pane,
                Archive::mount(path.canonicalize().unwrap_or_else(|_| path.into())),
                window,
                cx,
            );
        } else if let Some(path) = entry.path.local().filter(|_| external) {
            let path = path.to_path_buf();
            std::thread::spawn(move || {
                let _ = open::that(path);
            });
        } else {
            self.open_document_into(pane, entry.path, false, None, window, cx);
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
        let path = match path {
            Location::Local(path) => Location::Local(path.canonicalize().unwrap_or(path)),
            archive => archive,
        };
        let readonly = readonly || path.local().is_none();
        if readonly && let Some(local) = path.local() {
            let viewer = self
                .extensions
                .iter()
                .flat_map(|e| &e.contributions)
                .filter_map(|c| c.viewer.as_ref())
                .filter_map(|v| {
                    MediaFilter::new(&v.patterns)
                        .ok()
                        .filter(|f| f.matches(local))
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
                    .filter(|d| {
                        d.path
                            .local()
                            .and_then(|p| p.canonicalize().ok())
                            .as_deref()
                            == local.parent()
                    })
                    .map(|d| (d.id, d.path.clone()));
                let view = cx.new(|cx| {
                    MediaViewer::new(
                        local.to_path_buf(),
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
                                    selected_path.or(Some(new_path.into()));
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
                        path: local.to_path_buf(),
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
        if readonly && path.local().is_some() && image_format(&path).is_some() {
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
        if let Some(doc) = self.documents.values_mut().find(|d| d.path == path) {
            let id = doc.id;
            if !readonly && std::str::from_utf8(&doc.saved).is_ok() {
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
        if tab.is_none() {
            self.status = format!("Opening {}…", path.display());
        }
        std::thread::spawn(move || {
            let result = path.read_preview();
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
    fn enable_language_server(&mut self, id: u64, cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        let Some(doc) = self
            .documents
            .get(&id)
            .filter(|doc| !doc.readonly && doc.lsp.is_none())
        else {
            return;
        };
        let Some(path) = doc.path.local().map(PathBuf::from) else {
            return;
        };
        let text = doc.input.read(cx).text().clone();
        let Some((extension, language)) =
            vscode::language_extension(&self.vscode_extensions, &self.settings.vscode, &path)
        else {
            return;
        };
        let Some(buffer) = self.language_servers.attach_extension(
            id,
            &path,
            text,
            language,
            extension,
            &self.settings.vscode,
        ) else {
            return;
        };
        let doc = self.documents.get_mut(&id).unwrap();
        doc.lsp_status = Some(format!("Starting {}…", buffer.server_name()));
        doc.lsp = Some(buffer.clone());
        let provider = Rc::new(lsp::EditorProvider(buffer));
        doc.input.update(cx, |state, _| {
            let lsp = state.lsp_mut();
            lsp.completion_provider = Some(provider.clone());
            lsp.definition_provider = Some(provider);
            lsp.show_document = Some(Rc::new(move |params, window, cx| {
                let Some(destination) = lsp::uri_to_path(&params.uri) else {
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
                        this.open_source_location(pane, destination, target, window, cx);
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
        let path = path.canonicalize().unwrap_or(path);
        self.open_document(pane, path.clone(), false, window, cx);
        if let Some(position) = position {
            let selected = &self.panes[pane].tabs[self.panes[pane].active];
            if let TabContent::Document(id) = selected.content
                && self.documents[&id].path.local() == Some(path.as_path())
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
                editor.set_cursor_position(position, window, cx);
            });
        }
    }

    pub fn save(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(d) = self.documents.get_mut(&id) {
            if d.readonly || d.saving {
                return;
            }
            let Some(path) = d.path.local().map(PathBuf::from) else {
                return;
            };
            d.saving = true;
            d.error = None;
            let (path, expected, text) =
                (path, d.saved.clone(), d.input.read(cx).value().to_string());
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
            p.remove_tab(i);
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
                        Some(SavedLocation::from(&d.current().path))
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

fn image_format(path: &Location) -> Option<ImageFormat> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some(ImageFormat::Png),
        "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
        "gif" => Some(ImageFormat::Gif),
        "webp" => Some(ImageFormat::Webp),
        "bmp" => Some(ImageFormat::Bmp),
        "ico" => Some(ImageFormat::Ico),
        _ => None,
    }
}

#[cfg(test)]
mod tab_tests {
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
