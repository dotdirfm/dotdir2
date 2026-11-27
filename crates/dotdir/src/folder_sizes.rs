use crate::{app::Message, archive::Location, filesystems::Filesystems, model::human_size};
use anyhow::{Context as _, Result, ensure};
use dotdir_core::{copy::CancelToken, watch::ChangeWatch};
use std::{
    collections::VecDeque,
    fs,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    time::{Duration, Instant},
};

// Each exact result can hold a recursive filesystem watch.
const CACHE_LIMIT: usize = 16;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);

pub(crate) enum FolderSizeEvent {
    Progress(u64, u64),
    Changed(u64),
    Finished(u64, Result<(u64, Option<ChangeWatch>)>),
}

struct Scan {
    id: u64,
    path: Location,
    cancel: CancelToken,
    bytes: u64,
    changed: bool,
}

struct Measurement {
    id: u64,
    path: Location,
    bytes: u64,
    stale: bool,
    watch: Option<ChangeWatch>,
}

#[derive(Default)]
pub(crate) struct FolderSizes {
    active: Option<Scan>,
    cache: VecDeque<Measurement>,
}

impl FolderSizes {
    pub(crate) fn start(
        &mut self,
        id: u64,
        path: Location,
        filesystems: Filesystems,
        tx: Sender<Message>,
    ) {
        if self.active.as_ref().is_some_and(|scan| scan.path == path) {
            return;
        }
        if let Some(scan) = self.active.take() {
            scan.cancel.cancel();
        }
        let cancel = CancelToken::new();
        self.active = Some(Scan {
            id,
            path: path.clone(),
            cancel: cancel.clone(),
            bytes: 0,
            changed: false,
        });
        std::thread::spawn(move || {
            if cancel.is_cancelled() {
                return;
            }
            // Start the watch before scanning so changes during the walk are also detected.
            let watch = path.host_path().and_then(|source| {
                let tx = tx.clone();
                let reported = AtomicBool::new(false);
                ChangeWatch::new(source, path.local().is_some(), move || {
                    if !reported.swap(true, Ordering::Relaxed) {
                        let _ = tx.send(Message::FolderSize(FolderSizeEvent::Changed(id)));
                    }
                })
                .ok()
            });
            let mut last_progress = Instant::now();
            let result = measure(&path, &filesystems, &cancel, |bytes| {
                if last_progress.elapsed() >= PROGRESS_INTERVAL {
                    last_progress = Instant::now();
                    let _ = tx.send(Message::FolderSize(FolderSizeEvent::Progress(id, bytes)));
                }
            });
            let _ = tx.send(Message::FolderSize(FolderSizeEvent::Finished(
                id,
                result.map(|bytes| (bytes, watch)),
            )));
        });
    }

    pub(crate) fn cancel_for(&mut self, path: &Location) -> bool {
        let Some(scan) = self.active.as_ref().filter(|scan| &scan.path == path) else {
            return false;
        };
        scan.cancel.cancel();
        self.active = None;
        true
    }

    pub(crate) fn update(&mut self, event: FolderSizeEvent) -> Result<()> {
        match event {
            FolderSizeEvent::Progress(id, bytes) => {
                if let Some(scan) = self.active.as_mut().filter(|scan| scan.id == id) {
                    scan.bytes = bytes;
                }
            }
            FolderSizeEvent::Changed(id) => {
                if let Some(scan) = self.active.as_mut().filter(|scan| scan.id == id) {
                    scan.changed = true;
                }
                if let Some(result) = self.cache.iter_mut().find(|result| result.id == id) {
                    result.stale = true;
                    result.watch = None;
                }
            }
            FolderSizeEvent::Finished(id, result) => {
                if self.active.as_ref().is_some_and(|scan| scan.id == id) {
                    let scan = self.active.take().unwrap();
                    match result {
                        Ok((bytes, watch)) => {
                            self.cache.retain(|result| result.path != scan.path);
                            self.cache.push_front(Measurement {
                                id,
                                path: scan.path,
                                bytes,
                                stale: scan.changed,
                                watch: if scan.changed { None } else { watch },
                            });
                            self.cache.truncate(CACHE_LIMIT);
                        }
                        Err(error) => {
                            if let Some(old) =
                                self.cache.iter_mut().find(|old| old.path == scan.path)
                            {
                                old.stale = true;
                                old.watch = None;
                            }
                            return Err(error)
                                .with_context(|| format!("Scan {}", scan.path.display()));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn invalidate_unwatched(&mut self, matches: impl Fn(&Location) -> bool) {
        for result in &mut self.cache {
            if result.watch.is_none() && matches(&result.path) {
                result.stale = true;
            }
        }
        if let Some(scan) = &mut self.active {
            if scan.path.host_path().is_none() && matches(&scan.path) {
                scan.changed = true;
            }
        }
    }

    pub(crate) fn readout(&self, path: &Location) -> Option<String> {
        if let Some(scan) = self.active.as_ref().filter(|scan| &scan.path == path) {
            return Some(format!("Scanning… · {}", human_size(scan.bytes)));
        }
        self.cache
            .iter()
            .find(|result| &result.path == path)
            .map(|result| {
                let prefix = if result.stale { "~" } else { "" };
                format!("{prefix}{}", human_size(result.bytes))
            })
    }
}

fn measure(
    location: &Location,
    filesystems: &Filesystems,
    cancel: &CancelToken,
    mut progress: impl FnMut(u64),
) -> Result<u64> {
    let mut bytes = 0u64;
    let mut count = |len| -> Result<()> {
        bytes = bytes
            .checked_add(len)
            .context("Folder size exceeds the supported range")?;
        progress(bytes);
        Ok(())
    };
    ensure!(!cancel.is_cancelled(), "Scan cancelled");
    match location {
        Location::Local(path) => scan_local(path, cancel, count)?,
        Location::Archive { archive, member } => {
            let entries = archive.mounted(filesystems, cancel)?.index()?;
            ensure!(
                member.as_os_str().is_empty()
                    || entries.get(member).is_some_and(|entry| entry.directory),
                "Archive directory is missing"
            );
            for (_, entry) in entries
                .range(member.clone()..)
                .take_while(|(path, _)| path.starts_with(member))
            {
                ensure!(!cancel.is_cancelled(), "Scan cancelled");
                if !entry.directory && !entry.symlink {
                    count(entry.size)?;
                }
            }
        }
        Location::Provider(path) => {
            let mut pending = vec![path.clone()];
            while let Some(path) = pending.pop() {
                ensure!(!cancel.is_cancelled(), "Scan cancelled");
                for entry in filesystems.read_directory(&path)? {
                    ensure!(!cancel.is_cancelled(), "Scan cancelled");
                    if entry.directory {
                        pending.push(path.child(entry.name));
                    } else {
                        count(entry.size)?;
                    }
                }
            }
        }
    }
    Ok(bytes)
}

fn scan_local(
    path: &Path,
    cancel: &CancelToken,
    mut count: impl FnMut(u64) -> Result<()>,
) -> Result<()> {
    let mut pending = vec![path.to_path_buf()];
    while let Some(path) = pending.pop() {
        ensure!(!cancel.is_cancelled(), "Scan cancelled");
        for entry in fs::read_dir(&path).with_context(|| format!("Read {}", path.display()))? {
            ensure!(!cancel.is_cancelled(), "Scan cancelled");
            let entry = entry.with_context(|| format!("Read {}", path.display()))?;
            // DirEntry::metadata does not follow symbolic links.
            let metadata = entry
                .metadata()
                .with_context(|| format!("Read {}", entry.path().display()))?;
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                const REPARSE_POINT: u32 = 0x400;
                if metadata.is_dir() && metadata.file_attributes() & REPARSE_POINT != 0 {
                    continue;
                }
            }
            if metadata.is_file() {
                count(metadata.len())?;
            } else if metadata.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_nested_and_hidden_files_without_following_links() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("folder");
        let nested = path.join("nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(path.join(".hidden"), [0; 7]).unwrap();
        fs::write(nested.join("data"), [0; 13]).unwrap();
        #[cfg(unix)]
        {
            let outside = temp.path().join("outside");
            fs::write(&outside, [0; 99]).unwrap();
            std::os::unix::fs::symlink(outside, nested.join("link")).unwrap();
            std::os::unix::fs::symlink(&path, nested.join("cycle")).unwrap();
        }
        let bytes = measure(
            &Location::Local(path),
            &Filesystems::new(Default::default()),
            &CancelToken::new(),
            |_| {},
        )
        .unwrap();
        assert_eq!(bytes, 20);
    }

    #[test]
    fn a_measurement_keeps_its_size_when_stale_and_ignores_old_scan_events() {
        let mut sizes = FolderSizes::default();
        let path = Location::Local("folder".into());
        let new_scan = |id| Scan {
            id,
            path: path.clone(),
            cancel: CancelToken::new(),
            bytes: 0,
            changed: false,
        };
        sizes.active = Some(new_scan(1));
        sizes
            .update(FolderSizeEvent::Finished(1, Ok((20, None))))
            .unwrap();
        assert_eq!(sizes.readout(&path).as_deref(), Some("20"));
        sizes.update(FolderSizeEvent::Changed(1)).unwrap();
        assert_eq!(sizes.readout(&path).as_deref(), Some("~20"));

        sizes.active = Some(new_scan(2));
        sizes
            .update(FolderSizeEvent::Finished(2, Ok((30, None))))
            .unwrap();
        sizes.update(FolderSizeEvent::Changed(1)).unwrap();
        assert_eq!(sizes.readout(&path).as_deref(), Some("30"));
    }
}
