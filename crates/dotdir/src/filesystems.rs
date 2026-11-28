use crate::{
    archive_passwords::{ArchiveAccess, ArchiveKey, ArchivePasswords, PasswordScope},
    extension_logs::ExtensionLogs,
    vscode,
};
use anyhow::{Context, Result, bail, ensure};
use dotdir_core::copy::CancelToken;
use dotdir_remote::{Connection, Profile, Protocol, RemoteFile};
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

pub const LOG_SCHEME: &str = "ext-logs";
const LOG_FILE: &str = "output.log";
const LIST_LIMIT: usize = 8 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const SEGMENT_ENCODING: &percent_encoding::AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b':');

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProviderPath {
    scheme: String,
    parts: Vec<String>,
}

impl TryFrom<String> for ProviderPath {
    type Error = anyhow::Error;

    fn try_from(uri: String) -> Result<Self> {
        Self::parse(&uri)?.context("Expected a filesystem protocol URI")
    }
}

impl From<ProviderPath> for String {
    fn from(path: ProviderPath) -> Self {
        path.to_string()
    }
}

fn valid_scheme(scheme: &str) -> bool {
    let mut bytes = scheme.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

fn valid_component(name: &str) -> bool {
    !name.is_empty()
        && !matches!(name, "." | "..")
        && !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\'))
}

impl ProviderPath {
    pub fn parse(uri: &str) -> Result<Option<Self>> {
        let Some((scheme, rest)) = uri.split_once("://") else {
            return Ok(None);
        };
        // Local paths can contain the same punctuation in a directory name.
        if scheme.starts_with(['/', '.']) || scheme.contains(['/', '\\']) {
            return Ok(None);
        }
        ensure!(valid_scheme(scheme), "Invalid filesystem protocol");
        ensure!(
            !rest.contains(['?', '#', '@']),
            "Encode special characters in filesystem locations"
        );
        let rest = rest.trim_matches('/');
        let mut parts = Vec::new();
        if !rest.is_empty() {
            for part in rest.split('/') {
                let part = percent_decode_str(part).decode_utf8()?.into_owned();
                ensure!(valid_component(&part), "Invalid filesystem path component");
                parts.push(part);
            }
        }
        Ok(Some(Self {
            scheme: scheme.to_ascii_lowercase(),
            parts,
        }))
    }

    pub fn root(scheme: &str) -> Self {
        Self {
            scheme: scheme.into(),
            parts: Vec::new(),
        }
    }

    pub fn name(&self) -> Option<&str> {
        self.parts.last().map(String::as_str)
    }

    pub fn is_log(&self) -> bool {
        self.scheme == LOG_SCHEME
    }

    pub fn parent(&self) -> Option<Self> {
        let (_, parts) = self.parts.split_last()?;
        Some(Self {
            scheme: self.scheme.clone(),
            parts: parts.to_vec(),
        })
    }

    pub fn child(&self, name: String) -> Self {
        let mut child = self.clone();
        child.parts.push(name);
        child
    }

    pub fn resolve(&self, relative: &str) -> Result<Self> {
        let mut path = self.clone();
        for part in relative.trim_end_matches('/').split('/') {
            match part {
                ".." => {
                    path.parts.pop();
                }
                "." => {}
                _ => {
                    let part = percent_decode_str(part).decode_utf8()?.into_owned();
                    ensure!(valid_component(&part), "Invalid filesystem path component");
                    path.parts.push(part);
                }
            }
        }
        Ok(path)
    }
}

impl std::fmt::Display for ProviderPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}://", self.scheme)?;
        for (i, part) in self.parts.iter().enumerate() {
            if i > 0 {
                write!(f, "/")?;
            }
            write!(f, "{}", utf8_percent_encode(part, SEGMENT_ENCODING))?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderEntry {
    pub name: String,
    #[serde(default)]
    pub directory: bool,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub modified_ms: u64,
}

trait Provider: Send + Sync {
    fn read_directory(
        &self,
        path: &ProviderPath,
        cancel: &CancelToken,
    ) -> Result<Vec<ProviderEntry>>;
    fn read_file(&self, path: &ProviderPath, limit: usize, cancel: &CancelToken)
    -> Result<Vec<u8>>;
}

#[derive(Clone)]
pub struct Filesystems {
    providers: Arc<HashMap<String, Arc<dyn Provider>>>,
    remote: Arc<HashMap<String, RemoteProvider>>,
    pub(crate) archive_passwords: Arc<ArchivePasswords>,
    password_scope: Option<Arc<PasswordScope>>,
    interactive: bool,
}

impl Filesystems {
    pub fn new(logs: ExtensionLogs) -> Self {
        Self {
            providers: Self::builtin_providers(logs),
            remote: Arc::new(HashMap::new()),
            archive_passwords: Arc::new(ArchivePasswords::unattended()),
            password_scope: None,
            interactive: true,
        }
    }

    fn builtin_providers(logs: ExtensionLogs) -> Arc<HashMap<String, Arc<dyn Provider>>> {
        Arc::new(HashMap::from([(
            LOG_SCHEME.into(),
            Arc::new(logs) as Arc<dyn Provider>,
        )]))
    }

    pub(crate) fn rebuild_providers(&self, logs: ExtensionLogs) -> Self {
        Self {
            providers: Self::builtin_providers(logs),
            ..self.clone()
        }
    }

    pub(crate) fn archive_operation(&self) -> Self {
        let mut filesystems = self.clone();
        filesystems
            .password_scope
            .get_or_insert_with(Default::default);
        filesystems
    }

    pub(crate) fn without_password_prompts(&self) -> Self {
        let mut filesystems = self.clone();
        filesystems.interactive = false;
        filesystems
    }

    pub(crate) fn password_access<'a>(
        &'a self,
        key: &'a ArchiveKey,
        cancel: &'a CancelToken,
    ) -> ArchiveAccess<'a> {
        ArchiveAccess {
            service: &self.archive_passwords,
            scope: self.password_scope.as_deref(),
            key,
            cancel,
            interactive: self.interactive,
            requested: Default::default(),
        }
    }

    fn provider(&self, path: &ProviderPath) -> Result<&dyn Provider> {
        if let Some(provider) = self.remote.get(&path.scheme) {
            return Ok(provider);
        }
        self.providers
            .get(&path.scheme)
            .map(Arc::as_ref)
            .with_context(|| format!("No filesystem provider for {}://", path.scheme))
    }

    pub fn check(&self, path: &ProviderPath) -> Result<()> {
        self.provider(path).map(|_| ())
    }

    pub fn read_directory(&self, path: &ProviderPath) -> Result<Vec<ProviderEntry>> {
        self.read_directory_cancellable(path, &CancelToken::new())
    }

    pub fn read_directory_cancellable(
        &self,
        path: &ProviderPath,
        cancel: &CancelToken,
    ) -> Result<Vec<ProviderEntry>> {
        let entries = self.provider(path)?.read_directory(path, cancel)?;
        let mut names = HashSet::new();
        for entry in &entries {
            ensure!(
                valid_component(&entry.name),
                "Invalid provider entry name: {:?}",
                entry.name
            );
            ensure!(
                names.insert(&entry.name),
                "Duplicate provider entry: {:?}",
                entry.name
            );
        }
        Ok(entries)
    }

    pub fn read_file(
        &self,
        path: &ProviderPath,
        limit: usize,
        cancel: &CancelToken,
    ) -> Result<Vec<u8>> {
        ensure!(!cancel.is_cancelled(), "File read cancelled");
        let data = self.provider(path)?.read_file(path, limit, cancel)?;
        ensure!(
            data.len() <= limit,
            "Files over 16 MiB require the streaming viewer"
        );
        Ok(data)
    }

    pub fn set_remote_profiles(&mut self, profiles: &[Profile], settings_path: &Path) {
        let namespace = credential_namespace(settings_path);
        self.remote = Arc::new(
            Protocol::ALL
                .into_iter()
                .filter(|protocol| protocol.enabled())
                .map(|protocol| {
                    let mounts = profiles
                        .iter()
                        .filter(|profile| profile.protocol == protocol)
                        .map(|profile| {
                            (
                                profile.name.clone(),
                                Mount {
                                    profile: profile.clone(),
                                    connection: Mutex::default(),
                                },
                            )
                        })
                        .collect();
                    (
                        protocol.scheme().to_owned(),
                        RemoteProvider {
                            namespace: namespace.clone(),
                            mounts,
                        },
                    )
                })
                .collect(),
        );
    }

    pub fn is_remote(&self, path: &ProviderPath) -> bool {
        self.remote.contains_key(&path.scheme)
    }

    pub fn open_remote(&self, path: &ProviderPath, cancel: &CancelToken) -> Result<RemoteFile> {
        self.remote
            .get(&path.scheme)
            .context("Expected a remote connection")?
            .open(path, cancel)
    }

    pub fn register(
        &mut self,
        package: NodeManifest,
        root: &Path,
        node: Option<PathBuf>,
        logs: ExtensionLogs,
    ) -> Result<()> {
        ensure!(
            package.api_version == 1 && valid_component(&package.id),
            "Invalid filesystem extension manifest"
        );
        ensure!(
            !package.filesystems.is_empty(),
            "Filesystem extension has no protocols"
        );
        let root = root.canonicalize()?;
        let entry = root
            .join(package.main)
            .canonicalize()
            .context("Find filesystem extension entry point")?;
        ensure!(
            entry.starts_with(&root) && entry.is_file(),
            "Filesystem extension entry point must be inside its package"
        );
        let mut schemes = HashSet::new();
        for scheme in &package.filesystems {
            ensure!(
                valid_scheme(scheme) && scheme == &scheme.to_ascii_lowercase() && scheme != "file",
                "Invalid filesystem protocol: {scheme}"
            );
            ensure!(
                schemes.insert(scheme)
                    && !self.providers.contains_key(scheme)
                    && !self.remote.contains_key(scheme),
                "Filesystem protocol is already registered: {scheme}"
            );
        }
        logs.register(&package.id);
        let provider = Arc::new(NodeProvider {
            id: package.id,
            entry,
            root,
            node,
            logs,
        });
        let providers = Arc::make_mut(&mut self.providers);
        for scheme in package.filesystems {
            providers.insert(scheme, provider.clone());
        }
        Ok(())
    }
}

pub(crate) fn credential_namespace(settings_path: &Path) -> String {
    format!(
        "{:x}",
        Sha256::digest(settings_path.as_os_str().as_encoded_bytes())
    )
}

struct Mount {
    profile: Profile,
    connection: Mutex<Option<Connection>>,
}

struct RemoteProvider {
    namespace: String,
    mounts: std::collections::BTreeMap<String, Mount>,
}

impl RemoteProvider {
    fn mount<'a>(&'a self, path: &'a ProviderPath) -> Result<(&'a Mount, &'a [String])> {
        let (name, parts) = path
            .parts
            .split_first()
            .context("Select a saved connection")?;
        let mount = self
            .mounts
            .get(name)
            .with_context(|| format!("Unknown connection: {name}"))?;
        Ok((mount, parts))
    }

    fn connection(&self, mount: &Mount, cancel: &CancelToken) -> Result<Connection> {
        let mut slot = mount
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Remote connection is unavailable"))?;
        if let Some(connection) = &*slot {
            return Ok(connection.clone());
        }
        let connection = mount.profile.connect(&self.namespace, cancel)?;
        *slot = Some(connection.clone());
        Ok(connection)
    }

    fn open(&self, path: &ProviderPath, cancel: &CancelToken) -> Result<RemoteFile> {
        let (mount, parts) = self.mount(path)?;
        self.connection(mount, cancel)?.open(parts, cancel)
    }
}

impl Provider for RemoteProvider {
    fn read_directory(
        &self,
        path: &ProviderPath,
        cancel: &CancelToken,
    ) -> Result<Vec<ProviderEntry>> {
        if path.parts.is_empty() {
            return Ok(self
                .mounts
                .keys()
                .map(|name| ProviderEntry {
                    name: name.clone(),
                    directory: true,
                    size: 0,
                    modified_ms: 0,
                })
                .collect());
        }
        let (mount, parts) = self.mount(path)?;
        Ok(self
            .connection(mount, cancel)?
            .entries(parts, cancel)?
            .into_iter()
            .map(|entry| ProviderEntry {
                name: entry.name,
                directory: entry.directory,
                size: entry.size,
                modified_ms: entry.modified_ms,
            })
            .collect())
    }

    fn read_file(
        &self,
        path: &ProviderPath,
        limit: usize,
        cancel: &CancelToken,
    ) -> Result<Vec<u8>> {
        let file = self.open(path, cancel)?;
        ensure!(
            file.size <= limit as u64,
            "Remote file exceeds the preview limit"
        );
        let mut data = Vec::with_capacity(file.size as usize);
        file.reader.take(file.size).read_to_end(&mut data)?;
        ensure!(
            data.len() as u64 == file.size,
            "Remote file changed during the read"
        );
        Ok(data)
    }
}

impl Provider for ExtensionLogs {
    fn read_directory(&self, path: &ProviderPath, _: &CancelToken) -> Result<Vec<ProviderEntry>> {
        match path.parts.as_slice() {
            [] => Ok(self
                .groups()
                .into_iter()
                .filter(|name| valid_component(name))
                .map(|name| ProviderEntry {
                    name,
                    directory: true,
                    size: 0,
                    modified_ms: 0,
                })
                .collect()),
            [group] => {
                let output = self
                    .group_output(group)
                    .context("Extension log folder does not exist")?;
                Ok(vec![ProviderEntry {
                    name: LOG_FILE.into(),
                    directory: false,
                    size: output.len() as u64,
                    modified_ms: 0,
                }])
            }
            _ => bail!("Extension log folder does not exist"),
        }
    }

    fn read_file(&self, path: &ProviderPath, _: usize, _: &CancelToken) -> Result<Vec<u8>> {
        let [group, file] = path.parts.as_slice() else {
            bail!("Extension log file does not exist")
        };
        ensure!(file == LOG_FILE, "Extension log file does not exist");
        Ok(self
            .group_output(group)
            .context("Extension log file does not exist")?
            .into_bytes())
    }
}

#[derive(Deserialize)]
pub struct NodeManifest {
    pub id: String,
    api_version: u32,
    main: PathBuf,
    filesystems: Vec<String>,
}

struct NodeProvider {
    id: String,
    entry: PathBuf,
    root: PathBuf,
    node: Option<PathBuf>,
    logs: ExtensionLogs,
}

impl NodeProvider {
    fn request(&self, operation: &str, path: &ProviderPath, limit: usize) -> Result<Vec<u8>> {
        let node = vscode::node_path(self.node.as_deref())
            .context("Node.js 22.15 or newer is required for filesystem extensions")?;
        let cache = dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("dev.dotdir.native/node");
        let address =
            json!({"uri": path.to_string(), "scheme": path.scheme, "segments": path.parts})
                .to_string();
        let mut child = vscode::host_command(&node, &cache, include_str!("filesystem_host.mjs"))
            .arg("--")
            .arg(&self.entry)
            .arg(operation)
            .arg(address)
            .arg(limit.to_string())
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("Starting filesystem extension {}", self.id))?;
        self.logs
            .capture(self.id.clone(), child.stderr.take().expect("piped stderr"));
        let output = child.stdout.take().expect("piped stdout");
        let (tx, rx) = mpsc::channel();
        let capacity = limit.max(8192) + 2;
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = output
                .take(capacity as u64)
                .read_to_end(&mut bytes)
                .map(|_| bytes);
            let _ = tx.send(result);
        });
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let result = (|| -> Result<Vec<u8>> {
            let bytes = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .context("Filesystem provider request timed out")??;
            ensure!(
                bytes.len() < capacity,
                "Provider result exceeds the size limit"
            );
            let status = loop {
                if let Some(status) = child.try_wait()? {
                    break status;
                }
                ensure!(
                    Instant::now() < deadline,
                    "Filesystem provider request timed out"
                );
                std::thread::sleep(Duration::from_millis(5));
            };
            ensure!(status.success(), "Filesystem provider exited with {status}");
            let (kind, bytes) = bytes
                .split_first()
                .context("Filesystem provider returned no response")?;
            if *kind == 1 {
                bail!("{}", String::from_utf8_lossy(bytes));
            }
            ensure!(
                *kind == 0 && bytes.len() <= limit,
                "Invalid filesystem provider response"
            );
            Ok(bytes.to_vec())
        })();
        if let Err(error) = &result {
            let _ = child.kill();
            let _ = child.wait();
            self.logs
                .append(&self.id, &format!("{operation} {}: {error:#}", path));
        }
        result
    }
}

impl Provider for NodeProvider {
    fn read_directory(&self, path: &ProviderPath, _: &CancelToken) -> Result<Vec<ProviderEntry>> {
        let data = self.request("readDirectory", path, LIST_LIMIT)?;
        Ok(serde_json::from_slice(&data)
            .context("Invalid filesystem provider directory entries")?)
    }

    fn read_file(&self, path: &ProviderPath, limit: usize, _: &CancelToken) -> Result<Vec<u8>> {
        self.request("readFile", path, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        archive::{Location, SavedLocation},
        model::{SortOrder, read_location},
    };

    fn address(uri: &str) -> ProviderPath {
        ProviderPath::parse(uri).unwrap().unwrap()
    }

    #[test]
    fn provider_paths_roundtrip_and_keep_components_inside_the_scheme() {
        let path = address("EXAMPLE://server:2222/work%20files/%E2%98%83%23.log");
        assert_eq!(path.name(), Some("☃#.log"));
        assert_eq!(
            path.to_string(),
            "example://server:2222/work%20files/%E2%98%83%23.log"
        );
        assert_eq!(
            path.resolve("../next").unwrap().to_string(),
            "example://server:2222/work%20files/next"
        );
        assert!(address("example://").parent().is_none());
        assert!(
            ProviderPath::parse("/local/example://files")
                .unwrap()
                .is_none()
        );
        for uri in [
            "example://dir/%2E%2E",
            "example://dir/%2Ftmp",
            "example://dir//file",
        ] {
            assert!(ProviderPath::parse(uri).is_err(), "{uri}");
        }
        let location = Location::Provider(path);
        let json = serde_json::to_string(&SavedLocation::from(&location)).unwrap();
        let restored = serde_json::from_str::<SavedLocation>(&json)
            .unwrap()
            .restore(&crate::archive_extensions::BUNDLED)
            .unwrap();
        assert_eq!(restored, location);
        assert!(restored.local().is_none());
        assert!(restored.host_path().is_none());
    }

    #[test]
    fn extension_logs_are_organized_as_read_only_files_in_a_panel() {
        let logs = ExtensionLogs::default();
        logs.register("disabled.extension");
        logs.source("typescriptteam.native-preview", "TypeScript 7 #2")
            .append("Language client ready");
        logs.append("another.extension", "other output");
        let filesystems = Filesystems::new(logs.clone());

        let root = Location::Provider(address("ext-logs://"));
        let entries = read_location(&root, &filesystems, false, SortOrder::Name).unwrap();
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            [
                "another.extension",
                "disabled.extension",
                "typescriptteam.native-preview"
            ]
        );
        assert!(entries.iter().all(|e| e.directory && !e.parent));

        let folder = &entries[2].path;
        let files = read_location(folder, &filesystems, false, SortOrder::Name).unwrap();
        assert_eq!(files[0].path, root);
        assert_eq!(
            files[1].path.display(),
            "ext-logs://typescriptteam.native-preview/output.log"
        );
        let content = String::from_utf8(files[1].path.read_preview(&filesystems).unwrap()).unwrap();
        assert!(content.ends_with("[TypeScript 7 #2] Language client ready"));
        assert!(!content.contains("other output"));
        assert_eq!(files[1].size, content.len() as u64);
        let empty = Location::Provider(address("ext-logs://disabled.extension/output.log"));
        assert!(empty.read_preview(&filesystems).unwrap().is_empty());
        assert!(filesystems.check(&address("uninstalled://")).is_err());
    }

    #[test]
    #[ignore = "requires Node.js"]
    fn installed_node_provider_lists_and_reads_files() {
        let package = tempfile::tempdir().unwrap();
        std::fs::write(package.path().join("main.mjs"), r#"
            export async function readDirectory({ segments }) {
                console.log('Listing fixture');
                if (segments[0] === 'error') throw new Error('Fixture request failed');
                if (segments[0] === 'invalid') return [{ name: '../escape' }];
                return segments.length ? [{name: 'data #1.bin', size: 3}]
                    : [{name: 'folder', directory: true}];
            }
            export async function readFile({ uri }, limit) {
                if (uri !== 'fixture://folder/data%20%231.bin' || limit < 3) throw new Error('Bad file request');
                return new Uint8Array([0, 28, 255]);
            }
        "#).unwrap();
        let logs = ExtensionLogs::default();
        let mut filesystems = Filesystems::new(logs.clone());
        let manifest = |schemes: &[&str]| {
            serde_json::from_value(json!({
                "id": "fixture", "api_version": 1, "main": "main.mjs", "filesystems": schemes
            }))
            .unwrap()
        };
        assert!(
            filesystems
                .register(
                    manifest(&["other", "ext-logs"]),
                    package.path(),
                    None,
                    logs.clone()
                )
                .is_err()
        );
        assert!(filesystems.check(&address("other://")).is_err());
        filesystems
            .register(manifest(&["fixture"]), package.path(), None, logs.clone())
            .unwrap();
        let root = address("fixture://");
        let folder = filesystems.read_directory(&root).unwrap().remove(0);
        assert!(folder.directory);
        let folder = root.child(folder.name);
        let file = filesystems.read_directory(&folder).unwrap().remove(0);
        assert_eq!(file.size, 3);
        assert_eq!(
            filesystems
                .read_file(&folder.child(file.name), 3, &CancelToken::new())
                .unwrap(),
            [0, 28, 255]
        );
        assert!(
            filesystems
                .read_directory(&address("fixture://invalid"))
                .is_err()
        );
        let error = filesystems
            .read_directory(&address("fixture://error"))
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "Fixture request failed");
        let output = logs.group_output("fixture").unwrap();
        assert!(output.contains("Listing fixture") && output.contains("Fixture request failed"));
    }
}
