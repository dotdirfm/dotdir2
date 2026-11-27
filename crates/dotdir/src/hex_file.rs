use crate::{
    archive::{Location, PREVIEW_LIMIT, SourceStamp, VirtualFile},
    filesystems::Filesystems,
    media::image_format,
};
use anyhow::{Context as _, Result, ensure};
use dotdir_core::copy::CancelToken;
use gpui_kit::ImageFormat;
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{Arc, Mutex},
};

pub const PAGE_BYTES: u64 = 64 * 1024;

pub enum OpenedFile {
    Text(String),
    Hex(HexFile),
    Image(ImageFormat, Vec<u8>),
}

pub struct HexFile {
    pub source: ByteSource,
    pub stamp: Option<SourceStamp>,
}

#[derive(Clone)]
pub enum ByteSource {
    Local { file: Arc<Mutex<File>>, len: u64 },
    Virtual(VirtualFile),
}

impl ByteSource {
    pub fn len(&self) -> u64 {
        match self {
            Self::Local { len, .. } => *len,
            Self::Virtual(file) => file.len(),
        }
    }

    pub fn read(&self, offset: u64, count: u64) -> Result<Vec<u8>> {
        let count = self.len().saturating_sub(offset).min(count);
        let mut bytes = vec![0; usize::try_from(count)?];
        if bytes.is_empty() {
            return Ok(bytes);
        }
        fn read_at(input: &mut (impl Read + Seek), offset: u64, bytes: &mut [u8]) -> Result<()> {
            input.seek(SeekFrom::Start(offset))?;
            input.read_exact(bytes)?;
            Ok(())
        }
        match self {
            Self::Local { file, .. } => read_at(&mut *file.lock().unwrap(), offset, &mut bytes)?,
            Self::Virtual(file) => read_at(&mut file.clone(), offset, &mut bytes)?,
        }
        Ok(bytes)
    }
}

fn is_binary(bytes: &[u8], complete: bool) -> bool {
    bytes.iter().any(|&byte| byte == 0)
        || std::str::from_utf8(bytes).is_err_and(|error| complete || error.error_len().is_some())
}

pub fn open(
    path: &Location,
    readonly: bool,
    force_bytes: bool,
    filesystems: &Filesystems,
) -> Result<OpenedFile> {
    if !force_bytes
        && readonly
        && path.local().is_none()
        && let Some(format) = image_format(path)
    {
        return Ok(OpenedFile::Image(format, path.read_preview(filesystems)?));
    }
    let file = if let Some(path) = path.local() {
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        ensure!(metadata.is_file(), "Expected a regular file");
        HexFile {
            stamp: Some(SourceStamp::from_file(&file)?),
            source: ByteSource::Local {
                file: Arc::new(Mutex::new(file)),
                len: metadata.len(),
            },
        }
    } else {
        HexFile {
            stamp: None,
            source: ByteSource::Virtual(path.open_seekable(filesystems, &CancelToken::new())?),
        }
    };
    let len = file.source.len();
    if force_bytes {
        return Ok(OpenedFile::Hex(file));
    }
    let sample = file.source.read(0, PAGE_BYTES)?;
    let known_binary = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            ["bin", "exe", "dll", "so", "dylib", "o", "obj"]
                .iter()
                .any(|name| name.eq_ignore_ascii_case(ext))
        });
    if known_binary || is_binary(&sample, len <= PAGE_BYTES) || (readonly && len > PREVIEW_LIMIT) {
        return Ok(OpenedFile::Hex(file));
    }
    if len > PREVIEW_LIMIT {
        // A binary file can begin with a long ASCII header. A sample taken at
        // an arbitrary offset may start or end inside a UTF-8 code point.
        for offset in [len / 2, len - PAGE_BYTES] {
            let sample = file.source.read(offset, PAGE_BYTES)?;
            let continuation = sample
                .iter()
                .take(3)
                .take_while(|&&byte| byte & 0xc0 == 0x80)
                .count();
            if is_binary(&sample[continuation..], false) {
                return Ok(OpenedFile::Hex(file));
            }
        }
        anyhow::bail!("File exceeds the 16 MiB text editor limit. Use Edit as Hex to edit bytes.");
    }
    let bytes = if len <= PAGE_BYTES {
        sample
    } else {
        file.source.read(0, len)?
    };
    if is_binary(&bytes, true) {
        Ok(OpenedFile::Hex(file))
    } else {
        Ok(OpenedFile::Text(String::from_utf8(bytes)?))
    }
}

#[derive(Clone, Copy)]
pub struct ByteEdit {
    pub before: u8,
    pub after: u8,
}

struct EditRun {
    offset: u64,
    before: Vec<u8>,
    after: Vec<u8>,
}

impl EditRun {
    fn contiguous(edits: &BTreeMap<u64, ByteEdit>) -> Vec<Self> {
        let mut runs: Vec<Self> = Vec::new();
        for (&offset, edit) in edits {
            if let Some(run) = runs.last_mut()
                && offset - run.offset == run.after.len() as u64
            {
                run.before.push(edit.before);
                run.after.push(edit.after);
            } else {
                runs.push(Self {
                    offset,
                    before: vec![edit.before],
                    after: vec![edit.after],
                });
            }
        }
        runs
    }
}

pub fn save(
    path: &Path,
    stamp: &SourceStamp,
    edits: &BTreeMap<u64, ByteEdit>,
) -> Result<SourceStamp> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1); // FILE_SHARE_READ
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    file.try_lock()
        .map_err(|error| anyhow::anyhow!("Could not lock the file for saving: {error}"))?;
    let check = |file: &File| -> Result<()> {
        ensure!(
            SourceStamp::from_file(file)? == *stamp,
            "File changed on disk. Your edits are still open."
        );
        Ok(())
    };
    check(&file)?;
    let len = file.metadata()?.len();
    let runs = EditRun::contiguous(edits);
    let mut original = Vec::new();
    for run in &runs {
        ensure!(
            run.offset <= len && run.before.len() as u64 <= len - run.offset,
            "Edit extends beyond the end of the file"
        );
        original.resize(run.before.len(), 0);
        file.seek(SeekFrom::Start(run.offset))?;
        file.read_exact(&mut original)?;
        ensure!(
            original == run.before,
            "File changed on disk. Your edits are still open."
        );
    }
    stamp.check(path)?;
    let mut written = 0;
    let result = (|| -> Result<()> {
        for run in &runs {
            file.seek(SeekFrom::Start(run.offset))?;
            written += 1; // write_all can fail after a partial write
            file.write_all(&run.after)?;
        }
        file.sync_data()?;
        Ok(())
    })();
    if let Err(error) = result {
        let rollback = (|| -> Result<()> {
            for run in &runs[..written] {
                file.seek(SeekFrom::Start(run.offset))?;
                file.write_all(&run.before)?;
            }
            file.sync_data()?;
            Ok(())
        })();
        if let Err(rollback) = rollback {
            return Err(error.context(format!(
                "Save failed and could not restore the affected bytes: {rollback}"
            )));
        }
        return Err(error.context("Save failed. The affected bytes were restored."));
    }
    // Windows may finalize the modification time when the last write handle closes.
    drop(file);
    let saved = SourceStamp::from_path(path)
        .context("Bytes were saved, but the file could not be reopened")?;
    ensure!(
        stamp.same_file(&saved),
        "The file was replaced while saving. Your edits are still open."
    );
    Ok(saved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::SeekFrom;

    #[test]
    fn reads_and_saves_small_ranges_above_four_gib() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.bin");
        let offset = 5 * 1024 * 1024 * 1024 + 7;
        let len = offset + 3;
        let mut file = File::create(&path).unwrap();
        // On Windows, mark the fixture sparse before extending it.
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            unsafe extern "system" {
                fn DeviceIoControl(
                    handle: *mut std::ffi::c_void,
                    control: u32,
                    input: *mut std::ffi::c_void,
                    input_len: u32,
                    output: *mut std::ffi::c_void,
                    output_len: u32,
                    written: *mut u32,
                    overlapped: *mut std::ffi::c_void,
                ) -> i32;
            }
            let null = std::ptr::null_mut();
            let mut written = 0;
            let ok = unsafe {
                DeviceIoControl(
                    file.as_raw_handle(),
                    0x900c4,
                    null,
                    0,
                    null,
                    0,
                    &mut written,
                    null,
                )
            };
            assert_ne!(ok, 0, "{}", std::io::Error::last_os_error());
        }
        file.set_len(len).unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(&[0xab, 0xcd, 0xef]).unwrap();
        drop(file);
        let OpenedFile::Hex(hex) = open(
            &Location::from(path.clone()),
            false,
            false,
            &Filesystems::new(Default::default()),
        )
        .unwrap() else {
            panic!("expected hex");
        };
        assert_eq!(hex.source.len(), len);
        assert_eq!(hex.source.read(offset, 32).unwrap(), [0xab, 0xcd, 0xef]);
        let edits = BTreeMap::from([
            (
                offset,
                ByteEdit {
                    before: 0xab,
                    after: 0x42,
                },
            ),
            (
                offset + 1,
                ByteEdit {
                    before: 0xcd,
                    after: 0x43,
                },
            ),
        ]);
        let stamp = save(&path, hex.stamp.as_ref().unwrap(), &edits).unwrap();
        assert_eq!(
            hex.source.read(offset - 1, 32).unwrap(),
            [0, 0x42, 0x43, 0xef]
        );
        assert_eq!(std::fs::metadata(&path).unwrap().len(), len);
        let stale = BTreeMap::from([(
            offset + 2,
            ByteEdit {
                before: 0,
                after: 0xee,
            },
        )]);
        assert!(save(&path, &stamp, &stale).is_err());
        assert_eq!(hex.source.read(offset, 32).unwrap(), [0x42, 0x43, 0xef]);
        assert!(save(&path, hex.stamp.as_ref().unwrap(), &edits).is_err());
        let second = BTreeMap::from([(
            offset + 2,
            ByteEdit {
                before: 0xef,
                after: 0xee,
            },
        )]);
        save(&path, &stamp, &second).unwrap();
        assert_eq!(hex.source.read(offset, 32).unwrap(), [0x42, 0x43, 0xee]);
    }

    #[test]
    fn text_detection_handles_nuls_and_split_utf8() {
        assert!(!is_binary(b"hello\n", true));
        assert!(is_binary(b"hello\0", true));
        assert!(!is_binary(b"hello\xe2\x82", false));
        assert!(is_binary(b"hello\xe2\x82", true));
        assert!(is_binary(b"hello\xff", false));
    }
}
