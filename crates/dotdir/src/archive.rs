use crate::{
    archive_passwords::{ArchiveKey, UnlockCancelled, UnlockRequired},
    filesystems::{Filesystems, ProviderPath},
};
use anyhow::{Context, Result, bail, ensure};
pub use dotdir_archive_api::VirtualFile;
use dotdir_archive_api::{ArchiveFormat, ArchiveReader, Changes, FormatInfo, Input, WriteSeek};
use dotdir_core::copy::{CancelToken, ConflictPolicy, ConflictResolution, CopyEvent, CopyProgress};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ffi::OsStr,
    fs::{self, File},
    hash::{Hash, Hasher},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::{Instant, SystemTime},
};

pub const PREVIEW_LIMIT: u64 = 16 * 1024 * 1024;
pub const SEEKABLE_LIMIT: u64 = 64 * 1024 * 1024;
const MAX_ARCHIVE_DEPTH: usize = 8;
pub(crate) static ARCHIVE_WRITE_LOCK: Mutex<()> = Mutex::new(());

pub struct ArchiveFormats(&'static [&'static dyn ArchiveFormat]);

impl ArchiveFormats {
    pub const fn new(formats: &'static [&'static dyn ArchiveFormat]) -> Self {
        Self(formats)
    }

    pub fn iter(&self) -> impl Iterator<Item = &'static FormatInfo> + '_ {
        self.0.iter().map(|format| format.info())
    }

    pub fn creators(&self) -> impl Iterator<Item = &'static dyn ArchiveFormat> + '_ {
        self.0
            .iter()
            .copied()
            .filter(|format| format.creation().is_some())
    }

    pub fn for_path(&self, path: &Path) -> Option<&'static dyn ArchiveFormat> {
        let name = path.file_name()?.to_string_lossy();
        let bytes = name.as_bytes();
        self.0
            .iter()
            .flat_map(|&format| {
                format.info().suffixes.iter().filter_map(move |suffix| {
                    let prefix = bytes.len().checked_sub(suffix.len() + 1)?;
                    (prefix > 0
                        && bytes[prefix] == b'.'
                        && bytes[prefix + 1..].eq_ignore_ascii_case(suffix.as_bytes()))
                    .then_some((suffix.len(), format))
                })
            })
            .max_by_key(|(length, _)| *length)
            .map(|(_, format)| format)
    }

    fn by_id(&self, id: &str) -> Option<&'static dyn ArchiveFormat> {
        self.0.iter().copied().find(|format| format.info().id == id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Location {
    Local(PathBuf),
    Archive {
        archive: Arc<Archive>,
        member: PathBuf,
    },
    Provider(ProviderPath),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SavedLocation {
    Local(PathBuf),
    Archive {
        format: String,
        source: Box<SavedLocation>,
        member: PathBuf,
    },
    Provider {
        uri: ProviderPath,
    },
    LegacyZip {
        archive: PathBuf,
        member: PathBuf,
    },
    LegacyNestedZip {
        source: Box<SavedLocation>,
        member: PathBuf,
    },
}

impl SavedLocation {
    pub fn restore(self, formats: &ArchiveFormats) -> Option<Location> {
        self.restore_at(formats, 0)
    }

    fn restore_at(self, formats: &ArchiveFormats, depth: usize) -> Option<Location> {
        const LEGACY_ZIP_ID: &str = "dotdir.zip";
        let (format, source, member) = match self {
            Self::Local(path) => {
                let available = if depth == 0 {
                    path.is_dir()
                } else {
                    path.is_file()
                };
                return available.then_some(Location::Local(path));
            }
            Self::Provider { uri } => return Some(Location::Provider(uri)),
            Self::Archive {
                format,
                source,
                member,
            } => (formats.by_id(&format)?, *source, member),
            Self::LegacyZip { archive, member } => {
                (formats.by_id(LEGACY_ZIP_ID)?, Self::Local(archive), member)
            }
            Self::LegacyNestedZip { source, member } => {
                (formats.by_id(LEGACY_ZIP_ID)?, *source, member)
            }
        };
        if depth >= MAX_ARCHIVE_DEPTH {
            return None;
        }
        let source = source.restore_at(formats, depth + 1)?;
        Some(Arc::new(Archive::new(source, format)).location(member))
    }
}

impl From<&Location> for SavedLocation {
    fn from(location: &Location) -> Self {
        match location {
            Location::Local(path) => Self::Local(path.clone()),
            Location::Archive { archive, member } => Self::Archive {
                format: archive.info().id.into(),
                source: Box::new(Self::from(&archive.source)),
                member: member.clone(),
            },
            Location::Provider(uri) => Self::Provider { uri: uri.clone() },
        }
    }
}

impl From<PathBuf> for Location {
    fn from(path: PathBuf) -> Self {
        Self::Local(path)
    }
}

impl From<&Path> for Location {
    fn from(path: &Path) -> Self {
        Self::Local(path.into())
    }
}

pub(crate) fn display_local_path(path: &Path) -> String {
    let display = path.display().to_string();
    #[cfg(windows)]
    if let Some(std::path::Component::Prefix(prefix)) = path.components().next() {
        use std::path::Prefix;
        match prefix.kind() {
            Prefix::VerbatimDisk(_) => return display[4..].to_owned(),
            Prefix::VerbatimUNC(_, _) => return format!(r"\\{}", &display[8..]),
            _ => {}
        }
    }
    display
}

impl Location {
    pub fn editable(&self) -> bool {
        match self {
            Self::Local(_) => true,
            Self::Archive { archive, .. } => archive.writable(),
            Self::Provider(_) => false,
        }
    }

    pub(crate) fn is_within(&self, ancestor: &Self) -> bool {
        match (self, ancestor) {
            (Self::Local(path), Self::Local(parent)) => path.starts_with(parent),
            (
                Self::Archive { archive, member },
                Self::Archive {
                    archive: parent,
                    member: prefix,
                },
            ) if archive == parent => member.starts_with(prefix),
            (Self::Archive { archive, .. }, _) => archive.source.is_within(ancestor),
            _ => self == ancestor,
        }
    }

    pub fn archive_root(&self) -> Option<Self> {
        match self {
            Self::Archive { archive, .. } => Some(archive.location(PathBuf::new())),
            _ => None,
        }
    }

    pub fn archive_member(&self) -> Option<&Path> {
        match self {
            Self::Archive { member, .. } => Some(member),
            _ => None,
        }
    }

    pub fn resolve_archive_member(&self, path: &str) -> Option<Self> {
        let Self::Archive { archive, .. } = self else {
            return None;
        };
        if path.is_empty() {
            return Some(archive.location(PathBuf::new()));
        }
        path.split('/')
            .all(valid_component)
            .then(|| archive.location(path.into()))
    }

    pub fn archive_metadata(&self, filesystems: &Filesystems) -> Result<Option<Member>> {
        let Self::Archive { archive, member } = self else {
            return Ok(None);
        };
        Ok(archive.index(filesystems)?.get(member).cloned())
    }

    pub fn local(&self) -> Option<&Path> {
        match self {
            Self::Local(path) => Some(path),
            _ => None,
        }
    }

    pub fn host_path(&self) -> Option<&Path> {
        match self {
            Self::Local(path) => Some(path),
            Self::Archive { archive, .. } => archive.source.host_path(),
            Self::Provider(_) => None,
        }
    }

    pub fn name_path(&self) -> &Path {
        match self {
            Self::Archive { archive, member } => {
                if member.as_os_str().is_empty() {
                    archive.source.name_path()
                } else {
                    member
                }
            }
            Self::Provider(path) => Path::new(path.name().unwrap_or_default()),
            Self::Local(path) => path,
        }
    }

    pub fn file_name(&self) -> Option<&OsStr> {
        match self {
            Self::Provider(path) => path.name().map(OsStr::new),
            _ => self.name_path().file_name(),
        }
    }

    pub fn label(&self) -> String {
        self.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| {
                if let Self::Local(path) = self {
                    return if path.has_root() {
                        display_local_path(path)
                    } else {
                        std::path::MAIN_SEPARATOR.to_string()
                    };
                }
                if matches!(self, Self::Provider(_)) {
                    self.display()
                } else {
                    "/".into()
                }
            })
    }

    pub fn extension(&self) -> Option<&OsStr> {
        self.name_path().extension()
    }

    pub fn parent(&self) -> Option<Self> {
        match self {
            Self::Archive { archive, member } if !member.as_os_str().is_empty() => {
                Some(archive.location(member.parent()?.into()))
            }
            Self::Archive { archive, .. } => archive.source.parent(),
            Self::Provider(path) => Some(Self::Provider(path.parent()?)),
            Self::Local(path) => Some(path.parent()?.into()),
        }
    }

    pub fn display(&self) -> String {
        match self {
            Self::Local(path) => path.display().to_string(),
            Self::Archive { archive, member } => {
                format!("{}!/{}", archive.source.display(), member.display())
            }
            Self::Provider(path) => path.to_string(),
        }
    }

    pub fn read_preview(&self, filesystems: &Filesystems) -> Result<Vec<u8>> {
        self.read_limited(filesystems, PREVIEW_LIMIT, &CancelToken::new())
    }

    pub fn read_limited(
        &self,
        filesystems: &Filesystems,
        limit: u64,
        cancel: &CancelToken,
    ) -> Result<Vec<u8>> {
        ensure!(!cancel.is_cancelled(), "File read cancelled");
        match self {
            Self::Local(path) => {
                let file = File::open(path)?;
                let size = file.metadata()?.len();
                ensure!(
                    size <= limit,
                    "File exceeds the {} MiB preview limit",
                    limit / (1024 * 1024)
                );
                read_exact_bytes(file, size, cancel)
            }
            Self::Archive { archive, member } => {
                archive.read_member(member, &filesystems.archive_operation(), limit, cancel)
            }
            Self::Provider(path) => filesystems.read_file(path, limit as usize, cancel),
        }
    }

    pub fn open_seekable(
        &self,
        filesystems: &Filesystems,
        cancel: &CancelToken,
    ) -> Result<VirtualFile> {
        ensure!(!cancel.is_cancelled(), "File read cancelled");
        match self {
            Self::Local(path) => VirtualFile::local(path),
            Self::Archive { archive, member } => archive.open_member(
                member,
                &filesystems.archive_operation(),
                cancel,
                SEEKABLE_LIMIT,
            ),
            Self::Provider(path) => {
                if filesystems.is_remote(path) {
                    let file = filesystems.open_remote(path, cancel)?;
                    Ok(VirtualFile::streaming(file.reader, file.size))
                } else {
                    Ok(VirtualFile::memory(filesystems.read_file(
                        path,
                        SEEKABLE_LIMIT as usize,
                        cancel,
                    )?))
                }
            }
        }
    }

    fn archive_depth(&self) -> usize {
        match self {
            Self::Archive { archive, .. } => archive.depth,
            _ => 0,
        }
    }

    pub fn save_archive_member(
        &self,
        expected: &[u8],
        text: &str,
        filesystems: &Filesystems,
    ) -> Result<()> {
        let Self::Archive { archive, member } = self else {
            bail!("Expected an archive file entry");
        };
        let filesystems = &filesystems.archive_operation();
        let _guard = ARCHIVE_WRITE_LOCK.lock().unwrap();
        let stamp = archive.stamp()?;
        ensure!(
            text.len() as u64 <= PREVIEW_LIMIT,
            "Archive text entries are limited to 16 MiB"
        );
        ensure!(
            self.read_preview(filesystems)? == expected,
            "Archive entry changed on disk. Your edits are still open."
        );

        let input = input_bytes(member.clone(), text.as_bytes());
        archive.rewrite(
            Changes {
                remove: &[],
                write: &[input],
            },
            &stamp,
            filesystems,
            &CancelToken::new(),
        )?;
        Ok(())
    }
}

fn input_bytes(path: PathBuf, bytes: &[u8]) -> Input<'_> {
    Input {
        path,
        size: bytes.len() as u64,
        modified: None,
        unix_mode: None,
        contents: Some(Box::new(move |read| read(&mut io::Cursor::new(bytes)))),
    }
}

pub(crate) struct CancellableOutput<'a> {
    pub output: &'a mut dyn WriteSeek,
    pub cancel: &'a CancelToken,
}

impl Write for CancellableOutput<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(io::Error::other("Archive operation cancelled"));
        }
        self.output.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

impl Seek for CancellableOutput<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.output.seek(position)
    }
}

#[derive(Default)]
struct NestedArchiveOutput(io::Cursor<Vec<u8>>);

impl Write for NestedArchiveOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if (bytes.len() as u64).saturating_add(self.0.position()) > SEEKABLE_LIMIT {
            return Err(io::Error::other(
                "The updated nested archive exceeds the 64 MiB memory limit",
            ));
        }
        self.0.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for NestedArchiveOutput {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.0.seek(from)
    }
}

fn read_exact_bytes(reader: impl Read, size: u64, cancel: &CancelToken) -> Result<Vec<u8>> {
    let mut reader = reader.take(size + 1);
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(usize::try_from(size)?)?;
    let mut buffer = [0; 64 * 1024];
    loop {
        ensure!(!cancel.is_cancelled(), "File read cancelled");
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        ensure!(
            bytes.len() as u64 + count as u64 <= size,
            "File exceeds its declared size"
        );
        bytes.extend_from_slice(&buffer[..count]);
    }
    ensure!(
        bytes.len() as u64 == size,
        "File size does not match its contents"
    );
    Ok(bytes)
}

#[derive(Debug)]
pub struct Archive {
    source: Location,
    format: &'static dyn ArchiveFormat,
    depth: usize,
    cache: Mutex<Option<CachedArchive>>,
}

#[derive(Debug)]
struct CachedArchive {
    revision: ArchiveRevision,
    mounted: Weak<MountedArchive>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ArchiveRevision {
    length: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SourceStamp {
    revision: ArchiveRevision,
    identity: same_file::Handle,
}

impl SourceStamp {
    pub(crate) fn from_path(path: &Path) -> Result<Self> {
        Self::from_file(&File::open(path)?)
    }

    pub(crate) fn from_file(file: &File) -> Result<Self> {
        Ok(Self {
            revision: file.metadata()?.into(),
            identity: same_file::Handle::from_file(file.try_clone()?)?,
        })
    }

    pub(crate) fn same_file(&self, other: &Self) -> bool {
        self.identity == other.identity
    }

    pub(crate) fn check(&self, path: &Path) -> Result<()> {
        ensure!(
            *self == Self::from_path(path)?,
            "Source changed during the operation: {}",
            path.display()
        );
        Ok(())
    }
}

impl From<fs::Metadata> for ArchiveRevision {
    fn from(metadata: fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            length: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            #[cfg(unix)]
            identity: (metadata.dev(), metadata.ino()),
        }
    }
}

#[derive(Debug)]
pub(crate) struct MountedArchive {
    reader: Box<dyn ArchiveReader>,
    index: Mutex<Option<Arc<Index>>>,
    memory_usage: u64,
    key: ArchiveKey,
    source_protected: bool,
}

// Directory history keeps locations alive. Limit how much of their decompressed
// data and how many open archive handles can remain cached after navigation.
fn retain_mount(mount: &Arc<MountedArchive>) {
    static RECENT: Mutex<VecDeque<Arc<MountedArchive>>> = Mutex::new(VecDeque::new());
    let mut recent = RECENT.lock().unwrap();
    recent.retain(|previous| !Arc::ptr_eq(previous, mount));
    recent.push_back(mount.clone());
    let mut bytes: u64 = recent.iter().map(|item| item.memory_usage).sum();
    while recent.len() > 16 || bytes > 2 * SEEKABLE_LIMIT {
        bytes -= recent.pop_front().unwrap().memory_usage;
    }
}

#[derive(Debug, Clone)]
pub struct Member {
    pub name: String,
    pub directory: bool,
    pub symlink: bool,
    encrypted: bool,
    pub size: u64,
    pub modified: f64,
}

pub(crate) type Index = BTreeMap<PathBuf, Member>;

impl PartialEq for Archive {
    fn eq(&self, other: &Self) -> bool {
        self.info().id == other.info().id && self.source == other.source
    }
}
impl Eq for Archive {}
impl Hash for Archive {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.info().id.hash(state);
        self.source.hash(state);
    }
}

impl Archive {
    fn new(source: Location, format: &'static dyn ArchiveFormat) -> Self {
        Self {
            depth: source.archive_depth() + 1,
            source,
            format,
            cache: Mutex::new(None),
        }
    }

    pub fn info(&self) -> &'static FormatInfo {
        self.format.info()
    }

    pub fn mount_location(
        source: Location,
        format: &'static dyn ArchiveFormat,
    ) -> Result<Location> {
        ensure!(
            source.archive_depth() < MAX_ARCHIVE_DEPTH,
            "Archive nesting is limited to {MAX_ARCHIVE_DEPTH} layers"
        );
        let source = match source {
            Location::Local(path) => Location::Local(path.canonicalize().unwrap_or(path)),
            source => source,
        };
        Ok(Arc::new(Self::new(source, format)).location(PathBuf::new()))
    }

    pub(crate) fn location(self: &Arc<Self>, member: PathBuf) -> Location {
        Location::Archive {
            archive: self.clone(),
            member,
        }
    }

    pub(crate) fn mounted(
        &self,
        filesystems: &Filesystems,
        cancel: &CancelToken,
    ) -> Result<Arc<MountedArchive>> {
        ensure!(
            self.depth <= MAX_ARCHIVE_DEPTH,
            "Archive nesting is limited to {MAX_ARCHIVE_DEPTH} layers"
        );
        let revision = self
            .source
            .host_path()
            .map(fs::metadata)
            .transpose()?
            .map(ArchiveRevision::from);
        let mut cache = self.cache.lock().unwrap();
        if let (Some(revision), Some(cached)) = (&revision, &*cache)
            && cached.revision == *revision
            && let Some(mounted) = cached.mounted.upgrade()
        {
            retain_mount(&mounted);
            return Ok(mounted);
        }
        *cache = None;
        let source = self.source.open_seekable(filesystems, cancel)?;
        let memory_usage = source.memory_usage();
        let key = ArchiveKey {
            source: self.source.clone(),
            format: self.info().id,
        };
        let access = filesystems.password_access(&key, cancel);
        let encrypted_source = source.is_protected();
        let reader = self.format.open(source, &access).map_err(|error| {
            if error.is::<UnlockCancelled>() || error.is::<UnlockRequired>() {
                error
            } else {
                error.context(format!("Open archive {}", self.source.display()))
            }
        })?;
        let source_protected = encrypted_source || access.requested.get();
        let mounted = Arc::new(MountedArchive {
            reader,
            index: Mutex::new(None),
            memory_usage,
            key,
            source_protected,
        });
        if let Some(revision) = revision
            && !source_protected
        {
            *cache = Some(CachedArchive {
                revision,
                mounted: Arc::downgrade(&mounted),
            });
            retain_mount(&mounted);
        }
        Ok(mounted)
    }

    pub(crate) fn index(&self, filesystems: &Filesystems) -> Result<Arc<Index>> {
        self.mounted(filesystems, &CancelToken::new())?.index()
    }

    pub(crate) fn writable(&self) -> bool {
        self.info().writable && self.source.editable()
    }

    pub(crate) fn stamp(&self) -> Result<SourceStamp> {
        SourceStamp::from_path(
            self.source
                .host_path()
                .context("The archive has no local source")?,
        )
    }

    /// The caller holds ARCHIVE_WRITE_LOCK across validation and publication.
    pub(crate) fn rewrite(
        &self,
        changes: Changes<'_>,
        stamp: &SourceStamp,
        filesystems: &Filesystems,
        cancel: &CancelToken,
    ) -> Result<SourceStamp> {
        ensure!(self.writable(), "This archive is read-only");
        let outer = self
            .source
            .host_path()
            .context("The archive has no writable local source")?;
        stamp.check(outer)?;
        let permissions = fs::metadata(outer)?.permissions();
        ensure!(!permissions.readonly(), "The archive file is read-only");
        let snapshot = self.mounted(filesystems, cancel)?;
        let passwords = filesystems.password_access(&snapshot.key, cancel);
        let rewrite = |output: &mut dyn WriteSeek| {
            snapshot.reader.rewrite(
                changes,
                &passwords,
                &mut CancellableOutput { output, cancel },
            )
        };
        match &self.source {
            Location::Local(_) => {
                let parent = outer.parent().context("Missing archive directory")?;
                let mut output = tempfile::NamedTempFile::new_in(parent)?;
                rewrite(output.as_file_mut())?;
                output.as_file().set_permissions(permissions)?;
                output.as_file().sync_all()?;
                let written = SourceStamp::from_file(output.as_file())?;
                stamp.check(outer)?;
                ensure!(!cancel.is_cancelled(), "Archive operation cancelled");
                output.persist(outer)?;
                Ok(written)
            }
            Location::Archive { archive, member } => {
                let mut output = NestedArchiveOutput::default();
                rewrite(&mut output)?;
                let bytes = output.0.into_inner();
                let input = input_bytes(member.clone(), &bytes);
                archive.rewrite(
                    Changes {
                        remove: &[],
                        write: &[input],
                    },
                    stamp,
                    filesystems,
                    cancel,
                )
            }
            Location::Provider(_) => bail!("This archive is read-only"),
        }
    }

    pub fn list(
        self: &Arc<Self>,
        dir: &Path,
        filesystems: &Filesystems,
    ) -> Result<Vec<(Location, Member)>> {
        let entries = self.index(filesystems)?;
        if !dir.as_os_str().is_empty() {
            ensure!(
                entries.get(dir).is_some_and(|entry| entry.directory),
                "Archive directory is missing"
            );
        }
        Ok(entries
            .range(dir.to_path_buf()..)
            .take_while(|(path, _)| path.starts_with(dir))
            .filter(|(path, _)| path.parent() == Some(dir))
            .map(|(path, member)| (self.location(path.clone()), member.clone()))
            .collect())
    }

    fn read_member(
        &self,
        path: &Path,
        filesystems: &Filesystems,
        limit: u64,
        cancel: &CancelToken,
    ) -> Result<Vec<u8>> {
        self.mounted(filesystems, cancel)?
            .read_member(path, limit, filesystems, cancel)
    }

    fn open_member(
        &self,
        path: &Path,
        filesystems: &Filesystems,
        cancel: &CancelToken,
        limit: u64,
    ) -> Result<VirtualFile> {
        let mounted = self.mounted(filesystems, cancel)?;
        let (size, encrypted) = mounted.regular_entry(path)?;
        if let Some(file) = mounted.reader.seekable_member(path)? {
            ensure!(
                file.len() == size,
                "Archive entry size does not match its contents"
            );
            return Ok(if encrypted || mounted.source_protected {
                file.mark_protected()
            } else {
                file
            });
        }
        ensure!(
            size <= limit,
            "Compressed or encrypted archive entries over {} MiB cannot be opened in memory",
            limit / (1024 * 1024)
        );
        let file = VirtualFile::memory(mounted.read_member(path, limit, filesystems, cancel)?);
        Ok(if encrypted || mounted.source_protected {
            file.mark_protected()
        } else {
            file
        })
    }

    pub fn extract(
        &self,
        members: Vec<PathBuf>,
        destination: &Path,
        moving: bool,
        mut policy: ConflictPolicy,
        filesystems: &Filesystems,
        cancel: &CancelToken,
        emit: &dyn Fn(CopyEvent),
        conflict: &dyn Fn(&Path, &Path) -> ConflictResolution,
    ) -> Result<Vec<PathBuf>> {
        let mounted = self.mounted(filesystems, cancel)?;
        let entries = mounted.index()?;
        let base = destination.canonicalize()?;
        let archive_path = self
            .source
            .host_path()
            .map(Path::canonicalize)
            .transpose()?;
        let mut jobs = BTreeMap::new();
        let mut completed = members.iter().cloned().collect::<BTreeSet<_>>();
        let mut published = Vec::new();
        for member in members.iter().collect::<BTreeSet<_>>() {
            ensure!(
                entries.contains_key(member),
                "Archive entry is missing: {}",
                member.display()
            );
            let parent = member
                .parent()
                .context("Select an archive entry to extract")?;
            for (path, entry) in entries
                .range(member.clone()..)
                .take_while(|(path, _)| path.starts_with(member))
            {
                let relative = path.strip_prefix(parent)?;
                ensure!(
                    !entry.symlink,
                    "Archive contains a symbolic link: {}",
                    path.display()
                );
                if let Some(previous) = jobs.insert(relative, (path, entry)) {
                    ensure!(
                        previous.0 == path,
                        "Selected archive entries have the same extraction path"
                    );
                }
            }
        }
        let mut progress = CopyProgress {
            bytes_copied: 0,
            bytes_total: jobs
                .values()
                .fold(0u64, |total, (_, entry)| total.saturating_add(entry.size)),
            files_done: 0,
            files_total: jobs.len().min(u32::MAX as usize) as u32,
            current_file: String::new(),
        };
        let mut last_progress = Instant::now();
        let mut buffer = vec![0; 128 * 1024];
        for (relative, (path, entry)) in jobs {
            if cancel.is_cancelled() {
                break;
            }
            progress.current_file = path.display().to_string();
            let target = base.join(relative);
            if entry.directory {
                ensure_directory(&base, relative)?;
            } else {
                let parent = relative.parent().context("Invalid extraction path")?;
                ensure_directory(&base, parent)?;
                let source_label =
                    PathBuf::from(format!("{}!/{}", self.source.display(), path.display()));
                if let Some((target, mode)) = extraction_target(
                    &source_label,
                    target,
                    entry.modified,
                    &mut policy,
                    cancel,
                    conflict,
                )? {
                    ensure!(
                        archive_path.as_ref().is_none_or(|source| target != *source
                            && !same_file::is_same_file(&target, source).unwrap_or(false)),
                        "Extraction would overwrite the source archive"
                    );
                    ensure_directory(&base, parent)?;
                    let mut file = tempfile::NamedTempFile::new_in(base.join(parent))?;
                    if matches!(mode, WriteMode::Append) {
                        ensure!(
                            target.symlink_metadata()?.is_file(),
                            "Extraction destination changed: {}",
                            target.display()
                        );
                        let mut original = File::open(&target)?;
                        loop {
                            if cancel.is_cancelled() {
                                return Ok(Vec::new());
                            }
                            let count = original.read(&mut buffer)?;
                            if count == 0 {
                                break;
                            }
                            file.write_all(&buffer[..count])?;
                        }
                    }
                    mounted.with_member(path, filesystems, cancel, &mut |input| {
                        let mut written = 0u64;
                        loop {
                            if cancel.is_cancelled() {
                                return Ok(());
                            }
                            let count = input.read(&mut buffer)?;
                            if count == 0 {
                                break;
                            }
                            written += count as u64;
                            ensure!(
                                written <= entry.size,
                                "Archive entry exceeds its declared size"
                            );
                            file.write_all(&buffer[..count])?;
                            if last_progress.elapsed().as_millis() >= 50 {
                                let mut snapshot = progress.clone();
                                snapshot.bytes_copied =
                                    snapshot.bytes_copied.saturating_add(written);
                                emit(CopyEvent::Progress(snapshot));
                                last_progress = Instant::now();
                            }
                        }
                        ensure!(
                            written == entry.size,
                            "Archive entry size does not match its contents"
                        );
                        Ok(())
                    })?;
                    if cancel.is_cancelled() {
                        return Ok(Vec::new());
                    }
                    if moving {
                        file.as_file().sync_all()?;
                    }
                    let written = if matches!(mode, WriteMode::New) {
                        file.persist_noclobber(&target)?
                    } else {
                        file.persist(&target)?
                    };
                    if moving {
                        published.push((target, ArchiveRevision::from(written.metadata()?)));
                    }
                } else {
                    completed.retain(|root| !path.starts_with(root));
                }
            }
            progress.bytes_copied = progress.bytes_copied.saturating_add(entry.size);
            progress.files_done = progress.files_done.saturating_add(1);
            if last_progress.elapsed().as_millis() >= 50
                || progress.files_done == progress.files_total
            {
                emit(CopyEvent::Progress(progress.clone()));
                last_progress = Instant::now();
            }
        }
        for (path, revision) in published {
            ensure!(
                ArchiveRevision::from(path.symlink_metadata()?) == revision,
                "Extraction destination changed during the move: {}",
                path.display()
            );
        }
        Ok(if cancel.is_cancelled() {
            Vec::new()
        } else {
            completed.into_iter().collect()
        })
    }
}

impl MountedArchive {
    pub(crate) fn with_member(
        &self,
        path: &Path,
        filesystems: &Filesystems,
        cancel: &CancelToken,
        read: &mut dyn FnMut(&mut dyn Read) -> Result<()>,
    ) -> Result<()> {
        self.reader
            .with_member(path, &filesystems.password_access(&self.key, cancel), read)
    }

    pub(crate) fn index(&self) -> Result<Arc<Index>> {
        let mut cached = self.index.lock().unwrap();
        if let Some(entries) = &*cached {
            return Ok(entries.clone());
        }
        let entries = Arc::new(build_index(&*self.reader)?);
        *cached = Some(entries.clone());
        Ok(entries)
    }

    fn regular_entry(&self, path: &Path) -> Result<(u64, bool)> {
        let entries = self.index()?;
        let entry = entries.get(path).context("Archive entry is missing")?;
        ensure!(
            !entry.directory && !entry.symlink,
            "Archive entry is not a regular file"
        );
        Ok((entry.size, entry.encrypted))
    }

    fn read_member(
        &self,
        path: &Path,
        limit: u64,
        filesystems: &Filesystems,
        cancel: &CancelToken,
    ) -> Result<Vec<u8>> {
        let (size, _) = self.regular_entry(path)?;
        ensure!(
            size <= limit,
            "Archive previews are limited to {} MiB",
            limit / (1024 * 1024)
        );
        let mut bytes = None;
        self.with_member(path, filesystems, cancel, &mut |input| {
            bytes = Some(read_exact_bytes(input, size, cancel)?);
            Ok(())
        })?;
        bytes.context("Archive extension did not return entry data")
    }
}

fn build_index(reader: &dyn ArchiveReader) -> Result<Index> {
    let mut entries = Index::new();
    reader.visit_entries(&mut |file| {
        let clean = file.path.strip_suffix('/').unwrap_or(&file.path);
        if clean.is_empty() || !clean.split('/').all(valid_component) {
            return Ok(());
        }
        let path = PathBuf::from(clean);
        let is_directory = file.directory;
        let entry = Member {
            name: clean.rsplit('/').next().unwrap_or_default().into(),
            directory: is_directory,
            symlink: file.symlink,
            encrypted: file.encrypted,
            size: if is_directory { 0 } else { file.size },
            modified: file.modified,
        };
        ensure!(
            entries
                .get(&path)
                .is_none_or(|old| old.directory && is_directory),
            "Conflicting archive entries: {}",
            path.display()
        );
        entries.insert(path.clone(), entry);
        for parent in path
            .ancestors()
            .skip(1)
            .filter(|p| !p.as_os_str().is_empty())
        {
            let entry = entries.entry(parent.into()).or_insert_with(|| Member {
                name: parent
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                directory: true,
                symlink: false,
                encrypted: false,
                size: 0,
                modified: 0.,
            });
            ensure!(
                entry.directory && !entry.symlink,
                "Conflicting archive entries: {}",
                parent.display()
            );
        }
        Ok(())
    })?;
    Ok(entries)
}

pub(crate) fn valid_component(name: &str) -> bool {
    if name.is_empty() || matches!(name, "." | "..") || name.contains(['\\', ':', '\0']) {
        return false;
    }
    #[cfg(windows)]
    {
        let stem = name
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        if name.ends_with([' ', '.'])
            || name.contains(['<', '>', '"', '|', '?', '*'])
            || name.chars().any(char::is_control)
            || matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            )
            || ["COM", "LPT"].iter().any(|prefix| {
                stem.strip_prefix(prefix).is_some_and(|n| {
                    matches!(
                        n,
                        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                    )
                })
            })
        {
            return false;
        }
    }
    true
}

pub(crate) fn ensure_directory(base: &Path, relative: &Path) -> Result<()> {
    let mut path = base.to_path_buf();
    for component in relative.components() {
        path.push(component);
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                ensure!(
                    path.symlink_metadata()?.file_type().is_dir(),
                    "Extraction destination is not a regular directory: {}",
                    path.display()
                );
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(crate) enum WriteMode {
    New,
    Replace,
    Append,
}

pub(crate) fn extraction_target(
    source: &Path,
    target: PathBuf,
    modified_ms: f64,
    policy: &mut ConflictPolicy,
    cancel: &CancelToken,
    conflict: &dyn Fn(&Path, &Path) -> ConflictResolution,
) -> Result<Option<(PathBuf, WriteMode)>> {
    let metadata = match target.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Some((target, WriteMode::New)));
        }
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_file(),
        "Extraction destination is not a regular file: {}",
        target.display()
    );
    let resolution = match policy {
        ConflictPolicy::Ask => conflict(source, &target),
        ConflictPolicy::Overwrite => ConflictResolution::Overwrite,
        ConflictPolicy::Skip => ConflictResolution::Skip,
        ConflictPolicy::Rename => {
            let stem = target.file_stem().unwrap_or_default().to_string_lossy();
            let ext = target
                .extension()
                .map(|ext| format!(".{}", ext.to_string_lossy()))
                .unwrap_or_default();
            let name = (1..)
                .map(|n| format!("{stem} ({n}){ext}"))
                .find(|name| target.with_file_name(name).symlink_metadata().is_err())
                .context("No available extraction filename")?;
            ConflictResolution::Rename(name)
        }
        ConflictPolicy::Append => return Ok(Some((target, WriteMode::Append))),
        ConflictPolicy::Resume => bail!("Resume is available for local copies only"),
        ConflictPolicy::OnlyNewer => {
            let modified = metadata
                .modified()?
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default();
            if modified_ms > modified.as_millis() as f64 {
                ConflictResolution::Overwrite
            } else {
                ConflictResolution::Skip
            }
        }
    };
    Ok(match resolution {
        ConflictResolution::Overwrite => Some((target, WriteMode::Replace)),
        ConflictResolution::OverwriteAll => {
            *policy = ConflictPolicy::Overwrite;
            Some((target, WriteMode::Replace))
        }
        ConflictResolution::Skip => None,
        ConflictResolution::SkipAll => {
            *policy = ConflictPolicy::Skip;
            None
        }
        ConflictResolution::Cancel => {
            cancel.cancel();
            None
        }
        ConflictResolution::Rename(name) => {
            if !valid_component(&name) || name.contains('/') {
                bail!("Invalid extraction filename");
            }
            Some((target.with_file_name(name), WriteMode::New))
        }
    })
}

#[cfg(test)]
mod format_tests {
    use super::*;
    use dotdir_archive_api::Entry;

    #[derive(Debug)]
    struct FixtureFormat {
        info: &'static FormatInfo,
        member: &'static str,
    }

    impl ArchiveFormat for FixtureFormat {
        fn info(&self) -> &'static FormatInfo {
            self.info
        }

        fn open(
            &self,
            source: VirtualFile,
            _access: &dyn dotdir_archive_api::PasswordAccess,
        ) -> Result<Box<dyn ArchiveReader>> {
            Ok(Box::new(FixtureReader {
                source,
                member: self.member,
            }))
        }
    }

    #[derive(Debug)]
    struct FixtureReader {
        source: VirtualFile,
        member: &'static str,
    }

    impl ArchiveReader for FixtureReader {
        fn visit_entries(&self, visit: &mut dyn FnMut(Entry) -> Result<()>) -> Result<()> {
            visit(Entry {
                path: self.member.into(),
                directory: false,
                symlink: false,
                encrypted: false,
                size: self.source.len(),
                modified: 0.,
            })
        }

        fn with_member(
            &self,
            path: &Path,
            _passwords: &dyn dotdir_archive_api::PasswordAccess,
            read: &mut dyn FnMut(&mut dyn Read) -> Result<()>,
        ) -> Result<()> {
            ensure!(path == Path::new(self.member), "Unknown fixture entry");
            read(&mut self.source.clone())
        }
    }

    static FIXTURES: [FormatInfo; 2] = [
        FormatInfo {
            id: "fixture.outer",
            name: "Outer fixture",
            version: "1",
            suffixes: &["bundle"],
            writable: false,
        },
        FormatInfo {
            id: "fixture.inner",
            name: "Inner fixture",
            version: "1",
            suffixes: &["data.bundle"],
            writable: false,
        },
    ];

    #[test]
    fn registered_formats_can_nest_and_restore_without_zip() {
        static FORMATS: ArchiveFormats = ArchiveFormats::new(&[
            &FixtureFormat {
                info: &FIXTURES[0],
                member: "inner.DATA.BUNDLE",
            },
            &FixtureFormat {
                info: &FIXTURES[1],
                member: "src/code.ts",
            },
        ]);
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("outer.bundle");
        fs::write(&path, b"source").unwrap();
        let format = FORMATS.for_path(&path).unwrap();
        assert_eq!(format.info().id, "fixture.outer");
        let outer = Archive::mount_location(path.into(), format).unwrap();
        let filesystems = Filesystems::new(Default::default());
        let Location::Archive { archive, member } = &outer else {
            panic!("expected an archive location");
        };
        let entries = archive.list(member, &filesystems).unwrap();
        let nested = &entries[0].0;
        let format = FORMATS.for_path(nested.name_path()).unwrap();
        assert_eq!(format.info().id, "fixture.inner");
        let inner = Archive::mount_location(nested.clone(), format).unwrap();
        let saved = serde_json::to_vec(&SavedLocation::from(&inner)).unwrap();
        let saved: SavedLocation = serde_json::from_slice(&saved).unwrap();
        assert!(saved.clone().restore(&ArchiveFormats::new(&[])).is_none());
        let restored = saved.restore(&FORMATS).unwrap();
        assert_eq!(restored, inner);
        assert_eq!(restored.parent(), Some(outer));
        let code = restored.resolve_archive_member("src/code.ts").unwrap();
        assert_eq!(code.read_preview(&filesystems).unwrap(), b"source");
        assert!(!code.editable());
        assert!(
            code.save_archive_member(b"source", "edit", &filesystems)
                .is_err()
        );
    }
}

#[cfg(all(test, feature = "zip-extension"))]
mod tests {
    use super::*;
    use crate::archive_extensions::BUNDLED;
    use dotdir_zip::ZIP;
    use std::io::Cursor;
    use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

    fn mount_zip(path: PathBuf) -> Location {
        Archive::mount_location(path.into(), &ZIP).unwrap()
    }

    fn fixture(path: &Path) {
        let mut zip = ZipWriter::new(File::create(path).unwrap());
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zip.add_directory("empty/", options).unwrap();
        for (name, bytes) in [
            ("docs/hello.txt", b"hello".as_slice()),
            ("sibling.txt", b"world".as_slice()),
            ("../outside.txt", b"outside".as_slice()),
        ] {
            zip.start_file(name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn zip_save_rebuilds_the_nested_member_and_preserves_siblings() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.zip");
        let mut child = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        child.start_file("main.ts", options).unwrap();
        child.write_all(b"const count = 1;").unwrap();
        child.start_file("lib.ts", options).unwrap();
        child.write_all(b"export {};").unwrap();
        let child = child.finish().unwrap().into_inner();
        let mut outer = ZipWriter::new(File::create(&path).unwrap());
        outer.set_comment("archive comment").unwrap();
        outer.start_file("inner.zip", options).unwrap();
        outer.write_all(&child).unwrap();
        outer.start_file("sibling.txt", options).unwrap();
        outer.write_all(b"unchanged").unwrap();
        outer.finish().unwrap();

        let fs = Filesystems::new(Default::default());
        let root = mount_zip(path.clone());
        let inner =
            Archive::mount_location(root.resolve_archive_member("inner.zip").unwrap(), &ZIP)
                .unwrap();
        let file = inner.resolve_archive_member("main.ts").unwrap();
        assert_eq!(file.read_preview(&fs).unwrap(), b"const count = 1;");
        file.save_archive_member(b"const count = 1;", "const count = 2;", &fs)
            .unwrap();
        assert_eq!(file.read_preview(&fs).unwrap(), b"const count = 2;");
        assert_eq!(
            inner
                .resolve_archive_member("lib.ts")
                .unwrap()
                .read_preview(&fs)
                .unwrap(),
            b"export {};"
        );
        assert_eq!(
            root.resolve_archive_member("sibling.txt")
                .unwrap()
                .read_preview(&fs)
                .unwrap(),
            b"unchanged"
        );
        assert_eq!(
            ZipArchive::new(File::open(&path).unwrap())
                .unwrap()
                .comment(),
            b"archive comment"
        );
        let saved = std::fs::read(&path).unwrap();
        assert!(
            file.save_archive_member(b"const count = 1;", "stale edit", &fs)
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), saved);
    }

    #[test]
    fn stored_and_compressed_members_open_as_bounded_seekable_files() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("media.zip");
        let mut zip = ZipWriter::new(File::create(&path).unwrap());
        for (name, method) in [
            ("stored.mp4", CompressionMethod::Stored),
            ("compressed.mp4", CompressionMethod::Deflated),
        ] {
            zip.start_file(
                name,
                SimpleFileOptions::default().compression_method(method),
            )
            .unwrap();
            zip.write_all(b"0123456789").unwrap();
        }
        zip.finish().unwrap();
        let Location::Archive { archive, .. } = mount_zip(path) else {
            panic!()
        };
        let fs = Filesystems::new(Default::default());
        let cancel = CancelToken::new();
        let stored = archive
            .open_member(Path::new("stored.mp4"), &fs, &cancel, 4)
            .unwrap();
        assert_eq!(stored.memory_usage(), 0);
        let compressed = archive
            .open_member(Path::new("compressed.mp4"), &fs, &cancel, 10)
            .unwrap();
        assert_eq!(compressed.memory_usage(), 10);
        for mut reader in [stored, compressed] {
            assert_eq!(reader.len(), 10);
            reader.seek(SeekFrom::End(-3)).unwrap();
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"789");
            reader.seek(SeekFrom::Start(0)).unwrap();
            let mut prefix = [0; 3];
            reader.read_exact(&mut prefix).unwrap();
            assert_eq!(&prefix, b"012");
        }
        assert!(
            archive
                .open_member(Path::new("compressed.mp4"), &fs, &cancel, 4)
                .is_err()
        );
    }

    #[test]
    fn nested_zip_locations_restore_and_return_to_their_parent_directory() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("outer!.zip");
        let mut child = ZipWriter::new(Cursor::new(Vec::new()));
        child
            .start_file("images/photo.png", SimpleFileOptions::default())
            .unwrap();
        child.write_all(b"picture").unwrap();
        let child = child.finish().unwrap().into_inner();
        let mut zip = ZipWriter::new(File::create(&path).unwrap());
        for (name, method) in [
            ("folder/inner-stored.zip", CompressionMethod::Stored),
            ("folder/inner-compressed.zip", CompressionMethod::Deflated),
        ] {
            zip.start_file(
                name,
                SimpleFileOptions::default().compression_method(method),
            )
            .unwrap();
            zip.write_all(&child).unwrap();
        }
        zip.finish().unwrap();
        let path = path.canonicalize().unwrap();
        let Location::Archive { archive, .. } = mount_zip(path.clone()) else {
            panic!()
        };
        let fs = Filesystems::new(Default::default());
        let folder = archive.location("folder".into());
        let legacy: SavedLocation = serde_json::from_value(serde_json::json!({
            "source": { "archive": path, "member": "folder/inner-stored.zip" },
            "member": "images"
        }))
        .unwrap();
        let legacy = legacy.restore(&BUNDLED).unwrap();
        assert_eq!(
            legacy
                .resolve_archive_member("images/photo.png")
                .unwrap()
                .read_preview(&fs)
                .unwrap(),
            b"picture"
        );
        for (entry, _) in archive.list(Path::new("folder"), &fs).unwrap() {
            let inner = Archive::mount_location(entry, &ZIP).unwrap();
            assert_eq!(inner.parent(), Some(folder.clone()));
            let expected = SavedLocation::from(&inner);
            let saved: SavedLocation =
                serde_json::from_str(&serde_json::to_string(&expected).unwrap()).unwrap();
            let restored = saved.restore(&BUNDLED).unwrap();
            assert_eq!(restored, inner);
            let Location::Archive { archive, .. } = restored else {
                panic!()
            };
            let files = archive.list(Path::new("images"), &fs).unwrap();
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].0.read_preview(&fs).unwrap(), b"picture");
        }
    }

    #[test]
    fn archive_nesting_has_a_depth_limit() {
        let mut location = mount_zip("outer.zip".into());
        for _ in 1..MAX_ARCHIVE_DEPTH {
            let Location::Archive { archive, .. } = location else {
                panic!()
            };
            location =
                Archive::mount_location(archive.location("nested.zip".into()), &ZIP).unwrap();
        }
        assert!(Archive::mount_location(location, &ZIP).is_err());
    }

    #[test]
    fn zip_lists_implicit_folders_reads_members_and_round_trips_locations() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.ZIP");
        fixture(&path);
        let root = mount_zip(path);
        assert_eq!(
            root.parent(),
            Some(temp.path().canonicalize().unwrap().into())
        );
        let Location::Archive { archive, member } = &root else {
            panic!()
        };
        let fs = Filesystems::new(Default::default());
        let children = archive.list(member, &fs).unwrap();
        assert_eq!(
            children
                .iter()
                .map(|(_, entry)| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["docs", "empty", "sibling.txt"]
        );
        assert!(children[0].1.directory);
        let nested = archive.list(Path::new("docs"), &fs).unwrap();
        let (file, _) = &nested[0];
        assert_eq!(
            file.read_preview(&Filesystems::new(Default::default()))
                .unwrap(),
            b"hello"
        );
        assert_eq!(file.parent(), Some(children[0].0.clone()));
        let restored: SavedLocation =
            serde_json::from_slice(&serde_json::to_vec(&SavedLocation::from(file)).unwrap())
                .unwrap();
        assert_eq!(restored.restore(&BUNDLED).unwrap(), *file);
        let previous: SavedLocation = serde_json::from_str(r#""/tmp""#).unwrap();
        assert_eq!(
            previous.restore(&BUNDLED).unwrap().local(),
            Some(Path::new("/tmp"))
        );
    }

    #[test]
    fn zip_extraction_streams_selected_trees_and_respects_conflicts() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("example.zip");
        let dest = temp.path().join("dest");
        fs::create_dir(&dest).unwrap();
        fixture(&path);
        let Location::Archive { archive, .. } = mount_zip(path.clone()) else {
            panic!()
        };
        let extract = |policy, conflict: &dyn Fn(&Path, &Path) -> ConflictResolution| {
            archive
                .extract(
                    vec!["docs".into(), "empty".into()],
                    &dest,
                    false,
                    policy,
                    &Filesystems::new(Default::default()),
                    &CancelToken::new(),
                    &|_| {},
                    conflict,
                )
                .unwrap();
        };
        let original = fs::read(&path).unwrap();
        extract(ConflictPolicy::Ask, &|_, _| panic!("no conflict expected"));
        assert_eq!(fs::read(dest.join("docs/hello.txt")).unwrap(), b"hello");
        assert!(dest.join("empty").is_dir());
        assert!(!dest.join("sibling.txt").exists());
        assert!(!temp.path().join("outside.txt").exists());
        fs::write(dest.join("docs/hello.txt"), b"keep").unwrap();
        extract(ConflictPolicy::Ask, &|_, _| ConflictResolution::SkipAll);
        assert_eq!(fs::read(dest.join("docs/hello.txt")).unwrap(), b"keep");
        extract(ConflictPolicy::Ask, &|_, _| {
            ConflictResolution::OverwriteAll
        });
        assert_eq!(fs::read(dest.join("docs/hello.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn zip_extraction_rejects_a_symlink_directory_in_the_destination() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("example.zip");
        let dest = temp.path().join("dest");
        let outside = temp.path().join("outside");
        fs::create_dir(&dest).unwrap();
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, dest.join("docs")).unwrap();
        fixture(&path);
        let Location::Archive { archive, .. } = mount_zip(path) else {
            panic!()
        };
        assert!(
            archive
                .extract(
                    vec!["docs".into()],
                    &dest,
                    false,
                    ConflictPolicy::Overwrite,
                    &Filesystems::new(Default::default()),
                    &CancelToken::new(),
                    &|_| {},
                    &|_, _| unreachable!()
                )
                .is_err()
        );
        assert!(fs::read_dir(outside).unwrap().next().is_none());
    }
}

#[cfg(all(
    test,
    feature = "sevenz-extension",
    feature = "tar-extension",
    feature = "zip-extension"
))]
mod mixed_format_tests {
    use super::*;
    use crate::{archive_extensions::BUNDLED, archive_passwords::ArchivePasswords};
    use dotdir_archive_api::Password;
    use sevenz_rust2::{
        ArchiveEntry, ArchiveWriter, EncoderMethod, encoder_options::AesEncoderOptions,
    };
    use std::{
        io::Cursor,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use zip::{ZipWriter, write::SimpleFileOptions};

    fn mount(source: Location) -> Location {
        let format = BUNDLED.for_path(source.name_path()).unwrap();
        Archive::mount_location(source, format).unwrap()
    }

    fn member(root: &Location, name: &str) -> Location {
        root.resolve_archive_member(name).unwrap()
    }

    fn tar_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut archive = tar::Builder::new(Vec::new());
        for (path, bytes) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            archive.append_data(&mut header, path, *bytes).unwrap();
        }
        archive.into_inner().unwrap()
    }

    #[test]
    fn mixed_archives_restore_and_preview_through_virtual_files() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("outer.zip");
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(&tar_bytes(&[
            ("./media/picture.png", b"image fixture"),
            ("./media/clip.mp4", b"video fixture"),
        ]))
        .unwrap();
        let gzip = gzip.finish().unwrap();

        let mut sevenz = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
        sevenz
            .push_archive_entry(
                ArchiveEntry::new_file("files.TAR.GZ"),
                Some(gzip.as_slice()),
            )
            .unwrap();
        let sevenz = sevenz.finish().unwrap().into_inner();
        let mut zip = ZipWriter::new(File::create(&path).unwrap());
        zip.start_file("nested.7z", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(&sevenz).unwrap();
        zip.finish().unwrap();

        let filesystems = Filesystems::new(Default::default());
        let outer = mount(path.into());
        let archive_7z = mount(member(&outer, "nested.7z"));
        let archive_tar = mount(member(&archive_7z, "files.TAR.GZ"));
        assert_eq!(archive_tar.parent(), Some(archive_7z));
        let media = member(&archive_tar, "media");
        let serialized = serde_json::to_vec(&SavedLocation::from(&media)).unwrap();
        let saved: SavedLocation = serde_json::from_slice(&serialized).unwrap();
        let restored = saved.restore(&BUNDLED).unwrap();
        assert_eq!(restored, media);
        let image = member(&restored, "media/picture.png");
        assert_eq!(image.read_preview(&filesystems).unwrap(), b"image fixture");
        assert!(!image.editable());
        let mut video = member(&restored, "media/clip.mp4")
            .open_seekable(&filesystems, &CancelToken::new())
            .unwrap();
        video.seek(SeekFrom::End(-7)).unwrap();
        let mut tail = Vec::new();
        video.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, b"fixture");

        let Location::Archive { archive, .. } = restored else {
            unreachable!()
        };
        archive
            .extract(
                vec!["media".into()],
                temp.path(),
                false,
                ConflictPolicy::Ask,
                &filesystems,
                &CancelToken::new(),
                &|_| {},
                &|_, _| unreachable!(),
            )
            .unwrap();
        assert_eq!(
            fs::read(temp.path().join("media/picture.png")).unwrap(),
            b"image fixture"
        );
    }

    #[test]
    fn plain_tar_and_stored_7z_supply_direct_seek_ranges() {
        let temp = tempfile::tempdir().unwrap();
        let mut sevenz = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
        sevenz.set_content_methods(vec![EncoderMethod::COPY.into()]);
        sevenz
            .push_archive_entry(
                ArchiveEntry::new_file("clip.mp4"),
                Some(b"0123456789".as_slice()),
            )
            .unwrap();
        let sevenz = sevenz.finish().unwrap().into_inner();
        let path = temp.path().join("bundle.tar");
        fs::write(&path, tar_bytes(&[("video.7z", &sevenz)])).unwrap();

        let filesystems = Filesystems::new(Default::default());
        let root = mount(path.into());
        let embedded = member(&root, "video.7z");
        let zip = mount(embedded.clone());
        let cancel = CancelToken::new();
        let direct = embedded.open_seekable(&filesystems, &cancel).unwrap();
        assert_eq!(direct.memory_usage(), 0);
        let Location::Archive { archive, .. } = zip else {
            unreachable!()
        };
        let mut clip = archive
            .open_member(Path::new("clip.mp4"), &filesystems, &cancel, 2)
            .unwrap();
        assert_eq!(clip.memory_usage(), 0);
        clip.seek(SeekFrom::Start(5)).unwrap();
        let mut bytes = Vec::new();
        clip.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"56789");
    }

    #[test]
    fn encrypted_7z_uses_the_shared_password_prompt_and_cache() {
        let temp = tempfile::tempdir().unwrap();
        for encrypted_header in [false, true] {
            let path = temp.path().join(format!("{encrypted_header}.7z"));
            let mut writer = ArchiveWriter::new(File::create(&path).unwrap()).unwrap();
            writer.set_content_methods(vec![
                AesEncoderOptions::new("test-archive-key".into()).into(),
                EncoderMethod::LZMA2.into(),
            ]);
            writer.set_encrypt_header(encrypted_header);
            writer
                .push_archive_entry(
                    ArchiveEntry::new_file("readme.txt"),
                    Some(b"protected".as_slice()),
                )
                .unwrap();
            writer.finish().unwrap();

            let prompts = Arc::new(AtomicUsize::new(0));
            let calls = prompts.clone();
            let mut filesystems = Filesystems::new(Default::default());
            filesystems.archive_passwords = Arc::new(ArchivePasswords::new(15, move |request| {
                let count = calls.fetch_add(1, Ordering::Relaxed);
                assert!(count < 2);
                assert_eq!(request.incorrect, count == 1);
                assert_eq!(request.member.as_os_str().is_empty(), encrypted_header);
                let bytes = if count == 0 {
                    b"wrong".as_slice()
                } else {
                    b"test-archive-key"
                };
                request.reply.send(Password::new(bytes.to_vec())).unwrap();
            }));
            let root = mount(path.into());
            let file = member(&root, "readme.txt");
            assert_eq!(file.read_preview(&filesystems).unwrap(), b"protected");
            assert_eq!(file.read_preview(&filesystems).unwrap(), b"protected");
            assert_eq!(prompts.load(Ordering::Relaxed), 2);
            filesystems.archive_passwords.set_minutes(0);
            assert!(
                file.read_preview(&filesystems.without_password_prompts())
                    .unwrap_err()
                    .is::<UnlockRequired>()
            );
        }
    }
}
