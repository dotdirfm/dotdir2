use crate::{
    archive::Location,
    archive_ops::{self, TransferContext},
    filesystems::Filesystems,
    remote_download,
};
use anyhow::{Context, bail};
use dotdir_archive_api::{ArchiveFormat, CreationOptions};
use dotdir_core::{
    copy::{self, CancelToken, ConflictResolution, CopyEvent, CopyOptions},
    move_op::{self, MoveOptions},
};
use std::{
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

#[derive(Clone, Copy, Debug)]
pub enum OperationKind {
    Copy,
    Download,
    Move,
    Trash,
    Extract,
    DeleteEntries,
    CreateEntry,
    CreateArchive,
}
impl OperationKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Copy => "Copy",
            Self::Download => "Download",
            Self::Move => "Move",
            Self::Trash => "Move to Trash",
            Self::Extract => "Extract Archive",
            Self::DeleteEntries => "Delete Archive Entries",
            Self::CreateEntry => "Create Archive Entry",
            Self::CreateArchive => "Create Archive",
        }
    }
}
pub enum OperationEvent {
    Progress(CopyEvent),
    Conflict {
        source: PathBuf,
        destination: PathBuf,
        reply: mpsc::Sender<ConflictResolution>,
    },
    Settled(Result<(), String>),
}
pub struct Operation {
    pub id: u64,
    pub kind: OperationKind,
    pub token: CancelToken,
    pub events: mpsc::Receiver<OperationEvent>,
    pub progress: String,
    pub fraction: f32,
}
impl Operation {
    pub fn copy(
        id: u64,
        sources: Vec<PathBuf>,
        destinations: Vec<PathBuf>,
        options: CopyOptions,
    ) -> Self {
        Self::spawn(id, OperationKind::Copy, move |cancel, emit, conflict| {
            copy::copy_to_directories(&sources, &destinations, &options, cancel, emit, conflict)?;
            Ok(())
        })
    }

    pub fn start(
        id: u64,
        kind: OperationKind,
        sources: Vec<PathBuf>,
        destination: PathBuf,
        options: CopyOptions,
    ) -> Self {
        Self::spawn(id, kind, move |cancel, emit, conflict| {
            for p in &sources {
                if p.parent().is_none() || p.file_name().is_none() {
                    anyhow::bail!("Filesystem roots cannot be transferred or removed");
                }
            }
            if sources.is_empty() {
                return Ok(());
            }
            match kind {
                OperationKind::Copy => {
                    copy::copy_tree(&sources, &destination, &options, cancel, emit, conflict)?;
                }
                OperationKind::Move => {
                    move_op::move_tree(
                        &sources,
                        &destination,
                        &MoveOptions {
                            conflict_policy: options.conflict_policy,
                        },
                        cancel,
                        emit,
                        conflict,
                    )?;
                }
                OperationKind::Trash => {
                    for (i, path) in sources.iter().enumerate() {
                        if cancel.is_cancelled() {
                            break;
                        }
                        trash::delete(path)?;
                        emit(CopyEvent::Progress(copy::CopyProgress {
                            bytes_copied: i as u64 + 1,
                            bytes_total: sources.len() as u64,
                            files_done: i as u32 + 1,
                            files_total: sources.len() as u32,
                            current_file: path.to_string_lossy().into_owned(),
                        }));
                    }
                }
                _ => bail!("This operation needs an archive"),
            }
            Ok(())
        })
    }

    pub fn transfer(
        id: u64,
        kind: OperationKind,
        sources: Vec<Location>,
        destination: Location,
        options: CopyOptions,
        filesystems: Filesystems,
    ) -> Self {
        if matches!(kind, OperationKind::Download) {
            return Self::spawn(id, kind, move |cancel, emit, conflict| {
                let destination = destination
                    .local()
                    .context("Choose a local directory for downloads")?;
                remote_download::download(
                    sources,
                    destination,
                    options.conflict_policy,
                    &TransferContext {
                        filesystems: &filesystems,
                        cancel,
                        emit,
                        conflict,
                    },
                )
            });
        }
        if let Location::Local(directory) = &destination
            && sources.iter().all(|source| source.local().is_some())
        {
            let paths = sources
                .into_iter()
                .map(|source| source.local().unwrap().to_path_buf())
                .collect();
            return Self::start(id, kind, paths, directory.clone(), options);
        }
        let moving = matches!(kind, OperationKind::Move);
        let kind = if !moving && destination.local().is_some() {
            OperationKind::Extract
        } else {
            kind
        };
        Self::spawn(id, kind, move |cancel, emit, conflict| {
            let filesystems = filesystems.archive_operation();
            let ctx = TransferContext {
                filesystems: &filesystems,
                cancel,
                emit,
                conflict,
            };
            match destination {
                Location::Archive { .. } => archive_ops::transfer_into(
                    &sources,
                    &destination,
                    moving,
                    options.conflict_policy,
                    &ctx,
                ),
                Location::Local(path) => {
                    let Some(Location::Archive { archive, .. }) = sources.first() else {
                        bail!("Expected an archive source");
                    };
                    let members = sources
                        .iter()
                        .map(|source| {
                            source
                                .archive_member()
                                .map(Path::to_path_buf)
                                .context("Expected an archive entry")
                        })
                        .collect::<anyhow::Result<_>>()?;
                    archive_ops::extract(
                        archive,
                        members,
                        &path,
                        moving,
                        options.conflict_policy,
                        &ctx,
                    )
                }
                Location::Provider(_) => bail!("Destination filesystem is read-only"),
            }
        })
    }

    pub fn delete_entries(id: u64, sources: Vec<Location>, filesystems: Filesystems) -> Self {
        Self::spawn(id, OperationKind::DeleteEntries, move |cancel, _, _| {
            let filesystems = filesystems.archive_operation();
            let Some(Location::Archive { archive, .. }) = sources.first() else {
                bail!("Expected an archive source");
            };
            let members = sources
                .iter()
                .map(|source| {
                    source
                        .archive_member()
                        .map(Path::to_path_buf)
                        .context("Expected an archive entry")
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            archive_ops::delete(archive, &members, &filesystems, cancel)
        })
    }

    pub fn create_entry(
        id: u64,
        directory: Location,
        name: String,
        folder: bool,
        filesystems: Filesystems,
        opened: impl FnOnce(Location) + Send + 'static,
    ) -> Self {
        Self::spawn(id, OperationKind::CreateEntry, move |cancel, _, _| {
            let path = archive_ops::create_entry(
                &directory,
                &name,
                folder,
                &filesystems.archive_operation(),
                cancel,
            )?;
            if !folder && !cancel.is_cancelled() {
                opened(path);
            }
            Ok(())
        })
    }

    pub fn create_archive(
        id: u64,
        sources: Vec<Location>,
        destination: PathBuf,
        format: &'static dyn ArchiveFormat,
        options: CreationOptions,
        filesystems: Filesystems,
    ) -> Self {
        Self::spawn(
            id,
            OperationKind::CreateArchive,
            move |cancel, emit, conflict| {
                let filesystems = filesystems.archive_operation();
                let ctx = TransferContext {
                    filesystems: &filesystems,
                    cancel,
                    emit,
                    conflict,
                };
                archive_ops::create(&sources, &destination, format, &options, &ctx)
            },
        )
    }

    fn spawn(
        id: u64,
        kind: OperationKind,
        work: impl FnOnce(
            &CancelToken,
            &dyn Fn(CopyEvent),
            &dyn Fn(&Path, &Path) -> ConflictResolution,
        ) -> anyhow::Result<()>
        + Send
        + 'static,
    ) -> Self {
        let (tx, events) = mpsc::channel();
        let token = CancelToken::new();
        let cancel = token.clone();
        std::thread::spawn(move || {
            let emit = |event| {
                let _ = tx.send(OperationEvent::Progress(event));
            };
            let conflict = |source: &std::path::Path, destination: &std::path::Path| {
                let (reply, rx) = mpsc::channel();
                let _ = tx.send(OperationEvent::Conflict {
                    source: source.into(),
                    destination: destination.into(),
                    reply,
                });
                loop {
                    if cancel.is_cancelled() {
                        return ConflictResolution::Cancel;
                    }
                    match rx.recv_timeout(Duration::from_millis(50)) {
                        Ok(answer) => return answer,
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            return ConflictResolution::Cancel;
                        }
                        Err(_) => {}
                    }
                }
            };
            let result = work(&cancel, &emit, &conflict);
            let _ = tx.send(OperationEvent::Settled(result.map_err(|e| e.to_string())));
        });
        Self {
            id,
            kind,
            token,
            events,
            progress: "Preparing…".into(),
            fraction: 0.,
        }
    }
}
impl Drop for Operation {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

/// Saves are serialized by the document owner. An external modification is
/// rejected before writing, and the UI retains the unsaved buffer on failure.
pub fn save_document(path: &std::path::Path, expected: &[u8], text: &str) -> anyhow::Result<()> {
    let disk = std::fs::read(path)?;
    if disk != expected {
        anyhow::bail!(
            "File changed on disk. Your buffer is retained; reopen the file to compare before saving."
        );
    }
    std::fs::write(path, text.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn save_rejects_external_changes() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("a");
        std::fs::write(&p, "external").unwrap();
        assert!(save_document(&p, b"old", "mine").is_err());
        assert_eq!(std::fs::read_to_string(p).unwrap(), "external");
    }
    #[test]
    fn transfer_conflict_is_resolved_on_worker_and_settles() {
        let t = tempfile::tempdir().unwrap();
        let source = t.path().join("a");
        let dest = t.path().join("dest");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(&source, "new").unwrap();
        std::fs::write(dest.join("a"), "old").unwrap();
        let op = Operation::start(
            1,
            OperationKind::Copy,
            vec![source],
            dest.clone(),
            CopyOptions::default(),
        );
        loop {
            match op.events.recv_timeout(Duration::from_secs(5)).unwrap() {
                OperationEvent::Conflict { reply, .. } => {
                    reply.send(ConflictResolution::Skip).unwrap()
                }
                OperationEvent::Settled(r) => {
                    r.unwrap();
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(std::fs::read_to_string(dest.join("a")).unwrap(), "old");
    }
    #[test]
    fn cancellation_interrupts_a_pending_conflict() {
        let t = tempfile::tempdir().unwrap();
        let source = t.path().join("a");
        let dest = t.path().join("dest");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(&source, "new").unwrap();
        std::fs::write(dest.join("a"), "old").unwrap();
        let op = Operation::start(
            1,
            OperationKind::Copy,
            vec![source],
            dest.clone(),
            CopyOptions::default(),
        );
        let mut pending_reply = None;
        loop {
            match op.events.recv_timeout(Duration::from_secs(3)).unwrap() {
                OperationEvent::Conflict { reply, .. } => {
                    pending_reply = Some(reply);
                    op.token.cancel();
                }
                OperationEvent::Settled(_) => break,
                _ => {}
            }
        }
        assert!(pending_reply.is_some());
        assert_eq!(std::fs::read_to_string(dest.join("a")).unwrap(), "old");
    }
}
