//! Bundled native provider for 7z archives.

use anyhow::{Context, Result, ensure};
use dotdir_archive_api::{
    ArchiveFormat, ArchiveReader, CheckedFile, Entry, FormatInfo, PasswordAccess, VirtualFile,
    member_name,
};
use sevenz_rust2::{
    Archive, ArchiveEntry, BlockDecoder, EncoderMethod, Error, NtTime, Password,
    SIGNATURE_HEADER_SIZE,
};
use std::{
    collections::BTreeMap,
    io::{self, Read},
    path::Path,
};

const DECODER_LIMIT_KIB: usize = 128 * 1024;

pub static SEVEN_ZIP: SevenZipFormat = SevenZipFormat;

#[derive(Debug)]
pub struct SevenZipFormat;

impl ArchiveFormat for SevenZipFormat {
    fn info(&self) -> &'static FormatInfo {
        static INFO: FormatInfo = FormatInfo {
            id: "dotdir.7z",
            name: "7-Zip Archives",
            version: env!("CARGO_PKG_VERSION"),
            suffixes: &["7z"],
            writable: false,
        };
        &INFO
    }

    fn open(
        &self,
        source: VirtualFile,
        access: &dyn PasswordAccess,
    ) -> Result<Box<dyn ArchiveReader>> {
        let parsed = match parse(&source, &Password::empty(), access) {
            Err(Error::PasswordRequired) => {
                let mut parsed = None;
                access.unlock(Path::new(""), &mut |password| match parse(
                    &source,
                    &decode_password(password)?,
                    access,
                ) {
                    Ok(archive) => {
                        parsed = Some(archive);
                        Ok(true)
                    }
                    Err(error) => check_password_error(error, access),
                })?;
                parsed.context("7z header could not be unlocked")?
            }
            result => result?,
        };
        let mut members = BTreeMap::new();
        for (index, entry) in parsed.files.iter().enumerate() {
            if entry.is_anti_item || entry_kind(entry).is_none() {
                continue;
            }
            let name = entry.name.replace('\\', "/");
            let path = name.trim_start_matches("./").trim_end_matches('/');
            if path.is_empty() || path == "." {
                continue;
            }
            ensure!(
                members.insert(path.to_owned(), index).is_none(),
                "Duplicate 7z entry: {path}"
            );
        }
        Ok(Box::new(SevenZipReader {
            source,
            archive: parsed,
            members,
        }))
    }
}

fn parse(
    source: &VirtualFile,
    password: &Password,
    access: &dyn PasswordAccess,
) -> Result<Archive, Error> {
    Archive::read_with_memory_limit(
        &mut source.clone().checked(access),
        password,
        DECODER_LIMIT_KIB,
    )
}

fn decode_password(password: &[u8]) -> Result<Password> {
    Ok(Password::new(std::str::from_utf8(password)?))
}

fn check_password_error(error: Error, access: &dyn PasswordAccess) -> Result<bool> {
    access.check_cancelled()?;
    match error {
        Error::PasswordRequired
        | Error::MaybeBadPassword(_)
        | Error::ChecksumVerificationFailed => Ok(false),
        error => Err(error.into()),
    }
}

#[derive(Debug)]
struct SevenZipReader {
    source: VirtualFile,
    archive: Archive,
    members: BTreeMap<String, usize>,
}

impl SevenZipReader {
    fn regular(&self, path: &Path) -> Result<(usize, &ArchiveEntry)> {
        let name = member_name(path)?;
        let index = *self
            .members
            .get(name.as_ref())
            .context("7z entry is missing")?;
        let file = &self.archive.files[index];
        ensure!(
            entry_kind(file) == Some(Kind::File),
            "7z entry is not a regular file"
        );
        Ok((index, file))
    }

    fn block(&self, index: usize) -> Option<usize> {
        self.archive
            .stream_map
            .file_block_index
            .get(index)
            .copied()
            .flatten()
    }

    fn decoder<'a, 'access>(
        &'a self,
        block: usize,
        password: &'a Password,
        source: &'a mut CheckedFile<'access>,
    ) -> BlockDecoder<'a, CheckedFile<'access>> {
        BlockDecoder::new_with_memory_limit(
            1,
            DECODER_LIMIT_KIB,
            block,
            &self.archive,
            password,
            source,
        )
    }

    fn verify_password(
        &self,
        block: usize,
        file: &ArchiveEntry,
        password: &[u8],
        access: &dyn PasswordAccess,
    ) -> Result<bool> {
        let password = decode_password(password)?;
        let mut source = self.source.clone().checked(access);
        let mut verified_member = false;
        let result = self
            .decoder(block, &password, &mut source)
            .for_each_entries(&mut |entry, _| {
                if verified_member && file.has_crc {
                    return Ok(false);
                }
                verified_member |= std::ptr::eq(entry, file);
                Ok(true)
            });
        match result {
            Ok(_) => Ok(verified_member),
            Err(error) => check_password_error(error, access),
        }
    }
}

impl ArchiveReader for SevenZipReader {
    fn visit_entries(&self, visit: &mut dyn FnMut(Entry) -> Result<()>) -> Result<()> {
        for (path, &index) in &self.members {
            let entry = &self.archive.files[index];
            let kind = entry_kind(entry).unwrap();
            let modified = if entry.has_last_modified_date {
                u64::from(entry.last_modified_date).saturating_sub(u64::from(NtTime::UNIX_EPOCH))
                    as f64
                    / 10_000.
            } else {
                0.
            };
            visit(Entry {
                path: path.clone(),
                directory: kind == Kind::Directory,
                symlink: kind == Kind::Link,
                encrypted: self.archive.is_file_encrypted(index),
                size: entry.size,
                modified,
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
        let (index, file) = self.regular(path)?;
        if !file.has_stream {
            ensure!(file.size == 0, "Missing 7z data stream");
            return read(&mut io::empty());
        }
        let block = self.block(index).context("Missing 7z compression block")?;
        let password = if self.archive.is_file_encrypted(index) {
            let secret = access.unlock(path, &mut |password| {
                self.verify_password(block, file, password, access)
            })?;
            decode_password(&secret)?
        } else {
            Password::empty()
        };
        let mut source = self.source.clone().checked(access);
        let mut output = None;
        self.decoder(block, &password, &mut source)
            .for_each_entries(&mut |entry, stream| {
                if std::ptr::eq(entry, file) {
                    output = Some(read(stream));
                    return Ok(false);
                }
                Ok(true)
            })?;
        output.context("7z entry is missing from its compression block")?
    }

    fn seekable_member(&self, path: &Path) -> Result<Option<VirtualFile>> {
        let (index, file) = self.regular(path)?;
        let Some(block_index) = self.block(index) else {
            return Ok(None);
        };
        let block = self
            .archive
            .blocks
            .get(block_index)
            .context("Missing 7z compression block")?;
        if !file.has_stream
            || block.coders.len() != 1
            || block.coders[0].encoder_method_id() != EncoderMethod::ID_COPY
        {
            return Ok(None);
        }
        let map = &self.archive.stream_map;
        let stream = *map
            .block_first_pack_stream_index()
            .get(block_index)
            .context("Missing 7z packed stream")?;
        let packed_size = *self
            .archive
            .pack_sizes()
            .get(stream)
            .context("Missing 7z packed stream size")?;
        ensure!(
            packed_size == block.get_unpack_size_at_index(0),
            "7z stored block has inconsistent sizes"
        );
        let first = *map
            .block_first_file_index
            .get(block_index)
            .context("Missing first 7z member in block")?;
        let prefix = self
            .archive
            .files
            .get(first..index)
            .context("Invalid 7z member range")?
            .iter()
            .try_fold(0u64, |sum, entry| sum.checked_add(entry.size))
            .context("Invalid 7z member offset")?;
        ensure!(
            prefix <= packed_size && file.size <= packed_size - prefix,
            "7z entry extends beyond its block"
        );
        let stream_offset = *map
            .pack_stream_offsets()
            .get(stream)
            .context("Missing 7z packed stream offset")?;
        let offset = SIGNATURE_HEADER_SIZE
            .checked_add(self.archive.pack_pos())
            .and_then(|start| start.checked_add(stream_offset))
            .and_then(|start| start.checked_add(prefix))
            .context("Invalid 7z member offset")?;
        Ok(Some(self.source.clone().slice(offset, file.size)?))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Directory,
    Link,
}

fn entry_kind(entry: &ArchiveEntry) -> Option<Kind> {
    let attrs = if entry.has_windows_attributes {
        entry.windows_attributes
    } else {
        0
    };
    let unix_kind = if attrs & 0x8000 != 0 {
        (attrs >> 16) & 0o170000
    } else {
        0
    };
    match unix_kind {
        0o120000 => Some(Kind::Link),
        0 | 0o040000 | 0o100000 if attrs & 0x400 != 0 => Some(Kind::Link),
        0 | 0o040000 | 0o100000 => Some(if entry.is_directory {
            Kind::Directory
        } else {
            Kind::File
        }),
        _ => None,
    }
}
