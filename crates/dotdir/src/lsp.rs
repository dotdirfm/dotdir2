use crate::{
    archive::Location,
    archive_typescript::{self, OpenFiles, Workspace},
    extension_logs::{ExtensionLogs, LogSource},
    filesystems::Filesystems,
    vscode,
};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use gpui_kit::{
    App, Entity, SharedString, Task, Window,
    base::input::DiagnosticSet,
    component::input::{
        CodeActionProvider, CompletionProvider, DefinitionProvider, EditorState, Rope, RopeExt,
    },
};
use lsp_types::{
    CodeAction, CompletionContext, CompletionResponse, CompletionTextEdit, Diagnostic,
    GotoDefinitionResponse, Hover, LocationLink, Position, Range,
};
use serde_json::{Value, json};
use std::{
    cell::{Cell, Ref, RefCell},
    collections::{HashMap, HashSet},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    rc::Rc,
    sync::{
        Arc, RwLock, Weak,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use url::Url;

const DEBOUNCE: Duration = Duration::from_millis(250);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(30);

pub enum Notice {
    CommandsChanged,
    QuickPick {
        session: u64,
        request: vscode::QuickPick,
        reply: mpsc::Sender<usize>,
    },
    QuickPickClosed {
        session: u64,
    },
    Ready {
        session: u64,
        document: u64,
    },
    Diagnostics {
        session: u64,
        document: u64,
        revision: i32,
        items: Vec<Diagnostic>,
    },
    Failed {
        session: u64,
        error: String,
    },
    CodeActionFailed {
        session: u64,
        document: u64,
        error: String,
    },
}

type Notify = Arc<dyn Fn(Notice) + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Project {
    root: PathBuf,
    detached: Option<PathBuf>,
}

impl Project {
    fn for_extension(path: &Path, extension: &vscode::Extension) -> Option<Self> {
        let parent = path.parent()?;
        let mut markers = extension.workspace_markers().peekable();
        let workspace = if markers.peek().is_some() {
            parent.ancestors().find(|dir| {
                extension
                    .workspace_markers()
                    .any(|marker| dir.join(marker).exists())
            })
        } else {
            Some(
                parent
                    .ancestors()
                    .find(|dir| dir.join(".git").exists())
                    .unwrap_or(parent),
            )
        };
        Some(Self {
            root: workspace.unwrap_or(parent).into(),
            detached: workspace.is_none().then(|| path.into()),
        })
    }
}

pub(crate) struct Launch {
    pub name: String,
    pub script: &'static str,
    pub command: Box<dyn FnOnce() -> Result<Command> + Send>,
    pub options: Value,
}

pub struct LanguageServers {
    config: PathBuf,
    logs: ExtensionLogs,
    sessions: HashMap<(String, Project), Weak<Connection>>,
    archive_sessions: HashMap<Location, Weak<Connection>>,
    archive_files: OpenFiles,
    next_session: u64,
    notify: Notify,
}

impl LanguageServers {
    pub fn new(
        config: PathBuf,
        logs: ExtensionLogs,
        notify: impl Fn(Notice) + Send + Sync + 'static,
    ) -> Self {
        Self {
            config,
            logs,
            sessions: HashMap::new(),
            archive_sessions: HashMap::new(),
            archive_files: OpenFiles::default(),
            next_session: 0,
            notify: Arc::new(notify),
        }
    }

    pub fn forget_extension_sessions(&mut self) {
        self.sessions.clear();
        self.archive_sessions.clear();
    }

    pub fn archive_text(&mut self, path: &Location, text: Option<Rope>) {
        let Some(root) = path.archive_root() else {
            return;
        };
        let mut files = self.archive_files.write().unwrap();
        let changed = match text {
            Some(text) => {
                files.insert(path.clone(), text);
                true
            }
            None => files.remove(path).is_some(),
        };
        drop(files);
        if changed
            && let Some(connection) = self.archive_sessions.get(&root).and_then(Weak::upgrade)
            && let Some(file) = connection
                .archive
                .as_ref()
                .and_then(|workspace| workspace.file_path(path))
        {
            let _ = connection.send(Event::ArchiveChanged(file));
        }
    }

    pub fn attach_archive(
        &mut self,
        id: u64,
        path: &Location,
        text: Rope,
        extension: &vscode::Extension,
        language: &str,
        settings: &vscode::Settings,
        filesystems: Filesystems,
    ) -> Result<Rc<Buffer>> {
        let root = path.archive_root().context("Expected an archive member")?;
        self.archive_sessions
            .retain(|_, weak| weak.strong_count() > 0);
        self.archive_files
            .write()
            .unwrap()
            .insert(path.clone(), text.clone());
        let connection = if let Some(active) = self
            .archive_sessions
            .get(&root)
            .and_then(Weak::upgrade)
            .filter(|connection| connection.running.load(Ordering::Relaxed))
        {
            active
        } else {
            self.next_session += 1;
            let workspace = Arc::new(Workspace::new(
                root.clone(),
                &self.config,
                self.next_session,
                filesystems,
                self.archive_files.clone(),
            )?);
            let project = Project {
                root: workspace.root.clone(),
                detached: None,
            };
            let connection = Connection::start(
                self.next_session,
                extension.id.clone(),
                archive_typescript::launch(extension, settings),
                project,
                Some(workspace),
                self.logs.clone(),
                self.notify.clone(),
            );
            self.archive_sessions
                .insert(root, Arc::downgrade(&connection));
            connection
        };
        let file = connection
            .archive
            .as_ref()
            .and_then(|workspace| workspace.file_path(path))
            .and_then(|path| file_uri(&path))
            .context("Invalid archive document URI")?;
        Buffer::open(id, file, text, language, connection)
    }

    pub fn attach_extension(
        &mut self,
        id: u64,
        path: &Path,
        text: Rope,
        language: &str,
        extension: &vscode::Extension,
        settings: &vscode::Settings,
    ) -> Option<Rc<Buffer>> {
        let uri = file_uri(path)?;
        let project = Project::for_extension(path, extension)?;
        let key = (extension.id.clone(), project);
        self.sessions.retain(|_, weak| weak.strong_count() > 0);
        let active = self
            .sessions
            .get(&key)
            .and_then(Weak::upgrade)
            .filter(|connection| connection.running.load(Ordering::Relaxed));
        let connection = if let Some(active) = active {
            active
        } else {
            let launch = extension.language_launch(settings, &self.config);
            self.next_session += 1;
            let connection = Connection::start(
                self.next_session,
                key.0.clone(),
                launch,
                key.1.clone(),
                None,
                self.logs.clone(),
                self.notify.clone(),
            );
            self.sessions.insert(key, Arc::downgrade(&connection));
            connection
        };
        Buffer::open(id, uri, text, language, connection).ok()
    }
}

fn file_uri(path: &Path) -> Option<String> {
    Url::from_file_path(path).ok().map(Into::into)
}

pub fn uri_to_path(uri: &lsp_types::Uri) -> Option<PathBuf> {
    Url::parse(uri.as_str()).ok()?.to_file_path().ok()
}

pub struct Buffer {
    id: u64,
    uri: String,
    connection: Arc<Connection>,
    text: RefCell<Rope>,
    revision: Cell<i32>,
    diagnostics: RefCell<Option<DiagnosticSet>>,
    code_actions: RefCell<Option<ListedActions>>,
    action_generation: Cell<u64>,
    next_action_request: Cell<u64>,
}

struct ListedActions {
    range: std::ops::Range<usize>,
    actions: Vec<CodeAction>,
}

impl Buffer {
    fn open(
        id: u64,
        uri: String,
        text: Rope,
        language: &str,
        connection: Arc<Connection>,
    ) -> Result<Rc<Self>> {
        connection.send(Event::Open {
            id,
            uri: uri.clone(),
            language: language.into(),
            text: text.clone(),
        })?;
        Ok(Rc::new(Self {
            id,
            uri,
            connection,
            text: RefCell::new(text),
            revision: Cell::new(1),
            diagnostics: RefCell::new(None),
            code_actions: RefCell::new(None),
            action_generation: Cell::new(0),
            next_action_request: Cell::new(0),
        }))
    }

    pub fn source_location(&self, uri: &lsp_types::Uri) -> Option<Location> {
        let path = uri_to_path(uri)?;
        if let Some(workspace) = &self.connection.archive
            && path.starts_with(&workspace.root)
        {
            return workspace.location(&path);
        }
        Some(path.into())
    }

    pub fn session(&self) -> u64 {
        self.connection.id
    }

    pub fn server_name(&self) -> &str {
        &self.connection.name
    }

    pub fn extension_id(&self) -> &str {
        &self.connection.extension
    }

    pub fn revision(&self) -> i32 {
        self.revision.get()
    }

    pub(crate) fn diagnostics(&self) -> Ref<'_, Option<DiagnosticSet>> {
        self.diagnostics.borrow()
    }

    pub(crate) fn set_diagnostics(&self, items: Vec<Diagnostic>) {
        let diagnostics = (!items.is_empty()).then(|| {
            let mut diagnostics = DiagnosticSet::new(&self.text.borrow());
            diagnostics.extend(items);
            diagnostics
        });
        self.diagnostics.replace(diagnostics);
        self.invalidate_code_actions();
    }

    fn invalidate_code_actions(&self) {
        self.code_actions.borrow_mut().take();
        self.action_generation.set(self.action_generation.get() + 1);
    }

    pub(crate) fn action_generation(&self) -> u64 {
        self.action_generation.get()
    }

    pub(crate) fn has_code_actions(&self, range: &std::ops::Range<usize>) -> bool {
        self.code_actions
            .borrow()
            .as_ref()
            .is_some_and(|list| &list.range == range && !list.actions.is_empty())
    }

    pub fn change(&self, text: Rope) {
        let mut previous = self.text.borrow_mut();
        // Editor snapshots share rope chunks. Comparing their identity avoids
        // rescanning the document when a query and the change event see the same edit.
        let same_snapshot = previous.len() == text.len()
            && previous
                .chunks()
                .zip(text.chunks())
                .all(|(old, new)| std::ptr::eq(old, new));
        if !same_snapshot {
            let revision = self.revision.get() + 1;
            self.revision.set(revision);
            self.diagnostics.borrow_mut().take();
            self.invalidate_code_actions();
            *previous = text.clone();
            let _ = self.connection.send(Event::Change {
                id: self.id,
                revision,
                text,
            });
        }
    }

    pub fn save(&self) {
        let _ = self.connection.send(Event::Save(self.id));
    }

    pub fn command_available(&self, command: &str) -> bool {
        self.connection.running.load(Ordering::Relaxed)
            && self.connection.commands.read().unwrap().contains(command)
    }

    pub fn execute_command(
        &self,
        command: &str,
        text: &Rope,
        anchor: usize,
        active: usize,
    ) -> Result<async_channel::Receiver<Result<Value>>> {
        ensure!(
            self.command_available(command),
            "Command is no longer available"
        );
        self.change(text.clone());
        self.request(
            "dotdir/executeCommand",
            json!({
                "command": command,
                "textDocument": {"uri": self.uri},
                "anchor": wire_position(text, anchor),
                "active": wire_position(text, active),
            }),
        )
    }

    fn request(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<async_channel::Receiver<Result<Value>>> {
        let (reply, receiver) = async_channel::bounded(1);
        self.connection.send(Event::Query {
            id: self.id,
            method,
            params,
            reply,
        })?;
        Ok(receiver)
    }

    fn query(
        self: &Rc<Self>,
        text: &Rope,
        method: &'static str,
        params: Value,
        cx: &App,
    ) -> Task<Result<Value>> {
        self.change(text.clone());
        let revision = self.revision();
        let receiver = match self.request(method, params) {
            Ok(receiver) => receiver,
            Err(error) => return Task::ready(Err(error)),
        };
        let buffer = Rc::downgrade(self);
        cx.spawn(async move |_| {
            let value = receiver
                .recv()
                .await
                .context("Language server disconnected")??;
            ensure!(
                buffer.upgrade().is_some_and(|b| b.revision() == revision),
                "Language server result is for a previous revision"
            );
            Ok(value)
        })
    }

    pub fn hover(
        self: &Rc<Self>,
        text: &Rope,
        offset: usize,
        cx: &App,
    ) -> Task<Result<Option<Hover>>> {
        let request = self.query(
            text,
            "textDocument/hover",
            self.position_params(text, offset),
            cx,
        );
        cx.spawn(async move |_| Ok(serde_json::from_value(request.await?)?))
    }

    fn position_params(&self, text: &Rope, offset: usize) -> Value {
        json!({"textDocument": {"uri": self.uri}, "position": wire_position(text, offset)})
    }

    pub(crate) fn list_code_actions(
        self: &Rc<Self>,
        text: &Rope,
        range: std::ops::Range<usize>,
        cx: &App,
    ) -> Task<Result<Vec<CodeAction>>> {
        self.change(text.clone());
        if let Some(list) = self
            .code_actions
            .borrow()
            .as_ref()
            .filter(|list| list.range == range)
        {
            return Task::ready(Ok(list.actions.clone()));
        }
        let revision = self.revision();
        let generation = self.action_generation();
        let request_id = self.next_action_request.get() + 1;
        self.next_action_request.set(request_id);
        let request = self.query(
            text,
            "textDocument/codeAction",
            json!({
                "textDocument": {"uri": self.uri},
                "version": revision,
                "range": Range::new(wire_position(text, range.start), wire_position(text, range.end)),
            }),
            cx,
        );
        let buffer = Rc::downgrade(self);
        cx.spawn(async move |_| {
            let mut actions: Vec<CodeAction> =
                serde_json::from_value::<Option<_>>(request.await?)?.unwrap_or_default();
            let buffer = buffer.upgrade().context("Editor is closed")?;
            ensure!(
                buffer.action_generation() == generation,
                "Code actions expired"
            );
            actions.sort_by_key(|action| action.is_preferred != Some(true));
            for action in &mut actions {
                action.data = Some(json!({"revision": revision, "wire": action.data.take()}));
            }
            if buffer.next_action_request.get() == request_id {
                *buffer.code_actions.borrow_mut() = Some(ListedActions {
                    range,
                    actions: actions.clone(),
                });
            }
            Ok(actions)
        })
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        let _ = self.connection.send(Event::Close(self.id));
    }
}

pub struct EditorProvider(pub Rc<Buffer>);

impl CodeActionProvider for EditorProvider {
    fn id(&self) -> SharedString {
        format!("dotdir-lsp-{}", self.0.session()).into()
    }

    fn code_actions(
        &self,
        state: Entity<EditorState>,
        range: std::ops::Range<usize>,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<CodeAction>>> {
        let editor = state.read(cx);
        self.0.list_code_actions(
            editor.text(),
            crate::quick_actions::action_range(&self.0, editor.text(), range),
            cx,
        )
    }

    fn perform_code_action(
        &self,
        state: Entity<EditorState>,
        mut action: CodeAction,
        _: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let buffer = self.0.clone();
        let state = state.downgrade();
        let provider_id = self.id();
        window.spawn(cx, async move |cx| {
            let result: Result<()> = async {
                // GPUI invokes the provider while it holds a mutable editor lease.
                let (text, revision, resolution) = cx.update(|_, cx| -> Result<_> {
                    let editor = state.upgrade().context("Editor is closed")?;
                    let text = editor.read(cx).text().clone();
                    buffer.change(text.clone());
                    let revision = buffer.revision();
                    let data = action.data.take().unwrap_or_default();
                    ensure!(
                        data["revision"].as_i64() == Some(i64::from(revision)),
                        "The buffer changed. Open Code Actions again."
                    );
                    action.data = data.get("wire").filter(|value| !value.is_null()).cloned();
                    let resolution = action
                        .edit
                        .is_none()
                        .then(|| buffer.query(&text, "codeAction/resolve", json!(action), cx));
                    Ok((text, revision, resolution))
                })??;
                if let Some(request) = resolution {
                    action = serde_json::from_value(request.await?)?;
                }
                let mut edit = action.edit.context("Code action did not supply an edit")?;
                let changes = edit
                    .changes
                    .take()
                    .filter(|changes| changes.len() == 1)
                    .context("Code action requires changes outside this editor")?;
                ensure!(
                    edit.document_changes.is_none() && edit.change_annotations.is_none(),
                    "Unsupported workspace edit"
                );
                let (_, edits) = changes
                    .into_iter()
                    .find(|(uri, _)| uri.as_str() == buffer.uri)
                    .context("Code action targets a different file")?;
                cx.update(|window, cx| {
                    let state = state.upgrade().context("Editor is closed")?;
                    state.update(cx, |editor, cx| {
                        ensure!(
                            editor.is_editable()
                                && buffer.revision() == revision
                                && editor.text() == &text
                                && editor
                                    .lsp()
                                    .code_action_providers
                                    .iter()
                                    .any(|provider| provider.id() == provider_id),
                            "The buffer changed. Open Code Actions again."
                        );
                        let batch = vscode::EditBatch::prepare(
                            editor.text(),
                            edits.into_iter().map(Into::into).collect(),
                        )
                        .context("Invalid code action edits")?;
                        batch.apply(editor, window, cx);
                        Ok(())
                    })
                })??;
                Ok(())
            }
            .await;
            if let Err(error) = result {
                let _ = buffer
                    .connection
                    .send(Event::CodeActionError(buffer.id, format!("{error:#}")));
            }
            Ok(())
        })
    }
}

impl DefinitionProvider for EditorProvider {
    fn definitions(
        &self,
        text: &Rope,
        offset: usize,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<LocationLink>>> {
        let request = self.0.query(
            text,
            "textDocument/definition",
            self.0.position_params(text, offset),
            cx,
        );
        let text = text.clone();
        let uri = self.0.uri.clone();
        cx.spawn(async move |_| {
            let response: Option<GotoDefinitionResponse> = serde_json::from_value(request.await?)?;
            let convert = |location: lsp_types::Location| LocationLink {
                origin_selection_range: None,
                target_uri: location.uri,
                target_range: location.range,
                target_selection_range: location.range,
            };
            let mut links = match response {
                None => vec![],
                Some(GotoDefinitionResponse::Scalar(location)) => vec![convert(location)],
                Some(GotoDefinitionResponse::Array(locations)) => {
                    locations.into_iter().map(convert).collect()
                }
                Some(GotoDefinitionResponse::Link(links)) => links,
            };
            for link in &mut links {
                if let Some(range) = link.origin_selection_range.as_mut() {
                    *range = editor_range(&text, *range);
                }
                if link.target_uri.as_str() == uri {
                    link.target_range = editor_range(&text, link.target_range);
                    link.target_selection_range = editor_range(&text, link.target_selection_range);
                }
            }
            Ok(links)
        })
    }
}

impl CompletionProvider for EditorProvider {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: CompletionContext,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let mut params = self.0.position_params(text, offset);
        params["context"] = json!(trigger);
        let request = self.0.query(text, "textDocument/completion", params, cx);
        let text = text.clone();
        cx.spawn(async move |_| {
            let response: Option<CompletionResponse> = serde_json::from_value(request.await?)?;
            let mut items = match response {
                Some(CompletionResponse::List(list)) => list.items,
                Some(CompletionResponse::Array(items)) => items,
                None => vec![],
            };
            for item in &mut items {
                if let Some(edit) = &mut item.text_edit {
                    match edit {
                        CompletionTextEdit::Edit(edit) => {
                            edit.range = editor_range(&text, edit.range)
                        }
                        CompletionTextEdit::InsertAndReplace(edit) => {
                            edit.insert = editor_range(&text, edit.insert);
                            edit.replace = editor_range(&text, edit.replace);
                        }
                    }
                }
                if let Some(edits) = &mut item.additional_text_edits {
                    for edit in edits {
                        edit.range = editor_range(&text, edit.range);
                    }
                }
            }
            Ok(CompletionResponse::Array(items))
        })
    }

    fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut App) -> bool {
        let mut chars = new_text.chars();
        chars
            .next()
            .is_some_and(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '$' | '.'))
            && chars.next().is_none()
    }
}

// GPUI Kit counts Unicode scalars while the servers use UTF-16 code units.
fn wire_position(text: &Rope, byte: usize) -> Position {
    let point = text.offset_to_point(byte);
    let character = text.slice_line(point.row).byte_to_utf16_idx(point.column);
    Position::new(point.row as u32, character as u32)
}

pub fn editor_position(text: &Rope, position: Position) -> Position {
    let row = (position.line as usize).min(text.lines_len().saturating_sub(1));
    let line = text.slice_line(row);
    let byte = line.utf16_to_byte_idx((position.character as usize).min(line.len_utf16()));
    Position::new(row as u32, line.byte_to_char_idx(byte) as u32)
}

fn editor_range(text: &Rope, range: Range) -> Range {
    Range::new(
        editor_position(text, range.start),
        editor_position(text, range.end),
    )
}

fn incremental_change(old: &Rope, new: &Rope) -> Option<Value> {
    let prefix = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    if prefix == old.len() && prefix == new.len() {
        return None;
    }
    let mut prefix = old.char_to_byte_idx(old.byte_to_char_idx(prefix));
    if splits_crlf(old, prefix) || splits_crlf(new, prefix) {
        prefix -= 1;
    }
    let suffix = old
        .bytes_at(old.len())
        .reversed()
        .zip(new.bytes_at(new.len()).reversed())
        .take(old.len().min(new.len()) - prefix)
        .take_while(|(a, b)| a == b)
        .count();
    let old_end = old.len() - suffix;
    let mut old_boundary = old.char_to_byte_idx(old.byte_to_char_idx(old_end));
    if old_boundary < old_end {
        old_boundary = old.char_to_byte_idx(old.byte_to_char_idx(old_end) + 1);
    }
    let mut new_end = new.len() - (old.len() - old_boundary);
    if splits_crlf(old, old_boundary) || splits_crlf(new, new_end) {
        old_boundary += 1;
        new_end += 1;
    }
    Some(json!({
        "range": Range::new(wire_position(old, prefix), wire_position(old, old_boundary)),
        "text": new.slice(prefix..new_end).to_string(),
    }))
}

fn splits_crlf(text: &Rope, byte: usize) -> bool {
    byte > 0 && byte < text.len() && text.byte(byte - 1) == b'\r' && text.byte(byte) == b'\n'
}

struct Connection {
    id: u64,
    extension: String,
    name: String,
    events: mpsc::Sender<Event>,
    running: Arc<AtomicBool>,
    commands: Arc<RwLock<HashSet<String>>>,
    archive: Option<Arc<Workspace>>,
}

impl Connection {
    fn start(
        id: u64,
        extension: String,
        launch: Launch,
        project: Project,
        archive: Option<Arc<Workspace>>,
        logs: ExtensionLogs,
        notify: Notify,
    ) -> Arc<Self> {
        let (events, receiver) = mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let commands = Arc::new(RwLock::new(HashSet::new()));
        let log = logs.source(&extension, &format!("{} #{id}", launch.name));
        let connection = Arc::new(Self {
            id,
            extension,
            name: launch.name.clone(),
            events: events.clone(),
            running: running.clone(),
            commands: commands.clone(),
            archive: archive.clone(),
        });
        std::thread::spawn(move || {
            log.append(&format!("Starting in {}", project.root.display()));
            let result = (launch.command)().and_then(|mut command| {
                // The bundled host can exceed the Windows command line limit.
                let mut script = tempfile::Builder::new()
                    .prefix("dotdir-")
                    .suffix(".mjs")
                    .tempfile()
                    .context("Creating the language host script")?;
                script
                    .write_all(launch.script.as_bytes())
                    .context("Writing the language host script")?;
                let script = script.into_temp_path();
                command.arg(&*script);
                let mut server = Server::start(
                    id,
                    command,
                    &project.root,
                    events,
                    receiver,
                    log.clone(),
                    commands,
                    archive,
                )?;
                server.run(&project, launch.options, &notify)
            });
            running.store(false, Ordering::Relaxed);
            if let Err(error) = result {
                log.error(&format!("{error:#}"));
                notify(Notice::Failed {
                    session: id,
                    error: format!("{}: {error}", launch.name),
                });
            } else {
                log.append("Stopped");
            }
        });
        connection
    }

    fn send(&self, event: Event) -> Result<()> {
        self.events
            .send(event)
            .map_err(|_| anyhow!("Language server is unavailable"))
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.send(Event::Stop);
    }
}

type Reply = async_channel::Sender<Result<Value>>;

enum Event {
    ArchiveChanged(PathBuf),
    CodeActionError(u64, String),
    Open {
        id: u64,
        uri: String,
        language: String,
        text: Rope,
    },
    Change {
        id: u64,
        revision: i32,
        text: Rope,
    },
    Save(u64),
    Close(u64),
    Query {
        id: u64,
        method: &'static str,
        params: Value,
        reply: Reply,
    },
    Message(Value),
    Disconnected(String),
    Stop,
}

struct TrackedDocument {
    uri: String,
    text: Rope,
    revision: i32,
    pending: Option<(i32, Rope)>,
    due: Option<Instant>,
    diagnostic: Option<u64>,
    ready_deadline: Option<Instant>,
}

enum Pending {
    Query(Reply),
    Diagnostic { document: u64, revision: i32 },
}

struct PendingQuickPick {
    id: String,
    command: u64,
    reply: mpsc::Receiver<usize>,
}

struct Server {
    id: u64,
    child: Child,
    outgoing: mpsc::Sender<Value>,
    receiver: mpsc::Receiver<Event>,
    next_request: u64,
    documents: HashMap<u64, TrackedDocument>,
    requests: HashMap<u64, (Instant, Pending)>,
    pull_diagnostics: bool,
    workspace_folders: Value,
    log: LogSource,
    commands: Arc<RwLock<HashSet<String>>>,
    quick_pick: Option<PendingQuickPick>,
    archive: Option<Arc<Workspace>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl Server {
    fn start(
        id: u64,
        mut command: Command,
        root: &Path,
        events: mpsc::Sender<Event>,
        receiver: mpsc::Receiver<Event>,
        log: LogSource,
        commands: Arc<RwLock<HashSet<String>>>,
        archive: Option<Arc<Workspace>>,
    ) -> Result<Self> {
        let program = command.get_program().to_os_string();
        if archive.is_none() {
            command.current_dir(root);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("Could not start {}", Path::new(&program).display()))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        log.capture(child.stderr.take().expect("piped stderr"));
        let (outgoing, queue) = mpsc::channel();
        let write_events = events.clone();
        std::thread::spawn(move || {
            let mut stream = BufWriter::new(stdin);
            for message in queue {
                if let Err(error) = write_message(&mut stream, &message) {
                    let _ = write_events.send(Event::Disconnected(error.to_string()));
                    break;
                }
            }
        });
        std::thread::spawn(move || {
            let mut stream = BufReader::new(stdout);
            loop {
                match read_message(&mut stream) {
                    Ok(message) => {
                        if events.send(Event::Message(message)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = events.send(Event::Disconnected(error.to_string()));
                        break;
                    }
                }
            }
        });
        Ok(Self {
            id,
            child,
            outgoing,
            receiver,
            next_request: 1,
            documents: HashMap::new(),
            requests: HashMap::new(),
            pull_diagnostics: false,
            workspace_folders: Value::Null,
            log,
            commands,
            quick_pick: None,
            archive,
        })
    }

    fn run(&mut self, project: &Project, options: Value, notify: &Notify) -> Result<()> {
        let root_uri = file_uri(&project.root).context("Invalid language server project path")?;
        let name = project
            .root
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        self.workspace_folders = if project.detached.is_none() {
            json!([{"uri": root_uri, "name": name}])
        } else {
            json!([])
        };
        let root_uri = project.detached.is_none().then_some(root_uri);
        let init = self.request(
            "initialize",
            json!({
                "clientInfo": {"name": "DotDir"},
                "processId": std::process::id(),
                "rootUri": root_uri,
                "workspaceFolders": self.workspace_folders,
                "initializationOptions": options,
                "capabilities": {
                    "general": {"positionEncodings": ["utf-16"]},
                    "workspace": {"configuration": true, "workspaceFolders": true,
                        "diagnostics": {"refreshSupport": true}},
                    "textDocument": {
                        "completion": {"contextSupport": true},
                        "definition": {"linkSupport": true},
                        "hover": {"contentFormat": ["markdown", "plaintext"]},
                        "diagnostic": {"dynamicRegistration": false}
                    }
                }
            }),
        )?;
        let mut waiting = Vec::new();
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let event = self
                .receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .context("Timed out initializing language server")?;
            match event {
                Event::Message(message)
                    if message["id"].as_u64() == Some(init) && message.get("method").is_none() =>
                {
                    let response = response_result(message)?;
                    let capabilities = &response["capabilities"];
                    ensure!(
                        capabilities["positionEncoding"]
                            .as_str()
                            .unwrap_or("utf-16")
                            == "utf-16",
                        "Unsupported language server position encoding"
                    );
                    self.pull_diagnostics = capabilities["diagnosticProvider"].is_object();
                    break;
                }
                Event::Message(message) => self.handle_message(message, notify)?,
                Event::Disconnected(error) => bail!("{error}"),
                Event::Stop => return Ok(()),
                other => waiting.push(other),
            }
            ensure!(
                Instant::now() < deadline,
                "Timed out initializing language server"
            );
        }
        self.notify("initialized", json!({}))?;
        for event in waiting {
            self.handle_event(event, notify)?;
        }

        loop {
            match self.receiver.recv_timeout(Duration::from_millis(35)) {
                Ok(Event::Stop) => return self.shutdown(),
                Ok(event) => self.handle_event(event, notify)?,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
            self.flush_due()?;
            self.finish_quick_pick()?;
            self.expire_requests();
            let now = Instant::now();
            ensure!(
                !self
                    .documents
                    .values()
                    .any(|doc| doc.ready_deadline.is_some_and(|deadline| now >= deadline)),
                "Timed out activating the language extension"
            );
        }
    }

    fn handle_event(&mut self, event: Event, notify: &Notify) -> Result<()> {
        match event {
            Event::CodeActionError(document, error) => {
                self.log.error(&format!("Code action: {error}"));
                notify(Notice::CodeActionFailed {
                    session: self.id,
                    document,
                    error,
                });
            }
            Event::ArchiveChanged(path) => {
                self.notify("dotdir/archiveChanged", json!({"uri": file_uri(&path)}))?;
            }
            Event::Open {
                id,
                uri,
                language,
                text,
            } => {
                self.notify("textDocument/didOpen", json!({
                    "textDocument": {"uri": uri, "languageId": language, "version": 1, "text": text.to_string()},
                }))?;
                self.documents.insert(
                    id,
                    TrackedDocument {
                        uri,
                        text,
                        revision: 1,
                        pending: None,
                        due: Some(Instant::now()),
                        diagnostic: None,
                        ready_deadline: Some(Instant::now() + ACTIVATION_TIMEOUT),
                    },
                );
            }
            Event::Change { id, revision, text } => {
                if let Some(document) = self.documents.get_mut(&id) {
                    document.pending = Some((revision, text));
                    document.due = Some(Instant::now() + DEBOUNCE);
                    if let Some(request) = document.diagnostic.take() {
                        self.requests.remove(&request);
                        self.notify("$/cancelRequest", json!({"id": request}))?;
                    }
                }
            }
            Event::Save(id) => {
                self.flush_document(id)?;
                if let Some(document) = self.documents.get_mut(&id) {
                    let uri = document.uri.clone();
                    document.due = Some(Instant::now());
                    self.notify(
                        "textDocument/didSave",
                        json!({"textDocument": {"uri": uri}}),
                    )?;
                }
            }
            Event::Close(id) => {
                if let Some(document) = self.documents.remove(&id) {
                    if let Some(request) = document.diagnostic {
                        self.requests.remove(&request);
                        self.notify("$/cancelRequest", json!({"id": request}))?;
                    }
                    self.notify(
                        "textDocument/didClose",
                        json!({"textDocument": {"uri": document.uri}}),
                    )?;
                }
            }
            Event::Query {
                id,
                method,
                params,
                reply,
            } => {
                if !reply.is_closed() && self.documents.contains_key(&id) {
                    self.flush_document(id)?;
                    let request = self.request(method, params)?;
                    self.requests.insert(
                        request,
                        (Instant::now() + REQUEST_TIMEOUT, Pending::Query(reply)),
                    );
                }
            }
            Event::Message(message) => self.handle_message(message, notify)?,
            Event::Disconnected(error) => bail!("{error}"),
            Event::Stop => {}
        }
        Ok(())
    }

    fn flush_document(&mut self, id: u64) -> Result<()> {
        let Some(document) = self.documents.get_mut(&id) else {
            return Ok(());
        };
        if let Some((revision, text)) = document.pending.take() {
            let change = incremental_change(&document.text, &text);
            document.text = text;
            document.revision = revision;
            if let Some(change) = change {
                let uri = document.uri.clone();
                self.notify("textDocument/didChange", json!({
                    "textDocument": {"uri": uri, "version": revision}, "contentChanges": [change],
                }))?;
            }
        }
        Ok(())
    }

    fn flush_due(&mut self) -> Result<()> {
        let now = Instant::now();
        let due = self
            .documents
            .iter()
            .filter(|(_, doc)| doc.due.is_some_and(|due| due <= now))
            .map(|(&id, _)| id)
            .collect::<Vec<_>>();
        for id in due {
            self.flush_document(id)?;
            let document = self.documents.get_mut(&id).expect("tracked document");
            document.due = None;
            if self.pull_diagnostics && document.diagnostic.is_none() {
                let uri = document.uri.clone();
                let revision = document.revision;
                let request = self.request(
                    "textDocument/diagnostic",
                    json!({"textDocument": {"uri": uri}}),
                )?;
                self.documents.get_mut(&id).unwrap().diagnostic = Some(request);
                self.requests.insert(
                    request,
                    (
                        now + REQUEST_TIMEOUT,
                        Pending::Diagnostic {
                            document: id,
                            revision,
                        },
                    ),
                );
            }
        }
        Ok(())
    }

    fn expire_requests(&mut self) {
        let now = Instant::now();
        let expired = self
            .requests
            .iter()
            .filter_map(|(&id, (deadline, pending))| {
                let waiting_for_user = self
                    .quick_pick
                    .as_ref()
                    .is_some_and(|picker| picker.command == id);
                ((!waiting_for_user && *deadline <= now)
                    || matches!(pending, Pending::Query(reply) if reply.is_closed()))
                .then_some(id)
            })
            .collect::<Vec<_>>();
        for id in expired {
            if let Some((_, pending)) = self.requests.remove(&id) {
                match pending {
                    Pending::Query(reply) => {
                        let _ = reply.try_send(Err(anyhow!("Language server request timed out")));
                    }
                    Pending::Diagnostic { document, .. } => {
                        if let Some(document) = self.documents.get_mut(&document) {
                            document.diagnostic = None;
                        }
                    }
                }
                let _ = self.notify("$/cancelRequest", json!({"id": id}));
            }
        }
    }

    fn handle_message(&mut self, message: Value, notify: &Notify) -> Result<()> {
        if let Some(method) = message["method"].as_str() {
            match method {
                "dotdir/archiveFile" if !message["id"].is_null() => {
                    let result = self.archive.as_ref().context("No archive workspace")
                        .and_then(|workspace| workspace.serve(&message["params"]));
                    match result {
                        Ok(value) => self.respond(&message, value)?,
                        Err(error) => self.write(json!({"jsonrpc":"2.0", "id":message["id"],
                            "error":{"code":-32000, "message":format!("{error:#}")}}))?,
                    }
                }
                "dotdir/showQuickPick" => {
                    let params = &message["params"];
                    if self.quick_pick.is_none()
                        && let Some(id) = message["id"].as_str()
                        && let Some(command) = params["command"].as_u64()
                        && matches!(self.requests.get(&command), Some((_, Pending::Query(_))))
                        && let Ok(request) = serde_json::from_value::<vscode::QuickPick>(params.clone())
                    {
                        let (reply, receive) = mpsc::channel();
                        self.quick_pick = Some(PendingQuickPick { id: id.into(), command, reply: receive });
                        notify(Notice::QuickPick { session: self.id, request, reply });
                    } else {
                        self.respond(&message, Value::Null)?;
                    }
                }
                "dotdir/cancelQuickPick" => {
                    if self.quick_pick.as_ref().is_some_and(|picker| {
                        Some(picker.id.as_str()) == message["params"]["id"].as_str()
                    }) {
                        self.reply_to_quick_pick(None)?;
                        notify(Notice::QuickPickClosed { session: self.id });
                    }
                }
                "dotdir/commandsChanged" => {
                    if let Ok(commands) =
                        serde_json::from_value(message["params"]["commands"].clone())
                    {
                        *self.commands.write().unwrap() = commands;
                        notify(Notice::CommandsChanged);
                    }
                }
                "dotdir/documentReady" => {
                    if let Some((&document, tracked)) = self.documents.iter_mut().find(|(_, tracked)| {
                        tracked.ready_deadline.is_some() && Some(tracked.uri.as_str()) == message["params"]["uri"].as_str()
                    }) {
                        tracked.ready_deadline = None;
                        self.log.append("Language client ready");
                        notify(Notice::Ready { session: self.id, document });
                    }
                }
                "dotdir/activationFailed" => {
                    return Err(vscode::host_error(
                        message["params"]["message"].as_str().unwrap_or("Extension activation failed"),
                        message["params"]["stack"].as_str(),
                    ));
                }
                "window/logMessage" => {
                    let params = &message["params"];
                    if let Some(message) = params["message"].as_str() {
                        if params["type"].as_u64() == Some(1) {
                            self.log.error(message);
                        } else {
                            self.log.append(message);
                        }
                    }
                }
                "workspace/configuration" => {
                    let count = message["params"]["items"].as_array().map_or(0, Vec::len);
                    self.respond(&message, json!(vec![Value::Null; count]))?;
                }
                "workspace/workspaceFolders" => self.respond(&message, self.workspace_folders.clone())?,
                "workspace/diagnostic/refresh" => {
                    for document in self.documents.values_mut() {
                        document.due = Some(Instant::now() + DEBOUNCE);
                    }
                    self.respond(&message, Value::Null)?;
                }
                "textDocument/publishDiagnostics" => {
                    if !self.pull_diagnostics {
                        let params = serde_json::from_value::<lsp_types::PublishDiagnosticsParams>(message["params"].clone())?;
                        if let Some((&id, document)) = self.documents.iter().find(|(_, doc)| doc.uri == params.uri.as_str())
                            && document.pending.is_none() && params.version.is_none_or(|v| v == document.revision)
                        {
                            send_diagnostics(notify, self.id, id, document, document.revision, params.diagnostics);
                        }
                    }
                }
                _ if !message["id"].is_null() => self.write(json!({
                    "jsonrpc":"2.0", "id": message["id"],
                    "error": {"code": -32601, "message": format!("DotDir does not support {method}")},
                }))?,
                _ => {}
            }
            return Ok(());
        }
        if let Some(request) = message["id"].as_u64()
            && let Some((_, pending)) = self.requests.remove(&request)
        {
            if self
                .quick_pick
                .as_ref()
                .is_some_and(|picker| picker.command == request)
            {
                self.quick_pick = None;
                notify(Notice::QuickPickClosed { session: self.id });
            }
            let result = response_result(message);
            match pending {
                Pending::Query(reply) => {
                    let _ = reply.try_send(result);
                }
                Pending::Diagnostic {
                    document: id,
                    revision,
                } => {
                    if let Some(document) = self.documents.get_mut(&id) {
                        document.diagnostic = None;
                        if document.pending.is_none()
                            && document.revision == revision
                            && let Ok(report) = result
                            && let Some(items) = report.get("items").cloned()
                            && let Ok(items) = serde_json::from_value::<Vec<Diagnostic>>(items)
                        {
                            send_diagnostics(notify, self.id, id, document, revision, items);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn finish_quick_pick(&mut self) -> Result<()> {
        let Some(picker) = &self.quick_pick else {
            return Ok(());
        };
        let selection = match picker.reply.try_recv() {
            Ok(index) => Some(index),
            Err(mpsc::TryRecvError::Disconnected) => None,
            Err(mpsc::TryRecvError::Empty) => return Ok(()),
        };
        self.reply_to_quick_pick(selection)
    }

    fn reply_to_quick_pick(&mut self, selection: Option<usize>) -> Result<()> {
        let Some(picker) = self.quick_pick.take() else {
            return Ok(());
        };
        if let Some((deadline, _)) = self.requests.get_mut(&picker.command) {
            *deadline = Instant::now() + REQUEST_TIMEOUT;
        }
        self.write(json!({"jsonrpc": "2.0", "id": picker.id, "result": selection}))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<u64> {
        let id = self.next_request;
        self.next_request += 1;
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        Ok(id)
    }

    fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write(json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    fn respond(&self, message: &Value, result: Value) -> Result<()> {
        if !message["id"].is_null() {
            self.write(json!({"jsonrpc": "2.0", "id": message["id"], "result": result}))?;
        }
        Ok(())
    }

    fn write(&self, message: Value) -> Result<()> {
        self.outgoing
            .send(message)
            .map_err(|_| anyhow!("Language server disconnected"))
    }

    fn shutdown(&mut self) -> Result<()> {
        if let Ok(id) = self.request("shutdown", Value::Null) {
            let deadline = Instant::now() + Duration::from_millis(700);
            while let Ok(event) = self
                .receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                if matches!(event, Event::Message(ref msg) if msg["id"].as_u64() == Some(id) && msg.get("method").is_none())
                {
                    let _ = self.notify("exit", Value::Null);
                    for _ in 0..20 {
                        if self.child.try_wait()?.is_some() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    break;
                }
                if Instant::now() >= deadline {
                    break;
                }
            }
        }
        Ok(())
    }
}

fn send_diagnostics(
    notify: &Notify,
    session: u64,
    id: u64,
    document: &TrackedDocument,
    revision: i32,
    mut items: Vec<Diagnostic>,
) {
    for item in &mut items {
        item.range = editor_range(&document.text, item.range);
    }
    // The editor builds its sorted range index in this order.
    items.sort_by_key(|item| (item.range.start.line, item.range.start.character));
    notify(Notice::Diagnostics {
        session,
        document: id,
        revision,
        items,
    });
}

fn response_result(mut message: Value) -> Result<Value> {
    if let Some(error) = message.get("error") {
        return Err(vscode::host_error(
            error["message"]
                .as_str()
                .unwrap_or("Language server request failed"),
            error["data"]["stack"].as_str(),
        ));
    }
    Ok(message["result"].take())
}

fn read_message(reader: &mut impl BufRead) -> Result<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        ensure!(
            reader.read_line(&mut line)? > 0,
            "Language server closed its output"
        );
        if line.trim_end().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("Content-Length")
        {
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let length = length.context("Missing LSP Content-Length")?;
    ensure!(
        length <= 64 * 1024 * 1024,
        "Language server reply exceeds the message limit"
    );
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload)?;
    Ok(serde_json::from_slice(&payload)?)
}

fn write_message(writer: &mut impl Write, message: &Value) -> Result<()> {
    let payload = serde_json::to_vec(message)?;
    write!(writer, "Content-Length: {}\r\n\r\n", payload.len())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn edit_ranges_use_utf16_and_keep_crlf_boundaries() {
        let old = Rope::from("const s = '😀x';\r\nconst s2 = 'α';\r\n");
        let new = Rope::from("const s = '😀y';\r\nconst s2 = 'α';\r\n");
        let change = incremental_change(&old, &new).unwrap();
        assert_eq!(
            change,
            json!({"range":{"start":{"line":0,"character":13},"end":{"line":0,"character":14}}, "text":"y"})
        );
        assert_eq!(
            editor_position(&old, Position::new(0, 13)),
            Position::new(0, 12)
        );
        let change = incremental_change(&Rope::from("a\r\nb"), &Rope::from("a\nb")).unwrap();
        assert_eq!(
            change,
            json!({"range":{"start":{"line":0,"character":1},"end":{"line":1,"character":0}}, "text":"\n"})
        );
    }

    fn installed_extensions_root() -> PathBuf {
        std::env::var_os("DOTDIR_TEST_LSP_EXTENSIONS")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::config_dir()
                    .unwrap()
                    .join("dev.dotdir.native/extensions")
            })
    }

    fn query(buffer: &Buffer, method: &'static str, line: u32, character: u32) -> Value {
        let (reply, result) = async_channel::bounded(1);
        buffer.connection.send(Event::Query {
            id: buffer.id,
            method,
            params: json!({"textDocument": {"uri": buffer.uri}, "position": {"line": line, "character": character}}),
            reply,
        }).unwrap();
        result.recv_blocking().unwrap().unwrap()
    }

    #[test]
    #[cfg(feature = "zip-extension")]
    #[ignore = "requires the installed TypeScript 7 Native Preview SDK"]
    fn typescript_archive_resolves_virtual_files_and_unsaved_edits() {
        use crate::archive::Archive;
        use std::io::Write;
        use zip::{ZipWriter, write::SimpleFileOptions};

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("project.zip");
        let config = r#"{"compilerOptions":{"strict":true},"files":["main.ts"]}"#;
        let lib = "export const count: number = 1;";
        let main = "import { count } from './lib';\n/* 😀 */ export const value: string = count;\nconst object = { answer: 42 }; object.answer;\n";
        let mut zip = ZipWriter::new(fs::File::create(&path).unwrap());
        for (name, text) in [
            ("project/tsconfig.json", config),
            ("project/main.ts", main),
            ("project/lib.ts", lib),
            ("loose/main.ts", main),
            ("loose/lib.ts", lib),
        ] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(text.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        let pristine = fs::read(&path).unwrap();
        let archive = Archive::mount_location(path.clone().into(), &dotdir_zip::ZIP).unwrap();
        let source = archive.resolve_archive_member("project/main.ts").unwrap();
        let dependency = archive.resolve_archive_member("project/lib.ts").unwrap();
        let settings = vscode::Settings {
            enabled: vec!["typescriptteam.native-preview".into()],
            ..Default::default()
        };
        let extensions = vscode::discover(&installed_extensions_root(), &settings);
        let (extension, language) =
            archive_typescript::extension(&source, &extensions, &settings).unwrap();
        let logs = ExtensionLogs::default();
        let mut servers = LanguageServers::new(temp.path().into(), logs, |_| {});
        let fs = Filesystems::new(Default::default());
        let buffer = servers
            .attach_archive(
                1,
                &source,
                Rope::from(main),
                extension,
                language,
                &settings,
                fs.clone(),
            )
            .unwrap();
        let diagnostics = query(&buffer, "textDocument/diagnostic", 0, 0);
        let items = diagnostics["items"].as_array().unwrap();
        let assignment = items
            .iter()
            .find(|item| item["code"] == 2322)
            .expect("type mismatch from an imported ZIP entry");
        assert_eq!(
            assignment["range"]["start"],
            json!({"line": 1, "character": 22})
        );
        let definition = query(&buffer, "textDocument/definition", 1, 39);
        let uri: lsp_types::Uri = serde_json::from_value(definition[0]["uri"].clone()).unwrap();
        assert_eq!(buffer.source_location(&uri), Some(dependency.clone()));
        let hover = query(&buffer, "textDocument/hover", 1, 39);
        assert!(
            hover["contents"]["value"]
                .as_str()
                .unwrap()
                .contains("number")
        );
        let completion = query(&buffer, "textDocument/completion", 2, 38);
        assert!(
            completion["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["label"] == "answer")
        );

        let edited = "export const count: string = 'one';";
        servers.archive_text(&dependency, Some(Rope::from(edited)));
        let after = query(&buffer, "textDocument/diagnostic", 0, 0);
        assert!(after["items"].as_array().unwrap().is_empty(), "{after}");
        servers.archive_text(&dependency, None);
        assert!(
            query(&buffer, "textDocument/diagnostic", 0, 0)["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["code"] == 2322)
        );
        let loose = archive.resolve_archive_member("loose/main.ts").unwrap();
        let loose_lib = archive.resolve_archive_member("loose/lib.ts").unwrap();
        let loose_buffer = servers
            .attach_archive(
                2,
                &loose,
                Rope::from(main),
                extension,
                language,
                &settings,
                fs,
            )
            .unwrap();
        assert!(
            query(&loose_buffer, "textDocument/diagnostic", 0, 0)["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["code"] == 2322)
        );
        servers.archive_text(&loose_lib, Some(Rope::from(edited)));
        let loose_after = query(&loose_buffer, "textDocument/diagnostic", 0, 0);
        assert!(
            loose_after["items"].as_array().unwrap().is_empty(),
            "{loose_after}"
        );
        assert_eq!(std::fs::read(path).unwrap(), pristine);
        assert!(!buffer.connection.archive.as_ref().unwrap().root.exists());
    }

    #[test]
    fn uses_manifest_workspace_markers_and_isolates_detached_files() {
        let temp = tempfile::tempdir().unwrap();
        let package = temp.path().join("extensions/fixture.language");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("package.json"), json!({
            "publisher": "fixture", "name": "language", "version": "1.0.0", "main": "./main.js",
            "activationEvents": ["workspaceContains:project.toml", "workspaceContains:*/project.toml"],
            "contributes": {"languages": [{"id": "fixture", "extensions": [".src"]}]}
        }).to_string()).unwrap();
        let extensions = vscode::discover(
            &temp.path().join("extensions"),
            &vscode::Settings::default(),
        );
        let extension = &extensions[0];
        let root = temp.path().join("project");
        let path = root.join("src/lib.src");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(root.join("project.toml"), "").unwrap();
        assert_eq!(extension.language_for(&path), Some("fixture"));
        assert_eq!(
            Project::for_extension(&path, extension),
            Some(Project {
                root,
                detached: None
            })
        );
        let standalone = temp.path().join("example.src");
        assert_eq!(
            Project::for_extension(&standalone, extension),
            Some(Project {
                root: temp.path().into(),
                detached: Some(standalone),
            })
        );
    }

    #[test]
    #[ignore = "requires the locally installed rust-analyzer extension"]
    fn installed_rust_analyzer_serves_a_cargo_project() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"dotdir_lsp_probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        let file = root.join("src/lib.rs");
        let helper = root.join("src/helper.rs");
        let broken = root.join("src/broken.rs");
        let source =
            "mod helper;\npub fn example() -> u32 {\n    helper::answer()\n}\nmod broken;\n";
        let saved_error = "fn broken() { let _ = ; }\n";
        fs::write(&file, source).unwrap();
        fs::write(&helper, "pub fn answer() -> u32 { 42 }\n").unwrap();
        fs::write(&broken, saved_error).unwrap();

        let (events, notices) = mpsc::channel();
        let extensions =
            vscode::discover(&installed_extensions_root(), &vscode::Settings::default());
        let cargo = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs::home_dir().unwrap().join(".cargo"));
        let path = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(cargo.join("bin")).chain(std::env::split_paths(&path)),
        )
        .unwrap();
        let mut settings = vscode::Settings {
            enabled: vec!["rust-lang.rust-analyzer".into()],
            ..Default::default()
        };
        settings.configuration.insert(
            "rust-analyzer.server.extraEnv".into(),
            json!({"PATH": path.to_str().unwrap()}),
        );
        let (extension, language) =
            vscode::language_extension(&extensions, &settings, &file).unwrap();
        let mut servers = LanguageServers::new(
            temp.path().into(),
            ExtensionLogs::default(),
            move |notice| {
                let _ = events.send(notice);
            },
        );
        let buffer = servers
            .attach_extension(1, &file, Rope::from(source), language, extension, &settings)
            .expect("rust-analyzer VS Code client");
        assert_eq!(buffer.extension_id(), "rust-lang.rust-analyzer");
        let other = servers
            .attach_extension(
                2,
                &helper,
                Rope::from(fs::read_to_string(&helper).unwrap()),
                language,
                extension,
                &settings,
            )
            .expect("analyzer for second document");
        assert_eq!(buffer.session(), other.session());
        let broken = servers
            .attach_extension(
                3,
                &broken,
                Rope::from(saved_error),
                language,
                extension,
                &settings,
            )
            .expect("analyzer for the document with a compiler error");
        assert_eq!(buffer.session(), broken.session());
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match notices.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Notice::Diagnostics {
                    document: 3, items, ..
                }) if items.iter().any(|item| {
                    item.source.as_deref() == Some("rustc")
                        && item.severity == Some(lsp_types::DiagnosticSeverity::ERROR)
                }) =>
                {
                    break;
                }
                Ok(Notice::Failed { error, .. }) => panic!("{error}"),
                Ok(_) => {}
                Err(error) => panic!("No Rust compiler diagnostic arrived: {error}"),
            }
        }

        let helper_uri = file_uri(&helper).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let definition = query(&buffer, "textDocument/definition", 2, 14);
            let hover = query(&buffer, "textDocument/hover", 2, 14);
            let completion = query(&buffer, "textDocument/completion", 2, 12);
            let understood = definition.to_string().contains(&helper_uri)
                && hover.to_string().contains("answer")
                && completion.to_string().contains("answer");
            if understood {
                assert!(serde_json::from_value::<GotoDefinitionResponse>(definition).is_ok());
                assert!(serde_json::from_value::<Hover>(hover).is_ok());
                assert!(serde_json::from_value::<CompletionResponse>(completion).is_ok());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Rust analysis was incomplete: {definition}, {hover}, {completion}"
            );
            std::thread::sleep(Duration::from_millis(200));
        }

        let expect_diagnostics = |revision, has_errors| {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                match notices
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap()
                {
                    Notice::Diagnostics {
                        document: 1,
                        revision: actual,
                        items,
                        ..
                    } if actual == revision => {
                        if items
                            .iter()
                            .any(|item| item.severity == Some(lsp_types::DiagnosticSeverity::ERROR))
                            == has_errors
                        {
                            break;
                        }
                    }
                    Notice::Failed { error, .. } => panic!("{error}"),
                    _ => {}
                }
            }
        };
        buffer.change(Rope::from(format!("{source}fn broken() {{ let _ = ; }}\n")));
        expect_diagnostics(2, true);
        buffer.change(Rope::from(source));
        expect_diagnostics(3, false);
        assert_eq!(fs::read_to_string(file).unwrap(), source);

        let text = buffer.text.borrow().clone();
        let error = buffer
            .execute_command("rust-analyzer.viewCrateGraph", &text, 0, 0)
            .unwrap()
            .recv_blocking()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("requires VS Code webviews"));
        assert!(format!("{error:#}").contains("out/main.js"));
        assert!(!query(&buffer, "textDocument/hover", 2, 14).is_null());
    }

    #[test]
    #[ignore = "requires the locally installed TypeScript 7 extension"]
    fn installed_typescript_serves_a_live_unsaved_buffer() {
        let root = std::env::var_os("DOTDIR_TEST_TYPESCRIPT_EXTENSIONS")
            .map(PathBuf::from)
            .unwrap_or_else(installed_extensions_root);
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().canonicalize().unwrap();
        let path = project.join("sample #1.ts");
        let input = "const answer: number = \"wrong\";\nanswer;\n\nimport { greet } from 'source-probe';\ngreet();\n";
        fs::write(&path, input).unwrap();
        fs::write(
            project.join("tsconfig.json"),
            r#"{"compilerOptions":{"types":[],"module":"NodeNext","moduleResolution":"NodeNext"}}"#,
        )
        .unwrap();
        let dependency = project.join("node_modules/source-probe");
        fs::create_dir_all(&dependency).unwrap();
        fs::write(
            dependency.join("package.json"),
            r#"{"name":"source-probe","version":"1.0.0","types":"index.d.ts","main":"index.js"}"#,
        )
        .unwrap();
        fs::write(
            dependency.join("index.d.ts"),
            "export function greet(): string;\n",
        )
        .unwrap();
        let implementation = dependency.join("index.js");
        fs::write(
            &implementation,
            "// Implementation\nexports.greet = function greet() { return 'hello'; };\n",
        )
        .unwrap();
        let (events, notices) = mpsc::channel();
        let extensions = vscode::discover(&root, &vscode::Settings::default());
        let mut settings = vscode::Settings {
            enabled: vec!["typescriptteam.native-preview".into()],
            ..Default::default()
        };
        let (extension, language) =
            vscode::language_extension(&extensions, &settings, &path).unwrap();
        let executable = if cfg!(windows) { "tsc.exe" } else { "tsc" };
        let sdk = fs::read_dir(extension.directory().join("node_modules/@typescript"))
            .unwrap()
            .filter_map(|entry| Some(entry.ok()?.path().join("lib")))
            .find(|dir| dir.join(executable).is_file())
            .expect("TypeScript SDK in the installed package");
        settings
            .configuration
            .insert("js/ts.experimental.useTsgo".into(), Value::Bool(true));
        settings
            .configuration
            .insert("js/ts.tsdk.path".into(), json!(sdk));
        let mut typescript = LanguageServers::new(
            temp.path().into(),
            ExtensionLogs::default(),
            move |notice| {
                let _ = events.send(notice);
            },
        );
        let buffer = typescript
            .attach_extension(1, &path, Rope::from(input), language, extension, &settings)
            .expect("TypeScript 7 VS Code client");
        assert_eq!(buffer.extension_id(), "typescriptteam.native-preview");
        let diagnostic = |document, revision, matches: &dyn Fn(&[Diagnostic]) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match notices
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap()
                {
                    Notice::Diagnostics {
                        document: id,
                        revision: actual,
                        items,
                        ..
                    } if id == document && actual == revision && matches(&items) => break,
                    Notice::Failed { error, .. } => panic!("{error}"),
                    _ => {}
                }
            }
        };
        let has_type_error = |items: &[Diagnostic]| {
            items
                .iter()
                .any(|d| d.code == Some(lsp_types::NumberOrString::Number(2322)))
        };
        diagnostic(1, 1, &has_type_error);
        assert!(!query(&buffer, "textDocument/hover", 1, 2).is_null());
        assert!(!query(&buffer, "textDocument/definition", 1, 2).is_null());
        assert!(!query(&buffer, "textDocument/completion", 1, 2).is_null());

        let unsaved = Rope::from(
            input
                .replace("\"wrong\"", "42")
                .replace("answer", "unsavedAnswer"),
        );
        buffer.change(unsaved.clone());
        diagnostic(1, 2, &|items| items.is_empty());

        let restart = "typescript.native-preview.restart";
        assert!(buffer.command_available(restart));
        buffer
            .execute_command(restart, &unsaved, 0, 0)
            .unwrap()
            .recv_blocking()
            .unwrap()
            .unwrap();
        assert!(
            query(&buffer, "textDocument/hover", 1, 2)
                .to_string()
                .contains("unsavedAnswer")
        );

        let go_to_source = |offset| {
            let value = buffer
                .execute_command(
                    "typescript.native-preview.goToSourceDefinition",
                    &unsaved,
                    offset,
                    offset,
                )
                .unwrap()
                .recv_blocking()
                .unwrap()
                .unwrap();
            serde_json::from_value::<vscode::LanguageCommandResult>(value).unwrap()
        };
        let reference = unsaved.to_string().rfind("greet").unwrap() + 1;
        let vscode::LanguageCommandResult::GoToLocation { location } = go_to_source(reference)
        else {
            panic!("Expected a source definition");
        };
        assert_eq!(uri_to_path(&location.uri).unwrap(), implementation);
        assert_eq!(location.range.start.line, 1);
        let whitespace = unsaved.to_string().find("\n\n").unwrap() + 1;
        assert!(matches!(
            go_to_source(whitespace),
            vscode::LanguageCommandResult::Status { message } if message == "No source definitions found."
        ));

        for accept in [false, true] {
            let command = buffer
                .execute_command("typescript.native-preview.selectVersion", &unsaved, 0, 0)
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            let (picker, reply) = loop {
                match notices
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap()
                {
                    Notice::QuickPick {
                        session,
                        request,
                        reply,
                    } if session == buffer.session() => {
                        break (request, reply);
                    }
                    Notice::Failed { error, .. } => panic!("{error}"),
                    _ => {}
                }
            };
            let (index, item) = picker
                .items
                .iter()
                .enumerate()
                .find(|(_, item)| item.label.contains("Use TypeScript 7"))
                .expect("TypeScript 7 version option");
            assert!(item.detail.as_deref().unwrap().ends_with(executable));
            if accept {
                reply.send(index).unwrap();
            }
            drop(reply);
            command.recv_blocking().unwrap().unwrap();
        }
        let storage = fs::read_dir(temp.path().join("vscode-storage"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let workspace = fs::read_dir(storage.join("workspace"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let configuration: Value =
            serde_json::from_slice(&fs::read(workspace.join("configuration.json")).unwrap())
                .unwrap();
        assert_eq!(configuration["js/ts.experimental.useTsgo"], true);
        assert!(
            query(&buffer, "textDocument/hover", 1, 2)
                .to_string()
                .contains("unsavedAnswer")
        );
        buffer.change(Rope::from(input));
        diagnostic(1, 3, &has_type_error);
        assert_eq!(fs::read_to_string(&path).unwrap(), input);

        let session = buffer.session();
        drop(buffer);
        let clean = input.replace("\"wrong\"", "42");
        fs::write(&path, &clean).unwrap();
        let reopened = typescript
            .attach_extension(2, &path, Rope::from(clean), language, extension, &settings)
            .unwrap();
        assert_ne!(session, reopened.session());
        assert!(!query(&reopened, "textDocument/hover", 1, 2).is_null());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match notices
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
            {
                Notice::Ready {
                    session,
                    document: 2,
                } if session == reopened.session() => break,
                Notice::Failed { error, .. } => panic!("{error}"),
                _ => {}
            }
        }
        let missing_member = "export class Foo implements Bar {\n}\ninterface Bar { x: number; }\n";
        reopened.change(Rope::from(missing_member));
        diagnostic(2, 2, &|items| !items.is_empty());
        let actions = reopened
            .request(
                "textDocument/codeAction",
                json!({
                    "textDocument": {"uri": reopened.uri},
                    "version": 2,
                    "range": Range::new(Position::new(0, 14), Position::new(0, 14)),
                }),
            )
            .unwrap()
            .recv_blocking()
            .unwrap()
            .unwrap();
        let action = actions
            .as_array()
            .unwrap()
            .iter()
            .find(|action| action["kind"] == "quickfix")
            .unwrap_or_else(|| panic!("Expected TypeScript interface fix, got {actions}"));
        let action = if action.get("edit").is_some() {
            action.clone()
        } else {
            reopened
                .request("codeAction/resolve", action.clone())
                .unwrap()
                .recv_blocking()
                .unwrap()
                .unwrap()
        };
        let edits = action["edit"]["changes"][&reopened.uri].as_array().unwrap();
        assert!(edits.iter().any(|edit| {
            edit["newText"]
                .as_str()
                .is_some_and(|text| text.contains("x: number;"))
        }));
    }

    #[test]
    #[ignore = "requires the locally installed Red Hat YAML extension and Node.js"]
    fn installed_yaml_validates_and_completes_against_a_local_schema() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().canonicalize().unwrap().join("sample.yml");
        let source = "# yaml-language-server: $schema=./schema.json\nreplicas: wrong\n";
        fs::write(&path, source).unwrap();
        fs::write(
            path.with_file_name("schema.json"),
            json!({
                "type": "object",
                "properties": {
                    "replicas": {"type": "integer", "description": "Number of instances"},
                    "image": {"type": "string"}
                }
            })
            .to_string(),
        )
        .unwrap();
        let (events, notices) = mpsc::channel();
        let extensions =
            vscode::discover(&installed_extensions_root(), &vscode::Settings::default());
        let mut settings = vscode::Settings {
            enabled: vec!["redhat.vscode-yaml".into()],
            ..Default::default()
        };
        settings
            .configuration
            .insert("yaml.schemaStore.enable".into(), Value::Bool(false));
        let (extension, language) =
            vscode::language_extension(&extensions, &settings, &path).unwrap();
        let mut servers = LanguageServers::new(
            temp.path().into(),
            ExtensionLogs::default(),
            move |notice| {
                let _ = events.send(notice);
            },
        );
        let buffer = servers
            .attach_extension(1, &path, Rope::from(source), language, extension, &settings)
            .unwrap();
        assert_eq!(buffer.extension_id(), "redhat.vscode-yaml");
        let expect_diagnostics = |revision, matches: &dyn Fn(&[Diagnostic]) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match notices
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap()
                {
                    Notice::Diagnostics {
                        document: 1,
                        revision: actual,
                        items,
                        ..
                    } if actual == revision && matches(&items) => break,
                    Notice::Failed { error, .. } => panic!("{error}"),
                    _ => {}
                }
            }
        };
        expect_diagnostics(1, &|items| {
            items.iter().any(|item| item.message.contains("integer"))
        });
        let hover = query(&buffer, "textDocument/hover", 1, 2);
        assert!(
            hover["contents"]["value"]
                .as_str()
                .unwrap()
                .contains("Number of instances")
        );
        let completion = query(&buffer, "textDocument/completion", 2, 0);
        assert!(
            completion["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["label"] == "image")
        );

        buffer.change(Rope::from(source.replace("wrong", "2")));
        expect_diagnostics(2, &|items| items.is_empty());
        assert_eq!(fs::read_to_string(path).unwrap(), source);
    }
}
