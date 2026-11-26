use crate::filesystems::{Filesystems, ProviderPath};
use anyhow::{Context, Result, bail, ensure};
use dotdir_core::copy::{CancelToken, ConflictPolicy, ConflictResolution, CopyEvent, CopyProgress};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs::{self, File},
    hash::{Hash, Hasher},
    io::{BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Instant, SystemTime},
};
use zip::ZipArchive;

pub const PREVIEW_LIMIT: u64 = 16 * 1024 * 1024;

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
    Zip { archive: PathBuf, member: PathBuf },
    Provider { uri: ProviderPath },
}

impl SavedLocation {
    pub fn available(&self) -> bool {
        match self {
            Self::Local(path) => path.is_dir(),
            Self::Zip { archive, .. } => archive.is_file(),
            Self::Provider { .. } => true,
        }
    }
}

impl From<SavedLocation> for Location {
    fn from(saved: SavedLocation) -> Self {
        match saved {
            SavedLocation::Local(path) => Self::Local(path),
            SavedLocation::Zip { archive, member } => Self::Zip {
                archive: Arc::new(Archive::new(archive)),
                member,
            },
            SavedLocation::Provider { uri } => Self::Provider(uri),
        }
    }
}

impl From<&Location> for SavedLocation {
    fn from(location: &Location) -> Self {
        match location {
            Location::Local(path) => Self::Local(path.clone()),
            Location::Zip { archive, member } => Self::Zip {
                archive: archive.path.clone(),
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

impl Location {
    pub fn local(&self) -> Option<&Path> {
        match self {
            Self::Local(path) => Some(path),
            _ => None,
        }
    }

    pub fn host_path(&self) -> Option<&Path> {
        match self {
            Self::Local(path) => Some(path),
            Self::Zip { archive, .. } => Some(&archive.path),
            Self::Provider(_) => None,
        }
    }

    pub fn name_path(&self) -> &Path {
        match self {
            Self::Zip { member, .. } if !member.as_os_str().is_empty() => member,
            Self::Provider(path) => Path::new(path.name().unwrap_or_default()),
            _ => self.host_path().expect("local or archive path"),
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
            Self::Provider(path) => Some(Self::Provider(path.parent()?)),
            _ => Some(self.host_path()?.parent()?.into()),
        }
    }

    pub fn display(&self) -> String {
        match self {
            Self::Local(path) => path.display().to_string(),
            Self::Zip { archive, member } => {
                format!("{}!/{}", archive.path.display(), member.display())
            }
            Self::Provider(path) => path.to_string(),
        }
    }

    pub fn read_preview(&self, filesystems: &Filesystems) -> Result<Vec<u8>> {
        match self {
            Self::Local(path) => {
                ensure!(
                    fs::metadata(path)?.len() <= PREVIEW_LIMIT,
                    "Files over 16 MiB require the streaming viewer"
                );
                let mut data = Vec::new();
                File::open(path)?
                    .take(PREVIEW_LIMIT + 1)
                    .read_to_end(&mut data)?;
                ensure!(
                    data.len() as u64 <= PREVIEW_LIMIT,
                    "Files over 16 MiB require the streaming viewer"
                );
                Ok(data)
            }
            Self::Zip { archive, member } => archive.read_preview(member),
            Self::Provider(path) => filesystems.read_file(path, PREVIEW_LIMIT as usize),
        }
    }
}

#[derive(Debug)]
pub struct Archive {
    path: PathBuf,
    index: Mutex<Option<CachedIndex>>,
}

#[derive(Debug)]
struct CachedIndex {
    length: u64,
    modified: Option<SystemTime>,
    entries: Arc<Index>,
}

#[derive(Debug, Clone)]
pub struct Member {
    pub name: String,
    pub directory: bool,
    pub symlink: bool,
    pub size: u64,
    pub modified: f64,
}

type Reader = ZipArchive<BufReader<File>>;
type Index = BTreeMap<PathBuf, Member>;

impl PartialEq for Archive {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}
impl Eq for Archive {}
impl Hash for Archive {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.path.hash(state);
    }
}

pub fn is_zip(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
}

impl Archive {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            index: Mutex::new(None),
        }
    }

    pub fn mount(path: PathBuf) -> Location {
        Arc::new(Self::new(path)).location(PathBuf::new())
    }

    fn location(self: &Arc<Self>, member: PathBuf) -> Location {
        Location::Zip {
            archive: self.clone(),
            member,
        }
    }

    fn open(&self) -> Result<Reader> {
        ZipArchive::new(BufReader::new(File::open(&self.path)?))
            .with_context(|| format!("Open ZIP archive {}", self.path.display()))
    }

    pub fn list(self: &Arc<Self>, dir: &Path) -> Result<Vec<(Location, Member)>> {
        let entries = {
            let mut cache = self.index.lock().unwrap();
            let metadata = fs::metadata(&self.path)?;
            let modified = metadata.modified().ok();
            if let Some(cached) = cache
                .as_ref()
                .filter(|cached| cached.length == metadata.len() && cached.modified == modified)
            {
                cached.entries.clone()
            } else {
                let entries = Arc::new(build_index(&mut self.open()?)?);
                *cache = Some(CachedIndex {
                    length: metadata.len(),
                    modified,
                    entries: entries.clone(),
                });
                entries
            }
        };
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

    fn read_preview(&self, path: &Path) -> Result<Vec<u8>> {
        let mut zip = self.open()?;
        let mut entry = zip.by_name(path.to_str().context("Invalid ZIP path")?)?;
        ensure!(
            entry.is_file() && !entry.is_symlink(),
            "ZIP entry is not a regular file"
        );
        let size = entry.size();
        ensure!(size <= PREVIEW_LIMIT, "ZIP previews are limited to 16 MiB");
        let mut bytes = Vec::new();
        (&mut entry).take(size + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 == size,
            "ZIP entry size does not match its contents"
        );
        Ok(bytes)
    }

    pub fn extract(
        &self,
        members: Vec<PathBuf>,
        destination: &Path,
        mut policy: ConflictPolicy,
        cancel: &CancelToken,
        emit: &dyn Fn(CopyEvent),
        conflict: &dyn Fn(&Path, &Path) -> ConflictResolution,
    ) -> Result<()> {
        let mut zip = self.open()?;
        let entries = build_index(&mut zip)?;
        let base = destination.canonicalize()?;
        let archive_path = self.path.canonicalize()?;
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
                    PathBuf::from(format!("{}!/{}", self.path.display(), path.display()));
                if let Some((target, mode)) =
                    extraction_target(&source_label, target, entry, &mut policy, cancel, conflict)?
                {
                    ensure!(
                        target != archive_path
                            && !same_file::is_same_file(&target, &archive_path).unwrap_or(false),
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
                    let mut input = zip.by_name(path.to_str().context("Invalid ZIP path")?)?;
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
    use zip::{ZipWriter, write::SimpleFileOptions};

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
    fn zip_lists_implicit_folders_reads_members_and_round_trips_locations() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.ZIP");
        fixture(&path);
        let root = Archive::mount(path);
        assert_eq!(root.parent(), Some(temp.path().into()));
        let Location::Zip { archive, member } = &root else {
            panic!()
        };
        let children = archive.list(member).unwrap();
        assert_eq!(
            children
                .iter()
                .map(|(_, entry)| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["docs", "empty", "sibling.txt"]
        );
        assert!(children[0].1.directory);
        let nested = archive.list(Path::new("docs")).unwrap();
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
                    &CancelToken::new(),
                    &|_| {},
                    &|_, _| unreachable!()
                )
                .is_err()
        );
        assert!(fs::read_dir(outside).unwrap().next().is_none());
    }
}
