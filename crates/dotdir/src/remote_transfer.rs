use crate::{
    archive::{
        ArchiveRevision, Location, SourceStamp, WriteMode, ensure_directory, extraction_target,
        valid_component,
    },
    archive_ops::TransferContext,
    filesystems::ProviderPath,
};
use anyhow::{Context, Result, bail, ensure};
use dotdir_core::copy::{ConflictPolicy, ConflictResolution, CopyEvent, CopyProgress};
use dotdir_remote::{NativeTransfer, ObjectVersion, ReadSeek, Upload};
use std::{
    collections::HashSet,
    fs::{self, File},
    io::{Read, Write},
    path::Path,
    time::{Duration, Instant, UNIX_EPOCH},
};

enum Version {
    Local(Option<SourceStamp>),
    Remote(ObjectVersion),
}

struct Job {
    source: Location,
    relative: Vec<String>,
    version: Version,
    size: u64,
    modified_ms: u64,
    directory: bool,
}

fn plan(sources: Vec<Location>, ctx: &TransferContext<'_>) -> Result<Vec<Job>> {
    let mut pending = Vec::new();
    let mut seen = HashSet::new();
    for source in sources.into_iter().rev() {
        if seen.insert(source.clone()) {
            let name = source
                .file_name()
                .and_then(|name| name.to_str())
                .context("Remote operations require a named entry with a UTF-8 filename")?
                .to_owned();
            pending.push((source, vec![name], None));
        }
    }
    let mut jobs = Vec::new();
    while let Some((source, relative, directory_hint)) = pending.pop() {
        if ctx.cancel.is_cancelled() {
            return Ok(Vec::new());
        }
        ensure!(
            relative.len() <= 256,
            "Remote directory nesting exceeds the transfer limit"
        );
        let (version, size, modified_ms, directory) = match &source {
            Location::Local(path) => {
                let metadata = path.symlink_metadata()?;
                let directory = metadata.is_dir();
                ensure!(
                    directory || metadata.is_file(),
                    "Remote transfers require regular files: {}",
                    path.display()
                );
                if directory {
                    for child in path.read_dir()? {
                        let child = child?;
                        let name = child.file_name().into_string().map_err(|_| {
                            anyhow::anyhow!("Remote transfers require UTF-8 filenames")
                        })?;
                        let mut relative = relative.clone();
                        relative.push(name);
                        pending.push((child.path().into(), relative, None));
                    }
                }
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .and_then(|time| time.as_millis().try_into().ok())
                    .unwrap_or(0);
                let stamp = if directory {
                    None
                } else {
                    Some(SourceStamp::from_path(path)?)
                };
                (Version::Local(stamp), metadata.len(), modified, directory)
            }
            Location::Provider(path) => {
                ensure!(
                    ctx.filesystems.can_change_remote(path),
                    "Select an entry inside a remote connection"
                );
                let directory = if let Some(directory) = directory_hint {
                    directory
                } else {
                    let parent = path
                        .parent()
                        .context("Expected an entry inside a connection")?;
                    ctx.filesystems
                        .read_directory_cancellable(&parent, ctx.cancel)?
                        .into_iter()
                        .find(|entry| Some(entry.name.as_str()) == path.name())
                        .context("The selected remote entry is no longer available")?
                        .directory
                };
                let version = if directory {
                    let entries = ctx
                        .filesystems
                        .read_directory_cancellable(path, ctx.cancel)?;
                    for entry in entries.into_iter().rev() {
                        let mut relative = relative.clone();
                        relative.push(entry.name.clone());
                        pending.push((
                            Location::Provider(path.child(entry.name)),
                            relative,
                            Some(entry.directory),
                        ));
                    }
                    ObjectVersion {
                        etag: String::new(),
                        size: 0,
                        modified_ms: 0,
                        directory: true,
                    }
                } else {
                    ctx.filesystems
                        .upload_version(path, ctx.cancel)?
                        .context("Remote source disappeared")?
                };
                (
                    Version::Remote(version.clone()),
                    version.size,
                    version.modified_ms,
                    directory,
                )
            }
            Location::Archive { .. } => bail!("Choose local or remote entries for this transfer"),
        };
        jobs.push(Job {
            source,
            relative,
            version,
            size,
            modified_ms,
            directory,
        });
    }
    Ok(jobs)
}

fn child_path(base: &Location, relative: &[String]) -> Result<Location> {
    match base {
        Location::Provider(base) => {
            let mut path = base.clone();
            for name in relative {
                path = path.checked_child(name)?;
            }
            Ok(Location::Provider(path))
        }
        Location::Local(base) => {
            let mut path = base.clone();
            for name in relative {
                ensure!(
                    valid_component(name) && !name.contains('/'),
                    "Invalid local filename: {name:?}"
                );
                path.push(name);
            }
            Ok(path.into())
        }
        _ => bail!("Choose a local or remote destination"),
    }
}

fn remote_target(
    job: &Job,
    target: &ProviderPath,
    policy: &mut ConflictPolicy,
    ctx: &TransferContext<'_>,
) -> Result<Option<(ProviderPath, Option<ObjectVersion>)>> {
    let stat = |path: &ProviderPath| ctx.filesystems.upload_version(path, ctx.cancel);
    let Some(previous) = stat(target)? else {
        return Ok(Some((target.clone(), None)));
    };
    ensure!(!previous.directory, "Destination is a directory: {target}");
    let resolution = match policy {
        ConflictPolicy::Ask => (ctx.conflict)(
            Path::new(&job.source.display()),
            Path::new(&target.to_string()),
        ),
        ConflictPolicy::Overwrite => ConflictResolution::Overwrite,
        ConflictPolicy::Skip => ConflictResolution::Skip,
        ConflictPolicy::Rename => {
            let parent = target.parent().context("Expected a remote file path")?;
            let name = Path::new(target.name().unwrap_or_default());
            let stem = name
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            let extension = name
                .extension()
                .and_then(|name| name.to_str())
                .map_or_else(String::new, |ext| format!(".{ext}"));
            for index in 1.. {
                let candidate = parent.checked_child(&format!("{stem} ({index}){extension}"))?;
                if stat(&candidate)?.is_none() {
                    return Ok(Some((candidate, None)));
                }
            }
            unreachable!()
        }
        ConflictPolicy::OnlyNewer if job.modified_ms > previous.modified_ms => {
            ConflictResolution::Overwrite
        }
        ConflictPolicy::OnlyNewer => ConflictResolution::Skip,
        ConflictPolicy::Append | ConflictPolicy::Resume => {
            bail!("Append and Resume are unavailable for remote transfers")
        }
    };
    Ok(match resolution {
        ConflictResolution::Overwrite => Some((target.clone(), Some(previous))),
        ConflictResolution::OverwriteAll => {
            *policy = ConflictPolicy::Overwrite;
            Some((target.clone(), Some(previous)))
        }
        ConflictResolution::Skip => None,
        ConflictResolution::SkipAll => {
            *policy = ConflictPolicy::Skip;
            None
        }
        ConflictResolution::Cancel => {
            ctx.cancel.cancel();
            None
        }
        ConflictResolution::Rename(name) => Some((
            target
                .parent()
                .context("Expected a remote file path")?
                .checked_child(&name)?,
            None,
        )),
    })
}

fn open_source(job: &Job, ctx: &TransferContext<'_>) -> Result<Box<dyn ReadSeek>> {
    match (&job.source, &job.version) {
        (Location::Local(path), Version::Local(Some(expected))) => {
            let file = File::open(path)?;
            ensure!(
                SourceStamp::from_file(&file)? == *expected,
                "Source changed: {}",
                path.display()
            );
            Ok(Box::new(file))
        }
        (Location::Provider(path), Version::Remote(expected)) => {
            ensure!(
                ctx.filesystems.upload_version(path, ctx.cancel)?.as_ref() == Some(expected),
                "Remote source changed: {path}"
            );
            let file = ctx
                .filesystems
                .open_remote_snapshot(path, expected, ctx.cancel)?;
            ensure!(
                file.size == expected.size,
                "Remote source size changed: {path}"
            );
            Ok(file.reader)
        }
        _ => bail!("Expected a file to transfer"),
    }
}

fn remove(job: &Job, ctx: &TransferContext<'_>) -> Result<()> {
    match (&job.source, &job.version) {
        (Location::Local(path), Version::Local(stamp)) => {
            if let Some(stamp) = stamp {
                stamp.check(path)?;
                fs::remove_file(path)?;
            } else {
                fs::remove_dir(path)?;
            }
            Ok(())
        }
        (Location::Provider(path), Version::Remote(stamp)) => {
            ctx.filesystems.remove_remote(path, stamp, ctx.cancel)
        }
        _ => bail!("Expected an entry to remove"),
    }
}

fn write_local(
    job: &Job,
    path: &Path,
    base: &Path,
    policy: &mut ConflictPolicy,
    ctx: &TransferContext<'_>,
    progress: &mut dyn FnMut(u64),
) -> Result<bool> {
    let parent = path.parent().context("Invalid destination path")?;
    let relative = parent.strip_prefix(base)?;
    ensure_directory(base, relative)?;
    let Some((target, mode)) = extraction_target(
        Path::new(&job.source.display()),
        path.to_path_buf(),
        job.modified_ms as f64,
        policy,
        ctx.cancel,
        ctx.conflict,
    )?
    else {
        return Ok(false);
    };
    let previous = match mode {
        WriteMode::New => None,
        WriteMode::Replace => {
            let metadata = target.symlink_metadata()?;
            ensure!(metadata.is_file(), "Destination changed");
            Some(ArchiveRevision::from(metadata))
        }
        WriteMode::Append => unreachable!("remote copies disallow append"),
    };
    let mut input = open_source(job, ctx)?;
    let mut output = tempfile::NamedTempFile::new_in(parent)?;
    let mut remaining = job.size;
    let mut buffer = vec![0; 256 * 1024];
    while remaining > 0 {
        ensure!(!ctx.cancel.is_cancelled(), "Remote operation cancelled");
        let count = remaining.min(buffer.len() as u64) as usize;
        input.read_exact(&mut buffer[..count])?;
        output.write_all(&buffer[..count])?;
        remaining -= count as u64;
        progress(job.size - remaining);
    }
    ensure!(!ctx.cancel.is_cancelled(), "Remote operation cancelled");
    ensure_directory(base, relative)?;
    if let Some(previous) = previous {
        ensure!(
            ArchiveRevision::from(target.symlink_metadata()?) == previous,
            "Destination changed"
        );
        output.persist(&target)?;
    } else {
        output.persist_noclobber(&target)?;
    }
    Ok(true)
}

enum FileTransfer {
    Skipped,
    Copied,
    Moved,
}

fn transfer_file(
    job: &Job,
    path: &Location,
    base: &Location,
    moving: bool,
    policy: &mut ConflictPolicy,
    ctx: &TransferContext<'_>,
    progress: &mut dyn FnMut(u64),
) -> Result<FileTransfer> {
    match path {
        Location::Provider(path) => {
            let Some((target, previous)) = remote_target(job, path, policy, ctx)? else {
                return Ok(FileTransfer::Skipped);
            };
            if let (Location::Provider(source), Version::Remote(version)) =
                (&job.source, &job.version)
            {
                let native = |moving| NativeTransfer {
                    source: version,
                    previous: previous.as_ref(),
                    moving,
                };
                if moving
                    && ctx
                        .filesystems
                        .native_transfer(source, &target, native(true), ctx.cancel)?
                {
                    progress(job.size);
                    return Ok(FileTransfer::Moved);
                }
                if ctx
                    .filesystems
                    .native_transfer(source, &target, native(false), ctx.cancel)?
                {
                    progress(job.size);
                    return Ok(FileTransfer::Copied);
                }
            }
            let mut reader = open_source(job, ctx)?;
            ctx.filesystems.upload(
                &target,
                Upload {
                    reader: &mut *reader,
                    size: job.size,
                    previous: previous.as_ref(),
                    progress,
                },
                ctx.cancel,
            )?;
        }
        Location::Local(path) => {
            if !write_local(
                job,
                path,
                base.local().context("Expected a local destination")?,
                policy,
                ctx,
                progress,
            )? {
                return Ok(FileTransfer::Skipped);
            }
        }
        _ => bail!("Choose a local or remote destination"),
    }
    Ok(FileTransfer::Copied)
}

pub(crate) fn transfer(
    sources: Vec<Location>,
    destination: &Location,
    moving: bool,
    mut policy: ConflictPolicy,
    ctx: &TransferContext<'_>,
) -> Result<()> {
    ensure!(
        !matches!(policy, ConflictPolicy::Append | ConflictPolicy::Resume),
        "Append and Resume are unavailable for remote transfers"
    );
    let destination = match destination {
        Location::Local(path) => {
            let path = path.canonicalize()?;
            ensure!(path.is_dir(), "Choose a destination directory");
            path.into()
        }
        Location::Provider(path) => {
            ensure!(
                ctx.filesystems.can_upload_to(path),
                "Choose a remote connection or folder"
            );
            destination.clone()
        }
        _ => bail!("Choose a local or remote destination"),
    };
    let jobs = match plan(sources, ctx) {
        Err(_) if ctx.cancel.is_cancelled() => return Ok(()),
        result => result?,
    };
    if jobs.is_empty() {
        return Ok(());
    }
    let targets = jobs
        .iter()
        .map(|job| {
            let target = child_path(&destination, &job.relative)?;
            if let (Location::Provider(source), Location::Provider(target)) = (&job.source, &target)
            {
                ensure!(
                    !ctx.filesystems.remote_within(target, source),
                    "Source and destination overlap: {source}"
                );
            }
            Ok(target)
        })
        .collect::<Result<Vec<_>>>()?;

    if let Location::Provider(path) = &destination {
        let mut current = path.clone();
        let mut parents = Vec::new();
        while ctx.filesystems.can_change_remote(&current) {
            parents.push(current.clone());
            current = current.parent().unwrap();
        }
        for parent in parents.into_iter().rev() {
            ctx.filesystems.create_remote_dir(&parent, ctx.cancel)?;
        }
    }
    let mut progress = CopyProgress {
        bytes_copied: 0,
        bytes_total: jobs
            .iter()
            .filter(|job| !job.directory)
            .fold(0u64, |total, job| total.saturating_add(job.size)),
        files_done: 0,
        files_total: jobs.len().try_into().unwrap_or(u32::MAX),
        current_file: String::new(),
    };
    let mut last = Instant::now();
    let mut directories = Vec::new();
    let mut moved_collection: Option<Vec<String>> = None;
    for (job, target) in jobs.into_iter().zip(targets) {
        if ctx.cancel.is_cancelled() {
            return Ok(());
        }
        progress.current_file = target.display();
        if moved_collection
            .as_ref()
            .is_some_and(|root| job.relative.starts_with(root))
        {
            if !job.directory {
                progress.bytes_copied = progress.bytes_copied.saturating_add(job.size);
            }
            progress.files_done = progress.files_done.saturating_add(1);
            continue;
        }
        moved_collection = None;
        if job.directory {
            if moving
                && let (Location::Provider(from), Location::Provider(to), Version::Remote(version)) =
                    (&job.source, &target, &job.version)
                && ctx.filesystems.is_webdav(from)
                && ctx.filesystems.same_remote_connection(from, to)
                && ctx.filesystems.upload_version(to, ctx.cancel)?.is_none()
                && ctx.filesystems.native_transfer(
                    from,
                    to,
                    NativeTransfer {
                        source: version,
                        previous: None,
                        moving: true,
                    },
                    ctx.cancel,
                )?
            {
                moved_collection = Some(job.relative);
                progress.files_done = progress.files_done.saturating_add(1);
                (ctx.emit)(CopyEvent::Progress(progress.clone()));
                continue;
            }
            match &target {
                Location::Local(path) => {
                    let base = destination.local().unwrap();
                    ensure_directory(base, path.strip_prefix(base)?)?;
                }
                Location::Provider(path) => ctx.filesystems.create_remote_dir(path, ctx.cancel)?,
                _ => unreachable!(),
            }
            if moving {
                directories.push(job);
            }
        } else {
            let before = progress.bytes_copied;
            let mut update = |count: u64| {
                progress.bytes_copied = before.saturating_add(count);
                if last.elapsed() >= Duration::from_millis(50) {
                    (ctx.emit)(CopyEvent::Progress(progress.clone()));
                    last = Instant::now();
                }
            };
            let copied = transfer_file(
                &job,
                &target,
                &destination,
                moving,
                &mut policy,
                ctx,
                &mut update,
            )
            .with_context(|| format!("Transferring {}", job.source.display()));
            if ctx.cancel.is_cancelled() {
                return Ok(());
            }
            match copied? {
                FileTransfer::Copied if moving => {
                    remove(&job, ctx).with_context(|| {
                        format!(
                            "Copied {}, but the original could not be removed",
                            job.source.display()
                        )
                    })?;
                }
                FileTransfer::Skipped => {
                    progress.bytes_total = progress.bytes_total.saturating_sub(job.size)
                }
                _ => {}
            }
        }
        progress.files_done = progress.files_done.saturating_add(1);
        (ctx.emit)(CopyEvent::Progress(progress.clone()));
    }
    for job in directories.into_iter().rev() {
        if ctx.cancel.is_cancelled() {
            break;
        }
        let empty = match &job.source {
            Location::Local(path) => path.read_dir()?.next().is_none(),
            Location::Provider(path) if !ctx.filesystems.is_webdav(path) => ctx
                .filesystems
                .read_directory_cancellable(path, ctx.cancel)?
                .is_empty(),
            _ => false,
        };
        if empty {
            remove(&job, ctx)?;
        }
    }
    Ok(())
}

pub(crate) fn delete(sources: Vec<Location>, ctx: &TransferContext<'_>) -> Result<()> {
    let jobs = match plan(sources, ctx) {
        Err(_) if ctx.cancel.is_cancelled() => return Ok(()),
        result => result?,
    };
    let count = jobs.len().try_into().unwrap_or(u32::MAX);
    for (index, job) in jobs.into_iter().rev().enumerate() {
        if ctx.cancel.is_cancelled() {
            return Ok(());
        }
        ensure!(
            matches!(job.source, Location::Provider(_)),
            "Choose remote entries to delete"
        );
        remove(&job, ctx)?;
        (ctx.emit)(CopyEvent::Progress(CopyProgress {
            bytes_copied: 0,
            bytes_total: 0,
            files_done: (index + 1).try_into().unwrap_or(u32::MAX),
            files_total: count,
            current_file: job.source.display(),
        }));
    }
    Ok(())
}

#[cfg(all(test, feature = "s3-extension"))]
mod tests {
    use super::*;
    use crate::{
        extension_logs::ExtensionLogs,
        filesystems::Filesystems,
        operations::{Operation, OperationEvent, OperationKind},
    };
    use dotdir_core::copy::{CancelToken, CopyOptions};

    #[test]
    #[ignore = "requires local writable servers configured in DOTDIR_TEST_LOCAL_TRANSFERS"]
    fn transfers_and_deletes_between_local_protocol_servers() {
        let config = std::env::var_os("DOTDIR_TEST_LOCAL_TRANSFERS").unwrap();
        let profiles: Vec<dotdir_remote::Profile> =
            serde_json::from_slice(&fs::read(config).unwrap()).unwrap();
        for profile in &profiles {
            assert!(
                profile
                    .endpoint
                    .split_once("://")
                    .unwrap()
                    .1
                    .starts_with("127.0.0.1:")
            );
        }
        let scratch = tempfile::tempdir().unwrap();
        let mut filesystems = Filesystems::new(ExtensionLogs::default());
        filesystems.set_remote_profiles(&profiles, &scratch.path().join("settings.json"));
        filesystems
            .use_test_remote_credentials(&profiles, "local-test-only")
            .unwrap();
        let local = scratch.path().join("workspace");
        fs::create_dir(&local).unwrap();
        let bytes = b"DotDir streaming transfer fixture";
        fs::write(local.join("example.txt"), bytes).unwrap();
        fs::write(local.join("empty.txt"), []).unwrap();
        let wait = |operation: Operation| loop {
            match operation
                .events
                .recv_timeout(Duration::from_secs(40))
                .unwrap()
            {
                OperationEvent::Settled(result) => break result,
                OperationEvent::Conflict { .. } => panic!("Unexpected conflict prompt"),
                _ => {}
            }
        };
        let transfer = |kind, from: Location, to: Location, policy| {
            wait(Operation::transfer(
                1,
                kind,
                vec![from],
                to,
                CopyOptions {
                    conflict_policy: policy,
                    ..Default::default()
                },
                filesystems.clone(),
            ))
        };
        let cancel = CancelToken::new();
        let root = |profile: &dotdir_remote::Profile| {
            ProviderPath::root(profile.protocol.scheme()).child(profile.name.clone())
        };
        let read = |file: &ProviderPath| filesystems.read_file(file, bytes.len(), &cancel).unwrap();

        for profile in &profiles {
            let root = root(profile);
            let base = root.child(format!(
                "transfer-{}-{}",
                profile.name,
                scratch.path().file_name().unwrap().to_string_lossy()
            ));
            transfer(
                OperationKind::Copy,
                local.clone().into(),
                Location::Provider(base.clone()),
                ConflictPolicy::Ask,
            )
            .unwrap_or_else(|e| panic!("{} upload: {e:#}", profile.name));
            let source = base.child("workspace".into()).child("example.txt".into());
            let first = base.child("first".into());
            let second = base.child("second".into());
            transfer(
                OperationKind::Copy,
                Location::Provider(source.clone()),
                Location::Provider(first.clone()),
                ConflictPolicy::Ask,
            )
            .unwrap_or_else(|e| panic!("{} copy: {e:#}", profile.name));
            let copied = first.child("example.txt".into());
            assert_eq!(read(&copied), bytes);
            transfer(
                OperationKind::Move,
                Location::Provider(source.clone()),
                Location::Provider(first.clone()),
                ConflictPolicy::Skip,
            )
            .unwrap();
            assert_eq!(read(&source), bytes);
            assert!(
                transfer(
                    OperationKind::Move,
                    Location::Provider(copied.clone()),
                    Location::Provider(first),
                    ConflictPolicy::Overwrite
                )
                .is_err()
            );
            transfer(
                OperationKind::Move,
                Location::Provider(copied.clone()),
                Location::Provider(second.clone()),
                ConflictPolicy::Ask,
            )
            .unwrap_or_else(|e| panic!("{} move: {e:#}", profile.name));
            assert!(
                filesystems
                    .upload_version(&copied, &cancel)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(read(&second.child("example.txt".into())), bytes);
            wait(Operation::delete_remote(
                1,
                vec![Location::Provider(second.clone())],
                filesystems.clone(),
            ))
            .unwrap_or_else(|e| panic!("{} delete: {e:#}", profile.name));
            assert!(
                filesystems
                    .upload_version(&second, &cancel)
                    .unwrap()
                    .is_none()
            );
            if profile.protocol == dotdir_remote::Protocol::WebDav {
                let folder = base.child("workspace".into());
                let destination = base.child("folder-move".into());
                transfer(
                    OperationKind::Move,
                    Location::Provider(folder.clone()),
                    Location::Provider(destination.clone()),
                    ConflictPolicy::Ask,
                )
                .unwrap();
                assert!(
                    filesystems
                        .upload_version(&folder, &cancel)
                        .unwrap()
                        .is_none()
                );
                assert_eq!(
                    read(
                        &destination
                            .child("workspace".into())
                            .child("example.txt".into())
                    ),
                    bytes
                );
            }
            println!("{} upload, copy, move, and delete passed", profile.name);
        }

        let mut pairs: Vec<_> = profiles
            .windows(2)
            .map(|pair| [&pair[0], &pair[1]])
            .collect();
        if let (Some(ftp), Some(s3)) = (
            profiles.iter().find(|p| p.name == "ftp"),
            profiles
                .iter()
                .find(|p| p.protocol == dotdir_remote::Protocol::S3),
        ) {
            pairs.push([ftp, s3]);
        }
        for pair in pairs {
            let first = root(&pair[0]).child(format!(
                "stream-{}",
                scratch.path().file_name().unwrap().to_string_lossy()
            ));
            let second = root(&pair[1]).child(format!(
                "stream-{}",
                scratch.path().file_name().unwrap().to_string_lossy()
            ));
            // Adjacent profiles may target the same fixture on distinct protocols.
            let second = second.child(pair[1].name.clone());
            let file = local.join("example.txt");
            transfer(
                OperationKind::Copy,
                file.into(),
                Location::Provider(first.clone()),
                ConflictPolicy::Ask,
            )
            .unwrap();
            let source = first.child("example.txt".into());
            transfer(
                OperationKind::Move,
                Location::Provider(source.clone()),
                Location::Provider(second.clone()),
                ConflictPolicy::Overwrite,
            )
            .unwrap_or_else(|e| panic!("{} to {}: {e:#}", pair[0].name, pair[1].name));
            assert!(
                filesystems
                    .upload_version(&source, &cancel)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(read(&second.child("example.txt".into())), bytes);
            println!("{} to {} streamed move passed", pair[0].name, pair[1].name);
        }
    }

    #[test]
    #[ignore = "requires the local S3 fixture in DOTDIR_TEST_S3_ENDPOINT"]
    fn copies_a_local_folder_to_s3() {
        let endpoint = std::env::var("DOTDIR_TEST_S3_ENDPOINT").unwrap();
        assert!(endpoint.starts_with("http://127.0.0.1:"));
        let profile: dotdir_remote::Profile = serde_json::from_value(serde_json::json!({
            "protocol": "s3", "name": "upload", "endpoint": endpoint,
            "share": "dotdir-edit-fixture", "region": "us-east-1", "awsProfile": "dotdir-test"
        }))
        .unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let mut filesystems = Filesystems::new(ExtensionLogs::default());
        filesystems.set_remote_profiles(&[profile], &scratch.path().join("settings.json"));
        let source = scratch.path().join("local folder");
        std::fs::create_dir(&source).unwrap();
        let sample = vec![b'x'; 9 * 1024 * 1024 + 17];
        std::fs::write(source.join("large.bin"), &sample).unwrap();
        std::fs::write(source.join("empty.txt"), []).unwrap();
        let target = ProviderPath::root("s3")
            .child("upload".into())
            .child("nested".into());
        let copy = |policy| {
            let operation = Operation::transfer(
                1,
                OperationKind::Copy,
                vec![source.clone().into()],
                Location::Provider(target.clone()),
                CopyOptions {
                    conflict_policy: policy,
                    ..Default::default()
                },
                filesystems.clone(),
            );
            loop {
                match operation
                    .events
                    .recv_timeout(Duration::from_secs(60))
                    .unwrap()
                {
                    OperationEvent::Settled(result) => break result.unwrap(),
                    OperationEvent::Conflict { .. } => panic!("Unexpected upload conflict prompt"),
                    _ => {}
                }
            }
        };
        copy(ConflictPolicy::Ask);
        let folder = target.child("local folder".into());
        let read = |name: &str| {
            filesystems
                .read_file(
                    &folder.child(name.into()),
                    sample.len(),
                    &CancelToken::new(),
                )
                .unwrap()
        };
        assert_eq!(read("large.bin"), sample);
        assert!(read("empty.txt").is_empty());
        std::fs::write(source.join("large.bin"), b"new contents").unwrap();
        copy(ConflictPolicy::Skip);
        assert_eq!(read("large.bin"), sample);
        copy(ConflictPolicy::Overwrite);
        assert_eq!(read("large.bin"), b"new contents");
    }
}

#[cfg(all(test, feature = "webdav-extension"))]
mod download_tests {
    use super::*;
    use crate::{extension_logs::ExtensionLogs, filesystems::Filesystems};
    use dotdir_core::copy::{CancelToken, ConflictResolution};
    use std::io::{Seek, SeekFrom};

    fn download(
        sources: Vec<Location>,
        destination: &Path,
        policy: ConflictPolicy,
        ctx: &TransferContext<'_>,
    ) -> Result<()> {
        transfer(sources, &Location::from(destination), false, policy, ctx)
    }

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
