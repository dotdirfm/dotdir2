//! Native archive format extensions share these types with DotDir.

use anyhow::{Result, bail, ensure};
use std::{
    borrow::Cow,
    collections::BTreeMap,
    fmt::Debug,
    fs::File,
    io::{self, BufReader, Read, Seek, SeekFrom, Write},
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};
pub use zeroize::Zeroizing;

#[derive(Debug)]
pub struct FormatInfo {
    /// Stable identifier stored in session locations.
    pub id: &'static str,
    pub name: &'static str,
    pub version: &'static str,
    /// Filename suffixes without their leading dot, such as "zip" or "tar.gz".
    pub suffixes: &'static [&'static str],
    pub writable: bool,
}

pub trait ArchiveFormat: Debug + Send + Sync {
    fn info(&self) -> &'static FormatInfo;
    fn open(
        &self,
        source: VirtualFile,
        access: &dyn PasswordAccess,
    ) -> Result<Box<dyn ArchiveReader>>;

    fn creation(&self) -> Option<&'static Creation> {
        None
    }

    fn create(
        &self,
        _entries: &[Input<'_>],
        _options: &CreationOptions,
        _output: &mut dyn WriteSeek,
    ) -> Result<()> {
        bail!("This format cannot create archives")
    }
}

pub struct Creation {
    pub name: &'static str,
    pub suffix: &'static str,
    pub fields: &'static [OptionField],
}

pub struct OptionField {
    pub id: &'static str,
    pub label: &'static str,
    pub default: &'static str,
    pub kind: OptionKind,
}

pub enum OptionKind {
    Choice(&'static [Choice]),
    Text,
    Secret,
}

pub struct Choice {
    pub value: &'static str,
    pub label: &'static str,
    /// Output suffix when this choice changes the archive's on-disk format.
    pub suffix: Option<&'static str>,
}

pub type CreationOptions = BTreeMap<&'static str, OptionValue>;

pub enum OptionValue {
    Text(String),
    Secret(Password),
}

impl OptionValue {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Secret(_) => None,
        }
    }

    pub fn as_secret(&self) -> Option<&[u8]> {
        match self {
            Self::Text(_) => None,
            Self::Secret(password) => Some(password.as_slice()),
        }
    }
}

impl From<&str> for OptionValue {
    fn from(text: &str) -> Self {
        Self::Text(text.into())
    }
}

pub struct Password(Zeroizing<Vec<u8>>);

impl Password {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }
}

impl Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Password([redacted])")
    }
}

impl Deref for Password {
    type Target = Vec<u8>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Password {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Request access to a protected member. The provider checks the credential
/// against that member and the host handles prompts, retries and retention.
pub trait PasswordAccess {
    fn check_cancelled(&self) -> Result<()> {
        Ok(())
    }

    /// An empty member path requests the password for the archive header.
    fn unlock(
        &self,
        member: &Path,
        verify: &mut dyn FnMut(&[u8]) -> Result<bool>,
    ) -> Result<Password>;
}

/// Convert a validated relative member path back to an archive entry name.
pub fn member_name(path: &Path) -> Result<Cow<'_, str>> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid archive member path"))?;
    if cfg!(windows) {
        Ok(path.replace('\\', "/").into())
    } else {
        Ok(path.into())
    }
}

/// The host validates paths and supplies streams for regular files. Directories
/// have no stream. The provider calls the stream callback once per file and
/// reads it to EOF before returning from the callback.
pub struct Input<'a> {
    pub path: PathBuf,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub unix_mode: Option<u32>,
    pub contents: Option<Box<ReadContents<'a>>>,
}

pub type ReadContents<'a> = dyn Fn(&mut dyn FnMut(&mut dyn Read) -> Result<()>) -> Result<()> + 'a;

/// Remove entire subtrees, then write the supplied entries. A file supplied
/// at an existing path replaces it. Other entries and their metadata survive.
pub struct Changes<'a> {
    pub remove: &'a [PathBuf],
    pub write: &'a [Input<'a>],
}

pub trait WriteSeek: Write + Seek {}
impl<T: Write + Seek + ?Sized> WriteSeek for T {}

/// A mounted snapshot. Implementations should give concurrent calls independent cursors.
pub trait ArchiveReader: Debug + Send + Sync {
    fn visit_entries(&self, visit: &mut dyn FnMut(Entry) -> Result<()>) -> Result<()>;

    /// Call read exactly once with a regular member stream. The host owns size
    /// limits, cancellation, and extraction destinations.
    fn with_member(
        &self,
        path: &Path,
        passwords: &dyn PasswordAccess,
        read: &mut dyn FnMut(&mut dyn Read) -> Result<()>,
    ) -> Result<()>;

    /// Provide direct random access when possible. Returning None lets the host
    /// read the entry into its bounded memory buffer.
    fn seekable_member(&self, _path: &Path) -> Result<Option<VirtualFile>> {
        Ok(None)
    }

    /// Write a complete archive with the requested changes. The host decides how
    /// to stage the new archive and when to publish it.
    fn rewrite(
        &self,
        _changes: Changes<'_>,
        _passwords: &dyn PasswordAccess,
        _output: &mut dyn WriteSeek,
    ) -> Result<()> {
        bail!("This archive format is read-only")
    }
}

pub struct Entry {
    /// Relative UTF-8 entry name with forward slashes. The host validates it
    /// before exposing it or using it as an extraction path.
    pub path: String,
    pub directory: bool,
    pub symlink: bool,
    pub encrypted: bool,
    pub size: u64,
    /// Last modification time in milliseconds since the Unix epoch.
    pub modified: f64,
}

trait ReadSeek: Read + Seek + Send {}
impl<T: Read + Seek + Send> ReadSeek for T {}

#[derive(Clone)]
enum FileBacking {
    Local(Arc<Mutex<BufReader<File>>>),
    Memory(Arc<Zeroizing<Vec<u8>>>),
    Stream(Arc<Mutex<Box<dyn ReadSeek>>>),
}

/// An independent cursor over an entire file or a bounded subrange of it.
#[derive(Clone)]
pub struct VirtualFile {
    backing: FileBacking,
    start: u64,
    length: u64,
    position: u64,
    protected: bool,
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
    pub fn local(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        ensure!(metadata.is_file(), "Expected a regular file");
        Ok(Self {
            backing: FileBacking::Local(Arc::new(Mutex::new(BufReader::new(file)))),
            start: 0,
            length: metadata.len(),
            position: 0,
            protected: false,
        })
    }

    pub fn memory(data: Vec<u8>) -> Self {
        Self {
            length: data.len() as u64,
            backing: FileBacking::Memory(Arc::new(Zeroizing::new(data))),
            start: 0,
            position: 0,
            protected: false,
        }
    }

    pub fn streaming(reader: impl Read + Seek + Send + 'static, length: u64) -> Self {
        Self {
            backing: FileBacking::Stream(Arc::new(Mutex::new(Box::new(reader)))),
            start: 0,
            length,
            position: 0,
            protected: false,
        }
    }

    pub fn slice(self, start: u64, length: u64) -> Result<Self> {
        ensure!(
            start <= self.length && length <= self.length - start,
            "File slice extends beyond its source"
        );
        Ok(Self {
            backing: self.backing,
            start: self.start + start,
            length,
            position: 0,
            protected: self.protected,
        })
    }

    pub fn len(&self) -> u64 {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn mark_protected(mut self) -> Self {
        self.protected = true;
        self
    }

    pub fn is_protected(&self) -> bool {
        self.protected
    }

    pub fn checked(self, access: &dyn PasswordAccess) -> CheckedFile<'_> {
        CheckedFile { file: self, access }
    }

    pub fn memory_usage(&self) -> u64 {
        match &self.backing {
            FileBacking::Local(_) | FileBacking::Stream(_) => 0,
            FileBacking::Memory(data) => data.len() as u64,
        }
    }
}

/// A virtual file cursor that lets long decodes stop when the host cancels.
pub struct CheckedFile<'a> {
    file: VirtualFile,
    access: &'a dyn PasswordAccess,
}

impl Read for CheckedFile<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.access.check_cancelled().map_err(io::Error::other)?;
        self.file.read(output)
    }
}

impl Seek for CheckedFile<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.access.check_cancelled().map_err(io::Error::other)?;
        self.file.seek(position)
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
            FileBacking::Stream(reader) => {
                let mut reader = reader
                    .lock()
                    .map_err(|_| io::Error::other("File reader is unavailable"))?;
                if reader.stream_position()? != offset {
                    reader.seek(SeekFrom::Start(offset))?;
                }
                reader.read(output)?
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
