use crate::{
    extension_logs::ExtensionLogs,
    vscode_nls::{ExtensionLocalization, Locale},
};
use anyhow::{Context as _, Result, bail, ensure};
use gpui_kit::{
    Context, EntityInputHandler, Window,
    component::input::{EditorState, Rope, RopeExt},
};
use lsp_types::Position;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    io::{BufRead, BufReader, BufWriter, Read, Write},
    ops::Range,
    path::{Component, Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, OnceLock, mpsc},
    time::{Duration, Instant},
};

const HOST: &str = include_str!("vscode_host.mjs");
const LANGUAGE_HOST: &str = concat!(
    include_str!("vscode_language.mjs"),
    "\n",
    include_str!("vscode_host.mjs")
);
const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_REPLY_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(crate) enum LanguageCommandResult {
    GoToLocation { location: lsp_types::Location },
    Status { message: String },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QuickPick {
    pub title: Option<String>,
    pub place_holder: Option<String>,
    pub items: Vec<QuickPickItem>,
    #[serde(default)]
    pub match_on_description: bool,
    #[serde(default)]
    pub match_on_detail: bool,
}

#[derive(Deserialize)]
pub(crate) struct QuickPickItem {
    pub label: String,
    pub description: Option<String>,
    pub detail: Option<String>,
    #[serde(default)]
    pub separator: bool,
}

impl QuickPick {
    pub fn matches(&self, query: &str) -> Vec<usize> {
        let query = query.to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                if item.separator {
                    return None;
                }
                let label = item.label.to_lowercase();
                let description = item
                    .description
                    .as_deref()
                    .filter(|_| self.match_on_description)
                    .map(str::to_lowercase);
                let detail = item
                    .detail
                    .as_deref()
                    .filter(|_| self.match_on_detail)
                    .map(str::to_lowercase);
                query
                    .split_whitespace()
                    .all(|word| {
                        label.contains(word)
                            || description.as_ref().is_some_and(|text| text.contains(word))
                            || detail.as_ref().is_some_and(|text| text.contains(word))
                    })
                    .then_some(index)
            })
            .collect()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct Settings {
    pub node: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    pub enabled: Vec<String>,
    pub configuration: Map<String, Value>,
}

impl Settings {
    pub fn enabled(&self, id: &str) -> bool {
        self.enabled
            .iter()
            .any(|entry| entry.eq_ignore_ascii_case(id))
    }
}

pub(crate) fn language_extension<'a>(
    extensions: &'a [Extension],
    settings: &Settings,
    path: &Path,
) -> Option<(&'a Extension, &'a str)> {
    extensions
        .iter()
        .filter(|extension| settings.enabled(&extension.id))
        .find_map(|extension| Some((extension, extension.language_for(path)?)))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    name: String,
    publisher: String,
    display_name: Option<String>,
    version: String,
    main: Option<PathBuf>,
    l10n: Option<PathBuf>,
    #[serde(default)]
    activation_events: Vec<String>,
    #[serde(default)]
    contributes: Contributions,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Contributions {
    commands: Vec<ManifestCommand>,
    languages: Vec<ManifestLanguage>,
}

#[derive(Debug, Deserialize)]
struct ManifestLanguage {
    id: String,
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    filenames: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ManifestCommand {
    command: String,
    title: String,
    category: Option<String>,
}

pub(crate) struct Extension {
    pub id: String,
    pub title: String,
    pub version: String,
    pub commands: Vec<ExtensionCommand>,
    root: PathBuf,
    main: PathBuf,
    configuration: Map<String, Value>,
    activation_events: Vec<String>,
    languages: Vec<ManifestLanguage>,
    localization: ExtensionLocalization,
}

pub(crate) struct ExtensionCommand {
    pub id: String,
    pub title: String,
    pub original_title: Option<String>,
    pub command: String,
}

pub(crate) fn discover(directory: &Path, settings: &Settings) -> Vec<Extension> {
    let locale = Locale::detect(settings.locale.as_deref());
    let mut extensions: BTreeMap<String, Extension> = BTreeMap::new();
    for entry in fs::read_dir(directory).into_iter().flatten().flatten() {
        let directory = entry.path();
        let extension = [directory.join("extension"), directory]
            .iter()
            .find_map(|path| read_extension(path, &locale));
        if let Some(extension) = extension
            && extensions.get(&extension.id).is_none_or(|current| {
                (version_key(&extension.version), &extension.root)
                    > (version_key(&current.version), &current.root)
            })
        {
            extensions.insert(extension.id.clone(), extension);
        }
    }
    extensions.into_values().collect()
}

fn read_extension(root: &Path, locale: &Locale) -> Option<Extension> {
    let root = root.canonicalize().ok()?;
    let mut package_json: Value =
        serde_json::from_slice(&fs::read(root.join("package.json")).ok()?).ok()?;
    let manifest = Manifest::deserialize(&package_json).ok()?;
    let main = manifest.main?;
    if main.as_os_str().is_empty()
        || main
            .components()
            .any(|part| !matches!(part, Component::CurDir | Component::Normal(_)))
    {
        return None;
    }
    let messages = locale.manifest(&root);
    let id = format!("{}.{}", manifest.publisher, manifest.name).to_ascii_lowercase();
    let commands: Vec<_> = manifest
        .contributes
        .commands
        .into_iter()
        .filter(|command| !command.command.is_empty() && !command.title.is_empty())
        .map(|command| {
            let title = command_title(
                command.category.as_deref().map(|s| messages.translate(s)),
                messages.translate(&command.title),
            );
            let original = command_title(
                command.category.as_deref().map(|s| messages.original(s)),
                messages.original(&command.title),
            );
            ExtensionCommand {
                id: format!("vscode:{id}:{}", command.command),
                original_title: (original != title).then_some(original),
                title,
                command: command.command,
            }
        })
        .collect();
    messages.localize(&mut package_json);
    let configuration = &package_json["contributes"]["configuration"];
    let configuration = configuration
        .as_array()
        .map_or(std::slice::from_ref(configuration), Vec::as_slice);
    let configuration = configuration
        .iter()
        .filter_map(|group| group.get("properties")?.as_object())
        .flatten()
        .filter_map(|(key, schema)| Some((key.clone(), schema.get("default")?.clone())))
        .collect();
    let extension = Extension {
        id,
        title: messages
            .translate(manifest.display_name.as_deref().unwrap_or(&manifest.name))
            .into(),
        version: manifest.version,
        commands,
        localization: locale.extension(&root, manifest.l10n.as_deref(), package_json),
        root,
        main,
        configuration,
        activation_events: manifest.activation_events,
        languages: manifest.contributes.languages,
    };
    (!extension.commands.is_empty() || extension.provides_language()).then_some(extension)
}

fn command_title(category: Option<&str>, title: &str) -> String {
    match category {
        Some(category) => format!("{category}: {title}"),
        None => title.into(),
    }
}

pub(crate) fn version_key(version: &str) -> (Vec<u32>, bool) {
    let (stable, prerelease) = version.split_once('-').unwrap_or((version, ""));
    (
        stable
            .split('.')
            .map(|part| part.parse().unwrap_or(0))
            .collect(),
        prerelease.is_empty(),
    )
}

impl Extension {
    pub fn directory(&self) -> &Path {
        &self.root
    }

    pub fn workspace_markers(&self) -> impl Iterator<Item = &str> {
        self.activation_events.iter().filter_map(|event| {
            let marker = event.strip_prefix("workspaceContains:")?;
            (!marker.is_empty()
                && !marker.contains(['*', '?', '[', '{'])
                && Path::new(marker)
                    .components()
                    .all(|part| matches!(part, Component::Normal(_) | Component::CurDir)))
            .then_some(marker)
        })
    }

    pub fn provides_language(&self) -> bool {
        !self.languages.is_empty()
            || self
                .activation_events
                .iter()
                .any(|event| event == "onLanguage" || event.starts_with("onLanguage:"))
    }

    pub fn language_for<'a>(&'a self, path: &Path) -> Option<&'a str> {
        let name = path.file_name()?.to_str()?;
        let lower = name.to_ascii_lowercase();
        if let Some(language) = self.languages.iter().find(|language| {
            language.filenames.iter().any(|file| file == name)
                || language
                    .extensions
                    .iter()
                    .any(|suffix| lower.ends_with(&suffix.to_ascii_lowercase()))
        }) {
            return Some(&language.id);
        }
        let language = language_id(path);
        self.activation_events
            .iter()
            .any(|event| {
                event == "onLanguage" || event.strip_prefix("onLanguage:") == Some(language)
            })
            .then_some(language)
    }

    pub fn language_launch(&self, settings: &Settings, config: &Path) -> crate::lsp::Launch {
        let cache = dirs::cache_dir()
            .unwrap_or_else(env::temp_dir)
            .join("dev.dotdir.native/node");
        let configured_node = settings.node.clone();
        crate::lsp::Launch {
            name: self.title.clone(),
            script: LANGUAGE_HOST,
            command: Box::new(move || {
                let node = node_path(configured_node.as_deref()).context(
                    "Node.js 22.15 or newer is required. Set vscode.node in settings.json to its executable",
                )?;
                let mut command = node_command(&node);
                command
                    .env("DOTDIR_VSCODE_LANGUAGE", "1")
                    .env("NODE_COMPILE_CACHE", &cache);
                Ok(command)
            }),
            options: json!({
                "extension": self.id,
                "root": self.root,
                "main": self.main,
                "localization": self.localization,
                "configuration": self.configuration,
                "configurationOverrides": settings.configuration,
                "storage": config.join("vscode-storage"),
            }),
        }
    }

    pub fn invocation(
        &self,
        command: &ExtensionCommand,
        settings: &Settings,
        id: u64,
        editor: Snapshot,
    ) -> Invocation {
        let mut configuration = self.configuration.clone();
        configuration.extend(settings.configuration.clone());
        Invocation {
            id,
            extension: self.id.clone(),
            root: self.root.clone(),
            main: self.main.clone(),
            command: command.command.clone(),
            localization: self.localization.clone(),
            configuration,
            editor,
        }
    }
}

fn language_id(path: &Path) -> &'static str {
    let language = crate::syntax::language_for(path);
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jsx") => "javascriptreact",
        Some("tsx") => "typescriptreact",
        _ if language == "text" => "plaintext",
        _ => language,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Snapshot {
    file_name: PathBuf,
    language_id: &'static str,
    version: u64,
    text: String,
    anchor: Position,
    active: Position,
}

impl Snapshot {
    pub fn new(
        path: &Path,
        version: u64,
        text: &Rope,
        selection: Range<usize>,
        reversed: bool,
    ) -> Self {
        let (anchor, active) = if reversed {
            (selection.end, selection.start)
        } else {
            (selection.start, selection.end)
        };
        Self {
            file_name: path.into(),
            language_id: language_id(path),
            version,
            text: text.to_string(),
            anchor: position_at(text, anchor),
            active: position_at(text, active),
        }
    }
}

fn line_content(text: &Rope, row: usize) -> Range<usize> {
    let start = text.line_start_offset(row);
    let mut end = text.line_end_offset(row);
    if start < end && row + 1 < text.lines_len() && text.char_at(end - 1) == Some('\r') {
        end -= 1;
    }
    start..end
}

fn position_at(text: &Rope, offset: usize) -> Position {
    let point = text.offset_to_point(offset);
    let line = text.slice(line_content(text, point.row));
    let column = line.byte_to_utf16_idx(point.column.min(line.len()));
    Position::new(point.row as u32, column as u32)
}

fn offset_at(text: &Rope, position: Position) -> Option<usize> {
    let row = position.line as usize;
    if row >= text.lines_len() {
        return None;
    }
    let range = line_content(text, row);
    let line = text.slice(range.clone());
    let column = position.character as usize;
    if column > line.len_utf16() {
        return None;
    }
    let byte = line.utf16_to_byte_idx(column);
    (line.byte_to_utf16_idx(byte) == column).then_some(range.start + byte)
}

#[derive(Debug, Deserialize)]
pub(crate) struct Edit {
    start: Position,
    end: Position,
    text: String,
}

impl From<lsp_types::TextEdit> for Edit {
    fn from(edit: lsp_types::TextEdit) -> Self {
        Self {
            start: edit.range.start,
            end: edit.range.end,
            text: edit.new_text,
        }
    }
}

pub(crate) struct EditBatch {
    edits: Vec<(Range<usize>, String)>,
    pub range: Range<usize>,
    pub text: String,
}

impl EditBatch {
    pub fn prepare(source: &Rope, edits: Vec<Edit>) -> Option<Self> {
        let mut edits = edits
            .into_iter()
            .map(|edit| {
                let start = offset_at(source, edit.start)?;
                let end = offset_at(source, edit.end)?;
                (start <= end).then_some((start..end, edit.text))
            })
            .collect::<Option<Vec<_>>>()?;
        edits.sort_by_key(|(range, _)| (range.start, range.end));
        let start = edits.first()?.0.start;
        let mut end = start;
        let mut text = String::new();
        for (range, replacement) in &edits {
            if range.start < end {
                return None;
            }
            text.extend(source.slice(end..range.start).chunks());
            text.push_str(replacement);
            end = range.end;
        }
        Some(Self {
            edits,
            range: start..end,
            text,
        })
    }

    pub fn translate(&self, offset: usize) -> usize {
        let mut delta = 0isize;
        for (range, text) in &self.edits {
            if offset < range.start {
                break;
            }
            if offset < range.end {
                return range.start.saturating_add_signed(delta)
                    + (offset - range.start).min(text.len());
            }
            delta += text.len() as isize - range.len() as isize;
        }
        offset.saturating_add_signed(delta)
    }

    pub(crate) fn apply(
        self,
        editor: &mut EditorState,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) -> bool {
        if editor
            .text()
            .slice(self.range.clone())
            .bytes()
            .eq(self.text.bytes())
        {
            return false;
        }
        let selection = editor.selected_range();
        let reversed = editor.is_selection_reversed();
        let translated = self.translate(selection.start)..self.translate(selection.end);
        editor.unmark_text(window, cx);
        editor.set_selected_range(self.range, cx);
        editor.replace(self.text, window, cx);
        editor.set_selected_range_with_direction(translated, reversed, cx);
        true
    }
}

#[derive(Serialize)]
pub(crate) struct Invocation {
    id: u64,
    extension: String,
    root: PathBuf,
    main: PathBuf,
    command: String,
    localization: ExtensionLocalization,
    configuration: Map<String, Value>,
    editor: Snapshot,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Request {
    Run(Invocation),
    Ack {
        id: u64,
        serial: u64,
        editor: Option<Snapshot>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Reply {
    Log {
        extension: String,
        message: String,
    },
    Edit {
        id: u64,
        serial: u64,
        version: u64,
        edits: Vec<Edit>,
    },
    Done {
        id: u64,
        error: Option<CommandError>,
    },
}

#[derive(Deserialize)]
pub(crate) struct CommandError {
    message: String,
    stack: Option<String>,
}

impl From<CommandError> for anyhow::Error {
    fn from(error: CommandError) -> Self {
        host_error(&error.message, error.stack.as_deref())
    }
}

pub(crate) fn host_error(message: &str, stack: Option<&str>) -> anyhow::Error {
    match stack.filter(|stack| !stack.is_empty()) {
        Some(stack) => anyhow::anyhow!("{stack}").context(message.to_owned()),
        None => anyhow::anyhow!("{message}"),
    }
}

pub(crate) struct Event {
    pub process: u64,
    pub reply: Result<Reply>,
}

pub(crate) struct Pending {
    pub id: u64,
    pub document: u64,
    pub version: u64,
    pub text: Rope,
    pub extension: String,
    pub command: String,
    deadline: Instant,
}

pub(crate) struct Host {
    next_process: u64,
    process: Option<Process>,
    pub pending: Option<Pending>,
    notify: Arc<dyn Fn(Event) + Send + Sync>,
    cache: PathBuf,
    logs: ExtensionLogs,
}

impl Host {
    pub fn new(
        cache: PathBuf,
        logs: ExtensionLogs,
        notify: impl Fn(Event) + Send + Sync + 'static,
    ) -> Self {
        Self {
            next_process: 0,
            process: None,
            pending: None,
            notify: Arc::new(notify),
            cache,
            logs,
        }
    }

    pub fn run(
        &mut self,
        settings: &Settings,
        invocation: Invocation,
        document: u64,
        text: Rope,
    ) -> Result<()> {
        ensure!(
            self.pending.is_none(),
            "A VS Code extension command is already running"
        );
        if self.process.is_none() {
            let node = node_path(settings.node.as_deref()).context(
                "Node.js 22.15 or newer is required. Set vscode.node in settings.json to its executable",
            )?;
            self.next_process += 1;
            self.process = Some(Process::start(
                self.next_process,
                &node,
                &self.cache,
                &self.logs,
                &self.notify,
            )?);
        }
        let id = invocation.id;
        let version = invocation.editor.version;
        self.logs
            .source(&invocation.extension, "VS Code commands")
            .append(&format!("Running {}", invocation.command));
        let extension = invocation.extension.clone();
        let command = invocation.command.clone();
        self.send(Request::Run(invocation))?;
        self.pending = Some(Pending {
            id,
            document,
            version,
            text,
            extension,
            command,
            deadline: Instant::now() + TIMEOUT,
        });
        Ok(())
    }

    pub fn is_current(&self, process: u64) -> bool {
        self.process
            .as_ref()
            .is_some_and(|child| child.id == process)
    }

    pub fn expire(&mut self) -> Option<Pending> {
        if Instant::now() < self.pending.as_ref()?.deadline {
            return None;
        }
        let pending = self.pending.take();
        self.reset();
        pending
    }

    pub fn reset(&mut self) {
        self.pending = None;
        self.process = None;
    }

    pub fn acknowledge(&mut self, id: u64, serial: u64, editor: Option<Snapshot>) -> Result<()> {
        self.send(Request::Ack { id, serial, editor })
    }

    fn send(&mut self, request: Request) -> Result<()> {
        let sent = self
            .process
            .as_ref()
            .is_some_and(|process| process.input.send(request).is_ok());
        if !sent {
            self.reset();
            bail!("VS Code extension host disconnected");
        }
        Ok(())
    }
}

struct Process {
    id: u64,
    child: Child,
    input: mpsc::Sender<Request>,
}

impl Process {
    fn start(
        id: u64,
        node: &Path,
        cache: &Path,
        logs: &ExtensionLogs,
        notify: &Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<Self> {
        let mut child = host_command(node, cache, HOST)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("Starting the VS Code host with {}", node.display()))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        logs.capture(
            "VS Code commands".into(),
            child.stderr.take().expect("piped stderr"),
        );
        let (input, requests) = mpsc::channel();
        let notify_write = notify.clone();
        std::thread::spawn(move || {
            let mut writer = BufWriter::new(stdin);
            for request in requests {
                let mut send = || -> Result<()> {
                    serde_json::to_writer(&mut writer, &request)?;
                    writer.write_all(b"\n")?;
                    writer.flush()?;
                    Ok(())
                };
                if let Err(error) = send() {
                    notify_write(Event {
                        process: id,
                        reply: Err(error.context("Sending to the VS Code host")),
                    });
                    break;
                }
            }
        });
        let notify = notify.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                let reply = read_reply(&mut reader, &mut line);
                let ended = reply.is_err();
                notify(Event { process: id, reply });
                if ended {
                    break;
                }
            }
        });
        Ok(Self { id, child, input })
    }
}

pub(crate) fn node_command(node: &Path) -> Command {
    let mut command = Command::new(node);
    command.env_remove("NODE_OPTIONS");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

pub(crate) fn host_command(node: &Path, cache: &Path, script: &str) -> Command {
    let mut command = node_command(node);
    command
        .args(["--input-type=module", "--eval", script])
        .env_remove("DOTDIR_VSCODE_LANGUAGE")
        .env("NODE_COMPILE_CACHE", cache);
    command
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_reply(reader: &mut impl BufRead, line: &mut Vec<u8>) -> Result<Reply> {
    line.clear();
    let bytes = reader.take(MAX_REPLY_BYTES + 1).read_until(b'\n', line)?;
    ensure!(
        bytes > 0,
        "VS Code extension host exited. Check that Node.js 22.15 or newer is installed"
    );
    ensure!(
        bytes as u64 <= MAX_REPLY_BYTES && line.last() == Some(&b'\n'),
        "Invalid or oversized VS Code extension host message"
    );
    serde_json::from_slice(line).context("Reading the VS Code extension host reply")
}

pub(crate) fn node_path(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = configured {
        return Some(path.into());
    }
    static DISCOVERED: OnceLock<PathBuf> = OnceLock::new();
    if let Some(path) = DISCOVERED.get().filter(|path| path.is_file()) {
        return Some(path.clone());
    }
    let usual = [
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ];
    let name = if cfg!(windows) { "node.exe" } else { "node" };
    let path = env::var_os("PATH").unwrap_or_default();
    // App bundles launched by Finder do not source nvm's shell setup.
    let nvm = env::var_os("NVM_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".nvm")));
    let mut nvm_versions = nvm
        .and_then(|dir| fs::read_dir(dir.join("versions/node")).ok())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let version = entry.file_name();
            Some((
                version_key(version.to_str()?.strip_prefix('v')?),
                entry.path().join("bin"),
            ))
        })
        .collect::<Vec<_>>();
    nvm_versions.sort_unstable_by(|a, b| b.cmp(a));
    let mut seen = HashSet::new();
    let node = env::split_paths(&path)
        .chain(usual)
        .chain(nvm_versions.into_iter().map(|(_, directory)| directory))
        .map(|directory| directory.join(name))
        .find(|candidate| {
            candidate.is_file() && seen.insert(candidate.clone()) && compatible_node(candidate)
        })?;
    let _ = DISCOVERED.set(node.clone());
    Some(node)
}

fn compatible_node(path: &Path) -> bool {
    let output = node_command(path).arg("--version").output();
    output
        .ok()
        .filter(|result| result.status.success())
        .and_then(|result| {
            let version = std::str::from_utf8(&result.stdout)
                .ok()?
                .trim()
                .strip_prefix('v')?;
            let mut numbers = version.split('.');
            let major = numbers.next()?.parse::<u32>().ok()?;
            let minor = numbers.next()?.parse::<u32>().ok()?;
            Some((major, minor) >= (22, 15))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{
        Edit, EditBatch, Host, Reply, Settings, Snapshot, discover, offset_at, position_at,
    };
    use crate::extension_logs::ExtensionLogs;
    use gpui_kit::component::input::Rope;
    use lsp_types::Position;
    use std::{env, path::PathBuf, sync::mpsc, time::Duration};

    #[test]
    fn localizes_extension_names_and_commands_with_locale_fallbacks() {
        let temp = tempfile::tempdir().unwrap();
        let package = temp.path().join("example/extension");
        std::fs::create_dir_all(package.join("l10n")).unwrap();
        for (file, contents) in [
            (
                "package.json",
                r#"{
                "name":"example", "publisher":"Fixture", "version":"1.0.0",
                "displayName":"%name%", "main":"main.js", "l10n":"./l10n",
                "contributes":{"commands":[
                    {"command":"example.open", "title":"%open%", "category":"%category%"},
                    {"command":"example.copy", "title":"%copy%"},
                    {"command":"example.help", "title":"100% help"}
                ]}
            }"#,
            ),
            (
                "package.nls.json",
                r#"{"name":"Example","category":{"message":"Files","comment":["category"]},"open":"Open","copy":"Copy"}"#,
            ),
            (
                "package.nls.fr.json",
                r#"{"name":"Exemple","category":"Fichiers","open":"Ouvrir"}"#,
            ),
            (
                "package.nls.fr-ca.json",
                r#"{"open":"Ouvrir ici","copy":42}"#,
            ),
            (
                "l10n/bundle.l10n.fr.json",
                r#"{"Hello, {name}":"Bonjour, {name}"}"#,
            ),
        ] {
            std::fs::write(package.join(file), contents).unwrap();
        }
        let settings = Settings {
            locale: Some("fr_CA.UTF-8".into()),
            ..Default::default()
        };
        let extensions = discover(temp.path(), &settings);
        let extension = &extensions[0];
        assert_eq!(extension.title, "Exemple");
        assert_eq!(
            extension.commands[0].id,
            "vscode:fixture.example:example.open"
        );
        assert_eq!(extension.commands[0].title, "Fichiers: Ouvrir ici");
        assert_eq!(
            extension.commands[0].original_title.as_deref(),
            Some("Files: Open")
        );
        assert_eq!(extension.commands[1].title, "Copy");
        assert_eq!(extension.commands[2].title, "100% help");
        let localization = serde_json::to_value(&extension.localization).unwrap();
        assert_eq!(localization["locale"], "fr-ca");
        assert_eq!(localization["packageJson"]["displayName"], "Exemple");
        assert_eq!(
            localization["bundle"]["messages"]["Hello, {name}"],
            "Bonjour, {name}"
        );

        let base = discover(
            temp.path(),
            &Settings {
                locale: Some("ja".into()),
                ..Default::default()
            },
        );
        assert_eq!(base[0].commands[0].title, "Files: Open");
        assert!(base[0].commands[0].original_title.is_none());
    }

    #[test]
    fn edit_batch_validates_utf16_and_preserves_text_between_replacements() {
        let source = Rope::from("z😀\r\nkeep\r\nβ");
        let edit = |start, end, text: &str| Edit {
            start,
            end,
            text: text.into(),
        };
        let batch = EditBatch::prepare(
            &source,
            vec![
                edit(Position::new(2, 0), Position::new(2, 1), "b"),
                edit(Position::new(0, 1), Position::new(0, 3), "a"),
            ],
        )
        .unwrap();
        assert_eq!(batch.range, 1..source.len());
        assert_eq!(batch.text, "a\r\nkeep\r\nb");
        assert_eq!(batch.translate(source.len()), 1 + batch.text.len());
        assert_eq!(position_at(&source, 6), Position::new(0, 3)); // Inside CRLF.
        assert_eq!(offset_at(&source, Position::new(0, 4)), None);
        assert_eq!(offset_at(&source, Position::new(0, 2)), None); // Inside surrogate pair.
        assert!(
            EditBatch::prepare(
                &source,
                vec![
                    edit(Position::new(0, 0), Position::new(1, 0), ""),
                    edit(Position::new(0, 1), Position::new(0, 3), "a"),
                ],
            )
            .is_none()
        );
    }

    #[test]
    #[ignore = "requires Node.js and the published Sort Lines VSIX unpacked in DOTDIR_SORT_LINES_EXTENSIONS"]
    fn real_sort_lines_runs_in_node_and_waits_for_the_editor() {
        let root = PathBuf::from(env::var_os("DOTDIR_SORT_LINES_EXTENSIONS").unwrap());
        let extensions = discover(&root, &Settings::default());
        let extension = extensions
            .iter()
            .find(|extension| extension.id == "tyriar.sort-lines")
            .unwrap();
        let command = extension
            .commands
            .iter()
            .find(|entry| entry.command == "sortLines.sortLines")
            .unwrap();
        let cache = tempfile::tempdir().unwrap();
        let (send, receive) = mpsc::channel();
        let mut host = Host::new(
            cache.path().into(),
            ExtensionLogs::default(),
            move |event| {
                let _ = send.send(event);
            },
        );
        let settings = Settings::default();
        let path = root.join("fixture.txt");
        let text = Rope::from("header\r\nzebra\r\nbanana\r\napple\r\nfooter");
        let selection = 8..28;
        let snapshot = Snapshot::new(&path, 1, &text, selection, false);
        host.run(
            &settings,
            extension.invocation(command, &settings, 1, snapshot),
            42,
            text.clone(),
        )
        .unwrap();
        let event = receive.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(host.is_current(event.process));
        let Reply::Edit {
            id,
            serial,
            version,
            edits,
        } = event.reply.unwrap()
        else {
            panic!("Sort Lines did not ask the editor to apply an edit");
        };
        assert_eq!((id, version), (1, 1));
        let batch = EditBatch::prepare(&text, edits).unwrap();
        assert_eq!(batch.range, 8..28);
        assert_eq!(batch.text, "apple\r\nbanana\r\nzebra");
        assert!(receive.try_recv().is_err());
        let after = Rope::from("header\r\napple\r\nbanana\r\nzebra\r\nfooter");
        host.acknowledge(
            id,
            serial,
            Some(Snapshot::new(&path, 2, &after, 8..28, false)),
        )
        .unwrap();
        let event = receive.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            event.reply.unwrap(),
            Reply::Done { id: 1, error: None }
        ));
    }
}
