use crate::filesystems::{Filesystems, ProviderPath};
use anyhow::{Context, Result, bail, ensure};
use dotdir_core::copy::{CancelToken, ConflictPolicy, ConflictResolution, CopyEvent, CopyProgress};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ffi::OsStr,
    fs::{self, File},
    hash::{Hash, Hasher},
    io::{self, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::{Instant, SystemTime},
};
use zip::{ZipArchive, ZipWriter};

pub const PREVIEW_LIMIT: u64 = 16 * 1024 * 1024;
pub const SEEKABLE_LIMIT: u64 = 64 * 1024 * 1024;
const MAX_ARCHIVE_DEPTH: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Location {
    Local(PathBuf),
    Zip {
        archive: Arc<Archive>,
        member: PathBuf,
    },
    Provider(ProviderPath),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SavedLocation {
    Local(PathBuf),
    Zip {
        archive: PathBuf,
        member: PathBuf,
    },
    Provider {
        uri: ProviderPath,
    },
    NestedZip {
        source: Box<SavedLocation>,
        member: PathBuf,
    },
}

impl SavedLocation {
    pub fn available(&self) -> bool {
        let mut depth = 0;
        let mut source = self;
        loop {
            match source {
                Self::Local(path) => return path.is_dir(),
                Self::Zip { archive, .. } => return depth < MAX_ARCHIVE_DEPTH && archive.is_file(),
                Self::Provider { .. } => return true,
                Self::NestedZip { source: parent, .. } => {
                    depth += 1;
                    if depth > MAX_ARCHIVE_DEPTH {
                        return false;
                    }
                    source = parent;
                }
            }
        }
    }
}

impl From<SavedLocation> for Location {
    fn from(saved: SavedLocation) -> Self {
        match saved {
            SavedLocation::Local(path) => Self::Local(path),
            SavedLocation::Zip { archive, member } => Self::Zip {
                archive: Arc::new(Archive::new(archive.into())),
                member,
            },
            SavedLocation::Provider { uri } => Self::Provider(uri),
            SavedLocation::NestedZip { source, member } => Self::Zip {
                archive: Arc::new(Archive::new(Self::from(*source))),
                member,
            },
        }
    }
}

impl From<&Location> for SavedLocation {
    fn from(location: &Location) -> Self {
        match location {
            Location::Local(path) => Self::Local(path.clone()),
            Location::Zip { archive, member } => match &archive.source {
                Location::Local(path) => Self::Zip {
                    archive: path.clone(),
                    member: member.clone(),
                },
                source => Self::NestedZip {
                    source: Box::new(Self::from(source)),
                    member: member.clone(),
                },
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

impl Location {
    pub fn editable(&self) -> bool {
        self.local().is_some() || (self.archive_root().is_some() && self.host_path().is_some())
    }

    pub fn archive_root(&self) -> Option<Self> {
        match self {
            Self::Zip { archive, .. } => Some(archive.location(PathBuf::new())),
            _ => None,
        }
    }

    pub fn archive_member(&self) -> Option<&Path> {
        match self {
            Self::Zip { member, .. } => Some(member),
            _ => None,
        }
    }

    pub fn resolve_archive_member(&self, path: &str) -> Option<Self> {
        let Self::Zip { archive, .. } = self else {
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
        let Self::Zip { archive, member } = self else {
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
            Self::Zip { archive, .. } => archive.source.host_path(),
            Self::Provider(_) => None,
        }
    }

    pub fn name_path(&self) -> &Path {
        match self {
            Self::Zip { archive, member } => {
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
            Self::Zip { archive, member } if !member.as_os_str().is_empty() => {
                Some(archive.location(member.parent()?.into()))
            }
            Self::Zip { archive, .. } => archive.source.parent(),
            Self::Provider(path) => Some(Self::Provider(path.parent()?)),
            Self::Local(path) => Some(path.parent()?.into()),
        }
    }

    pub fn display(&self) -> String {
        match self {
            Self::Local(path) => path.display().to_string(),
            Self::Zip { archive, member } => {
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
            Self::Zip { archive, member } => {
                archive.read_member(member, filesystems, limit, cancel)
            }
            Self::Provider(path) => filesystems.read_file(path, limit as usize),
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
            Self::Zip { archive, member } => {
                archive.open_member(member, filesystems, cancel, SEEKABLE_LIMIT)
            }
            Self::Provider(path) => Ok(VirtualFile::memory(
                filesystems.read_file(path, SEEKABLE_LIMIT as usize)?,
            )),
        }
    }

    fn archive_depth(&self) -> usize {
        match self {
            Self::Zip { archive, .. } => archive.depth,
            _ => 0,
        }
    }

    pub fn save_archive_member(
        &self,
        expected: &[u8],
        text: &str,
        filesystems: &Filesystems,
    ) -> Result<()> {
        static SAVE_LOCK: Mutex<()> = Mutex::new(());
        let _guard = SAVE_LOCK.lock().unwrap();
        let outer = self
            .host_path()
            .context("The archive has no writable local source")?;
        let original = fs::metadata(outer)?;
        ensure!(
            !original.permissions().readonly(),
            "The ZIP archive is read-only"
        );
        let identity = same_file::Handle::from_path(outer)?;
        ensure!(
            text.len() as u64 <= PREVIEW_LIMIT,
            "ZIP text entries are limited to 16 MiB"
        );
        ensure!(
            self.read_preview(filesystems)? == expected,
            "ZIP entry changed on disk. Your edits are still open."
        );

        let mut location = self;
        let mut compressed_archive = None;
        let cancel = CancelToken::new();
        while let Self::Zip { archive, member } = location {
            let mut reader = archive.mounted(filesystems, &cancel)?.reader.clone();
            let contents = compressed_archive.as_deref().unwrap_or(text.as_bytes());
            if archive.source.local().is_some() {
                let parent = outer.parent().context("Missing archive directory")?;
                let mut output = tempfile::NamedTempFile::new_in(parent)?;
                rewrite_member(&mut reader, output.as_file_mut(), member, contents)?;
                output.as_file().set_permissions(original.permissions())?;
                output.as_file().sync_all()?;
                let current = fs::metadata(outer)?;
                ensure!(
                    current.len() == original.len()
                        && current.modified()? == original.modified()?
                        && same_file::Handle::from_path(outer)? == identity,
                    "ZIP archive changed while saving. Your edits are still open."
                );
                output.persist(outer)?;
                return Ok(());
            }
            let output = rewrite_member(&mut reader, NestedZipOutput::default(), member, contents)?;
            compressed_archive = Some(output.0.into_inner());
            location = &archive.source;
        }
        bail!("Expected a ZIP file entry")
    }
}

#[derive(Default)]
struct NestedZipOutput(io::Cursor<Vec<u8>>);

impl Write for NestedZipOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if (bytes.len() as u64).saturating_add(self.0.position()) > SEEKABLE_LIMIT {
            return Err(io::Error::other(
                "The updated nested ZIP exceeds the 64 MiB memory limit",
            ));
        }
        self.0.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for NestedZipOutput {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.0.seek(from)
    }
}

fn rewrite_member<W: Write + Seek>(
    reader: &mut Reader,
    output: W,
    member: &Path,
    contents: &[u8],
) -> Result<W> {
    ensure!(
        reader.offset() == 0 && !reader.has_duplicate_names(),
        "ZIPs with a prepended payload or duplicate entry names cannot be edited"
    );
    let name = member_name(member)?;
    let replaced = reader
        .index_for_name(&name)
        .context("The ZIP entry is missing")?;
    let mut writer = ZipWriter::new(output);
    writer.set_raw_comment(reader.comment().into())?;
    for index in 0..reader.len() {
        let entry = reader.by_index_raw(index)?;
        if index == replaced {
            ensure!(
                entry.is_file() && !entry.is_symlink() && !entry.encrypted(),
                "Only regular, unencrypted ZIP files can be edited"
            );
            let options = entry.options();
            writer.start_file(name.as_ref(), options)?;
            writer.write_all(contents)?;
        } else {
            writer.raw_copy_file(entry)?;
        }
    }
    Ok(writer.finish()?)
}

#[derive(Debug, Clone)]
enum FileBacking {
    Local(Arc<Mutex<BufReader<File>>>),
    Memory(Arc<Vec<u8>>),
}

/// An independent cursor over an entire file or a bounded subrange of it.
#[derive(Clone)]
pub struct VirtualFile {
    backing: FileBacking,
    start: u64,
    length: u64,
    position: u64,
}

impl std::fmt::Debug for VirtualFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VirtualFile")
            .field("start", &self.start)
            .field("length", &self.length)
            .finish_non_exhaustive()
    }
}

impl VirtualFile {
    fn local(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        ensure!(metadata.is_file(), "Expected a regular file");
        Ok(Self {
            backing: FileBacking::Local(Arc::new(Mutex::new(BufReader::new(file)))),
            start: 0,
            length: metadata.len(),
            position: 0,
        })
    }

    fn memory(data: Vec<u8>) -> Self {
        Self {
            length: data.len() as u64,
            backing: FileBacking::Memory(Arc::new(data)),
            start: 0,
            position: 0,
        }
    }

    fn slice(self, start: u64, length: u64) -> Result<Self> {
        ensure!(
            start <= self.length && length <= self.length - start,
            "ZIP entry extends beyond the archive"
        );
        Ok(Self {
            backing: self.backing,
            start: self.start + start,
            length,
            position: 0,
        })
    }

    pub fn len(&self) -> u64 {
        self.length
    }

    fn memory_usage(&self) -> u64 {
        match &self.backing {
            FileBacking::Local(_) => 0,
            FileBacking::Memory(data) => data.len() as u64,
        }
    }
}

impl Read for VirtualFile {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = self
            .length
            .saturating_sub(self.position)
            .min(bytes.len() as u64) as usize;
        if count == 0 {
            return Ok(0);
        }
        let offset = self.start + self.position;
        let output = &mut bytes[..count];
        let read = match &self.backing {
            FileBacking::Local(file) => {
                let mut file = file.lock().unwrap();
                if file.stream_position()? != offset {
                    file.seek(SeekFrom::Start(offset))?;
                }
                file.read(output)?
            }
            FileBacking::Memory(data) => {
                let offset = offset as usize;
                output.copy_from_slice(&data[offset..offset + count]);
                count
            }
        };
        self.position += read as u64;
        Ok(read)
    }
}

impl Seek for VirtualFile {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::Current(offset) => self.position.checked_add_signed(offset),
            SeekFrom::End(offset) => self.length.checked_add_signed(offset),
        };
        self.position = position
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid file offset"))?;
        Ok(self.position)
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

fn member_name(path: &Path) -> Result<std::borrow::Cow<'_, str>> {
    let name = path.to_str().context("Invalid ZIP path")?;
    if cfg!(windows) {
        Ok(name.replace(std::path::MAIN_SEPARATOR, "/").into())
    } else {
        Ok(name.into())
    }
}

#[derive(Debug)]
pub struct Archive {
    source: Location,
    depth: usize,
    cache: Mutex<Option<CachedArchive>>,
}

#[derive(Debug)]
struct CachedArchive {
    revision: ArchiveRevision,
    mounted: Weak<MountedArchive>,
}

#[derive(Debug, PartialEq, Eq)]
struct ArchiveRevision {
    length: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64),
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
struct MountedArchive {
    reader: Reader,
    index: Mutex<Option<Arc<Index>>>,
    memory_usage: u64,
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
    pub size: u64,
    pub modified: f64,
}

type Reader = ZipArchive<VirtualFile>;
type Index = BTreeMap<PathBuf, Member>;

impl PartialEq for Archive {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
    }
}
impl Eq for Archive {}
impl Hash for Archive {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.source.hash(state);
    }
}

pub fn is_zip(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
}

impl Archive {
    fn new(source: Location) -> Self {
        Self {
            depth: source.archive_depth() + 1,
            source,
            cache: Mutex::new(None),
        }
    }

    pub fn mount(path: PathBuf) -> Location {
        Arc::new(Self::new(path.into())).location(PathBuf::new())
    }

    pub fn mount_location(source: Location) -> Result<Location> {
        ensure!(
            source.archive_depth() < MAX_ARCHIVE_DEPTH,
            "ZIP nesting is limited to {MAX_ARCHIVE_DEPTH} archives"
        );
        let source = match source {
            Location::Local(path) => Location::Local(path.canonicalize().unwrap_or(path)),
            source => source,
        };
        Ok(Arc::new(Self::new(source)).location(PathBuf::new()))
    }

    fn location(self: &Arc<Self>, member: PathBuf) -> Location {
        Location::Zip {
            archive: self.clone(),
            member,
        }
    }

    fn mounted(
        &self,
        filesystems: &Filesystems,
        cancel: &CancelToken,
    ) -> Result<Arc<MountedArchive>> {
        ensure!(
            self.depth <= MAX_ARCHIVE_DEPTH,
            "ZIP nesting is limited to {MAX_ARCHIVE_DEPTH} archives"
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
        let reader = ZipArchive::new(source)
            .with_context(|| format!("Open ZIP archive {}", self.source.display()))?;
        let mounted = Arc::new(MountedArchive {
            reader,
            index: Mutex::new(None),
            memory_usage,
        });
        if let Some(revision) = revision {
            *cache = Some(CachedArchive {
                revision,
                mounted: Arc::downgrade(&mounted),
            });
            retain_mount(&mounted);
        }
        Ok(mounted)
    }

    fn index(&self, filesystems: &Filesystems) -> Result<Arc<Index>> {
        let mounted = self.mounted(filesystems, &CancelToken::new())?;
        let mut index = mounted.index.lock().unwrap();
        if let Some(entries) = &*index {
            return Ok(entries.clone());
        }
        let entries = Arc::new(build_index(&mut mounted.reader.clone())?);
        *index = Some(entries.clone());
        Ok(entries)
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
                "ZIP directory is missing"
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
        let mut zip = self.mounted(filesystems, cancel)?.reader.clone();
        let entry = zip.by_name(member_name(path)?.as_ref())?;
        ensure!(
            entry.is_file() && !entry.is_symlink(),
            "ZIP entry is not a regular file"
        );
        let size = entry.size();
        ensure!(
            size <= limit,
            "ZIP previews are limited to {} MiB",
            limit / (1024 * 1024)
        );
        read_exact_bytes(entry, size, cancel)
    }

    fn open_member(
        &self,
        path: &Path,
        filesystems: &Filesystems,
        cancel: &CancelToken,
        limit: u64,
    ) -> Result<VirtualFile> {
        let mut zip = self.mounted(filesystems, cancel)?.reader.clone();
        let entry = zip.by_name(member_name(path)?.as_ref())?;
        ensure!(
            entry.is_file() && !entry.is_symlink() && !entry.encrypted(),
            "ZIP entry is not a readable regular file"
        );
        let size = entry.size();
        if entry.compression() == zip::CompressionMethod::Stored {
            ensure!(
                entry.compressed_size() == size,
                "Stored ZIP entry has inconsistent sizes"
            );
            let start = entry
                .data_start()
                .context("Missing ZIP entry data offset")?;
            drop(entry);
            return zip.into_inner().slice(start, size);
        }
        ensure!(
            size <= limit,
            "Compressed ZIP entries over {} MiB cannot be opened in memory",
            limit / (1024 * 1024)
        );
        Ok(VirtualFile::memory(read_exact_bytes(entry, size, cancel)?))
    }

    pub fn extract(
        &self,
        members: Vec<PathBuf>,
        destination: &Path,
        mut policy: ConflictPolicy,
        filesystems: &Filesystems,
        cancel: &CancelToken,
        emit: &dyn Fn(CopyEvent),
        conflict: &dyn Fn(&Path, &Path) -> ConflictResolution,
    ) -> Result<()> {
        let mut zip = self.mounted(filesystems, cancel)?.reader.clone();
        let entries = build_index(&mut zip)?;
        let base = destination.canonicalize()?;
        let archive_path = self
            .source
            .host_path()
            .map(Path::canonicalize)
            .transpose()?;
        let mut jobs = BTreeMap::new();
        for member in members.iter().collect::<BTreeSet<_>>() {
            ensure!(
                entries.contains_key(member),
                "ZIP entry is missing: {}",
                member.display()
            );
            let parent = member.parent().context("Select a ZIP entry to extract")?;
            for (path, entry) in entries
                .range(member.clone()..)
                .take_while(|(path, _)| path.starts_with(member))
            {
                let relative = path.strip_prefix(parent)?;
                ensure!(
                    !entry.symlink,
                    "ZIP contains a symbolic link: {}",
                    path.display()
                );
                if let Some(previous) = jobs.insert(relative, (path, entry)) {
                    ensure!(
                        previous.0 == path,
                        "Selected ZIP entries have the same extraction path"
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
                if let Some((target, mode)) =
                    extraction_target(&source_label, target, entry, &mut policy, cancel, conflict)?
                {
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
                                return Ok(());
                            }
                            let count = original.read(&mut buffer)?;
                            if count == 0 {
                                break;
                            }
                            file.write_all(&buffer[..count])?;
                        }
                    }
                    let mut input = zip.by_name(member_name(path)?.as_ref())?;
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
                        ensure!(written <= entry.size, "ZIP entry exceeds its declared size");
                        file.write_all(&buffer[..count])?;
                        if last_progress.elapsed().as_millis() >= 50 {
                            let mut snapshot = progress.clone();
                            snapshot.bytes_copied = snapshot.bytes_copied.saturating_add(written);
                            emit(CopyEvent::Progress(snapshot));
                            last_progress = Instant::now();
                        }
                    }
                    ensure!(
                        written == entry.size,
                        "ZIP entry size does not match its contents"
                    );
                    if cancel.is_cancelled() {
                        return Ok(());
                    }
                    if matches!(mode, WriteMode::New) {
                        file.persist_noclobber(target)?;
                    } else {
                        file.persist(target)?;
                    }
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
        Ok(())
    }
}

fn build_index(zip: &mut Reader) -> Result<Index> {
    let mut entries = Index::new();
    for index in 0..zip.len() {
        let file = zip.by_index_raw(index)?;
        let name = file.name()?;
        let clean = name.strip_suffix('/').unwrap_or(&name);
        if clean.is_empty() || !clean.split('/').all(valid_component) {
            continue;
        }
        let path = PathBuf::from(clean);
        let is_directory = file.is_dir();
        let modified = file
            .last_modified()
            .and_then(|date| {
                chrono::NaiveDate::from_ymd_opt(
                    date.year().into(),
                    date.month().into(),
                    date.day().into(),
                )?
                .and_hms_opt(
                    date.hour().into(),
                    date.minute().into(),
                    date.second().into(),
                )
                .and_then(|time| time.and_local_timezone(chrono::Local).earliest())
                .map(|time| time.timestamp_millis() as f64)
            })
            .unwrap_or_default();
        let entry = Member {
            name: clean.rsplit('/').next().unwrap_or_default().into(),
            directory: is_directory,
            symlink: file.is_symlink(),
            size: if is_directory { 0 } else { file.size() },
            modified,
        };
        ensure!(
            entries
                .get(&path)
                .is_none_or(|old| old.directory && is_directory),
            "Conflicting ZIP entries: {}",
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
                size: 0,
                modified: 0.,
            });
            ensure!(
                entry.directory && !entry.symlink,
                "Conflicting ZIP entries: {}",
                parent.display()
            );
        }
    }
    Ok(entries)
}

fn valid_component(name: &str) -> bool {
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

fn ensure_directory(base: &Path, relative: &Path) -> Result<()> {
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

enum WriteMode {
    New,
    Replace,
    Append,
}

fn extraction_target(
    source: &Path,
    target: PathBuf,
    entry: &Member,
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
        ConflictPolicy::OnlyNewer => {
            let modified = metadata
                .modified()?
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default();
            if entry.modified > modified.as_millis() as f64 {
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
mod tests {
    use super::*;
    use std::io::Cursor;
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

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
        let root = Archive::mount(path.clone());
        let inner =
            Archive::mount_location(root.resolve_archive_member("inner.zip").unwrap()).unwrap();
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
        let Location::Zip { archive, .. } = Archive::mount(path) else {
            panic!()
        };
        let fs = Filesystems::new(Default::default());
        let cancel = CancelToken::new();
        let stored = archive
            .open_member(Path::new("stored.mp4"), &fs, &cancel, 4)
            .unwrap();
        assert!(matches!(&stored.backing, FileBacking::Local(_)));
        let compressed = archive
            .open_member(Path::new("compressed.mp4"), &fs, &cancel, 10)
            .unwrap();
        assert!(matches!(&compressed.backing, FileBacking::Memory(_)));
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
        let Location::Zip { archive, .. } = Archive::mount(path.canonicalize().unwrap()) else {
            panic!()
        };
        let fs = Filesystems::new(Default::default());
        let folder = archive.location("folder".into());
        for (entry, _) in archive.list(Path::new("folder"), &fs).unwrap() {
            let inner = Archive::mount_location(entry).unwrap();
            assert_eq!(inner.parent(), Some(folder.clone()));
            let expected = SavedLocation::from(&inner);
            let saved: SavedLocation =
                serde_json::from_str(&serde_json::to_string(&expected).unwrap()).unwrap();
            assert!(saved.available());
            assert_eq!(Location::from(saved.clone()), inner);
            let Location::Zip { archive, .. } = Location::from(saved) else {
                panic!()
            };
            let files = archive.list(Path::new("images"), &fs).unwrap();
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].0.read_preview(&fs).unwrap(), b"picture");
        }
    }

    #[test]
    fn archive_nesting_has_a_depth_limit() {
        let mut location = Archive::mount("outer.zip".into());
        for _ in 1..MAX_ARCHIVE_DEPTH {
            let Location::Zip { archive, .. } = location else {
                panic!()
            };
            location = Archive::mount_location(archive.location("nested.zip".into())).unwrap();
        }
        assert!(Archive::mount_location(location).is_err());
    }

    #[test]
    fn zip_lists_implicit_folders_reads_members_and_round_trips_locations() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.ZIP");
        fixture(&path);
        let root = Archive::mount(path);
        assert_eq!(root.parent(), Some(temp.path().into()));
        let Location::Zip { archive, member } = &root else {
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
        assert_eq!(Location::from(restored), *file);
        let previous: SavedLocation = serde_json::from_str(r#""/tmp""#).unwrap();
        assert_eq!(Location::from(previous).local(), Some(Path::new("/tmp")));
    }

    #[test]
    fn zip_extraction_streams_selected_trees_and_respects_conflicts() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("example.zip");
        let dest = temp.path().join("dest");
        fs::create_dir(&dest).unwrap();
        fixture(&path);
        let Location::Zip { archive, .. } = Archive::mount(path.clone()) else {
            panic!()
        };
        let extract = |policy, conflict: &dyn Fn(&Path, &Path) -> ConflictResolution| {
            archive
                .extract(
                    vec!["docs".into(), "empty".into()],
                    &dest,
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
        let Location::Zip { archive, .. } = Archive::mount(path) else {
            panic!()
        };
        assert!(
            archive
                .extract(
                    vec!["docs".into()],
                    &dest,
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
