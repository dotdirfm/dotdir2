use dotdir_core::{
    copy::{self, CancelToken, ConflictResolution, CopyEvent, CopyOptions},
    move_op::{self, MoveOptions},
};
use std::{path::PathBuf, sync::mpsc, time::Duration};

#[derive(Clone, Copy, Debug)]
pub enum OperationKind {
    Copy,
    Move,
    Trash,
}
impl OperationKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Copy => "Copy",
            Self::Move => "Move",
            Self::Trash => "Move to Trash",
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
    pub fn start(
        id: u64,
        kind: OperationKind,
        sources: Vec<PathBuf>,
        destination: PathBuf,
        options: CopyOptions,
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
            let result = (|| -> anyhow::Result<()> {
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
                        copy::copy_tree(
                            &sources,
                            &destination,
                            &options,
                            &cancel,
                            &emit,
                            &conflict,
                        )?;
                    }
                    OperationKind::Move => {
                        move_op::move_tree(
                            &sources,
                            &destination,
                            &MoveOptions {
                                conflict_policy: options.conflict_policy,
                            },
                            &cancel,
                            &emit,
                            &conflict,
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
                }
                Ok(())
            })();
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
