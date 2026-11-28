use crate::{Entry, Profile, RemoteFile, component, run};
use anyhow::{Result, ensure};
use dotdir_core::copy::CancelToken;
use futures::io::{AsyncReadExt, AsyncSeekExt};
use opendal::{FuturesAsyncReader, Operator};
use std::io::{self, Read, Seek, SeekFrom};

pub(crate) struct Backend(Operator);

impl Backend {
    #[cfg(feature = "ftp")]
    pub fn ftp(profile: &Profile, secret: &str) -> Result<Self> {
        let mut ftp = opendal::services::Ftp::default()
            .endpoint(&profile.endpoint)
            .root(&profile.root);
        if !profile.user.is_empty() {
            ftp = ftp.user(&profile.user).password(secret);
        }
        Ok(Self(Operator::new(ftp)?))
    }

    #[cfg(feature = "webdav")]
    pub fn webdav(profile: &Profile, secret: &str) -> Result<Self> {
        let mut dav = opendal::services::Webdav::default()
            .endpoint(&profile.endpoint)
            .root(&profile.root);
        if !profile.user.is_empty() {
            dav = dav.username(&profile.user).password(secret);
        }
        Ok(Self(Operator::new(dav)?))
    }

    #[cfg(feature = "s3")]
    pub fn s3(profile: &Profile, secret: &str) -> Result<Self> {
        let mut s3 = opendal::services::S3::default()
            .bucket(&profile.share)
            .root(&profile.root);
        if !profile.endpoint.is_empty() {
            s3 = s3.endpoint(&profile.endpoint);
        }
        if !profile.region.is_empty() {
            s3 = s3.region(&profile.region);
        }
        if !profile.user.is_empty() {
            s3 = s3.access_key_id(&profile.user).secret_access_key(secret);
        } else if !profile.aws_profile.is_empty() {
            s3 = s3.profile(&profile.aws_profile);
        }
        Ok(Self(Operator::new(s3)?))
    }
}

impl crate::Backend for Backend {
    fn entries(&self, path: &str, cancel: &CancelToken) -> Result<Vec<Entry>> {
        let path = format!("{path}/");
        run(cancel, async {
            let entries = self.0.list(&path).await?;
            Ok(entries
                .into_iter()
                .filter(|entry| {
                    entry.path() != path && component(entry.name().trim_end_matches('/'))
                })
                .map(|entry| {
                    let metadata = entry.metadata();
                    Entry {
                        name: entry.name().trim_end_matches('/').to_string(),
                        directory: metadata.is_dir(),
                        size: metadata.content_length(),
                        modified_ms: metadata
                            .last_modified()
                            .map_or(0, |time| time.into_inner().as_millisecond().max(0) as u64),
                    }
                })
                .collect())
        })
    }

    fn open(&self, path: &str, cancel: &CancelToken) -> Result<RemoteFile> {
        run(cancel, async {
            let metadata = self.0.stat(path).await?;
            ensure!(metadata.is_file(), "Expected a remote file");
            let size = metadata.content_length();
            let reader = self
                .0
                .reader(path)
                .await?
                .into_futures_async_read(0..size)
                .await?;
            Ok(RemoteFile {
                size,
                reader: Box::new(Reader {
                    reader,
                    cancel: cancel.clone(),
                }),
            })
        })
    }
}

struct Reader {
    reader: FuturesAsyncReader,
    cancel: CancelToken,
}

impl Read for Reader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        run(&self.cancel, async { Ok(self.reader.read(bytes).await?) }).map_err(io::Error::other)
    }
}

impl Seek for Reader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        run(&self.cancel, async {
            Ok(self.reader.seek(position).await?)
        })
        .map_err(io::Error::other)
    }
}
