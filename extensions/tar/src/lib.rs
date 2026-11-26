//! Bundled native provider for TAR archives and compressed tarballs.

use anyhow::{Context, Result, bail, ensure};
use dotdir_archive_api::{
    ArchiveFormat, ArchiveReader, Entry, FormatInfo, PasswordAccess, VirtualFile, member_name,
};
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

pub static TAR: TarFormat = TarFormat;

#[derive(Debug)]
pub struct TarFormat;

impl ArchiveFormat for TarFormat {
    fn info(&self) -> &'static FormatInfo {
        static INFO: FormatInfo = FormatInfo {
            id: "dotdir.tar",
            name: "TAR Archives",
            version: env!("CARGO_PKG_VERSION"),
            suffixes: &[
                "tar", "tar.gz", "tgz", "tar.bz2", "tbz2", "tbz", "tar.xz", "txz", "tar.zst",
                "tzst",
            ],
            writable: false,
        };
        &INFO
    }

    fn open(
        &self,
        source: VirtualFile,
        access: &dyn PasswordAccess,
    ) -> Result<Box<dyn ArchiveReader>> {
        let compression = Compression::detect(source.clone())?;
        let members = if matches!(compression, Compression::None) {
            let mut archive = tar::Archive::new(source.clone());
            scan(archive.entries_with_seek()?, Some(source.len()), access)?
        } else {
            let mut archive = tar::Archive::new(compression.reader(source.clone(), access)?);
            scan(archive.entries()?, None, access)?
        };
        Ok(Box::new(TarReader {
            source,
            compression,
            members,
        }))
    }
}

#[derive(Clone, Copy, Debug)]
enum Compression {
    None,
    Gzip,
    Bzip2,
    Xz,
    Zstd,
}

impl Compression {
    fn detect(mut source: VirtualFile) -> Result<Self> {
        source.seek(SeekFrom::Start(0))?;
        let mut magic = [0; 6];
        let length = source.len().min(magic.len() as u64) as usize;
        source.read_exact(&mut magic[..length])?;
        Ok(match &magic[..length] {
            [0x1f, 0x8b, ..] => Self::Gzip,
            [b'B', b'Z', b'h', b'1'..=b'9', 0x31, 0x41]
            | [b'B', b'Z', b'h', b'1'..=b'9', 0x17, 0x72] => Self::Bzip2,
            [0xfd, b'7', b'z', b'X', b'Z', 0] => Self::Xz,
            [0x28, 0xb5, 0x2f, 0xfd, ..] | [0x50..=0x5f, 0x2a, 0x4d, 0x18, ..] => Self::Zstd,
            _ => Self::None,
        })
    }

    fn reader<'a>(
        self,
        source: VirtualFile,
        access: &'a dyn PasswordAccess,
    ) -> Result<Box<dyn Read + 'a>> {
        let source = source.checked(access);
        Ok(match self {
            Self::None => Box::new(source),
            Self::Gzip => Box::new(flate2::read::MultiGzDecoder::new(source)),
            Self::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(source)),
            Self::Xz => Box::new(lzma_rust2::XzReader::new_mem_limit(
                source,
                true,
                128 * 1024,
            )),
            Self::Zstd => {
                let mut decoder = zstd::stream::read::Decoder::new(source)?;
                decoder.window_log_max(27)?;
                Box::new(decoder)
            }
        })
    }
}

#[derive(Debug)]
struct TarReader {
    source: VirtualFile,
    compression: Compression,
    members: BTreeMap<String, TarMember>,
}

#[derive(Debug)]
struct TarMember {
    ordinal: usize,
    offset: Option<u64>,
    size: u64,
    directory: bool,
    symlink: bool,
    modified: f64,
}

impl TarReader {
    fn regular(&self, path: &Path) -> Result<&TarMember> {
        let name = member_name(path)?;
        let member = self
            .members
            .get(name.as_ref())
            .context("TAR entry is missing")?;
        ensure!(
            !member.directory && !member.symlink,
            "TAR entry is not a regular file"
        );
        Ok(member)
    }
}

impl ArchiveReader for TarReader {
    fn visit_entries(&self, visit: &mut dyn FnMut(Entry) -> Result<()>) -> Result<()> {
        for (path, member) in &self.members {
            visit(Entry {
                path: path.clone(),
                directory: member.directory,
                symlink: member.symlink,
                encrypted: false,
                size: member.size,
                modified: member.modified,
            })?;
        }
        Ok(())
    }

    fn with_member(
        &self,
        path: &Path,
        access: &dyn PasswordAccess,
        read: &mut dyn FnMut(&mut dyn Read) -> Result<()>,
    ) -> Result<()> {
        let member = self.regular(path)?;
        if let Some(offset) = member.offset {
            return read(&mut self.source.clone().slice(offset, member.size)?);
        }
        let mut archive = tar::Archive::new(self.compression.reader(self.source.clone(), access)?);
        for (ordinal, entry) in archive.entries()?.enumerate() {
            access.check_cancelled()?;
            let mut entry = entry?;
            if ordinal == member.ordinal {
                return read(&mut entry);
            }
        }
        bail!("TAR entry is missing")
    }

    fn seekable_member(&self, path: &Path) -> Result<Option<VirtualFile>> {
        let member = self.regular(path)?;
        member
            .offset
            .map(|offset| self.source.clone().slice(offset, member.size))
            .transpose()
    }
}

fn scan<R: Read>(
    entries: tar::Entries<'_, R>,
    source_length: Option<u64>,
    access: &dyn PasswordAccess,
) -> Result<BTreeMap<String, TarMember>> {
    let mut members = BTreeMap::new();
    for (ordinal, entry) in entries.enumerate() {
        access.check_cancelled()?;
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        let directory = kind.is_dir();
        let symlink = kind.is_symlink() || kind.is_hard_link();
        let regular = kind.is_file() || kind.is_gnu_sparse() || kind.is_contiguous();
        if !directory && !symlink && !regular {
            continue;
        }
        if let Some(pax) = entry.pax_extensions()? {
            let mut unsupported_sparse = false;
            for value in pax {
                unsupported_sparse |= value?.key_bytes().starts_with(b"GNU.sparse.");
            }
            if unsupported_sparse {
                continue;
            }
        }
        let raw = entry.path_bytes();
        let Ok(mut path) = std::str::from_utf8(&raw) else {
            continue;
        };
        while let Some(stripped) = path.strip_prefix("./") {
            path = stripped;
        }
        let path = path.trim_end_matches('/');
        if path.is_empty() || path == "." {
            continue;
        }
        let size = entry.size();
        let offset =
            if let Some(length) = source_length.filter(|_| regular && !kind.is_gnu_sparse()) {
                let offset = entry.raw_file_position();
                ensure!(
                    offset <= length && size <= length - offset,
                    "TAR entry extends beyond the archive"
                );
                Some(offset)
            } else {
                None
            };
        // TAR can contain later versions of a path appended to the same archive.
        members.insert(
            path.into(),
            TarMember {
                ordinal,
                offset,
                size,
                directory,
                symlink,
                modified: entry.header().mtime().unwrap_or_default() as f64 * 1000.,
            },
        );
    }
    Ok(members)
}
