use crate::{
    archive::{
        ArchiveRevision, Location, WriteMode, ensure_directory, extraction_target, valid_component,
    },
    archive_ops::TransferContext,
    filesystems::{ProviderEntry, ProviderPath},
};
use anyhow::{Context, Result, ensure};
use dotdir_core::copy::{ConflictPolicy, CopyEvent, CopyProgress};
use std::{
    collections::HashSet,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

struct Job {
    source: ProviderPath,
    relative: PathBuf,
    entry: ProviderEntry,
}

fn plan(sources: Vec<Location>, ctx: &TransferContext<'_>) -> Result<Vec<Job>> {
    let mut selected = sources
        .into_iter()
        .map(|source| match source {
            Location::Provider(path) if ctx.filesystems.is_remote(&path) => Ok(path),
            _ => anyhow::bail!("Expected a remote source"),
        })
        .collect::<Result<HashSet<_>>>()?;
    let Some(first) = selected.iter().next() else {
        return Ok(Vec::new());
    };
    let parent = first
        .parent()
        .context("Select files or folders within a connection")?;
    let mut pending = Vec::new();
    for entry in ctx
        .filesystems
        .read_directory_cancellable(&parent, ctx.cancel)?
    {
        let source = parent.child(entry.name.clone());
        if selected.remove(&source) {
            pending.push(Job {
                source,
                relative: PathBuf::from(&entry.name),
                entry,
            });
        }
    }
    ensure!(
        selected.is_empty(),
        "Some selected entries are no longer in the remote directory"
    );

    let mut jobs = Vec::new();
    while let Some(job) = pending.pop() {
        if ctx.cancel.is_cancelled() {
            return Ok(Vec::new());
        }
        let name = &job.entry.name;
        ensure!(
            valid_component(name) && !name.contains('/'),
            "Cannot download filename: {name}"
        );
        ensure!(
            job.relative.components().count() <= 256,
            "Remote directory nesting exceeds the download limit"
        );
        if job.entry.directory {
            let entries = ctx
                .filesystems
                .read_directory_cancellable(&job.source, ctx.cancel)?;
            pending.extend(entries.into_iter().rev().map(|entry| Job {
                source: job.source.child(entry.name.clone()),
                relative: job.relative.join(&entry.name),
                entry,
            }));
        }
        jobs.push(job);
    }
    Ok(jobs)
}

pub(crate) fn download(
    sources: Vec<Location>,
    destination: &Path,
    mut policy: ConflictPolicy,
    ctx: &TransferContext<'_>,
) -> Result<()> {
    ensure!(
        !matches!(policy, ConflictPolicy::Append | ConflictPolicy::Resume),
        "Unsupported download conflict policy"
    );
    let base = destination.canonicalize()?;
    ensure!(base.is_dir(), "Choose a local directory for downloads");
    let jobs = match plan(sources, ctx) {
        Err(_) if ctx.cancel.is_cancelled() => return Ok(()),
        result => result?,
    };
    let mut progress = CopyProgress {
        bytes_copied: 0,
        bytes_total: jobs
            .iter()
            .filter(|job| !job.entry.directory)
            .fold(0u64, |total, job| total.saturating_add(job.entry.size)),
        files_done: 0,
        files_total: jobs.len().try_into().unwrap_or(u32::MAX),
        current_file: String::new(),
    };
    let mut last = Instant::now();
    let mut buffer = vec![0; 128 * 1024];
    for job in jobs {
        if ctx.cancel.is_cancelled() {
            break;
        }
        progress.current_file = job.source.to_string();
        if job.entry.directory {
            ensure_directory(&base, &job.relative)?;
        } else {
            let parent = job.relative.parent().context("Invalid destination path")?;
            ensure_directory(&base, parent)?;
            let source = PathBuf::from(job.source.to_string());
            if let Some((target, mode)) = extraction_target(
                &source,
                base.join(&job.relative),
                job.entry.modified_ms as f64,
                &mut policy,
                ctx.cancel,
                ctx.conflict,
            )? {
                let previous = match mode {
                    WriteMode::New => None,
                    WriteMode::Replace => {
                        let metadata = target.symlink_metadata()?;
                        ensure!(metadata.is_file(), "Download destination changed");
                        Some(ArchiveRevision::from(metadata))
                    }
                    WriteMode::Append => unreachable!(),
                };
                let opened = ctx.filesystems.open_remote(&job.source, ctx.cancel);
                if ctx.cancel.is_cancelled() {
                    return Ok(());
                }
                let mut remote = opened?;
                progress.bytes_total = progress
                    .bytes_total
                    .saturating_sub(job.entry.size)
                    .saturating_add(remote.size);
                let mut output = tempfile::NamedTempFile::new_in(base.join(parent))?;
                let mut remaining = remote.size;
                while remaining > 0 {
                    let limit = remaining.min(buffer.len() as u64) as usize;
                    let read = remote.reader.read(&mut buffer[..limit]);
                    if ctx.cancel.is_cancelled() {
                        return Ok(());
                    }
                    let read = read?;
                    ensure!(read != 0, "Remote file ended before the download completed");
                    output.write_all(&buffer[..read])?;
                    remaining -= read as u64;
                    progress.bytes_copied = progress.bytes_copied.saturating_add(read as u64);
                    if last.elapsed() >= Duration::from_millis(50) {
                        (ctx.emit)(CopyEvent::Progress(progress.clone()));
                        last = Instant::now();
                    }
                }
                if ctx.cancel.is_cancelled() {
                    return Ok(());
                }
                ensure_directory(&base, parent)?;
                if let Some(revision) = previous {
                    ensure!(
                        revision == ArchiveRevision::from(target.symlink_metadata()?),
                        "Download destination changed"
                    );
                    output.persist(&target)?;
                } else {
                    output.persist_noclobber(&target)?;
                }
            } else {
                progress.bytes_total = progress.bytes_total.saturating_sub(job.entry.size);
            }
        }
        progress.files_done = progress.files_done.saturating_add(1);
        (ctx.emit)(CopyEvent::Progress(progress.clone()));
    }
    Ok(())
}

#[cfg(all(test, feature = "webdav-extension"))]
mod tests {
    use super::*;
    use crate::{extension_logs::ExtensionLogs, filesystems::Filesystems};
    use dotdir_core::copy::{CancelToken, ConflictResolution};
    use std::io::{Seek, SeekFrom};

    #[test]
    #[ignore = "requires the anonymous test WebDAV server in DOTDIR_TEST_WEBDAV"]
    fn previews_and_downloads_a_remote_folder() {
        let scratch = tempfile::tempdir().unwrap();
        let profile: dotdir_remote::Profile = serde_json::from_value(serde_json::json!({
            "protocol": "webdav",
            "name": "fixture",
            "endpoint": std::env::var("DOTDIR_TEST_WEBDAV").unwrap(),
        }))
        .unwrap();
        let mut filesystems = Filesystems::new(ExtensionLogs::default());
        filesystems.set_remote_profiles(&[profile], &scratch.path().join("settings.json"));
        let folder = ProviderPath::root("webdav")
            .child("fixture".into())
            .child("folder".into());
        let file = Location::Provider(folder.child("sample.bin".into()));
        let cancel = CancelToken::new();
        let ctx = TransferContext {
            filesystems: &filesystems,
            cancel: &cancel,
            emit: &|_| {},
            conflict: &|_, _| ConflictResolution::Overwrite,
        };
        let mut preview = file.open_seekable(&filesystems, &cancel).unwrap();
        preview.seek(SeekFrom::End(-17)).unwrap();
        let mut suffix = [0; 17];
        preview.read_exact(&mut suffix).unwrap();

        let expected: Vec<_> = (0..2 * 1024 * 1024 + 17).map(|i| (i % 251) as u8).collect();
        assert_eq!(suffix, expected[expected.len() - 17..]);
        let download_folder = || vec![Location::Provider(folder.clone())];
        download(download_folder(), scratch.path(), ConflictPolicy::Ask, &ctx).unwrap();
        let output = scratch.path().join("folder/sample.bin");
        assert_eq!(std::fs::read(&output).unwrap(), expected);

        std::fs::write(&output, "existing file").unwrap();
        download(
            download_folder(),
            scratch.path(),
            ConflictPolicy::Skip,
            &ctx,
        )
        .unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b"existing file");
        cancel.cancel();
        download(
            download_folder(),
            scratch.path(),
            ConflictPolicy::Overwrite,
            &ctx,
        )
        .unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b"existing file");
    }
}
