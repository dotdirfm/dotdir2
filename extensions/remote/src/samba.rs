use crate::Backend as _;
use crate::{
    Entry, NativeTransfer, ObjectVersion, Profile, RUNTIME, RemoteFile, Upload, component, run,
};
use anyhow::{Context, Result, ensure};
use dotdir_core::copy::CancelToken;
use futures::{StreamExt, pin_mut};
use smb::{
    Client, ClientConfig, ConnectionConfig, CreateDisposition, CreateOptions, Directory, File,
    FileAccessMask, FileAttributes, FileCreateArgs, FileDirectoryInformation,
    FileDispositionInformation, FileRenameInformation, GetLen, ReadAt, Resource, ResourceHandle,
    UncPath, WriteAt,
};
use std::{
    io::{self, Read, Seek, SeekFrom},
    sync::Arc,
};
use url::Url;

pub(crate) struct Backend {
    session: Arc<Session>,
    share: UncPath,
    root: String,
}

impl Backend {
    pub fn connect(profile: &Profile, secret: &str, cancel: &CancelToken) -> Result<Self> {
        let address = Url::parse(&profile.endpoint)?;
        let share = UncPath::new(address.host_str().context("Enter an SMB host")?)?
            .with_share(&profile.share)?;
        let client = run(cancel, async {
            let client = Client::new(ClientConfig {
                connection: ConnectionConfig {
                    port: address.port(),
                    smb2_only_negotiate: true,
                    ..Default::default()
                },
                ..Default::default()
            });
            client
                .share_connect(&share, &profile.user, secret.to_owned())
                .await?;
            Ok(client)
        })?;
        Ok(Self {
            session: Arc::new(Session(Some(client))),
            share,
            root: profile.root.trim_matches('/').to_string(),
        })
    }

    fn path(&self, path: &str) -> UncPath {
        let relative = [self.root.as_str(), path]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\\");
        self.share.clone().with_path(&relative.replace('/', "\\"))
    }

    fn stage(&self, path: &str, readable: bool, cancel: &CancelToken) -> Result<File> {
        let staging = crate::staging_path(path);
        run(cancel, async {
            let args = FileCreateArgs {
                disposition: CreateDisposition::Create,
                attributes: FileAttributes::new(),
                options: CreateOptions::new().with_non_directory_file(true),
                desired_access: FileAccessMask::new()
                    .with_generic_read(readable)
                    .with_file_read_data(readable)
                    .with_generic_write(true)
                    .with_file_write_data(true)
                    .with_delete(true),
            };
            let resource = self
                .session
                .client()
                .create_file(&self.path(&staging), &args)
                .await?;
            let Resource::File(file) = resource else {
                anyhow::bail!("Expected an SMB staging file");
            };
            Ok(file)
        })
    }

    fn finish_stage(
        &self,
        file: File,
        path: &str,
        previous: Option<&ObjectVersion>,
        cancel: &CancelToken,
        staged: Result<bool>,
    ) -> Result<bool> {
        let published = staged.and_then(|ready| {
            if !ready {
                return Ok(false);
            }
            ensure!(
                self.upload_version(path, cancel)?.as_ref() == previous,
                "SMB destination changed before publishing"
            );
            run(cancel, async {
                file.flush().await?;
                self.rename(&file, path, previous.is_some()).await?;
                Ok(true)
            })
        });
        let close = run(&CancelToken::new(), async move {
            if !matches!(published.as_ref(), Ok(true)) {
                let _ = file.set_info(FileDispositionInformation::default()).await;
            }
            file.close().await?;
            published
        });
        close
    }

    async fn rename(&self, file: &ResourceHandle, to: &str, replace: bool) -> Result<()> {
        let path = self.path(to);
        file.set_info(FileRenameInformation {
            replace_if_exists: replace.into(),
            root_directory: 0,
            file_name: path.path().context("Expected SMB destination")?.into(),
        })
        .await?;
        Ok(())
    }
}

fn handle(resource: &Resource) -> Result<&ResourceHandle> {
    match resource {
        Resource::File(file) => Ok(file),
        Resource::Directory(directory) => Ok(directory),
        Resource::Pipe(_) => anyhow::bail!("Expected an SMB file or directory"),
    }
}

struct Session(Option<Client>);

impl Session {
    fn client(&self) -> &Client {
        self.0.as_ref().expect("active SMB session")
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // The SMB client starts Tokio tasks when its connection handlers are dropped.
        if let (Ok(runtime), Some(client)) = (RUNTIME.as_ref(), self.0.take()) {
            runtime.spawn(async move {
                let _ = client.close().await;
            });
        }
    }
}

impl crate::Backend for Backend {
    fn entries(&self, path: &str, cancel: &CancelToken) -> Result<Vec<Entry>> {
        run(cancel, async {
            let args =
                FileCreateArgs::make_open_existing(FileAccessMask::new().with_generic_read(true));
            let resource = self
                .session
                .client()
                .create_file(&self.path(path), &args)
                .await?;
            let Resource::Directory(directory) = resource else {
                anyhow::bail!("Expected a remote directory");
            };
            let directory = Arc::new(directory);
            let result = async {
                let stream = Directory::query::<FileDirectoryInformation>(&directory, "*").await?;
                pin_mut!(stream);
                let mut entries = Vec::new();
                while let Some(entry) = stream.next().await {
                    let entry = entry?;
                    let name = entry.file_name.to_string();
                    if !component(&name) || entry.file_attributes.reparse_point() {
                        continue;
                    }
                    entries.push(Entry {
                        name,
                        directory: entry.file_attributes.directory(),
                        size: entry.end_of_file,
                        modified_ms: (entry
                            .last_write_time
                            .date_time()
                            .assume_utc()
                            .unix_timestamp_nanos()
                            / 1_000_000)
                            .try_into()
                            .unwrap_or_default(),
                    });
                }
                Ok(entries)
            }
            .await;
            let _ = directory.close().await;
            result
        })
    }

    fn open(&self, path: &str, cancel: &CancelToken) -> Result<RemoteFile> {
        run(cancel, async {
            let args =
                FileCreateArgs::make_open_existing(FileAccessMask::new().with_generic_read(true));
            let resource = self
                .session
                .client()
                .create_file(&self.path(path), &args)
                .await?;
            let Resource::File(file) = resource else {
                anyhow::bail!("Expected a remote file");
            };
            let size = file.get_len().await?;
            Ok(RemoteFile {
                size,
                reader: Box::new(Reader {
                    file: Some(file),
                    session: self.session.clone(),
                    cancel: cancel.clone(),
                    size,
                    position: 0,
                }),
            })
        })
    }

    fn upload_version(&self, path: &str, cancel: &CancelToken) -> Result<Option<ObjectVersion>> {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        Ok(self
            .entries(parent, cancel)?
            .into_iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(name))
            .map(|entry| ObjectVersion {
                etag: String::new(),
                size: entry.size,
                modified_ms: entry.modified_ms,
                directory: entry.directory,
            }))
    }

    fn create_dir(&self, path: &str, cancel: &CancelToken) -> Result<()> {
        run(cancel, async {
            let args = FileCreateArgs {
                disposition: CreateDisposition::OpenIf,
                attributes: FileAttributes::new().with_directory(true),
                options: CreateOptions::new().with_directory_file(true),
                desired_access: FileAccessMask::new().with_generic_read(true),
            };
            let resource = self
                .session
                .client()
                .create_file(&self.path(path), &args)
                .await?;
            handle(&resource)?.close().await?;
            Ok(())
        })
    }

    fn remove(&self, path: &str, expected: &ObjectVersion, cancel: &CancelToken) -> Result<()> {
        ensure!(
            self.upload_version(path, cancel)?
                .as_ref()
                .is_some_and(|actual| actual == expected || expected.directory && actual.directory),
            "SMB source changed before removal"
        );
        run(cancel, async {
            let args = FileCreateArgs::make_open_existing(FileAccessMask::new().with_delete(true));
            let resource = self
                .session
                .client()
                .create_file(&self.path(path), &args)
                .await?;
            let handle = handle(&resource)?;
            handle
                .set_info(FileDispositionInformation::default())
                .await?;
            handle.close().await?;
            Ok(())
        })
    }

    fn transfer(
        &self,
        from: &str,
        to: &str,
        op: NativeTransfer<'_>,
        cancel: &CancelToken,
    ) -> Result<bool> {
        if op.source.directory {
            return Ok(false);
        }
        if !op.moving {
            // The client sends one CopyChunk request, so larger files use the streaming path.
            if op.source.size > 16 * 1024 * 1024 {
                return Ok(false);
            }
            ensure!(
                self.upload_version(from, cancel)?.as_ref() == Some(op.source),
                "SMB source changed before copying"
            );
            let file = self.stage(to, true, cancel)?;
            let copied = run(cancel, async {
                let args = FileCreateArgs::make_open_existing(
                    FileAccessMask::new()
                        .with_generic_read(true)
                        .with_file_read_data(true),
                );
                let resource = self
                    .session
                    .client()
                    .create_file(&self.path(from), &args)
                    .await?;
                let Resource::File(source) = resource else {
                    anyhow::bail!("Expected an SMB source file");
                };
                let result = file.srv_copy(&source).await;
                source.close().await?;
                match result {
                    Ok(()) => Ok(true),
                    Err(smb::Error::UnexpectedMessageStatus(status))
                        if matches!(
                            status,
                            smb::Status::U32_NOT_SUPPORTED
                                | smb::Status::U32_INVALID_PARAMETER
                                | smb::Status::U32_INVALID_DEVICE_REQUEST0
                                | smb::Status::U32_ACCESS_DENIED
                        ) =>
                    {
                        Ok(false)
                    }
                    Err(error) => Err(error.into()),
                }
            });
            return self.finish_stage(file, to, op.previous, cancel, copied);
        }
        ensure!(
            self.upload_version(from, cancel)?.as_ref() == Some(op.source),
            "SMB source changed before moving"
        );
        ensure!(
            self.upload_version(to, cancel)?.as_ref() == op.previous,
            "SMB destination changed before moving"
        );
        run(cancel, async {
            let args = FileCreateArgs::make_open_existing(FileAccessMask::new().with_delete(true));
            let resource = self
                .session
                .client()
                .create_file(&self.path(from), &args)
                .await?;
            let handle = handle(&resource)?;
            self.rename(handle, to, op.previous.is_some()).await?;
            handle.close().await?;
            Ok(true)
        })
    }

    fn upload(&self, path: &str, mut upload: Upload<'_>, cancel: &CancelToken) -> Result<()> {
        let file = self.stage(path, false, cancel)?;
        let mut offset = 0;
        let result = crate::pump(&mut upload, cancel, |mut bytes| {
            while !bytes.is_empty() {
                let count = run(cancel, async { Ok(file.write_at(bytes, offset).await?) })?;
                ensure!(count > 0, "SMB upload made no progress");
                offset += count as u64;
                bytes = &bytes[count..];
            }
            Ok(())
        });
        self.finish_stage(file, path, upload.previous, cancel, result.map(|()| true))
            .map(|_| ())
    }
}

struct Reader {
    file: Option<File>,
    session: Arc<Session>,
    cancel: CancelToken,
    size: u64,
    position: u64,
}

impl Read for Reader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = self
            .size
            .saturating_sub(self.position)
            .min(bytes.len() as u64)
            .min(256 * 1024) as usize;
        if count == 0 {
            return Ok(0);
        }
        let read = run(&self.cancel, async {
            Ok(self
                .file
                .as_ref()
                .expect("open SMB file")
                .read_at(&mut bytes[..count], self.position)
                .await?)
        })
        .map_err(io::Error::other)?;
        self.position += read as u64;
        Ok(read)
    }
}

impl Seek for Reader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.position = match position {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::Current(offset) => self.position.checked_add_signed(offset),
            SeekFrom::End(offset) => self.size.checked_add_signed(offset),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid seek position"))?;
        Ok(self.position)
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        if let (Ok(runtime), Some(file)) = (RUNTIME.as_ref(), self.file.take()) {
            let session = self.session.clone();
            runtime.spawn(async move {
                let _ =
                    tokio::time::timeout(std::time::Duration::from_secs(30), file.close()).await;
                drop(file);
                drop(session);
            });
        }
    }
}
