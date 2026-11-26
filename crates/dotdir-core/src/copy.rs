/// Cross-platform copy engine with progress reporting, cancellation, and conflict resolution.
///
/// Platform fast paths:
///   macOS  — clonefile() (CoW) → chunked fallback
///   Linux  — ioctl(FICLONE) → copy_file_range() → chunked fallback
///   Windows — CopyFileExW with progress callback → chunked fallback
use crate::error::FsError;
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const CHUNK_SIZE: usize = 256 * 1024; // 256 KB
const SPARSE_BLOCK_SIZE: usize = 4096;

// ── Types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConflictPolicy {
    Ask,
    Overwrite,
    Skip,
    Rename,
    Append,
    Resume,
    OnlyNewer,
}

impl Default for ConflictPolicy {
    fn default() -> Self {
        Self::Ask
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SymlinkMode {
    Smart,
    AlwaysLink,
    AlwaysTarget,
}

impl Default for SymlinkMode {
    fn default() -> Self {
        Self::Smart
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyOptions {
    #[serde(default)]
    pub conflict_policy: ConflictPolicy,
    #[serde(default = "default_true")]
    pub copy_permissions: bool,
    #[serde(default)]
    pub copy_xattrs: bool,
    #[serde(default)]
    pub sparse_files: bool,
    #[serde(default)]
    pub use_cow: bool,
    #[serde(default)]
    pub symlink_mode: SymlinkMode,
    #[serde(default)]
    pub disable_write_cache: bool,
    #[serde(default = "default_true")]
    pub ask_read_only: bool,
    #[serde(default)]
    pub filter: Option<String>,
}

fn default_true() -> bool {
    true
}

impl Default for CopyOptions {
    fn default() -> Self {
        Self {
            conflict_policy: ConflictPolicy::default(),
            copy_permissions: true,
            copy_xattrs: false,
            sparse_files: false,
            use_cow: false,
            symlink_mode: SymlinkMode::default(),
            disable_write_cache: false,
            ask_read_only: true,
            filter: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConflictResolution {
    Overwrite,
    Skip,
    Rename(String),
    OverwriteAll,
    SkipAll,
    Cancel,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyProgress {
    pub bytes_copied: u64,
    pub bytes_total: u64,
    pub files_done: u32,
    pub files_total: u32,
    pub current_file: String,
}

/// Event emitted to the frontend during copy.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum CopyEvent {
    Progress(CopyProgress),
    Conflict {
        src: String,
        dest: String,
        #[serde(rename = "srcSize")]
        src_size: u64,
        #[serde(rename = "srcMtimeMs")]
        src_mtime_ms: f64,
        #[serde(rename = "destSize")]
        dest_size: u64,
        #[serde(rename = "destMtimeMs")]
        dest_mtime_ms: f64,
    },
    Done {
        #[serde(rename = "filesDone")]
        files_done: u32,
        #[serde(rename = "bytesCopied")]
        bytes_copied: u64,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct CopyResult {
    pub files_copied: u32,
    pub bytes_copied: u64,
}

/// Thread-safe cancellation token.
#[derive(Debug, Clone)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

// ── Pre-walk to calculate totals ─────────────────────────────────────

pub(crate) struct PreWalkResult {
    pub(crate) total_bytes: u64,
    pub(crate) total_files: u32,
}

pub(crate) fn pre_walk(sources: &[PathBuf]) -> PreWalkResult {
    let mut total_bytes = 0u64;
    let mut total_files = 0u32;
    let mut visited_dirs = HashSet::new();

    fn walk(path: &Path, bytes: &mut u64, files: &mut u32, visited: &mut HashSet<PathBuf>) {
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(_) => return,
        };
        if meta.file_type().is_symlink() {
            // Count symlinks as files; don't follow them to avoid loops
            *files += 1;
            return;
        }
        if meta.is_dir() {
            // Track visited directories by canonical path to detect loops
            let canonical = match path.canonicalize() {
                Ok(c) => c,
                Err(_) => return,
            };
            if !visited.insert(canonical) {
                return; // Already visited — symlink loop
            }
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    walk(&entry.path(), bytes, files, visited);
                }
            }
        } else {
            *bytes += meta.len();
            *files += 1;
        }
    }

    for source in sources {
        walk(
            source,
            &mut total_bytes,
            &mut total_files,
            &mut visited_dirs,
        );
    }

    PreWalkResult {
        total_bytes,
        total_files,
    }
}

// ── Unique name generation ──────────────────────────────────────────

pub fn generate_unique_name(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    let parent = path.parent().unwrap_or(path);

    let candidate = parent.join(format!("{stem}_copy{ext}"));
    if candidate.symlink_metadata().is_err() {
        return candidate;
    }

    for i in 2.. {
        let candidate = parent.join(format!("{stem}_copy{i}{ext}"));
        if candidate.symlink_metadata().is_err() {
            return candidate;
        }
    }
    unreachable!()
}

// ── Metadata helpers ─────────────────────────────────────────────────

pub(crate) fn mtime_ms(meta: &fs::Metadata) -> f64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

// ── Platform-specific single file copy ──────────────────────────────

#[cfg(target_os = "macos")]
fn copy_single_file_platform(
    src: &Path,
    dest: &Path,
    options: &CopyOptions,
    _token: &CancelToken,
    _on_progress: &dyn Fn(u64),
) -> Result<bool, FsError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    if options.use_cow {
        let src_c = CString::new(src.as_os_str().as_bytes()).map_err(|_| FsError::InvalidInput)?;
        let dest_c =
            CString::new(dest.as_os_str().as_bytes()).map_err(|_| FsError::InvalidInput)?;
        let expected_permissions = if options.copy_permissions {
            None
        } else {
            let file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(dest)
                .map_err(FsError::from_io)?;
            let permissions = file.metadata().map_err(FsError::from_io)?.permissions();
            drop(file);
            fs::remove_file(dest).map_err(FsError::from_io)?;
            Some(permissions)
        };
        // From sys/clonefile.h. Keep ownership local to the copy.
        const CLONE_NOOWNERCOPY: u32 = 0x0002;
        let ret = unsafe { libc::clonefile(src_c.as_ptr(), dest_c.as_ptr(), CLONE_NOOWNERCOPY) };
        if ret == 0 {
            if !options.copy_xattrs {
                let strip = || -> io::Result<()> {
                    for key in xattr::list(dest)? {
                        xattr::remove(dest, key)?;
                    }
                    Ok(())
                };
                if strip().is_err() {
                    fs::remove_file(dest).map_err(FsError::from_io)?;
                    return Ok(false);
                }
            }
            if let Some(permissions) = expected_permissions {
                fs::set_permissions(dest, permissions).map_err(FsError::from_io)?;
            }
            if let Ok(meta) = fs::metadata(src) {
                _on_progress(meta.len());
            }
            return Ok(true);
        }
        // ENOTSUP/EXDEV → fall through to regular copy
    }

    // Fall through to chunked copy
    Ok(false)
}

#[cfg(target_os = "linux")]
fn copy_single_file_platform(
    src: &Path,
    dest: &Path,
    options: &CopyOptions,
    _token: &CancelToken,
    on_progress: &dyn Fn(u64),
) -> Result<bool, FsError> {
    use std::os::unix::io::AsRawFd;

    if options.use_cow {
        // Try FICLONE
        let src_file = fs::File::open(src).map_err(FsError::from_io)?;
        let dest_file = fs::File::create(dest).map_err(FsError::from_io)?;
        // FICLONE = _IOW(0x94, 9, int) = 0x40049409
        const FICLONE: libc::c_ulong = 0x40049409;
        let ret = unsafe { libc::ioctl(dest_file.as_raw_fd(), FICLONE, src_file.as_raw_fd()) };
        if ret == 0 {
            if let Ok(meta) = fs::metadata(src) {
                on_progress(meta.len());
            }
            return Ok(true);
        }
        // Fall through
    }

    // Try copy_file_range
    {
        let src_file = fs::File::open(src).map_err(FsError::from_io)?;
        let dest_file = fs::File::create(dest).map_err(FsError::from_io)?;
        let src_meta = src_file.metadata().map_err(FsError::from_io)?;
        let total = src_meta.len();
        let mut copied = 0u64;

        loop {
            if _token.is_cancelled() {
                return Err(FsError::Io(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "cancelled",
                )));
            }
            let chunk = std::cmp::min(CHUNK_SIZE as u64, total - copied) as usize;
            if chunk == 0 {
                break;
            }
            let n = unsafe {
                libc::copy_file_range(
                    src_file.as_raw_fd(),
                    std::ptr::null_mut(),
                    dest_file.as_raw_fd(),
                    std::ptr::null_mut(),
                    chunk,
                    0,
                )
            };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if matches!(
                    err.raw_os_error(),
                    Some(libc::ENOSYS | libc::EXDEV | libc::EOPNOTSUPP | libc::EINVAL)
                ) {
                    // Not supported, fall through to chunked
                    // Clean up the partially created dest
                    drop(dest_file);
                    let _ = fs::remove_file(dest);
                    return Ok(false);
                }
                return Err(FsError::Io(err));
            }
            if n == 0 {
                break;
            }
            copied += n as u64;
            on_progress(n as u64);
        }
        return Ok(true);
    }
}

#[cfg(windows)]
fn copy_single_file_platform(
    src: &Path,
    dest: &Path,
    _options: &CopyOptions,
    token: &CancelToken,
    on_progress: &dyn Fn(u64),
) -> Result<bool, FsError> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    fn to_wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    let src_wide = to_wide(src.as_os_str());
    let dest_wide = to_wide(dest.as_os_str());

    struct CallbackData<'a> {
        token: &'a CancelToken,
        on_progress: &'a dyn Fn(u64),
        last_transferred: u64,
    }

    unsafe extern "system" fn progress_routine(
        _total_size: i64,
        total_transferred: i64,
        _stream_size: i64,
        _stream_transferred: i64,
        _stream_number: u32,
        _callback_reason: u32,
        _src_handle: *mut std::ffi::c_void,
        _dest_handle: *mut std::ffi::c_void,
        data: *const std::ffi::c_void,
    ) -> u32 {
        let data = &mut *(data as *mut CallbackData);
        if data.token.is_cancelled() {
            return 1; // PROGRESS_CANCEL
        }
        let transferred = total_transferred as u64;
        let delta = transferred.saturating_sub(data.last_transferred);
        if delta > 0 {
            (data.on_progress)(delta);
            data.last_transferred = transferred;
        }
        0 // PROGRESS_CONTINUE
    }

    let mut cb_data = CallbackData {
        token,
        on_progress,
        last_transferred: 0,
    };

    let ret = unsafe {
        windows_sys::Win32::Storage::FileSystem::CopyFileExW(
            src_wide.as_ptr(),
            dest_wide.as_ptr(),
            Some(progress_routine),
            &mut cb_data as *mut _ as *mut std::ffi::c_void,
            std::ptr::null_mut(),
            0,
        )
    };

    if ret == 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(1235) {
            // ERROR_REQUEST_ABORTED (cancel)
            return Err(FsError::Io(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "cancelled",
            )));
        }
        return Err(FsError::Io(err));
    }

    Ok(true)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn copy_single_file_platform(
    _src: &Path,
    _dest: &Path,
    _options: &CopyOptions,
    _token: &CancelToken,
    _on_progress: &dyn Fn(u64),
) -> Result<bool, FsError> {
    Ok(false)
}

/// Chunked read/write fallback (all platforms).
fn copy_single_file_chunked(
    src: &Path,
    dest: &Path,
    options: &CopyOptions,
    token: &CancelToken,
    on_progress: &dyn Fn(u64),
) -> Result<(), FsError> {
    let mut reader = fs::File::open(src).map_err(FsError::from_io)?;
    let mut writer = fs::File::create(dest).map_err(FsError::from_io)?;
    set_no_cache(&writer, options)?;
    copy_stream(&mut reader, &mut writer, options, token, on_progress)
}

fn set_no_cache(file: &fs::File, options: &CopyOptions) -> Result<(), FsError> {
    #[cfg(target_os = "macos")]
    if options.disable_write_cache {
        use std::os::fd::AsRawFd;
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1) } == -1 {
            return Err(FsError::from_io(io::Error::last_os_error()));
        }
    }
    let _ = (file, options);
    Ok(())
}

fn copy_stream(
    reader: &mut fs::File,
    writer: &mut fs::File,
    options: &CopyOptions,
    token: &CancelToken,
    on_progress: &dyn Fn(u64),
) -> Result<(), FsError> {
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut logical_len = writer.stream_position().map_err(FsError::from_io)?;
    let mut hole_start = None;

    loop {
        check_cancelled(token)?;
        let n = reader.read(&mut buf).map_err(FsError::from_io)?;
        if n == 0 {
            break;
        }
        if options.sparse_files {
            for block in buf[..n].chunks(SPARSE_BLOCK_SIZE) {
                if block.iter().all(|&byte| byte == 0) {
                    hole_start.get_or_insert(logical_len);
                    writer
                        .seek_relative(block.len() as i64)
                        .map_err(FsError::from_io)?;
                } else {
                    writer.write_all(block).map_err(FsError::from_io)?;
                    if let Some(start) = hole_start.take() {
                        deallocate_zero_range(writer, start, logical_len)?;
                    }
                }
                logical_len += block.len() as u64;
            }
        } else {
            writer.write_all(&buf[..n]).map_err(FsError::from_io)?;
        }
        on_progress(n as u64);
    }
    if options.sparse_files {
        writer.set_len(logical_len).map_err(FsError::from_io)?;
        if let Some(start) = hole_start {
            deallocate_zero_range(writer, start, logical_len)?;
        }
    }
    Ok(())
}

// APFS can allocate skipped blocks while extending a file. Punch only full
// blocks whose contents were verified to be zero.
#[cfg(target_os = "macos")]
fn deallocate_zero_range(file: &fs::File, start: u64, end: u64) -> Result<(), FsError> {
    use std::os::fd::AsRawFd;
    let block = SPARSE_BLOCK_SIZE as u64;
    let start = start.div_ceil(block) * block;
    let end = end / block * block;
    if end <= start {
        return Ok(());
    }
    let hole = libc::fpunchhole_t {
        fp_flags: 0,
        reserved: 0,
        fp_offset: start as i64,
        fp_length: (end - start) as i64,
    };
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &hole) } == -1 {
        let error = io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::EINVAL)) {
            return Err(FsError::from_io(error));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn deallocate_zero_range(_file: &fs::File, _start: u64, _end: u64) -> Result<(), FsError> {
    Ok(())
}

/// Copy permissions from source to dest (all platforms).
pub(crate) fn copy_permissions(src: &Path, dest: &Path) -> Result<(), FsError> {
    let meta = fs::metadata(src).map_err(FsError::from_io)?;
    fs::set_permissions(dest, meta.permissions()).map_err(FsError::from_io)?;
    Ok(())
}

/// Copy extended attributes (unix only).
#[cfg(unix)]
pub(crate) fn copy_xattrs(src: &Path, dest: &Path) -> Result<(), FsError> {
    let attrs = match xattr::list(src) {
        Ok(a) => a,
        Err(_) => return Ok(()),
    };
    for attr in attrs {
        if let Ok(Some(value)) = xattr::get(src, &attr) {
            let _ = xattr::set(dest, &attr, &value);
        }
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn copy_xattrs(_src: &Path, _dest: &Path) -> Result<(), FsError> {
    Ok(())
}

/// Copy a single file using platform fast paths with chunked fallback.
pub(crate) fn copy_single_file(
    src: &Path,
    dest: &Path,
    options: &CopyOptions,
    token: &CancelToken,
    on_progress: &dyn Fn(u64),
) -> Result<(), FsError> {
    // Try platform-specific first
    let handled = !options.sparse_files
        && (!options.disable_write_cache || (cfg!(target_os = "macos") && options.use_cow))
        && copy_single_file_platform(src, dest, options, token, on_progress)?;
    if !handled {
        copy_single_file_chunked(src, dest, options, token, on_progress)?;
    }

    if options.copy_permissions {
        let _ = copy_permissions(src, dest);
    }
    if options.copy_xattrs {
        let _ = copy_xattrs(src, dest);
    }

    Ok(())
}

// ── Entry point ─────────────────────────────────────────────────────

/// Copy one or more source paths into dest_dir.
pub fn copy_tree(
    sources: &[PathBuf],
    dest_dir: &Path,
    options: &CopyOptions,
    token: &CancelToken,
    on_progress: &dyn Fn(CopyEvent),
    on_conflict: &dyn Fn(&Path, &Path) -> ConflictResolution,
) -> Result<CopyResult, FsError> {
    copy_to_directories(
        sources,
        &[dest_dir.to_path_buf()],
        options,
        token,
        on_progress,
        on_conflict,
    )
}

pub fn validate_copy_filter(pattern: &str) -> Result<(), FsError> {
    compile_filter(pattern).map(|_| ())
}

fn compile_filter(pattern: &str) -> Result<globset::GlobMatcher, FsError> {
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|error| invalid(format!("Invalid copy filter: {error}")))
}

fn invalid(message: impl Into<String>) -> FsError {
    FsError::Io(io::Error::new(io::ErrorKind::InvalidInput, message.into()))
}

fn check_cancelled(token: &CancelToken) -> Result<(), FsError> {
    if token.is_cancelled() {
        return Err(FsError::Io(io::Error::new(
            io::ErrorKind::Interrupted,
            "cancelled",
        )));
    }
    Ok(())
}

fn existing(path: &Path) -> Result<Option<fs::Metadata>, FsError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(FsError::from_io(error)),
    }
}

struct CopyEntry {
    source: PathBuf,
    name: OsString,
    kind: CopyEntryKind,
}

enum CopyEntryKind {
    Directory(Vec<CopyEntry>),
    Link(PathBuf),
    File(u64),
}

struct CopyPlan<'a> {
    selected: Vec<PathBuf>,
    destinations: &'a [PathBuf],
    options: &'a CopyOptions,
    filter: Option<globset::GlobMatcher>,
    token: &'a CancelToken,
    bytes: u64,
    files: u32,
}

impl CopyPlan<'_> {
    fn entry(
        &mut self,
        source: PathBuf,
        relative: &Path,
        ancestors: &mut HashSet<PathBuf>,
    ) -> Result<Option<CopyEntry>, FsError> {
        check_cancelled(self.token)?;
        let name = relative
            .file_name()
            .ok_or(FsError::InvalidInput)?
            .to_owned();
        let mut source = source;
        let mut meta = fs::symlink_metadata(&source).map_err(FsError::from_io)?;
        let link = if meta.is_symlink() {
            let target = fs::read_link(&source).map_err(FsError::from_io)?;
            let resolved = match self.options.symlink_mode {
                SymlinkMode::AlwaysLink => None,
                SymlinkMode::AlwaysTarget => Some(source.canonicalize().map_err(FsError::from_io)?),
                SymlinkMode::Smart => source.canonicalize().ok().filter(|resolved| {
                    !self.selected.iter().any(|root| resolved.starts_with(root))
                }),
            };
            if let Some(resolved) = resolved {
                source = resolved;
                meta = fs::metadata(&source).map_err(FsError::from_io)?;
                None
            } else {
                Some(target)
            }
        } else {
            None
        };
        let kind = if meta.is_dir() {
            let canonical = source.canonicalize().map_err(FsError::from_io)?;
            if self
                .destinations
                .iter()
                .any(|path| path.starts_with(&canonical))
            {
                return Err(invalid(format!(
                    "Cannot copy '{}' into itself",
                    source.display()
                )));
            }
            if !ancestors.insert(canonical.clone()) {
                return Err(invalid(format!(
                    "Circular symlink at '{}'",
                    relative.display()
                )));
            }
            let mut children = Vec::new();
            for item in fs::read_dir(&source).map_err(FsError::from_io)? {
                let item = item.map_err(FsError::from_io)?;
                if let Some(entry) =
                    self.entry(item.path(), &relative.join(item.file_name()), ancestors)?
                {
                    children.push(entry);
                }
            }
            ancestors.remove(&canonical);
            if self.filter.is_some() && children.is_empty() {
                return Ok(None);
            }
            CopyEntryKind::Directory(children)
        } else {
            if self.filter.as_ref().is_some_and(|filter| {
                !filter.is_match(relative) && !filter.is_match(Path::new(&name))
            }) {
                return Ok(None);
            }
            self.files += 1;
            if let Some(target) = link {
                CopyEntryKind::Link(target)
            } else if meta.is_file() {
                self.bytes += meta.len();
                CopyEntryKind::File(meta.len())
            } else {
                return Err(invalid(format!(
                    "Unsupported file type: {}",
                    source.display()
                )));
            }
        };
        Ok(Some(CopyEntry { source, name, kind }))
    }
}

/// Copy the same selection to each directory as one cancellable operation.
pub fn copy_to_directories(
    sources: &[PathBuf],
    destinations: &[PathBuf],
    options: &CopyOptions,
    token: &CancelToken,
    on_progress: &dyn Fn(CopyEvent),
    on_conflict: &dyn Fn(&Path, &Path) -> ConflictResolution,
) -> Result<CopyResult, FsError> {
    let mut destinations = destinations
        .iter()
        .map(|path| path.canonicalize().map_err(FsError::from_io))
        .collect::<Result<Vec<_>, _>>()?;
    let mut seen = HashSet::new();
    destinations.retain(|path| seen.insert(path.clone()));
    if destinations.iter().any(|path| !path.is_dir()) {
        return Err(invalid("Copy destinations must be directories"));
    }
    let mut plan = CopyPlan {
        selected: sources
            .iter()
            .filter_map(|path| {
                let meta = fs::symlink_metadata(path).ok()?;
                (!meta.is_symlink())
                    .then(|| path.canonicalize().ok())
                    .flatten()
            })
            .collect(),
        destinations: &destinations,
        options,
        filter: options.filter.as_deref().map(compile_filter).transpose()?,
        token,
        files: 0,
        bytes: 0,
    };
    let mut entries = Vec::new();
    for source in sources {
        let name = source
            .file_name()
            .ok_or_else(|| invalid("Filesystem roots cannot be copied"))?;
        if let Some(entry) = plan.entry(source.clone(), Path::new(name), &mut HashSet::new())? {
            entries.push(entry);
        }
    }
    if options.filter.is_some() && entries.is_empty() {
        return Err(invalid("No files match the copy filter"));
    }
    let mut job = CopyJob {
        options,
        token,
        on_progress,
        on_conflict,
        override_all: None,
        read_only_override: None,
        bytes: Cell::new(0),
        files: 0,
        total_bytes: plan.bytes.saturating_mul(destinations.len() as u64),
        total_files: plan.files.saturating_mul(destinations.len() as u32),
    };
    for directory in &destinations {
        for entry in &entries {
            job.copy(entry, &directory.join(&entry.name))?;
        }
    }
    on_progress(CopyEvent::Done {
        files_done: job.files,
        bytes_copied: job.bytes.get(),
    });
    Ok(CopyResult {
        files_copied: job.files,
        bytes_copied: job.bytes.get(),
    })
}

struct CopyJob<'a> {
    options: &'a CopyOptions,
    token: &'a CancelToken,
    on_progress: &'a dyn Fn(CopyEvent),
    on_conflict: &'a dyn Fn(&Path, &Path) -> ConflictResolution,
    override_all: Option<ConflictResolution>,
    read_only_override: Option<ConflictResolution>,
    bytes: Cell<u64>,
    files: u32,
    total_bytes: u64,
    total_files: u32,
}

#[derive(Clone, Copy)]
enum WriteMode {
    Replace,
    Append,
    Resume,
}

impl CopyJob<'_> {
    fn report(&self, entry: &CopyEntry, delta: u64) {
        self.bytes.set(self.bytes.get() + delta);
        (self.on_progress)(CopyEvent::Progress(CopyProgress {
            bytes_copied: self.bytes.get(),
            bytes_total: self.total_bytes,
            files_done: self.files,
            files_total: self.total_files,
            current_file: entry.name.to_string_lossy().into_owned(),
        }));
    }

    fn destination(
        &mut self,
        source: &Path,
        dest: &Path,
        meta: &fs::Metadata,
    ) -> Result<Option<(PathBuf, WriteMode)>, FsError> {
        if matches!(self.override_all, Some(ConflictResolution::Skip)) {
            return Ok(None);
        }
        let result = |mode| Ok(Some((dest.to_path_buf(), mode)));
        let mode = match self.options.conflict_policy {
            ConflictPolicy::Skip => return Ok(None),
            ConflictPolicy::Rename => {
                return Ok(Some((generate_unique_name(dest), WriteMode::Replace)))
            }
            ConflictPolicy::OnlyNewer => {
                if mtime_ms(&fs::symlink_metadata(source).map_err(FsError::from_io)?)
                    <= mtime_ms(meta)
                {
                    return Ok(None);
                }
                WriteMode::Replace
            }
            ConflictPolicy::Append => WriteMode::Append,
            ConflictPolicy::Resume => WriteMode::Resume,
            _ => WriteMode::Replace,
        };
        let read_only =
            self.options.ask_read_only && !meta.is_symlink() && meta.permissions().readonly();
        if read_only || self.options.conflict_policy == ConflictPolicy::Ask {
            let preference = if read_only {
                &mut self.read_only_override
            } else {
                &mut self.override_all
            };
            let resolution = preference
                .clone()
                .unwrap_or_else(|| (self.on_conflict)(source, dest));
            return match resolution {
                ConflictResolution::Overwrite => result(mode),
                ConflictResolution::OverwriteAll => {
                    *preference = Some(ConflictResolution::Overwrite);
                    result(mode)
                }
                ConflictResolution::SkipAll => {
                    *preference = Some(ConflictResolution::Skip);
                    Ok(None)
                }
                ConflictResolution::Skip => Ok(None),
                ConflictResolution::Cancel => {
                    self.token.cancel();
                    check_cancelled(self.token)?;
                    Ok(None)
                }
                ConflictResolution::Rename(name) => {
                    let mut components = Path::new(&name).components();
                    if !matches!(components.next(), Some(std::path::Component::Normal(_)))
                        || components.next().is_some()
                    {
                        return Err(FsError::InvalidInput);
                    }
                    Ok(Some((dest.with_file_name(name), WriteMode::Replace)))
                }
            };
        }
        result(mode)
    }

    fn copy(&mut self, entry: &CopyEntry, dest: &Path) -> Result<(), FsError> {
        check_cancelled(self.token)?;
        let meta = existing(dest)?;
        let original = dest;
        let is_dir = matches!(entry.kind, CopyEntryKind::Directory(_));
        let merge = is_dir
            && meta.as_ref().is_some_and(fs::Metadata::is_dir)
            && !same_file::is_same_file(dest, &entry.source).unwrap_or(false);
        // Copying into the source parent is valid with Rename or Skip. Guard
        // the final destination after conflict resolution, including dir merges.
        let (dest, mode) = if let Some(meta) = &meta {
            if merge {
                (dest.to_owned(), WriteMode::Replace)
            } else {
                let Some(target) = self.destination(&entry.source, dest, meta)? else {
                    return Ok(());
                };
                target
            }
        } else {
            (dest.to_owned(), WriteMode::Replace)
        };
        let current = existing(&dest)?;
        if dest != original && current.is_some() {
            return Err(invalid(format!(
                "Rename destination already exists: {}",
                dest.display()
            )));
        }
        if dest == original && !same_destination(meta.as_ref(), current.as_ref()) {
            return Err(invalid(format!(
                "Destination changed while resolving the conflict: {}",
                dest.display()
            )));
        }
        if entry.source == dest
            || current.as_ref().is_some_and(|meta| {
                !meta.is_symlink() && same_file::is_same_file(&dest, &entry.source).unwrap_or(false)
            })
        {
            return Err(invalid(format!(
                "Cannot copy '{}' onto itself",
                entry.source.display()
            )));
        }
        if current.as_ref().is_some_and(fs::Metadata::is_dir) && !is_dir {
            return Err(invalid(format!(
                "Cannot replace directory '{}' with a file",
                dest.display()
            )));
        }
        match &entry.kind {
            CopyEntryKind::Directory(children) => {
                if !merge || current.as_ref().is_none_or(|meta| !meta.is_dir()) {
                    if current.is_some() {
                        return Err(invalid(format!(
                            "Cannot replace '{}' with a directory",
                            dest.display()
                        )));
                    }
                    fs::create_dir(&dest).map_err(FsError::from_io)?;
                }
                for child in children {
                    self.copy(child, &dest.join(&child.name))?;
                }
                if self.options.copy_permissions {
                    copy_permissions(&entry.source, &dest)?;
                }
                if self.options.copy_xattrs {
                    copy_xattrs(&entry.source, &dest)?;
                }
            }
            CopyEntryKind::Link(target) => {
                if !matches!(mode, WriteMode::Replace) {
                    return Err(invalid("Append and Resume require regular files"));
                }
                self.publish(&dest, current.as_ref(), |staged| {
                    create_link(&entry.source, target, staged)?;
                    if self.options.copy_xattrs {
                        copy_xattrs(&entry.source, staged)?;
                    }
                    Ok(())
                })?;
                self.files += 1;
                self.report(entry, 0);
            }
            CopyEntryKind::File(length) => {
                self.report(entry, 0);
                let progress = |delta| self.report(entry, delta);
                match mode {
                    WriteMode::Replace => self.publish(&dest, current.as_ref(), |staged| {
                        copy_single_file(
                            &entry.source,
                            staged,
                            self.options,
                            self.token,
                            &progress,
                        )?;
                        if !self.options.copy_permissions {
                            if let Some(previous) =
                                current.as_ref().filter(|metadata| metadata.is_file())
                            {
                                fs::set_permissions(staged, previous.permissions())
                                    .map_err(FsError::from_io)?;
                            }
                        }
                        Ok(())
                    })?,
                    WriteMode::Append | WriteMode::Resume => {
                        if !current.as_ref().is_some_and(fs::Metadata::is_file) {
                            return Err(invalid("Append and Resume require regular files"));
                        }
                        self.publish(&dest, current.as_ref(), |staged| {
                            let mut input =
                                fs::File::open(&entry.source).map_err(FsError::from_io)?;
                            let mut prior = open_copy_target(&dest)?;
                            let mut output = fs::File::create(staged).map_err(FsError::from_io)?;
                            set_no_cache(&output, self.options)?;
                            if matches!(mode, WriteMode::Resume) {
                                let prefix = prior.metadata().map_err(FsError::from_io)?.len();
                                if prefix > *length {
                                    return Err(invalid(format!(
                                        "Cannot resume '{}': destination is larger than the source",
                                        dest.display()
                                    )));
                                }
                                verify_prefix(&mut input, &mut prior, prefix, self.token, &dest)?;
                                prior.rewind().map_err(FsError::from_io)?;
                                copy_stream(
                                    &mut prior,
                                    &mut output,
                                    self.options,
                                    self.token,
                                    &progress,
                                )?;
                            } else {
                                copy_stream(
                                    &mut prior,
                                    &mut output,
                                    self.options,
                                    self.token,
                                    &|_| {},
                                )?;
                            }
                            copy_stream(
                                &mut input,
                                &mut output,
                                self.options,
                                self.token,
                                &progress,
                            )?;
                            if self.options.copy_permissions {
                                copy_permissions(&entry.source, staged)?;
                            } else {
                                copy_permissions(&dest, staged)?;
                            }
                            if self.options.copy_xattrs {
                                copy_xattrs(&entry.source, staged)?;
                            }
                            Ok(())
                        })?;
                    }
                }
                self.files += 1;
            }
        }
        Ok(())
    }

    fn publish(
        &self,
        dest: &Path,
        previous: Option<&fs::Metadata>,
        write: impl FnOnce(&Path) -> Result<(), FsError>,
    ) -> Result<(), FsError> {
        let parent = dest.parent().ok_or(FsError::InvalidInput)?;
        let stage = tempfile::Builder::new()
            .prefix(".dotdir-copy-")
            .tempdir_in(parent)
            .map_err(FsError::from_io)?;
        let path = stage.path().join("data");
        write(&path)?;
        check_cancelled(self.token)?;
        if let Some(previous) = previous {
            if !existing(dest)?
                .as_ref()
                .is_some_and(|metadata| same_revision(previous, metadata))
            {
                return Err(invalid(format!(
                    "Destination changed during copy: {}",
                    dest.display()
                )));
            }
            fs::rename(path, dest).map_err(FsError::from_io)
        } else {
            publish_new(&path, dest).map_err(FsError::from_io)
        }
    }
}

fn same_revision(previous: &fs::Metadata, current: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (
            previous.dev(),
            previous.ino(),
            previous.mode(),
            previous.ctime(),
            previous.ctime_nsec(),
        ) != (
            current.dev(),
            current.ino(),
            current.mode(),
            current.ctime(),
            current.ctime_nsec(),
        ) {
            return false;
        }
    }
    previous.file_type() == current.file_type()
        && previous.len() == current.len()
        && previous.modified().ok() == current.modified().ok()
        && previous.created().ok() == current.created().ok()
}

fn same_destination(previous: Option<&fs::Metadata>, current: Option<&fs::Metadata>) -> bool {
    match (previous, current) {
        (Some(previous), Some(current)) => same_revision(previous, current),
        (None, None) => true,
        _ => false,
    }
}

fn publish_new(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let src = CString::new(source.as_os_str().as_bytes())?;
        let dest = CString::new(destination.as_os_str().as_bytes())?;
        if unsafe { libc::renamex_np(src.as_ptr(), dest.as_ptr(), libc::RENAME_EXCL) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::EINVAL)) {
            return Err(error);
        }
    }
    // A hard link atomically claims the new name without replacing an entry
    // created by another process. The staging directory removes its old name.
    #[cfg(unix)]
    {
        match fs::hard_link(source, destination) {
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {}
            result => return result,
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let src = source
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let dest = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        if unsafe {
            windows_sys::Win32::Storage::FileSystem::MoveFileExW(src.as_ptr(), dest.as_ptr(), 0)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        return Ok(());
    }
    #[cfg(not(windows))]
    {
        // Volumes without exclusive rename or hard links still support copy.
        if destination.symlink_metadata().is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Copy destination already exists",
            ));
        }
        fs::rename(source, destination)
    }
}

fn open_copy_target(path: &Path) -> Result<fs::File, FsError> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(FsError::from_io)?;
    if !file.metadata().map_err(FsError::from_io)?.is_file() {
        return Err(invalid("Append and Resume require regular files"));
    }
    Ok(file)
}

fn verify_prefix(
    src: &mut fs::File,
    dest: &mut fs::File,
    mut left: u64,
    token: &CancelToken,
    path: &Path,
) -> Result<(), FsError> {
    let mut source = vec![0; CHUNK_SIZE];
    let mut previous = vec![0; CHUNK_SIZE];
    while left > 0 {
        check_cancelled(token)?;
        let size = left.min(CHUNK_SIZE as u64) as usize;
        src.read_exact(&mut source[..size])
            .map_err(FsError::from_io)?;
        dest.read_exact(&mut previous[..size])
            .map_err(FsError::from_io)?;
        if source[..size] != previous[..size] {
            return Err(invalid(format!(
                "Cannot resume '{}': existing contents do not match the source",
                path.display()
            )));
        }
        left -= size as u64;
    }
    Ok(())
}

fn create_link(source: &Path, target: &Path, dest: &Path) -> Result<(), FsError> {
    #[cfg(unix)]
    {
        let _ = source;
        std::os::unix::fs::symlink(target, dest).map_err(FsError::from_io)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{symlink_dir, symlink_file, FileTypeExt};
        let kind = fs::symlink_metadata(source)
            .map_err(FsError::from_io)?
            .file_type();
        if kind.is_symlink_dir() {
            symlink_dir(target, dest).map_err(FsError::from_io)
        } else {
            symlink_file(target, dest).map_err(FsError::from_io)
        }
    }
    #[cfg(not(any(unix, windows)))]
    Err(invalid("Symbolic links are unavailable on this platform"))
}

pub(crate) fn copy_symlink(src: &Path, dest: &Path) -> Result<(), FsError> {
    let target = fs::read_link(src).map_err(FsError::from_io)?;
    create_link(src, &target, dest)
}

/// Handle conflict: returns Some(dest_path) to proceed, None to skip.
pub(crate) fn handle_conflict(
    src: &Path,
    dest: &Path,
    options: &CopyOptions,
    on_conflict: &dyn Fn(&Path, &Path) -> ConflictResolution,
    override_all: &mut Option<ConflictResolution>,
    token: &CancelToken,
) -> Result<Option<PathBuf>, FsError> {
    if !dest.exists() {
        return Ok(Some(dest.to_path_buf()));
    }

    // Check for "All" overrides first
    if let Some(ref all) = override_all {
        return apply_resolution(all, src, dest);
    }

    match options.conflict_policy {
        ConflictPolicy::Overwrite => Ok(Some(dest.to_path_buf())),
        ConflictPolicy::Skip => Ok(None),
        ConflictPolicy::Rename => Ok(Some(generate_unique_name(dest))),
        ConflictPolicy::OnlyNewer => {
            let src_meta = fs::metadata(src).map_err(FsError::from_io)?;
            let dest_meta = fs::metadata(dest).map_err(FsError::from_io)?;
            if mtime_ms(&src_meta) > mtime_ms(&dest_meta) {
                Ok(Some(dest.to_path_buf()))
            } else {
                Ok(None)
            }
        }
        ConflictPolicy::Append => {
            // Append is unusual for copy — treat as overwrite for now
            Ok(Some(dest.to_path_buf()))
        }
        ConflictPolicy::Resume => Err(invalid("Resume is available for local copies only")),
        ConflictPolicy::Ask => {
            if token.is_cancelled() {
                return Ok(None);
            }
            let resolution = on_conflict(src, dest);
            match &resolution {
                ConflictResolution::OverwriteAll => {
                    *override_all = Some(ConflictResolution::Overwrite);
                    Ok(Some(dest.to_path_buf()))
                }
                ConflictResolution::SkipAll => {
                    *override_all = Some(ConflictResolution::Skip);
                    Ok(None)
                }
                ConflictResolution::Cancel => {
                    token.cancel();
                    Ok(None)
                }
                other => apply_resolution(other, src, dest),
            }
        }
    }
}

pub(crate) fn apply_resolution(
    resolution: &ConflictResolution,
    _src: &Path,
    dest: &Path,
) -> Result<Option<PathBuf>, FsError> {
    match resolution {
        ConflictResolution::Overwrite | ConflictResolution::OverwriteAll => {
            Ok(Some(dest.to_path_buf()))
        }
        ConflictResolution::Skip | ConflictResolution::SkipAll => Ok(None),
        ConflictResolution::Rename(new_name) => {
            let parent = dest.parent().unwrap_or(dest);
            Ok(Some(parent.join(new_name)))
        }
        ConflictResolution::Cancel => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let dest = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&dest).unwrap();
        (temp, source, dest)
    }

    fn unexpected_conflict(_: &Path, _: &Path) -> ConflictResolution {
        panic!("unexpected conflict");
    }

    #[test]
    fn append_and_resume_preserve_data_on_cancellation_or_mismatch() {
        let (_temp, source, dest) = fixture();
        let source = source.join("example");
        let target = dest.join("example");
        fs::write(&source, "abcdef").unwrap();
        let mut options = CopyOptions {
            conflict_policy: ConflictPolicy::Resume,
            ..Default::default()
        };
        let run = |options: &CopyOptions| {
            copy_tree(
                std::slice::from_ref(&source),
                &dest,
                options,
                &CancelToken::new(),
                &|_| {},
                &unexpected_conflict,
            )
        };

        fs::write(&target, "abc").unwrap();
        assert_eq!(run(&options).unwrap().bytes_copied, 6);
        assert_eq!(fs::read(&target).unwrap(), b"abcdef");
        fs::write(&target, "ax").unwrap();
        assert!(run(&options)
            .unwrap_err()
            .to_string()
            .contains("do not match"));
        assert_eq!(fs::read(&target).unwrap(), b"ax");
        options.conflict_policy = ConflictPolicy::Append;
        assert_eq!(run(&options).unwrap().bytes_copied, 6);
        assert_eq!(fs::read(&target).unwrap(), b"axabcdef");
        options.conflict_policy = ConflictPolicy::Overwrite;
        let cancel = CancelToken::new();
        let result = copy_tree(
            std::slice::from_ref(&source),
            &dest,
            &options,
            &cancel,
            &|event| {
                if matches!(event, CopyEvent::Progress(progress) if progress.bytes_copied > 0) {
                    cancel.cancel();
                }
            },
            &unexpected_conflict,
        );
        assert!(result.is_err());
        assert_eq!(fs::read(&target).unwrap(), b"axabcdef");
        assert_eq!(fs::read_dir(dest).unwrap().count(), 1);
    }

    #[test]
    fn filters_nested_files_and_counts_each_distinct_destination() {
        let (temp, source, dest) = fixture();
        let other = temp.path().join("other");
        fs::create_dir(&other).unwrap();
        fs::create_dir_all(source.join("nested/empty")).unwrap();
        fs::write(source.join("nested/yes.rs"), "rust").unwrap();
        fs::write(source.join("nested/no.txt"), "text").unwrap();
        let options = CopyOptions {
            filter: Some("*.rs".into()),
            ..Default::default()
        };
        let result = copy_to_directories(
            &[source],
            &[dest.clone(), other.clone(), dest.clone()],
            &options,
            &CancelToken::new(),
            &|_| {},
            &unexpected_conflict,
        )
        .unwrap();
        assert_eq!((result.files_copied, result.bytes_copied), (2, 8));
        for dir in [dest, other] {
            assert_eq!(fs::read(dir.join("source/nested/yes.rs")).unwrap(), b"rust");
            assert!(!dir.join("source/nested/no.txt").exists());
            assert!(!dir.join("source/nested/empty").exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_modes_preserve_internal_links_and_copy_external_trees() {
        use std::os::unix::fs::symlink;
        let (temp, source, dest) = fixture();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("payload"), "external").unwrap();
        fs::write(source.join("file"), "internal").unwrap();
        symlink("file", source.join("inside")).unwrap();
        symlink("../outside", source.join("external")).unwrap();
        symlink("missing", source.join("dangling")).unwrap();
        let run = |destination: &Path, mode| {
            copy_tree(
                std::slice::from_ref(&source),
                destination,
                &CopyOptions {
                    symlink_mode: mode,
                    ..Default::default()
                },
                &CancelToken::new(),
                &|_| {},
                &unexpected_conflict,
            )
        };
        run(&dest, SymlinkMode::Smart).unwrap();
        let output = dest.join("source");
        assert_eq!(
            fs::read_link(output.join("inside")).unwrap(),
            Path::new("file")
        );
        assert_eq!(
            fs::read(output.join("external/payload")).unwrap(),
            b"external"
        );
        assert!(!output
            .join("external")
            .symlink_metadata()
            .unwrap()
            .is_symlink());
        assert_eq!(
            fs::read_link(output.join("dangling")).unwrap(),
            Path::new("missing")
        );

        let second = temp.path().join("second");
        fs::create_dir(&second).unwrap();
        run(&second, SymlinkMode::AlwaysLink).unwrap();
        assert_eq!(
            fs::read_link(second.join("source/external")).unwrap(),
            Path::new("../outside")
        );

        fs::remove_file(source.join("dangling")).unwrap();
        let third = temp.path().join("third");
        fs::create_dir(&third).unwrap();
        run(&third, SymlinkMode::AlwaysTarget).unwrap();
        assert!(!third
            .join("source/inside")
            .symlink_metadata()
            .unwrap()
            .is_symlink());
        assert_eq!(fs::read(third.join("source/inside")).unwrap(), b"internal");
        symlink(".", source.join("loop")).unwrap();
        assert!(run(&third, SymlinkMode::AlwaysTarget)
            .unwrap_err()
            .to_string()
            .contains("Circular symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn sparse_and_mac_cache_options_preserve_data_and_read_only_confirms() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let (_temp, source, dest) = fixture();
        let source = source.join("example");
        let target = dest.join("example");
        let mut payload = vec![0; 2 * CHUNK_SIZE];
        payload[0] = 2;
        payload[CHUNK_SIZE] = 7;
        fs::write(&source, &payload).unwrap();
        fs::write(&target, "old").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o444)).unwrap();
        let attribute = "user.dotdir_copy_test";
        xattr::set(&source, attribute, b"fixture").unwrap();
        let options = CopyOptions {
            conflict_policy: ConflictPolicy::Overwrite,
            sparse_files: true,
            disable_write_cache: cfg!(target_os = "macos"),
            copy_xattrs: true,
            ..Default::default()
        };
        let calls = Cell::new(0);
        copy_tree(
            &[source],
            &dest,
            &options,
            &CancelToken::new(),
            &|_| {},
            &|_, _| {
                calls.set(calls.get() + 1);
                ConflictResolution::Overwrite
            },
        )
        .unwrap();
        assert_eq!(calls.get(), 1);
        let meta = target.metadata().unwrap();
        assert_eq!(meta.mode() & 0o777, 0o640);
        assert!(meta.blocks() * 512 < payload.len() as u64);
        assert_eq!(fs::read(&target).unwrap(), payload);
        assert_eq!(xattr::get(&target, attribute).unwrap().unwrap(), b"fixture");
    }

    #[test]
    fn copy_cannot_overwrite_its_source_and_detects_a_new_destination() {
        let (_temp, source, dest) = fixture();
        let item = source.join("item");
        let target = dest.join("item");
        fs::write(&item, "source").unwrap();
        let token = CancelToken::new();
        let options = CopyOptions {
            conflict_policy: ConflictPolicy::Overwrite,
            ..Default::default()
        };
        assert!(copy_tree(
            std::slice::from_ref(&item),
            &source,
            &options,
            &token,
            &|_| {},
            &unexpected_conflict
        )
        .is_err());
        assert_eq!(fs::read(&item).unwrap(), b"source");
        assert!(copy_tree(
            &[item],
            &dest,
            &options,
            &token,
            &|event| {
                if matches!(event, CopyEvent::Progress(progress) if progress.bytes_copied > 0) {
                    fs::write(&target, "concurrent").unwrap();
                }
            },
            &unexpected_conflict
        )
        .is_err());
        assert_eq!(fs::read(&target).unwrap(), b"concurrent");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn copy_on_write_respects_metadata_options() {
        use std::os::unix::fs::PermissionsExt;
        let (temp, source, dest) = fixture();
        let source = source.join("example");
        let no_metadata = dest.join("example");
        let preserve = temp.path().join("preserve");
        let attribute = "user.dotdir_clone_test";
        fs::create_dir(&preserve).unwrap();
        fs::write(&source, "content").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o700)).unwrap();
        xattr::set(&source, attribute, b"fixture").unwrap();
        let mut options = CopyOptions {
            use_cow: true,
            copy_permissions: false,
            ..Default::default()
        };
        copy_tree(
            std::slice::from_ref(&source),
            &dest,
            &options,
            &CancelToken::new(),
            &|_| {},
            &unexpected_conflict,
        )
        .unwrap();
        assert_eq!(
            no_metadata.metadata().unwrap().permissions().mode() & 0o111,
            0
        );
        assert!(xattr::get(&no_metadata, attribute).unwrap().is_none());
        options.copy_permissions = true;
        options.copy_xattrs = true;
        copy_tree(
            std::slice::from_ref(&source),
            &preserve,
            &options,
            &CancelToken::new(),
            &|_| {},
            &unexpected_conflict,
        )
        .unwrap();
        let cloned = preserve.join("example");
        assert_eq!(
            cloned.metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(xattr::get(&cloned, attribute).unwrap().unwrap(), b"fixture");
        fs::write(cloned, "changed").unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"content");
    }
}
