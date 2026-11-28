use crate::{Entry, Profile, RUNTIME, RemoteFile, component, run};
use anyhow::{Context, Result};
use dotdir_core::copy::CancelToken;
use futures::{StreamExt, pin_mut};
use smb::{
    Client, ClientConfig, ConnectionConfig, Directory, File, FileAccessMask, FileCreateArgs,
    FileDirectoryInformation, GetLen, ReadAt, Resource, UncPath,
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
