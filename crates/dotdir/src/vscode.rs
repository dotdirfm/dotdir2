use anyhow::{Context as _, Result, bail, ensure};
use gpui_kit::component::input::{Rope, RopeExt};
use lsp_types::Position;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    env, fs,
    io::{BufRead, BufReader, BufWriter, Read, Write},
    ops::Range,
    path::{Component, Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

const HOST: &str = include_str!("vscode_host.mjs");
const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_REPLY_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct Settings {
    pub node: Option<PathBuf>,
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    name: String,
    publisher: String,
    display_name: Option<String>,
    version: String,
    main: Option<PathBuf>,
    contributes: Contributions,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Contributions {
    commands: Vec<ManifestCommand>,
    configuration: Value,
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
}

pub(crate) struct ExtensionCommand {
    pub id: String,
    pub title: String,
    command: String,
}

pub(crate) fn discover(directory: &Path) -> Vec<Extension> {
    let mut extensions: BTreeMap<String, Extension> = BTreeMap::new();
    for entry in fs::read_dir(directory).into_iter().flatten().flatten() {
        let directory = entry.path();
        let extension = [directory.join("extension"), directory]
            .iter()
            .find_map(|path| read_extension(path));
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

fn read_extension(root: &Path) -> Option<Extension> {
    let manifest: Manifest =
        serde_json::from_slice(&fs::read(root.join("package.json")).ok()?).ok()?;
    let main = manifest.main?;
    if main.as_os_str().is_empty()
        || main
            .components()
            .any(|part| !matches!(part, Component::CurDir | Component::Normal(_)))
    {
        return None;
    }
    let id = format!("{}.{}", manifest.publisher, manifest.name).to_ascii_lowercase();
    let commands: Vec<_> = manifest
        .contributes
        .commands
        .into_iter()
        .filter(|command| !command.command.is_empty() && !command.title.is_empty())
        .map(|command| ExtensionCommand {
            id: format!("vscode:{id}:{}", command.command),
            title: command
                .category
                .map(|category| format!("{category}: {}", command.title))
                .unwrap_or(command.title),
            command: command.command,
        })
        .collect();
    if commands.is_empty() {
        return None;
    }
    let configuration = &manifest.contributes.configuration;
    let configuration = configuration
        .as_array()
        .map_or(std::slice::from_ref(configuration), Vec::as_slice);
    let configuration = configuration
        .iter()
        .filter_map(|group| group.get("properties")?.as_object())
        .flatten()
        .filter_map(|(key, schema)| Some((key.clone(), schema.get("default")?.clone())))
        .collect();
    Some(Extension {
        id,
        title: manifest.display_name.unwrap_or(manifest.name),
        version: manifest.version,
        commands,
        root: root.canonicalize().ok()?,
        main,
        configuration,
    })
}

fn version_key(version: &str) -> (Vec<u32>, bool) {
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
            configuration,
            editor,
        }
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
        let language = crate::syntax::language_for(path);
        let language_id = match path.extension().and_then(|ext| ext.to_str()) {
            Some("jsx") => "javascriptreact",
            Some("tsx") => "typescriptreact",
            _ if language == "text" => "plaintext",
            _ => language,
        };
        Self {
            file_name: path.into(),
            language_id,
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
}

#[derive(Serialize)]
pub(crate) struct Invocation {
    id: u64,
    extension: String,
    root: PathBuf,
    main: PathBuf,
    command: String,
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
    Edit {
        id: u64,
        serial: u64,
        version: u64,
        edits: Vec<Edit>,
    },
    Done {
        id: u64,
        error: Option<String>,
    },
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
    deadline: Instant,
}

pub(crate) struct Host {
    next_process: u64,
    process: Option<Process>,
    pub pending: Option<Pending>,
    notify: Arc<dyn Fn(Event) + Send + Sync>,
    cache: PathBuf,
}

impl Host {
    pub fn new(cache: PathBuf, notify: impl Fn(Event) + Send + Sync + 'static) -> Self {
        Self {
            next_process: 0,
            process: None,
            pending: None,
            notify: Arc::new(notify),
            cache,
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
                &self.notify,
            )?);
        }
        let id = invocation.id;
        let version = invocation.editor.version;
        self.send(Request::Run(invocation))?;
        self.pending = Some(Pending {
            id,
            document,
            version,
            text,
            deadline: Instant::now() + TIMEOUT,
        });
        Ok(())
    }

    pub fn is_current(&self, process: u64) -> bool {
        self.process
            .as_ref()
            .is_some_and(|child| child.id == process)
    }

    pub fn expire(&mut self) -> bool {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| Instant::now() >= pending.deadline)
        {
            self.reset();
            true
        } else {
            false
        }
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
        notify: &Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<Self> {
        let mut child = Command::new(node)
            .args(["--input-type=module", "--eval", HOST])
            .env_remove("NODE_OPTIONS")
            .env("NODE_COMPILE_CACHE", cache)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("Starting the VS Code host with {}", node.display()))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
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
    let usual = [
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ];
    let name = if cfg!(windows) { "node.exe" } else { "node" };
    let path = env::var_os("PATH").unwrap_or_default();
    let from_path = env::split_paths(&path)
        .chain(usual)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file());
    from_path.or_else(|| {
        // App bundles launched by Finder do not source nvm's shell setup.
        let nvm = env::var_os("NVM_DIR")
            .map(PathBuf::from)
            .or_else(|| Some(dirs::home_dir()?.join(".nvm")))?;
        fs::read_dir(nvm.join("versions/node"))
            .ok()?
            .flatten()
            .filter_map(|entry| {
                let version = entry.file_name();
                let version = version.to_str()?.strip_prefix('v')?;
                let executable = entry.path().join("bin").join(name);
                executable
                    .is_file()
                    .then(|| (version_key(version), executable))
            })
            .max()
            .map(|(_, executable)| executable)
    })
}

#[cfg(test)]
mod tests {
    use super::{
        Edit, EditBatch, Host, Reply, Settings, Snapshot, discover, offset_at, position_at,
    };
    use gpui_kit::component::input::Rope;
    use lsp_types::Position;
    use std::{env, path::PathBuf, sync::mpsc, time::Duration};

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
        let extensions = discover(&root);
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
        let mut host = Host::new(cache.path().into(), move |event| {
            let _ = send.send(event);
        });
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
