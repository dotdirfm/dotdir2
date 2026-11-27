use crate::{archive::Location, filesystems::Filesystems, lsp::Launch, vscode};
use anyhow::{Context, Result, bail};
use gpui_kit::component::input::Rope;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

const HOST: &str = include_str!("archive_typescript.mjs");
const EXTENSION_ID: &str = "typescriptteam.native-preview";

pub(crate) type OpenFiles = Arc<RwLock<HashMap<Location, Rope>>>;

pub(crate) fn extension<'a>(
    path: &Location,
    extensions: &'a [vscode::Extension],
    settings: &vscode::Settings,
) -> Option<(&'a vscode::Extension, &'a str)> {
    path.archive_member()?;
    let extension = extensions
        .iter()
        .find(|extension| extension.id == EXTENSION_ID && settings.enabled(&extension.id))?;
    Some((extension, extension.language_for(path.name_path())?))
}

pub(crate) fn launch(extension: &vscode::Extension, settings: &vscode::Settings) -> Launch {
    let configured_node = settings.node.clone();
    Launch {
        name: "TypeScript 7 (Archive)".into(),
        script: HOST,
        command: Box::new(move || {
            let node = vscode::node_path(configured_node.as_deref())
                .context("Node.js 22.15 or newer is required for TypeScript in archives")?;
            let mut command = vscode::node_command(&node);
            command.env("NODE_DISABLE_COMPILE_CACHE", "1");
            Ok(command)
        }),
        options: json!({"extensionRoot": extension.directory()}),
    }
}

pub(crate) struct Workspace {
    pub archive: Location,
    pub root: PathBuf,
    filesystems: Filesystems,
    open_files: OpenFiles,
}

impl Workspace {
    pub fn new(
        archive: Location,
        configuration: &Path,
        id: u64,
        filesystems: Filesystems,
        open_files: OpenFiles,
    ) -> Result<Self> {
        let configuration = std::path::absolute(configuration)?;
        let volume = configuration
            .ancestors()
            .last()
            .context("Missing filesystem root")?;
        // The SDK needs an absolute filename namespace. This directory is never created.
        let root = volume.join(format!(".dotdir-archive-{}-{id}", std::process::id()));
        Ok(Self {
            archive,
            root,
            filesystems,
            open_files,
        })
    }

    pub fn file_path(&self, location: &Location) -> Option<PathBuf> {
        let member = location.archive_member()?;
        (location.archive_root().as_ref() == Some(&self.archive)).then(|| self.root.join(member))
    }

    pub fn location(&self, path: &Path) -> Option<Location> {
        let relative = path.strip_prefix(&self.root).ok()?;
        let relative = relative
            .iter()
            .map(|part| part.to_str())
            .collect::<Option<Vec<_>>>()?
            .join("/");
        self.archive.resolve_archive_member(&relative)
    }

    pub fn serve(&self, params: &Value) -> Result<Value> {
        let filesystems = self
            .filesystems
            .without_password_prompts()
            .archive_operation();
        let path = params["path"].as_str().context("Missing archive path")?;
        let location = self
            .archive
            .resolve_archive_member(path)
            .context("Invalid archive path")?;
        let metadata = if path.is_empty() {
            None
        } else {
            location.archive_metadata(&filesystems)?
        };
        let regular = metadata
            .as_ref()
            .is_some_and(|entry| !entry.directory && !entry.symlink);
        let directory = path.is_empty() || metadata.as_ref().is_some_and(|entry| entry.directory);
        Ok(match params["operation"].as_str().unwrap_or_default() {
            "fileExists" => json!(regular),
            "directoryExists" => json!(directory),
            "realpath" => json!(self.file_path(&location)),
            "getAccessibleEntries" => {
                let mut files = Vec::new();
                let mut directories = Vec::new();
                if directory && let Location::Archive { archive, member } = &location {
                    for (_, entry) in archive.list(member, &filesystems)? {
                        if entry.directory {
                            directories.push(entry.name);
                        } else if !entry.symlink {
                            files.push(entry.name);
                        }
                    }
                }
                json!({"files": files, "directories": directories})
            }
            "readFile" if regular => {
                let open = self.open_files.read().unwrap().get(&location).cloned();
                if let Some(text) = open {
                    json!(text.to_string())
                } else {
                    json!(String::from_utf8(location.read_preview(&filesystems)?).ok())
                }
            }
            "readFile" => Value::Null,
            _ => bail!("Unsupported archive filesystem operation"),
        })
    }
}
