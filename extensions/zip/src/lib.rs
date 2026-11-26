//! Bundled native format provider for ZIP archives.

use anyhow::{Context, Result, bail, ensure};
use chrono::{Datelike, Timelike};
use dotdir_archive_api::{
    ArchiveFormat, ArchiveReader, Changes, Choice, Creation, CreationOptions, Entry, FormatInfo,
    Input, OptionField, OptionKind, OptionValue, Password, PasswordAccess, VirtualFile, WriteSeek,
    member_name,
};
use std::{
    collections::BTreeMap,
    io::{self, Read},
    path::{Path, PathBuf},
};
use zip::{
    AesMode, CompressionMethod, DateTime, ZipArchive, ZipWriter,
    result::ZipError,
    unstable::write::FileOptionsExt,
    write::{FileOptions, SimpleFileOptions},
};

pub static ZIP: ZipFormat = ZipFormat;

#[derive(Debug)]
pub struct ZipFormat;

impl ArchiveFormat for ZipFormat {
    fn info(&self) -> &'static FormatInfo {
        static INFO: FormatInfo = FormatInfo {
            id: "dotdir.zip",
            name: "ZIP Archives",
            version: env!("CARGO_PKG_VERSION"),
            suffixes: &["zip"],
            writable: true,
        };
        &INFO
    }

    fn open(
        &self,
        source: VirtualFile,
        _access: &dyn PasswordAccess,
    ) -> Result<Box<dyn ArchiveReader>> {
        Ok(Box::new(ZipReader(ZipArchive::new(source)?)))
    }

    fn creation(&self) -> Option<&'static Creation> {
        Some(&Creation {
            name: "ZIP",
            suffix: "zip",
            fields: &[
                OptionField {
                    id: "compression",
                    label: "Compression",
                    default: "normal",
                    kind: OptionKind::Choice(&[
                        Choice {
                            value: "store",
                            label: "Store",
                            suffix: None,
                        },
                        Choice {
                            value: "fast",
                            label: "Fast",
                            suffix: None,
                        },
                        Choice {
                            value: "normal",
                            label: "Normal",
                            suffix: None,
                        },
                        Choice {
                            value: "maximum",
                            label: "Maximum",
                            suffix: None,
                        },
                    ]),
                },
                OptionField {
                    id: "password",
                    label: "Password (AES-256, optional)",
                    default: "",
                    kind: OptionKind::Secret,
                },
            ],
        })
    }

    fn create(
        &self,
        entries: &[Input<'_>],
        options: &CreationOptions,
        output: &mut dyn WriteSeek,
    ) -> Result<()> {
        let (method, level) = match options.get("compression").and_then(OptionValue::as_text) {
            Some("store") => (CompressionMethod::Stored, None),
            Some("fast") => (CompressionMethod::Deflated, Some(1)),
            None | Some("normal") => (CompressionMethod::Deflated, Some(6)),
            Some("maximum") => (CompressionMethod::Deflated, Some(9)),
            Some(_) => bail!("Unknown ZIP compression setting"),
        };
        let password = options
            .get("password")
            .and_then(OptionValue::as_secret)
            .filter(|value| !value.is_empty());
        let mut writer = ZipWriter::new(output);
        for input in entries {
            let options = SimpleFileOptions::default()
                .compression_method(method)
                .compression_level(level);
            let options = match password {
                Some(password) if input.contents.is_some() => {
                    options.with_aes_encryption_bytes(AesMode::Aes256, password)
                }
                _ => options,
            };
            write_entry(&mut writer, input, options)?;
        }
        writer.finish()?;
        Ok(())
    }
}

#[derive(Debug)]
struct ZipReader(ZipArchive<VirtualFile>);

impl ArchiveReader for ZipReader {
    fn visit_entries(&self, visit: &mut dyn FnMut(Entry) -> Result<()>) -> Result<()> {
        let mut zip = self.0.clone();
        for index in 0..zip.len() {
            let file = zip.by_index_raw(index)?;
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
                    )?
                    .and_local_timezone(chrono::Local)
                    .earliest()
                    .map(|time| time.timestamp_millis() as f64)
                })
                .unwrap_or_default();
            visit(Entry {
                path: file.name()?.into_owned(),
                directory: file.is_dir(),
                symlink: file.is_symlink(),
                encrypted: file.encrypted(),
                size: file.size(),
                modified,
            })?;
        }
        Ok(())
    }

    fn with_member(
        &self,
        path: &Path,
        passwords: &dyn PasswordAccess,
        read: &mut dyn FnMut(&mut dyn Read) -> Result<()>,
    ) -> Result<()> {
        let mut zip = self.0.clone();
        let index = zip
            .index_for_name(&member_name(path)?)
            .context("ZIP entry is missing")?;
        let entry = zip.by_index_data(index)?;
        ensure!(
            entry.is_file() && !entry.is_symlink(),
            "ZIP entry is not a regular file"
        );
        let mut file = if entry.encrypted() {
            let password = unlock_member(&mut zip, index, path, passwords)?;
            zip.by_index_decrypt(index, &password)?
        } else {
            zip.by_index(index)?
        };
        read(&mut file)
    }

    fn seekable_member(&self, path: &Path) -> Result<Option<VirtualFile>> {
        let mut zip = self.0.clone();
        let index = zip
            .index_for_name(&member_name(path)?)
            .context("ZIP entry is missing")?;
        let entry = zip.by_index_raw(index)?;
        ensure!(
            entry.is_file() && !entry.is_symlink(),
            "ZIP entry is not a readable regular file"
        );
        if entry.encrypted() || entry.compression() != CompressionMethod::Stored {
            return Ok(None);
        }
        let size = entry.size();
        ensure!(
            entry.compressed_size() == size,
            "Stored ZIP entry has inconsistent sizes"
        );
        let start = entry
            .data_start()
            .context("Missing ZIP entry data offset")?;
        drop(entry);
        Ok(Some(zip.into_inner().slice(start, size)?))
    }

    fn rewrite(
        &self,
        changes: Changes<'_>,
        passwords: &dyn PasswordAccess,
        output: &mut dyn WriteSeek,
    ) -> Result<()> {
        let mut reader = self.0.clone();
        ensure!(
            reader.offset() == 0 && !reader.has_duplicate_names(),
            "ZIPs with a prepended payload or duplicate entry names cannot be edited"
        );
        let mut writes = changes
            .write
            .iter()
            .map(|input| Ok((member_name(&input.path)?, input)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let mut writer = ZipWriter::new(output);
        writer.set_raw_comment(reader.comment().into())?;
        let mut encryption_template = None;
        for index in 0..reader.len() {
            let entry = reader.by_index_raw(index)?;
            let name = entry.name()?;
            let name = name.trim_end_matches('/');
            let encrypted = entry.encrypted();
            if encrypted && encryption_template.is_none() {
                encryption_template = Some((index, PathBuf::from(name)));
            }
            if changes
                .remove
                .iter()
                .any(|path| Path::new(name).starts_with(path))
            {
                if encrypted {
                    let path = PathBuf::from(name);
                    drop(entry);
                    unlock_member(&mut reader, index, &path, passwords)?;
                }
                continue;
            }
            if let Some(input) = writes.remove(name) {
                ensure!(
                    entry.is_file() && !entry.is_symlink(),
                    "Only regular ZIP files can be edited"
                );
                let options = entry.options();
                drop(entry);
                let encryption = if encrypted {
                    Some(unlock_encryption(
                        &mut reader,
                        index,
                        &input.path,
                        passwords,
                    )?)
                } else {
                    None
                };
                write_entry(
                    &mut writer,
                    input,
                    apply_encryption(options, encryption.as_ref())?,
                )?;
            } else {
                writer.raw_copy_file(entry)?;
            }
        }
        let encryption = match encryption_template {
            Some((index, path)) if writes.values().any(|input| input.contents.is_some()) => {
                Some(unlock_encryption(&mut reader, index, &path, passwords)?)
            }
            _ => None,
        };
        for input in writes.into_values() {
            let encryption = encryption.as_ref().filter(|_| input.contents.is_some());
            write_entry(
                &mut writer,
                input,
                apply_encryption(SimpleFileOptions::default(), encryption)?,
            )?;
        }
        writer.finish()?;
        Ok(())
    }
}

fn unlock_member(
    zip: &mut ZipArchive<VirtualFile>,
    index: usize,
    path: &Path,
    passwords: &dyn PasswordAccess,
) -> Result<Password> {
    passwords.unlock(
        path,
        &mut |password| match zip.by_index_decrypt(index, password) {
            Ok(_) => Ok(true),
            Err(ZipError::InvalidPassword) => Ok(false),
            Err(error) => Err(error.into()),
        },
    )
}

struct Encryption {
    password: Password,
    aes: Option<AesMode>,
}

fn unlock_encryption(
    zip: &mut ZipArchive<VirtualFile>,
    index: usize,
    path: &Path,
    passwords: &dyn PasswordAccess,
) -> Result<Encryption> {
    let password = unlock_member(zip, index, path, passwords)?;
    let aes = zip
        .get_aes_verification_key_and_salt(index)?
        .map(|info| info.aes_mode);
    Ok(Encryption { password, aes })
}

fn apply_encryption<'a>(
    options: FileOptions<'static, 'static, ()>,
    encryption: Option<&'a Encryption>,
) -> Result<FileOptions<'a, 'static, ()>> {
    let Some(encryption) = encryption else {
        return Ok(options);
    };
    Ok(match encryption.aes {
        Some(mode) => options.with_aes_encryption_bytes(mode, &encryption.password),
        None => options.with_deprecated_encryption(&encryption.password)?,
    })
}

fn write_entry(
    writer: &mut ZipWriter<&mut dyn WriteSeek>,
    input: &Input<'_>,
    mut options: FileOptions<'_, '_, ()>,
) -> Result<()> {
    if let Some(mode) = input.unix_mode {
        options = options.unix_permissions(mode);
    }
    if let Some(modified) = input.modified {
        let date: chrono::DateTime<chrono::Local> = modified.into();
        if let Ok(date) = DateTime::from_date_and_time(
            date.year() as u16,
            date.month() as u8,
            date.day() as u8,
            date.hour() as u8,
            date.minute() as u8,
            date.second() as u8,
        ) {
            options = options.last_modified_time(date);
        }
    }
    let name = member_name(&input.path)?;
    if let Some(contents) = &input.contents {
        writer.start_file(name, options.large_file(input.size >= u32::MAX as u64))?;
        contents(&mut |input| Ok(io::copy(input, writer).map(|_| ())?))?;
    } else {
        writer.add_directory(name, options)?;
    }
    Ok(())
}
