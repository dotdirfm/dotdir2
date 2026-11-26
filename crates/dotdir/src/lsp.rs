use anyhow::{Context as _, Result, anyhow, bail, ensure};
use gpui_kit::{
    App, Task, Window,
    component::input::{CompletionProvider, DefinitionProvider, Rope, RopeExt},
};
use lsp_types::{
    CompletionContext, CompletionResponse, CompletionTextEdit, Diagnostic, GotoDefinitionResponse,
    Hover, LocationLink, Position, Range,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    env, fs,
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    rc::Rc,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use url::Url;

const DEBOUNCE: Duration = Duration::from_millis(250);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

pub enum Notice {
    Diagnostics {
        document: u64,
        revision: i32,
        items: Vec<Diagnostic>,
    },
    Failed {
        session: u64,
        error: String,
    },
}

type Notify = Arc<dyn Fn(Notice) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ServerKind {
    TypeScript,
    Rust,
    Yaml,
}

impl ServerKind {
    fn display_name(self) -> &'static str {
        match self {
            Self::TypeScript => "TypeScript 7",
            Self::Rust => "rust-analyzer",
            Self::Yaml => "YAML Language Server",
        }
    }

    fn extension_id(self) -> (&'static str, &'static str) {
        match self {
            Self::TypeScript => ("typescriptteam", "native-preview"),
            Self::Rust => ("rust-lang", "rust-analyzer"),
            Self::Yaml => ("redhat", "vscode-yaml"),
        }
    }

    fn project(self, path: &Path) -> Option<Project> {
        let parent = path.parent()?;
        match self {
            Self::TypeScript => Some(Project {
                root: typescript_root(parent),
                detached: None,
            }),
            Self::Rust => {
                let root = parent.ancestors().find(|dir| {
                    ["Cargo.toml", "rust-project.json", ".rust-project.json"]
                        .iter()
                        .any(|name| dir.join(name).is_file())
                });
                Some(match root {
                    Some(root) => Project {
                        root: root.into(),
                        detached: None,
                    },
                    None => Project {
                        root: parent.into(),
                        detached: Some(path.into()),
                    },
                })
            }
            Self::Yaml => Some(Project {
                root: parent
                    .ancestors()
                    .find(|dir| dir.join(".git").exists())
                    .unwrap_or(parent)
                    .into(),
                detached: None,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Project {
    root: PathBuf,
    detached: Option<PathBuf>,
}

pub struct LanguageServers {
    extensions: PathBuf,
    node: Option<PathBuf>,
    sessions: HashMap<(ServerKind, Project), Weak<Connection>>,
    notify: Notify,
}

impl LanguageServers {
    pub fn new(
        extensions: PathBuf,
        node: Option<PathBuf>,
        notify: impl Fn(Notice) + Send + Sync + 'static,
    ) -> Self {
        Self {
            extensions,
            node,
            sessions: HashMap::new(),
            notify: Arc::new(notify),
        }
    }

    pub fn attach(&mut self, id: u64, path: &Path, text: Rope) -> Option<Rc<Buffer>> {
        let (kind, language) = language_for(path)?;
        let uri = file_uri(path)?;
        let key = (kind, kind.project(path)?);
        self.sessions.retain(|_, weak| weak.strong_count() > 0);
        let active = self
            .sessions
            .get(&key)
            .and_then(Weak::upgrade)
            .filter(|connection| connection.running.load(Ordering::Relaxed));
        let connection = if let Some(active) = active {
            active
        } else {
            let executable = installed_server(&self.extensions, kind)?
                .canonicalize()
                .ok()?;
            let connection = Connection::start(
                id,
                kind,
                executable,
                self.node.clone(),
                key.1.clone(),
                self.notify.clone(),
            );
            self.sessions.insert(key, Arc::downgrade(&connection));
            connection
        };
        connection
            .send(Event::Open {
                id,
                uri: uri.clone(),
                language,
                text: text.clone(),
            })
            .ok()?;
        Some(Rc::new(Buffer {
            id,
            uri,
            connection,
            text: RefCell::new(text),
            revision: Cell::new(1),
        }))
    }
}

#[derive(Deserialize)]
struct ExtensionManifest {
    name: String,
    publisher: String,
    version: String,
}

fn installed_server(root: &Path, kind: ServerKind) -> Option<PathBuf> {
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "win32",
        _ => return None,
    };
    let native = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        _ => return None,
    };
    let architectures = if platform == "darwin" && native == "arm64" {
        &["arm64", "x64"][..]
    } else {
        std::slice::from_ref(&native)
    };
    let suffix = if platform == "win32" { ".exe" } else { "" };
    let (publisher, name) = kind.extension_id();
    fs::read_dir(root)
        .ok()?
        .filter_map(|entry| {
            let root = entry.ok()?.path();
            let (dir, manifest) = [root.join("extension"), root].into_iter().find_map(|dir| {
                let manifest = serde_json::from_slice::<ExtensionManifest>(
                    &fs::read(dir.join("package.json")).ok()?,
                )
                .ok()?;
                Some((dir, manifest))
            })?;
            if !manifest.publisher.eq_ignore_ascii_case(publisher)
                || !manifest.name.eq_ignore_ascii_case(name)
            {
                return None;
            }
            let version = manifest
                .version
                .split('.')
                .map(|n| n.parse::<u32>().unwrap_or_default())
                .collect::<Vec<_>>();
            let (path, preferred) = match kind {
                ServerKind::TypeScript => architectures.iter().find_map(|arch| {
                    let path = dir.join(format!(
                        "node_modules/@typescript/typescript-{platform}-{arch}/lib/tsc{suffix}"
                    ));
                    path.is_file().then_some((path, *arch == native))
                }),
                ServerKind::Rust => {
                    let path = dir.join(format!("server/rust-analyzer{suffix}"));
                    path.is_file().then_some((path, true))
                }
                ServerKind::Yaml => {
                    let path = dir.join("dist/languageserver.js");
                    path.is_file().then_some((path, true))
                }
            }?;
            Some((preferred, version, path))
        })
        .max()
        .map(|(_, _, path)| path)
}

fn language_for(path: &Path) -> Option<(ServerKind, &'static str)> {
    let (kind, language) = match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "ts" | "mts" | "cts" => (ServerKind::TypeScript, "typescript"),
        "tsx" => (ServerKind::TypeScript, "typescriptreact"),
        "js" | "mjs" | "cjs" => (ServerKind::TypeScript, "javascript"),
        "jsx" => (ServerKind::TypeScript, "javascriptreact"),
        "rs" => (ServerKind::Rust, "rust"),
        "yaml" | "yml" => (ServerKind::Yaml, "yaml"),
        _ => return None,
    };
    Some((kind, language))
}

fn typescript_root(parent: &Path) -> PathBuf {
    let mut package = None;
    for dir in parent.ancestors().filter(|dir| dir.parent().is_some()) {
        if ["tsconfig.json", "jsconfig.json"]
            .iter()
            .any(|name| dir.join(name).is_file())
        {
            return dir.into();
        }
        if package.is_none() && dir.join("package.json").is_file() {
            package = Some(dir.to_path_buf());
        }
        if dir.join(".git").exists() {
            return package.unwrap_or_else(|| dir.into());
        }
    }
    package.unwrap_or_else(|| parent.into())
}

// Finder launches may omit the Rust toolchain's bin directory from PATH.
fn rust_toolchain_path() -> Option<std::ffi::OsString> {
    let bin = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".cargo")))?
        .join("bin");
    let path = env::var_os("PATH").unwrap_or_default();
    let mut dirs = env::split_paths(&path).collect::<Vec<_>>();
    if bin.is_dir() && !dirs.contains(&bin) {
        dirs.push(bin);
    }
    env::join_paths(dirs).ok()
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
}

impl Buffer {
    pub fn session(&self) -> u64 {
        self.connection.id
    }

    pub fn server_name(&self) -> &'static str {
        self.connection.kind.display_name()
    }
    pub fn revision(&self) -> i32 {
        self.revision.get()
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

    fn query(
        self: &Rc<Self>,
        text: &Rope,
        method: &'static str,
        params: Value,
        cx: &App,
    ) -> Task<Result<Value>> {
        self.change(text.clone());
        let revision = self.revision();
        let (reply, receiver) = async_channel::bounded(1);
        if self
            .connection
            .send(Event::Query {
                id: self.id,
                method,
                params,
                reply,
            })
            .is_err()
        {
            return Task::ready(Err(anyhow!("Language server is unavailable")));
        }
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
}

impl Drop for Buffer {
    fn drop(&mut self) {
        let _ = self.connection.send(Event::Close(self.id));
    }
}

pub struct EditorProvider(pub Rc<Buffer>);

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
    kind: ServerKind,
    events: mpsc::Sender<Event>,
    running: Arc<AtomicBool>,
}

impl Connection {
    fn start(
        id: u64,
        kind: ServerKind,
        executable: PathBuf,
        node: Option<PathBuf>,
        project: Project,
        notify: Notify,
    ) -> Arc<Self> {
        let (events, receiver) = mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let connection = Arc::new(Self {
            id,
            kind,
            events: events.clone(),
            running: running.clone(),
        });
        std::thread::spawn(move || {
            let options = if kind == ServerKind::Yaml {
                // The extension passes this directory to its bundled server.
                executable
                    .parent()
                    .map(|dir| json!({"l10nPath": dir.join("l10n")}))
            } else {
                project
                    .detached
                    .as_ref()
                    .map(|path| json!({"detachedFiles": [path]}))
            };
            let result = Server::start(
                kind,
                &executable,
                node.as_deref(),
                &project.root,
                events,
                receiver,
            )
            .and_then(|mut server| server.run(&project, options, &notify));
            running.store(false, Ordering::Relaxed);
            if let Err(error) = result {
                notify(Notice::Failed {
                    session: id,
                    error: format!("{}: {error:#}", kind.display_name()),
                });
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
    Open {
        id: u64,
        uri: String,
        language: &'static str,
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
}

enum Pending {
    Query(Reply),
    Diagnostic { document: u64, revision: i32 },
}

struct Server {
    child: Child,
    outgoing: mpsc::Sender<Value>,
    receiver: mpsc::Receiver<Event>,
    next_request: u64,
    documents: HashMap<u64, TrackedDocument>,
    requests: HashMap<u64, (Instant, Pending)>,
    pull_diagnostics: bool,
    workspace_folders: Value,
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
        kind: ServerKind,
        executable: &Path,
        node: Option<&Path>,
        root: &Path,
        events: mpsc::Sender<Event>,
        receiver: mpsc::Receiver<Event>,
    ) -> Result<Self> {
        let program = if kind == ServerKind::Yaml {
            crate::vscode::node_path(node).context(
                "Node.js is required. Set vscode.node in settings.json to its executable",
            )?
        } else {
            executable.into()
        };
        let mut command = Command::new(&program);
        match kind {
            ServerKind::TypeScript => {
                command.args(["--lsp", "--stdio"]);
            }
            ServerKind::Rust => {
                if let Some(path) = rust_toolchain_path() {
                    command.env("PATH", path);
                }
            }
            ServerKind::Yaml => {
                command
                    .arg(executable)
                    .arg("--stdio")
                    .env_remove("NODE_OPTIONS");
            }
        }
        let mut child = command
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("Could not start {}", program.display()))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
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
            child,
            outgoing,
            receiver,
            next_request: 1,
            documents: HashMap::new(),
            requests: HashMap::new(),
            pull_diagnostics: false,
            workspace_folders: Value::Null,
        })
    }

    fn run(&mut self, project: &Project, options: Option<Value>, notify: &Notify) -> Result<()> {
        let root_uri = file_uri(&project.root).context("Invalid language server project path")?;
        let name = project
            .root
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        self.workspace_folders = json!([{"uri": root_uri, "name": name}]);
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
            self.expire_requests();
        }
    }

    fn handle_event(&mut self, event: Event, notify: &Notify) -> Result<()> {
        match event {
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
                (*deadline <= now || matches!(pending, Pending::Query(reply) if reply.is_closed()))
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
                            send_diagnostics(notify, id, document, document.revision, params.diagnostics);
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
                            send_diagnostics(notify, id, document, revision, items);
                        }
                    }
                }
            }
        }
        Ok(())
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
                    // TypeScript 7.0.2 can exit with status 1 after a successful shutdown.
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
        document: id,
        revision,
        items,
    });
}

fn response_result(mut message: Value) -> Result<Value> {
    if let Some(error) = message.get("error") {
        bail!(
            "{}",
            error["message"]
                .as_str()
                .unwrap_or("Language server request failed")
        );
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
    fn rust_uses_the_nearest_project_and_isolates_detached_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let path = root.join("src/lib.rs");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(root.join("Cargo.toml"), "").unwrap();
        assert_eq!(
            ServerKind::Rust.project(&path),
            Some(Project {
                root,
                detached: None
            })
        );
        let standalone = temp.path().join("example.rs");
        assert_eq!(language_for(&standalone), Some((ServerKind::Rust, "rust")));
        assert_eq!(
            ServerKind::Rust.project(&standalone),
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
        let source = "mod helper;\npub fn example() -> u32 {\n    helper::answer()\n}\n";
        fs::write(&file, source).unwrap();
        fs::write(&helper, "pub fn answer() -> u32 { 42 }\n").unwrap();

        let (events, notices) = mpsc::channel();
        let mut servers = LanguageServers::new(installed_extensions_root(), None, move |notice| {
            let _ = events.send(notice);
        });
        let buffer = servers
            .attach(1, &file, Rope::from(source))
            .expect("rust-analyzer executable in extension directory");
        assert_eq!(buffer.server_name(), "rust-analyzer");
        let other = servers
            .attach(2, &helper, Rope::from(fs::read_to_string(&helper).unwrap()))
            .expect("analyzer for second document");
        assert_eq!(buffer.session(), other.session());
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
        let input = "const answer: number = \"wrong\";\nanswer;\n";
        fs::write(&path, input).unwrap();
        let (events, notices) = mpsc::channel();
        let mut typescript = LanguageServers::new(root, None, move |notice| {
            let _ = events.send(notice);
        });
        let buffer = typescript
            .attach(1, &path, Rope::from(input))
            .expect("TypeScript 7 executable in extension directory");
        let diagnostic = |revision| match notices.recv_timeout(Duration::from_secs(10)).unwrap() {
            Notice::Diagnostics {
                document: 1,
                revision: actual,
                items,
            } => {
                assert_eq!(actual, revision);
                items
            }
            Notice::Failed { error, .. } => panic!("{error}"),
            _ => panic!("Unexpected diagnostic message"),
        };
        let original = diagnostic(1);
        assert!(
            original
                .iter()
                .any(|d| d.code == Some(lsp_types::NumberOrString::Number(2322)))
        );

        assert!(!query(&buffer, "textDocument/hover", 1, 2).is_null());
        assert!(!query(&buffer, "textDocument/definition", 1, 2).is_null());
        assert!(!query(&buffer, "textDocument/completion", 1, 2).is_null());
        buffer.change(Rope::from(input.replace("\"wrong\"", "42")));
        assert!(diagnostic(2).is_empty());
        assert_eq!(fs::read_to_string(path).unwrap(), input);
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
        let mut servers = LanguageServers::new(installed_extensions_root(), None, move |notice| {
            let _ = events.send(notice);
        });
        let buffer = servers.attach(1, &path, Rope::from(source)).unwrap();
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
        assert_eq!(hover["contents"]["value"], "Number of instances");
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
