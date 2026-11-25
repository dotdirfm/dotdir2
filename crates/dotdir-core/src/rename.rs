//! Atomic no-replace rename. Never fall back to a check followed by ordinary rename.
use std::{io, path::Path};

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: Both pointers reference valid NUL-terminated strings for this call.
    #[cfg(target_os = "macos")]
    let result =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    // Use the syscall directly to support libc versions without a renameat2 wrapper.
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
pub(crate) fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    fn wide(path: &Path) -> io::Result<Vec<u16>> {
        let mut value: Vec<u16> = path.as_os_str().encode_wide().collect();
        if value.contains(&0) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        value.push(0);
        Ok(value)
    }
    let source = wide(source)?;
    let destination = wide(destination)?;
    // SAFETY: Valid NUL-terminated UTF-16 strings; flags deliberately omit REPLACE_EXISTING.
    let result = unsafe {
        windows_sys::Win32::Storage::FileSystem::MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            0,
        )
    };
    if result != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(crate) fn rename_no_replace(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Atomic no-replace rename is unavailable",
    ))
}

#[cfg(test)]
mod tests {
    use crate::move_op::rename_item;
    use std::{
        fs,
        sync::{Arc, Barrier},
        thread,
    };

    #[test]
    fn rename_preserves_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let dest = dir.path().join("dest");
        fs::write(&source, "source bytes").unwrap();
        fs::write(&dest, "destination bytes").unwrap();
        assert_eq!(
            rename_item(&source, "dest").unwrap_err().errno_str(),
            "EEXIST"
        );
        assert_eq!(fs::read_to_string(source).unwrap(), "source bytes");
        assert_eq!(fs::read_to_string(dest).unwrap(), "destination bytes");
    }

    #[test]
    fn rename_directory_preserves_children() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("child"), "contents").unwrap();
        let dest = rename_item(&source, "dest").unwrap();
        assert!(!source.exists());
        assert_eq!(fs::read_to_string(dest.join("child")).unwrap(), "contents");
    }

    #[test]
    fn rename_competing_sources_have_exactly_one_winner() {
        let dir = tempfile::tempdir().unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|index| {
                let source = dir.path().join(format!("source{index}"));
                fs::write(&source, index.to_string()).unwrap();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    (index, source.clone(), rename_item(&source, "dest"))
                })
            })
            .collect();
        let mut winners = 0;
        for thread in threads {
            let (index, source, result) = thread.join().unwrap();
            match result {
                Ok(dest) => {
                    winners += 1;
                    assert!(!source.exists());
                    assert_eq!(fs::read_to_string(dest).unwrap(), index.to_string());
                }
                Err(error) => {
                    assert_eq!(error.errno_str(), "EEXIST");
                    assert_eq!(fs::read_to_string(source).unwrap(), index.to_string());
                }
            }
        }
        assert_eq!(winners, 1);
    }

    #[test]
    fn rename_racing_file_creation_never_overwrites() {
        use std::io::Write;
        for _ in 0..32 {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("source");
            let dest = dir.path().join("dest");
            fs::write(&source, "renamed").unwrap();
            let barrier = Arc::new(Barrier::new(2));
            let creator = {
                let barrier = barrier.clone();
                let dest = dest.clone();
                thread::spawn(move || {
                    barrier.wait();
                    match fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(dest)
                    {
                        Ok(mut file) => {
                            file.write_all(b"created").unwrap();
                            true
                        }
                        Err(error) => {
                            assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
                            false
                        }
                    }
                })
            };
            barrier.wait();
            let result = rename_item(&source, "dest");
            let created = creator.join().unwrap();
            if created {
                assert_eq!(result.unwrap_err().errno_str(), "EEXIST");
                assert_eq!(fs::read_to_string(&source).unwrap(), "renamed");
                assert_eq!(fs::read_to_string(&dest).unwrap(), "created");
            } else {
                result.unwrap();
                assert!(!source.exists());
                assert_eq!(fs::read_to_string(&dest).unwrap(), "renamed");
            }
        }
    }

    #[test]
    fn rename_does_not_replace_an_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let dest = dir.path().join("dest");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&dest).unwrap();
        assert_eq!(
            rename_item(&source, "dest").unwrap_err().errno_str(),
            "EEXIST"
        );
        assert!(source.is_dir());
        assert!(dest.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn rename_does_not_replace_dangling_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let dest = dir.path().join("dest");
        fs::write(&source, "contents").unwrap();
        std::os::unix::fs::symlink("missing", &dest).unwrap();
        assert_eq!(
            rename_item(&source, "dest").unwrap_err().errno_str(),
            "EEXIST"
        );
        assert_eq!(
            fs::read_link(dest).unwrap(),
            std::path::Path::new("missing")
        );
        assert_eq!(fs::read_to_string(source).unwrap(), "contents");
    }

    #[test]
    fn rename_case_only_preserves_data_whether_supported_or_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("name");
        fs::write(&source, "contents").unwrap();
        match rename_item(&source, "NAME") {
            Ok(dest) => {
                assert_eq!(fs::read_to_string(dest).unwrap(), "contents");
                assert!(fs::read_dir(dir.path())
                    .unwrap()
                    .any(|entry| entry.unwrap().file_name() == "NAME"));
            }
            Err(error) => {
                assert_eq!(error.errno_str(), "EEXIST");
                assert_eq!(fs::read_to_string(source).unwrap(), "contents");
            }
        }
    }
}
