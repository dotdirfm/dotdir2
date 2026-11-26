use crate::{
    archive::{
        ARCHIVE_WRITE_LOCK, Archive, ArchiveRevision, CancellableOutput, Location, MountedArchive,
        valid_component,
    },
    filesystems::Filesystems,
};
use anyhow::{Context, Result, bail, ensure};
use dotdir_archive_api::{ArchiveFormat, Changes, CreationOptions, Input};
use dotdir_core::copy::{CancelToken, ConflictPolicy, ConflictResolution, CopyEvent, CopyProgress};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

pub(crate) struct TransferContext<'a> {
    pub filesystems: &'a Filesystems,
    pub cancel: &'a CancelToken,
    pub emit: &'a dyn Fn(CopyEvent),
    pub conflict: &'a dyn Fn(&Path, &Path) -> ConflictResolution,
}

struct Source {
    location: Location,
    relative: PathBuf,
    origin: Origin,
    directory: bool,
    size: u64,
    modified: Option<SystemTime>,
    unix_mode: Option<u32>,
}

enum Origin {
    Local(ArchiveRevision),
    Archive(Arc<MountedArchive>),
}

impl Source {
    fn check_local(&self) -> Result<()> {
        if let Origin::Local(revision) = &self.origin {
            let path = self.location.local().unwrap();
            let metadata = path.symlink_metadata()?;
            ensure!(
                !metadata.is_symlink()
                    && path.canonicalize()? == path
                    && *revision == ArchiveRevision::from(metadata),
                "Source changed during transfer: {}",
                path.display()
            );
        }
        Ok(())
    }

    fn write_to(&self, output: &mut dyn Write, progress: &mut Progress<'_>) -> Result<()> {
        match &self.origin {
            Origin::Local(_) => {
                self.check_local()?;
                let mut input = File::open(self.location.local().unwrap())?;
                progress.copy(&mut input, output, self.size)?;
                self.check_local()
            }
            Origin::Archive(snapshot) => {
                let ctx = progress.ctx;
                snapshot.with_member(
                    self.location.archive_member().unwrap(),
                    ctx.filesystems,
                    ctx.cancel,
                    &mut |input| progress.copy(input, output, self.size),
                )
            }
        }
    }
}

struct Progress<'a> {
    state: CopyProgress,
    last: Instant,
    ctx: &'a TransferContext<'a>,
}

impl<'a> Progress<'a> {
    fn new(entries: &[Planned<'_>], ctx: &'a TransferContext<'a>) -> Self {
        Self {
            state: CopyProgress {
                bytes_copied: 0,
                bytes_total: entries
                    .iter()
                    .map(|p| p.size())
                    .fold(0u64, u64::saturating_add),
                files_done: 0,
                files_total: entries
                    .iter()
                    .filter(|p| !p.source.directory)
                    .count()
                    .min(u32::MAX as usize) as u32,
                current_file: String::new(),
            },
            last: Instant::now(),
            ctx,
        }
    }

    fn copy(&mut self, input: &mut dyn Read, output: &mut dyn Write, size: u64) -> Result<()> {
        let mut written = 0u64;
        let mut buffer = [0; 64 * 1024];
        loop {
            ensure!(
                !self.ctx.cancel.is_cancelled(),
                "Archive operation cancelled"
            );
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            written = written
                .checked_add(count as u64)
                .context("File is too large")?;
            ensure!(written <= size, "File exceeds its declared size");
            output.write_all(&buffer[..count])?;
            self.state.bytes_copied = self.state.bytes_copied.saturating_add(count as u64);
            if self.last.elapsed() >= Duration::from_millis(50) {
                self.report();
            }
        }
        ensure!(written == size, "File size does not match its contents");
        Ok(())
    }

    fn report(&mut self) {
        (self.ctx.emit)(CopyEvent::Progress(self.state.clone()));
        self.last = Instant::now();
    }
}

fn collect(selections: &[Location], ctx: &TransferContext<'_>) -> Result<Vec<Source>> {
    let mut result = Vec::new();
    for selection in selections {
        ensure!(!ctx.cancel.is_cancelled(), "Archive operation cancelled");
        let name = selection
            .name_path()
            .file_name()
            .context("Select a file or directory")?;
        match selection {
            Location::Local(path) => {
                let path = path
                    .parent()
                    .context("Select a file or directory")?
                    .canonicalize()?
                    .join(name);
                let mut pending = vec![(path, PathBuf::from(name))];
                while let Some((path, relative)) = pending.pop() {
                    ensure!(!ctx.cancel.is_cancelled(), "Archive operation cancelled");
                    validate_path(&relative)?;
                    let metadata = path.symlink_metadata()?;
                    let directory = metadata.is_dir();
                    ensure!(
                        directory || metadata.is_file(),
                        "Cannot archive a symbolic link or special file: {}",
                        path.display()
                    );
                    if directory {
                        for child in fs::read_dir(&path)? {
                            let child = child?;
                            pending.push((child.path(), relative.join(child.file_name())));
                        }
                    }
                    #[cfg(unix)]
                    let unix_mode = {
                        use std::os::unix::fs::PermissionsExt;
                        Some(metadata.permissions().mode() & 0o777)
                    };
                    #[cfg(not(unix))]
                    let unix_mode = None;
                    result.push(Source {
                        location: path.into(),
                        relative,
                        directory,
                        unix_mode,
                        size: if directory { 0 } else { metadata.len() },
                        modified: metadata.modified().ok(),
                        origin: Origin::Local(metadata.into()),
                    });
                }
            }
            Location::Archive { archive, member } => {
                let snapshot = archive.mounted(ctx.filesystems, ctx.cancel)?;
                let entries = snapshot.index()?;
                ensure!(
                    entries.contains_key(member),
                    "Archive entry is missing: {}",
                    selection.display()
                );
                let parent = member
                    .parent()
                    .context("Select an archive file or directory")?;
                for (path, entry) in entries
                    .range(member.clone()..)
                    .take_while(|(path, _)| path.starts_with(member))
                {
                    ensure!(!ctx.cancel.is_cancelled(), "Archive operation cancelled");
                    ensure!(
                        !entry.symlink,
                        "Cannot archive a symbolic link: {}",
                        path.display()
                    );
                    result.push(Source {
                        location: archive.location(path.clone()),
                        relative: path.strip_prefix(parent)?.into(),
                        origin: Origin::Archive(snapshot.clone()),
                        directory: entry.directory,
                        size: entry.size,
                        modified: (entry.modified > 0.).then(|| {
                            SystemTime::UNIX_EPOCH + Duration::from_millis(entry.modified as u64)
                        }),
                        unix_mode: None,
                    });
                }
            }
            Location::Provider(_) => bail!("This filesystem cannot supply archive entries"),
        }
    }
    result.sort_by(|a, b| a.relative.cmp(&b.relative));
    ensure!(
        !result
            .windows(2)
            .any(|pair| pair[0].relative == pair[1].relative),
        "Selected entries have the same name"
    );
    Ok(result)
}

pub(crate) fn validate_path(path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(name)
                if name.to_str().is_some_and(valid_component))),
        "Invalid archive path: {}",
        path.display()
    );
    Ok(())
}

struct Planned<'a> {
    source: &'a Source,
    target: PathBuf,
    append: Option<(Arc<MountedArchive>, u64)>,
}

impl Planned<'_> {
    fn size(&self) -> u64 {
        self.source
            .size
            .saturating_add(self.append.as_ref().map_or(0, |(_, size)| *size))
    }

    fn input<'a>(&'a self, progress: &'a std::cell::RefCell<Progress<'_>>) -> Input<'a> {
        let contents: Option<Box<dotdir_archive_api::WriteContents<'a>>> = (!self.source.directory)
            .then(|| {
                Box::new(move |output: &mut dyn Write| {
                    let mut progress = progress.borrow_mut();
                    progress.state.current_file = self.target.display().to_string();
                    if let Some((snapshot, size)) = &self.append {
                        let ctx = progress.ctx;
                        snapshot.with_member(
                            &self.target,
                            ctx.filesystems,
                            ctx.cancel,
                            &mut |input| progress.copy(input, output, *size),
                        )?;
                    }
                    self.source.write_to(output, &mut progress)?;
                    progress.state.files_done = progress.state.files_done.saturating_add(1);
                    if progress.state.files_done == progress.state.files_total {
                        progress.report();
                    }
                    Ok(())
                }) as Box<_>
            });
        Input {
            path: self.target.clone(),
            size: self.size(),
            modified: self.source.modified,
            unix_mode: self.source.unix_mode,
            contents,
        }
    }
}

pub(crate) fn create(
    selections: &[Location],
    destination: &Path,
    format: &'static dyn ArchiveFormat,
    options: &CreationOptions,
    ctx: &TransferContext<'_>,
) -> Result<()> {
    let _guard = ARCHIVE_WRITE_LOCK.lock().unwrap();
    ensure!(
        format.creation().is_some(),
        "This format cannot create archives"
    );
    ensure!(!selections.is_empty(), "Select files or folders to archive");
    let entries = collect(selections, ctx)?;
    let parent = destination.parent().context("Enter an archive file path")?;
    let planned: Vec<_> = entries
        .iter()
        .map(|source| Planned {
            source,
            target: source.relative.clone(),
            append: None,
        })
        .collect();
    let progress = std::cell::RefCell::new(Progress::new(&planned, ctx));
    let inputs: Vec<_> = planned.iter().map(|entry| entry.input(&progress)).collect();
    let mut output = tempfile::NamedTempFile::new_in(parent)?;
    format.create(
        &inputs,
        options,
        &mut CancellableOutput {
            output: output.as_file_mut(),
            cancel: ctx.cancel,
        },
    )?;
    output.as_file().sync_all()?;
    ensure!(!ctx.cancel.is_cancelled(), "Archive operation cancelled");
    output.persist_noclobber(destination)?;
    Ok(())
}

pub(crate) fn create_entry(
    directory: &Location,
    name: &str,
    folder: bool,
    filesystems: &Filesystems,
    cancel: &CancelToken,
) -> Result<Location> {
    let Location::Archive { archive, member } = directory else {
        bail!("Expected an archive directory");
    };
    let relative = entry_path(name, folder)?;
    let _guard = ARCHIVE_WRITE_LOCK.lock().unwrap();
    let stamp = archive.stamp()?;
    let index = archive.index(filesystems)?;
    let path = member.join(relative);
    check_directory(&index, path.parent().unwrap())?;
    let target = archive.location(path.clone());
    if let Some(entry) = index.get(&path) {
        ensure!(
            !folder && !entry.directory && !entry.symlink,
            "An entry already exists with this name"
        );
        return Ok(target);
    }
    let input = Input {
        path,
        size: 0,
        modified: Some(SystemTime::now()),
        unix_mode: None,
        contents: (!folder).then(|| Box::new(|_: &mut dyn Write| Ok(())) as Box<_>),
    };
    archive.rewrite(
        Changes {
            remove: &[],
            write: &[input],
        },
        &stamp,
        filesystems,
        cancel,
    )?;
    Ok(target)
}

pub(crate) fn entry_path(name: &str, folder: bool) -> Result<&Path> {
    if folder {
        ensure!(
            valid_component(name) && !name.contains('/'),
            "Enter one valid directory name"
        );
    }
    let path = Path::new(name);
    validate_path(path)?;
    Ok(path)
}

pub(crate) fn delete(
    archive: &Archive,
    members: &[PathBuf],
    filesystems: &Filesystems,
    cancel: &CancelToken,
) -> Result<()> {
    let _guard = ARCHIVE_WRITE_LOCK.lock().unwrap();
    let stamp = archive.stamp()?;
    let index = archive.index(filesystems)?;
    for member in members {
        validate_path(member)?;
        ensure!(
            index.contains_key(member),
            "Archive entry is missing: {}",
            member.display()
        );
    }
    archive.rewrite(
        Changes {
            remove: members,
            write: &[],
        },
        &stamp,
        filesystems,
        cancel,
    )?;
    Ok(())
}

pub(crate) fn transfer_into(
    selections: &[Location],
    destination: &Location,
    moving: bool,
    mut policy: ConflictPolicy,
    ctx: &TransferContext<'_>,
) -> Result<()> {
    let Location::Archive { archive, member } = destination else {
        bail!("Expected an archive destination");
    };
    let _guard = ARCHIVE_WRITE_LOCK.lock().unwrap();
    ensure!(archive.writable(), "Destination archive is read-only");
    let stamp = archive.stamp()?;
    let index = archive.index(ctx.filesystems)?;
    check_directory(&index, member)?;
    let archive_source = selections.first().and_then(|source| match source {
        Location::Archive { archive, .. } => Some(archive),
        _ => None,
    });
    let source_archive_stamp = if moving {
        archive_source
            .map(|source| {
                ensure!(source.writable(), "Source archive is read-only");
                source.stamp()
            })
            .transpose()?
    } else {
        None
    };
    let entries = collect(selections, ctx)?;
    let roots: Vec<_> = entries
        .iter()
        .filter(|source| source.relative.components().count() == 1)
        .collect();
    for root in &roots {
        let original = &root.location;
        let target = archive.location(member.join(&root.relative));
        let same_container =
            original
                .local()
                .zip(destination.host_path())
                .is_some_and(|(file, container)| {
                    !root.directory && same_file::is_same_file(file, container).unwrap_or(false)
                });
        ensure!(
            !same_container && !destination.is_within(original) && &target != original,
            "An entry cannot be transferred into itself"
        );
    }

    let mut skipped = BTreeSet::new();
    let mut targets: BTreeSet<_> = entries
        .iter()
        .map(|entry| member.join(&entry.relative))
        .collect();
    let mut planned = Vec::new();
    for source in &entries {
        let mut target = member.join(&source.relative);
        let mut append = None;
        if let Some(existing) = index.get(&target) {
            ensure!(
                !existing.symlink && existing.directory == source.directory,
                "Conflicting archive entry type: {}",
                target.display()
            );
            if source.directory {
                continue;
            }
            let source_label = PathBuf::from(source.location.display());
            let target_label = PathBuf::from(archive.location(target.clone()).display());
            let decision = match policy {
                ConflictPolicy::Ask => (ctx.conflict)(&source_label, &target_label),
                ConflictPolicy::Overwrite => ConflictResolution::Overwrite,
                ConflictPolicy::Skip => ConflictResolution::Skip,
                ConflictPolicy::Rename => {
                    let name = free_name(&target, |path| {
                        index.contains_key(path) || targets.contains(path)
                    })?;
                    ConflictResolution::Rename(name)
                }
                ConflictPolicy::OnlyNewer => {
                    let millis = source
                        .modified
                        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                        .map_or(0., |time| time.as_millis() as f64);
                    if millis > existing.modified {
                        ConflictResolution::Overwrite
                    } else {
                        ConflictResolution::Skip
                    }
                }
                ConflictPolicy::Append => {
                    append = Some((archive.mounted(ctx.filesystems, ctx.cancel)?, existing.size));
                    ConflictResolution::Overwrite
                }
            };
            match decision {
                ConflictResolution::Overwrite => {}
                ConflictResolution::OverwriteAll => policy = ConflictPolicy::Overwrite,
                ConflictResolution::Skip | ConflictResolution::SkipAll => {
                    if matches!(decision, ConflictResolution::SkipAll) {
                        policy = ConflictPolicy::Skip;
                    }
                    skipped.insert(
                        source
                            .relative
                            .components()
                            .next()
                            .unwrap()
                            .as_os_str()
                            .to_owned(),
                    );
                    continue;
                }
                ConflictResolution::Cancel => {
                    ctx.cancel.cancel();
                    return Ok(());
                }
                ConflictResolution::Rename(name) => {
                    ensure!(
                        valid_component(&name) && !name.contains('/'),
                        "Invalid archive entry name"
                    );
                    target.set_file_name(name);
                    ensure!(
                        !index.contains_key(&target) && targets.insert(target.clone()),
                        "Archive destination already exists: {}",
                        target.display()
                    );
                }
            }
        }
        let target_location = archive.location(target.clone());
        ensure!(
            !roots
                .iter()
                .any(|root| root.location.is_within(&target_location)),
            "Transfer would overwrite a source archive"
        );
        planned.push(Planned {
            source,
            target,
            append,
        });
    }

    let moved: Vec<_> = roots
        .iter()
        .copied()
        .filter(|root| !skipped.contains(root.relative.as_os_str()))
        .collect();
    let members: Vec<_> = moved
        .iter()
        .filter_map(|root| root.location.archive_member().map(Path::to_path_buf))
        .collect();
    let in_place = archive_source.is_some_and(|source| source == archive);
    if moving {
        for source in &entries {
            source.check_local()?;
        }
        if let (Some(source), Some(stamp)) = (archive_source, &source_archive_stamp) {
            stamp.check(source.location(PathBuf::new()).host_path().unwrap())?;
        }
    }
    let remove = if moving && in_place {
        members.as_slice()
    } else {
        &[]
    };
    let changed = if !planned.is_empty() || !remove.is_empty() {
        let progress = std::cell::RefCell::new(Progress::new(&planned, ctx));
        let inputs: Vec<_> = planned.iter().map(|entry| entry.input(&progress)).collect();
        Some(archive.rewrite(
            Changes {
                remove,
                write: &inputs,
            },
            &stamp,
            ctx.filesystems,
            ctx.cancel,
        )?)
    } else {
        None
    };
    if moving && !in_place && !ctx.cancel.is_cancelled() {
        let destination_host = destination
            .host_path()
            .context("Destination archive has no local file")?;
        if let Some(updated) = &changed {
            updated.check(destination_host)?;
        }
        if let (Some(source), Some(stamp)) = (archive_source, &source_archive_stamp) {
            if !members.is_empty() {
                let source_location = source.location(PathBuf::new());
                let updated = if source_location.host_path() == destination.host_path() {
                    changed.as_ref().unwrap_or(stamp)
                } else {
                    stamp
                };
                source.rewrite(
                    Changes {
                        remove: &members,
                        write: &[],
                    },
                    updated,
                    ctx.filesystems,
                    ctx.cancel,
                )?;
            }
        } else {
            for root in moved {
                if ctx.cancel.is_cancelled() {
                    break;
                }
                for source in entries
                    .iter()
                    .filter(|source| source.relative.starts_with(&root.relative))
                {
                    source.check_local()?;
                }
                if let Some(updated) = &changed {
                    updated.check(destination_host)?;
                }
                trash::delete(root.location.local().unwrap())?;
            }
        }
    }
    Ok(())
}

pub(crate) fn extract(
    archive: &Archive,
    members: Vec<PathBuf>,
    destination: &Path,
    moving: bool,
    policy: ConflictPolicy,
    ctx: &TransferContext<'_>,
) -> Result<()> {
    // Keep the source snapshot stable until a successful move can remove it.
    let _guard = moving.then(|| ARCHIVE_WRITE_LOCK.lock().unwrap());
    let stamp = if moving {
        ensure!(archive.writable(), "Source archive is read-only");
        Some(archive.stamp()?)
    } else {
        None
    };
    let extracted = archive.extract(
        members,
        destination,
        moving,
        policy,
        ctx.filesystems,
        ctx.cancel,
        ctx.emit,
        ctx.conflict,
    )?;
    if let Some(stamp) = stamp
        && !extracted.is_empty()
        && !ctx.cancel.is_cancelled()
    {
        archive.rewrite(
            Changes {
                remove: &extracted,
                write: &[],
            },
            &stamp,
            ctx.filesystems,
            ctx.cancel,
        )?;
    }
    Ok(())
}

fn check_directory(index: &crate::archive::Index, path: &Path) -> Result<()> {
    ensure!(
        path.as_os_str().is_empty()
            || index
                .get(path)
                .is_some_and(|entry| entry.directory && !entry.symlink),
        "Archive destination directory does not exist"
    );
    Ok(())
}

fn free_name(path: &Path, exists: impl Fn(&Path) -> bool) -> Result<String> {
    let stem = path
        .file_stem()
        .context("Missing entry name")?
        .to_string_lossy();
    let extension = path
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy()))
        .unwrap_or_default();
    (1..=10_000)
        .map(|i| format!("{stem} ({i}){extension}"))
        .find(|name| !exists(&path.with_file_name(name)))
        .context("No available archive entry name")
}

#[cfg(all(test, feature = "zip-extension"))]
mod tests {
    use super::*;
    use dotdir_zip::ZIP;
    use std::io::Cursor;
    use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

    fn context<'a>(filesystems: &'a Filesystems, cancel: &'a CancelToken) -> TransferContext<'a> {
        TransferContext {
            filesystems,
            cancel,
            emit: &|_| {},
            conflict: &|_, _| ConflictResolution::Skip,
        }
    }

    fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    fn mount(path: impl Into<Location>) -> Location {
        Archive::mount_location(path.into(), &ZIP).unwrap()
    }

    fn entry(root: &Location, member: &str) -> Location {
        root.resolve_archive_member(member).unwrap()
    }

    #[test]
    fn nested_archive_transfers_and_mutations_preserve_other_entries() {
        let temp = tempfile::tempdir().unwrap();
        let host = temp.path().join("outer.zip");
        let inner_bytes = zip_bytes(&[("original.txt", b"original")]);
        let sibling_bytes = zip_bytes(&[("keep.txt", b"sibling")]);
        fs::write(
            &host,
            zip_bytes(&[
                ("one.zip", &inner_bytes),
                ("two.zip", &sibling_bytes),
                ("keep.txt", b"outer"),
            ]),
        )
        .unwrap();
        let outer = mount(host.clone());
        let inner = mount(entry(&outer, "one.zip"));
        let sibling = mount(entry(&outer, "two.zip"));
        let fs = Filesystems::new(Default::default());
        let cancel = CancelToken::new();
        let ctx = context(&fs, &cancel);

        let incoming = temp.path().join("incoming");
        fs::create_dir_all(incoming.join("empty")).unwrap();
        fs::write(incoming.join("source.txt"), b"copied").unwrap();
        transfer_into(
            &[incoming.clone().into()],
            &inner,
            false,
            ConflictPolicy::Ask,
            &ctx,
        )
        .unwrap();
        assert!(incoming.join("source.txt").exists());
        assert_eq!(
            entry(&inner, "incoming/source.txt")
                .read_preview(&fs)
                .unwrap(),
            b"copied"
        );
        assert!(
            entry(&inner, "incoming/empty")
                .archive_metadata(&fs)
                .unwrap()
                .unwrap()
                .directory
        );

        let new_dir = create_entry(&inner, "notes", true, &fs, &cancel).unwrap();
        let new_file = create_entry(&new_dir, "draft.txt", false, &fs, &cancel).unwrap();
        new_file.save_archive_member(b"", "written", &fs).unwrap();
        assert_eq!(new_file.read_preview(&fs).unwrap(), b"written");
        let reopened = create_entry(&new_dir, "draft.txt", false, &fs, &cancel).unwrap();
        assert_eq!(reopened.read_preview(&fs).unwrap(), b"written");

        // Moving within a ZIP uses one rewrite. Moving to another ZIP in
        // the same container must preserve both updates in that container.
        transfer_into(
            &[entry(&inner, "original.txt")],
            &new_dir,
            true,
            ConflictPolicy::Ask,
            &ctx,
        )
        .unwrap();
        assert!(
            entry(&inner, "original.txt")
                .archive_metadata(&fs)
                .unwrap()
                .is_none()
        );
        transfer_into(
            std::slice::from_ref(&new_dir),
            &sibling,
            true,
            ConflictPolicy::Ask,
            &ctx,
        )
        .unwrap();
        assert!(new_dir.archive_metadata(&fs).unwrap().is_none());
        assert_eq!(
            entry(&sibling, "notes/draft.txt")
                .read_preview(&fs)
                .unwrap(),
            b"written"
        );
        assert_eq!(
            entry(&sibling, "notes/original.txt")
                .read_preview(&fs)
                .unwrap(),
            b"original"
        );

        let Location::Archive { archive, .. } = &inner else {
            unreachable!()
        };
        let output = temp.path().join("output");
        fs::create_dir(&output).unwrap();
        fs::create_dir(output.join("incoming")).unwrap();
        fs::write(output.join("incoming/source.txt"), b"existing").unwrap();
        extract(
            archive,
            vec!["incoming".into()],
            &output,
            true,
            ConflictPolicy::Skip,
            &ctx,
        )
        .unwrap();
        assert_eq!(
            entry(&inner, "incoming/source.txt")
                .read_preview(&fs)
                .unwrap(),
            b"copied"
        );
        assert_eq!(
            fs::read(output.join("incoming/source.txt")).unwrap(),
            b"existing"
        );
        extract(
            archive,
            vec!["incoming".into()],
            &output,
            true,
            ConflictPolicy::Overwrite,
            &ctx,
        )
        .unwrap();
        assert_eq!(
            fs::read(output.join("incoming/source.txt")).unwrap(),
            b"copied"
        );
        assert!(output.join("incoming/empty").is_dir());
        assert!(
            entry(&inner, "incoming")
                .archive_metadata(&fs)
                .unwrap()
                .is_none()
        );

        let Location::Archive { archive, .. } = &sibling else {
            unreachable!()
        };
        delete(archive, &["notes".into()], &fs, &cancel).unwrap();
        assert!(
            entry(&sibling, "notes/draft.txt")
                .archive_metadata(&fs)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            entry(&sibling, "keep.txt").read_preview(&fs).unwrap(),
            b"sibling"
        );
        assert_eq!(
            entry(&outer, "keep.txt").read_preview(&fs).unwrap(),
            b"outer"
        );
    }

    #[test]
    fn creating_zip_applies_format_options_and_keeps_existing_destination() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("sample");
        fs::create_dir_all(folder.join("empty")).unwrap();
        fs::write(folder.join("note.txt"), b"compressed data".repeat(200)).unwrap();
        let source = Location::Local(folder);
        let fs = Filesystems::new(Default::default());
        let cancel = CancelToken::new();
        let ctx = context(&fs, &cancel);
        let plain = temp.path().join("plain.zip");
        let stored = CreationOptions::from([("compression", "store".into())]);
        create(std::slice::from_ref(&source), &plain, &ZIP, &stored, &ctx).unwrap();
        let mut zip = ZipArchive::new(File::open(&plain).unwrap()).unwrap();
        assert!(zip.by_name("sample/empty/").unwrap().is_dir());
        assert_eq!(
            zip.by_name("sample/note.txt").unwrap().compression(),
            CompressionMethod::Stored
        );
        let original = fs::read(&plain).unwrap();

        let secret = temp.path().join("secret.zip");
        let options = CreationOptions::from([
            ("compression", "maximum".into()),
            ("password", "unit-test-key".into()),
        ]);
        create(std::slice::from_ref(&source), &secret, &ZIP, &options, &ctx).unwrap();
        let mut zip = ZipArchive::new(File::open(&secret).unwrap()).unwrap();
        assert!(zip.by_name("sample/note.txt").is_err());
        let mut file = zip
            .by_name_decrypt("sample/note.txt", b"unit-test-key")
            .unwrap();
        assert_eq!(file.compression(), CompressionMethod::Deflated);
        let mut content = Vec::new();
        file.read_to_end(&mut content).unwrap();
        assert_eq!(content, b"compressed data".repeat(200));
        assert!(create(&[source], &plain, &ZIP, &options, &ctx).is_err());
        assert_eq!(fs::read(&plain).unwrap(), original);
    }

    #[test]
    fn protected_zip_prompts_retries_and_preserves_encryption_across_operations() {
        use crate::archive_passwords::{ArchivePasswords, DEFAULT_MINUTES};
        use dotdir_archive_api::Password;
        use std::sync::Mutex;
        use zip::AesSalt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("protected.zip");
        let child = zip_bytes(&[("readme.txt", b"nested")]);
        let mut writer = ZipWriter::new(File::create(&path).unwrap());
        for (index, (name, bytes)) in [("nested.zip", child.as_slice()), ("existing.txt", b"old")]
            .into_iter()
            .enumerate()
        {
            let options = SimpleFileOptions::default()
                .compression_method(CompressionMethod::Stored)
                .with_aes_encryption_and_salt(
                    b"sample-password",
                    AesSalt::Aes256([index as u8; 16]),
                );
            writer.start_file(name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();

        let prompts = Arc::new(Mutex::new(Vec::new()));
        let observed = prompts.clone();
        let passwords = ArchivePasswords::new(DEFAULT_MINUTES, move |request| {
            let mut prompts = observed.lock().unwrap();
            prompts.push(request.incorrect);
            let password = if prompts.len() == 1 {
                "wrong-input"
            } else {
                "sample-password"
            };
            request
                .reply
                .send(Password::new(password.as_bytes().to_vec()))
                .unwrap();
        });
        let mut fs = Filesystems::new(Default::default());
        fs.archive_passwords = Arc::new(passwords);
        let outer = mount(path.clone());
        let inner = mount(entry(&outer, "nested.zip"));
        let nested = entry(&inner, "readme.txt");
        let existing = entry(&outer, "existing.txt");

        assert_eq!(nested.read_preview(&fs).unwrap(), b"nested");
        assert_eq!(*prompts.lock().unwrap(), [false, true]);
        let fs = fs.rebuild_providers(Default::default());
        existing
            .save_archive_member(b"old", "changed", &fs)
            .unwrap();
        nested
            .save_archive_member(b"nested", "nested edit", &fs)
            .unwrap();

        let cancel = CancelToken::new();
        let source = temp.path().join("incoming.txt");
        fs::write(&source, b"new entry").unwrap();
        let scoped = fs.archive_operation();
        let ctx = context(&scoped, &cancel);
        transfer_into(&[source.into()], &outer, false, ConflictPolicy::Ask, &ctx).unwrap();
        let output = temp.path().join("output");
        fs::create_dir(&output).unwrap();
        let Location::Archive { archive, .. } = &outer else {
            unreachable!()
        };
        extract(
            archive,
            vec!["existing.txt".into()],
            &output,
            false,
            ConflictPolicy::Ask,
            &context(&fs.archive_operation(), &cancel),
        )
        .unwrap();
        assert_eq!(fs::read(output.join("existing.txt")).unwrap(), b"changed");
        assert_eq!(prompts.lock().unwrap().len(), 2);

        let mut written = ZipArchive::new(File::open(path).unwrap()).unwrap();
        for (name, expected) in [
            ("incoming.txt", b"new entry".as_slice()),
            ("existing.txt", b"changed"),
        ] {
            assert!(written.by_name(name).is_err());
            let mut file = written.by_name_decrypt(name, b"sample-password").unwrap();
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, expected);
        }
        assert!(written.by_name("nested.zip").is_err());

        fs.archive_passwords.set_minutes(0);
        assert!(nested.read_preview(&fs.without_password_prompts()).is_err());
        assert_eq!(nested.read_preview(&fs).unwrap(), b"nested edit");
        assert_eq!(prompts.lock().unwrap().len(), 3);
        existing
            .save_archive_member(b"changed", "no cache", &fs)
            .unwrap();
        assert_eq!(prompts.lock().unwrap().len(), 4);
    }
}
